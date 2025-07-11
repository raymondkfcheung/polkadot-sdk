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

//! Validation host - is the primary interface for this crate. It allows the clients to enqueue
//! jobs for PVF execution or preparation.
//!
//! The validation host is represented by a future/task that runs an event-loop and by a handle,
//! [`ValidationHost`], that allows communication with that event-loop.

use crate::{
	artifacts::{ArtifactId, ArtifactPathId, ArtifactState, Artifacts, ArtifactsCleanupConfig},
	execute::{self, PendingExecutionRequest},
	metrics::Metrics,
	prepare, Priority, SecurityStatus, ValidationError, LOG_TARGET,
};
use always_assert::never;
use futures::{
	channel::{mpsc, oneshot},
	Future, FutureExt, SinkExt, StreamExt,
};
#[cfg(feature = "test-utils")]
use polkadot_node_core_pvf_common::ArtifactChecksum;
use polkadot_node_core_pvf_common::{
	error::{PrecheckResult, PrepareError},
	prepare::PrepareSuccess,
	pvf::PvfPrepData,
};
use polkadot_node_primitives::PoV;
use polkadot_node_subsystem::{
	messages::PvfExecKind, ActiveLeavesUpdate, SubsystemError, SubsystemResult,
};
use polkadot_parachain_primitives::primitives::ValidationResult;
use polkadot_primitives::{Hash, PersistedValidationData};
use std::{
	collections::HashMap,
	path::PathBuf,
	sync::Arc,
	time::{Duration, SystemTime},
};

/// The time period after which a failed preparation artifact is considered ready to be retried.
/// Note that we will only retry if another request comes in after this cooldown has passed.
#[cfg(not(test))]
pub const PREPARE_FAILURE_COOLDOWN: Duration = Duration::from_secs(15 * 60);
#[cfg(test)]
pub const PREPARE_FAILURE_COOLDOWN: Duration = Duration::from_millis(200);

/// The amount of times we will retry failed prepare jobs.
pub const NUM_PREPARE_RETRIES: u32 = 5;

/// The name of binary spawned to prepare a PVF artifact
pub const PREPARE_BINARY_NAME: &str = "polkadot-prepare-worker";

/// The name of binary spawned to execute a PVF
pub const EXECUTE_BINARY_NAME: &str = "polkadot-execute-worker";

/// The size of incoming message queue
pub const HOST_MESSAGE_QUEUE_SIZE: usize = 10;

/// An alias to not spell the type for the oneshot sender for the PVF execution result.
pub(crate) type ResultSender = oneshot::Sender<Result<ValidationResult, ValidationError>>;

/// Transmission end used for sending the PVF preparation result.
pub(crate) type PrecheckResultSender = oneshot::Sender<PrecheckResult>;

/// A handle to the async process serving the validation host requests.
#[derive(Clone)]
pub struct ValidationHost {
	to_host_tx: mpsc::Sender<ToHost>,
	/// Available security features, detected by the host during startup.
	pub security_status: SecurityStatus,
}

impl ValidationHost {
	/// Precheck PVF with the given code, i.e. verify that it compiles within a reasonable time
	/// limit. This will prepare the PVF. The result of preparation will be sent to the provided
	/// result sender.
	///
	/// This is async to accommodate the possibility of back-pressure. In the vast majority of
	/// situations this function should return immediately.
	///
	/// Returns an error if the request cannot be sent to the validation host, i.e. if it shut down.
	pub async fn precheck_pvf(
		&mut self,
		pvf: PvfPrepData,
		result_tx: PrecheckResultSender,
	) -> Result<(), String> {
		self.to_host_tx
			.send(ToHost::PrecheckPvf { pvf, result_tx })
			.await
			.map_err(|_| "the inner loop hung up".to_string())
	}

	/// Execute PVF with the given code, execution timeout, parameters and priority.
	/// The result of execution will be sent to the provided result sender.
	///
	/// This is async to accommodate the possibility of back-pressure. In the vast majority of
	/// situations this function should return immediately.
	///
	/// Returns an error if the request cannot be sent to the validation host, i.e. if it shut down.
	pub async fn execute_pvf(
		&mut self,
		pvf: PvfPrepData,
		exec_timeout: Duration,
		pvd: Arc<PersistedValidationData>,
		pov: Arc<PoV>,
		priority: Priority,
		exec_kind: PvfExecKind,
		result_tx: ResultSender,
	) -> Result<(), String> {
		self.to_host_tx
			.send(ToHost::ExecutePvf(ExecutePvfInputs {
				pvf,
				exec_timeout,
				pvd,
				pov,
				priority,
				exec_kind,
				result_tx,
			}))
			.await
			.map_err(|_| "the inner loop hung up".to_string())
	}

	/// Sends a signal to the validation host requesting to prepare a list of the given PVFs.
	///
	/// This is async to accommodate the possibility of back-pressure. In the vast majority of
	/// situations this function should return immediately.
	///
	/// Returns an error if the request cannot be sent to the validation host, i.e. if it shut down.
	pub async fn heads_up(&mut self, active_pvfs: Vec<PvfPrepData>) -> Result<(), String> {
		self.to_host_tx
			.send(ToHost::HeadsUp { active_pvfs })
			.await
			.map_err(|_| "the inner loop hung up".to_string())
	}

	/// Sends a signal to the validation host requesting to update best block.
	///
	/// Returns an error if the request cannot be sent to the validation host, i.e. if it shut down.
	pub async fn update_active_leaves(
		&mut self,
		update: ActiveLeavesUpdate,
		ancestors: Vec<Hash>,
	) -> Result<(), String> {
		self.to_host_tx
			.send(ToHost::UpdateActiveLeaves { update, ancestors })
			.await
			.map_err(|_| "the inner loop hung up".to_string())
	}

	/// Replace the artifact checksum with a new one.
	///
	/// Only for test purposes to imitate a corruption of the artifact on disk.
	#[cfg(feature = "test-utils")]
	pub async fn replace_artifact_checksum(
		&mut self,
		checksum: ArtifactChecksum,
		new_checksum: ArtifactChecksum,
	) -> Result<(), String> {
		self.to_host_tx
			.send(ToHost::ReplaceArtifactChecksum { checksum, new_checksum })
			.await
			.map_err(|_| "the inner loop hung up".to_string())
	}
}

enum ToHost {
	PrecheckPvf {
		pvf: PvfPrepData,
		result_tx: PrecheckResultSender,
	},
	ExecutePvf(ExecutePvfInputs),
	HeadsUp {
		active_pvfs: Vec<PvfPrepData>,
	},
	UpdateActiveLeaves {
		update: ActiveLeavesUpdate,
		ancestors: Vec<Hash>,
	},
	#[cfg(feature = "test-utils")]
	ReplaceArtifactChecksum {
		checksum: ArtifactChecksum,
		new_checksum: ArtifactChecksum,
	},
}

struct ExecutePvfInputs {
	pvf: PvfPrepData,
	exec_timeout: Duration,
	pvd: Arc<PersistedValidationData>,
	pov: Arc<PoV>,
	priority: Priority,
	exec_kind: PvfExecKind,
	result_tx: ResultSender,
}

