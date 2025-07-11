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

//! Vote import logic.
//!
//! This module encapsulates the actual logic for importing new votes and provides easy access of
//! the current state for votes for a particular candidate.
//!
//! In particular there is `CandidateVoteState` which tells what can be concluded for a particular
//! set of votes. E.g. whether a dispute is ongoing, whether it is confirmed, concluded, ..
//!
//! Then there is `ImportResult` which reveals information about what changed once additional votes
//! got imported on top of an existing `CandidateVoteState` and reveals "dynamic" information, like
//! whether due to the import a dispute was raised/got confirmed, ...

use std::collections::{BTreeMap, HashMap, HashSet};

use polkadot_node_primitives::{
	disputes::ValidCandidateVotes, CandidateVotes, DisputeStatus, SignedDisputeStatement, Timestamp,
};
use polkadot_node_subsystem::overseer;
use polkadot_node_subsystem_util::{runtime::RuntimeInfo, ControlledValidatorIndices};
use polkadot_primitives::{
	vstaging::CandidateReceiptV2 as CandidateReceipt, CandidateHash, DisputeStatement,
	ExecutorParams, Hash, IndexedVec, SessionIndex, SessionInfo, ValidDisputeStatementKind,
	ValidatorId, ValidatorIndex, ValidatorSignature,
};

use crate::LOG_TARGET;

/// (Session) environment of a candidate.
pub struct CandidateEnvironment<'a> {
	/// The session the candidate appeared in.
	session_index: SessionIndex,
	/// Session for above index.
	session: &'a SessionInfo,
	/// Executor parameters for the session.
	executor_params: &'a ExecutorParams,
	/// Validator indices controlled by this node.
	controlled_indices: HashSet<ValidatorIndex>,
	/// Indices of on-chain disabled validators at the `relay_parent` combined
	/// with the off-chain state.
	disabled_indices: HashSet<ValidatorIndex>,
}

#[overseer::contextbounds(DisputeCoordinator, prefix = self::overseer)]
impl<'a> CandidateEnvironment<'a> {
	/// Create `CandidateEnvironment`.
	///
	/// Return: `None` in case session is outside of session window.
	pub async fn new<Context>(
		ctx: &mut Context,
		runtime_info: &'a mut RuntimeInfo,
		session_index: SessionIndex,
		relay_parent: Hash,
		disabled_offchain: impl IntoIterator<Item = ValidatorIndex>,
		controlled_indices: &mut ControlledValidatorIndices,
	) -> Option<CandidateEnvironment<'a>> {
		let disabled_onchain = runtime_info
			.get_disabled_validators(ctx.sender(), relay_parent)
			.await
			.unwrap_or_else(|err| {
				gum::info!(target: LOG_TARGET, ?err, "Failed to get disabled validators");
				Vec::new()
			});

		let (session, executor_params) = match runtime_info
			.get_session_info_by_index(ctx.sender(), relay_parent, session_index)
			.await
		{
			Ok(extended_session_info) =>
				(&extended_session_info.session_info, &extended_session_info.executor_params),
			Err(_) => return None,
		};

		let n_validators = session.validators.len();
		let byzantine_threshold = polkadot_primitives::byzantine_threshold(n_validators);
		// combine on-chain with off-chain disabled validators
		// process disabled validators in the following order:
		// - on-chain disabled validators
		// - prioritized order of off-chain disabled validators
		// deduplicate the list and take at most `byzantine_threshold` validators
		let disabled_indices = {
			let mut d: HashSet<ValidatorIndex> = HashSet::new();
			for v in disabled_onchain.into_iter().chain(disabled_offchain.into_iter()) {
				if d.len() == byzantine_threshold {
					break
				}
				d.insert(v);
			}
			d
		};

		let controlled_indices = controlled_indices
			.get(session_index, &session.validators)
			.map_or(HashSet::new(), |index| HashSet::from([index]));

