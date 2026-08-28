//! Part 1 condition schema, due-selection, and daemon-gateway tests.

use std::sync::{Arc, Mutex};

use awman::command::commands::amie::gateway::{
    ConditionGateway, CreateCondition, LocalConditionGateway,
};
use awman::command::dispatch::Engines;
use awman::data::fs::{
    AuthPathResolver, Condition, ConditionStatus, ConditionStore, DataPaths, MountScope,
};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::amie::SchedulerStatus;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use chrono::{Duration, Utc};

fn condition(name: &str, now: chrono::DateTime<Utc>) -> Condition {
    Condition {
        id: format!("id-{name}"),
        name: name.into(),
        description: format!("when {name} happens"),
        repo_scope: "/repo".into(),
        mount_scope: MountScope::GitRoot,
        interval_secs: 300,
        status: ConditionStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now - Duration::hours(1),
        updated_at: now - Duration::hours(1),
        last_run_at: None,
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
fn condition_store_schema_migration_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let first = ConditionStore::open(&db).unwrap();
    first.migrate().unwrap();
    drop(first);
    let second = ConditionStore::open(&db).unwrap();
    second.migrate().unwrap();
    assert!(second.list().unwrap().is_empty());
    drop(second);
    // A third open + migrate verifies that both CREATE TABLE IF NOT EXISTS and
    // the additive-column migration remain no-ops after repeated startup, and
    // that `migrate` is callable more than once against one open store.
    let third = ConditionStore::open(&db).unwrap();
    third.migrate().unwrap();
    third.migrate().unwrap();
    assert!(third.list().unwrap().is_empty());
}

#[test]
fn due_for_evaluation_applies_pause_backoff_interval_and_running_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = ConditionStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let due_fresh = condition("fresh", now);
    let due_elapsed = Condition {
        last_run_at: Some(now - Duration::seconds(301)),
        ..condition("elapsed", now)
    };
    let paused = Condition {
        status: ConditionStatus::Paused,
        ..condition("paused", now)
    };
    let backed_off = Condition {
        backoff_until: Some(now + Duration::minutes(5)),
        ..condition("backoff", now)
    };
    let interval_not_elapsed = Condition {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        ..condition("interval", now)
    };
    let running = condition("running", now);

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
async fn condition_store_crud_is_exercised_through_daemon_gateway() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(ConditionStore::open(&db).unwrap());
    store.migrate().unwrap();
    let gateway = LocalConditionGateway::new(
        store,
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
    );
    let request = || CreateCondition {
        name: "issue-triage".into(),
        description: "when an issue is opened".into(),
        repo_scope: repo.clone(),
        mount_scope: MountScope::GitRoot,
        interval_secs: 300,
        agent: None,
        model: None,
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
        .set_status("issue-triage", ConditionStatus::Paused)
        .await
        .unwrap();
    assert_eq!(
        gateway.get("issue-triage").await.unwrap().status,
        ConditionStatus::Paused
    );
    gateway
        .set_status("issue-triage", ConditionStatus::Active)
        .await
        .unwrap();
    gateway.delete("issue-triage").await.unwrap();
    assert!(gateway.list().await.unwrap().is_empty());
    assert!(gateway.get("issue-triage").await.is_err());
}