/// Configuration for the validation host.
#[derive(Debug)]
pub struct Config {
	/// The root directory where the prepared artifacts can be stored.
	pub cache_path: PathBuf,
	/// The version of the node. `None` can be passed to skip the version check (only for tests).
	pub node_version: Option<String>,
	/// Whether the node is attempting to run as a secure validator.
	pub secure_validator_mode: bool,

	/// The path to the program that can be used to spawn the prepare workers.
	pub prepare_worker_program_path: PathBuf,
	/// The time allotted for a prepare worker to spawn and report to the host.
	pub prepare_worker_spawn_timeout: Duration,
	/// The maximum number of workers that can be spawned in the prepare pool for tasks with the
	/// priority below critical.
	pub prepare_workers_soft_max_num: usize,
	/// The absolute number of workers that can be spawned in the prepare pool.
	pub prepare_workers_hard_max_num: usize,

	/// The path to the program that can be used to spawn the execute workers.
	pub execute_worker_program_path: PathBuf,
	/// The time allotted for an execute worker to spawn and report to the host.
	pub execute_worker_spawn_timeout: Duration,
	/// The maximum number of execute workers that can run at the same time.
	pub execute_workers_max_num: usize,
}

impl Config {
	/// Create a new instance of the configuration.
	pub fn new(
		cache_path: PathBuf,
		node_version: Option<String>,
		secure_validator_mode: bool,
		prepare_worker_program_path: PathBuf,
		execute_worker_program_path: PathBuf,
		execute_workers_max_num: usize,
		prepare_workers_soft_max_num: usize,
		prepare_workers_hard_max_num: usize,
	) -> Self {
		Self {
			cache_path,
			node_version,
			secure_validator_mode,

			prepare_worker_program_path,
			prepare_worker_spawn_timeout: Duration::from_secs(3),
			prepare_workers_soft_max_num,
			prepare_workers_hard_max_num,

			execute_worker_program_path,
			execute_worker_spawn_timeout: Duration::from_secs(3),
			execute_workers_max_num,
		}
	}
}

/// Start the validation host.
///
/// Returns a [handle][`ValidationHost`] to the started validation host and the future. The future
/// must be polled in order for validation host to function.
///
/// The future should not return normally but if it does then that indicates an unrecoverable error.
/// In that case all pending requests will be canceled, dropping the result senders and new ones
/// will be rejected.
pub async fn start(
	config: Config,
	metrics: Metrics,
) -> SubsystemResult<(ValidationHost, impl Future<Output = ()>)> {
	gum::debug!(target: LOG_TARGET, ?config, "starting PVF validation host");

	// Make sure the cache is initialized before doing anything else.
	let artifacts = Artifacts::new(&config.cache_path).await;

	// Run checks for supported security features once per host startup. If some checks fail, warn
	// if Secure Validator Mode is disabled and return an error otherwise.
	#[cfg(target_os = "linux")]
	let security_status = match crate::security::check_security_status(&config).await {
		Ok(ok) => ok,
		Err(err) => return Err(SubsystemError::Context(err)),
	};
	#[cfg(not(target_os = "linux"))]
	let security_status = if config.secure_validator_mode {
		gum::error!(
			target: LOG_TARGET,
			"{}{}{}",
			crate::SECURE_MODE_ERROR,
			crate::SECURE_LINUX_NOTE,
			crate::IGNORE_SECURE_MODE_TIP
		);
		return Err(SubsystemError::Context(
			"could not enable Secure Validator Mode for non-Linux; check logs".into(),
		));
	} else {
		gum::warn!(
			target: LOG_TARGET,
			"{}{}",
			crate::SECURE_MODE_WARNING,
			crate::SECURE_LINUX_NOTE,
		);
		SecurityStatus::default()
	};

	let (to_host_tx, to_host_rx) = mpsc::channel(HOST_MESSAGE_QUEUE_SIZE);

	let validation_host = ValidationHost { to_host_tx, security_status: security_status.clone() };

	let (to_prepare_pool, from_prepare_pool, run_prepare_pool) = prepare::start_pool(
		metrics.clone(),
		config.prepare_worker_program_path.clone(),
		config.cache_path.clone(),
		config.prepare_worker_spawn_timeout,
		config.node_version.clone(),
		security_status.clone(),
	);

	let (to_prepare_queue_tx, from_prepare_queue_rx, run_prepare_queue) = prepare::start_queue(
		metrics.clone(),
		config.prepare_workers_soft_max_num,
		config.prepare_workers_hard_max_num,
		config.cache_path.clone(),
		to_prepare_pool,
		from_prepare_pool,
	);

	let (to_execute_queue_tx, from_execute_queue_rx, run_execute_queue) = execute::start(
		metrics,
		config.execute_worker_program_path.to_owned(),
		config.cache_path.clone(),
		config.execute_workers_max_num,
		config.execute_worker_spawn_timeout,
		config.node_version,
		security_status,
	);

	let (to_sweeper_tx, to_sweeper_rx) = mpsc::channel(100);
	let run_sweeper = sweeper_task(to_sweeper_rx);

	let run_host = async move {
		run(Inner {
			cleanup_pulse_interval: Duration::from_secs(3600),
			cleanup_config: ArtifactsCleanupConfig::default(),
			artifacts,
			to_host_rx,
			to_prepare_queue_tx,
			from_prepare_queue_rx,
			to_execute_queue_tx,
			from_execute_queue_rx,
			to_sweeper_tx,
			awaiting_prepare: AwaitingPrepare::default(),
		})
		.await
	};

	let task = async move {
		// Bundle the sub-components' tasks together into a single future.
		futures::select! {
			_ = run_host.fuse() => {},
			_ = run_prepare_queue.fuse() => {},
			_ = run_prepare_pool.fuse() => {},
			_ = run_execute_queue.fuse() => {},
			_ = run_sweeper.fuse() => {},
		};
	};

	Ok((validation_host, task))
}

/// A mapping from an artifact ID which is in preparation state to the list of pending execution
/// requests that should be executed once the artifact's preparation is finished.
#[derive(Default)]
struct AwaitingPrepare(HashMap<ArtifactId, Vec<PendingExecutionRequest>>);

impl AwaitingPrepare {
	fn add(&mut self, artifact_id: ArtifactId, pending_execution_request: PendingExecutionRequest) {
		self.0.entry(artifact_id).or_default().push(pending_execution_request);
	}

	fn take(&mut self, artifact_id: &ArtifactId) -> Vec<PendingExecutionRequest> {
		self.0.remove(artifact_id).unwrap_or_default()
	}
}

struct Inner {
	cleanup_pulse_interval: Duration,
	cleanup_config: ArtifactsCleanupConfig,
	artifacts: Artifacts,

	to_host_rx: mpsc::Receiver<ToHost>,

	to_prepare_queue_tx: mpsc::Sender<prepare::ToQueue>,
	from_prepare_queue_rx: mpsc::UnboundedReceiver<prepare::FromQueue>,

	to_execute_queue_tx: mpsc::Sender<execute::ToQueue>,
	from_execute_queue_rx: mpsc::UnboundedReceiver<execute::FromQueue>,

	to_sweeper_tx: mpsc::Sender<PathBuf>,

	awaiting_prepare: AwaitingPrepare,
}

#[derive(Debug)]
struct Fatal;

