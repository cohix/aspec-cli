//! Part 1 task schema, due-selection, and daemon-gateway tests.

use std::sync::{Arc, Mutex};

use awman::command::commands::squad::gateway::{CreateTask, LocalTaskGateway, TaskGateway};
use awman::command::dispatch::Engines;
use awman::data::fs::{
    AuthPathResolver, DataPaths, MountScope, RunDetail, RunStatus, Task, TaskStatus, TaskStore,
    TaskWorkspace,
};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::SchedulerStatus;
use chrono::{Duration, Utc};

fn task(name: &str, now: chrono::DateTime<Utc>) -> Task {
    Task {
        id: format!("id-{name}"),
        name: name.into(),
        description: format!("when {name} happens"),
        repo_scope: "/repo".into(),
        mount_scope: MountScope::GitRoot,
        overlays: Vec::new(),
        interval_secs: 300,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now - Duration::hours(1),
        updated_at: now - Duration::hours(1),
        last_run_at: None,
        last_run_status: None,
    }
}

fn test_engines(root: &std::path::Path) -> Engines {
    let api_paths = awman::data::fs::ApiPaths::from_root(root.join("api"));
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay,
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths)),
        agent_engine: agent,
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(root)),
    }
}

#[test]
fn task_store_schema_migration_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let first = TaskStore::open(&db).unwrap();
    first.migrate().unwrap();
    drop(first);
    let second = TaskStore::open(&db).unwrap();
    second.migrate().unwrap();
    assert!(second.list().unwrap().is_empty());
    drop(second);
    // A third open + migrate verifies that both CREATE TABLE IF NOT EXISTS and
    // the additive-column migration remain no-ops after repeated startup, and
    // that `migrate` is callable more than once against one open store.
    let third = TaskStore::open(&db).unwrap();
    third.migrate().unwrap();
    third.migrate().unwrap();
    assert!(third.list().unwrap().is_empty());
}

#[test]
fn squad_schema_and_workspace_overlay_fields_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();
    let workspace = tmp.path().join("squad/tasks/durable/workspace");
    let stored = Task {
        id: "durable-id".into(),
        name: "durable".into(),
        description: "preserve task state".into(),
        // `TaskWorkspace` is resolved before persistence.  This is the
        // persisted representation of the Default Task Workspace choice.
        repo_scope: workspace.clone(),
        mount_scope: MountScope::Directory,
        overlays: vec![
            "dir(/host/data:/task-data:ro)".into(),
            "env(SQUAD_TOKEN)".into(),
            "skill(review)".into(),
        ],
        interval_secs: 6 * 60 * 60,
        status: TaskStatus::Active,
        agent: Some("codex".into()),
        model: Some("gpt-5".into()),
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        last_run_status: None,
    };
    store.create(&stored).unwrap();

    assert_eq!(store.get("durable").unwrap(), Some(stored.clone()));
    assert!(
        !stored.uses_worktree(),
        "the persisted default-workspace representation must mount directly"
    );

    let run_id = store.start_run(&stored.id, Some("session"), now).unwrap();
    store
        .finish_run(&run_id, RunStatus::NotTriggered, &RunDetail::default(), now)
        .unwrap();
    let runs = store.runs_for("durable", 1).unwrap();
    assert_eq!(runs[0].task_id, stored.id);
    assert_eq!(runs[0].status, RunStatus::NotTriggered);

    let conn = rusqlite::Connection::open(db).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(tables.contains(&"squad_tasks".to_string()));
    assert!(tables.contains(&"squad_runs".to_string()));
    assert!(
        !tables
            .iter()
            .any(|name| name.contains("amie") || name.contains("conditions")),
        "the unreleased rename must not create legacy schema tables: {tables:?}"
    );
}

#[tokio::test]
async fn malformed_task_overlay_is_rejected_before_store_or_workspace_write() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let paths = awman::data::fs::SquadPaths::from_root(tmp.path().join("squad"));
    let gateway = LocalTaskGateway::new(
        store.clone(),
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        paths.clone(),
    );
    let request = CreateTask {
        name: "bad-overlay".into(),
        description: "must fail before persistence".into(),
        workspace: TaskWorkspace::Default,
        mount_scope: MountScope::Directory,
        interval_secs: 6 * 60 * 60,
        agent: None,
        model: None,
        overlays: vec!["not-an-overlay".into()],
    };

    let error = gateway.create(request).await.unwrap_err();
    assert!(error.to_string().contains("overlay"), "{error}");
    assert!(
        store.list().unwrap().is_empty(),
        "no invalid task may be stored"
    );
    assert!(
        !paths.task_dir("bad-overlay").unwrap().exists(),
        "validation failure must not create a durable workspace"
    );
}

