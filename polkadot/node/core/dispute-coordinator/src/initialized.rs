// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Dispute coordinator subsystem in initialized state (after first active leaf is received).

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
};

use futures::{
	channel::{mpsc, oneshot},
	FutureExt, StreamExt,
};

use sc_keystore::LocalKeystore;

use polkadot_node_primitives::{
	disputes::ValidCandidateVotes, CandidateVotes, DisputeStatus, SignedDisputeStatement,
	Timestamp, DISPUTE_WINDOW,
};
use polkadot_node_subsystem::{
	messages::{
		ApprovalVotingParallelMessage, BlockDescription, ChainSelectionMessage,
		DisputeCoordinatorMessage, DisputeDistributionMessage, ImportStatementsResult,
	},
	overseer, ActivatedLeaf, ActiveLeavesUpdate, FromOrchestra, OverseerSignal, RuntimeApiError,
};
use polkadot_node_subsystem_util::{
	runtime::{self, key_ownership_proof, submit_report_dispute_lost, RuntimeInfo},
	ControlledValidatorIndices,
};
use polkadot_primitives::{
	slashing,
	vstaging::{CandidateReceiptV2 as CandidateReceipt, ScrapedOnChainVotes},
	BlockNumber, CandidateHash, CompactStatement, DisputeStatement, DisputeStatementSet, Hash,
	SessionIndex, ValidDisputeStatementKind, ValidatorId, ValidatorIndex,
};
use schnellru::{LruMap, UnlimitedCompact};

use crate::{
	db::{self, v1::RecentDisputes},
	error::{log_error, FatalError, FatalResult, JfyiError, JfyiResult, Result},
	import::{CandidateEnvironment, CandidateVoteState},
	is_potential_spam,
	metrics::Metrics,
	scraping::ScrapedUpdates,
	status::{get_active_with_status, Clock},
	DisputeCoordinatorSubsystem, LOG_TARGET,
};

use super::{
	backend::Backend,
	make_dispute_message,
	participation::{
		self, Participation, ParticipationPriority, ParticipationRequest, ParticipationStatement,
		WorkerMessageReceiver,
	},
	scraping::ChainScraper,
	spam_slots::SpamSlots,
	OverlayedBackend,
};

/// How many blocks we import votes from per leaf update.
///
/// Since vote import is relatively slow, we have to limit the maximum amount of work we do on leaf
/// updates (and especially on startup) so the dispute coordinator won't be considered stalling.
const CHAIN_IMPORT_MAX_BATCH_SIZE: usize = 8;

// Initial data for `dispute-coordinator`. It is provided only at first start.
pub struct InitialData {
	pub participations: Vec<(ParticipationPriority, ParticipationRequest)>,
	pub votes: Vec<ScrapedOnChainVotes>,
	pub leaf: ActivatedLeaf,
}

/// After the first active leaves update we transition to `Initialized` state.
///
/// Before the first active leaves update we can't really do much. We cannot check incoming
/// statements for validity, we cannot query orderings, we have no valid `SessionInfo`,
/// ...
pub(crate) struct Initialized {
	keystore: Arc<LocalKeystore>,
	runtime_info: RuntimeInfo,
	/// We have the onchain state of disabled validators as well as the offchain
	/// state that is based on the lost disputes.
	offchain_disabled_validators: OffchainDisabledValidators,
	/// The indices of the controlled validators, cached by session.
	controlled_validator_indices: ControlledValidatorIndices,
	/// This is the highest `SessionIndex` seen via `ActiveLeavesUpdate`. It doesn't matter if it
	/// was cached successfully or not. It is used to detect ancient disputes.
	highest_session_seen: SessionIndex,
	/// Will be set to `true` if an error occurred during the last caching attempt
	gaps_in_cache: bool,
	spam_slots: SpamSlots,
	participation: Participation,
	scraper: ChainScraper,
	participation_receiver: WorkerMessageReceiver,
	/// Backlog of still to be imported votes from chain.
	///
	/// For some reason importing votes is relatively slow, if there is a large finality lag (~50
	/// blocks) we will be too slow importing all votes from unfinalized chains on startup
	/// (dispute-coordinator gets killed because of unresponsiveness).
	///
	/// https://github.com/paritytech/polkadot/issues/6912
	///
	/// To resolve this, we limit the amount of votes imported at once to
	/// `CHAIN_IMPORT_MAX_BATCH_SIZE` and put the rest here for later processing.
	chain_import_backlog: VecDeque<ScrapedOnChainVotes>,
	metrics: Metrics,
}

#[overseer::contextbounds(DisputeCoordinator, prefix = self::overseer)]
impl Initialized {
	/// Make initialized subsystem, ready to `run`.
	pub fn new(
		subsystem: DisputeCoordinatorSubsystem,
		runtime_info: RuntimeInfo,
		spam_slots: SpamSlots,
		scraper: ChainScraper,
		highest_session_seen: SessionIndex,
		gaps_in_cache: bool,
		offchain_disabled_validators: OffchainDisabledValidators,
		controlled_validator_indices: ControlledValidatorIndices,
	) -> Self {
		let DisputeCoordinatorSubsystem { config: _, store: _, keystore, metrics } = subsystem;

		let (participation_sender, participation_receiver) = mpsc::channel(1);
		let participation = Participation::new(participation_sender, metrics.clone());

		Self {
			keystore,
			runtime_info,
			offchain_disabled_validators,
			controlled_validator_indices,
			highest_session_seen,
			gaps_in_cache,
			spam_slots,
			scraper,
			participation,
			participation_receiver,
			chain_import_backlog: VecDeque::new(),
			metrics,
		}
	}

	/// Run the initialized subsystem.
	///
	/// `initial_data` is optional. It is passed on first start and is `None` on subsystem restarts.
	pub async fn run<B, Context>(
		mut self,
		mut ctx: Context,
		mut backend: B,
		mut initial_data: Option<InitialData>,
		clock: Box<dyn Clock>,
	) -> FatalResult<()>
	where
		B: Backend,
	{
		loop {
			let res =
				self.run_until_error(&mut ctx, &mut backend, &mut initial_data, &*clock).await;
			if let Ok(()) = res {
				gum::info!(target: LOG_TARGET, "received `Conclude` signal, exiting");
				return Ok(())
			}
			log_error(res)?;
		}
	}

