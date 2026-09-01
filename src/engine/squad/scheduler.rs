//! The squad tick loop (Layer 1).
//!
//! [`SquadScheduler`] wakes on a fixed 30-second cadence — independent of any
//! task's own interval — re-reads global config, asks the store which
//! tasks are due (a decision taken *wholly in SQL*, never re-derived
//! here), and dispatches each onto a bounded task set. For every due task
//! it opens an `squad_runs` row, delegates the actual evaluation to a
//! [`TaskEvaluator`], and records the terminal status. Repeatedly-failing
//! tasks grow an exponential backoff so a persistent auth or rate-limit
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
use crate::data::fs::{RunDetail, RunId, RunStatus, SquadPaths, Task, TaskStore};
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::container::naming::parse_squad_task_slug;
use crate::engine::squad::launcher::prepare_run_log_dir;

use super::evaluator::{EvaluationOutcome, EvaluationRequest, RunProgress, TaskEvaluator};

/// Records the live workflow-state path on the run row the moment the generated
/// workflow starts. The store stays the scheduler's to write.
struct StoreRunProgress {
    store: Arc<TaskStore>,
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
            tracing::warn!("squad: failed to record workflow state path: {error}");
        }
    }
}

/// The scheduler's wake cadence. Independent of any task's interval: a
/// task with a 5-minute interval is *considered* every 30s and *selected*
/// once its interval has elapsed (the interval check lives in the SQL).
pub const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// The exponential-backoff ceiling for a repeatedly-failing task.
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

/// The always-on task scheduler.
pub struct SquadScheduler {
    store: Arc<TaskStore>,
    paths: SquadPaths,
    evaluator: Arc<dyn TaskEvaluator>,
    env: EnvSnapshot,
    status: Arc<Mutex<SchedulerStatus>>,
    /// Consecutive-failure counts per task id, driving backoff growth.
    /// In-memory only: a daemon restart resets the count to zero (the
    /// conservative choice — restart is a fresh chance), while the last
    /// `backoff_until` persists in SQLite and keeps the task parked until
    /// it elapses.
    failure_counts: Arc<Mutex<HashMap<String, u32>>>,
    /// The production daemon's runtime, used only to report currently-running
    /// containers on the scheduler tick. It is optional so deterministic
    /// scheduler unit tests do not need a container runtime fixture.
    runtime: Option<Arc<dyn AgentRuntimeEngine>>,
    /// The wake cadence. Always [`TICK_INTERVAL`] in production; tests shorten
    /// it so multi-tick behaviour (the concurrency bound, live config re-read)
    /// is observable without a 30-second wait.
    tick_interval: Duration,
}

impl SquadScheduler {
    pub fn new(
        store: Arc<TaskStore>,
        paths: SquadPaths,
        evaluator: Arc<dyn TaskEvaluator>,
        env: EnvSnapshot,
    ) -> Self {
        Self {
            store,
            paths,
            evaluator,
            env,
            status: Arc::new(Mutex::new(SchedulerStatus::default())),
            failure_counts: Arc::new(Mutex::new(HashMap::new())),
            runtime: None,
            tick_interval: TICK_INTERVAL,
        }
    }

