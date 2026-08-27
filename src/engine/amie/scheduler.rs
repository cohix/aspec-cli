//! The amie tick loop (Layer 1).
//!
//! [`AmieScheduler`] wakes on a fixed 30-second cadence — independent of any
//! condition's own interval — re-reads global config, asks the store which
//! conditions are due (a decision taken *wholly in SQL*, never re-derived
//! here), and dispatches each onto a bounded task set. For every due condition
//! it opens an `amie_runs` row, delegates the actual evaluation to a
//! [`ConditionEvaluator`], and records the terminal status. Repeatedly-failing
//! conditions grow an exponential backoff so a persistent auth or rate-limit
//! error does not re-fire every tick.
//!
//! The scheduler shares no code with the API server's `QueueWorker`: their work
//! models differ (claim-next-queued vs. select-by-elapsed-interval) and the
//! honest shared surface is ~20 lines of poll loop, duplicated deliberately.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::data::config::{EnvSnapshot, GlobalConfig};
use crate::data::fs::{AmiePaths, Condition, ConditionStore, RunDetail, RunId, RunStatus};

use super::evaluator::{ConditionEvaluator, EvaluationOutcome, EvaluationRequest, RunProgress};

/// Records the live workflow-state path on the run row the moment the generated
/// workflow starts. The store stays the scheduler's to write.
struct StoreRunProgress {
    store: Arc<ConditionStore>,
}

impl RunProgress for StoreRunProgress {
    fn workflow_started(
        &self,
        run_id: &RunId,
        workflow_path: &std::path::Path,
        state_path: &std::path::Path,
    ) {
        if let Err(error) =
            self.store
                .set_workflow_state_path(run_id, Some(workflow_path), Some(state_path))
        {
            tracing::warn!("amie: failed to record workflow state path: {error}");
        }
    }
}

/// The scheduler's wake cadence. Independent of any condition's interval: a
/// condition with a 5-minute interval is *considered* every 30s and *selected*
/// once its interval has elapsed (the interval check lives in the SQL).
pub const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// The exponential-backoff ceiling for a repeatedly-failing condition.
const MAX_BACKOFF_SECS: u64 = 6 * 60 * 60; // 6 hours

/// A snapshot of the scheduler's liveness, read by `GET /v1/status`.
#[derive(Debug, Clone, Default)]
pub struct SchedulerStatus {
    /// When the last tick ran.
    pub last_tick: Option<DateTime<Utc>>,
    /// How many ticks have run since the scheduler started.
    pub tick_count: u64,
    /// How many evaluations are executing right now.
    pub in_flight: usize,
}

/// The always-on condition scheduler.
pub struct AmieScheduler {
    store: Arc<ConditionStore>,
    paths: AmiePaths,
    evaluator: Arc<dyn ConditionEvaluator>,
    env: EnvSnapshot,
    status: Arc<Mutex<SchedulerStatus>>,
    /// Consecutive-failure counts per condition id, driving backoff growth.
    /// In-memory only: a daemon restart resets the count to zero (the
    /// conservative choice — restart is a fresh chance), while the last
    /// `backoff_until` persists in SQLite and keeps the condition parked until
    /// it elapses.
    failure_counts: Arc<Mutex<HashMap<String, u32>>>,
    /// The wake cadence. Always [`TICK_INTERVAL`] in production; tests shorten
    /// it so multi-tick behaviour (the concurrency bound, live config re-read)
    /// is observable without a 30-second wait.
    tick_interval: Duration,
}

impl AmieScheduler {
    pub fn new(
        store: Arc<ConditionStore>,
        paths: AmiePaths,
        evaluator: Arc<dyn ConditionEvaluator>,
        env: EnvSnapshot,
    ) -> Self {
        Self {
            store,
            paths,
            evaluator,
            env,
            status: Arc::new(Mutex::new(SchedulerStatus::default())),
            failure_counts: Arc::new(Mutex::new(HashMap::new())),
            tick_interval: TICK_INTERVAL,
        }
    }

    /// Override the wake cadence. Production never calls this; it exists so a
    /// test can observe more than one tick of a genuinely running scheduler.
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// A shared handle to the scheduler's status, cloned before `run` consumes
    /// the scheduler so the daemon's HTTP layer can read liveness.
    pub fn status_handle(&self) -> Arc<Mutex<SchedulerStatus>> {
        Arc::clone(&self.status)
    }

    /// Run the tick loop until `shutdown` is cancelled, then drain in-flight
    /// evaluations before returning.
    pub async fn run(self, shutdown: CancellationToken) {
        let mut tasks: JoinSet<()> = JoinSet::new();
        loop {
            // Reap any finished evaluations so the set does not grow unbounded.
            while tasks.try_join_next().is_some() {}

            self.tick(&mut tasks);

            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(self.tick_interval) => {}
            }
        }