		Some(Self { session_index, session, executor_params, controlled_indices, disabled_indices })
	}

	/// Validators in the candidate's session.
	pub fn validators(&self) -> &IndexedVec<ValidatorIndex, ValidatorId> {
		&self.session.validators
	}

	/// `SessionInfo` for the candidate's session.
	pub fn session_info(&self) -> &SessionInfo {
		&self.session
	}

	/// Executor parameters for the candidate's session
	pub fn executor_params(&self) -> &ExecutorParams {
		&self.executor_params
	}

	/// Retrieve `SessionIndex` for this environment.
	pub fn session_index(&self) -> SessionIndex {
		self.session_index
	}

	/// Indices controlled by this node.
	pub fn controlled_indices(&'a self) -> &'a HashSet<ValidatorIndex> {
		&self.controlled_indices
	}

	/// Indices of off-chain and on-chain disabled validators.
	pub fn disabled_indices(&'a self) -> &'a HashSet<ValidatorIndex> {
		&self.disabled_indices
	}
}

/// Whether or not we already issued some statement about a candidate.
pub enum OwnVoteState {
	/// Our votes, if any.
	Voted(Vec<(ValidatorIndex, (DisputeStatement, ValidatorSignature))>),

	/// We are not a parachain validator in the session.
	///
	/// Hence we cannot vote.
	CannotVote,
}

impl OwnVoteState {
	fn new(votes: &CandidateVotes, env: &CandidateEnvironment) -> Self {
		let controlled_indices = env.controlled_indices();
		if controlled_indices.is_empty() {
			return Self::CannotVote
		}

		let our_valid_votes = controlled_indices
			.iter()
			.filter_map(|i| votes.valid.raw().get_key_value(i))
			.map(|(index, (kind, sig))| {
				(*index, (DisputeStatement::Valid(kind.clone()), sig.clone()))
			});
		let our_invalid_votes = controlled_indices
			.iter()
			.filter_map(|i| votes.invalid.get_key_value(i))
			.map(|(index, (kind, sig))| (*index, (DisputeStatement::Invalid(*kind), sig.clone())));

		Self::Voted(our_valid_votes.chain(our_invalid_votes).collect())
	}

	/// Is a vote from us missing but we are a validator able to vote?
	fn vote_missing(&self) -> bool {
		match self {
			Self::Voted(votes) if votes.is_empty() => true,
			Self::Voted(_) | Self::CannotVote => false,
		}
	}

	/// Get own approval votes, if any.
	///
	/// Empty iterator means, no approval votes. `None` means, there will never be any (we cannot
	/// vote).
	fn approval_votes(
		&self,
	) -> Option<impl Iterator<Item = (ValidatorIndex, &ValidatorSignature)>> {
		match self {
			Self::Voted(votes) => Some(votes.iter().filter_map(|(index, (kind, sig))| {
				if let DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking) = kind {
					Some((*index, sig))
				} else {
					None
				}
			})),
			Self::CannotVote => None,
		}
	}

	/// Get our votes if there are any.
	///
	/// Empty iterator means, no votes. `None` means, there will never be any (we cannot
	/// vote).
	fn votes(&self) -> Option<&Vec<(ValidatorIndex, (DisputeStatement, ValidatorSignature))>> {
		match self {
			Self::Voted(votes) => Some(&votes),
			Self::CannotVote => None,
		}
	}
}

/// Complete state of votes for a candidate.
///
/// All votes + information whether a dispute is ongoing, confirmed, concluded, whether we already
/// voted, ...
pub struct CandidateVoteState<Votes> {
	/// Votes already existing for the candidate + receipt.
	votes: Votes,

	/// Information about own votes:
	own_vote: OwnVoteState,

	/// Current dispute status, if there is any.
	dispute_status: Option<DisputeStatus>,

	/// Are there `byzantine threshold + 1` invalid votes
	byzantine_threshold_against: bool,
}

impl CandidateVoteState<CandidateVotes> {
	/// Create an empty `CandidateVoteState`
	///
	/// in case there have not been any previous votes.
	pub fn new_from_receipt(candidate_receipt: CandidateReceipt) -> Self {
		let votes = CandidateVotes {
			candidate_receipt,
			valid: ValidCandidateVotes::new(),
			invalid: BTreeMap::new(),
		};
		Self {
			votes,
			own_vote: OwnVoteState::CannotVote,
			dispute_status: None,
			byzantine_threshold_against: false,
		}
	}