	// Run the subsystem until an error is encountered or a `conclude` signal is received.
	// Most errors are non-fatal and should lead to another call to this function.
	//
	// A return value of `Ok` indicates that an exit should be made, while non-fatal errors
	// lead to another call to this function.
	async fn run_until_error<B, Context>(
		&mut self,
		ctx: &mut Context,
		backend: &mut B,
		initial_data: &mut Option<InitialData>,
		clock: &dyn Clock,
	) -> Result<()>
	where
		B: Backend,
	{
		if let Some(InitialData { participations, votes: on_chain_votes, leaf: first_leaf }) =
			initial_data.take()
		{
			for (priority, request) in participations {
				self.participation.queue_participation(ctx, priority, request).await?;
			}

			let mut overlay_db = OverlayedBackend::new(backend);

			self.process_chain_import_backlog(
				ctx,
				&mut overlay_db,
				on_chain_votes,
				clock.now(),
				first_leaf.hash,
			)
			.await;

			if !overlay_db.is_empty() {
				let ops = overlay_db.into_write_ops();
				backend.write(ops)?;
			}

			// Also provide first leaf to participation for good measure.
			self.participation
				.process_active_leaves_update(ctx, &ActiveLeavesUpdate::start_work(first_leaf))
				.await?;
		}

		loop {
			gum::trace!(target: LOG_TARGET, "Waiting for message");
			let mut overlay_db = OverlayedBackend::new(backend);
			let default_confirm = Box::new(|| Ok(()));
			let confirm_write =
				match MuxedMessage::receive(ctx, &mut self.participation_receiver).await? {
					MuxedMessage::Participation(msg) => {
						gum::trace!(target: LOG_TARGET, "MuxedMessage::Participation");
						let ParticipationStatement {
							session,
							candidate_hash,
							candidate_receipt,
							outcome,
						} = self.participation.get_participation_result(ctx, msg).await?;
						if let Some(valid) = outcome.validity() {
							gum::trace!(
								target: LOG_TARGET,
								?session,
								?candidate_hash,
								?valid,
								"Issuing local statement based on participation outcome."
							);
							self.issue_local_statement(
								ctx,
								&mut overlay_db,
								candidate_hash,
								candidate_receipt,
								session,
								valid,
								clock.now(),
							)
							.await?;
						} else {
							gum::warn!(target: LOG_TARGET, ?outcome, "Dispute participation failed");
						}
						default_confirm
					},
					MuxedMessage::Subsystem(msg) => match msg {
						FromOrchestra::Signal(OverseerSignal::Conclude) => return Ok(()),
						FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)) => {
							gum::trace!(target: LOG_TARGET, "OverseerSignal::ActiveLeaves");
							self.process_active_leaves_update(
								ctx,
								&mut overlay_db,
								update,
								clock.now(),
							)
							.await?;
							default_confirm
						},
						FromOrchestra::Signal(OverseerSignal::BlockFinalized(_, n)) => {
							gum::trace!(target: LOG_TARGET, "OverseerSignal::BlockFinalized");
							self.scraper.process_finalized_block(&n);
							default_confirm
						},
						FromOrchestra::Communication { msg } =>
							self.handle_incoming(ctx, &mut overlay_db, msg, clock.now()).await?,
					},
				};

