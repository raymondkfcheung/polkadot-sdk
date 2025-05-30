// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Tests for top-level transaction pool api

use codec::Encode;
use futures::{
	executor::{block_on, block_on_stream},
	prelude::*,
	task::Poll,
};
use sc_block_builder::BlockBuilderBuilder;
use sc_client_api::client::BlockchainEvents;
use sc_transaction_pool::*;
use sc_transaction_pool_api::{
	ChainEvent, MaintainedTransactionPool, TransactionPool, TransactionStatus,
};
use sp_blockchain::HeaderBackend;
use sp_consensus::BlockOrigin;
use sp_runtime::{
	generic::BlockId,
	traits::Block as _,
	transaction_validity::{TransactionSource, ValidTransaction},
};
use std::{collections::BTreeSet, pin::Pin, sync::Arc};
use substrate_test_runtime_client::{
	runtime::{Block, Extrinsic, ExtrinsicBuilder, Hash, Header, Nonce, Transfer, TransferData},
	ClientBlockImportExt,
	Sr25519Keyring::*,
};
use substrate_test_runtime_transaction_pool::{uxt, TestApi};
use tracing::{debug, trace};

type Pool<Api> = sc_transaction_pool::Pool<Api, ()>;

const LOG_TARGET: &str = "txpool";

fn pool() -> (Pool<TestApi>, Arc<TestApi>) {
	let api = Arc::new(TestApi::with_alice_nonce(209));
	(Pool::new_with_staticly_sized_rotator(Default::default(), true.into(), api.clone()), api)
}

fn maintained_pool() -> (BasicPool<TestApi, Block>, Arc<TestApi>, futures::executor::ThreadPool) {
	let api = Arc::new(TestApi::with_alice_nonce(209));
	let (pool, background_task) = create_basic_pool_with_genesis(api.clone());

	let thread_pool = futures::executor::ThreadPool::new().unwrap();
	thread_pool.spawn_ok(background_task);
	(pool, api, thread_pool)
}

fn create_basic_pool_with_genesis(
	test_api: Arc<TestApi>,
) -> (BasicPool<TestApi, Block>, Pin<Box<dyn Future<Output = ()> + Send>>) {
	let genesis_hash = {
		test_api
			.chain()
			.read()
			.block_by_number
			.get(&0)
			.map(|blocks| blocks[0].0.header.hash())
			.expect("there is block 0. qed")
	};
	BasicPool::new_test(test_api, genesis_hash, genesis_hash, Default::default())
}

fn create_basic_pool(test_api: TestApi) -> BasicPool<TestApi, Block> {
	create_basic_pool_with_genesis(Arc::from(test_api)).0
}

const TSOURCE: TimedTransactionSource =
	TimedTransactionSource { source: TransactionSource::External, timestamp: None };
const SOURCE: TransactionSource = TransactionSource::External;

#[test]
fn submission_should_work() {
	let (pool, api) = pool();
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 209).into()))
		.unwrap();

	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, vec![209]);
}

#[test]
fn multiple_submission_should_work() {
	let (pool, api) = pool();
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 209).into()))
		.unwrap();
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 210).into()))
		.unwrap();

	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, vec![209, 210]);
}

#[test]
fn early_nonce_should_be_culled() {
	sp_tracing::try_init_simple();
	let (pool, api) = pool();
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 208).into()))
		.unwrap();

	debug!(target: LOG_TARGET, pool_status = ?pool.validated_pool().status(), "Validated pool status");
	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, Vec::<Nonce>::new());
}

#[test]
fn late_nonce_should_be_queued() {
	let (pool, api) = pool();

	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 210).into()))
		.unwrap();
	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, Vec::<Nonce>::new());

	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 209).into()))
		.unwrap();
	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, vec![209, 210]);
}

#[test]
fn prune_tags_should_work() {
	let (pool, api) = pool();
	let hash209 =
		block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 209).into()))
			.map(|o| o.hash())
			.unwrap();
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt(Alice, 210).into()))
		.unwrap();

	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, vec![209, 210]);

	pool.validated_pool().api().push_block(1, Vec::new(), true);
	block_on(pool.prune_tags(&api.expect_hash_and_number(1), vec![vec![209]], vec![hash209]));

	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, vec![210]);
}

#[test]
fn should_ban_invalid_transactions() {
	let (pool, api) = pool();
	let uxt = Arc::from(uxt(Alice, 209));
	let hash = block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt.clone()))
		.unwrap()
		.hash();
	pool.validated_pool().remove_invalid(&[hash]);
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt.clone()))
		.map(|_| ())
		.unwrap_err();

	// when
	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, Vec::<Nonce>::new());

	// then
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, uxt.clone()))
		.map(|_| ())
		.unwrap_err();
}

#[test]
fn only_prune_on_new_best() {
	let (pool, api, _) = maintained_pool();
	let uxt = uxt(Alice, 209);

	let _ = block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, uxt.clone()))
		.expect("1. Imported");
	api.push_block(1, vec![uxt.clone()], true);
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(2, vec![uxt], true);
	let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
	block_on(pool.maintain(event));
	assert_eq!(pool.status().ready, 0);
}

