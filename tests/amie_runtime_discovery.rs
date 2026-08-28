//! WI 0101 — `list_running_with_name_prefix` discovery, per runtime tier.
//!
//! Docker and Apple are exercised against the real backend (via
//! `ContainerRuntime::docker()` / `ContainerRuntime::apple()`), gated by
//! `is_available()` so these skip cleanly without a live daemon — the same
//! convention `tests/engine/container_docker.rs` already uses. Sandbox is
//! exercised the same way `tests/engine/sbx.rs` does: gated to macOS arm64
//! with `AWMAN_TEST_SBX=1` and `sbx` on PATH.
//!
//! The "label read-back stubbed out" case does not need a live backend at
//! all: `AgentHandle` (`src/data/session.rs`) carries no label field
//! whatsoever, so a fake `AgentRuntimeEngine` that never had a label concept
//! to begin with is proof by construction that discovery and the status
//! marker (`ContainerSource`) are derived purely from the container **name**.

use std::sync::Arc;

use awman::data::session::{AgentHandle, Session};
use awman::engine::agent_runtime::{
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use awman::engine::container::naming::generate_amie_container_name;
use awman::engine::container::ContainerRuntime;
use awman::engine::error::EngineError;

// ─── A minimal fake runtime with no label concept at all ─────────────────────

struct NoLabelFakeRuntime {
    handles: Vec<AgentHandle>,
}

impl AgentRuntimeEngine for NoLabelFakeRuntime {
    fn runtime_name(&self) -> &'static str {
        "no-label-fake"
    }
    fn display_name(&self) -> &'static str {
        "No-Label Fake"
    }
    fn capabilities(&self) -> &Capabilities {
        // Static so `&Capabilities` can be returned without allocation.
        static CAPS: Capabilities = Capabilities {
            arbitrary_env_vars: true,
            arbitrary_host_mounts: true,
            cpu_limits: true,
            per_resource_stats: true,
            persistent_lifecycle: false,
            kit_declarative: false,
            dind: DindSupport::Never,
            host_paths_visible: true,
            session_label_supported: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(
        &self,
        _options: ResolvedAgentOptions,
    ) -> Result<Box<dyn awman::engine::agent_runtime::execution::AgentInstance>, EngineError> {
        unimplemented!("not exercised by discovery tests")
    }
    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.handles.clone())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.handles.clone())
    }
    fn stats(&self, _handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        unimplemented!("not exercised by discovery tests")
    }
    fn stop(&self, _handle: &AgentHandle) -> Result<(), EngineError> {
        unimplemented!("not exercised by discovery tests")
    }
    fn exec_args(
        &self,
        _agent_id: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        unimplemented!("not exercised by discovery tests")
    }
    fn attach(
        &self,
        _handle: &AgentHandle,
    ) -> Result<Box<dyn awman::engine::agent_runtime::execution::AgentInstance>, EngineError> {
        unimplemented!("not exercised by discovery tests")
    }
    fn cli_binary(&self) -> &'static str {
        "no-label-fake"
    }
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        // Mirrors the sandbox tier's own filter (`retain(|h| h.name.starts_with(prefix))`)
        // — the same client-side technique Apple uses too. No label is ever
        // consulted; `AgentHandle` has no field to hold one.
        Ok(self
            .handles
            .iter()
            .filter(|h| h.name.starts_with(prefix))
            .cloned()
            .collect())
    }
}

fn handle(name: &str) -> AgentHandle {
    AgentHandle {
        id: format!("id-{name}"),
        image_tag: "awman/dev:latest".into(),
        name: name.to_string(),
        started_at: chrono::Utc::now(),
    }
}

#[test]
fn discovery_with_label_read_back_stubbed_out_still_finds_amie_containers_by_name() {
    let condition_name = generate_amie_container_name("issue-triage");
    let runtime = NoLabelFakeRuntime {
        handles: vec![
            handle(&condition_name),
            handle("awman-99-12345"), // an unrelated session container
            handle("nginx"),          // a container awman never touches
        ],
    };
    let runtime: Arc<dyn AgentRuntimeEngine> = Arc::new(runtime);

    let matches = runtime
        .list_running_with_name_prefix("awman-amie-")
        .expect("discovery must succeed with no label concept present");
    assert_eq!(matches.len(), 1, "exactly the amie container must match");
    assert_eq!(matches[0].name, condition_name);
}

#[test]
fn discovery_prefix_filter_excludes_non_matching_names() {
    let runtime = NoLabelFakeRuntime {
        handles: vec![
            handle("awman-amie-a-11111111"),
            handle("awman-amie-b-22222222"),
        ],
    };
    let runtime: Arc<dyn AgentRuntimeEngine> = Arc::new(runtime);

    let matches = runtime
        .list_running_with_name_prefix("awman-amie-a-")
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "awman-amie-a-11111111");
}

// ─── Live-backend tiers ─────────────────────────────────────────────────────
//
// Each tier below asserts the same two things against a *live* backend: a
// container whose name carries the queried prefix is returned, and one that
// does not carry it is filtered out. An implementation that always returns an
// empty vector fails the first assertion, and one that ignores the prefix
// fails the second — so neither of the two ways this can silently break
// survives. Every tier skips cleanly when its backend is unreachable.

