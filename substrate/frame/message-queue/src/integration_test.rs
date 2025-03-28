// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Stress tests pallet-message-queue. Defines its own runtime config to use larger constants for
//! `HeapSize` and `MaxStale`.
//!
//! The tests in this file are ignored by default, since they are quite slow. You can run them
//! manually like this:
//!
//! ```sh
//! RUST_LOG=info cargo test -p pallet-message-queue --profile testnet -- --ignored
//! ```

#![cfg(test)]

use crate::{
	mock::{
		build_and_execute, gen_seed, set_weight, Callback, CountingMessageProcessor, IntoWeight,
		MessagesProcessed, MockedWeightInfo, NumMessagesProcessed, YieldingQueues,
	},
	mock_helpers::{MessageOrigin, MessageOrigin::Everywhere},
	*,
};

use crate as pallet_message_queue;
use frame_support::{derive_impl, parameter_types};
use rand::{rngs::StdRng, Rng, SeedableRng};
use rand_distr::Pareto;
use std::collections::{BTreeMap, BTreeSet};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		MessageQueue: pallet_message_queue,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}

parameter_types! {
	pub const HeapSize: u32 = 32 * 1024;
	pub const MaxStale: u32 = 32;
	pub static ServiceWeight: Option<Weight> = Some(Weight::from_parts(100, 100));
}

impl Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = MockedWeightInfo;
	type MessageProcessor = CountingMessageProcessor;
	type Size = u32;
	type QueueChangeHandler = AhmPrioritizer;
	type QueuePausedQuery = ();
	type HeapSize = HeapSize;
	type MaxStale = MaxStale;
	type ServiceWeight = ServiceWeight;
	type IdleMaxServiceWeight = ();
}

/// The object that does the AHM message prioritization for us.
#[derive(Debug, Default, codec::Encode, codec::Decode)]
pub struct AhmPrioritizer {
	streak_until: Option<u64>,
	prioritized_queue: Option<MessageOriginOf<Test>>,
	favorite_queue_num_messages: Option<u64>,
}

// The whole `AhmPrioritizer` could be part of the AHM controller pallet.
parameter_types! {
	pub storage AhmPrioritizerStorage: AhmPrioritizer = AhmPrioritizer::default();
}

/// Instead of giving our prioritized queue only one block, we give it a streak of blocks.
const STREAK_LEN: u64 = 3;

impl OnQueueChanged<MessageOrigin> for AhmPrioritizer {
	fn on_queue_changed(origin: MessageOrigin, f: QueueFootprint) {
		let mut this = AhmPrioritizerStorage::get();

		if this.prioritized_queue != Some(origin) {
			return;
		}

		// Return early if this was an enqueue instead of a dequeue.
		if this.favorite_queue_num_messages.map_or(false, |n| n <= f.storage.count) {
			return;
		}
		this.favorite_queue_num_messages = Some(f.storage.count);

		// only update when we are not already in a streak
		if this.streak_until.map_or(false, |s| s < System::block_number()) {
			this.streak_until = Some(System::block_number().saturating_add(STREAK_LEN));
		}
	}
}

impl AhmPrioritizer {
	// This will need to be called by the migration controller.
	fn on_initialize(now: u64) -> Weight {
		let mut meter = WeightMeter::new();
		let mut this = AhmPrioritizerStorage::get();

		let Some(q) = this.prioritized_queue else {
			return meter.consumed();
		};
		// init
		if this.streak_until.is_none() {
			this.streak_until = Some(0);
		}
		if this.favorite_queue_num_messages.is_none() {
			this.favorite_queue_num_messages = Some(0);
		}

		// Our queue did not get a streak since 10 blocks. It must either be empty or starved:
		if Pallet::<Test>::footprint(q).pages == 0 {
			return meter.consumed();
		}
		if this.streak_until.map_or(false, |until| until < now.saturating_sub(10)) {
			log::warn!("Queue is being starved, scheduling streak of {} blocks", STREAK_LEN);
			this.streak_until = Some(now.saturating_add(STREAK_LEN));
		}

		if this.streak_until.map_or(false, |until| until > now) {
			let _ = Pallet::<Test>::force_set_head(&mut meter, &q).defensive();
		}

		meter.consumed()
	}
}