#[test]
fn should_correctly_prune_transactions_providing_more_than_one_tag() {
	sp_tracing::try_init_simple();
	let api = Arc::new(TestApi::with_alice_nonce(209));
	api.set_valid_modifier(Box::new(|v: &mut ValidTransaction| {
		v.provides.push(vec![155]);
	}));
	let pool = Pool::new_with_staticly_sized_rotator(Default::default(), true.into(), api.clone());
	let xt0 = Arc::from(uxt(Alice, 209));
	block_on(pool.submit_one(&api.expect_hash_and_number(0), TSOURCE, xt0.clone()))
		.expect("1. Imported");
	assert_eq!(pool.validated_pool().status().ready, 1);
	assert_eq!(api.validation_requests().len(), 1);

	// remove the transaction that just got imported.
	api.increment_nonce(Alice.into());
	api.push_block(1, Vec::new(), true);
	block_on(pool.prune_tags(&api.expect_hash_and_number(1), vec![vec![209]], vec![]));
	assert_eq!(api.validation_requests().len(), 2);
	assert_eq!(pool.validated_pool().status().ready, 0);
	// it's re-imported to future, API does not support stale - xt0 becomes future
	assert_eq!(pool.validated_pool().status().future, 1);

	// so now let's insert another transaction that also provides the 155
	api.increment_nonce(Alice.into());
	api.push_block(2, Vec::new(), true);
	let xt1 = uxt(Alice, 211);
	block_on(pool.submit_one(&api.expect_hash_and_number(2), TSOURCE, xt1.clone().into()))
		.expect("2. Imported");
	assert_eq!(api.validation_requests().len(), 3);
	assert_eq!(pool.validated_pool().status().ready, 1);
	assert_eq!(pool.validated_pool().status().future, 1);
	let pending: Vec<_> = pool
		.validated_pool()
		.ready()
		.map(|a| TransferData::try_from(&*a.data).unwrap().nonce)
		.collect();
	assert_eq!(pending, vec![211]);

	// prune it and make sure the pool is empty
	api.increment_nonce(Alice.into());
	api.push_block(3, Vec::new(), true);
	block_on(pool.prune_tags(&api.expect_hash_and_number(3), vec![vec![155]], vec![]));
	assert_eq!(api.validation_requests().len(), 4);
	//xt0 was future, it failed (bc of 155 tag conflict) and was removed
	assert_eq!(pool.validated_pool().status().ready, 0);
	//xt1 was ready, it was pruned (bc of 155 tag conflict) but was revalidated and resubmitted
	// (API does not know about 155).
	assert_eq!(pool.validated_pool().status().future, 1);

	let pending: Vec<_> = pool.validated_pool().futures().iter().map(|(hash, _)| *hash).collect();
	assert_eq!(pending[0], api.hash_and_length(&xt1).0);
}

fn block_event(header: Header) -> ChainEvent<Block> {
	ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None }
}

fn block_event_with_retracted(
	new_best_block_header: Header,
	retracted_start: Hash,
	api: &TestApi,
) -> ChainEvent<Block> {
	let tree_route = api
		.tree_route(retracted_start, new_best_block_header.parent_hash)
		.expect("Tree route exists");

	ChainEvent::NewBestBlock {
		hash: new_best_block_header.hash(),
		tree_route: Some(Arc::new(tree_route)),
	}
}

#[test]
fn should_prune_old_during_maintenance() {
	let xt = uxt(Alice, 209);

	let (pool, api, _guard) = maintained_pool();

	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt.clone()))
		.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(1, vec![xt.clone()], true);

	block_on(pool.maintain(block_event(header)));
	assert_eq!(pool.status().ready, 0);
}

#[test]
fn should_revalidate_during_maintenance() {
	let xt1 = uxt(Alice, 209);
	let xt2 = uxt(Alice, 210);

	let (pool, api, _guard) = maintained_pool();
	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt1.clone()))
		.expect("1. Imported");
	let watcher =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, xt2.clone()))
			.expect("import"); //todo
	assert_eq!(pool.status().ready, 2);
	assert_eq!(api.validation_requests().len(), 2);

	let header = api.push_block(1, vec![xt1.clone()], true);

	api.add_invalid(&xt2);

	block_on(pool.maintain(block_event(header)));
	assert_eq!(pool.status().ready, 1);

	// test that pool revalidated transaction that left ready and not included in the block
	assert_eq!(
		futures::executor::block_on_stream(watcher).collect::<Vec<_>>(),
		vec![TransactionStatus::Ready, TransactionStatus::Invalid],
	);
}

#[test]
fn should_resubmit_from_retracted_during_maintenance() {
	let xt = uxt(Alice, 209);

	let (pool, api, _guard) = maintained_pool();

	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt.clone()))
		.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(1, vec![], true);
	let fork_header = api.push_block(1, vec![], true);

	let event = block_event_with_retracted(header, fork_header.hash(), pool.api());

	block_on(pool.maintain(event));
	assert_eq!(pool.status().ready, 1);
}

