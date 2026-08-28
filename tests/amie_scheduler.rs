//! WI 0101 — `AmieScheduler` tick loop, driven with a mocked
//! `ConditionEvaluator`.
//!
//! `ConditionEvaluator` is the Layer-1/Layer-2 seam designed for exactly this
//! purpose (`src/engine/amie/evaluator.rs`): the scheduler owns the run row's
//! lifecycle and never touches condition-evaluation logic itself, so a mock
//! evaluator exercises the scheduler's real behaviour end-to-end without a
//! real leader agent or container.
//!
//! NOTE — scope: at the time this test was written, `command-layer`'s
//! `LocalConditionEvaluator` (the real Layer-2 evaluator that would validate
//! and execute a leader-produced `workflow.toml` through the actual workflow
//! engine) does not exist in this tree (no
//! `src/command/commands/amie/evaluation.rs`, confirmed absent). The mock
//! evaluator below simulates what a real evaluator does at the *contract*
//! level — it receives the request, "writes" `workflow.toml` into the
//! condition directory the same way a real leader would, and returns the
//! outcome the scheduler is contractually promised — which is exactly what
//! `engine-amie`'s own handoff scoped this test file to cover. True
//! end-to-end validation (a real agent producing and running a real
//! workflow) is blocked on that missing evaluator; see
//! `test-plan-amie.md` for the flagged gap.
//!
//! `run_one_tick` drives the scheduler through exactly one tick using its
//! public `run(shutdown)` API: spawn `run()`, let the first (synchronous)
//! `tick()` dispatch before the loop reaches its 30s sleep, then cancel —
//! `run()`'s shutdown path drains every in-flight evaluation before
//! returning, so by the time `run_one_tick` returns, the tick is fully done.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio_util::sync::CancellationToken;

use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
use awman::data::config::global::GlobalConfig;
use awman::data::config::repo::AmieConfig;
use awman::data::fs::{AmiePaths, Condition, ConditionStatus, ConditionStore, MountScope};
use awman::engine::amie::{
    AmieScheduler, ConditionEvaluator, EvaluationOutcome, EvaluationRequest,
};

// ─── Mock evaluator ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RecordedRequest {
    condition_name: String,
    condition_dir: PathBuf,
    guidance: Option<Vec<String>>,
    agents_to_models: Option<HashMap<String, Vec<String>>>,
    default_leader: Option<String>,
}

struct MockEvaluator<F> {
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    respond: F,
}

#[async_trait]
impl<F> ConditionEvaluator for MockEvaluator<F>
where
    F: Fn(&EvaluationRequest) -> EvaluationOutcome + Send + Sync,
{
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
        self.recorded.lock().unwrap().push(RecordedRequest {
            condition_name: request.condition.name.clone(),
            condition_dir: request.condition_dir.clone(),
            guidance: request.guidance.clone(),
            agents_to_models: request.agents_to_models.clone(),
            default_leader: request.default_leader.clone(),
        });
        (self.respond)(&request)
    }
}

// ─── Test fixtures ─────────────────────────────────────────────────────────

fn condition(name: &str, interval_secs: u64) -> Condition {
    let now = Utc::now();
    Condition {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        description: "test condition".into(),
        repo_scope: PathBuf::from("/repo"),
        mount_scope: MountScope::GitRoot,
        interval_secs,
        status: ConditionStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
    }
}

/// Run the scheduler through exactly one tick and return once every
/// evaluation it dispatched has finished.
async fn run_one_tick(scheduler: AmieScheduler) {
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(scheduler.run(shutdown_clone));
    // Yield long enough for the first (synchronous) tick() to run and spawn
    // its evaluation tasks before we request shutdown. TICK_INTERVAL is 30s,
    // so there is no risk of a second tick sneaking in during this window.
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.cancel();
    handle.await.expect("scheduler run() task must not panic");
}

fn amie_root_env(tmp: &std::path::Path) -> EnvSnapshot {
    EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.to_str().unwrap())])
}

// ─── Full lifecycle: mocked evaluation agent writes workflow.toml ───────────