			if !overlay_db.is_empty() {
				let ops = overlay_db.into_write_ops();
				backend.write(ops)?;
			}
			// even if the changeset was empty,
			// otherwise the caller will error.
			confirm_write()?;
		}
	}

	async fn process_active_leaves_update<Context>(
		&mut self,
		ctx: &mut Context,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		update: ActiveLeavesUpdate,
		now: u64,
	) -> Result<()> {
		gum::trace!(target: LOG_TARGET, timestamp = now, "Processing ActiveLeavesUpdate");
		let scraped_updates =
			self.scraper.process_active_leaves_update(ctx.sender(), &update).await?;
		log_error(
			self.participation
				.bump_to_priority_for_candidates(ctx, &scraped_updates.included_receipts)
				.await,
		)?;
		self.participation.process_active_leaves_update(ctx, &update).await?;

		if let Some(new_leaf) = update.activated {
			gum::trace!(
				target: LOG_TARGET,
				leaf_hash = ?new_leaf.hash,
				block_number = new_leaf.number,
				"Processing ActivatedLeaf"
			);

			let session_idx =
				self.runtime_info.get_session_index_for_child(ctx.sender(), new_leaf.hash).await;

			match session_idx {
				Ok(session_idx)
					if self.gaps_in_cache || session_idx > self.highest_session_seen =>
				{
					// Pin the block from the new session.
					self.runtime_info.pin_block(session_idx, new_leaf.unpin_handle);
					// Fetch the last `DISPUTE_WINDOW` number of sessions unless there are no gaps
					// in cache and we are not missing too many `SessionInfo`s
					let prune_up_to = session_idx.saturating_sub(DISPUTE_WINDOW.get() - 1);
					let fetch_lower_bound =
						if !self.gaps_in_cache && self.highest_session_seen > prune_up_to {
							self.highest_session_seen + 1
						} else {
							prune_up_to
						};

					// There is a new session. Perform a dummy fetch to cache it.
					for idx in fetch_lower_bound..=session_idx {
						if let Err(err) = self
							.runtime_info
							.get_session_info_by_index(ctx.sender(), new_leaf.hash, idx)
							.await
						{
							gum::debug!(
								target: LOG_TARGET,
								session_idx,
								leaf_hash = ?new_leaf.hash,
								?err,
								"Error caching SessionInfo on ActiveLeaves update"
							);
							self.gaps_in_cache = true;
						}
					}

					self.highest_session_seen = session_idx;

					db::v1::note_earliest_session(overlay_db, prune_up_to)?;
					self.spam_slots.prune_old(prune_up_to);
					self.offchain_disabled_validators.prune_old(prune_up_to);
				},
				Ok(_) => { /* no new session => nothing to cache */ },
				Err(err) => {
					gum::debug!(
						target: LOG_TARGET,
						?err,
						"Failed to update session cache for disputes - can't fetch session index",
					);
				},
			}

			let ScrapedUpdates { unapplied_slashes, on_chain_votes, .. } = scraped_updates;

			self.process_unapplied_slashes(ctx, new_leaf.hash, unapplied_slashes).await;

			gum::trace!(
				target: LOG_TARGET,
				timestamp = now,
				"Will process {} onchain votes",
				on_chain_votes.len()
			);

			self.process_chain_import_backlog(ctx, overlay_db, on_chain_votes, now, new_leaf.hash)
				.await;
		}

		gum::trace!(target: LOG_TARGET, timestamp = now, "Done processing ActiveLeavesUpdate");
		Ok(())
	}

	/// For each unapplied (past-session) slash, report an unsigned extrinsic
	/// to the runtime.
	async fn process_unapplied_slashes<Context>(
		&mut self,
		ctx: &mut Context,
		relay_parent: Hash,
		unapplied_slashes: Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)>,
	) {
		for (session_index, candidate_hash, pending) in unapplied_slashes {
			gum::info!(
				target: LOG_TARGET,
				?session_index,
				?candidate_hash,
				n_slashes = pending.keys.len(),
				"Processing unapplied validator slashes",
			);

			let pinned_hash = self.runtime_info.get_block_in_session(session_index);
			let inclusions = self.scraper.get_blocks_including_candidate(&candidate_hash);
			if pinned_hash.is_none() && inclusions.is_empty() {
				gum::info!(
					target: LOG_TARGET,
					?session_index,
					"Couldn't find blocks in the session for an unapplied slash",
				);
				return
			}

			// Find a relay block that we can use
			// to generate key ownership proof on.
			// We use inclusion parents as a fallback.
			let mut key_ownership_proofs = Vec::new();
			let mut dispute_proofs = Vec::new();

			let blocks_in_the_session =
				pinned_hash.into_iter().chain(inclusions.into_iter().map(|(_n, h)| h));
			for hash in blocks_in_the_session {
				for (validator_index, validator_id) in pending.keys.iter() {
					let res = key_ownership_proof(ctx.sender(), hash, validator_id.clone()).await;

					match res {
						Ok(Some(key_ownership_proof)) => {
							key_ownership_proofs.push(key_ownership_proof);
							let time_slot =
								slashing::DisputesTimeSlot::new(session_index, candidate_hash);
							let dispute_proof = slashing::DisputeProof {
								time_slot,
								kind: pending.kind,
								validator_index: *validator_index,
								validator_id: validator_id.clone(),
							};
							dispute_proofs.push(dispute_proof);
						},
						Ok(None) => {},
						Err(runtime::Error::RuntimeRequest(RuntimeApiError::NotSupported {
							..
						})) => {
							gum::debug!(
								target: LOG_TARGET,
								?session_index,
								?candidate_hash,
								?validator_id,
								"Key ownership proof not yet supported.",
							);
						},
						Err(error) => {
							gum::warn!(
								target: LOG_TARGET,
								?error,
								?session_index,
								?candidate_hash,
								?validator_id,
								"Could not generate key ownership proof",
							);
						},
					}
				}

				if !key_ownership_proofs.is_empty() {
					// If we found a parent that we can use, stop searching.
					// If one key ownership was resolved successfully, all of them should be.
					debug_assert_eq!(key_ownership_proofs.len(), pending.keys.len());
					break
				}
			}

			let expected_keys = pending.keys.len();
			let resolved_keys = key_ownership_proofs.len();
			if resolved_keys < expected_keys {
				gum::warn!(
					target: LOG_TARGET,
					?session_index,
					?candidate_hash,
					"Could not generate key ownership proofs for {} keys",
					expected_keys - resolved_keys,
				);
			}
			debug_assert_eq!(resolved_keys, dispute_proofs.len());

			for (key_ownership_proof, dispute_proof) in
				key_ownership_proofs.into_iter().zip(dispute_proofs.into_iter())
			{
				let validator_id = dispute_proof.validator_id.clone();

				gum::info!(
					target: LOG_TARGET,
					?session_index,
					?candidate_hash,
					key_ownership_proof_len = key_ownership_proof.len(),
					"Trying to submit a slashing report",
				);

				let res = submit_report_dispute_lost(
					ctx.sender(),
					relay_parent,
					dispute_proof,
					key_ownership_proof,
				)
				.await;

				match res {
					Err(runtime::Error::RuntimeRequest(RuntimeApiError::NotSupported {
						..
					})) => {
						gum::debug!(
							target: LOG_TARGET,
							?session_index,
							?candidate_hash,
							"Reporting pending slash not yet supported",
						);
					},
					Err(error) => {
						gum::warn!(
							target: LOG_TARGET,
							?error,
							?session_index,
							?candidate_hash,
							"Error reporting pending slash",
						);
					},
					Ok(Some(())) => {
						gum::info!(
							target: LOG_TARGET,
							?session_index,
							?candidate_hash,
							?validator_id,
							"Successfully reported pending slash",
						);
					},
					Ok(None) => {
						gum::debug!(
							target: LOG_TARGET,
							?session_index,
							?candidate_hash,
							?validator_id,
							"Duplicate pending slash report",
						);
					},
				}
			}
		}
	}

	/// Process one batch of our `chain_import_backlog`.
	///
	/// `new_votes` will be appended beforehand.
	async fn process_chain_import_backlog<Context>(
		&mut self,
		ctx: &mut Context,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		new_votes: Vec<ScrapedOnChainVotes>,
		now: u64,
		block_hash: Hash,
	) {
		let mut chain_import_backlog = std::mem::take(&mut self.chain_import_backlog);
		chain_import_backlog.extend(new_votes);
		let import_range =
			0..std::cmp::min(CHAIN_IMPORT_MAX_BATCH_SIZE, chain_import_backlog.len());
		// The `runtime-api` subsystem has an internal queue which serializes the execution,
		// so there is no point in running these in parallel
		for votes in chain_import_backlog.drain(import_range) {
			let res = self.process_on_chain_votes(ctx, overlay_db, votes, now, block_hash).await;
			match res {
				Ok(()) => {},
				Err(error) => {
					gum::warn!(target: LOG_TARGET, ?error, "Skipping scraping block due to error",);
				},
			};
		}
		self.chain_import_backlog = chain_import_backlog;
	}

	/// Scrapes on-chain votes (backing votes and concluded disputes) for a active leaf of the
	/// relay chain.
	async fn process_on_chain_votes<Context>(
		&mut self,
		ctx: &mut Context,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		votes: ScrapedOnChainVotes,
		now: u64,
		block_hash: Hash,
	) -> Result<()> {
		let ScrapedOnChainVotes { session, backing_validators_per_candidate, disputes } = votes;

		if backing_validators_per_candidate.is_empty() && disputes.is_empty() {
			return Ok(())
		}

		// Scraped on-chain backing votes for the candidates with
		// the new active leaf as if we received them via gossip.
		for (candidate_receipt, backers) in backing_validators_per_candidate {
			// Obtain the session info, for sake of `ValidatorId`s
			let relay_parent = candidate_receipt.descriptor.relay_parent();
			let session_info = match self
				.runtime_info
				.get_session_info_by_index(ctx.sender(), relay_parent, session)
				.await
			{
				Ok(extended_session_info) => &extended_session_info.session_info,
				Err(err) => {
					gum::warn!(
						target: LOG_TARGET,
						?session,
						?err,
						"Could not retrieve session info from RuntimeInfo",
					);
					return Ok(())
				},
			};

			let candidate_hash = candidate_receipt.hash();
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				?relay_parent,
				"Importing backing votes from chain for candidate"
			);
			let statements = backers
				.into_iter()
				.filter_map(|(validator_index, attestation)| {
					let validator_public: ValidatorId = session_info
						.validators
						.get(validator_index)
						.or_else(|| {
							gum::error!(
								target: LOG_TARGET,
								?session,
								?validator_index,
								"Missing public key for validator",
							);
							None
						})
						.cloned()?;
					let validator_signature = attestation.signature().clone();
					let valid_statement_kind =
						match attestation.to_compact_statement(candidate_hash) {
							CompactStatement::Seconded(_) =>
								ValidDisputeStatementKind::BackingSeconded(relay_parent),
							CompactStatement::Valid(_) =>
								ValidDisputeStatementKind::BackingValid(relay_parent),
						};
					debug_assert!(
						SignedDisputeStatement::new_checked(
							DisputeStatement::Valid(valid_statement_kind.clone()),
							candidate_hash,
							session,
							validator_public.clone(),
							validator_signature.clone(),
						).is_ok(),
						"Scraped backing votes had invalid signature! candidate: {:?}, session: {:?}, validator_public: {:?}, validator_index: {}",
						candidate_hash,
						session,
						validator_public,
						validator_index.0,
					);
					let signed_dispute_statement =
						SignedDisputeStatement::new_unchecked_from_trusted_source(
							DisputeStatement::Valid(valid_statement_kind.clone()),
							candidate_hash,
							session,
							validator_public,
							validator_signature,
						);
					Some((signed_dispute_statement, validator_index))
				})
				.collect();

			// Importantly, handling import statements for backing votes also
			// clears spam slots for any newly backed candidates
			let import_result = self
				.handle_import_statements(
					ctx,
					overlay_db,
					MaybeCandidateReceipt::Provides(candidate_receipt),
					session,
					statements,
					now,
				)
				.await?;
			match import_result {
				ImportStatementsResult::ValidImport => gum::trace!(
					target: LOG_TARGET,
					?relay_parent,
					?session,
					"Imported backing votes from chain"
				),
				ImportStatementsResult::InvalidImport => gum::warn!(
					target: LOG_TARGET,
					?relay_parent,
					?session,
					"Attempted import of on-chain backing votes failed"
				),
			}
		}

		// Import disputes from on-chain, this already went through a vote so it's assumed
		// as verified. This will only be stored, gossiping it is not necessary.
		for DisputeStatementSet { candidate_hash, session, statements } in disputes {
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				?session,
				"Importing dispute votes from chain for candidate"
			);
			let session_info = match self
				.runtime_info
				.get_session_info_by_index(ctx.sender(), block_hash, session)
				.await
			{
				Ok(extended_session_info) => &extended_session_info.session_info,
				Err(err) => {
					gum::warn!(
						target: LOG_TARGET,
						?candidate_hash,
						?session,
						?err,
						"Could not retrieve session info for recently concluded dispute"
					);
					continue
				},
			};

			let statements = statements
				.into_iter()
				.filter_map(|(dispute_statement, validator_index, validator_signature)| {
					let validator_public: ValidatorId = session_info
						.validators
						.get(validator_index)
						.or_else(|| {
							gum::error!(
								target: LOG_TARGET,
								?candidate_hash,
								?session,
								"Missing public key for validator {:?} that participated in concluded dispute",
								&validator_index
							);
							None
						})
						.cloned()?;

					Some((
						SignedDisputeStatement::new_unchecked_from_trusted_source(
							dispute_statement,
							candidate_hash,
							session,
							validator_public,
							validator_signature,
						),
						validator_index,
					))
				})
				.collect::<Vec<_>>();
			if statements.is_empty() {
				gum::debug!(target: LOG_TARGET, "Skipping empty from chain dispute import");
				continue
			}
			let import_result = self
				.handle_import_statements(
					ctx,
					overlay_db,
					// TODO <https://github.com/paritytech/polkadot/issues/4011>
					MaybeCandidateReceipt::AssumeBackingVotePresent(candidate_hash),
					session,
					statements,
					now,
				)
				.await?;
			match import_result {
				ImportStatementsResult::ValidImport => gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					"Imported statement of dispute from on-chain"
				),
				ImportStatementsResult::InvalidImport => gum::warn!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					"Attempted import of on-chain statement of dispute failed"
				),
			}
		}

		Ok(())
	}

	async fn handle_incoming<Context>(
		&mut self,
		ctx: &mut Context,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		message: DisputeCoordinatorMessage,
		now: Timestamp,
	) -> Result<Box<dyn FnOnce() -> JfyiResult<()>>> {
		match message {
			DisputeCoordinatorMessage::ImportStatements {
				candidate_receipt,
				session,
				statements,
				pending_confirmation,
			} => {
				gum::trace!(
					target: LOG_TARGET,
					candidate_hash = ?candidate_receipt.hash(),
					?session,
					"DisputeCoordinatorMessage::ImportStatements"
				);
				let outcome = self
					.handle_import_statements(
						ctx,
						overlay_db,
						MaybeCandidateReceipt::Provides(candidate_receipt),
						session,
						statements,
						now,
					)
					.await?;
				let report = move || match pending_confirmation {
					Some(pending_confirmation) => pending_confirmation
						.send(outcome)
						.map_err(|_| JfyiError::DisputeImportOneshotSend),
					None => Ok(()),
				};

				match outcome {
					ImportStatementsResult::InvalidImport => {
						report()?;
					},
					// In case of valid import, delay confirmation until actual disk write:
					ImportStatementsResult::ValidImport => return Ok(Box::new(report)),
				}
			},
			DisputeCoordinatorMessage::RecentDisputes(tx) => {
				gum::trace!(target: LOG_TARGET, "Loading recent disputes from db");
				let recent_disputes = if let Some(disputes) = overlay_db.load_recent_disputes()? {
					disputes
				} else {
					BTreeMap::new()
				};
				gum::trace!(target: LOG_TARGET, "Loaded recent disputes from db");

				let _ = tx.send(recent_disputes);
			},
			DisputeCoordinatorMessage::ActiveDisputes(tx) => {
				gum::trace!(target: LOG_TARGET, "DisputeCoordinatorMessage::ActiveDisputes");
				let recent_disputes = if let Some(disputes) = overlay_db.load_recent_disputes()? {
					disputes
				} else {
					BTreeMap::new()
				};

				let _ = tx.send(
					get_active_with_status(recent_disputes.into_iter(), now)
						.collect::<BTreeMap<_, _>>(),
				);
			},
			DisputeCoordinatorMessage::QueryCandidateVotes(query, tx) => {
				gum::trace!(target: LOG_TARGET, "DisputeCoordinatorMessage::QueryCandidateVotes");
				let mut query_output = Vec::new();
				for (session_index, candidate_hash) in query {
					if let Some(v) =
						overlay_db.load_candidate_votes(session_index, &candidate_hash)?
					{
						query_output.push((session_index, candidate_hash, v.into()));
					} else {
						gum::debug!(
							target: LOG_TARGET,
							session_index,
							"No votes found for candidate",
						);
					}
				}
				let _ = tx.send(query_output);
			},
			DisputeCoordinatorMessage::IssueLocalStatement(
				session,
				candidate_hash,
				candidate_receipt,
				valid,
			) => {
				gum::trace!(target: LOG_TARGET, "DisputeCoordinatorMessage::IssueLocalStatement");
				self.issue_local_statement(
					ctx,
					overlay_db,
					candidate_hash,
					candidate_receipt,
					session,
					valid,
					now,
				)
				.await?;
			},
			DisputeCoordinatorMessage::DetermineUndisputedChain {
				base: (base_number, base_hash),
				block_descriptions,
				tx,
			} => {
				gum::trace!(
					target: LOG_TARGET,
					"DisputeCoordinatorMessage::DetermineUndisputedChain"
				);

				let undisputed_chain = determine_undisputed_chain(
					overlay_db,
					base_number,
					base_hash,
					block_descriptions,
				)?;

				let _ = tx.send(undisputed_chain);
			},
		}

		Ok(Box::new(|| Ok(())))
	}

	// We use fatal result rather than result here. Reason being, We for example increase
	// spam slots in this function. If then the import fails for some non fatal and
	// unrelated reason, we should likely actually decrement previously incremented spam
	// slots again, for non fatal errors - which is cumbersome and actually not needed
	async fn handle_import_statements<Context>(
		&mut self,
		ctx: &mut Context,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		candidate_receipt: MaybeCandidateReceipt,
		session: SessionIndex,
		statements: Vec<(SignedDisputeStatement, ValidatorIndex)>,
		now: Timestamp,
	) -> FatalResult<ImportStatementsResult> {
		gum::trace!(target: LOG_TARGET, ?statements, "In handle import statements");
		if self.session_is_ancient(session) {
			// It is not valid to participate in an ancient dispute (spam?) or too new.
			return Ok(ImportStatementsResult::InvalidImport)
		}

		let candidate_hash = candidate_receipt.hash();
		let votes_in_db = overlay_db.load_candidate_votes(session, &candidate_hash)?;
		let relay_parent = match &candidate_receipt {
			MaybeCandidateReceipt::Provides(candidate_receipt) =>
				candidate_receipt.descriptor().relay_parent(),
			MaybeCandidateReceipt::AssumeBackingVotePresent(candidate_hash) => match &votes_in_db {
				Some(votes) => votes.candidate_receipt.descriptor().relay_parent(),
				None => {
					gum::warn!(
						target: LOG_TARGET,
						session,
						?candidate_hash,
						"Cannot obtain relay parent without `CandidateReceipt` available!"
					);
					return Ok(ImportStatementsResult::InvalidImport)
				},
			},
		};

		let env = match CandidateEnvironment::new(
			ctx,
			&mut self.runtime_info,
			session,
			relay_parent,
			self.offchain_disabled_validators.iter(session),
			&mut self.controlled_validator_indices,
		)
		.await
		{
			None => {
				gum::warn!(
					target: LOG_TARGET,
					session,
					"We are lacking a `SessionInfo` for handling import of statements."
				);

				return Ok(ImportStatementsResult::InvalidImport)
			},
			Some(env) => env,
		};

		let n_validators = env.validators().len();

		gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			?session,
			?n_validators,
			"Number of validators"
		);

		// In case we are not provided with a candidate receipt
		// we operate under the assumption, that a previous vote
		// which included a `CandidateReceipt` was seen.
		// This holds since every block is preceded by the `Backing`-phase.
		//
		// There is one exception: A sufficiently sophisticated attacker could prevent
		// us from seeing the backing votes by withholding arbitrary blocks, and hence we do
		// not have a `CandidateReceipt` available.
		let old_state = match votes_in_db.map(CandidateVotes::from) {
			Some(votes) => CandidateVoteState::new(votes, &env, now),
			None =>
				if let MaybeCandidateReceipt::Provides(candidate_receipt) = candidate_receipt {
					CandidateVoteState::new_from_receipt(candidate_receipt)
				} else {
					gum::warn!(
						target: LOG_TARGET,
						session,
						?candidate_hash,
						"Cannot import votes, without `CandidateReceipt` available!"
					);
					return Ok(ImportStatementsResult::InvalidImport)
				},
		};

		gum::trace!(target: LOG_TARGET, ?candidate_hash, ?session, "Loaded votes");

		let controlled_indices = env.controlled_indices();
		let own_statements = statements
			.iter()
			.filter(|(statement, validator_index)| {
				controlled_indices.contains(validator_index) &&
					*statement.candidate_hash() == candidate_hash
			})
			.cloned()
			.collect::<Vec<_>>();

		let import_result = {
			let intermediate_result = old_state.import_statements(&env, statements, now);

			// Handle approval vote import:
			//
			// See guide: We import on fresh disputes to maximize likelihood of fetching votes for
			// dead forks and once concluded to maximize time for approval votes to trickle in.
			if intermediate_result.is_freshly_disputed() ||
				intermediate_result.is_freshly_concluded()
			{
				gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					"Requesting approval signatures"
				);
				let (tx, rx) = oneshot::channel();
				// Use of unbounded channels justified because:
				// 1. Only triggered twice per dispute.
				// 2. Raising a dispute is costly (requires validation + recovery) by honest nodes,
				// dishonest nodes are limited by spam slots.
				// 3. Concluding a dispute is even more costly.
				// Therefore it is reasonable to expect a simple vote request to succeed way faster
				// than disputes are raised.
				// 4. We are waiting (and blocking the whole subsystem) on a response right after -
				// therefore even with all else failing we will never have more than
				// one message in flight at any given time.
				ctx.send_unbounded_message(
					ApprovalVotingParallelMessage::GetApprovalSignaturesForCandidate(
						candidate_hash,
						tx,
					),
				);

				match rx.await {
					Err(_) => {
						gum::warn!(
							target: LOG_TARGET,
							"Fetch for approval votes got cancelled, only expected during shutdown!"
						);
						intermediate_result
					},
					Ok(votes) => {
						gum::trace!(
							target: LOG_TARGET,
							count = votes.len(),
							"Successfully received approval votes."
						);
						intermediate_result.import_approval_votes(&env, votes, now)
					},
				}
			} else {
				gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					"Not requested approval signatures"
				);
				intermediate_result
			}
		};

		gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			?session,
			?n_validators,
			"Import result ready"
		);

		let new_state = import_result.new_state();

		let is_included = self.scraper.is_candidate_included(&candidate_hash);
		let is_backed = self.scraper.is_candidate_backed(&candidate_hash);
		let own_vote_missing = new_state.own_vote_missing();
		let is_disputed = new_state.is_disputed();
		let is_confirmed = new_state.is_confirmed();
		let is_disabled = |v: &ValidatorIndex| env.disabled_indices().contains(v);
		let potential_spam =
			is_potential_spam(&self.scraper, &new_state, &candidate_hash, is_disabled);
		let allow_participation = !potential_spam;

		gum::trace!(
			target: LOG_TARGET,
			?own_vote_missing,
			?potential_spam,
			?is_included,
			?candidate_hash,
			confirmed = ?new_state.is_confirmed(),
			has_invalid_voters = ?!import_result.new_invalid_voters().is_empty(),
			n_disabled_validators = ?env.disabled_indices().len(),
			"Is spam?"
		);

		// This check is responsible for all clearing of spam slots. It runs
		// whenever a vote is imported from on or off chain, and decrements
		// slots whenever a candidate is newly backed, confirmed, or has our
		// own vote.
		if !potential_spam {
			self.spam_slots.clear(&(session, candidate_hash));

		// Potential spam:
		} else if !import_result.new_invalid_voters().is_empty() {
			let mut free_spam_slots_available = false;
			// Only allow import if at least one validator voting invalid, has not exceeded
			// its spam slots:
			for index in import_result.new_invalid_voters() {
				// Disputes can only be triggered via an invalidity stating vote, thus we only
				// need to increase spam slots on invalid votes. (If we did not, we would also
				// increase spam slots for backing validators for example - as validators have to
				// provide some opposing vote for dispute-distribution).
				free_spam_slots_available |=
					self.spam_slots.add_unconfirmed(session, candidate_hash, *index);
			}
			if !free_spam_slots_available {
				gum::debug!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					invalid_voters = ?import_result.new_invalid_voters(),
					"Rejecting import because of full spam slots."
				);
				return Ok(ImportStatementsResult::InvalidImport)
			}
		}

		// Participate in dispute if we did not cast a vote before and actually have keys to cast a
		// local vote. Disputes should fall in one of the categories below, otherwise we will
		// refrain from participation:
		// - `is_included` lands in prioritised queue
		// - `is_confirmed` | `is_backed` lands in best effort queue
		// We don't participate in disputes on finalized candidates.
		if own_vote_missing && is_disputed && allow_participation {
			let priority = ParticipationPriority::with_priority_if(is_included);
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				?priority,
				"Queuing participation for candidate"
			);
			if priority.is_priority() {
				self.metrics.on_queued_priority_participation();
			} else {
				self.metrics.on_queued_best_effort_participation();
			}
			let request_timer = self.metrics.time_participation_pipeline();
			let r = self
				.participation
				.queue_participation(
					ctx,
					priority,
					ParticipationRequest::new(
						new_state.candidate_receipt().clone(),
						session,
						env.executor_params().clone(),
						request_timer,
					),
				)
				.await;
			log_error(r)?;
		} else {
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				?is_confirmed,
				?own_vote_missing,
				?is_disputed,
				?allow_participation,
				?is_included,
				?is_backed,
				"Will not queue participation for candidate"
			);

			if !allow_participation {
				self.metrics.on_refrained_participation();
			}
		}

		// Also send any already existing approval vote on new disputes:
		if import_result.is_freshly_disputed() {
			let our_approval_votes = new_state.own_approval_votes().into_iter().flatten();
			for (validator_index, sig) in our_approval_votes {
				let pub_key = match env.validators().get(validator_index) {
					None => {
						gum::error!(
							target: LOG_TARGET,
							?validator_index,
							?session,
							"Could not find pub key in `SessionInfo` for our own approval vote!"
						);
						continue
					},
					Some(k) => k,
				};
				let statement = SignedDisputeStatement::new_unchecked_from_trusted_source(
					DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking),
					candidate_hash,
					session,
					pub_key.clone(),
					sig.clone(),
				);
				gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					?validator_index,
					"Sending out own approval vote"
				);
				match make_dispute_message(
					env.session_info(),
					&new_state.votes(),
					statement,
					validator_index,
				) {
					Err(err) => {
						gum::error!(
							target: LOG_TARGET,
							?err,
							"No ongoing dispute, but we checked there is one!"
						);
					},
					Ok(dispute_message) => {
						ctx.send_message(DisputeDistributionMessage::SendDispute(dispute_message))
							.await;
					},
				};
			}
		}

		// All good, update recent disputes if state has changed:
		if let Some(new_status) = new_state.dispute_status() {
			// Only bother with db access, if there was an actual change.
			if import_result.dispute_state_changed() {
				let mut recent_disputes = overlay_db.load_recent_disputes()?.unwrap_or_default();

				let status =
					recent_disputes.entry((session, candidate_hash)).or_insert_with(|| {
						gum::info!(
							target: LOG_TARGET,
							?candidate_hash,
							session,
							"New dispute initiated for candidate.",
						);
						DisputeStatus::active()
					});

				*status = *new_status;

				gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					?status,
					has_concluded_for = ?new_state.has_concluded_for(),
					has_concluded_against = ?new_state.has_concluded_against(),
					"Writing recent disputes with updates for candidate"
				);
				overlay_db.write_recent_disputes(recent_disputes);
			}
		}

		// Notify ChainSelection if a dispute has concluded against a candidate. ChainSelection
		// will need to mark the candidate's relay parent as reverted.
		if import_result.has_fresh_byzantine_threshold_against() {
			let blocks_including = self.scraper.get_blocks_including_candidate(&candidate_hash);
			for (parent_block_number, parent_block_hash) in &blocks_including {
				gum::warn!(
					target: LOG_TARGET,
					?candidate_hash,
					?parent_block_number,
					?parent_block_hash,
					"Dispute has just concluded against the candidate hash noted. Its parent will be marked as reverted."
				);
			}
			if blocks_including.len() > 0 {
				ctx.send_message(ChainSelectionMessage::RevertBlocks(blocks_including)).await;
			} else {
				gum::warn!(
					target: LOG_TARGET,
					?candidate_hash,
					?session,
					"Could not find an including block for candidate against which a dispute has concluded."
				);
			}
		}

		// Update metrics:
		if import_result.is_freshly_disputed() {
			self.metrics.on_open();
		}
		self.metrics.on_valid_votes(import_result.imported_valid_votes());
		self.metrics.on_invalid_votes(import_result.imported_invalid_votes());
		gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			?session,
			imported_approval_votes = ?import_result.imported_approval_votes(),
			imported_valid_votes = ?import_result.imported_valid_votes(),
			imported_invalid_votes = ?import_result.imported_invalid_votes(),
			total_valid_votes = ?import_result.new_state().votes().valid.raw().len(),
			total_invalid_votes = ?import_result.new_state().votes().invalid.len(),
			confirmed = ?import_result.new_state().is_confirmed(),
			"Import summary"
		);

		self.metrics.on_approval_votes(import_result.imported_approval_votes());
		if import_result.is_freshly_concluded_for() {
			gum::info!(
				target: LOG_TARGET,
				?candidate_hash,
				session,
				"Dispute on candidate concluded with 'valid' result",
			);
			for (statement, validator_index) in own_statements.iter() {
				if statement.statement().indicates_invalidity() {
					gum::warn!(
						target: LOG_TARGET,
						?candidate_hash,
						?validator_index,
						"Voted against a candidate that was concluded valid.",
					);
				}
			}
			for validator_index in new_state.votes().invalid.keys() {
				gum::info!(
					target: LOG_TARGET,
					?candidate_hash,
					?validator_index,
					?session,
					"Disabled offchain for voting invalid against a valid candidate",
				);
				self.offchain_disabled_validators
					.insert_against_valid(session, *validator_index);
			}
			self.metrics.on_concluded_valid();
		}
		if import_result.is_freshly_concluded_against() {
			gum::info!(
				target: LOG_TARGET,
				?candidate_hash,
				session,
				"Dispute on candidate concluded with 'invalid' result",
			);
			for (statement, validator_index) in own_statements.iter() {
				if statement.statement().indicates_validity() {
					gum::warn!(
						target: LOG_TARGET,
						?candidate_hash,
						?validator_index,
						"Voted for a candidate that was concluded invalid.",
					);
				}
			}
			for (validator_index, (kind, _sig)) in new_state.votes().valid.raw() {
				let is_backer = kind.is_backing();
				gum::info!(
					target: LOG_TARGET,
					?candidate_hash,
					?validator_index,
					?session,
					?is_backer,
					"Disabled offchain for voting valid for an invalid candidate",
				);
				self.offchain_disabled_validators.insert_for_invalid(
					session,
					*validator_index,
					is_backer,
				);
			}
			self.metrics.on_concluded_invalid();
		}

		// After validators are disabled, revisit active disputes to unactivate those where all
		// raising parties are now disabled
		if import_result.is_freshly_concluded_for() || import_result.is_freshly_concluded_against()
		{
			self.revisit_active_disputes_after_disabling(overlay_db, session)?;
		}

		// Only write when votes have changed.
		if let Some(votes) = import_result.into_updated_votes() {
			overlay_db.write_candidate_votes(session, candidate_hash, votes.into());
		}

		Ok(ImportStatementsResult::ValidImport)
	}

	async fn issue_local_statement<Context>(
		&mut self,
		ctx: &mut Context,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		candidate_hash: CandidateHash,
		candidate_receipt: CandidateReceipt,
		session: SessionIndex,
		valid: bool,
		now: Timestamp,
	) -> Result<()> {
		gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			?session,
			?valid,
			?now,
			"Issuing local statement for candidate!"
		);

		// Load environment:
		let env = match CandidateEnvironment::new(
			ctx,
			&mut self.runtime_info,
			session,
			candidate_receipt.descriptor.relay_parent(),
			self.offchain_disabled_validators.iter(session),
			&mut self.controlled_validator_indices,
		)
		.await
		{
			None => {
				gum::warn!(
					target: LOG_TARGET,
					session,
					"Missing info for session which has an active dispute",
				);

				return Ok(())
			},
			Some(env) => env,
		};

		let votes = overlay_db
			.load_candidate_votes(session, &candidate_hash)?
			.map(CandidateVotes::from)
			.unwrap_or_else(|| CandidateVotes {
				candidate_receipt: candidate_receipt.clone(),
				valid: ValidCandidateVotes::new(),
				invalid: BTreeMap::new(),
			});

		// Sign a statement for each validator index we control which has
		// not already voted. This should generally be maximum 1 statement.
		let voted_indices = votes.voted_indices();
		let mut statements = Vec::new();

		let controlled_indices = env.controlled_indices();
		for index in controlled_indices {
			if voted_indices.contains(&index) {
				continue
			}

			let keystore = self.keystore.clone() as Arc<_>;
			let res = SignedDisputeStatement::sign_explicit(
				&keystore,
				valid,
				candidate_hash,
				session,
				env.validators()
					.get(*index)
					.expect("`controlled_indices` are derived from `validators`; qed")
					.clone(),
			);

			match res {
				Ok(Some(signed_dispute_statement)) => {
					statements.push((signed_dispute_statement, *index));
				},
				Ok(None) => {},
				Err(err) => {
					gum::error!(
						target: LOG_TARGET,
						?err,
						"Encountered keystore error while signing dispute statement",
					);
				},
			}
		}

		// Get our message out:
		for (statement, index) in &statements {
			let dispute_message =
				match make_dispute_message(env.session_info(), &votes, statement.clone(), *index) {
					Err(err) => {
						gum::debug!(target: LOG_TARGET, ?err, "Creating dispute message failed.");
						continue
					},
					Ok(dispute_message) => dispute_message,
				};

			ctx.send_message(DisputeDistributionMessage::SendDispute(dispute_message)).await;
		}

		// Do import
		if !statements.is_empty() {
			match self
				.handle_import_statements(
					ctx,
					overlay_db,
					MaybeCandidateReceipt::Provides(candidate_receipt),
					session,
					statements,
					now,
				)
				.await?
			{
				ImportStatementsResult::InvalidImport => {
					gum::error!(
						target: LOG_TARGET,
						?candidate_hash,
						?session,
						"`handle_import_statements` considers our own votes invalid!"
					);
				},
				ImportStatementsResult::ValidImport => {
					gum::trace!(
						target: LOG_TARGET,
						?candidate_hash,
						?session,
						"`handle_import_statements` successfully imported our vote!"
					);
				},
			}
		}

		Ok(())
	}

	fn session_is_ancient(&self, session_idx: SessionIndex) -> bool {
		return session_idx < self.highest_session_seen.saturating_sub(DISPUTE_WINDOW.get() - 1)
	}

	/// Revisit active non-confirmed disputes after validators have been disabled.
	/// Unactivates disputes where all raising parties (invalid voters) are now disabled.
	fn revisit_active_disputes_after_disabling(
		&mut self,
		overlay_db: &mut OverlayedBackend<'_, impl Backend>,
		session: SessionIndex,
	) -> FatalResult<()> {
		let mut recent_disputes = overlay_db.load_recent_disputes()?.unwrap_or_default();
		let mut disputes_to_remove = Vec::new();

		// Create session bounds for efficient iteration
		let session_start = (session, CandidateHash(Hash::zero()));
		let session_end = (session + 1, CandidateHash(Hash::zero()));

		for ((dispute_session, candidate_hash), status) in
			recent_disputes.range(&session_start..&session_end)
		{
			debug_assert_eq!(session, *dispute_session);
			// Only check unconfirmed
			if status.is_confirmed_concluded() {
				continue
			}
			let Some(votes) = overlay_db.load_candidate_votes(*dispute_session, candidate_hash)?
			else {
				continue
			};
			// Check if all invalid voters (raising parties) are disabled
			if !votes.invalid.is_empty() &&
				votes.invalid.iter().all(|(_, validator_index, _)| {
					self.offchain_disabled_validators.is_disabled(session, *validator_index)
				}) {
				disputes_to_remove.push((*dispute_session, *candidate_hash));

				gum::info!(
					target: LOG_TARGET,
					session = dispute_session,
					?candidate_hash,
					invalid_voters = ?votes.invalid.iter().map(|(_, idx, _)| *idx).collect::<Vec<_>>(),
					"Unactivating dispute where all raising parties are now disabled"
				);
			}
		}

		// Remove them from RecentDisputes (setting status to inactive)
		if !disputes_to_remove.is_empty() {
			for key in disputes_to_remove {
				recent_disputes.remove(&key);
				self.metrics.on_unactivated_dispute();
			}
			overlay_db.write_recent_disputes(recent_disputes);
		}

		Ok(())
	}
}

