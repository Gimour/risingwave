// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use risingwave_hummock_sdk::compact::compact_task_to_string;
use risingwave_hummock_sdk::CompactionGroupId;
use risingwave_pb::hummock::compact_task::TaskStatus;
use risingwave_pb::hummock::subscribe_compact_tasks_response::Task;
use risingwave_pb::hummock::CompactTask;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot::Receiver;

use crate::hummock::error::Error;
use crate::hummock::{CompactorManagerRef, HummockManagerRef};
use crate::manager::{LocalNotification, MetaSrvEnv};
use crate::storage::MetaStore;

pub type CompactionSchedulerRef<S> = Arc<CompactionScheduler<S>>;
pub type CompactionSchedulerChannelRef = Arc<dyn CompactionSchedulerChannel>;

pub trait CompactionSchedulerChannel: Send + Sync {
    fn try_sched_compaction(
        &self,
        compaction_group: CompactionGroupId,
    ) -> Result<bool, SendError<CompactionGroupId>>;

    fn unschedule(&self, compaction_group: CompactionGroupId);
}

/// [`CompactionRequestChannel`] wrappers a mpsc channel and deduplicate requests from same
/// compaction groups.
pub struct DefaultCompactionSchedulerChannel {
    tx: UnboundedSender<CompactionGroupId>,
    scheduled: Mutex<HashSet<CompactionGroupId>>,
}

/// A mock channel just swallow all compaction schedule requests
#[allow(dead_code)]
pub struct MockCompactionSchedulerChannel {
    tx: UnboundedSender<CompactionGroupId>,
}

#[derive(Debug, PartialEq)]
pub enum ScheduleStatus {
    Ok,
    NoTask,
    PickFailure,
    NoAvailableCompactor(CompactTask),
    AssignFailure(CompactTask),
    SendFailure(CompactTask),
}

impl ScheduleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScheduleStatus::Ok => "Ok",
            ScheduleStatus::NoTask => "NoTask",
            ScheduleStatus::PickFailure => "PickFailure",
            ScheduleStatus::NoAvailableCompactor(_) => "NoAvailableCompactor",
            ScheduleStatus::AssignFailure(_) => "AssignFailure",
            ScheduleStatus::SendFailure(_) => "SendFailure",
        }
    }
}

impl DefaultCompactionSchedulerChannel {
    pub(crate) fn new(tx: UnboundedSender<CompactionGroupId>) -> Self {
        Self {
            tx,
            scheduled: Default::default(),
        }
    }
}

impl MockCompactionSchedulerChannel {
    pub fn new(tx: UnboundedSender<CompactionGroupId>) -> Self {
        MockCompactionSchedulerChannel { tx }
    }
}

impl CompactionSchedulerChannel for MockCompactionSchedulerChannel {
    fn try_sched_compaction(
        &self,
        _compaction_group: CompactionGroupId,
    ) -> Result<bool, SendError<CompactionGroupId>> {
        // do nothing
        Ok(true)
    }

    fn unschedule(&self, _compaction_group: CompactionGroupId) {
        // do nothing
    }
}

impl CompactionSchedulerChannel for DefaultCompactionSchedulerChannel {
    /// Enqueues only if the target group is not in the queue.
    fn try_sched_compaction(
        &self,
        compaction_group: CompactionGroupId,
    ) -> Result<bool, SendError<CompactionGroupId>> {
        let mut guard = self.scheduled.lock();
        if guard.contains(&compaction_group) {
            return Ok(false);
        }
        self.tx.send(compaction_group)?;
        guard.insert(compaction_group);
        Ok(true)
    }

    fn unschedule(&self, compaction_group: CompactionGroupId) {
        self.scheduled.lock().remove(&compaction_group);
    }
}

/// Schedules compaction task picking and assignment.
pub struct CompactionScheduler<S>
where
    S: MetaStore,
{
    env: MetaSrvEnv<S>,
    hummock_manager: HummockManagerRef<S>,
    compactor_manager: CompactorManagerRef,
}