#[tokio::test]
async fn full_lifecycle_mocked_evaluator_writes_workflow_and_records_run() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());

    let cond = condition("issue-triage", 60);
    store.create(&cond).unwrap();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let evaluator = Arc::new(MockEvaluator {
        recorded: recorded.clone(),
        respond: |request: &EvaluationRequest| {
            // Simulate the leader agent's real effect: it writes a
            // workflow.toml into the condition directory. A real evaluator
            // seeds the directory via `AmieAgentLauncher::seed_condition_dir`
            // before launching the leader; replicate that here since the
            // evaluator itself is mocked.
            std::fs::create_dir_all(&request.condition_dir).unwrap();
            let workflow_path = request.condition_dir.join("workflow.toml");
            std::fs::write(&workflow_path, "# generated by mock leader\n").unwrap();
            EvaluationOutcome::WorkflowExecuted {
                workflow_path,
                workflow_state_path: Some(request.condition_dir.join("workflow-state.json")),
                exit_code: Some(0),
            }
        },
    });

    let scheduler = AmieScheduler::new(store.clone(), paths, evaluator, env);
    run_one_tick(scheduler).await;

    // The evaluator was invoked exactly once, with this condition's identity
    // and the config-sourced fields threaded through verbatim.
    let recorded = recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1, "evaluator must be invoked exactly once");
    assert_eq!(recorded[0].condition_name, "issue-triage");
    // No `amie.agentsToModels` / `defaultLeader` was configured, so both
    // pass through as the un-set defaults — proving the scheduler forwards
    // whatever the config actually has, rather than inventing a value.
    assert_eq!(recorded[0].agents_to_models, None);
    assert_eq!(recorded[0].default_leader, None);
    assert!(
        recorded[0].condition_dir.join("workflow.toml").exists(),
        "the leader's generated workflow.toml must exist on disk after the tick"
    );

    // The condition's own bookkeeping reflects a completed, non-failing run.
    let updated = store.get("issue-triage").unwrap().unwrap();
    assert!(updated.last_run_at.is_some(), "last_run_at must be set");
    assert!(
        updated.backoff_until.is_none(),
        "a successful run must carry no backoff"
    );
    assert!(
        store.running_run_for(&updated.id).unwrap().is_none(),
        "no run should still be 'running' after the tick finished"
    );

    // The run row itself: `ConditionStore`'s public surface has no history
    // query yet (`LocalConditionGateway::runs` even says so explicitly), so
    // read the persisted row directly to confirm it was recorded correctly.
    let (status, workflow_path, workflow_state_path, error): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.query_row(
            "SELECT status, workflow_path, workflow_state_path, error FROM amie_runs \
             WHERE condition_id = ?1 ORDER BY started_at DESC LIMIT 1",
            [&updated.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    };
    assert_eq!(status, "workflow_executed");
    assert!(workflow_path.unwrap().ends_with("workflow.toml"));
    assert!(workflow_state_path
        .unwrap()
        .ends_with("workflow-state.json"));
    assert!(error.is_none());
}

// ─── Not-triggered path ──────────────────────────────────────────────────────

