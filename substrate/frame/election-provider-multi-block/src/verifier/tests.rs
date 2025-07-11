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

use crate::{
	mock::{
		fake_solution, mine_solution, roll_to_snapshot_created, solution_from_supports,
		verifier_events, ExtBuilder, MaxBackersPerWinner, MaxWinnersPerPage, MultiBlock, Runtime,
		VerifierPallet, *,
	},
	verifier::{impls::Status, Event, FeasibilityError, Verifier, *},
	PagedRawSolution, Snapshot, *,
};
use frame_election_provider_support::Support;
use frame_support::{assert_noop, assert_ok};
use sp_core::bounded_vec;
use sp_npos_elections::ElectionScore;
use sp_runtime::{traits::Bounded, PerU16, Perbill};

mod feasibility_check {
	use super::*;

	#[test]
	fn missing_snapshot() {
		ExtBuilder::verifier().build_unchecked().execute_with(|| {
			// create snapshot just so that we can create a solution..
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			// ..remove the only page of the target snapshot.
			crate::Snapshot::<Runtime>::remove_target_page();

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(paged.solution_pages[0].clone(), 0),
				FeasibilityError::SnapshotUnavailable
			);
		});

		ExtBuilder::verifier().pages(2).build_unchecked().execute_with(|| {
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			// ..remove just one of the pages of voter snapshot that is relevant.
			crate::Snapshot::<Runtime>::remove_voter_page(0);

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(paged.solution_pages[0].clone(), 0),
				FeasibilityError::SnapshotUnavailable
			);
		});

		ExtBuilder::verifier().pages(2).build_unchecked().execute_with(|| {
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			// ..removing this page is not important, because we check page 0.
			crate::Snapshot::<Runtime>::remove_voter_page(1);

			assert_ok!(VerifierPallet::feasibility_check_page_inner(
				paged.solution_pages[0].clone(),
				0
			));
		});

		ExtBuilder::verifier().pages(2).build_unchecked().execute_with(|| {
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			// `DesiredTargets` missing is also an error
			crate::Snapshot::<Runtime>::kill_desired_targets();

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(paged.solution_pages[0].clone(), 0),
				FeasibilityError::SnapshotUnavailable
			);
		});

		ExtBuilder::verifier().pages(2).build_unchecked().execute_with(|| {
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			// `DesiredTargets` is not checked here.
			crate::Snapshot::<Runtime>::remove_target_page();

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(paged.solution_pages[1].clone(), 0),
				FeasibilityError::SnapshotUnavailable
			);
		});
	}

	#[test]
	fn winner_indices_single_page_must_be_in_bounds() {
		ExtBuilder::verifier().pages(1).desired_targets(2).build_and_execute(|| {
			roll_to_snapshot_created();
			let mut paged = mine_full_solution().unwrap();
			assert_eq!(crate::Snapshot::<Runtime>::targets().unwrap().len(), 4);
			// ----------------------------------------------------^^ valid range is [0..3].

			// Swap all votes from 3 to 4. here are only 4 targets, so index 4 is invalid.
			paged.solution_pages[0]
				.votes1
				.iter_mut()
				.filter(|(_, t)| *t == TargetIndex::from(3u16))
				.for_each(|(_, t)| *t += 1);

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(paged.solution_pages[0].clone(), 0),
				FeasibilityError::NposElection(sp_npos_elections::Error::SolutionInvalidIndex)
			);
		})
	}

	#[test]
	fn voter_indices_per_page_must_be_in_bounds() {
		ExtBuilder::verifier()
			.pages(1)
			.voter_per_page(Bounded::max_value())
			.desired_targets(2)
			.build_and_execute(|| {
				roll_to_snapshot_created();
				let mut paged = mine_full_solution().unwrap();

				assert_eq!(crate::Snapshot::<Runtime>::voters(0).unwrap().len(), 12);
				// ------------------------------------------------^^ valid range is [0..11] in page
				// 0.

				// Check that there is an index 11 in votes1, and flip to 12. There are only 12
				// voters, so index 12 is invalid.
				assert!(
					paged.solution_pages[0]
						.votes1
						.iter_mut()
						.filter(|(v, _)| *v == VoterIndex::from(11u32))
						.map(|(v, _)| *v = 12)
						.count() > 0
				);
				assert_noop!(
					VerifierPallet::feasibility_check_page_inner(
						paged.solution_pages[0].clone(),
						0
					),
					FeasibilityError::NposElection(sp_npos_elections::Error::SolutionInvalidIndex),
				);
			})
	}

	#[test]
	fn voter_must_have_same_targets_as_snapshot() {
		ExtBuilder::verifier()
			.pages(1)
			.voter_per_page(Bounded::max_value())
			.desired_targets(2)
			.build_and_execute(|| {
				roll_to_snapshot_created();
				let mut paged = mine_full_solution().unwrap();

				// First, check that voter at index 11 (40) actually voted for 3 (40) -- this is
				// self vote. Then, change the vote to 2 (30).
				assert_eq!(
					paged.solution_pages[0]
						.votes1
						.iter_mut()
						.filter(|(v, t)| *v == 11 && *t == 3)
						.map(|(_, t)| *t = 2)
						.count(),
					1,
				);
				assert_noop!(
					VerifierPallet::feasibility_check_page_inner(
						paged.solution_pages[0].clone(),
						0
					),
					FeasibilityError::InvalidVote,
				);
			})
	}

	#[test]
	fn prevents_duplicate_voter_index() {
		ExtBuilder::verifier().pages(1).build_and_execute(|| {
			roll_to_snapshot_created();

			// let's build a manual, bogus solution with duplicate voters, on top of page 0 of
			// snapshot (see `mock/staking.rs`).
			let faulty_page = TestNposSolution {
				// voter index 0 is giving 100% of stake to target index 0
				votes1: vec![(0, 0)],
				// and again 50% to target index 0 and target index 1. Both votes are "valid",
				// as in they are in the snapshot.
				votes2: vec![(0, [(0, PerU16::from_percent(50))], 1)],
				..Default::default()
			};

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(faulty_page, 0),
				FeasibilityError::NposElection(
					frame_election_provider_support::Error::DuplicateVoter
				),
			);
		});
	}

	#[test]
	fn prevents_duplicate_target_index() {
		ExtBuilder::verifier().pages(1).build_and_execute(|| {
			roll_to_snapshot_created();

			// A bad solution with duplicate targets for a single voter in votes2.
			let faulty_page = TestNposSolution {
				// 50% to 0, and then the rest to 0 again, not valid.
				votes2: vec![(0, [(0, PerU16::from_percent(50))], 0)],
				..Default::default()
			};

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(faulty_page, 0),
				FeasibilityError::NposElection(
					frame_election_provider_support::Error::DuplicateTarget
				),
			);
		});
	}

	#[test]
	fn heuristic_max_backers_per_winner_per_page() {
		ExtBuilder::verifier().max_backers_per_winner(2).build_and_execute(|| {
			roll_to_snapshot_created();

			// these votes are all valid, but some dude has 3 supports in a single page.
			let solution = solution_from_supports(
				vec![(40, Support { total: 30, voters: vec![(2, 10), (3, 10), (4, 10)] })],
				// all these voters are in page of the snapshot, the msp!
				2,
			);

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(solution, 2),
				FeasibilityError::FailedToBoundSupport,
			);
		})
	}

	#[test]
	fn heuristic_desired_target_check_per_page() {
		ExtBuilder::verifier().desired_targets(2).build_and_execute(|| {
			roll_to(25);
			assert_full_snapshot();

			// all of these votes are valid, but this solution is already presenting 3 winners,
			// while we just one 2.
			let solution = solution_from_supports(
				vec![
					(10, Support { total: 30, voters: vec![(4, 2)] }),
					(20, Support { total: 30, voters: vec![(4, 2)] }),
					(40, Support { total: 30, voters: vec![(4, 6)] }),
				],
				// all these voters are in page 2 of the snapshot, the msp!
				2,
			);

			assert_noop!(
				VerifierPallet::feasibility_check_page_inner(solution, 2),
				FeasibilityError::WrongWinnerCount,
			);
		})
	}
}