/// Messages to be handled in this subsystem.
enum MuxedMessage {
	/// Messages from other subsystems.
	Subsystem(FromOrchestra<DisputeCoordinatorMessage>),
	/// Messages from participation workers.
	Participation(participation::WorkerMessage),
}

#[overseer::contextbounds(DisputeCoordinator, prefix = self::overseer)]
impl MuxedMessage {
	async fn receive<Context>(
		ctx: &mut Context,
		from_sender: &mut participation::WorkerMessageReceiver,
	) -> FatalResult<Self> {
		// We are only fusing here to make `select` happy, in reality we will quit if the stream
		// ends.
		let from_overseer = ctx.recv().fuse();
		futures::pin_mut!(from_overseer, from_sender);
		futures::select!(
			msg = from_overseer => Ok(Self::Subsystem(msg.map_err(FatalError::SubsystemReceive)?)),
			msg = from_sender.next() => Ok(Self::Participation(msg.ok_or(FatalError::ParticipationWorkerReceiverExhausted)?)),
		)
	}
}

#[derive(Debug, Clone)]
enum MaybeCandidateReceipt {
	/// Directly provides the candidate receipt.
	Provides(CandidateReceipt),
	/// Assumes it was seen before by means of seconded message.
	AssumeBackingVotePresent(CandidateHash),
}