impl Drop for AhmPrioritizer {
	fn drop(&mut self) {
		AhmPrioritizerStorage::set(self);
	}
}

/// Simulates heavy usage by enqueueing and processing large amounts of messages.
///
/// # Example output
///
/// ```pre
/// Enqueued 1189 messages across 176 queues. Payload 46.97 KiB    
/// Processing 772 of 1189 messages    
/// Enqueued 9270 messages across 1559 queues. Payload 131.85 KiB    
/// Processing 6262 of 9687 messages    
/// Enqueued 5025 messages across 1225 queues. Payload 100.23 KiB    
/// Processing 1739 of 8450 messages    
/// Enqueued 42061 messages across 6357 queues. Payload 536.29 KiB    
/// Processing 11675 of 48772 messages    
/// Enqueued 20253 messages across 2420 queues. Payload 288.34 KiB    
/// Processing 28711 of 57350 messages
/// Processing all remaining 28639 messages
/// ```
#[test]
#[ignore] // Only run in the CI, otherwise its too slow.
fn stress_test_enqueue_and_service() {
	let blocks = 20;
	let max_queues = 10_000;
	let max_messages_per_queue = 10_000;
	let max_msg_len = MaxMessageLenOf::<Test>::get();
	let mut rng = StdRng::seed_from_u64(gen_seed());

	build_and_execute::<Test>(|| {
		let mut msgs_remaining = 0;
		for _ in 0..blocks {
			// Start by enqueuing a large number of messages.
			let enqueued =
				enqueue_messages(max_queues, max_messages_per_queue, max_msg_len, &mut rng);
			msgs_remaining += enqueued;

			// Pick a fraction of all messages currently in queue and process them.
			let processed = rng.gen_range(1..=msgs_remaining);
			log::info!("Processing {} of all messages {}", processed, msgs_remaining);
			process_some_messages(processed); // This also advances the block.
			msgs_remaining -= processed;
		}
		log::info!("Processing all remaining {} messages", msgs_remaining);
		process_all_messages(msgs_remaining);
		post_conditions();
	});
}

/// Simulate heavy usage while calling `force_set_head` on random queues.
#[test]
#[ignore] // Only run in the CI, otherwise its too slow.
fn stress_test_force_set_head() {
	let blocks = 20;
	let max_queues = 10_000;
	let max_messages_per_queue = 10_000;
	let max_msg_len = MaxMessageLenOf::<Test>::get();
	let mut rng = StdRng::seed_from_u64(gen_seed());

	build_and_execute::<Test>(|| {
		let mut msgs_remaining = 0;
		for _ in 0..blocks {
			// Start by enqueuing a large number of messages.
			let enqueued =
				enqueue_messages(max_queues, max_messages_per_queue, max_msg_len, &mut rng);
			msgs_remaining += enqueued;

			for _ in 0..10 {
				let random_queue = rng.gen_range(0..=max_queues);
				MessageQueue::force_set_head(&mut WeightMeter::new(), &Everywhere(random_queue))
					.unwrap();
			}

			// Pick a fraction of all messages currently in queue and process them.
			let processed = rng.gen_range(1..=msgs_remaining);
			log::info!("Processing {} of all messages {}", processed, msgs_remaining);
			process_some_messages(processed); // This also advances the block.
			msgs_remaining -= processed;
		}
		log::info!("Processing all remaining {} messages", msgs_remaining);
		process_all_messages(msgs_remaining);
		post_conditions();
	});
}