mod async_verification {
	use super::*;
	use sp_core::bounded_vec;
	// disambiguate event
	use crate::verifier::Event;

	#[test]
	fn basic_single_verification_works() {
		ExtBuilder::verifier().pages(1).build_and_execute(|| {
			// load a solution after the snapshot has been created.
			roll_to_snapshot_created();

			let solution = mine_full_solution().unwrap();
			load_mock_signed_and_start(solution.clone());

			// now let it verify
			roll_next();

			// It done after just one block.
			assert_eq!(VerifierPallet::status(), Status::Nothing);
			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(0, 2),
					Event::<Runtime>::Queued(solution.score, None)
				]
			);
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Queued]);
		});
	}

	#[test]
	fn basic_multi_verification_works() {
		ExtBuilder::verifier().pages(3).build_and_execute(|| {
			// load a solution after the snapshot has been created.
			roll_to_snapshot_created();

			let solution = mine_full_solution().unwrap();
			// ------------- ^^^^^^^^^^^^

			load_mock_signed_and_start(solution.clone());
			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);

			// now let it verify
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 2)]);
			// 1 page verified, stored as invalid.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 1);

			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(0));
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::Verified(2, 2), Event::<Runtime>::Verified(1, 2)]
			);
			// 2 pages verified, stored as invalid.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 2);

			// nothing is queued yet.
			assert_eq!(MockSignedResults::get(), vec![]);
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);
			assert!(QueuedSolution::<Runtime>::queued_score().is_none());

			// last block.
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Nothing);
			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(0, 2),
					Event::<Runtime>::Queued(solution.score, None),
				]
			);
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Queued]);

			// a solution has been queued
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 3);
			assert!(QueuedSolution::<Runtime>::queued_score().is_some());
		});
	}

	#[test]
	fn basic_multi_verification_partial() {
		ExtBuilder::verifier().pages(3).build_and_execute(|| {
			// load a solution after the snapshot has been created.
			roll_to_snapshot_created();

			let solution = mine_solution(2).unwrap();
			// -------------------------^^^

			load_mock_signed_and_start(solution.clone());

			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);

			// now let it verify
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 2)]);
			// 1 page verified, stored as invalid.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 1);

			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(0));
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::Verified(2, 2), Event::<Runtime>::Verified(1, 2),]
			);
			// 2 page verified, stored as invalid.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 2);

			// nothing is queued yet.
			assert_eq!(MockSignedResults::get(), vec![]);
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);
			assert!(QueuedSolution::<Runtime>::queued_score().is_none());

			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Nothing);

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Verified(1, 2),
					// this is a partial solution, no one in this page (lsp).
					Event::<Runtime>::Verified(0, 0),
					Event::<Runtime>::Queued(solution.score, None),
				]
			);

			// a solution has been queued
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Queued]);
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 3);
			assert!(QueuedSolution::<Runtime>::queued_score().is_some());

			// page 0 is empty..
			assert_eq!(QueuedSolution::<Runtime>::get_valid_page(0).unwrap().len(), 0);
			// .. the other two are not.
			assert_eq!(QueuedSolution::<Runtime>::get_valid_page(1).unwrap().len(), 2);
			assert_eq!(QueuedSolution::<Runtime>::get_valid_page(2).unwrap().len(), 2);
		});
	}

	#[test]
	fn solution_data_provider_empty_data_solution() {
		ExtBuilder::verifier().build_and_execute(|| {
			// not super important, but anyways..
			roll_to_snapshot_created();

			// The solution data provider is empty.
			assert_eq!(SignedPhaseSwitch::get(), SignedSwitch::Mock);
			assert_eq!(MockSignedNextSolution::get(), None);

			// nothing happens..
			assert_eq!(VerifierPallet::status(), Status::Nothing);
			assert_ok!(<VerifierPallet as AsynchronousVerifier>::start());
			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));

			roll_next();

			// After first roll, only page 2 is processed (as empty page), status is still
			// Ongoing(1).
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 0)]);
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));

			// Process the next page (page 1).
			roll_next();
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::Verified(2, 0), Event::<Runtime>::Verified(1, 0)]
			);
			assert_eq!(VerifierPallet::status(), Status::Ongoing(0));

			// Process the final page (page 0).
			roll_next();
			// Missing score data returns default score which fails quality checks and gets
			// rejected.
			assert_eq!(VerifierPallet::status(), Status::Nothing);
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		});
	}

	#[test]
	fn solution_data_provider_empty_data_midway() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let solution = mine_full_solution().unwrap();
			load_mock_signed_and_start(solution.clone());

			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));

			// now let it verify. first one goes fine.
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 2)]);
			assert_eq!(MockSignedResults::get(), vec![]);

			// 1 page verified, stored as invalid.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 1);
			assert_eq!(QueuedSolution::<Runtime>::backing_iter().count(), 1);
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);

			// suddenly clear this guy. Crucially, do not clear the score. That will be tested in
			// the scope of `solution_data_provider_missing_score_at_end`.
			MockSignedNextSolution::set(None);

			// Roll through the remaining pages, which will be treated as empty.
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(0));
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::Verified(2, 2), Event::<Runtime>::Verified(1, 0)]
			);

			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Nothing);
			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Verified(1, 0),
					Event::<Runtime>::Verified(0, 0),
					Event::<Runtime>::VerificationFailed(0, FeasibilityError::InvalidScore),
				]
			);

			// The system should be in a clean state after processing all pages.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 0);
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);
			assert_eq!(QueuedSolution::<Runtime>::backing_iter().count(), 0);

			// Empty pages are handled gracefully, solution is rejected.
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		})
	}

	#[test]
	fn solution_data_provider_missing_score_at_end() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let solution = mine_full_solution().unwrap();
			load_mock_signed_and_start(solution.clone());

			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));

			// First page is fine.
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 2)]);
			assert_eq!(MockSignedResults::get(), vec![]);

			// Now clear both the solution and the score to simulate missing score at the end.
			MockSignedNextSolution::set(None);
			MockSignedNextScore::set(Default::default());

			// Roll through remaining pages.
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(0));
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::Verified(2, 2), Event::<Runtime>::Verified(1, 0)]
			);
			roll_next();

			// Missing score data returns default score which fails quality checks and gets
			// rejected.
			assert_eq!(VerifierPallet::status(), Status::Nothing);
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		});
	}

	#[test]
	fn rejects_new_verification_via_start_if_ongoing() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let solution = mine_full_solution().unwrap();
			load_mock_signed_and_start(solution.clone());

			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));

			// nada
			assert_noop!(<VerifierPallet as AsynchronousVerifier>::start(), "verification ongoing");

			// now let it verify. first one goes fine.
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 2)]);
			assert_eq!(MockSignedResults::get(), vec![]);

			// retry, still nada.
			assert_noop!(<VerifierPallet as AsynchronousVerifier>::start(), "verification ongoing");
		})
	}

	#[test]
	fn verification_failure_clears_everything() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let mut solution = mine_full_solution().unwrap();
			// Make the solution invalid by corrupting the first page
			solution.solution_pages[0].votes1[0] = (0, 1000); // Invalid vote weight
			load_mock_signed_and_start(solution.clone());

			assert_eq!(VerifierPallet::status(), Status::Ongoing(2));

			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(1));
			assert_eq!(verifier_events(), vec![Event::<Runtime>::Verified(2, 2)]);

			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Ongoing(0));
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::Verified(2, 2), Event::<Runtime>::Verified(1, 2)]
			);

			// Verification fails on the last page due to invalid solution
			roll_next();
			assert_eq!(VerifierPallet::status(), Status::Nothing);

			// everything is cleared when verification fails.
			assert_eq!(QueuedSolution::<Runtime>::invalid_iter().count(), 0);
			assert_eq!(QueuedSolution::<Runtime>::valid_iter().count(), 0);
			assert_eq!(QueuedSolution::<Runtime>::backing_iter().count(), 0);

			// and we report that something was rejected.
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		})
	}

	#[test]
	fn weak_valid_solution_is_insta_rejected() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let paged = mine_full_solution().unwrap();
			load_mock_signed_and_start(paged.clone());
			let _ = roll_to_full_verification();

			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::Verified(1, 2),
					Event::Verified(0, 2),
					Event::Queued(paged.score, None)
				]
			);
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Queued]);

			// good boi, but you are too weak. This solution also does not have the full pages,
			// which is also fine. See `basic_multi_verification_partial`.
			let weak_page_partial =
				solution_from_supports(vec![(10, Support { total: 10, voters: vec![(1, 10)] })], 2);
			let weak_paged = PagedRawSolution::<Runtime> {
				solution_pages: bounded_vec![weak_page_partial],
				score: ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 },
				..Default::default()
			};

			load_mock_signed_and_start(weak_paged.clone());
			// this is insta-rejected, no need to proceed any more blocks.

			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::Verified(1, 2),
					Event::Verified(0, 2),
					Event::Queued(paged.score, None),
					Event::VerificationFailed(2, FeasibilityError::ScoreTooLow)
				]
			);

			assert_eq!(
				MockSignedResults::get(),
				vec![VerificationResult::Queued, VerificationResult::Rejected]
			);
		})
	}

	#[test]
	fn better_valid_solution_replaces() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			// a weak one, which we will still accept.
			let weak_page_partial = solution_from_supports(
				vec![
					(10, Support { total: 10, voters: vec![(1, 10)] }),
					(20, Support { total: 10, voters: vec![(4, 10)] }),
				],
				2,
			);
			let weak_paged = PagedRawSolution::<Runtime> {
				solution_pages: bounded_vec![weak_page_partial],
				score: ElectionScore { minimal_stake: 10, sum_stake: 20, sum_stake_squared: 200 },
				..Default::default()
			};

			load_mock_signed_and_start(weak_paged.clone());
			let _ = roll_to_full_verification();

			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::Verified(1, 0), // note: partial solution!
					Event::Verified(0, 0), // note: partial solution!
					Event::Queued(weak_paged.score, None)
				]
			);
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Queued]);

			let paged = mine_full_solution().unwrap();
			load_mock_signed_and_start(paged.clone());
			let _ = roll_to_full_verification();

			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::Verified(1, 0),
					Event::Verified(0, 0),
					Event::Queued(weak_paged.score, None),
					Event::Verified(2, 2),
					Event::Verified(1, 2),
					Event::Verified(0, 2),
					Event::Queued(paged.score, Some(weak_paged.score))
				]
			);
			assert_eq!(
				MockSignedResults::get(),
				vec![VerificationResult::Queued, VerificationResult::Queued]
			);
		})
	}

	#[test]
	fn invalid_solution_bad_score() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let mut paged = mine_full_solution().unwrap();

			// just tweak score.
			paged.score.minimal_stake += 1;
			assert!(<VerifierPallet as Verifier>::queued_score().is_none());

			load_mock_signed_and_start(paged);
			roll_to_full_verification();

			// nothing is verified.
			assert!(<VerifierPallet as Verifier>::queued_score().is_none());
			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(0, 2),
					Event::<Runtime>::VerificationFailed(0, FeasibilityError::InvalidScore)
				]
			);

			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		})
	}

	#[test]
	fn invalid_solution_bad_minimum_score() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			// our minimum score is our score, just a bit better.
			let mut better_score = paged.score;
			better_score.minimal_stake += 1;
			<VerifierPallet as Verifier>::set_minimum_score(better_score);

			load_mock_signed_and_start(paged);

			// note that we don't need to call to `roll_to_full_verification`, since this solution
			// is pretty much insta-rejected;
			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::VerificationFailed(2, FeasibilityError::ScoreTooLow)]
			);

			// nothing is verified..
			assert!(<VerifierPallet as Verifier>::queued_score().is_none());

			// result is reported back.
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		})
	}

	#[test]
	fn invalid_solution_bad_desired_targets() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			assert_eq!(crate::Snapshot::<Runtime>::desired_targets().unwrap(), 2);
			let paged = mine_full_solution().unwrap();

			// tweak this, for whatever reason.
			crate::Snapshot::<Runtime>::set_desired_targets(3);

			load_mock_signed_and_start(paged);
			roll_to_full_verification();

			// we detect this only in the last page.
			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::Verified(1, 2),
					Event::Verified(0, 2),
					Event::VerificationFailed(0, FeasibilityError::WrongWinnerCount)
				]
			);

			// nothing is verified..
			assert!(<VerifierPallet as Verifier>::queued_score().is_none());
			// result is reported back.
			assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
		})
	}

	#[test]
	fn invalid_solution_bad_bounds_per_page() {
		ExtBuilder::verifier()
			.desired_targets(1)
			.max_backers_per_winner(1) // in each page we allow 1 baker to be presented.
			.build_and_execute(|| {
				roll_to_snapshot_created();

				// This is a sneaky custom solution where it will fail in the second page.
				let page0 = solution_from_supports(
					vec![(10, Support { total: 10, voters: vec![(1, 10)] })],
					2,
				);
				let page1 = solution_from_supports(
					vec![(10, Support { total: 20, voters: vec![(5, 10), (8, 10)] })],
					1,
				);
				let page2 = solution_from_supports(
					vec![(10, Support { total: 10, voters: vec![(10, 10)] })],
					0,
				);
				let paged = PagedRawSolution {
					solution_pages: bounded_vec![page0, page1, page2],
					score: Default::default(), // score is never checked, so nada
					..Default::default()
				};

				load_mock_signed_and_start(paged);
				roll_to_full_verification();

				// we detect the bound issue in page 2.
				assert_eq!(
					verifier_events(),
					vec![
						Event::Verified(2, 1),
						Event::VerificationFailed(1, FeasibilityError::FailedToBoundSupport)
					]
				);

				// our state is fully cleaned.
				QueuedSolution::<Runtime>::assert_killed();
				assert_eq!(StatusStorage::<Runtime>::get(), Status::Nothing);
				// nothing is verified..
				assert!(<VerifierPallet as Verifier>::queued_score().is_none());
				// result is reported back.
				assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
			})
	}

	#[test]
	fn invalid_solution_bad_bounds_final() {
		ExtBuilder::verifier()
			.desired_targets(1)
			.max_backers_per_winner(2)
			.max_backers_per_winner_final(2)
			.build_and_execute(|| {
				roll_to_snapshot_created();

				// This is a sneaky custom solution where in each page 10 has 1 backers, so only in
				// the last page we can catch the mfer.
				let page0 = solution_from_supports(
					vec![(10, Support { total: 10, voters: vec![(1, 10)] })],
					2,
				);
				let page1 = solution_from_supports(
					vec![(10, Support { total: 10, voters: vec![(5, 10)] })],
					1,
				);
				let page2 = solution_from_supports(
					vec![(10, Support { total: 10, voters: vec![(10, 10)] })],
					0,
				);
				let paged = PagedRawSolution {
					solution_pages: bounded_vec![page0, page1, page2],
					score: ElectionScore {
						minimal_stake: 30,
						sum_stake: 30,
						sum_stake_squared: 900,
					},
					..Default::default()
				};

				load_mock_signed_and_start(paged);
				roll_to_full_verification();

				// we detect this only in the last page.
				assert_eq!(
					verifier_events(),
					vec![
						Event::Verified(2, 1),
						Event::Verified(1, 1),
						Event::Verified(0, 1),
						Event::VerificationFailed(0, FeasibilityError::FailedToBoundSupport)
					]
				);

				// our state is fully cleaned.
				QueuedSolution::<Runtime>::assert_killed();
				assert_eq!(StatusStorage::<Runtime>::get(), Status::Nothing);

				// nothing is verified..
				assert!(<VerifierPallet as Verifier>::queued_score().is_none());
				// result is reported back.
				assert_eq!(MockSignedResults::get(), vec![VerificationResult::Rejected]);
			})
	}

	#[test]
	fn invalid_solution_does_not_alter_queue() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let mut paged = mine_full_solution().unwrap();
			let correct_score = paged.score;

			assert!(<VerifierPallet as Verifier>::queued_score().is_none());

			load_mock_signed_and_start(paged.clone());
			roll_to_full_verification();

			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(correct_score));
			assert!(QueuedSolution::<Runtime>::invalid_iter().count().is_zero());
			assert!(QueuedSolution::<Runtime>::backing_iter().count().is_zero());

			// just tweak score. Note that we tweak for a higher score, so the verifier will accept
			// it.
			paged.score.minimal_stake += 1;
			load_mock_signed_and_start(paged.clone());
			roll_to_full_verification();

			// nothing is verified.
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(correct_score));
			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(0, 2),
					Event::<Runtime>::Queued(correct_score, None),
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(0, 2),
					Event::<Runtime>::VerificationFailed(0, FeasibilityError::InvalidScore),
				]
			);

			// the verification results.
			assert_eq!(
				MockSignedResults::get(),
				vec![VerificationResult::Queued, VerificationResult::Rejected]
			);

			// and the queue is still in good shape.
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(correct_score));
			assert!(QueuedSolution::<Runtime>::invalid_iter().count().is_zero());
			assert!(QueuedSolution::<Runtime>::backing_iter().count().is_zero());
		})
	}
}