impl MaybeCandidateReceipt {
	/// Retrieve `CandidateHash` for the corresponding candidate.
	pub fn hash(&self) -> CandidateHash {
		match self {
			Self::Provides(receipt) => receipt.hash(),
			Self::AssumeBackingVotePresent(hash) => *hash,
		}
	}
}

/// Determine the best block and its block number.
/// Assumes `block_descriptions` are sorted from the one
/// with the lowest `BlockNumber` to the highest.
fn determine_undisputed_chain(
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	base_number: BlockNumber,
	base_hash: Hash,
	block_descriptions: Vec<BlockDescription>,
) -> Result<(BlockNumber, Hash)> {
	let last = block_descriptions
		.last()
		.map(|e| (base_number + block_descriptions.len() as BlockNumber, e.block_hash))
		.unwrap_or((base_number, base_hash));

	// Fast path for no disputes.
	let recent_disputes = match overlay_db.load_recent_disputes()? {
		None => return Ok(last),
		Some(a) if a.is_empty() => return Ok(last),
		Some(a) => a,
	};

	let is_possibly_invalid = |session, candidate_hash| {
		recent_disputes
			.get(&(session, candidate_hash))
			.map_or(false, |status| status.is_possibly_invalid())
	};

	for (i, BlockDescription { session, candidates, .. }) in block_descriptions.iter().enumerate() {
		if candidates.iter().any(|c| is_possibly_invalid(*session, *c)) {
			if i == 0 {
				return Ok((base_number, base_hash))
			} else {
				return Ok((base_number + i as BlockNumber, block_descriptions[i - 1].block_hash))
			}
		}
	}

	Ok(last)
}