#[test]
fn should_not_resubmit_from_retracted_during_maintenance_if_tx_is_also_in_enacted() {
	let xt = uxt(Alice, 209);

	let (pool, api, _guard) = maintained_pool();

	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt.clone()))
		.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(1, vec![xt.clone()], true);
	let fork_header = api.push_block(1, vec![xt], true);

	let event = block_event_with_retracted(header, fork_header.hash(), pool.api());

	block_on(pool.maintain(event));
	assert_eq!(pool.status().ready, 0);
}

#[test]
fn should_not_retain_invalid_hashes_from_retracted() {
	let xt = uxt(Alice, 209);

	let (pool, api, _guard) = maintained_pool();

	let watcher =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, xt.clone()))
			.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(1, vec![], true);
	let fork_header = api.push_block(1, vec![xt.clone()], true);
	api.add_invalid(&xt);

	let event = block_event_with_retracted(header, fork_header.hash(), pool.api());
	block_on(pool.maintain(event));

	assert_eq!(
		futures::executor::block_on_stream(watcher).collect::<Vec<_>>(),
		vec![TransactionStatus::Ready, TransactionStatus::Invalid],
	);

	assert_eq!(pool.status().ready, 0);
}

#[test]
fn should_revalidate_across_many_blocks() {
	let xt1 = uxt(Alice, 209);
	let xt2 = uxt(Alice, 210);
	let xt3 = uxt(Alice, 211);

	let (pool, api, _guard) = maintained_pool();

	let watcher1 =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, xt1.clone()))
			.expect("1. Imported");
	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt2.clone()))
		.expect("1. Imported");
	assert_eq!(pool.status().ready, 2);

	let header = api.push_block(1, vec![], true);
	block_on(pool.maintain(block_event(header)));

	block_on(pool.submit_one(api.expect_hash_from_number(1), SOURCE, xt3.clone()))
		.expect("1. Imported");
	assert_eq!(pool.status().ready, 3);

	let header = api.push_block(2, vec![xt1.clone()], true);
	let block_hash = header.hash();
	block_on(pool.maintain(block_event(header.clone())));

	block_on(
		watcher1
			.take_while(|s| future::ready(*s != TransactionStatus::InBlock((block_hash, 0))))
			.collect::<Vec<_>>(),
	);

	assert_eq!(pool.status().ready, 2);
}

#[test]
fn should_push_watchers_during_maintenance() {
	fn alice_uxt(nonce: u64) -> Extrinsic {
		uxt(Alice, 209 + nonce)
	}

	// given
	let (pool, api, _guard) = maintained_pool();

	let tx0 = alice_uxt(0);
	let watcher0 =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, tx0.clone()))
			.unwrap();
	let tx1 = alice_uxt(1);
	let watcher1 =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, tx1.clone()))
			.unwrap();
	let tx2 = alice_uxt(2);
	let watcher2 =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, tx2.clone()))
			.unwrap();
	let tx3 = alice_uxt(3);
	let watcher3 =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, tx3.clone()))
			.unwrap();
	let tx4 = alice_uxt(4);
	let watcher4 =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, tx4.clone()))
			.unwrap();
	assert_eq!(pool.status().ready, 5);

	// when
	api.add_invalid(&tx3);
	api.add_invalid(&tx4);

	// clear timer events if any
	let header = api.push_block(1, vec![], true);
	block_on(pool.maintain(block_event(header)));

	// then
	// hash3 is now invalid
	// hash4 is now invalid
	assert_eq!(
		futures::executor::block_on_stream(watcher3).collect::<Vec<_>>(),
		vec![TransactionStatus::Ready, TransactionStatus::Invalid],
	);
	assert_eq!(
		futures::executor::block_on_stream(watcher4).collect::<Vec<_>>(),
		vec![TransactionStatus::Ready, TransactionStatus::Invalid],
	);
	assert_eq!(pool.status().ready, 3);

	// when
	let header = api.push_block(2, vec![tx0, tx1, tx2], true);
	let header_hash = header.hash();
	block_on(pool.maintain(block_event(header)));

	let event = ChainEvent::Finalized { hash: header_hash, tree_route: Arc::from(vec![]) };
	block_on(pool.maintain(event));

	// then
	// events for hash0 are: Ready, InBlock
	// events for hash1 are: Ready, InBlock
	// events for hash2 are: Ready, InBlock
	assert_eq!(
		futures::executor::block_on_stream(watcher0).collect::<Vec<_>>(),
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header_hash, 0)),
			TransactionStatus::Finalized((header_hash, 0))
		],
	);
	assert_eq!(
		futures::executor::block_on_stream(watcher1).collect::<Vec<_>>(),
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header_hash, 1)),
			TransactionStatus::Finalized((header_hash, 1))
		],
	);
	assert_eq!(
		futures::executor::block_on_stream(watcher2).collect::<Vec<_>>(),
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header_hash, 2)),
			TransactionStatus::Finalized((header_hash, 2))
		],
	);
}