mod multi_page_sync_verification {
	use super::*;
	use frame_support::hypothetically;

	#[test]
	fn basic_sync_verification_works() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let paged = mine_solution(2).unwrap();

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			let _ = <VerifierPallet as Verifier>::verify_synchronous_multi(
				paged.solution_pages.clone(),
				MultiBlock::msp_range_for(2),
				paged.score,
			)
			.unwrap();

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(paged.score, None)
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(paged.score));
		})
	}

	#[test]
	fn basic_sync_verification_works_full() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let paged = mine_full_solution().unwrap();

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			let _ = <VerifierPallet as Verifier>::verify_synchronous_multi(
				paged.solution_pages.clone(),
				MultiBlock::msp_range_for(3),
				paged.score,
			)
			.unwrap();

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(0, 2),
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(paged.score, None)
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(paged.score));
		})
	}

	#[test]
	fn incorrect_score_checked_at_end() {
		ExtBuilder::verifier().build_and_execute(|| {
			// A solution that where each individual page is valid, but the final score is bad.
			roll_to_snapshot_created();
			let mut paged = mine_solution(2).unwrap();
			paged.score.minimal_stake += 1;

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous_multi(
					paged.solution_pages.clone(),
					MultiBlock::msp_range_for(2),
					paged.score,
				)
				.unwrap_err(),
				FeasibilityError::InvalidScore
			);

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::VerificationFailed(2, FeasibilityError::InvalidScore),
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);
		})
	}

	#[test]
	fn invalid_second_page() {
		ExtBuilder::verifier().build_and_execute(|| {
			// A solution that where the second validated page is invalid.
			use frame_election_provider_support::traits::NposSolution;
			roll_to_snapshot_created();
			let mut paged = mine_solution(2).unwrap();
			paged.solution_pages.last_mut().map(|p| p.corrupt());

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous_multi(
					paged.solution_pages.clone(),
					MultiBlock::msp_range_for(2),
					paged.score,
				)
				.unwrap_err(),
				FeasibilityError::NposElection(sp_npos_elections::Error::SolutionInvalidIndex)
			);

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::VerificationFailed(
						2,
						FeasibilityError::NposElection(
							sp_npos_elections::Error::SolutionInvalidIndex
						)
					),
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);
		})
	}

	#[test]
	fn too_may_max_backers_per_winner_second_page() {
		ExtBuilder::verifier().build_and_execute(|| {
			// A solution that where the at the second page with hit the final max backers per
			// winner final bound.
			roll_to_snapshot_created();
			let paged = mine_solution(2).unwrap();

			hypothetically!({
				assert_ok!(<VerifierPallet as Verifier>::verify_synchronous_multi(
					paged.solution_pages.clone(),
					MultiBlock::msp_range_for(2),
					paged.score,
				));
				let p1 = QueuedSolution::<Runtime>::get_queued_solution_page(1).unwrap();
				let p2 = QueuedSolution::<Runtime>::get_queued_solution_page(2).unwrap();

				// 40 has 2 backers in the first page, and 3 in the second
				assert_eq!(
					p1.into_iter()
						.find_map(|(who, support)| {
							if who == 40 {
								Some(support.voters.len())
							} else {
								None
							}
						})
						.unwrap(),
					2
				);

				assert_eq!(
					p2.into_iter()
						.find_map(|(who, support)| {
							if who == 40 {
								Some(support.voters.len())
							} else {
								None
							}
						})
						.unwrap(),
					3
				);
			});

			// From the above, we know setting this will do the trick
			MaxBackersPerWinnerFinal::set(4);

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous_multi(
					paged.solution_pages.clone(),
					MultiBlock::msp_range_for(2),
					paged.score,
				)
				.unwrap_err(),
				FeasibilityError::FailedToBoundSupport
			);

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(1, 2),
					Event::<Runtime>::VerificationFailed(2, FeasibilityError::FailedToBoundSupport),
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);
		})
	}
}

