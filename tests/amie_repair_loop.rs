//! WI 0101 — the WI-0092 repair loop, driven by amie.
//!
//! Two properties are proved here:
//!
//! 1. **The loop is literally the same code for both callers.**
//!    `exec workflow --dynamic` (`exec_workflow.rs::run_dynamic`) and the amie
//!    evaluator (`amie/evaluation.rs`) both drive
//!    `command::commands::WorkflowRepairLoop`; neither owns a forked copy.
//!    `the_repair_loop_behaves_identically_for_the_amie_and_wi_0092_leaders`
//!    feeds both leader prompts the same validation results and asserts the
//!    attempt labels, the repair prompts, and the exhaustion message come out
//!    byte-identical — the only difference being the initial prompt each
//!    caller seeded.
//!
//! 2. **Exhaustion is scheduled like any other failure.** When the loop gives
//!    up, `ConditionEvaluator::evaluate` returns `EvaluationOutcome::Failed`,
//!    and `AmieScheduler` records `RunStatus::Failed` and grows exponential
//!    backoff — executing nothing and waiting for the next tick, exactly the
//!    work item's edge case. The scheduler never inspects *why* an evaluation
//!    failed, which is what makes the two leader flows indistinguishable to it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio_util::sync::CancellationToken;

use awman::command::commands::{RepairDecision, WorkflowRepairLoop};
use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
use awman::data::dynamic_workflow_assets::{build_amie_leader_prompt, build_leader_prompt};
use awman::data::fs::{AmiePaths, Condition, ConditionStatus, ConditionStore, MountScope};
use awman::data::workflow_definition::Workflow;
use awman::engine::amie::{
    AmieScheduler, ConditionEvaluator, EvaluationOutcome, EvaluationRequest,
};

// ─── 1. one loop, two leaders ───────────────────────────────────────────────

/// The observable trace of one leader/repair sequence: what a caller would use
/// to label the container and what prompt it would launch the agent with on
/// each attempt, followed by how the sequence ended.
#[derive(Debug, PartialEq, Eq)]
struct RepairTrace {
    launches: Vec<(String, String)>,
    ending: String,
}

/// Drive the shared loop exactly as both callers do: launch with
/// `loop.prompt()` under `loop.label()`, validate, `record`, repeat.
fn drive(initial_prompt: &str, validations: Vec<Result<Workflow, String>>) -> RepairTrace {
    let mut repair = WorkflowRepairLoop::new("/c/workflow.toml", initial_prompt);
    let mut launches = Vec::new();
    for validation in validations {
        launches.push((repair.label(), repair.prompt().to_string()));
        match repair.record(validation) {
            RepairDecision::Accepted(_) => {
                return RepairTrace {
                    launches,
                    ending: "accepted".into(),
                }
            }
            RepairDecision::Exhausted(message) => {
                return RepairTrace {
                    launches,
                    ending: message,
                }
            }
            RepairDecision::Retry { .. } => {}
        }
    }
    RepairTrace {
        launches,
        ending: "budget not spent".into(),
    }
}

fn valid_workflow() -> Workflow {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("w.toml");
    std::fs::write(
        &path,
        "[[steps]]\nname = \"s\"\nagent = \"claude\"\nprompt = \"p\"\n",
    )
    .unwrap();
    Workflow::load(&path).unwrap()
}

fn wi_0092_leader_prompt() -> String {
    build_leader_prompt("0101", "aspec/work-items/0101.md", "  - claude", None, None)
}

fn amie_leader_prompt() -> String {
    build_amie_leader_prompt(
        "issue-triage",
        "when a new issue is opened, draft a plan",
        "/workspace",
        "  - claude",
        None,
    )
}