async fn run(
	Inner {
		cleanup_pulse_interval,
		cleanup_config,
		mut artifacts,
		to_host_rx,
		from_prepare_queue_rx,
		mut to_prepare_queue_tx,
		from_execute_queue_rx,
		mut to_execute_queue_tx,
		mut to_sweeper_tx,
		mut awaiting_prepare,
	}: Inner,
) {
	macro_rules! break_if_fatal {
		($expr:expr) => {
			match $expr {
				Err(Fatal) => {
					gum::error!(
						target: LOG_TARGET,
						"Fatal error occurred, terminating the host. Line: {}",
						line!(),
					);
					break
				},
				Ok(v) => v,
			}
		};
	}

	let cleanup_pulse = pulse_every(cleanup_pulse_interval).fuse();
	futures::pin_mut!(cleanup_pulse);

	let mut to_host_rx = to_host_rx.fuse();
	let mut from_prepare_queue_rx = from_prepare_queue_rx.fuse();
	let mut from_execute_queue_rx = from_execute_queue_rx.fuse();

	loop {
		// biased to make it behave deterministically for tests.
		futures::select_biased! {
			from_execute_queue_rx = from_execute_queue_rx.next() => {
				let from_queue = break_if_fatal!(from_execute_queue_rx.ok_or(Fatal));
				let execute::FromQueue::RemoveArtifact { artifact, reply_to } = from_queue;
				break_if_fatal!(handle_artifact_removal(
					&mut to_sweeper_tx,
					&mut artifacts,
					artifact,
					reply_to,
				).await);
			},
			() = cleanup_pulse.select_next_some() => {
				// `select_next_some` because we don't expect this to fail, but if it does, we
				// still don't fail. The trade-off is that the compiled cache will start growing
				// in size. That is, however, rather a slow process and hopefully the operator
				// will notice it.

				break_if_fatal!(handle_cleanup_pulse(
					&mut to_sweeper_tx,
					&mut artifacts,
					&cleanup_config,
				).await);
			},
			to_host = to_host_rx.next() => {
				let to_host = match to_host {
					None => {
						// The sending half of the channel has been closed, meaning the
						// `ValidationHost` struct was dropped. Shutting down gracefully.
						break;
					},
					Some(to_host) => to_host,
				};

				// If the artifact failed before, it could be re-scheduled for preparation here if
				// the preparation failure cooldown has elapsed.
				break_if_fatal!(handle_to_host(
					&mut artifacts,
					&mut to_prepare_queue_tx,
					&mut to_execute_queue_tx,
					&mut awaiting_prepare,
					to_host,
				)
				.await);
			},
			from_prepare_queue = from_prepare_queue_rx.next() => {
				let from_queue = break_if_fatal!(from_prepare_queue.ok_or(Fatal));

				// Note that the preparation outcome is always reported as concluded.
				//
				// That's because the error conditions are written into the artifact and will be
				// reported at the time of the execution. It potentially, but not necessarily, can
				// be scheduled for execution as a result of this function call, in case there are
				// pending executions.
				//
				// We could be eager in terms of reporting and plumb the result from the preparation
				// worker but we don't for the sake of simplicity.
				break_if_fatal!(handle_prepare_done(
					&mut artifacts,
					&mut to_execute_queue_tx,
					&mut awaiting_prepare,
					from_queue,
				).await);
			},
		}
	}
}

async fn handle_to_host(
	artifacts: &mut Artifacts,
	prepare_queue: &mut mpsc::Sender<prepare::ToQueue>,
	execute_queue: &mut mpsc::Sender<execute::ToQueue>,
	awaiting_prepare: &mut AwaitingPrepare,
	to_host: ToHost,
) -> Result<(), Fatal> {
	match to_host {
		ToHost::PrecheckPvf { pvf, result_tx } => {
			handle_precheck_pvf(artifacts, prepare_queue, pvf, result_tx).await?;
		},
		ToHost::ExecutePvf(inputs) => {
			handle_execute_pvf(artifacts, prepare_queue, execute_queue, awaiting_prepare, inputs)
				.await?;
		},
		ToHost::HeadsUp { active_pvfs } =>
			handle_heads_up(artifacts, prepare_queue, active_pvfs).await?,
		ToHost::UpdateActiveLeaves { update, ancestors } =>
			handle_update_active_leaves(execute_queue, update, ancestors).await?,
		#[cfg(feature = "test-utils")]
		ToHost::ReplaceArtifactChecksum { checksum, new_checksum } => {
			artifacts.replace_artifact_checksum(checksum, new_checksum);
		},
	}

	Ok(())
}

/// Handles PVF prechecking requests.
///
/// This tries to prepare the PVF by compiling the WASM blob within a timeout set in
/// `PvfPrepData`.
///
/// We don't retry artifacts that previously failed preparation. We don't expect multiple
/// pre-checking requests.
async fn handle_precheck_pvf(
	artifacts: &mut Artifacts,
	prepare_queue: &mut mpsc::Sender<prepare::ToQueue>,
	pvf: PvfPrepData,
	result_sender: PrecheckResultSender,
) -> Result<(), Fatal> {
	let artifact_id = ArtifactId::from_pvf_prep_data(&pvf);

	if let Some(state) = artifacts.artifact_state_mut(&artifact_id) {
		match state {
			ArtifactState::Prepared { last_time_needed, .. } => {
				*last_time_needed = SystemTime::now();
				let _ = result_sender.send(Ok(()));
			},
			ArtifactState::Preparing { waiting_for_response, num_failures: _ } =>
				waiting_for_response.push(result_sender),
			ArtifactState::FailedToProcess { error, .. } => {
				// Do not retry an artifact that previously failed preparation.
				let _ = result_sender.send(PrecheckResult::Err(error.clone()));
			},
		}
	} else {
		artifacts.insert_preparing(artifact_id, vec![result_sender]);
		send_prepare(prepare_queue, prepare::ToQueue::Enqueue { priority: Priority::Normal, pvf })
			.await?;
	}
	Ok(())
}