	/// Create a new `CandidateVoteState` from already existing votes.
	pub fn new(votes: CandidateVotes, env: &CandidateEnvironment, now: Timestamp) -> Self {
		let own_vote = OwnVoteState::new(&votes, env);

		let n_validators = env.validators().len();

		let supermajority_threshold = polkadot_primitives::supermajority_threshold(n_validators);

		// We have a dispute, if we have votes on both sides, with at least one invalid vote
		// from non-disabled validator or with votes on both sides and confirmed.
		let has_non_disabled_invalid_votes =
			votes.invalid.keys().any(|i| !env.disabled_indices().contains(i));
		let byzantine_threshold = polkadot_primitives::byzantine_threshold(n_validators);
		let votes_on_both_sides = !votes.valid.raw().is_empty() && !votes.invalid.is_empty();
		let is_confirmed =
			votes_on_both_sides && (votes.voted_indices().len() > byzantine_threshold);
		let is_disputed =
			is_confirmed || (has_non_disabled_invalid_votes && !votes.valid.raw().is_empty());

		let (dispute_status, byzantine_threshold_against) = if is_disputed {
			let mut status = DisputeStatus::active();
			if is_confirmed {
				status = status.confirm();
			};
			let concluded_for = votes.valid.raw().len() >= supermajority_threshold;
			if concluded_for {
				status = status.conclude_for(now);
			};

			let concluded_against = votes.invalid.len() >= supermajority_threshold;
			if concluded_against {
				status = status.conclude_against(now);
			};
			(Some(status), votes.invalid.len() > byzantine_threshold)
		} else {
			(None, false)
		};

		Self { votes, own_vote, dispute_status, byzantine_threshold_against }
	}

	/// Import fresh statements.
	///
	/// Result will be a new state plus information about things that changed due to the import.
	pub fn import_statements(
		self,
		env: &CandidateEnvironment,
		statements: Vec<(SignedDisputeStatement, ValidatorIndex)>,
		now: Timestamp,
	) -> ImportResult {
		let (mut votes, old_state) = self.into_old_state();

		let mut new_invalid_voters = Vec::new();
		let mut imported_invalid_votes = 0;
		let mut imported_valid_votes = 0;

		let expected_candidate_hash = votes.candidate_receipt.hash();

		for (statement, val_index) in statements {
			if env
				.validators()
				.get(val_index)
				.map_or(true, |v| v != statement.validator_public())
			{
				gum::error!(
					target: LOG_TARGET,
					?val_index,
					session= ?env.session_index,
					claimed_key = ?statement.validator_public(),
					"Validator index doesn't match claimed key",
				);

				continue
			}
			if statement.candidate_hash() != &expected_candidate_hash {
				gum::error!(
					target: LOG_TARGET,
					?val_index,
					session= ?env.session_index,
					given_candidate_hash = ?statement.candidate_hash(),
					?expected_candidate_hash,
					"Vote is for unexpected candidate!",
				);
				continue
			}
			if statement.session_index() != env.session_index() {
				gum::error!(
					target: LOG_TARGET,
					?val_index,
					session= ?env.session_index,
					given_candidate_hash = ?statement.candidate_hash(),
					?expected_candidate_hash,
					"Vote is for unexpected session!",
				);
				continue
			}

			match statement.statement() {
				DisputeStatement::Valid(valid_kind) => {
					let fresh = votes.valid.insert_vote(
						val_index,
						valid_kind.clone(),
						statement.into_validator_signature(),
					);
					if fresh {
						imported_valid_votes += 1;
					}
				},
				DisputeStatement::Invalid(invalid_kind) => {
					let fresh = votes
						.invalid
						.insert(val_index, (*invalid_kind, statement.into_validator_signature()))
						.is_none();
					if fresh {
						new_invalid_voters.push(val_index);
						imported_invalid_votes += 1;
					}
				},
			}
		}

		let new_state = Self::new(votes, env, now);

		ImportResult {
			old_state,
			new_state,
			imported_invalid_votes,
			imported_valid_votes,
			imported_approval_votes: 0,
			new_invalid_voters,
		}
	}

	/// Retrieve `CandidateReceipt` in `CandidateVotes`.
	pub fn candidate_receipt(&self) -> &CandidateReceipt {
		&self.votes.candidate_receipt
	}