/// Ideally, we want to use the top `byzantine_threshold` offenders here based on the amount of
/// stake slashed. However, given that slashing might be applied with a delay, we want to have
/// some list of offenders as soon as disputes conclude offchain. This list only approximates
/// the top offenders and only accounts for lost disputes. But that should be good enough to
/// prevent spam attacks.
#[derive(Default)]
pub struct OffchainDisabledValidators {
	per_session: BTreeMap<SessionIndex, LostSessionDisputes>,
}

struct LostSessionDisputes {
	// We separate lost disputes to prioritize "for invalid" offenders. And among those, we
	// prioritize backing votes the most. There's no need to limit the size of these sets, as they
	// are already limited by the number of validators in the session. We use `LruMap` to ensure
	// the iteration order prioritizes most recently disputes lost over older ones in case we reach
	// the limit.
	backers_for_invalid: LruMap<ValidatorIndex, (), UnlimitedCompact>,
	for_invalid: LruMap<ValidatorIndex, (), UnlimitedCompact>,
	against_valid: LruMap<ValidatorIndex, (), UnlimitedCompact>,
}

impl Default for LostSessionDisputes {
	fn default() -> Self {
		Self {
			backers_for_invalid: LruMap::new(UnlimitedCompact),
			for_invalid: LruMap::new(UnlimitedCompact),
			against_valid: LruMap::new(UnlimitedCompact),
		}
	}
}