#[test]
fn finalization() {
	let xt = uxt(Alice, 209);
	let api = TestApi::with_alice_nonce(209);
	api.push_block(1, vec![], true);
	let pool = create_basic_pool(api);
	let api = pool.api();
	let watcher =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, xt.clone()))
			.expect("1. Imported");
	api.push_block(2, vec![xt.clone()], true);

	let header = api.chain().read().block_by_number.get(&2).unwrap()[0].0.header().clone();
	let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
	block_on(pool.maintain(event));

	let event = ChainEvent::Finalized { hash: header.hash(), tree_route: Arc::from(vec![]) };
	block_on(pool.maintain(event));

	let mut stream = futures::executor::block_on_stream(watcher);
	assert_eq!(stream.next(), Some(TransactionStatus::Ready));
	assert_eq!(stream.next(), Some(TransactionStatus::InBlock((header.hash(), 0))));
	assert_eq!(stream.next(), Some(TransactionStatus::Finalized((header.hash(), 0))));
	assert_eq!(stream.next(), None);
}

#[test]
fn fork_aware_finalization() {
	sp_tracing::try_init_simple();
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	let a_header = api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();
	let mut canon_watchers = vec![];

	let from_alice = uxt(Alice, 1);
	let from_dave = uxt(Dave, 2);
	let from_bob = uxt(Bob, 1);
	let from_charlie = uxt(Charlie, 1);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Dave.into());
	api.increment_nonce(Charlie.into());
	api.increment_nonce(Bob.into());

	let from_dave_watcher;
	let from_bob_watcher;
	let b1;
	let c1;
	let d1;
	let c2;
	let d2;

	block_on(pool.maintain(block_event(a_header)));

	// block B1
	{
		let watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_alice.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block(2, vec![from_alice.clone()], true);
		canon_watchers.push((watcher, header.hash()));
		assert_eq!(pool.status().ready, 1);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B1");
		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		b1 = header.hash();
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
		let event = ChainEvent::Finalized { hash: b1, tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	// block C2
	{
		let header = api.push_block_with_parent(b1, vec![from_dave.clone()], true);
		from_dave_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_dave.clone(),
		))
		.expect("1. Imported");
		assert_eq!(pool.status().ready, 1);
		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> C2");
		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		c2 = header.hash();
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	// block D2
	{
		from_bob_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_bob.clone(),
		))
		.expect("1. Imported");
		assert_eq!(pool.status().ready, 1);
		let header = api.push_block_with_parent(c2, vec![from_bob.clone()], true);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> D2");
		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		d2 = header.hash();
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	// block C1
	{
		let watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_charlie.clone(),
		))
		.expect("1.Imported");
		assert_eq!(pool.status().ready, 1);
		let header = api.push_block_with_parent(b1, vec![from_charlie.clone()], true);
		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> C1");
		c1 = header.hash();
		canon_watchers.push((watcher, header.hash()));
		let event = block_event_with_retracted(header.clone(), d2, api);
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 2);

		let event = ChainEvent::Finalized { hash: header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	// block D1
	{
		let xt = uxt(Eve, 0);
		let w = block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, xt.clone()))
			.expect("1. Imported");
		assert_eq!(pool.status().ready, 3);
		let header = api.push_block_with_parent(c1, vec![xt.clone()], true);
		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> D1");
		d1 = header.hash();
		canon_watchers.push((w, header.hash()));

		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 2);
		let event = ChainEvent::Finalized { hash: d1, tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	let e1;

	// block E1
	{
		let header = api.push_block_with_parent(d1, vec![from_dave, from_bob], true);
		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> E1");
		e1 = header.hash();
		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
		block_on(pool.maintain(ChainEvent::Finalized { hash: e1, tree_route: Arc::from(vec![]) }));
	}

	for (canon_watcher, h) in canon_watchers {
		let mut stream = futures::executor::block_on_stream(canon_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((h, 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((h, 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_dave_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((c2, 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(c2)));
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((e1, 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((e1, 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_bob_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((d2, 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(d2)));
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		// In block e1 we submitted: [dave, bob] xts in this order.
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((e1, 1))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((e1, 1))));
		assert_eq!(stream.next(), None);
	}
}

/// Tests that when pruning and retracing a tx by the same event, we generate
/// the correct events in the correct order.
#[test]
fn prune_and_retract_tx_at_same_time() {
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let from_alice = uxt(Alice, 1);
	api.increment_nonce(Alice.into());

	let watcher =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, from_alice.clone()))
			.expect("1. Imported");

	// Block B1
	let b1 = {
		let header = api.push_block(2, vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 1);

		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
		header.hash()
	};

	// Block B2
	let b2 = {
		let header = api.push_block(2, vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 0);

		let event = block_event_with_retracted(header.clone(), b1, api);
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);

		let event = ChainEvent::Finalized { hash: header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));

		header.hash()
	};

	{
		let mut stream = futures::executor::block_on_stream(watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1, 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(b1)));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2, 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b2, 0))));
		assert_eq!(stream.next(), None);
	}
}