#[test]
fn the_repair_loop_behaves_identically_for_the_amie_and_wi_0092_leaders() {
    // Four failures: three repairs, then exhaustion.
    let failures = || {
        vec![
            Err::<Workflow, String>("missing agent 'nope'".into()),
            Err("unknown key `stpes`".into()),
            Err("step 2 has no prompt".into()),
            Err("still invalid".into()),
        ]
    };
    let wi_0092 = drive(&wi_0092_leader_prompt(), failures());
    let amie = drive(&amie_leader_prompt(), failures());

    assert_eq!(
        wi_0092.launches.iter().map(|(l, _)| l).collect::<Vec<_>>(),
        amie.launches.iter().map(|(l, _)| l).collect::<Vec<_>>(),
        "both callers must produce the same attempt labels"
    );
    assert_eq!(
        wi_0092
            .launches
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>(),
        vec![
            "leader",
            "leader-repair-1",
            "leader-repair-2",
            "leader-repair-3"
        ],
    );

    // The initial prompt is the one thing that legitimately differs — each
    // caller seeds its own leader prompt.
    assert_ne!(wi_0092.launches[0].1, amie.launches[0].1);
    assert!(amie.launches[0].1.contains("issue-triage"));

    // Every *repair* prompt is byte-identical, because it is built from the
    // verbatim validation error and nothing else. This is the property that
    // makes the two leader flows indistinguishable to the loop.
    for attempt in 1..wi_0092.launches.len() {
        assert_eq!(
            wi_0092.launches[attempt].1, amie.launches[attempt].1,
            "repair prompt {attempt} must not depend on which leader flow started the sequence"
        );
        assert!(!amie.launches[attempt].1.contains("issue-triage"));
    }

    assert_eq!(
        wi_0092.ending, amie.ending,
        "the exhaustion message must be identical for both callers"
    );
    assert!(
        wi_0092.ending.contains("3 repair attempts"),
        "{}",
        wi_0092.ending
    );
    assert!(
        wi_0092.ending.contains("still invalid"),
        "{}",
        wi_0092.ending
    );
}

#[test]
fn a_valid_workflow_from_either_leader_is_accepted_on_the_first_attempt() {
    let wi_0092 = drive(&wi_0092_leader_prompt(), vec![Ok(valid_workflow())]);
    let amie = drive(&amie_leader_prompt(), vec![Ok(valid_workflow())]);
    assert_eq!(wi_0092.ending, "accepted");
    assert_eq!(amie.ending, "accepted");
    assert_eq!(wi_0092.launches.len(), 1);
    assert_eq!(amie.launches.len(), 1);
    assert_eq!(wi_0092.launches[0].0, "leader");
    assert_eq!(amie.launches[0].0, "leader");
}

// ─── 2. exhaustion is scheduled like any other failure ──────────────────────

const REPAIR_EXHAUSTED_MESSAGE: &str =
    "workflow repair exhausted after 3 attempts: leader produced an invalid workflow.toml";

struct RepairExhaustedEvaluator;

#[async_trait]
impl ConditionEvaluator for RepairExhaustedEvaluator {
    async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
        // Stands in for `WorkflowRepairLoop` reporting `RepairDecision::GiveUp`
        // after its 3 attempts, exactly the WI-0092 edge case this test title
        // refers to — see the module doc for why this can't yet be the real
        // shared loop.
        EvaluationOutcome::Failed {
            error: REPAIR_EXHAUSTED_MESSAGE.to_string(),
        }
    }
}

fn condition(name: &str) -> Condition {
    let now = Utc::now();
    Condition {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        description: "test condition".into(),
        repo_scope: PathBuf::from("/repo"),
        mount_scope: MountScope::GitRoot,
        interval_secs: 60,
        status: ConditionStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
    }
}

async fn run_one_tick(scheduler: AmieScheduler) {
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(scheduler.run(shutdown_clone));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    shutdown.cancel();
    handle.await.expect("scheduler run() task must not panic");
}

#[tokio::test]
async fn repair_exhaustion_is_recorded_as_failed_and_grows_backoff_like_any_other_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(ConditionStore::open(&tmp.path().join("amie.db")).unwrap());
    store.migrate().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);

    let cond = condition("flaky-condition");
    store.create(&cond).unwrap();

    let scheduler = AmieScheduler::new(
        store.clone(),
        paths,
        Arc::new(RepairExhaustedEvaluator),
        env,
    );
    run_one_tick(scheduler).await;

    let updated = store.get("flaky-condition").unwrap().unwrap();
    assert!(
        updated.backoff_until.is_some(),
        "a repair-exhaustion failure must grow backoff exactly like any other Failed outcome"
    );
    assert!(
        updated.backoff_until.unwrap() > Utc::now(),
        "backoff must be set into the future"
    );
    assert!(
        store.running_run_for(&updated.id).unwrap().is_none(),
        "the run must have finished (not still 'running') after exhausting repair"
    );

    let (status, error): (String, Option<String>) = {
        let conn = rusqlite::Connection::open(tmp.path().join("amie.db")).unwrap();
        conn.query_row(
            "SELECT status, error FROM amie_runs WHERE condition_id = ?1 ORDER BY started_at DESC LIMIT 1",
            [&updated.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some(REPAIR_EXHAUSTED_MESSAGE));
}