impl OffchainDisabledValidators {
	/// Creates a new instance populated from concluded disputes
	pub fn new_from_state(
		disputes: &RecentDisputes,
		load_candidate_votes: impl Fn(SessionIndex, &CandidateHash) -> Option<CandidateVotes>,
		earliest_session: SessionIndex,
	) -> Self {
		let mut disabled_validators = Self::default();

		// Process concluded disputes to identify validators that should be disabled
		for ((session, candidate_hash), dispute_status) in disputes {
			let session = *session;
			// Only process concluded disputes
			if dispute_status.concluded_at().is_none() {
				continue
			}
			if session < earliest_session {
				continue
			}

			// Get votes for this dispute
			let votes = match load_candidate_votes(session, candidate_hash) {
				Some(votes) => votes,
				None => continue,
			};

			// Process votes based on dispute outcome
			if dispute_status.has_concluded_for() {
				// Dispute concluded with candidate being valid - track validators that voted
				// against
				for (validator_index, _) in votes.invalid.iter() {
					disabled_validators.insert_against_valid(session, *validator_index);
				}
			} else if dispute_status.has_concluded_against() {
				// Dispute concluded with candidate being invalid - track validators that voted for
				for (validator_index, (kind, _)) in votes.valid.raw().iter() {
					let is_backer = kind.is_backing();
					disabled_validators.insert_for_invalid(session, *validator_index, is_backer);
				}
			}
		}

		disabled_validators
	}