    /// Override the wake cadence. Production never calls this; it exists so a
    /// test can observe more than one tick of a genuinely running scheduler.
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Enable the production-only running-container summary. This remains a
    /// tick-side observation, not another polling loop.
    pub fn with_runtime(mut self, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        self.runtime = Some(runtime);
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

    /// One tick: re-read config, select due tasks, dispatch each.
    fn tick(&self, tasks: &mut JoinSet<()>) {
        // Re-read config every tick — never cached at startup — so guidance /
        // agentsToModels / defaultLeader / maxConcurrentEvaluations edits take
        // effect on the next tick with no restart. A malformed config that
        // fails to load falls back to defaults rather than stalling the loop.
        let cfg = GlobalConfig::load_with(&self.env).unwrap_or_default();
        let squad = cfg.squad.unwrap_or_default();
        let max_concurrent = squad.max_concurrent_evaluations_or_default().max(1);
        let guidance = squad.guidance;
        let agents_to_models = squad.agents_to_models;
        let default_leader = squad.default_leader;

        self.log_running_agents(default_leader.as_deref(), agents_to_models.as_ref());

        let now = Utc::now();
        let tick_count;
        {
            let mut status = self.status.lock().expect("scheduler status poisoned");
            status.last_tick = Some(now);
            status.tick_count += 1;
            tick_count = status.tick_count;
        }
        tracing::info!(tick = tick_count, at = %now, "squad scheduler tick");

        // The whole admission predicate — active, off-backoff, interval
        // elapsed, and not already running — is enforced in SQL. Never
        // re-derive it here.
        let due = match self.store.due_for_evaluation(now) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!("squad: due_for_evaluation failed: {error}");
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
            tracing::info!(
                max_concurrent,
                due = due.len(),
                "squad: at the concurrency cap; deferring due tasks to a later tick"
            );
            return;
        }
        let deferred = due.len().saturating_sub(capacity);
        if deferred > 0 {
            tracing::info!(
                deferred,
                capacity,
                "squad: dispatching up to the concurrency cap this tick"
            );
        }

        for task in due.into_iter().take(capacity) {
            // The run row is opened *before* the task is spawned, so the
            // task is excluded from the very next tick's SQL selection by
            // its own `running` row. Opening it inside the task would let a
            // queued evaluation be selected a second time.
            let started_at = Utc::now();
            let task_dir = match self.paths.task_dir(&task.name) {
                Ok(dir) => dir,
                Err(error) => {
                    tracing::warn!(
                        "squad: cannot resolve task dir for {:?}: {error}",
                        task.name
                    );
                    continue;
                }
            };
            let run_id = match self.store.start_run(&task.id, None, started_at) {
                Ok(run_id) => run_id,
                Err(error) => {
                    tracing::warn!("squad: failed to open run row for {:?}: {error}", task.name);
                    continue;
                }
            };
            let run_log_dir = match prepare_run_log_dir(&task_dir, &run_id) {
                Ok(dir) => dir,
                Err(error) => {
                    let detail = RunDetail {
                        error: Some(format!("preparing per-container log directory: {error}")),
                        ..Default::default()
                    };
                    let _ = self
                        .store
                        .finish_run(&run_id, RunStatus::Failed, &detail, Utc::now());
                    tracing::warn!(
                        task = %task.name,
                        run_id = %run_id,
                        error = %error,
                        "squad: failed to prepare per-run log directory"
                    );
                    continue;
                }
            };
            tracing::info!(
                task = %task.name,
                run_id = %run_id,
                log_dir = %run_log_dir.display(),
                "squad task selected for evaluation"
            );
            adjust_in_flight(&self.status, 1);

            let store = Arc::clone(&self.store);
            let evaluator = Arc::clone(&self.evaluator);
            let status = Arc::clone(&self.status);
            let failures = Arc::clone(&self.failure_counts);
            let guidance = guidance.clone();
            let agents_to_models = agents_to_models.clone();
            let default_leader = default_leader.clone();
            tasks.spawn(async move {
                evaluate_task(EvaluateArgs {
                    store,
                    evaluator,
                    status,
                    failures,
                    task,
                    task_dir,
                    run_log_dir,
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

    /// Log one line per live squad container. An empty runtime snapshot emits
    /// nothing at all — including immediately after the last agent exits — so
    /// the daemon log stays a lifecycle record rather than a heartbeat.
    fn log_running_agents(
        &self,
        default_leader: Option<&str>,
        agents_to_models: Option<&HashMap<String, Vec<String>>>,
    ) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let handles = match runtime
            .list_running_with_name_prefix(crate::engine::container::naming::SQUAD_NAME_PREFIX)
        {
            Ok(handles) => handles,
            Err(error) => {
                tracing::warn!(error = %error, "squad: failed to list running containers");
                return;
            }
        };
        for summary in running_agent_summaries(
            &self.store,
            &handles,
            Utc::now(),
            default_leader,
            agents_to_models,
        ) {
            tracing::info!(
                task = %summary.task,
                container = %summary.container,
                agent = %summary.agent,
                model = ?summary.model,
                image = %summary.image,
                elapsed_secs = summary.elapsed_secs,
                "squad running agent"
            );
        }
    }
}

/// One line of the periodic running-agents summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAgentSummary {
    pub task: String,
    pub container: String,
    pub agent: String,
    pub model: Option<String>,
    pub image: String,
    pub elapsed_secs: i64,
}

/// Turn a live container snapshot into the summary lines the daemon log emits.
///
/// Separated from the logging so the "nothing running emits nothing / one entry
/// per running container" rule is testable without a container runtime: an
/// empty snapshot yields an empty vector, and the caller therefore logs
/// nothing at all rather than a heartbeat.
pub fn running_agent_summaries(
    store: &TaskStore,
    handles: &[crate::data::session::AgentHandle],
    now: DateTime<Utc>,
    default_leader: Option<&str>,
    agents_to_models: Option<&HashMap<String, Vec<String>>>,
) -> Vec<RunningAgentSummary> {
    let default_leader = default_leader.map(|value| {
        let (agent, model) = value
            .split_once("::")
            .map(|(agent, model)| (agent, Some(model)))
            .unwrap_or((value, None));
        (agent, model)
    });
    handles
        .iter()
        .map(|handle| {
            let task_name = parse_squad_task_slug(&handle.name).unwrap_or("unknown");
            let configured = store.get(task_name).ok().flatten();
            let agent = configured
                .as_ref()
                .and_then(|task| task.agent.as_deref())
                .or_else(|| default_leader.map(|(agent, _)| agent))
                .unwrap_or("configured-default");
            let model = configured
                .as_ref()
                .and_then(|task| task.model.as_deref())
                .or_else(|| default_leader.and_then(|(_, model)| model))
                .or_else(|| {
                    agents_to_models
                        .and_then(|models| models.get(agent))
                        .and_then(|models| models.first())
                        .map(String::as_str)
                });
            RunningAgentSummary {
                task: task_name.to_string(),
                container: handle.name.clone(),
                agent: agent.to_string(),
                model: model.map(str::to_string),
                image: handle.image_tag.clone(),
                elapsed_secs: now
                    .signed_duration_since(handle.started_at)
                    .num_seconds()
                    .max(0),
            }
        })
        .collect()
}

/// Bundled arguments for one task's evaluation task.
struct EvaluateArgs {
    store: Arc<TaskStore>,
    evaluator: Arc<dyn TaskEvaluator>,
    status: Arc<Mutex<SchedulerStatus>>,
    failures: Arc<Mutex<HashMap<String, u32>>>,
    task: Task,
    task_dir: std::path::PathBuf,
    run_log_dir: std::path::PathBuf,
    /// The `running` row the tick already committed for this evaluation.
    run_id: RunId,
    guidance: Option<Vec<String>>,
    agents_to_models: Option<HashMap<String, Vec<String>>>,
    default_leader: Option<String>,
}

/// Open a run row, delegate evaluation, record the terminal status, and adjust
/// backoff. Every store failure is logged and swallowed — one task's bad
/// tick must never take down the daemon.
async fn evaluate_task(args: EvaluateArgs) {
    let EvaluateArgs {
        store,
        evaluator,
        status,
        failures,
        task,
        task_dir,
        run_log_dir,
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
            task: task.clone(),
            run_id: run_id.clone(),
            task_dir,
            run_log_dir,
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
    tracing::info!(
        task = %task.name,
        run_id = %run_id,
        status = ?run_status,
        outcome = ?outcome_name(&outcome),
        "squad task run finished"
    );
    if let Err(error) = store.finish_run(&run_id, run_status, &detail, finished_at) {
        tracing::warn!(
            "squad: failed to record run outcome for {:?}: {error}",
            task.name
        );
    }

    if failed {
        let attempt = {
            let mut counts = failures.lock().expect("failure counts poisoned");
            let entry = counts.entry(task.id.clone()).or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        let until = finished_at
            + chrono::Duration::seconds(backoff_secs(task.interval_secs, attempt) as i64);
        if let Err(error) = store.set_backoff(&task.name, Some(until)) {
            tracing::warn!("squad: failed to set backoff for {:?}: {error}", task.name);
        }
        tracing::info!(
            task = %task.name,
            run_id = %run_id,
            attempt,
            backoff_until = %until,
            "squad task backed off after failed run"
        );
    } else {
        // A non-failing terminal status resets the streak and clears any
        // backoff the task was carrying.
        failures
            .lock()
            .expect("failure counts poisoned")
            .remove(&task.id);
        if task.backoff_until.is_some() {
            if let Err(error) = store.set_backoff(&task.name, None) {
                tracing::warn!(
                    "squad: failed to clear backoff for {:?}: {error}",
                    task.name
                );
            }
        }
    }
}

fn outcome_name(outcome: &EvaluationOutcome) -> &'static str {
    match outcome {
        EvaluationOutcome::NotTriggered => "not_triggered",
        EvaluationOutcome::WorkflowExecuted { .. } => "workflow_executed",
        EvaluationOutcome::Failed { .. } => "failed",
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

    // ── The periodic running-agents summary (WI 0106 §3b) ─────────────────

    fn summary_store(tmp: &std::path::Path) -> TaskStore {
        let db = crate::data::fs::DataPaths::at_root(tmp.join("data")).db_path();
        let store = TaskStore::open(&db).unwrap();
        store.migrate().unwrap();
        store
    }

    fn handle(
        name: &str,
        image: &str,
        started_at: DateTime<Utc>,
    ) -> crate::data::session::AgentHandle {
        crate::data::session::AgentHandle {
            id: format!("id-{name}"),
            image_tag: image.to_string(),
            name: name.to_string(),
            started_at,
        }
    }

    /// Nothing running means nothing logged — including on the tick right
    /// after the last container exits, since the snapshot is queried live and
    /// an empty snapshot produces no summary lines at all.
    #[test]
    fn the_running_agents_summary_is_empty_when_nothing_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let store = summary_store(tmp.path());
        assert!(
            running_agent_summaries(&store, &[], Utc::now(), Some("claude::opus"), None).is_empty(),
            "an empty container snapshot must produce no summary lines"
        );
    }

    /// One entry per running container, carrying the task, container name,
    /// agent/model, and elapsed running time.
    #[test]
    fn the_running_agents_summary_reports_one_entry_per_running_container() {
        let tmp = tempfile::tempdir().unwrap();
        let store = summary_store(tmp.path());
        let now = Utc::now();
        let mut configured = Task {
            id: "id-triage".into(),
            name: "issue-triage".into(),
            description: "d".into(),
            repo_scope: tmp.path().to_path_buf(),
            mount_scope: crate::data::fs::MountScope::Directory,
            overlays: Vec::new(),
            interval_secs: 21_600,
            status: crate::data::fs::TaskStatus::Active,
            agent: Some("codex".into()),
            model: Some("gpt-5".into()),
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            last_run_status: None,
        };
        store.create(&configured).unwrap();
        configured.id = "id-nightly".into();
        configured.name = "nightly-sweep".into();
        configured.agent = None;
        configured.model = None;
        store.create(&configured).unwrap();

        let handles = [
            handle(
                "awman-squad-issue-triage-0123abcd",
                "awman-ws-codex:latest",
                now - chrono::Duration::seconds(90),
            ),
            handle(
                "awman-squad-nightly-sweep-89abcdef",
                "awman-ws-claude:latest",
                now - chrono::Duration::seconds(5),
            ),
        ];

        let summaries = running_agent_summaries(&store, &handles, now, Some("claude::opus"), None);

        assert_eq!(summaries.len(), 2, "one entry per running container");
        assert_eq!(
            summaries[0],
            RunningAgentSummary {
                task: "issue-triage".into(),
                container: "awman-squad-issue-triage-0123abcd".into(),
                agent: "codex".into(),
                model: Some("gpt-5".into()),
                image: "awman-ws-codex:latest".into(),
                elapsed_secs: 90,
            }
        );
        // The second task configures neither agent nor model, so both fall back
        // to `squad.defaultLeader`.
        assert_eq!(summaries[1].task, "nightly-sweep");
        assert_eq!(summaries[1].agent, "claude");
        assert_eq!(summaries[1].model.as_deref(), Some("opus"));
        assert_eq!(summaries[1].elapsed_secs, 5);
    }
}