        // Stop accepting new work; let the in-flight evaluations finish.
        while tasks.join_next().await.is_some() {}
    }

    /// One tick: re-read config, select due conditions, dispatch each.
    fn tick(&self, tasks: &mut JoinSet<()>) {
        // Re-read config every tick — never cached at startup — so guidance /
        // agentsToModels / defaultLeader / maxConcurrentEvaluations edits take
        // effect on the next tick with no restart. A malformed config that
        // fails to load falls back to defaults rather than stalling the loop.
        let cfg = GlobalConfig::load_with(&self.env).unwrap_or_default();
        let amie = cfg.amie.unwrap_or_default();
        let max_concurrent = amie.max_concurrent_evaluations_or_default().max(1);
        let guidance = amie.guidance;
        let agents_to_models = amie.agents_to_models;
        let default_leader = amie.default_leader;

        let now = Utc::now();
        {
            let mut status = self.status.lock().expect("scheduler status poisoned");
            status.last_tick = Some(now);
            status.tick_count += 1;
        }

        // The whole admission predicate — active, off-backoff, interval
        // elapsed, and not already running — is enforced in SQL. Never
        // re-derive it here.
        let due = match self.store.due_for_evaluation(now) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!("amie: due_for_evaluation failed: {error}");
                return;
            }
        };
        if due.is_empty() {
            return;
        }

        // `maxConcurrentEvaluations` bounds the scheduler's *whole* in-flight
        // set, not one tick's fan-out: capacity is what the cap leaves after
        // evaluations still running from earlier ticks. Re-reading the cap each
        // tick is what makes a config edit take effect without a restart.
        //
        // This is a dispatch bound, not a second admission predicate — the
        // four admission rules stay wholly in `due_for_evaluation`'s SQL.
        let capacity = max_concurrent.saturating_sub(self.in_flight());
        if capacity == 0 {
            tracing::debug!(
                max_concurrent,
                due = due.len(),
                "amie: at the concurrency cap; deferring due conditions to a later tick"
            );
            return;
        }
        let deferred = due.len().saturating_sub(capacity);
        if deferred > 0 {
            tracing::debug!(
                deferred,
                capacity,
                "amie: dispatching up to the concurrency cap this tick"
            );
        }

        for condition in due.into_iter().take(capacity) {
            // The run row is opened *before* the task is spawned, so the
            // condition is excluded from the very next tick's SQL selection by
            // its own `running` row. Opening it inside the task would let a
            // queued evaluation be selected a second time.
            let started_at = Utc::now();
            let condition_dir = match self.paths.condition_dir(&condition.name) {
                Ok(dir) => dir,
                Err(error) => {
                    tracing::warn!(
                        "amie: cannot resolve condition dir for {:?}: {error}",
                        condition.name
                    );
                    continue;
                }
            };
            let run_id = match self.store.start_run(&condition.id, None, started_at) {
                Ok(run_id) => run_id,
                Err(error) => {
                    tracing::warn!(
                        "amie: failed to open run row for {:?}: {error}",
                        condition.name
                    );
                    continue;
                }
            };
            adjust_in_flight(&self.status, 1);

            let store = Arc::clone(&self.store);
            let evaluator = Arc::clone(&self.evaluator);
            let status = Arc::clone(&self.status);
            let failures = Arc::clone(&self.failure_counts);
            let guidance = guidance.clone();
            let agents_to_models = agents_to_models.clone();
            let default_leader = default_leader.clone();
            tasks.spawn(async move {
                evaluate_condition(EvaluateArgs {
                    store,
                    evaluator,
                    status,
                    failures,
                    condition,
                    condition_dir,
                    run_id,
                    guidance,
                    agents_to_models,
                    default_leader,
                })
                .await;
            });
        }
    }

    /// How many evaluations are executing right now.
    fn in_flight(&self) -> usize {
        self.status
            .lock()
            .expect("scheduler status poisoned")
            .in_flight
    }
}

/// Bundled arguments for one condition's evaluation task.
struct EvaluateArgs {
    store: Arc<ConditionStore>,
    evaluator: Arc<dyn ConditionEvaluator>,
    status: Arc<Mutex<SchedulerStatus>>,
    failures: Arc<Mutex<HashMap<String, u32>>>,
    condition: Condition,
    condition_dir: std::path::PathBuf,
    /// The `running` row the tick already committed for this evaluation.
    run_id: RunId,
    guidance: Option<Vec<String>>,
    agents_to_models: Option<HashMap<String, Vec<String>>>,
    default_leader: Option<String>,
}