/// Check that our AHM prioritization does not affect liveness. This does not really check the AHM
/// prioritization works itself, but rather that it does not break things. The actual test is in
/// another test below.
#[test]
#[ignore] // Only run in the CI, otherwise its too slow.
fn stress_test_prioritize_queue() {
	let blocks = 20;
	let max_queues = 10_000;
	let favorite_queue = Everywhere(9000);
	let max_messages_per_queue = 1_000;
	let max_msg_len = MaxMessageLenOf::<Test>::get();
	let mut rng = StdRng::seed_from_u64(gen_seed());

	build_and_execute::<Test>(|| {
		let mut prio = AhmPrioritizerStorage::get();
		prio.prioritized_queue = Some(favorite_queue);
		drop(prio);

		let mut msgs_remaining = 0;
		for _ in 0..blocks {
			// Start by enqueuing a large number of messages.
			let enqueued =
				enqueue_messages(max_queues, max_messages_per_queue, max_msg_len, &mut rng);
			msgs_remaining += enqueued;
			// ensure that our favorite queue always has some more messages
			for _ in 0..200 {
				MessageQueue::enqueue_message(
					BoundedSlice::defensive_truncate_from("favorite".as_bytes()),
					favorite_queue,
				);
				msgs_remaining += 1;
			}

			// Pick a fraction of all messages currently in queue and process them.
			let processed = rng.gen_range(1..=100);
			log::info!("Processing {} of all messages {}", processed, msgs_remaining);
			process_some_messages(processed); // This also advances the block.
			msgs_remaining -= processed;
		}
		log::info!("Processing all remaining {} messages", msgs_remaining);
		process_all_messages(msgs_remaining);
		post_conditions();
	});
}

/// Very similar to `stress_test_enqueue_and_service`, but enqueues messages while processing them.
#[test]
#[ignore] // Only run in the CI, otherwise its too slow.
fn stress_test_recursive() {
	let blocks = 20;
	let mut rng = StdRng::seed_from_u64(gen_seed());

	// We need to use thread-locals since the callback cannot capture anything.
	parameter_types! {
		pub static TotalEnqueued: u32 = 0;
		pub static Enqueued: u32 = 0;
		pub static Called: u32 = 0;
	}

	Called::take();
	Enqueued::take();
	TotalEnqueued::take();

	Callback::set(Box::new(|_, _| {
		let mut rng = StdRng::seed_from_u64(Enqueued::get() as u64);
		let max_queues = 1_000;
		let max_messages_per_queue = 1_000;
		let max_msg_len = MaxMessageLenOf::<Test>::get();

		// Instead of directly enqueueing, we enqueue inside a `service` call.
		let enqueued = enqueue_messages(max_queues, max_messages_per_queue, max_msg_len, &mut rng);
		TotalEnqueued::set(TotalEnqueued::get() + enqueued);
		Enqueued::set(Enqueued::get() + enqueued);
		Called::set(Called::get() + 1);
		Ok(())
	}));

	build_and_execute::<Test>(|| {
		let mut msgs_remaining = 0;
		for b in 0..blocks {
			log::info!("Block #{}", b);
			MessageQueue::enqueue_message(
				BoundedSlice::defensive_truncate_from(format!("callback={b}").as_bytes()),
				b.into(),
			);

			msgs_remaining += Enqueued::take() + 1;
			// Pick a fraction of all messages currently in queue and process them.
			let processed = rng.gen_range(1..=msgs_remaining);
			log::info!("Processing {} of all messages {}", processed, msgs_remaining);
			process_some_messages(processed); // This also advances the block.
			msgs_remaining -= processed;
			TotalEnqueued::set(TotalEnqueued::get() - processed + 1);
			MessageQueue::do_try_state().unwrap();
		}
		while Called::get() < blocks {
			msgs_remaining += Enqueued::take();
			// Pick a fraction of all messages currently in queue and process them.
			let processed = rng.gen_range(1..=msgs_remaining);
			log::info!("Processing {} of all messages {}", processed, msgs_remaining);
			process_some_messages(processed); // This also advances the block.
			msgs_remaining -= processed;
			TotalEnqueued::set(TotalEnqueued::get() - processed);
			MessageQueue::do_try_state().unwrap();
		}

		let msgs_remaining = TotalEnqueued::take();
		log::info!("Processing all remaining {} messages", msgs_remaining);
		process_all_messages(msgs_remaining);
		assert_eq!(Called::get(), blocks);
		post_conditions();
	});
}