	/// Prune state for ancient disputes.
	pub fn prune_old(&mut self, up_to_excluding: SessionIndex) {
		// split_off returns everything after the given key, including the key.
		let mut relevant = self.per_session.split_off(&up_to_excluding);
		std::mem::swap(&mut relevant, &mut self.per_session);
	}

	/// Disable a validator who voted for an invalid candidate.
	pub fn insert_for_invalid(
		&mut self,
		session_index: SessionIndex,
		validator_index: ValidatorIndex,
		is_backer: bool,
	) {
		let entry = self.per_session.entry(session_index).or_default();
		if is_backer {
			entry.backers_for_invalid.insert(validator_index, ());
		} else {
			entry.for_invalid.insert(validator_index, ());
		}
	}

	/// Disable a validator who voted against a valid candidate.
	pub fn insert_against_valid(
		&mut self,
		session_index: SessionIndex,
		validator_index: ValidatorIndex,
	) {
		self.per_session
			.entry(session_index)
			.or_default()
			.against_valid
			.insert(validator_index, ());
	}

	/// Iterate over all validators that are offchain disabled.
	/// The order of iteration prioritizes `for_invalid` offenders (and backers among those) over
	/// `against_valid` offenders. And most recently lost disputes over older ones.
	/// NOTE: the iterator might contain duplicates.
	pub fn iter(&self, session_index: SessionIndex) -> impl Iterator<Item = ValidatorIndex> + '_ {
		self.per_session.get(&session_index).into_iter().flat_map(|e| {
			e.backers_for_invalid
				.iter()
				.chain(e.for_invalid.iter())
				.chain(e.against_valid.iter())
				.map(|(i, _)| *i)
		})
	}

	/// Check if a validator is disabled for a given session.
	pub fn is_disabled(
		&self,
		session_index: SessionIndex,
		validator_index: ValidatorIndex,
	) -> bool {
		self.per_session
			.get(&session_index)
			.map(|session_disputes| {
				session_disputes.backers_for_invalid.peek(&validator_index).is_some() ||
					session_disputes.for_invalid.peek(&validator_index).is_some() ||
					session_disputes.against_valid.peek(&validator_index).is_some()
			})
			.unwrap_or(false)
	}
}