mod single_page_sync_verification {
	use super::*;

	#[test]
	fn basic_sync_verification_works() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let single_page = mine_solution(1).unwrap();

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			let _ = <VerifierPallet as Verifier>::verify_synchronous(
				single_page.solution_pages.first().cloned().unwrap(),
				single_page.score,
				MultiBlock::msp(),
			)
			.unwrap();

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(single_page.score, None)
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(single_page.score));
		})
	}

	#[test]
	fn winner_count_more() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let single_page = mine_solution(1).unwrap();

			// change the snapshot, as if the desired targets is now 1. This solution is then valid,
			// but has too many.
			Snapshot::<Runtime>::set_desired_targets(1);

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			// note: this is NOT a storage_noop! because we do emit events.
			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::WrongWinnerCount
			);

			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::VerificationFailed(2, FeasibilityError::WrongWinnerCount)]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);
		})
	}

	#[test]
	fn winner_count_less() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();
			let single_page = mine_solution(1).unwrap();

			assert_eq!(verifier_events(), vec![]);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			// Valid solution, but has now too few.
			Snapshot::<Runtime>::set_desired_targets(3);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::WrongWinnerCount
			);

			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::<Runtime>::VerificationFailed(2, FeasibilityError::WrongWinnerCount)
				]
			);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);
		})
	}

	#[test]
	fn incorrect_score_is_rejected() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let single_page = mine_solution(1).unwrap();
			let mut score_incorrect = single_page.score;
			score_incorrect.minimal_stake += 1;

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					score_incorrect,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::InvalidScore
			);

			assert_eq!(
				verifier_events(),
				vec![
					Event::Verified(2, 2),
					Event::<Runtime>::VerificationFailed(2, FeasibilityError::InvalidScore),
				]
			);
		})
	}

	#[test]
	fn minimum_untrusted_score_is_rejected() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let single_page = mine_solution(1).unwrap();

			// raise the bar such that we don't meet it.
			let mut unattainable_score = single_page.score;
			unattainable_score.minimal_stake += 1;

			<VerifierPallet as Verifier>::set_minimum_score(unattainable_score);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::ScoreTooLow
			);

			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::VerificationFailed(2, FeasibilityError::ScoreTooLow)]
			);
		})
	}

	#[test]
	fn bad_bounds_rejected_max_backers_per_winner() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let single_page = mine_solution(1).unwrap();
			// note: change this after the miner is done, otherwise it is smart enough to trim.
			MaxBackersPerWinner::set(1);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::FailedToBoundSupport
			);

			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::VerificationFailed(
					2,
					FeasibilityError::FailedToBoundSupport
				)]
			);
		});
	}

	#[test]
	fn bad_bounds_rejected_max_winners_per_page() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let single_page = mine_solution(1).unwrap();
			// note: the miner does feasibility internally, change this parameter afterwards.
			MaxWinnersPerPage::set(1);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::FailedToBoundSupport
			);

			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::VerificationFailed(
					2,
					FeasibilityError::FailedToBoundSupport
				)]
			);
		});
	}

	#[test]
	fn bad_bounds_rejected_max_backers_per_winner_final() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let single_page = mine_solution(1).unwrap();
			// note: the miner does feasibility internally, change this parameter afterwards.
			MaxBackersPerWinnerFinal::set(1);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::FailedToBoundSupport
			);

			assert_eq!(
				verifier_events(),
				vec![Event::<Runtime>::VerificationFailed(
					2,
					FeasibilityError::FailedToBoundSupport
				)]
			);
		});
	}

	#[test]
	fn solution_improvement_threshold_respected() {
		ExtBuilder::verifier()
			.solution_improvement_threshold(Perbill::from_percent(10))
			.build_and_execute(|| {
				roll_to_snapshot_created();

				// submit something good.
				let single_page = mine_solution(1).unwrap();
				let _ = <VerifierPallet as Verifier>::verify_synchronous(
					single_page.solution_pages.first().cloned().unwrap(),
					single_page.score,
					MultiBlock::msp(),
				)
				.unwrap();

				// the slightly better solution need not even be correct. We improve it by 5%, but
				// we need 10%.
				let mut better_score = single_page.score;
				let improvement = Perbill::from_percent(5) * better_score.minimal_stake;
				better_score.minimal_stake += improvement;
				let slightly_better = fake_solution(better_score);

				assert_eq!(
					<VerifierPallet as Verifier>::verify_synchronous(
						slightly_better.solution_pages.first().cloned().unwrap(),
						slightly_better.score,
						MultiBlock::msp(),
					)
					.unwrap_err(),
					FeasibilityError::ScoreTooLow
				);
			});
	}

	#[test]
	fn weak_score_is_insta_rejected() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			// queue something useful.
			let single_page = mine_solution(1).unwrap();
			let _ = <VerifierPallet as Verifier>::verify_synchronous(
				single_page.solution_pages.first().cloned().unwrap(),
				single_page.score,
				MultiBlock::msp(),
			)
			.unwrap();
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(single_page.score));

			// now try and submit that's really weak. Doesn't even need to be valid, since the score
			// is checked first.
			let mut bad_score = single_page.score;
			bad_score.minimal_stake -= 1;
			let weak = fake_solution(bad_score);

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					weak.solution_pages.first().cloned().unwrap(),
					weak.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::ScoreTooLow
			);

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(single_page.score, None),
					Event::<Runtime>::VerificationFailed(2, FeasibilityError::ScoreTooLow),
				]
			);
		})
	}

	#[test]
	fn good_solution_replaces() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			let weak_solution = solution_from_supports(
				vec![
					(10, Support { total: 10, voters: vec![(1, 10)] }),
					(20, Support { total: 10, voters: vec![(4, 10)] }),
				],
				2,
			);

			let weak_paged = PagedRawSolution::<Runtime> {
				solution_pages: bounded_vec![weak_solution],
				score: ElectionScore { minimal_stake: 10, sum_stake: 20, sum_stake_squared: 200 },
				..Default::default()
			};

			let _ = <VerifierPallet as Verifier>::verify_synchronous(
				weak_paged.solution_pages.first().cloned().unwrap(),
				weak_paged.score,
				MultiBlock::msp(),
			)
			.unwrap();
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(weak_paged.score));

			// now get a better solution.
			let better = mine_solution(1).unwrap();

			let _ = <VerifierPallet as Verifier>::verify_synchronous(
				better.solution_pages.first().cloned().unwrap(),
				better.score,
				MultiBlock::msp(),
			)
			.unwrap();

			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(better.score));

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(weak_paged.score, None),
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(better.score, Some(weak_paged.score)),
				]
			);
		})
	}

	#[test]
	fn weak_valid_is_discarded() {
		ExtBuilder::verifier().build_and_execute(|| {
			roll_to_snapshot_created();

			// first, submit something good
			let better = mine_solution(1).unwrap();
			let _ = <VerifierPallet as Verifier>::verify_synchronous(
				better.solution_pages.first().cloned().unwrap(),
				better.score,
				MultiBlock::msp(),
			)
			.unwrap();
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(better.score));

			// then try with something weaker.
			let weak_solution = solution_from_supports(
				vec![
					(10, Support { total: 10, voters: vec![(1, 10)] }),
					(20, Support { total: 10, voters: vec![(4, 10)] }),
				],
				2,
			);
			let weak_paged = PagedRawSolution::<Runtime> {
				solution_pages: bounded_vec![weak_solution],
				score: ElectionScore { minimal_stake: 10, sum_stake: 20, sum_stake_squared: 200 },
				..Default::default()
			};

			assert_eq!(
				<VerifierPallet as Verifier>::verify_synchronous(
					weak_paged.solution_pages.first().cloned().unwrap(),
					weak_paged.score,
					MultiBlock::msp(),
				)
				.unwrap_err(),
				FeasibilityError::ScoreTooLow
			);

			// queued solution has not changed.
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), Some(better.score));

			assert_eq!(
				verifier_events(),
				vec![
					Event::<Runtime>::Verified(2, 2),
					Event::<Runtime>::Queued(better.score, None),
					Event::<Runtime>::VerificationFailed(2, FeasibilityError::ScoreTooLow),
				]
			);
		})
	}
}