/// This test ensures that transactions from a fork are re-submitted if
/// the forked block is not part of the retracted blocks. This happens as the
/// retracted block list only contains the route from the old best to the new
/// best, without any further forks.
///
/// Given the following:
///
///     -> D0 (old best, tx0)
///    /
/// C - -> D1 (tx1)
///    \
///     -> D2 (new best)
///
/// Retracted will contain `D0`, but we need to re-submit `tx0` and `tx1` as both
/// blocks are not part of the canonical chain.
#[test]
fn resubmit_tx_of_fork_that_is_not_part_of_retracted() {
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let tx0 = uxt(Alice, 1);
	let tx1 = uxt(Dave, 2);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Dave.into());

	let d0;

	// Block D0
	{
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx0.clone()))
				.expect("1. Imported");
		let header = api.push_block(2, vec![tx0.clone()], true);
		assert_eq!(pool.status().ready, 1);

		let event = ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None };
		d0 = header.hash();
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	// Block D1
	{
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx1.clone()))
				.expect("1. Imported");
		api.push_block(2, vec![tx1.clone()], false);
		assert_eq!(pool.status().ready, 1);
	}

	// Block D2
	{
		//push new best block
		let header = api.push_block(2, vec![], true);
		let event = block_event_with_retracted(header, d0, api);
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 2);
	}
}

#[test]
fn resubmit_from_retracted_fork() {
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);

	let api = pool.api();

	let tx0 = uxt(Alice, 1);
	let tx1 = uxt(Dave, 2);
	let tx2 = uxt(Bob, 3);

	// Transactions of the fork that will be enacted later
	let tx3 = uxt(Eve, 1);
	let tx4 = uxt(Ferdie, 2);
	let tx5 = uxt(One, 3);

	api.increment_nonce(Alice.into());
	api.increment_nonce(Dave.into());
	api.increment_nonce(Bob.into());
	api.increment_nonce(Eve.into());
	api.increment_nonce(Ferdie.into());
	api.increment_nonce(One.into());

	// Block D0
	{
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx0.clone()))
				.expect("1. Imported");
		let header = api.push_block(2, vec![tx0.clone()], true);
		assert_eq!(pool.status().ready, 1);

		block_on(pool.maintain(block_event(header)));
		assert_eq!(pool.status().ready, 0);
	}

	// Block E0
	{
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx1.clone()))
				.expect("1. Imported");
		let header = api.push_block(3, vec![tx1.clone()], true);
		block_on(pool.maintain(block_event(header)));
		assert_eq!(pool.status().ready, 0);
	}

	// Block F0
	let f0 = {
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx2.clone()))
				.expect("1. Imported");
		let header = api.push_block(4, vec![tx2.clone()], true);
		block_on(pool.maintain(block_event(header.clone())));
		assert_eq!(pool.status().ready, 0);
		header.hash()
	};

	// Block D1
	let d1 = {
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx3.clone()))
				.expect("1. Imported");
		let header = api.push_block(2, vec![tx3.clone()], true);
		assert_eq!(pool.status().ready, 1);
		header.hash()
	};

	// Block E1
	let e1 = {
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx4.clone()))
				.expect("1. Imported");
		let header = api.push_block_with_parent(d1, vec![tx4.clone()], true);
		assert_eq!(pool.status().ready, 2);
		header.hash()
	};

	// Block F1
	let f1_header = {
		let _ =
			block_on(pool.submit_and_watch(api.expect_hash_from_number(1), SOURCE, tx5.clone()))
				.expect("1. Imported");
		let header = api.push_block_with_parent(e1, vec![tx5.clone()], true);
		// Don't announce the block event to the pool directly, because we will
		// re-org to this block.
		assert_eq!(pool.status().ready, 3);
		header
	};

	let ready = pool.ready().map(|t| t.data.encode()).collect::<BTreeSet<_>>();
	let expected_ready = vec![tx3, tx4, tx5].iter().map(Encode::encode).collect::<BTreeSet<_>>();
	assert_eq!(expected_ready, ready);

	let event = block_event_with_retracted(f1_header, f0, api);
	block_on(pool.maintain(event));

	assert_eq!(pool.status().ready, 3);
	let ready = pool.ready().map(|t| t.data.encode()).collect::<BTreeSet<_>>();
	let expected_ready = vec![tx0, tx1, tx2].iter().map(Encode::encode).collect::<BTreeSet<_>>();
	assert_eq!(expected_ready, ready);
}

#[test]
fn ready_set_should_not_resolve_before_block_update() {
	let (pool, api, _guard) = maintained_pool();
	let xt1 = uxt(Alice, 209);
	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt1.clone()))
		.expect("1. Imported");
	let hash_of_1 = api.push_block_with_parent(api.genesis_hash(), vec![], true).hash();

	assert!(pool.ready_at(hash_of_1).now_or_never().is_none());
}