/// Handles PVF execution.
///
/// This will try to prepare the PVF, if a prepared artifact does not already exist. If there is
/// already a preparation job, we coalesce the two preparation jobs.
///
/// If the prepare job succeeded previously, we will enqueue an execute job right away.
///
/// If the prepare job failed previously, we may retry it under certain conditions.
///
/// When preparing for execution, we use a more lenient timeout
/// ([`DEFAULT_LENIENT_PREPARATION_TIMEOUT`](polkadot_primitives::executor_params::DEFAULT_LENIENT_PREPARATION_TIMEOUT))
/// than when prechecking.
async fn handle_execute_pvf(
	artifacts: &mut Artifacts,
	prepare_queue: &mut mpsc::Sender<prepare::ToQueue>,
	execute_queue: &mut mpsc::Sender<execute::ToQueue>,
	awaiting_prepare: &mut AwaitingPrepare,
	inputs: ExecutePvfInputs,
) -> Result<(), Fatal> {
	let ExecutePvfInputs { pvf, exec_timeout, pvd, pov, priority, exec_kind, result_tx } = inputs;
	let artifact_id = ArtifactId::from_pvf_prep_data(&pvf);
	let executor_params = (*pvf.executor_params()).clone();

	if let Some(state) = artifacts.artifact_state_mut(&artifact_id) {
		match state {
			ArtifactState::Prepared { ref path, checksum, last_time_needed, .. } => {
				let file_metadata = std::fs::metadata(path);

				if file_metadata.is_ok() {
					*last_time_needed = SystemTime::now();

					// This artifact has already been prepared, send it to the execute queue.
					send_execute(
						execute_queue,
						execute::ToQueue::Enqueue {
							artifact: ArtifactPathId::new(artifact_id, path, *checksum),
							pending_execution_request: PendingExecutionRequest {
								exec_timeout,
								pvd,
								pov,
								executor_params,
								exec_kind,
								result_tx,
							},
						},
					)
					.await?;
				} else {
					gum::warn!(
						target: LOG_TARGET,
						?pvf,
						?artifact_id,
						"handle_execute_pvf: Re-queuing PVF preparation for prepared artifact with missing file."
					);

					// The artifact has been prepared previously but the file is missing, prepare it
					// again.
					*state = ArtifactState::Preparing {
						waiting_for_response: Vec::new(),
						num_failures: 0,
					};
					enqueue_prepare_for_execute(
						prepare_queue,
						awaiting_prepare,
						pvf,
						priority,
						artifact_id,
						PendingExecutionRequest {
							exec_timeout,
							pvd,
							pov,
							executor_params,
							exec_kind,
							result_tx,
						},
					)
					.await?;
				}
			},
			ArtifactState::Preparing { .. } => {
				awaiting_prepare.add(
					artifact_id,
					PendingExecutionRequest {
						exec_timeout,
						pvd,
						pov,
						executor_params,
						result_tx,
						exec_kind,
					},
				);
			},
			ArtifactState::FailedToProcess { last_time_failed, num_failures, error } => {
				if can_retry_prepare_after_failure(*last_time_failed, *num_failures, error) {
					gum::warn!(
						target: LOG_TARGET,
						?pvf,
						?artifact_id,
						?last_time_failed,
						%num_failures,
						%error,
						"handle_execute_pvf: Re-trying failed PVF preparation."
					);

					// If we are allowed to retry the failed prepare job, change the state to
					// Preparing and re-queue this job.
					*state = ArtifactState::Preparing {
						waiting_for_response: Vec::new(),
						num_failures: *num_failures,
					};
					enqueue_prepare_for_execute(
						prepare_queue,
						awaiting_prepare,
						pvf,
						priority,
						artifact_id,
						PendingExecutionRequest {
							exec_timeout,
							pvd,
							pov,
							executor_params,
							exec_kind,
							result_tx,
						},
					)
					.await?;
				} else {
					let _ = result_tx.send(Err(ValidationError::from(error.clone())));
				}
			},
		}
	} else {
		// Artifact is unknown: register it and enqueue a job with the corresponding priority and
		// PVF.
		artifacts.insert_preparing(artifact_id.clone(), Vec::new());
		enqueue_prepare_for_execute(
			prepare_queue,
			awaiting_prepare,
			pvf,
			priority,
			artifact_id,
			PendingExecutionRequest {
				exec_timeout,
				pvd,
				pov,
				executor_params,
				result_tx,
				exec_kind,
			},
		)
		.await?;
	}

	Ok(())
}

async fn handle_heads_up(
	artifacts: &mut Artifacts,
	prepare_queue: &mut mpsc::Sender<prepare::ToQueue>,
	active_pvfs: Vec<PvfPrepData>,
) -> Result<(), Fatal> {
	let now = SystemTime::now();

	for active_pvf in active_pvfs {
		let artifact_id = ArtifactId::from_pvf_prep_data(&active_pvf);
		if let Some(state) = artifacts.artifact_state_mut(&artifact_id) {
			match state {
				ArtifactState::Prepared { last_time_needed, .. } => {
					*last_time_needed = now;
				},
				ArtifactState::Preparing { .. } => {
					// The artifact is already being prepared, so we don't need to do anything.
				},
				ArtifactState::FailedToProcess { last_time_failed, num_failures, error } => {
					if can_retry_prepare_after_failure(*last_time_failed, *num_failures, error) {
						gum::warn!(
							target: LOG_TARGET,
							?active_pvf,
							?artifact_id,
							?last_time_failed,
							%num_failures,
							%error,
							"handle_heads_up: Re-trying failed PVF preparation."
						);

						// If we are allowed to retry the failed prepare job, change the state to
						// Preparing and re-queue this job.
						*state = ArtifactState::Preparing {
							waiting_for_response: vec![],
							num_failures: *num_failures,
						};
						send_prepare(
							prepare_queue,
							prepare::ToQueue::Enqueue {
								priority: Priority::Normal,
								pvf: active_pvf,
							},
						)
						.await?;
					}
				},
			}
		} else {
			// It's not in the artifacts, so we need to enqueue a job to prepare it.
			artifacts.insert_preparing(artifact_id.clone(), Vec::new());

			send_prepare(
				prepare_queue,
				prepare::ToQueue::Enqueue { priority: Priority::Normal, pvf: active_pvf },
			)
			.await?;
		}
	}

	Ok(())
}

async fn handle_prepare_done(
	artifacts: &mut Artifacts,
	execute_queue: &mut mpsc::Sender<execute::ToQueue>,
	awaiting_prepare: &mut AwaitingPrepare,
	from_queue: prepare::FromQueue,
) -> Result<(), Fatal> {
	let prepare::FromQueue { artifact_id, result } = from_queue;

	// Make some sanity checks and extract the current state.
	let state = match artifacts.artifact_state_mut(&artifact_id) {
		None => {
			// before sending request to prepare, the artifact is inserted with `preparing` state;
			// the requests are deduplicated for the same artifact id;
			// there is only one possible state change: prepare is done;
			// thus the artifact cannot be unknown, only preparing;
			// qed.
			never!("an unknown artifact was prepared: {:?}", artifact_id);
			return Ok(())
		},
		Some(ArtifactState::Prepared { .. }) => {
			// before sending request to prepare, the artifact is inserted with `preparing` state;
			// the requests are deduplicated for the same artifact id;
			// there is only one possible state change: prepare is done;
			// thus the artifact cannot be prepared, only preparing;
			// qed.
			never!("the artifact is already prepared: {:?}", artifact_id);
			return Ok(())
		},
		Some(ArtifactState::FailedToProcess { .. }) => {
			// The reasoning is similar to the above, the artifact cannot be
			// processed at this point.
			never!("the artifact is already processed unsuccessfully: {:?}", artifact_id);
			return Ok(())
		},
		Some(state @ ArtifactState::Preparing { .. }) => state,
	};

	let num_failures = if let ArtifactState::Preparing { waiting_for_response, num_failures } =
		state
	{
		for result_sender in waiting_for_response.drain(..) {
			let result = result.clone().map(|_| ());
			let _ = result_sender.send(result);
		}
		num_failures
	} else {
		never!("The reasoning is similar to the above, the artifact can only be preparing at this point; qed");
		return Ok(())
	};

	// It's finally time to dispatch all the execution requests that were waiting for this artifact
	// to be prepared.
	let pending_requests = awaiting_prepare.take(&artifact_id);
	for PendingExecutionRequest { exec_timeout, pvd, pov, executor_params, result_tx, exec_kind } in
		pending_requests
	{
		if result_tx.is_canceled() {
			// Preparation could've taken quite a bit of time and the requester may be not
			// interested in execution anymore, in which case we just skip the request.
			continue
		}

		let (path, checksum) = match &result {
			Ok(success) => (success.path.clone(), success.checksum),
			Err(error) => {
				let _ = result_tx.send(Err(ValidationError::from(error.clone())));
				continue
			},
		};

		send_execute(
			execute_queue,
			execute::ToQueue::Enqueue {
				artifact: ArtifactPathId::new(artifact_id.clone(), &path, checksum),
				pending_execution_request: PendingExecutionRequest {
					exec_timeout,
					pvd,
					pov,
					executor_params,
					exec_kind,
					result_tx,
				},
			},
		)
		.await?;
	}

	*state = match result {
		Ok(PrepareSuccess { checksum, path, size, .. }) =>
			ArtifactState::Prepared { checksum, path, last_time_needed: SystemTime::now(), size },
		Err(error) => {
			let last_time_failed = SystemTime::now();
			let num_failures = *num_failures + 1;

			gum::error!(
				target: LOG_TARGET,
				?artifact_id,
				time_failed = ?last_time_failed,
				%num_failures,
				"artifact preparation failed: {}",
				error
			);
			ArtifactState::FailedToProcess { last_time_failed, num_failures, error }
		},
	};

	Ok(())
}