/// Simulates heavy usage of the suspension logic via `Yield`.
///
/// # Example output
///
/// ```pre
/// Enqueued 11776 messages across 2526 queues. Payload 173.94 KiB    
/// Suspended 63 and resumed 7 queues of 2526 in total    
/// Processing 593 messages. Resumed msgs: 11599, All msgs: 11776    
/// Enqueued 30104 messages across 5533 queues. Payload 416.62 KiB    
/// Suspended 24 and resumed 15 queues of 5533 in total    
/// Processing 12841 messages. Resumed msgs: 40857, All msgs: 41287    
/// Processing all 28016 remaining resumed messages    
/// Resumed all 64 suspended queues    
/// Processing all remaining 430 messages
/// ```
#[test]
#[ignore] // Only run in the CI, otherwise its too slow.
fn stress_test_queue_suspension() {
	let blocks = 20;
	let max_queues = 10_000;
	let max_messages_per_queue = 10_000;
	let (max_suspend_per_block, max_resume_per_block) = (100, 50);
	let max_msg_len = MaxMessageLenOf::<Test>::get();
	let mut rng = StdRng::seed_from_u64(gen_seed());

	build_and_execute::<Test>(|| {
		let mut suspended = BTreeSet::<u32>::new();
		let mut msgs_remaining = 0;

		for _ in 0..blocks {
			// Start by enqueuing a large number of messages.
			let enqueued =
				enqueue_messages(max_queues, max_messages_per_queue, max_msg_len, &mut rng);
			msgs_remaining += enqueued;
			let per_queue = msgs_per_queue();

			// Suspend a random subset of queues.
			let to_suspend = rng.gen_range(0..max_suspend_per_block).min(per_queue.len());
			for _ in 0..to_suspend {
				let q = rng.gen_range(0..per_queue.len());
				suspended.insert(*per_queue.iter().nth(q).map(|(q, _)| q).unwrap());
			}
			// Resume a random subst of suspended queues.
			let to_resume = rng.gen_range(0..max_resume_per_block).min(suspended.len());
			for _ in 0..to_resume {
				let q = rng.gen_range(0..suspended.len());
				suspended.remove(&suspended.iter().nth(q).unwrap().clone());
			}
			log::info!(
				"Suspended {} and resumed {} queues of {} in total",
				to_suspend,
				to_resume,
				per_queue.len()
			);
			YieldingQueues::set(suspended.iter().map(|q| MessageOrigin::Everywhere(*q)).collect());

			// Pick a fraction of all messages currently in queue and process them.
			let resumed_messages =
				per_queue.iter().filter(|(q, _)| !suspended.contains(q)).map(|(_, n)| n).sum();
			let processed = rng.gen_range(1..=resumed_messages);
			log::info!(
				"Processing {} messages. Resumed msgs: {}, All msgs: {}",
				processed,
				resumed_messages,
				msgs_remaining
			);
			process_some_messages(processed); // This also advances the block.
			msgs_remaining -= processed;
		}
		let per_queue = msgs_per_queue();
		let resumed_messages =
			per_queue.iter().filter(|(q, _)| !suspended.contains(q)).map(|(_, n)| n).sum();
		log::info!("Processing all {} remaining resumed messages", resumed_messages);
		process_all_messages(resumed_messages);
		msgs_remaining -= resumed_messages;

		let resumed = YieldingQueues::take();
		log::info!("Resumed all {} suspended queues", resumed.len());
		log::info!("Processing all remaining {} messages", msgs_remaining);
		process_all_messages(msgs_remaining);
		post_conditions();
	});
}

/// Test that our AHM prioritizer will ensure that our favorite queue always gets some dedicated
/// weight.
#[test]
#[ignore]
fn stress_test_ahm_despair_mode_works() {
	build_and_execute::<Test>(|| {
		let blocks = 200;
		let queues = 200;

		for o in 0..queues {
			for i in 0..100 {
				MessageQueue::enqueue_message(
					BoundedSlice::defensive_truncate_from(format!("{}:{}", o, i).as_bytes()),
					Everywhere(o),
				);
			}
		}
		set_weight("bump_head", Weight::from_parts(1, 1));

		// Prioritize the last queue.
		let mut prio = AhmPrioritizerStorage::get();
		prio.prioritized_queue = Some(Everywhere(199));
		drop(prio);

		ServiceWeight::set(Some(Weight::from_parts(10, 10)));
		for _ in 0..blocks {
			next_block();
		}

		// Check that our favorite queue has processed the most messages.
		let mut min = u64::MAX;
		let mut min_origin = 0;

		for o in 0..queues {
			let fp = MessageQueue::footprint(Everywhere(o));
			if fp.storage.count < min {
				min = fp.storage.count;
				min_origin = o;
			}
		}
		assert_eq!(min_origin, 199);

		// Process all remaining messages.
		ServiceWeight::set(Some(Weight::MAX));
		next_block();
		post_conditions();
	});
}