#[test]
fn ready_set_should_resolve_after_block_update() {
	let (pool, api, _guard) = maintained_pool();
	let header = api.push_block(1, vec![], true);
	let hash_of_1 = header.hash();

	let xt1 = uxt(Alice, 209);

	block_on(pool.submit_one(api.expect_hash_from_number(1), SOURCE, xt1.clone()))
		.expect("1. Imported");
	block_on(pool.maintain(block_event(header)));

	assert!(pool.ready_at(hash_of_1).now_or_never().is_some());
}

#[test]
fn ready_set_should_eventually_resolve_when_block_update_arrives() {
	let (pool, api, _guard) = maintained_pool();
	let header = api.push_block(1, vec![], true);
	let hash_of_1 = header.hash();

	let xt1 = uxt(Alice, 209);

	block_on(pool.submit_one(api.expect_hash_from_number(1), SOURCE, xt1.clone()))
		.expect("1. Imported");

	let noop_waker = futures::task::noop_waker();
	let mut context = futures::task::Context::from_waker(&noop_waker);

	let mut ready_set_future = pool.ready_at(hash_of_1);
	if ready_set_future.poll_unpin(&mut context).is_ready() {
		panic!("Ready set should not be ready before block update!");
	}

	block_on(pool.maintain(block_event(header)));

	match ready_set_future.poll_unpin(&mut context) {
		Poll::Pending => {
			panic!("Ready set should become ready after block update!");
		},
		Poll::Ready(iterator) => {
			let data = iterator.collect::<Vec<_>>();
			assert_eq!(data.len(), 1);
		},
	}
}

#[test]
fn import_notification_to_pool_maintain_works() {
	let client = Arc::new(substrate_test_runtime_client::new());

	let best_hash = client.info().best_hash;
	let finalized_hash = client.info().finalized_hash;

	let pool = Arc::new(
		BasicPool::new_test(
			Arc::new(FullChainApi::new(
				client.clone(),
				None,
				&sp_core::testing::TaskExecutor::new(),
			)),
			best_hash,
			finalized_hash,
			Default::default(),
		)
		.0,
	);

	// Prepare the extrinsic, push it to the pool and check that it was added.
	let xt = uxt(Alice, 0);
	block_on(pool.submit_one(
		pool.api().block_id_to_hash(&BlockId::Number(0)).unwrap().unwrap(),
		SOURCE,
		xt.clone(),
	))
	.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let mut import_stream = block_on_stream(client.import_notification_stream());

	// Build the block with the transaction included
	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(best_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap();
	block_builder.push(xt).unwrap();
	let block = block_builder.build().unwrap().block;
	block_on(client.import(BlockOrigin::Own, block)).unwrap();

	// Get the notification of the block import and maintain the pool with it,
	// Now, the pool should not contain any transactions.
	let evt = import_stream.next().expect("Importing a block leads to an event");
	block_on(pool.maintain(evt.try_into().expect("Imported as new best block")));
	assert_eq!(pool.status().ready, 0);
}

// When we prune transactions, we need to make sure that we remove
#[test]
fn pruning_a_transaction_should_remove_it_from_best_transaction() {
	let (pool, api, _guard) = maintained_pool();

	let xt1 = ExtrinsicBuilder::new_include_data(Vec::new()).build();

	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt1.clone()))
		.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);
	let header = api.push_block(1, vec![xt1.clone()], true);

	// This will prune `xt1`.
	block_on(pool.maintain(block_event(header)));

	assert_eq!(pool.status().ready, 0);
}

#[test]
fn stale_transactions_are_pruned() {
	sp_tracing::try_init_simple();

	// Our initial transactions
	let xts = vec![
		Transfer { from: Alice.into(), to: Bob.into(), nonce: 10, amount: 1 },
		Transfer { from: Alice.into(), to: Bob.into(), nonce: 11, amount: 1 },
		Transfer { from: Alice.into(), to: Bob.into(), nonce: 12, amount: 1 },
	];

	let (pool, api, _guard) = maintained_pool();

	xts.into_iter().for_each(|xt| {
		block_on(pool.submit_one(
			api.expect_hash_from_number(0),
			SOURCE,
			xt.into_unchecked_extrinsic(),
		))
		.expect("1. Imported");
	});
	assert_eq!(pool.status().ready, 0);
	assert_eq!(pool.status().future, 3);

	// Almost the same as our initial transactions, but with some different `amount`s to make them
	// generate a different hash
	let xts = vec![
		Transfer { from: Alice.into(), to: Bob.into(), nonce: 1, amount: 2 }
			.into_unchecked_extrinsic(),
		Transfer { from: Alice.into(), to: Bob.into(), nonce: 2, amount: 2 }
			.into_unchecked_extrinsic(),
		Transfer { from: Alice.into(), to: Bob.into(), nonce: 3, amount: 2 }
			.into_unchecked_extrinsic(),
	];

	// Import block
	let header = api.push_block(1, xts, true);
	block_on(pool.maintain(block_event(header)));
	// The imported transactions have a different hash and should not evict our initial
	// transactions.
	debug!(target: LOG_TARGET, status = ?pool.status(), "Pool status");
	assert_eq!(pool.status().future, 3);

	// Import enough blocks to make our transactions stale
	for n in 1..66 {
		let header = api.push_block(n, vec![], true);
		block_on(pool.maintain(block_event(header)));
	}

	assert_eq!(pool.status().future, 0);
	assert_eq!(pool.status().ready, 0);
}