	/// Returns true if all the invalid votes are from disabled validators.
	pub fn invalid_votes_all_disabled(
		&self,
		mut is_disabled: impl FnMut(&ValidatorIndex) -> bool,
	) -> bool {
		self.votes.invalid.keys().all(|i| is_disabled(i))
	}

	/// Extract `CandidateVotes` for handling import of new statements.
	fn into_old_state(self) -> (CandidateVotes, CandidateVoteState<()>) {
		let CandidateVoteState { votes, own_vote, dispute_status, byzantine_threshold_against } =
			self;
		(
			votes,
			CandidateVoteState { votes: (), own_vote, dispute_status, byzantine_threshold_against },
		)
	}
}

impl<V> CandidateVoteState<V> {
	/// Whether or not we have an ongoing dispute.
	pub fn is_disputed(&self) -> bool {
		self.dispute_status.is_some()
	}

	/// Whether there is an ongoing confirmed dispute.
	///
	/// This checks whether there is a dispute ongoing and we have more than byzantine threshold
	/// votes.
	pub fn is_confirmed(&self) -> bool {
		self.dispute_status.map_or(false, |s| s.is_confirmed_concluded())
	}

	/// Are we a validator in the session, but have not yet voted?
	pub fn own_vote_missing(&self) -> bool {
		self.own_vote.vote_missing()
	}

	/// Own approval votes if any:
	pub fn own_approval_votes(
		&self,
	) -> Option<impl Iterator<Item = (ValidatorIndex, &ValidatorSignature)>> {
		self.own_vote.approval_votes()
	}

	/// Get own votes if there are any.
	pub fn own_votes(
		&self,
	) -> Option<&Vec<(ValidatorIndex, (DisputeStatement, ValidatorSignature))>> {
		self.own_vote.votes()
	}

	/// Whether or not there is a dispute and it has already enough valid votes to conclude.
	pub fn has_concluded_for(&self) -> bool {
		self.dispute_status.map_or(false, |s| s.has_concluded_for())
	}

	/// Whether or not there is a dispute and it has already enough invalid votes to conclude.
	pub fn has_concluded_against(&self) -> bool {
		self.dispute_status.map_or(false, |s| s.has_concluded_against())
	}

	/// Get access to the dispute status, in case there is one.
	pub fn dispute_status(&self) -> &Option<DisputeStatus> {
		&self.dispute_status
	}

	/// Access to underlying votes.
	pub fn votes(&self) -> &V {
		&self.votes
	}
}

/// An ongoing statement/vote import.
pub struct ImportResult {
	/// The state we had before importing new statements.
	old_state: CandidateVoteState<()>,
	/// The new state after importing the new statements.
	new_state: CandidateVoteState<CandidateVotes>,
	/// New invalid voters as of this import.
	new_invalid_voters: Vec<ValidatorIndex>,
	/// Number of successfully imported valid votes.
	imported_invalid_votes: u32,
	/// Number of successfully imported invalid votes.
	imported_valid_votes: u32,
	/// Number of approval votes imported via `import_approval_votes()`.
	///
	/// And only those: If normal import included approval votes, those are not counted here.
	///
	/// In other words, without a call `import_approval_votes()` this will always be 0.
	imported_approval_votes: u32,
}

impl ImportResult {
	/// Whether or not anything has changed due to the import.
	pub fn votes_changed(&self) -> bool {
		self.imported_valid_votes != 0 || self.imported_invalid_votes != 0
	}

	/// The dispute state has changed in some way.
	///
	/// - freshly disputed
	/// - freshly confirmed
	/// - freshly concluded (valid or invalid)
	pub fn dispute_state_changed(&self) -> bool {
		self.is_freshly_disputed() || self.is_freshly_confirmed() || self.is_freshly_concluded()
	}

	/// State as it was before import.
	pub fn old_state(&self) -> &CandidateVoteState<()> {
		&self.old_state
	}

	/// State after import
	pub fn new_state(&self) -> &CandidateVoteState<CandidateVotes> {
		&self.new_state
	}

	/// New "invalid" voters encountered during import.
	pub fn new_invalid_voters(&self) -> &Vec<ValidatorIndex> {
		&self.new_invalid_voters
	}

	/// Number of imported valid votes.
	pub fn imported_valid_votes(&self) -> u32 {
		self.imported_valid_votes
	}