#[tokio::test]
async fn not_triggered_path_records_not_triggered_and_generates_no_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());

    let cond = condition("watch-only", 60);
    store.create(&cond).unwrap();

    let evaluator = Arc::new(MockEvaluator {
        recorded: Arc::new(Mutex::new(Vec::new())),
        respond: |_request: &EvaluationRequest| EvaluationOutcome::NotTriggered,
    });

    let scheduler = AmieScheduler::new(store.clone(), paths.clone(), evaluator, env);
    run_one_tick(scheduler).await;

    let updated = store.get("watch-only").unwrap().unwrap();
    assert!(updated.last_run_at.is_some());
    assert!(updated.backoff_until.is_none());

    let condition_dir = paths.condition_dir("watch-only").unwrap();
    assert!(
        !condition_dir.join("workflow.toml").exists(),
        "not-triggered must generate no workflow"
    );

    let (status, workflow_path): (String, Option<String>) = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.query_row(
            "SELECT status, workflow_path FROM amie_runs WHERE condition_id = ?1 \
             ORDER BY started_at DESC LIMIT 1",
            [&updated.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(status, "not_triggered");
    assert!(workflow_path.is_none());
}

// ─── Persistent condition directory across two sequential runs ──────────────

#[tokio::test]
async fn condition_directory_persists_across_two_sequential_runs() {
    // Contrast with `context(workflow)`'s per-invocation directory: amie's
    // condition directory is `context(global)`-style — created once and
    // reused, never wiped between evaluations. Two *separate* scheduler
    // instances stand in for "two daemon runs" against the same store/paths.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());

    // interval_secs: 0 so the condition is due again immediately on the
    // second run (the store's own admission SQL is the only gate here —
    // `ConditionStore::create` performs no interval-range validation; that
    // bound is enforced by `LocalConditionGateway`, a higher layer).
    let cond = condition("persistent-dir", 0);
    store.create(&cond).unwrap();

    let evaluator1 = Arc::new(MockEvaluator {
        recorded: Arc::new(Mutex::new(Vec::new())),
        respond: |request: &EvaluationRequest| {
            std::fs::create_dir_all(&request.condition_dir).unwrap();
            std::fs::write(request.condition_dir.join("run1-marker.txt"), "run1").unwrap();
            EvaluationOutcome::NotTriggered
        },
    });
    let scheduler1 = AmieScheduler::new(store.clone(), paths.clone(), evaluator1, env.clone());
    run_one_tick(scheduler1).await;

    let evaluator2 = Arc::new(MockEvaluator {
        recorded: Arc::new(Mutex::new(Vec::new())),
        respond: |request: &EvaluationRequest| {
            std::fs::create_dir_all(&request.condition_dir).unwrap();
            std::fs::write(request.condition_dir.join("run2-marker.txt"), "run2").unwrap();
            EvaluationOutcome::NotTriggered
        },
    });
    let scheduler2 = AmieScheduler::new(store.clone(), paths.clone(), evaluator2, env);
    run_one_tick(scheduler2).await;

    let condition_dir = paths.condition_dir("persistent-dir").unwrap();
    assert!(
        condition_dir.join("run1-marker.txt").exists(),
        "the first run's marker must still be present after the second run"
    );
    assert!(
        condition_dir.join("run2-marker.txt").exists(),
        "the second run's marker must be present too"
    );
}

// ─── Live config re-read: amie.* edits take effect without a restart ────────

#[tokio::test]
async fn amie_config_edits_take_effect_on_the_next_tick_without_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());

    let cond = condition("config-reread", 0);
    store.create(&cond).unwrap();

    GlobalConfig {
        amie: Some(AmieConfig {
            guidance: Some(vec!["v1-guidance".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
    .save_with(&env)
    .unwrap();

    let recorded1 = Arc::new(Mutex::new(Vec::new()));
    let evaluator1 = Arc::new(MockEvaluator {
        recorded: recorded1.clone(),
        respond: |_request: &EvaluationRequest| EvaluationOutcome::NotTriggered,
    });
    let scheduler1 = AmieScheduler::new(store.clone(), paths.clone(), evaluator1, env.clone());
    run_one_tick(scheduler1).await;
    assert_eq!(
        recorded1.lock().unwrap()[0].guidance,
        Some(vec!["v1-guidance".to_string()])
    );

    // Edit config on disk — no scheduler restart, no new env, nothing but
    // rewriting the file the scheduler already points at.
    GlobalConfig {
        amie: Some(AmieConfig {
            guidance: Some(vec!["v2-guidance".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
    .save_with(&env)
    .unwrap();

    let recorded2 = Arc::new(Mutex::new(Vec::new()));
    let evaluator2 = Arc::new(MockEvaluator {
        recorded: recorded2.clone(),
        respond: |_request: &EvaluationRequest| EvaluationOutcome::NotTriggered,
    });
    let scheduler2 = AmieScheduler::new(store.clone(), paths.clone(), evaluator2, env);
    run_one_tick(scheduler2).await;
    assert_eq!(
        recorded2.lock().unwrap()[0].guidance,
        Some(vec!["v2-guidance".to_string()]),
        "the next tick must observe the edited config with no restart"
    );
}

// ─── Restart reconciliation ──────────────────────────────────────────────────

/// `frontend::amie::serve_with` calls `ConditionStore::reconcile_orphaned_runs`
/// immediately after opening the store, before the scheduler is spawned —
/// this test reproduces that exact sequence (open → reconcile → tick) against
/// a store that already has an orphaned `running` row, simulating a daemon
/// that crashed mid-evaluation and is now restarting.
#[tokio::test]
async fn restart_reconciliation_moves_orphaned_running_row_to_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");

    // First "daemon lifetime": open the store, create a condition, and leave
    // a run row stuck in `running` — as if the process was killed mid-tick.
    let cond_id = {
        let store = ConditionStore::open(&db_path).unwrap();
        store.migrate().unwrap();
        // interval_secs: 0 so the condition is immediately due again after
        // reconciliation, regardless of how little wall-clock time this fast
        // test takes between the two "daemon lifetimes".
        let cond = condition("orphaned-condition", 0);
        store.create(&cond).unwrap();
        store.start_run(&cond.id, None, Utc::now()).unwrap();
        assert!(
            store.running_run_for(&cond.id).unwrap().is_some(),
            "sanity: the run must be 'running' before the simulated crash"
        );
        cond.id
    };

    // Second "daemon lifetime": re-open the same database (a fresh
    // `ConditionStore`, mimicking a real restart) and reconcile — exactly
    // what `serve_with` does before ever constructing the scheduler.
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let reconciled = store.reconcile_orphaned_runs(Utc::now()).unwrap();
    assert_eq!(
        reconciled, 1,
        "exactly the one orphaned run must be reconciled"
    );

    assert!(
        store.running_run_for(&cond_id).unwrap().is_none(),
        "the orphaned run must no longer read as 'running'"
    );
    let status: String = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.query_row(
            "SELECT status FROM amie_runs WHERE condition_id = ?1 ORDER BY started_at DESC LIMIT 1",
            [&cond_id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(status, "interrupted");

    // The condition is due again immediately afterwards — reconciliation
    // must not leave it permanently stuck behind the SQL's
    // `NOT EXISTS (... status = 'running')` admission clause.
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());
    let evaluator = Arc::new(MockEvaluator {
        recorded: Arc::new(Mutex::new(Vec::new())),
        respond: |request: &EvaluationRequest| {
            std::fs::create_dir_all(&request.condition_dir).unwrap();
            EvaluationOutcome::NotTriggered
        },
    });
    let scheduler = AmieScheduler::new(store.clone(), paths, evaluator, env);
    run_one_tick(scheduler).await;

    let after_tick = store.get("orphaned-condition").unwrap().unwrap();
    assert!(
        after_tick.last_run_at.is_some(),
        "the reconciled condition must be selectable and evaluated again"
    );
}

// ─── Global concurrency bound (WI 0101 §2.4) ────────────────────────────────

/// `maxConcurrentEvaluations` bounds the scheduler's whole in-flight set, and a
/// condition that has been dispatched must hold a committed `running` row
/// before the next tick can select it again.
///
/// With a cap of 1 and two due conditions, tick 1 may dispatch exactly one of
/// them; the other must be left *undispatched* (no run row at all) rather than
/// queued behind a permit, because a queued condition with no `running` row is
/// exactly what `due_for_evaluation` would hand out a second time.
#[tokio::test]
async fn concurrency_cap_bounds_the_whole_in_flight_set_and_never_queues_an_unclaimed_condition() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());

    GlobalConfig {
        amie: Some(AmieConfig {
            max_concurrent_evaluations: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    }
    .save_with(&env)
    .unwrap();

    let first = condition("cap-one", 0);
    let second = condition("cap-two", 0);
    store.create(&first).unwrap();
    store.create(&second).unwrap();

    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));

    struct SlowEvaluator {
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ConditionEvaluator for SlowEvaluator {
        async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
            self.started.fetch_add(1, Ordering::SeqCst);
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(400)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            EvaluationOutcome::NotTriggered
        }
    }

    let evaluator = Arc::new(SlowEvaluator {
        live: live.clone(),
        peak: peak.clone(),
        started: started.clone(),
    });
    // A short cadence so several ticks fire while the first evaluation is still
    // in flight — the situation a per-tick bound would get wrong.
    let scheduler = AmieScheduler::new(store.clone(), paths, evaluator, env)
        .with_tick_interval(Duration::from_millis(50));

    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    // While the first evaluation is still running, several more ticks have
    // fired. The second condition must not have been dispatched at all.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the cap must hold across ticks, not just within one tick"
    );
    let second_runs = store.runs_for("cap-two", 10).unwrap();
    assert!(
        second_runs.is_empty(),
        "a condition the cap deferred must have no run row: it was never dispatched"
    );

    shutdown.cancel();
    handle.await.unwrap();

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "no more than maxConcurrentEvaluations may ever be in flight"
    );
    // The first condition's run row exists and was committed by the tick, not
    // by the task, so the SQL admission predicate could see it immediately.
    let first_runs = store.runs_for("cap-one", 10).unwrap();
    assert_eq!(first_runs.len(), 1);
}

/// A running scheduler must observe a config edit on its *next* tick — no
/// restart, no new scheduler instance. This drives one live scheduler across
/// two ticks with a shortened cadence.
#[tokio::test]
async fn a_running_scheduler_observes_a_config_edit_on_its_next_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amie.db");
    let store = Arc::new(ConditionStore::open(&db_path).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = amie_root_env(tmp.path());

    store.create(&condition("live-reread", 0)).unwrap();

    GlobalConfig {
        amie: Some(AmieConfig {
            guidance: Some(vec!["v1-guidance".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
    .save_with(&env)
    .unwrap();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let evaluator = Arc::new(MockEvaluator {
        recorded: recorded.clone(),
        respond: |_request: &EvaluationRequest| EvaluationOutcome::NotTriggered,
    });
    let scheduler = AmieScheduler::new(store.clone(), paths, evaluator, env.clone())
        .with_tick_interval(Duration::from_millis(50));

    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    // Wait for the first tick to observe v1.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        recorded.lock().unwrap().first().map(|r| r.guidance.clone()),
        Some(Some(vec!["v1-guidance".to_string()])),
        "the first tick must observe the config as written at start-up"
    );

    // Rewrite the config under the still-running scheduler.
    GlobalConfig {
        amie: Some(AmieConfig {
            guidance: Some(vec!["v2-guidance".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
    .save_with(&env)
    .unwrap();

    // Later ticks of the same scheduler must pick the edit up.
    let mut saw_v2 = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if recorded
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.guidance == Some(vec!["v2-guidance".to_string()]))
        {
            saw_v2 = true;
            break;
        }
    }
    shutdown.cancel();
    handle.await.unwrap();
    assert!(
        saw_v2,
        "a running scheduler must observe an amie.* edit on a later tick without a restart"
    );
}