#[test]
fn finalized_only_handled_correctly() {
	sp_tracing::try_init_simple();
	let xt = uxt(Alice, 209);

	let (pool, api, _guard) = maintained_pool();

	let watcher =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, xt.clone()))
			.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(1, vec![xt], true);

	let event =
		ChainEvent::Finalized { hash: header.clone().hash(), tree_route: Arc::from(vec![]) };
	block_on(pool.maintain(event));

	assert_eq!(pool.status().ready, 0);

	{
		let mut stream = futures::executor::block_on_stream(watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((header.clone().hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}
}

#[test]
fn best_block_after_finalized_handled_correctly() {
	sp_tracing::try_init_simple();
	let xt = uxt(Alice, 209);

	let (pool, api, _guard) = maintained_pool();

	let watcher =
		block_on(pool.submit_and_watch(api.expect_hash_from_number(0), SOURCE, xt.clone()))
			.expect("1. Imported");
	assert_eq!(pool.status().ready, 1);

	let header = api.push_block(1, vec![xt], true);

	let event =
		ChainEvent::Finalized { hash: header.clone().hash(), tree_route: Arc::from(vec![]) };
	block_on(pool.maintain(event));
	block_on(pool.maintain(block_event(header.clone())));

	assert_eq!(pool.status().ready, 0);

	{
		let mut stream = futures::executor::block_on_stream(watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((header.clone().hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}
}

#[test]
fn switching_fork_with_finalized_works() {
	sp_tracing::try_init_simple();
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	let a_header = api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let from_alice = uxt(Alice, 1);
	let from_bob = uxt(Bob, 2);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Bob.into());

	let from_alice_watcher;
	let from_bob_watcher;
	let b1_header;
	let b2_header;

	// block B1
	{
		from_alice_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_alice.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(a_header.hash(), vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 1);
		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B1");
		b1_header = header;
	}

	// block B2
	{
		from_bob_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_bob.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(
			a_header.hash(),
			vec![from_alice.clone(), from_bob.clone()],
			true,
		);
		assert_eq!(pool.status().ready, 2);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B2");
		b2_header = header;
	}

	{
		let event = ChainEvent::NewBestBlock { hash: b1_header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 1);
	}

	{
		let event = ChainEvent::Finalized { hash: b2_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	{
		let mut stream = futures::executor::block_on_stream(from_alice_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(b1_header.hash())));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b2_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_bob_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2_header.hash(), 1))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b2_header.hash(), 1))));
		assert_eq!(stream.next(), None);
	}
}

#[test]
fn switching_fork_multiple_times_works() {
	sp_tracing::try_init_simple();
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	let a_header = api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let from_alice = uxt(Alice, 1);
	let from_bob = uxt(Bob, 2);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Bob.into());

	let from_alice_watcher;
	let from_bob_watcher;
	let b1_header;
	let b2_header;

	// block B1
	{
		from_alice_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_alice.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(a_header.hash(), vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 1);
		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B1");
		b1_header = header;
	}

	// block B2
	{
		from_bob_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_bob.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(
			a_header.hash(),
			vec![from_alice.clone(), from_bob.clone()],
			true,
		);
		assert_eq!(pool.status().ready, 2);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B2");
		b2_header = header;
	}

	{
		// phase-0
		let event = ChainEvent::NewBestBlock { hash: b1_header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 1);
	}

	{
		// phase-1
		let event = block_event_with_retracted(b2_header.clone(), b1_header.hash(), api);
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	{
		// phase-2
		let event = block_event_with_retracted(b1_header.clone(), b2_header.hash(), api);
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 1);
	}

	{
		// phase-3
		let event = ChainEvent::Finalized { hash: b2_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	{
		let mut stream = futures::executor::block_on_stream(from_alice_watcher);
		//phase-0
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1_header.hash(), 0))));
		//phase-1
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(b1_header.hash())));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2_header.hash(), 0))));
		//phase-2
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(b2_header.hash())));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1_header.hash(), 0))));
		//phase-3
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(b1_header.hash())));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b2_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_bob_watcher);
		//phase-1
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2_header.hash(), 1))));
		//phase-2
		assert_eq!(stream.next(), Some(TransactionStatus::Retracted(b2_header.hash())));
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		//phase-3
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b2_header.hash(), 1))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b2_header.hash(), 1))));
		assert_eq!(stream.next(), None);
	}
}