	/// Number of imported invalid votes.
	pub fn imported_invalid_votes(&self) -> u32 {
		self.imported_invalid_votes
	}

	/// Number of imported approval votes.
	pub fn imported_approval_votes(&self) -> u32 {
		self.imported_approval_votes
	}

	/// Whether we now have a dispute and did not prior to the import.
	pub fn is_freshly_disputed(&self) -> bool {
		!self.old_state().is_disputed() && self.new_state().is_disputed()
	}

	/// Whether we just surpassed the byzantine threshold.
	pub fn is_freshly_confirmed(&self) -> bool {
		!self.old_state().is_confirmed() && self.new_state().is_confirmed()
	}

	/// Whether or not any dispute just concluded valid due to the import.
	pub fn is_freshly_concluded_for(&self) -> bool {
		!self.old_state().has_concluded_for() && self.new_state().has_concluded_for()
	}

	/// Whether or not any dispute just concluded invalid due to the import.
	pub fn is_freshly_concluded_against(&self) -> bool {
		!self.old_state().has_concluded_against() && self.new_state().has_concluded_against()
	}

	/// Whether or not any dispute just concluded either invalid or valid due to the import.
	pub fn is_freshly_concluded(&self) -> bool {
		self.is_freshly_concluded_against() || self.is_freshly_concluded_for()
	}

	/// Whether or not the invalid vote count for the dispute went beyond the byzantine threshold
	/// after the last import
	pub fn has_fresh_byzantine_threshold_against(&self) -> bool {
		!self.old_state().byzantine_threshold_against &&
			self.new_state().byzantine_threshold_against
	}

	/// Modify this `ImportResult`s, by importing additional approval votes.
	///
	/// Both results and `new_state` will be changed as if those approval votes had been in the
	/// original import.
	pub fn import_approval_votes(
		self,
		env: &CandidateEnvironment,
		approval_votes: HashMap<ValidatorIndex, (Vec<CandidateHash>, ValidatorSignature)>,
		now: Timestamp,
	) -> Self {
		let Self {
			old_state,
			new_state,
			new_invalid_voters,
			mut imported_valid_votes,
			imported_invalid_votes,
			mut imported_approval_votes,
		} = self;

		let (mut votes, _) = new_state.into_old_state();

		for (index, (candidate_hashes, sig)) in approval_votes.into_iter() {
			debug_assert!(
				{
					let pub_key = &env.session_info().validators.get(index).expect("indices are validated by approval-voting subsystem; qed");
					let session_index = env.session_index();
					candidate_hashes.contains(&votes.candidate_receipt.hash()) && DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(candidate_hashes.clone()))
						.check_signature(pub_key, *candidate_hashes.first().expect("Valid votes have at least one candidate; qed"), session_index, &sig)
						.is_ok()
				},
				"Signature check for imported approval votes failed! This is a serious bug. Session: {:?}, candidate hash: {:?}, validator index: {:?}", env.session_index(), votes.candidate_receipt.hash(), index
			);
			if votes.valid.insert_vote(
				index,
				// There is a hidden dependency here between approval-voting and this subsystem.
				// We should be able to start emitting
				// ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates only after:
				// 1. Runtime have been upgraded to know about the new format.
				// 2. All nodes have been upgraded to know about the new format.
				// Once those two requirements have been met we should be able to increase
				// max_approval_coalesce_count to values greater than 1.
				if candidate_hashes.len() > 1 {
					ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(candidate_hashes)
				} else {
					ValidDisputeStatementKind::ApprovalChecking
				},
				sig,
			) {
				imported_valid_votes += 1;
				imported_approval_votes += 1;
			}
		}

		let new_state = CandidateVoteState::new(votes, env, now);

		Self {
			old_state,
			new_state,
			new_invalid_voters,
			imported_valid_votes,
			imported_invalid_votes,
			imported_approval_votes,
		}
	}

	/// All done, give me those votes.
	///
	/// Returns: `None` in case nothing has changed (import was redundant).
	pub fn into_updated_votes(self) -> Option<CandidateVotes> {
		if self.votes_changed() {
			let CandidateVoteState { votes, .. } = self.new_state;
			Some(votes)
		} else {
			None
		}
	}
}