/// How many messages are in each queue.
fn msgs_per_queue() -> BTreeMap<u32, u32> {
	let mut per_queue = BTreeMap::new();
	for (o, q) in BookStateFor::<Test>::iter() {
		let MessageOrigin::Everywhere(o) = o else {
			unreachable!();
		};
		per_queue.insert(o, q.message_count as u32);
	}
	per_queue
}

/// Enqueue a random number of random messages into a random number of queues.
///
/// Returns the total number of enqueued messages, their combined length and the number of messages
/// per queue.
fn enqueue_messages(
	max_queues: u32,
	max_per_queue: u32,
	max_msg_len: u32,
	rng: &mut StdRng,
) -> u32 {
	let num_queues = rng.gen_range(1..max_queues);
	let mut num_messages = 0;
	let mut total_msg_len = 0;
	for origin in 0..num_queues {
		let num_messages_per_queue =
			(rng.sample(Pareto::new(1.0, 1.1).unwrap()) as u32).min(max_per_queue);

		for m in 0..num_messages_per_queue {
			let mut message = format!("{}:{}", &origin, &m).into_bytes();
			let msg_len = (rng.sample(Pareto::new(1.0, 1.0).unwrap()) as u32)
				.clamp(message.len() as u32, max_msg_len);
			message.resize(msg_len as usize, 0);
			MessageQueue::enqueue_message(
				BoundedSlice::defensive_truncate_from(&message),
				origin.into(),
			);
			total_msg_len += msg_len;
		}
		num_messages += num_messages_per_queue;
	}
	log::info!(
		"Enqueued {} messages across {} queues. Payload {:.2} KiB",
		num_messages,
		num_queues,
		total_msg_len as f64 / 1024.0
	);
	num_messages
}

/// Process the number of messages.
fn process_some_messages(num_msgs: u32) {
	let weight = (num_msgs as u64).into_weight();
	ServiceWeight::set(Some(weight));
	let consumed = next_block();

	for origin in BookStateFor::<Test>::iter_keys() {
		let fp = MessageQueue::footprint(origin);
		assert_eq!(fp.pages, fp.ready_pages);
	}

	assert_eq!(consumed, weight, "\n{}", MessageQueue::debug_info());
	assert_eq!(NumMessagesProcessed::take(), num_msgs as usize);
}

/// Process all remaining messages and assert their number.
fn process_all_messages(expected: u32) {
	ServiceWeight::set(Some(Weight::MAX));
	let consumed = next_block();

	assert_eq!(consumed, Weight::from_all(expected as u64));
	assert_eq!(NumMessagesProcessed::take(), expected as usize);
	MessagesProcessed::take();
}

/// Returns the weight consumed by `MessageQueue::on_initialize()`.
fn next_block() -> Weight {
	log::info!("Next block: {}", System::block_number() + 1);
	MessageQueue::on_finalize(System::block_number());
	System::on_finalize(System::block_number());
	System::set_block_number(System::block_number() + 1);
	System::on_initialize(System::block_number());
	AhmPrioritizer::on_initialize(System::block_number());
	MessageQueue::on_initialize(System::block_number())
}

/// Assert that the pallet is in the expected post state.
fn post_conditions() {
	// All queues are empty.
	for (_, book) in BookStateFor::<Test>::iter() {
		assert!(book.end >= book.begin);
		assert_eq!(book.count, 0);
		assert_eq!(book.size, 0);
		assert_eq!(book.message_count, 0);
		assert!(book.ready_neighbours.is_none());
	}
	// No pages remain.
	assert_eq!(Pages::<Test>::iter().count(), 0);
	// Service head is gone.
	assert!(ServiceHead::<Test>::get().is_none());
	// This still works fine.
	assert_eq!(MessageQueue::service_queues(Weight::MAX), Weight::zero(), "Nothing left");
	MessageQueue::do_try_state().unwrap();
	next_block();
}