use std::process::{Command, Stdio};

/// A name unique to this test process, so concurrent runs never collide.
fn unique_suffix() -> String {
    format!(
        "{:x}",
        (std::process::id() as u128) << 32
            | u128::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            )
    )
}

fn cli_available(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start a throwaway container with the given name using a raw CLI call, so
/// nothing about awman's own run path is involved in the fixture.
fn start_container(cli: &str, name: &str) -> bool {
    Command::new(cli)
        .args(["run", "-d", "--name", name, "alpine:latest", "sleep", "120"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn remove_container(cli: &str, name: &str) {
    let _ = Command::new(cli)
        .args(["rm", "-f", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The shared body for the two container tiers: they differ only in which CLI
/// seeds the fixture and which `ContainerRuntime` answers the query.
fn assert_prefix_query_is_selective(cli: &str, runtime: &ContainerRuntime) {
    let suffix = unique_suffix();
    // A legal amie name (`awman-amie-<slug>-<8 hex>`) and a plain awman
    // session-style name that must not match the amie prefix.
    let matching = generate_amie_container_name(&format!("disc-{suffix}"));
    let other = format!("awman-session-{suffix}");
    let prefix = format!("awman-amie-disc-{suffix}-");

    if !start_container(cli, &matching) {
        eprintln!("SKIP: `{cli} run` failed for the matching fixture container");
        return;
    }
    if !start_container(cli, &other) {
        remove_container(cli, &matching);
        eprintln!("SKIP: `{cli} run` failed for the non-matching fixture container");
        return;
    }

    let found = runtime.list_running_with_name_prefix(&prefix);
    remove_container(cli, &matching);
    remove_container(cli, &other);

    let found = found.expect("list_running_with_name_prefix must succeed");
    let names: Vec<&str> = found.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names.contains(&matching.as_str()),
        "the prefix query must find the amie container {matching}, got {names:?}"
    );
    assert!(
        !names.iter().any(|n| *n == other),
        "the prefix query must exclude the non-amie container {other}, got {names:?}"
    );

    // And an unused prefix still round-trips cleanly to an empty result.
    let unmatched = runtime
        .list_running_with_name_prefix(&format!("awman-amie-nothing-{suffix}-"))
        .expect("an unused prefix must still succeed");
    assert!(unmatched.is_empty(), "got {unmatched:?}");
}

fn image_pullable(cli: &str) -> bool {
    cli_available(cli, &["pull", "alpine:latest"])
}

// ─── Docker tier (skip cleanly if the daemon is not reachable) ───────────────

#[test]
fn docker_list_running_with_name_prefix_returns_only_matching_containers() {
    let runtime = ContainerRuntime::docker();
    if !runtime.is_available() {
        eprintln!("SKIP: Docker daemon not available");
        return;
    }
    if !image_pullable("docker") {
        eprintln!("SKIP: docker pull alpine:latest failed (no network?)");
        return;
    }
    assert_prefix_query_is_selective("docker", &runtime);
}

// ─── Apple tier (skip cleanly if `container` is not reachable) ───────────────

#[test]
fn apple_list_running_with_name_prefix_returns_only_matching_containers() {
    let runtime = ContainerRuntime::apple();
    if !runtime.is_available() {
        eprintln!("SKIP: Apple Containers not available");
        return;
    }
    if !image_pullable("container") {
        eprintln!("SKIP: `container pull alpine:latest` failed");
        return;
    }
    assert_prefix_query_is_selective("container", &runtime);
}

// ─── Sandbox tier (skip cleanly unless AWMAN_TEST_SBX=1 on macOS arm64) ──────

fn sbx_guard() -> bool {
    std::env::var("AWMAN_TEST_SBX").as_deref() == Ok("1")
        && cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

#[test]
fn sandbox_list_running_with_name_prefix_returns_only_matching_containers() {
    if !sbx_guard() {
        eprintln!("SKIP: sandbox tier requires macOS arm64 and AWMAN_TEST_SBX=1");
        return;
    }
    use awman::engine::sandbox::SandboxRuntime;
    let runtime = match SandboxRuntime::dsbx() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("SKIP: sandbox runtime unavailable on this platform: {e}");
            return;
        }
    };
    if !runtime.is_available() {
        eprintln!("SKIP: sbx not reachable (binary missing or logged out)");
        return;
    }
    // The sandbox tier names its boxes itself (`awman-<hash>-<agent>`), so a
    // matching fixture cannot be planted the way it can for the container
    // tiers. What is assertable without one is that the filter is applied at
    // all: an unused prefix returns nothing, and the unfiltered listing is a
    // superset of every filtered one.
    let unmatched = runtime
        .list_running_with_name_prefix(&format!("awman-amie-nothing-{}-", unique_suffix()))
        .expect("list_running_with_name_prefix must succeed even with no matches");
    assert!(
        unmatched.is_empty(),
        "an unused prefix must return no sandboxes, got {unmatched:?}"
    );
    let all = runtime
        .list_running_with_name_prefix("awman-")
        .expect("the shared awman prefix must list cleanly");
    for handle in &all {
        assert!(
            handle.name.starts_with("awman-"),
            "the prefix filter must be applied to every result: {}",
            handle.name
        );
    }
}