#[test]
fn due_for_evaluation_applies_pause_backoff_interval_and_running_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let due_fresh = task("fresh", now);
    let due_elapsed = Task {
        last_run_at: Some(now - Duration::seconds(301)),
        last_run_status: None,
        ..task("elapsed", now)
    };
    let paused = Task {
        status: TaskStatus::Paused,
        ..task("paused", now)
    };
    let backed_off = Task {
        backoff_until: Some(now + Duration::minutes(5)),
        ..task("backoff", now)
    };
    let interval_not_elapsed = Task {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        last_run_status: None,
        ..task("interval", now)
    };
    let running = task("running", now);

    for item in [
        due_fresh.clone(),
        due_elapsed.clone(),
        paused,
        backed_off,
        interval_not_elapsed,
        running.clone(),
    ] {
        store.create(&item).unwrap();
    }
    store.start_run(&running.id, Some("session"), now).unwrap();

    let names: std::collections::BTreeSet<_> = store
        .due_for_evaluation(now)
        .unwrap()
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert_eq!(
        names,
        ["fresh", "elapsed"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
}

#[tokio::test]
async fn task_store_crud_is_exercised_through_daemon_gateway() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let gateway = LocalTaskGateway::new(
        store,
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        awman::data::fs::SquadPaths::from_root(tmp.path().join("squad")),
    );
    let request = || CreateTask {
        name: "issue-triage".into(),
        description: "when an issue is opened".into(),
        workspace: TaskWorkspace::Custom(repo.clone()),
        mount_scope: MountScope::GitRoot,
        interval_secs: 300,
        agent: None,
        model: None,
        overlays: Vec::new(),
    };

    let created = gateway.create(request()).await.unwrap();
    assert_eq!(created.name, "issue-triage");
    assert_eq!(gateway.list().await.unwrap().len(), 1);
    assert_eq!(gateway.get("issue-triage").await.unwrap(), created);

    let duplicate = gateway
        .create(request())
        .await
        .expect_err("duplicate names must be rejected by the gateway");
    assert!(
        duplicate.to_string().contains("issue-triage"),
        "unique-name error should identify the existing name: {duplicate}"
    );

    gateway
        .set_status("issue-triage", TaskStatus::Paused)
        .await
        .unwrap();
    assert_eq!(
        gateway.get("issue-triage").await.unwrap().status,
        TaskStatus::Paused
    );
    gateway
        .set_status("issue-triage", TaskStatus::Active)
        .await
        .unwrap();
    gateway.delete("issue-triage").await.unwrap();
    assert!(gateway.list().await.unwrap().is_empty());
    assert!(gateway.get("issue-triage").await.is_err());
}

/// WI 0106 Part 5: the task grid shows every card's last-run *outcome*, so the
/// store reads it alongside the task in one statement rather than making the
/// UI ask for run history per card.
#[test]
fn list_and_get_carry_each_tasks_latest_run_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let never_run = task("never-run", now);
    let ran = task("ran", now);
    store.create(&never_run).unwrap();
    store.create(&ran).unwrap();

    assert_eq!(store.get("ran").unwrap().unwrap().last_run_status, None);

    // Two runs: the older triggered a workflow, the newer did not. Only the
    // newer one is the "last run".
    let first = store
        .start_run(&ran.id, None, now - Duration::hours(2))
        .unwrap();
    store
        .finish_run(
            &first,
            RunStatus::WorkflowExecuted,
            &RunDetail::default(),
            now - Duration::hours(2),
        )
        .unwrap();
    let second = store
        .start_run(&ran.id, None, now - Duration::minutes(5))
        .unwrap();
    store
        .finish_run(
            &second,
            RunStatus::NotTriggered,
            &RunDetail::default(),
            now - Duration::minutes(5),
        )
        .unwrap();

    assert_eq!(
        store.get("ran").unwrap().unwrap().last_run_status,
        Some(RunStatus::NotTriggered),
        "get must report the most recent run's outcome"
    );

    let listed = store.list().unwrap();
    let by_name = |name: &str| {
        listed
            .iter()
            .find(|task| task.name == name)
            .unwrap()
            .last_run_status
    };
    assert_eq!(by_name("ran"), Some(RunStatus::NotTriggered));
    assert_eq!(
        by_name("never-run"),
        None,
        "a task that has never run reports no outcome, not a neighbour's"
    );
}

/// WI 0106 §2b: worktree isolation is derived from "the effective task root
/// **is** a git repository root", not from "the effective task root is
/// somewhere inside one".
///
/// A worktree is a checkout of the whole repository, so worktree-isolating a
/// task bound to a *subdirectory* would silently hand the run the entire
/// enclosing repository instead of the folder the user picked — a widening the
/// captured mount scope exists to prevent. A subdirectory is exactly the "not
/// a git root" case the interview warns about, so it is bound as the plain
/// directory it is: direct mount, no worktree.
#[tokio::test]
async fn a_custom_workspace_below_a_repository_root_is_a_plain_directory_and_never_worktree_isolated(
) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_test_repo(&repo);
    let nested = repo.join("services").join("api");
    std::fs::create_dir_all(&nested).unwrap();

    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let gateway = LocalTaskGateway::new(
        store,
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        awman::data::fs::SquadPaths::from_root(tmp.path().join("squad")),
    );

    // The repository root itself keeps its captured scope and is
    // worktree-isolated: a worktree of the root mounts exactly the root.
    let at_root = gateway
        .create(CreateTask {
            name: "at-root".into(),
            description: "bound to the repository root".into(),
            workspace: TaskWorkspace::Custom(repo.clone()),
            mount_scope: MountScope::GitRoot,
            interval_secs: 6 * 60 * 60,
            agent: None,
            model: None,
            overlays: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(at_root.mount_scope, MountScope::GitRoot);
    assert!(
        at_root.uses_worktree(),
        "a task bound to a repository root is always worktree-isolated"
    );

    // A subdirectory of that same repository is not. Even asked for with
    // `--mount-scope gitroot`, it is stored as the directory it is.
    let below_root = gateway
        .create(CreateTask {
            name: "below-root".into(),
            description: "bound to a subdirectory".into(),
            workspace: TaskWorkspace::Custom(nested.clone()),
            mount_scope: MountScope::GitRoot,
            interval_secs: 6 * 60 * 60,
            agent: None,
            model: None,
            overlays: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        below_root.mount_scope,
        MountScope::Directory,
        "a path below the repository root is bound as a plain directory"
    );
    assert!(
        !below_root.uses_worktree(),
        "worktree isolation would widen this run's view to the whole repository"
    );
    assert_eq!(
        below_root.repo_scope,
        nested.canonicalize().unwrap(),
        "the effective root stays the folder the user chose"
    );
}

fn init_test_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .expect("git must run");
    assert!(status.success(), "git init must succeed");
}