async fn handle_update_active_leaves(
	execute_queue: &mut mpsc::Sender<execute::ToQueue>,
	update: ActiveLeavesUpdate,
	ancestors: Vec<Hash>,
) -> Result<(), Fatal> {
	send_execute(execute_queue, execute::ToQueue::UpdateActiveLeaves { update, ancestors }).await
}

async fn send_prepare(
	prepare_queue: &mut mpsc::Sender<prepare::ToQueue>,
	to_queue: prepare::ToQueue,
) -> Result<(), Fatal> {
	prepare_queue.send(to_queue).await.map_err(|_| Fatal)
}

async fn send_execute(
	execute_queue: &mut mpsc::Sender<execute::ToQueue>,
	to_queue: execute::ToQueue,
) -> Result<(), Fatal> {
	execute_queue.send(to_queue).await.map_err(|_| Fatal)
}

/// Sends a job to the preparation queue, and adds an execution request that will wait to run after
/// this prepare job has finished.
async fn enqueue_prepare_for_execute(
	prepare_queue: &mut mpsc::Sender<prepare::ToQueue>,
	awaiting_prepare: &mut AwaitingPrepare,
	pvf: PvfPrepData,
	priority: Priority,
	artifact_id: ArtifactId,
	pending_execution_request: PendingExecutionRequest,
) -> Result<(), Fatal> {
	send_prepare(prepare_queue, prepare::ToQueue::Enqueue { priority, pvf }).await?;

	// Add an execution request that will wait to run after this prepare job has finished.
	awaiting_prepare.add(artifact_id, pending_execution_request);

	Ok(())
}

async fn handle_cleanup_pulse(
	sweeper_tx: &mut mpsc::Sender<PathBuf>,
	artifacts: &mut Artifacts,
	cleanup_config: &ArtifactsCleanupConfig,
) -> Result<(), Fatal> {
	let to_remove = artifacts.prune(cleanup_config);
	gum::debug!(
		target: LOG_TARGET,
		"PVF pruning: {} artifacts reached their end of life",
		to_remove.len(),
	);
	for (artifact_id, path) in to_remove {
		gum::debug!(
			target: LOG_TARGET,
			validation_code_hash = ?artifact_id.code_hash,
			"pruning artifact",
		);
		sweeper_tx.send(path).await.map_err(|_| Fatal)?;
	}

	Ok(())
}

async fn handle_artifact_removal(
	sweeper_tx: &mut mpsc::Sender<PathBuf>,
	artifacts: &mut Artifacts,
	artifact_id: ArtifactId,
	reply_to: oneshot::Sender<()>,
) -> Result<(), Fatal> {
	let (artifact_id, path) = if let Some(artifact) = artifacts.remove(artifact_id) {
		artifact
	} else {
		// if we haven't found the artifact by its id,
		// it has been probably removed
		// anyway with the randomness of the artifact name
		// it is safe to ignore
		return Ok(());
	};
	reply_to
		.send(())
		.expect("the execute queue waits for the artifact remove confirmation; qed");
	// Thanks to the randomness of the artifact name (see
	// `artifacts::generate_artifact_path`) there is no issue with any name conflict on
	// future repreparation.
	// So we can confirm the artifact removal already
	gum::debug!(
		target: LOG_TARGET,
		validation_code_hash = ?artifact_id.code_hash,
		"PVF pruning: pruning artifact by request from the execute queue",
	);
	sweeper_tx.send(path).await.map_err(|_| Fatal)?;
	Ok(())
}

/// A simple task which sole purpose is to delete files thrown at it.
async fn sweeper_task(mut sweeper_rx: mpsc::Receiver<PathBuf>) {
	loop {
		match sweeper_rx.next().await {
			None => break,
			Some(condemned) => {
				let result = tokio::fs::remove_file(&condemned).await;
				gum::trace!(
					target: LOG_TARGET,
					?result,
					"Swept the artifact file {}",
					condemned.display(),
				);
			},
		}
	}
}

/// Check if the conditions to retry a prepare job have been met.
fn can_retry_prepare_after_failure(
	last_time_failed: SystemTime,
	num_failures: u32,
	error: &PrepareError,
) -> bool {
	if error.is_deterministic() {
		// This error is considered deterministic, so it will probably be reproducible. Don't retry.
		return false
	}

	// Retry if the retry cooldown has elapsed and if we have already retried less than
	// `NUM_PREPARE_RETRIES` times. IO errors may resolve themselves.
	SystemTime::now() >= last_time_failed + PREPARE_FAILURE_COOLDOWN &&
		num_failures <= NUM_PREPARE_RETRIES
}