/// Open a run row, delegate evaluation, record the terminal status, and adjust
/// backoff. Every store failure is logged and swallowed — one condition's bad
/// tick must never take down the daemon.
async fn evaluate_condition(args: EvaluateArgs) {
    let EvaluateArgs {
        store,
        evaluator,
        status,
        failures,
        condition,
        condition_dir,
        run_id,
        guidance,
        agents_to_models,
        default_leader,
    } = args;

    // The tick already incremented `in_flight` and opened the run row; this
    // guard restores the counter even if the evaluation panics or returns early.
    let _guard = InFlightGuard {
        status: Arc::clone(&status),
    };

    let outcome = evaluator
        .evaluate(EvaluationRequest {
            condition: condition.clone(),
            run_id: run_id.clone(),
            condition_dir,
            guidance,
            agents_to_models,
            default_leader,
            progress: Arc::new(StoreRunProgress {
                store: Arc::clone(&store),
            }),
        })
        .await;

    let (run_status, detail, failed) = classify(&outcome);
    let finished_at = Utc::now();
    if let Err(error) = store.finish_run(&run_id, run_status, &detail, finished_at) {
        tracing::warn!(
            "amie: failed to record run outcome for {:?}: {error}",
            condition.name
        );
    }

    if failed {
        let attempt = {
            let mut counts = failures.lock().expect("failure counts poisoned");
            let entry = counts.entry(condition.id.clone()).or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        let until = finished_at
            + chrono::Duration::seconds(backoff_secs(condition.interval_secs, attempt) as i64);
        if let Err(error) = store.set_backoff(&condition.name, Some(until)) {
            tracing::warn!(
                "amie: failed to set backoff for {:?}: {error}",
                condition.name
            );
        }
    } else {
        // A non-failing terminal status resets the streak and clears any
        // backoff the condition was carrying.
        failures
            .lock()
            .expect("failure counts poisoned")
            .remove(&condition.id);
        if condition.backoff_until.is_some() {
            if let Err(error) = store.set_backoff(&condition.name, None) {
                tracing::warn!(
                    "amie: failed to clear backoff for {:?}: {error}",
                    condition.name
                );
            }
        }
    }
}

/// Map an [`EvaluationOutcome`] onto the persisted run status, its detail row,
/// and whether it counts as a failure for backoff purposes.
fn classify(outcome: &EvaluationOutcome) -> (RunStatus, RunDetail, bool) {
    match outcome {
        EvaluationOutcome::NotTriggered => (RunStatus::NotTriggered, RunDetail::default(), false),
        EvaluationOutcome::WorkflowExecuted {
            workflow_path,
            workflow_state_path,
            ..
        } => (
            RunStatus::WorkflowExecuted,
            RunDetail {
                workflow_path: Some(workflow_path.clone()),
                workflow_state_path: workflow_state_path.clone(),
                error: None,
            },
            false,
        ),
        EvaluationOutcome::Failed { error } => (
            RunStatus::Failed,
            RunDetail {
                workflow_path: None,
                workflow_state_path: None,
                error: Some(error.clone()),
            },
            true,
        ),
    }
}

/// `now + min(interval * 2^attempt, 6h)`, saturating so a large attempt count
/// can never overflow.
fn backoff_secs(interval_secs: u64, attempt: u32) -> u64 {
    let factor = 2u64.checked_pow(attempt).unwrap_or(u64::MAX);
    interval_secs.saturating_mul(factor).min(MAX_BACKOFF_SECS)
}

fn adjust_in_flight(status: &Arc<Mutex<SchedulerStatus>>, delta: isize) {
    let mut status = status.lock().expect("scheduler status poisoned");
    if delta >= 0 {
        status.in_flight = status.in_flight.saturating_add(delta as usize);
    } else {
        status.in_flight = status.in_flight.saturating_sub((-delta) as usize);
    }
}

/// Restores `in_flight` on drop, so a panic or early return in an evaluation
/// task never leaks the counter.
struct InFlightGuard {
    status: Arc<Mutex<SchedulerStatus>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        adjust_in_flight(&self.status, -1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        // interval 60s: 120, 240, 480, … up to the 6h ceiling.
        assert_eq!(backoff_secs(60, 1), 120);
        assert_eq!(backoff_secs(60, 2), 240);
        assert_eq!(backoff_secs(60, 3), 480);
        assert_eq!(backoff_secs(60, 100), MAX_BACKOFF_SECS);
    }

    #[test]
    fn backoff_never_overflows() {
        assert_eq!(backoff_secs(u64::MAX, 63), MAX_BACKOFF_SECS);
        assert_eq!(backoff_secs(86_400, u32::MAX), MAX_BACKOFF_SECS);
    }

    #[test]
    fn classify_maps_each_outcome() {
        let (status, detail, failed) = classify(&EvaluationOutcome::NotTriggered);
        assert_eq!(status, RunStatus::NotTriggered);
        assert!(!failed);
        assert!(detail.error.is_none());

        let (status, detail, failed) = classify(&EvaluationOutcome::WorkflowExecuted {
            workflow_path: "/c/workflow.toml".into(),
            workflow_state_path: Some("/state.json".into()),
            exit_code: Some(0),
        });
        assert_eq!(status, RunStatus::WorkflowExecuted);
        assert!(!failed);
        assert_eq!(
            detail.workflow_path.as_deref(),
            Some("/c/workflow.toml".as_ref())
        );
        assert_eq!(
            detail.workflow_state_path.as_deref(),
            Some("/state.json".as_ref())
        );

        let (status, detail, failed) = classify(&EvaluationOutcome::Failed {
            error: "boom".to_string(),
        });
        assert_eq!(status, RunStatus::Failed);
        assert!(failed);
        assert_eq!(detail.error.as_deref(), Some("boom"));
    }
}