#[test]
fn two_blocks_delayed_finalization_works() {
	sp_tracing::try_init_simple();
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	let a_header = api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let from_alice = uxt(Alice, 1);
	let from_bob = uxt(Bob, 2);
	let from_charlie = uxt(Charlie, 3);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Bob.into());
	api.increment_nonce(Charlie.into());

	let from_alice_watcher;
	let from_bob_watcher;
	let from_charlie_watcher;
	let b1_header;
	let c1_header;
	let d1_header;

	// block B1
	{
		from_alice_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_alice.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(a_header.hash(), vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 1);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B1");
		b1_header = header;
	}

	// block C1
	{
		from_bob_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_bob.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(b1_header.hash(), vec![from_bob.clone()], true);
		assert_eq!(pool.status().ready, 2);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> C1");
		c1_header = header;
	}

	// block D1
	{
		from_charlie_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_charlie.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(c1_header.hash(), vec![from_charlie.clone()], true);
		assert_eq!(pool.status().ready, 3);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> D1");
		d1_header = header;
	}

	{
		let event = ChainEvent::Finalized { hash: a_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 3);
	}

	{
		let event = ChainEvent::NewBestBlock { hash: d1_header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	{
		let event = ChainEvent::Finalized {
			hash: c1_header.hash(),
			tree_route: Arc::from(vec![b1_header.hash()]),
		};
		block_on(pool.maintain(event));
	}

	// this is to collect events from_charlie_watcher and make sure nothing was retracted
	{
		let event = ChainEvent::Finalized { hash: d1_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	{
		let mut stream = futures::executor::block_on_stream(from_alice_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_bob_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((c1_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((c1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_charlie_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((d1_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((d1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}
}

#[test]
fn delayed_finalization_does_not_retract() {
	sp_tracing::try_init_simple();
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	let a_header = api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let from_alice = uxt(Alice, 1);
	let from_bob = uxt(Bob, 2);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Bob.into());

	let from_alice_watcher;
	let from_bob_watcher;
	let b1_header;
	let c1_header;

	// block B1
	{
		from_alice_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_alice.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(a_header.hash(), vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 1);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B1");
		b1_header = header;
	}

	// block C1
	{
		from_bob_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_bob.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(b1_header.hash(), vec![from_bob.clone()], true);
		assert_eq!(pool.status().ready, 2);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> C1");
		c1_header = header;
	}

	{
		// phase-0
		let event = ChainEvent::NewBestBlock { hash: b1_header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 1);
	}

	{
		// phase-1
		let event = ChainEvent::NewBestBlock { hash: c1_header.hash(), tree_route: None };
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	{
		// phase-2
		let event = ChainEvent::Finalized { hash: b1_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	{
		// phase-3
		let event = ChainEvent::Finalized { hash: c1_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	{
		let mut stream = futures::executor::block_on_stream(from_alice_watcher);
		//phase-0
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1_header.hash(), 0))));
		//phase-2
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_bob_watcher);
		//phase-0
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		//phase-1
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((c1_header.hash(), 0))));
		//phase-3
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((c1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}
}

#[test]
fn best_block_after_finalization_does_not_retract() {
	sp_tracing::try_init_simple();
	let api = TestApi::empty();
	// starting block A1 (last finalized.)
	let a_header = api.push_block(1, vec![], true);

	let pool = create_basic_pool(api);
	let api = pool.api();

	let from_alice = uxt(Alice, 1);
	let from_bob = uxt(Bob, 2);
	api.increment_nonce(Alice.into());
	api.increment_nonce(Bob.into());

	let from_alice_watcher;
	let from_bob_watcher;
	let b1_header;
	let c1_header;

	// block B1
	{
		from_alice_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_alice.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(a_header.hash(), vec![from_alice.clone()], true);
		assert_eq!(pool.status().ready, 1);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> B1");
		b1_header = header;
	}

	// block C1
	{
		from_bob_watcher = block_on(pool.submit_and_watch(
			api.expect_hash_from_number(1),
			SOURCE,
			from_bob.clone(),
		))
		.expect("1. Imported");
		let header = api.push_block_with_parent(b1_header.hash(), vec![from_bob.clone()], true);
		assert_eq!(pool.status().ready, 2);

		trace!(target: LOG_TARGET, hash = ?header.hash(), header = ?header, ">> C1");
		c1_header = header;
	}

	{
		let event = ChainEvent::Finalized { hash: a_header.hash(), tree_route: Arc::from(vec![]) };
		block_on(pool.maintain(event));
	}

	{
		let event = ChainEvent::Finalized {
			hash: c1_header.hash(),
			tree_route: Arc::from(vec![a_header.hash(), b1_header.hash()]),
		};
		block_on(pool.maintain(event));
		assert_eq!(pool.status().ready, 0);
	}

	{
		let event = ChainEvent::NewBestBlock { hash: b1_header.hash(), tree_route: None };
		block_on(pool.maintain(event));
	}

	{
		let mut stream = futures::executor::block_on_stream(from_alice_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((b1_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((b1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}

	{
		let mut stream = futures::executor::block_on_stream(from_bob_watcher);
		assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		assert_eq!(stream.next(), Some(TransactionStatus::InBlock((c1_header.hash(), 0))));
		assert_eq!(stream.next(), Some(TransactionStatus::Finalized((c1_header.hash(), 0))));
		assert_eq!(stream.next(), None);
	}
}