/// A stream that yields a pulse continuously at a given interval.
fn pulse_every(interval: std::time::Duration) -> impl futures::Stream<Item = ()> {
	futures::stream::unfold(interval, {
		|interval| async move {
			futures_timer::Delay::new(interval).await;
			Some(((), interval))
		}
	})
	.map(|_| ())
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::{artifacts::generate_artifact_path, testing::artifact_id, PossiblyInvalidError};
	use assert_matches::assert_matches;
	use futures::future::BoxFuture;
	use polkadot_node_primitives::BlockData;
	use sp_core::H256;

	const TEST_EXECUTION_TIMEOUT: Duration = Duration::from_secs(3);
	pub(crate) const TEST_PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);

	#[tokio::test]
	async fn pulse_test() {
		let pulse = pulse_every(Duration::from_millis(100));
		futures::pin_mut!(pulse);

		for _ in 0..5 {
			let start = std::time::Instant::now();
			let _ = pulse.next().await.unwrap();

			let el = start.elapsed().as_millis();
			assert!(el > 50 && el < 150, "pulse duration: {}", el);
		}
	}

	struct Builder {
		cleanup_pulse_interval: Duration,
		cleanup_config: ArtifactsCleanupConfig,
		artifacts: Artifacts,
	}

	impl Builder {
		fn default() -> Self {
			Self {
				// these are selected high to not interfere in tests in which pruning is irrelevant.
				cleanup_pulse_interval: Duration::from_secs(3600),
				cleanup_config: ArtifactsCleanupConfig::default(),
				artifacts: Artifacts::empty(),
			}
		}

		fn build(self) -> Test {
			Test::new(self)
		}
	}

	struct Test {
		to_host_tx: Option<mpsc::Sender<ToHost>>,

		to_prepare_queue_rx: mpsc::Receiver<prepare::ToQueue>,
		from_prepare_queue_tx: mpsc::UnboundedSender<prepare::FromQueue>,
		to_execute_queue_rx: mpsc::Receiver<execute::ToQueue>,
		#[allow(unused)]
		from_execute_queue_tx: mpsc::UnboundedSender<execute::FromQueue>,
		to_sweeper_rx: mpsc::Receiver<PathBuf>,

		run: BoxFuture<'static, ()>,
	}

	impl Test {
		fn new(Builder { cleanup_pulse_interval, artifacts, cleanup_config }: Builder) -> Self {
			let (to_host_tx, to_host_rx) = mpsc::channel(10);
			let (to_prepare_queue_tx, to_prepare_queue_rx) = mpsc::channel(10);
			let (from_prepare_queue_tx, from_prepare_queue_rx) = mpsc::unbounded();
			let (to_execute_queue_tx, to_execute_queue_rx) = mpsc::channel(10);
			let (from_execute_queue_tx, from_execute_queue_rx) = mpsc::unbounded();
			let (to_sweeper_tx, to_sweeper_rx) = mpsc::channel(10);

			let run = run(Inner {
				cleanup_pulse_interval,
				cleanup_config,
				artifacts,
				to_host_rx,
				to_prepare_queue_tx,
				from_prepare_queue_rx,
				to_execute_queue_tx,
				from_execute_queue_rx,
				to_sweeper_tx,
				awaiting_prepare: AwaitingPrepare::default(),
			})
			.boxed();

			Self {
				to_host_tx: Some(to_host_tx),
				to_prepare_queue_rx,
				from_prepare_queue_tx,
				to_execute_queue_rx,
				from_execute_queue_tx,
				to_sweeper_rx,
				run,
			}
		}

		fn host_handle(&mut self) -> ValidationHost {
			let to_host_tx = self.to_host_tx.take().unwrap();
			let security_status = Default::default();
			ValidationHost { to_host_tx, security_status }
		}

		async fn poll_and_recv_result<T>(&mut self, result_rx: oneshot::Receiver<T>) -> T
		where
			T: Send,
		{
			run_until(&mut self.run, async { result_rx.await.unwrap() }.boxed()).await
		}

		async fn poll_and_recv_to_prepare_queue(&mut self) -> prepare::ToQueue {
			let to_prepare_queue_rx = &mut self.to_prepare_queue_rx;
			run_until(&mut self.run, async { to_prepare_queue_rx.next().await.unwrap() }.boxed())
				.await
		}

		async fn poll_and_recv_to_execute_queue(&mut self) -> execute::ToQueue {
			let to_execute_queue_rx = &mut self.to_execute_queue_rx;
			run_until(&mut self.run, async { to_execute_queue_rx.next().await.unwrap() }.boxed())
				.await
		}

		async fn poll_ensure_to_prepare_queue_is_empty(&mut self) {
			use futures_timer::Delay;

			let to_prepare_queue_rx = &mut self.to_prepare_queue_rx;
			run_until(
				&mut self.run,
				async {
					futures::select! {
						_ = Delay::new(Duration::from_millis(500)).fuse() => (),
						_ = to_prepare_queue_rx.next().fuse() => {
							panic!("the prepare queue is supposed to be empty")
						}
					}
				}
				.boxed(),
			)
			.await
		}

		async fn poll_ensure_to_execute_queue_is_empty(&mut self) {
			use futures_timer::Delay;

			let to_execute_queue_rx = &mut self.to_execute_queue_rx;
			run_until(
				&mut self.run,
				async {
					futures::select! {
						_ = Delay::new(Duration::from_millis(500)).fuse() => (),
						_ = to_execute_queue_rx.next().fuse() => {
							panic!("the execute queue is supposed to be empty")
						}
					}
				}
				.boxed(),
			)
			.await
		}

		async fn poll_ensure_to_sweeper_is_empty(&mut self) {
			use futures_timer::Delay;

			let to_sweeper_rx = &mut self.to_sweeper_rx;
			run_until(
				&mut self.run,
				async {
					futures::select! {
						_ = Delay::new(Duration::from_millis(500)).fuse() => (),
						msg = to_sweeper_rx.next().fuse() => {
							panic!("the sweeper is supposed to be empty, but received: {:?}", msg)
						}
					}
				}
				.boxed(),
			)
			.await
		}
	}

	async fn run_until<R>(
		task: &mut (impl Future<Output = ()> + Unpin),
		mut fut: (impl Future<Output = R> + Unpin),
	) -> R {
		use std::task::Poll;

		let start = std::time::Instant::now();
		let fut = &mut fut;
		loop {
			if start.elapsed() > std::time::Duration::from_secs(2) {
				// We expect that this will take only a couple of iterations and thus to take way
				// less than a second.
				panic!("timeout");
			}

			if let Poll::Ready(r) = futures::poll!(&mut *fut) {
				break r
			}

			if futures::poll!(&mut *task).is_ready() {
				panic!()
			}
		}
	}

	#[tokio::test]
	async fn shutdown_on_handle_drop() {
		let test = Builder::default().build();

		let join_handle = tokio::task::spawn(test.run);

		// Dropping the handle will lead to conclusion of the read part and thus will make the event
		// loop to stop, which in turn will resolve the join handle.
		drop(test.to_host_tx);
		join_handle.await.unwrap();
	}

	#[tokio::test]
	async fn pruning() {
		let mock_now = SystemTime::now() - Duration::from_millis(1000);
		let tempdir = tempfile::tempdir().unwrap();
		let cache_path = tempdir.path();

		let mut builder = Builder::default();
		builder.cleanup_pulse_interval = Duration::from_millis(100);
		builder.cleanup_config = ArtifactsCleanupConfig::new(1024, Duration::from_secs(0));
		let path1 = generate_artifact_path(cache_path);
		let path2 = generate_artifact_path(cache_path);
		builder.artifacts.insert_prepared(
			artifact_id(1),
			path1.clone(),
			Default::default(),
			mock_now,
			1024,
		);
		builder.artifacts.insert_prepared(
			artifact_id(2),
			path2.clone(),
			Default::default(),
			mock_now,
			1024,
		);
		let mut test = builder.build();
		let mut host = test.host_handle();

		host.heads_up(vec![PvfPrepData::from_discriminator(1)]).await.unwrap();

		let to_sweeper_rx = &mut test.to_sweeper_rx;
		run_until(
			&mut test.run,
			async {
				assert_eq!(to_sweeper_rx.next().await.unwrap(), path2);
			}
			.boxed(),
		)
		.await;

		// Extend TTL for the first artifact and make sure we don't receive another file removal
		// request.
		host.heads_up(vec![PvfPrepData::from_discriminator(1)]).await.unwrap();
		test.poll_ensure_to_sweeper_is_empty().await;
	}

	#[tokio::test]
	async fn execute_pvf_requests() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();
		let pvd = Arc::new(PersistedValidationData {
			parent_head: Default::default(),
			relay_parent_number: 1u32,
			relay_parent_storage_root: H256::default(),
			max_pov_size: 4096 * 1024,
		});
		let pov1 = Arc::new(PoV { block_data: BlockData(b"pov1".to_vec()) });
		let pov2 = Arc::new(PoV { block_data: BlockData(b"pov2".to_vec()) });

		let (result_tx, result_rx_pvf_1_1) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov1.clone(),
			Priority::Normal,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		let (result_tx, result_rx_pvf_1_2) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov1,
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		let (result_tx, result_rx_pvf_2) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(2),
			TEST_EXECUTION_TIMEOUT,
			pvd,
			pov2,
			Priority::Normal,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);

		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Ok(PrepareSuccess::default()),
			})
			.await
			.unwrap();
		let result_tx_pvf_1_1 = assert_matches!(
			test.poll_and_recv_to_execute_queue().await,
			execute::ToQueue::Enqueue { pending_execution_request: PendingExecutionRequest { result_tx, .. }, .. } => result_tx
		);
		let result_tx_pvf_1_2 = assert_matches!(
			test.poll_and_recv_to_execute_queue().await,
			execute::ToQueue::Enqueue { pending_execution_request: PendingExecutionRequest { result_tx, .. }, .. } => result_tx
		);

		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(2),
				result: Ok(PrepareSuccess::default()),
			})
			.await
			.unwrap();
		let result_tx_pvf_2 = assert_matches!(
			test.poll_and_recv_to_execute_queue().await,
			execute::ToQueue::Enqueue { pending_execution_request: PendingExecutionRequest { result_tx, .. }, .. } => result_tx
		);

		result_tx_pvf_1_1
			.send(Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)))
			.unwrap();
		assert_matches!(
			result_rx_pvf_1_1.now_or_never().unwrap().unwrap(),
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath))
		);

		result_tx_pvf_1_2
			.send(Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)))
			.unwrap();
		assert_matches!(
			result_rx_pvf_1_2.now_or_never().unwrap().unwrap(),
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath))
		);

		result_tx_pvf_2
			.send(Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)))
			.unwrap();
		assert_matches!(
			result_rx_pvf_2.now_or_never().unwrap().unwrap(),
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath))
		);
	}

	#[tokio::test]
	async fn precheck_pvf() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();

		// First, test a simple precheck request.
		let (result_tx, result_rx) = oneshot::channel();
		host.precheck_pvf(PvfPrepData::from_discriminator_precheck(1), result_tx)
			.await
			.unwrap();

		// The queue received the prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		// Send `Ok` right away and poll the host.
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Ok(PrepareSuccess::default()),
			})
			.await
			.unwrap();
		// No pending execute requests.
		test.poll_ensure_to_execute_queue_is_empty().await;
		// Received the precheck result.
		assert_matches!(result_rx.now_or_never().unwrap().unwrap(), Ok(_));

		// Send multiple requests for the same PVF.
		let mut precheck_receivers = Vec::new();
		for _ in 0..3 {
			let (result_tx, result_rx) = oneshot::channel();
			host.precheck_pvf(PvfPrepData::from_discriminator_precheck(2), result_tx)
				.await
				.unwrap();
			precheck_receivers.push(result_rx);
		}
		// Received prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(2),
				result: Err(PrepareError::TimedOut),
			})
			.await
			.unwrap();
		test.poll_ensure_to_execute_queue_is_empty().await;
		for result_rx in precheck_receivers {
			assert_matches!(
				result_rx.now_or_never().unwrap().unwrap(),
				Err(PrepareError::TimedOut)
			);
		}
	}

	#[tokio::test]
	async fn test_prepare_done() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();
		let pvd = Arc::new(PersistedValidationData {
			parent_head: Default::default(),
			relay_parent_number: 1u32,
			relay_parent_storage_root: H256::default(),
			max_pov_size: 4096 * 1024,
		});
		let pov = Arc::new(PoV { block_data: BlockData(b"pov".to_vec()) });

		// Test mixed cases of receiving execute and precheck requests
		// for the same PVF.

		// Send PVF for the execution and request the prechecking for it.
		let (result_tx, result_rx_execute) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);

		let (result_tx, result_rx) = oneshot::channel();
		host.precheck_pvf(PvfPrepData::from_discriminator_precheck(1), result_tx)
			.await
			.unwrap();

		// Suppose the preparation failed, the execution queue is empty and both
		// "clients" receive their results.
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Err(PrepareError::TimedOut),
			})
			.await
			.unwrap();
		test.poll_ensure_to_execute_queue_is_empty().await;
		assert_matches!(result_rx.now_or_never().unwrap().unwrap(), Err(PrepareError::TimedOut));
		assert_matches!(
			result_rx_execute.now_or_never().unwrap().unwrap(),
			Err(ValidationError::Internal(_))
		);

		// Reversed case: first send multiple precheck requests, then ask for an execution.
		let mut precheck_receivers = Vec::new();
		for _ in 0..3 {
			let (result_tx, result_rx) = oneshot::channel();
			host.precheck_pvf(PvfPrepData::from_discriminator_precheck(2), result_tx)
				.await
				.unwrap();
			precheck_receivers.push(result_rx);
		}

		let (result_tx, _result_rx_execute) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(2),
			TEST_EXECUTION_TIMEOUT,
			pvd,
			pov,
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();
		// Received prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(2),
				result: Ok(PrepareSuccess::default()),
			})
			.await
			.unwrap();
		// The execute queue receives new request, preckecking is finished and we can
		// fetch results.
		assert_matches!(
			test.poll_and_recv_to_execute_queue().await,
			execute::ToQueue::Enqueue { .. }
		);
		for result_rx in precheck_receivers {
			assert_matches!(result_rx.now_or_never().unwrap().unwrap(), Ok(_));
		}
	}

	// Test that multiple prechecking requests do not trigger preparation retries if the first one
	// failed.
	#[tokio::test]
	async fn test_precheck_prepare_no_retry() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();

		// Submit a precheck request that fails.
		let (result_tx, result_rx) = oneshot::channel();
		host.precheck_pvf(PvfPrepData::from_discriminator_precheck(1), result_tx)
			.await
			.unwrap();

		// The queue received the prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		// Send a PrepareError.
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Err(PrepareError::TimedOut),
			})
			.await
			.unwrap();

		// The result should contain the error.
		let result = test.poll_and_recv_result(result_rx).await;
		assert_matches!(result, Err(PrepareError::TimedOut));

		// Submit another precheck request.
		let (result_tx_2, result_rx_2) = oneshot::channel();
		host.precheck_pvf(PvfPrepData::from_discriminator_precheck(1), result_tx_2)
			.await
			.unwrap();

		// Assert the prepare queue is empty.
		test.poll_ensure_to_prepare_queue_is_empty().await;

		// The result should contain the original error.
		let result = test.poll_and_recv_result(result_rx_2).await;
		assert_matches!(result, Err(PrepareError::TimedOut));

		// Pause for enough time to reset the cooldown for this failed prepare request.
		futures_timer::Delay::new(PREPARE_FAILURE_COOLDOWN).await;

		// Submit another precheck request.
		let (result_tx_3, result_rx_3) = oneshot::channel();
		host.precheck_pvf(PvfPrepData::from_discriminator_precheck(1), result_tx_3)
			.await
			.unwrap();

		// Assert the prepare queue is empty - we do not retry for precheck requests.
		test.poll_ensure_to_prepare_queue_is_empty().await;

		// The result should still contain the original error.
		let result = test.poll_and_recv_result(result_rx_3).await;
		assert_matches!(result, Err(PrepareError::TimedOut));
	}

	// Test that multiple execution requests trigger preparation retries if the first one failed due
	// to a potentially non-reproducible error.
	#[tokio::test]
	async fn test_execute_prepare_retry() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();
		let pvd = Arc::new(PersistedValidationData {
			parent_head: Default::default(),
			relay_parent_number: 1u32,
			relay_parent_storage_root: H256::default(),
			max_pov_size: 4096 * 1024,
		});
		let pov = Arc::new(PoV { block_data: BlockData(b"pov".to_vec()) });

		// Submit a execute request that fails.
		let (result_tx, result_rx) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		// The queue received the prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		// Send a PrepareError.
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Err(PrepareError::TimedOut),
			})
			.await
			.unwrap();

		// The result should contain the error.
		let result = test.poll_and_recv_result(result_rx).await;
		assert_matches!(result, Err(ValidationError::Internal(_)));

		// Submit another execute request. We shouldn't try to prepare again, yet.
		let (result_tx_2, result_rx_2) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx_2,
		)
		.await
		.unwrap();

		// Assert the prepare queue is empty.
		test.poll_ensure_to_prepare_queue_is_empty().await;

		// The result should contain the original error.
		let result = test.poll_and_recv_result(result_rx_2).await;
		assert_matches!(result, Err(ValidationError::Internal(_)));

		// Pause for enough time to reset the cooldown for this failed prepare request.
		futures_timer::Delay::new(PREPARE_FAILURE_COOLDOWN).await;

		// Submit another execute request.
		let (result_tx_3, result_rx_3) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx_3,
		)
		.await
		.unwrap();

		// Assert the prepare queue contains the request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);

		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Ok(PrepareSuccess::default()),
			})
			.await
			.unwrap();

		// Preparation should have been retried and succeeded this time.
		let result_tx_3 = assert_matches!(
			test.poll_and_recv_to_execute_queue().await,
			execute::ToQueue::Enqueue { pending_execution_request: PendingExecutionRequest { result_tx, .. }, .. } => result_tx
		);

		// Send an error for the execution here, just so we can check the result receiver is still
		// alive.
		result_tx_3
			.send(Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)))
			.unwrap();
		assert_matches!(
			result_rx_3.now_or_never().unwrap().unwrap(),
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath))
		);
	}

	// Test that multiple execution requests don't trigger preparation retries if the first one
	// failed due to a reproducible error (e.g. Prevalidation).
	#[tokio::test]
	async fn test_execute_prepare_no_retry() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();
		let pvd = Arc::new(PersistedValidationData {
			parent_head: Default::default(),
			relay_parent_number: 1u32,
			relay_parent_storage_root: H256::default(),
			max_pov_size: 4096 * 1024,
		});
		let pov = Arc::new(PoV { block_data: BlockData(b"pov".to_vec()) });

		// Submit an execute request that fails.
		let (result_tx, result_rx) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		// The queue received the prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		// Send a PrepareError.
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Err(PrepareError::Prevalidation("reproducible error".into())),
			})
			.await
			.unwrap();

		// The result should contain the error.
		let result = test.poll_and_recv_result(result_rx).await;
		assert_matches!(result, Err(ValidationError::Preparation(_)));

		// Submit another execute request.
		let (result_tx_2, result_rx_2) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx_2,
		)
		.await
		.unwrap();

		// Assert the prepare queue is empty.
		test.poll_ensure_to_prepare_queue_is_empty().await;

		// The result should contain the original error.
		let result = test.poll_and_recv_result(result_rx_2).await;
		assert_matches!(result, Err(ValidationError::Preparation(_)));

		// Pause for enough time to reset the cooldown for this failed prepare request.
		futures_timer::Delay::new(PREPARE_FAILURE_COOLDOWN).await;

		// Submit another execute request.
		let (result_tx_3, result_rx_3) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd.clone(),
			pov.clone(),
			Priority::Critical,
			PvfExecKind::Backing(H256::default()),
			result_tx_3,
		)
		.await
		.unwrap();

		// Assert the prepare queue is empty - we do not retry for prevalidation errors.
		test.poll_ensure_to_prepare_queue_is_empty().await;

		// The result should still contain the original error.
		let result = test.poll_and_recv_result(result_rx_3).await;
		assert_matches!(result, Err(ValidationError::Preparation(_)));
	}

	// Test that multiple heads-up requests trigger preparation retries if the first one failed.
	#[tokio::test]
	async fn test_heads_up_prepare_retry() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();

		// Submit a heads-up request that fails.
		host.heads_up(vec![PvfPrepData::from_discriminator(1)]).await.unwrap();

		// The queue received the prepare request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
		// Send a PrepareError.
		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Err(PrepareError::TimedOut),
			})
			.await
			.unwrap();

		// Submit another heads-up request.
		host.heads_up(vec![PvfPrepData::from_discriminator(1)]).await.unwrap();

		// Assert the prepare queue is empty.
		test.poll_ensure_to_prepare_queue_is_empty().await;

		// Pause for enough time to reset the cooldown for this failed prepare request.
		futures_timer::Delay::new(PREPARE_FAILURE_COOLDOWN).await;

		// Submit another heads-up request.
		host.heads_up(vec![PvfPrepData::from_discriminator(1)]).await.unwrap();

		// Assert the prepare queue contains the request.
		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);
	}

	#[tokio::test]
	async fn cancellation() {
		let mut test = Builder::default().build();
		let mut host = test.host_handle();
		let pvd = Arc::new(PersistedValidationData {
			parent_head: Default::default(),
			relay_parent_number: 1u32,
			relay_parent_storage_root: H256::default(),
			max_pov_size: 4096 * 1024,
		});
		let pov = Arc::new(PoV { block_data: BlockData(b"pov".to_vec()) });

		let (result_tx, result_rx) = oneshot::channel();
		host.execute_pvf(
			PvfPrepData::from_discriminator(1),
			TEST_EXECUTION_TIMEOUT,
			pvd,
			pov,
			Priority::Normal,
			PvfExecKind::Backing(H256::default()),
			result_tx,
		)
		.await
		.unwrap();

		assert_matches!(
			test.poll_and_recv_to_prepare_queue().await,
			prepare::ToQueue::Enqueue { .. }
		);

		test.from_prepare_queue_tx
			.send(prepare::FromQueue {
				artifact_id: artifact_id(1),
				result: Ok(PrepareSuccess::default()),
			})
			.await
			.unwrap();

		drop(result_rx);

		test.poll_ensure_to_execute_queue_is_empty().await;
	}
}