impl<S> CompactionScheduler<S>
where
    S: MetaStore,
{
    pub fn new(
        env: MetaSrvEnv<S>,
        hummock_manager: HummockManagerRef<S>,
        compactor_manager: CompactorManagerRef,
    ) -> Self {
        Self {
            env,
            hummock_manager,
            compactor_manager,
        }
    }

    pub async fn start(&self, mut shutdown_rx: Receiver<()>, deterministic_mode: bool) {
        let (sched_channel, mut sched_rx, side_sched_channel, mut side_sched_rx) =
            if deterministic_mode {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
                let mock_channel: CompactionSchedulerChannelRef =
                    Arc::new(MockCompactionSchedulerChannel::new(tx));

                let (side_tx, side_rx) =
                    tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
                let side_channel: CompactionSchedulerChannelRef =
                    Arc::new(DefaultCompactionSchedulerChannel::new(side_tx));
                (mock_channel, rx, Some(side_channel), Some(side_rx))
            } else {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
                let default_channel: CompactionSchedulerChannelRef =
                    Arc::new(DefaultCompactionSchedulerChannel::new(tx));
                (default_channel, rx, None, None)
            };

        self.hummock_manager
            .init_compaction_scheduler(sched_channel.clone(), side_sched_channel.clone())
            .await;

        tracing::info!("Start compaction scheduler.");
        let mut min_trigger_interval = tokio::time::interval(Duration::from_secs(
            self.env.opts.periodic_compaction_interval_sec,
        ));
        min_trigger_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let compaction_group: CompactionGroupId = tokio::select! {
                compaction_group = sched_rx.recv() => {
                    match compaction_group {
                        Some(compaction_group) => compaction_group,
                        None => {
                            tracing::warn!("Compactor Scheduler: The Hummock manager has dropped the connection,
                                it means it has either died or started a new session. Exiting.");
                            break;
                        }
                    }
                },
                // FIXME: handle side_sched_rx is None
                res = side_sched_rx.as_mut().unwrap().recv(), if deterministic_mode => {
                    match res {
                        Some(compaction_group) => compaction_group,
                        None => {
                            break;
                        }
                    }
                },
                _ = min_trigger_interval.tick() => {
                    // Periodically trigger compaction for all compaction groups.
                    for cg_id in self.hummock_manager.compaction_group_manager().compaction_group_ids().await {
                        if let Err(e) = sched_channel.try_sched_compaction(cg_id) {
                            tracing::warn!("Failed to schedule compaction for compaction group {}. {}", cg_id, e);
                        }
                    }
                    continue;
                },
                // Shutdown compactor scheduler
                _ = &mut shutdown_rx => {
                    break;
                }
            };
            sync_point::sync_point!("BEFORE_SCHEDULE_COMPACTION_TASK");
            let status = self
                .pick_and_assign(
                    compaction_group,
                    sched_channel.clone(),
                    side_sched_channel.clone(),
                )
                .await;
            if let ScheduleStatus::NoAvailableCompactor(_) = status {
                tokio::time::sleep(Duration::from_secs(
                    self.env.opts.no_available_compactor_stall_sec,
                ))
                .await;
            }
        }
        tracing::info!("Compaction scheduler is stopped");
    }

    /// Tries to pick a compaction task, schedule it to a compactor.
    ///
    /// Returns true if a task is successfully picked and sent.
    async fn pick_and_assign(
        &self,
        compaction_group: CompactionGroupId,
        sched_channel: CompactionSchedulerChannelRef,
        side_sched_channel: Option<CompactionSchedulerChannelRef>,
    ) -> ScheduleStatus {
        let schedule_status = self
            .pick_and_assign_impl(compaction_group, sched_channel.clone())
            .await;

        Self::unschedule(sched_channel, &side_sched_channel, compaction_group);
        let cancel_state = match &schedule_status {
            ScheduleStatus::Ok => None,
            ScheduleStatus::NoTask | ScheduleStatus::PickFailure => None,
            ScheduleStatus::NoAvailableCompactor(task) => {
                Some((task.clone(), TaskStatus::NoAvailCanceled))
            }
            ScheduleStatus::AssignFailure(task) => {
                Some((task.clone(), TaskStatus::AssignFailCanceled))
            }
            ScheduleStatus::SendFailure(task) => Some((task.clone(), TaskStatus::SendFailCanceled)),
        };

        if let Some((mut compact_task, task_state)) = cancel_state {
            // Try to cancel task immediately.
            if let Err(err) = self
                .hummock_manager
                .cancel_compact_task(&mut compact_task, task_state)
                .await
            {
                // Cancel task asynchronously.
                tracing::warn!(
                    "Failed to cancel task {}. {}. {:?} It will be cancelled asynchronously.",
                    compact_task.task_id,
                    err,
                    task_state
                );
                self.env
                    .notification_manager()
                    .notify_local_subscribers(LocalNotification::CompactionTaskNeedCancel(
                        compact_task,
                    ))
                    .await;
            }
        }
        schedule_status
    }

    async fn pick_and_assign_impl(
        &self,
        compaction_group: CompactionGroupId,
        sched_channel: CompactionSchedulerChannelRef,
    ) -> ScheduleStatus {
        // 1. Pick a compaction task.
        let compact_task = self
            .hummock_manager
            .get_compact_task(compaction_group)
            .await;
        let compact_task = match compact_task {
            Ok(Some(compact_task)) => compact_task,
            Ok(None) => {
                return ScheduleStatus::NoTask;
            }
            Err(err) => {
                tracing::warn!("Failed to get compaction task: {:#?}.", err);
                return ScheduleStatus::PickFailure;
            }
        };
        tracing::trace!(
            "Picked compaction task. {}",
            compact_task_to_string(&compact_task)
        );

        // 2. Assign the compaction task to a compactor.
        let compactor = match self
            .hummock_manager
            .assign_compaction_task(&compact_task)
            .await
        {
            Ok(compactor) => {
                tracing::trace!(
                    "Assigned compaction task. {}",
                    compact_task_to_string(&compact_task)
                );
                compactor
            }
            Err(err) => {
                tracing::warn!("Failed to assign compaction task to compactor: {:#?}", err);
                match err {
                    Error::NoIdleCompactor => {
                        let current_compactor_tasks =
                            self.hummock_manager.list_assigned_tasks_number().await;
                        tracing::warn!("The assigned task number for every compactor is (context_id, count):\n {:?}", current_compactor_tasks);
                        return ScheduleStatus::NoAvailableCompactor(compact_task);
                    }
                    Error::CompactionTaskAlreadyAssigned(_, _) => {
                        panic!("Compaction scheduler is the only tokio task that can assign task.");
                    }
                    Error::InvalidContext(context_id) => {
                        self.compactor_manager.remove_compactor(context_id);
                        return ScheduleStatus::AssignFailure(compact_task);
                    }
                    _ => {
                        return ScheduleStatus::AssignFailure(compact_task);
                    }
                }
            }
        };

        // 3. Send the compaction task.
        if let Err(e) = compactor
            .send_task(Task::CompactTask(compact_task.clone()))
            .await
        {
            tracing::warn!(
                "Failed to send task {} to {}. {:#?}",
                compact_task.task_id,
                compactor.context_id(),
                e
            );
            self.compactor_manager
                .pause_compactor(compactor.context_id());
            return ScheduleStatus::SendFailure(compact_task);
        }

        // Bypass reschedule if we want compaction scheduling in a deterministic way
        if self.env.opts.enable_compaction_deterministic {
            return ScheduleStatus::Ok;
        }

        // 4. Reschedule it with best effort, in case there are more tasks.
        if let Err(e) = sched_channel.try_sched_compaction(compaction_group) {
            tracing::error!(
                "Failed to reschedule compaction group {} after sending new task {}. {:#?}",
                compaction_group,
                compact_task.task_id,
                e
            );
        }
        ScheduleStatus::Ok
    }

    fn unschedule(
        sched_channel: CompactionSchedulerChannelRef,
        side_sched_channel: &Option<CompactionSchedulerChannelRef>,
        compaction_group: CompactionGroupId,
    ) {
        sched_channel.unschedule(compaction_group);
        if let Some(channel) = side_sched_channel {
            channel.unschedule(compaction_group);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use risingwave_hummock_sdk::compaction_group::StaticCompactionGroupId;
    use risingwave_hummock_sdk::CompactionGroupId;

    use crate::hummock::compaction_scheduler::{CompactionRequestChannel, ScheduleStatus};
    use crate::hummock::test_utils::{add_ssts, setup_compute_env};
    use crate::hummock::CompactionScheduler;

    #[tokio::test]
    async fn test_pick_and_assign() {
        let (env, hummock_manager, _cluster_manager, worker_node) = setup_compute_env(80).await;
        let context_id = worker_node.id;
        let compactor_manager = hummock_manager.compactor_manager_ref_for_test();
        let compaction_scheduler =
            CompactionScheduler::new(env, hummock_manager.clone(), compactor_manager.clone());

        let (request_tx, _request_rx) = tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
        let request_channel = Arc::new(DefaultCompactionSchedulerChannel::new(request_tx));

        // No task
        assert_eq!(
            ScheduleStatus::NoTask,
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone(),
                    None
                )
                .await
        );
        let _sst_infos = add_ssts(1, hummock_manager.as_ref(), context_id).await;

        // No compactor
        assert_eq!(compactor_manager.compactor_num(), 0);
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone()
                )
                .await,
            ScheduleStatus::NoAvailableCompactor(_)
        );

        // Add a compactor with invalid context_id.
        let _receiver = compactor_manager.add_compactor(1234, 1);
        assert_eq!(compactor_manager.compactor_num(), 1);
        // Cannot assign because of invalid compactor
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone()
                )
                .await,
            ScheduleStatus::AssignFailure(_)
        );
        assert_eq!(compactor_manager.compactor_num(), 0);

        // Add a valid compactor and succeed
        let _receiver = compactor_manager.add_compactor(context_id, 1);
        assert_eq!(compactor_manager.compactor_num(), 1);
        assert_eq!(
            ScheduleStatus::Ok,
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone(),
                    None
                )
                .await
        );

        // Add more SSTs for compaction.
        let _sst_infos = add_ssts(2, hummock_manager.as_ref(), context_id).await;

        // No idle compactor
        assert_eq!(
            hummock_manager.get_assigned_tasks_number(context_id).await,
            1
        );
        assert_eq!(compactor_manager.compactor_num(), 1);
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone()
                )
                .await,
            ScheduleStatus::NoAvailableCompactor(_)
        );

        // Increase compactor concurrency and succeed
        let _receiver = compactor_manager.add_compactor(context_id, 10);
        assert_eq!(
            hummock_manager.get_assigned_tasks_number(context_id).await,
            1
        );
        assert_eq!(
            ScheduleStatus::Ok,
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone(),
                    None
                )
                .await
        );
        assert_eq!(
            hummock_manager.get_assigned_tasks_number(context_id).await,
            2
        );
    }

    #[tokio::test]
    #[cfg(all(test, feature = "failpoints"))]
    async fn test_failpoints() {
        use risingwave_pb::hummock::compact_task::TaskStatus;

        use crate::manager::LocalNotification;

        let (env, hummock_manager, _cluster_manager, worker_node) = setup_compute_env(80).await;
        let context_id = worker_node.id;
        let compactor_manager = hummock_manager.compactor_manager_ref_for_test();
        let compaction_scheduler = CompactionScheduler::new(
            env.clone(),
            hummock_manager.clone(),
            compactor_manager.clone(),
        );

        let (request_tx, _request_rx) = tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
        let request_channel = Arc::new(DefaultCompactionSchedulerChannel::new(request_tx));

        let _sst_infos = add_ssts(1, hummock_manager.as_ref(), context_id).await;
        let _receiver = compactor_manager.add_compactor(context_id, 1);

        // Pick failure
        let fp_get_compact_task = "fp_get_compact_task";
        fail::cfg(fp_get_compact_task, "return").unwrap();
        assert_eq!(
            ScheduleStatus::PickFailure,
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone(),
                    None
                )
                .await
        );
        fail::remove(fp_get_compact_task);

        // Assign failed and task cancelled.
        let fp_assign_compaction_task_fail = "assign_compaction_task_fail";
        fail::cfg(fp_assign_compaction_task_fail, "return").unwrap();
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(StaticCompactionGroupId::StateDefault.into())
                .await,
            ScheduleStatus::AssignFailure(_)
        );
        fail::remove(fp_assign_compaction_task_fail);
        assert!(hummock_manager.list_all_tasks_ids().await.is_empty());

        // Send failed and task cancelled.
        let fp_compaction_send_task_fail = "compaction_send_task_fail";
        fail::cfg(fp_compaction_send_task_fail, "return").unwrap();
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(StaticCompactionGroupId::StateDefault.into())
                .await,
            ScheduleStatus::SendFailure(_)
        );
        fail::remove(fp_compaction_send_task_fail);
        assert!(hummock_manager.list_all_tasks_ids().await.is_empty());

        // Fail, because the compactor is paused after send failure.
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone()
                )
                .await,
            ScheduleStatus::NoAvailableCompactor(_)
        );
        assert!(hummock_manager.list_all_tasks_ids().await.is_empty());
        let _receiver = compactor_manager.add_compactor(context_id, 1);

        // Assign failed and task cancellation failed.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        env.notification_manager().insert_local_sender(tx).await;
        let fp_cancel_compact_task = "fp_cancel_compact_task";
        fail::cfg(fp_assign_compaction_task_fail, "return").unwrap();
        fail::cfg(fp_cancel_compact_task, "return").unwrap();
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(
                    StaticCompactionGroupId::StateDefault.into(),
                    request_channel.clone()
                )
                .await,
            ScheduleStatus::AssignFailure(_)
        );
        fail::remove(fp_assign_compaction_task_fail);
        fail::remove(fp_cancel_compact_task);
        assert_eq!(hummock_manager.list_all_tasks_ids().await.len(), 1);
        // Notified to retry cancellation.
        let mut task_to_cancel = match rx.recv().await.unwrap() {
            LocalNotification::WorkerNodeIsDeleted(_) => {
                panic!()
            }
            LocalNotification::CompactionTaskNeedCancel(task_to_cancel) => task_to_cancel,
        };
        hummock_manager
            .cancel_compact_task(&mut task_to_cancel, TaskStatus::ManualCanceled)
            .await
            .unwrap();
        assert!(hummock_manager.list_all_tasks_ids().await.is_empty());

        // Succeeded.
        assert_matches!(
            compaction_scheduler
                .pick_and_assign(StaticCompactionGroupId::StateDefault.into())
                .await,
            ScheduleStatus::Ok
        );
        assert_eq!(hummock_manager.list_all_tasks_ids().await.len(), 1);
    }
}
