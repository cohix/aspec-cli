//! Apple Containers backend — `pub(super)`. Same shape as Docker; the Apple
//! `container` CLI is a near-drop-in replacement (it shares the docker `run`
//! / `list` / `stats` / `stop` surface).

use std::process::{Command, Stdio};

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, AgentStats, ExecutionBackend,
};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::docker::build_run_argv;
use crate::engine::container::instance::{handle_now, ContainerId};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::credential_refresh::{register_container_leases, CredentialLease};
use crate::engine::error::EngineError;

const AWMAN_LABEL: &str = "awman=true";

/// Extract the container name from an Apple Containers JSON row.
///
/// Apple's schema uses `configuration.id` as the container name/identifier
/// (there is no separate short hex ID). Falls back to Docker-style fields
/// for forward-compatibility.
fn extract_apple_name(row: &serde_json::Value) -> String {
    if let Some(id) = row
        .get("configuration")
        .and_then(|c| c.get("id"))
        .and_then(|v| v.as_str())
    {
        return id.to_string();
    }
    let val = row
        .get("Names")
        .or_else(|| row.get("Name"))
        .or_else(|| row.get("name"));
    match val {
        Some(v) if v.is_array() => v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or_default()
            .to_string(),
        Some(v) => v
            .as_str()
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or_default()
            .to_string(),
        None => String::new(),
    }
}

/// Extract the image reference from an Apple Containers JSON row.
///
/// Apple stores the image as `configuration.image` (an object); we serialize
/// it for display. Falls back to Docker-style string `Image`/`image` fields.
fn extract_apple_image(row: &serde_json::Value) -> String {
    if let Some(img_obj) = row.get("configuration").and_then(|c| c.get("image")) {
        if let Some(s) = img_obj.as_str() {
            return s.to_string();
        }
        // Apple Containers stores the image name in descriptor.annotations.
        if let Some(s) = img_obj
            .get("descriptor")
            .and_then(|d| d.get("annotations"))
            .and_then(|a| a.get("com.apple.containerization.image.name"))
            .and_then(|v| v.as_str())
        {
            return s.to_string();
        }
        // Apple Containers uses "reference" for a full OCI image reference string.
        if let Some(s) = img_obj.get("reference").and_then(|v| v.as_str()) {
            return s.to_string();
        }
        if let Some(repo) = img_obj.get("repository").and_then(|v| v.as_str()) {
            return match img_obj.get("tag").and_then(|v| v.as_str()) {
                Some(tag) if !tag.is_empty() => format!("{repo}:{tag}"),
                _ => repo.to_string(),
            };
        }
        return serde_json::to_string(img_obj).unwrap_or_default();
    }
    row.get("Image")
        .or_else(|| row.get("image"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Extract the started-at timestamp from an Apple Containers JSON row.
///
/// Apple uses `startedDate` (float epoch seconds). Falls back to
/// Docker-style `CreatedAt`/`Created` RFC3339 strings.
fn extract_apple_started_at(row: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    if let Some(ts) = row.get("startedDate").and_then(|v| v.as_f64()) {
        let secs = ts as i64;
        let nanos = ((ts - secs as f64) * 1_000_000_000.0) as u32;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
            return dt;
        }
    }
    row.get("CreatedAt")
        .or_else(|| row.get("Created"))
        .or_else(|| row.get("created"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now)
}

/// Check whether the row represents a running container.
///
/// Apple uses a `status` field: "running" | "stopped" | "stopping" | "unknown".
/// If absent (Docker-style output from `ps`), assume running.
fn is_apple_running(row: &serde_json::Value) -> bool {
    match row.get("status").and_then(|v| v.as_str()) {
        Some(s) => s == "running",
        None => true,
    }
}

/// Check whether the row's name matches awman container patterns.
fn is_awman_container(name: &str) -> bool {
    name.starts_with("awman-") || name.contains("nanoclaw")
}

/// Parse the JSON output of `container list --format json` into container
/// handles, filtering for running awman containers. When `name_prefix` is
/// `Some`, additionally require the container name to start with it — Apple
/// has no server-side name filter, so name-prefix discovery is done here,
/// client-side.
fn parse_apple_list_output(stdout: &str, name_prefix: Option<&str>) -> Vec<AgentHandle> {
    let arr: Result<Vec<serde_json::Value>, _> = serde_json::from_str(stdout);
    let rows: Vec<serde_json::Value> = match arr {
        Ok(v) => v,
        Err(_) => stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    };
    let mut handles = Vec::new();
    for row in rows {
        if !is_apple_running(&row) {
            continue;
        }
        let name = extract_apple_name(&row);
        if !is_awman_container(&name) {
            continue;
        }
        if let Some(prefix) = name_prefix {
            if !name.starts_with(prefix) {
                continue;
            }
        }
        let id = name.clone();
        let image_tag = extract_apple_image(&row);
        let started_at = extract_apple_started_at(&row);
        if id.is_empty() && name.is_empty() {
            continue;
        }
        handles.push(AgentHandle {
            id,
            image_tag,
            name,
            started_at,
        });
    }
    handles
}

#[derive(Debug, Default)]
pub(super) struct AppleBackend;

impl AppleBackend {
    pub(super) fn new() -> Self {
        Self
    }
}

impl ContainerBackend for AppleBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        let image = options.image.clone().ok_or_else(|| {
            EngineError::ConflictingOptions("missing required Image option".into())
        })?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        // Register a refresh lease for every file-delivered credential BEFORE
        // the container is built and its child spawned — the single choke point
        // (INV-6). No-op for every launch that carries no such credential.
        let leases = register_container_leases(&options, &name.0);
        Ok(Box::new(AppleContainerInstance {
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
            leases,
        }))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_apple_list_output(&stdout, None))
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_apple_list_output(&stdout, None))
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        let take_sample = |name: &str| -> Result<(u64, u64), EngineError> {
            let out = Command::new("container")
                .args(["stats", "--no-stream", "--format", "json", name])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        EngineError::ContainerRuntimeUnavailable {
                            binary: "container".into(),
                        }
                    } else {
                        EngineError::Container(format!("container stats: {e}"))
                    }
                })?;
            if !out.status.success() {
                return Err(EngineError::Container(format!(
                    "container stats failed for {}",
                    name
                )));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let value: serde_json::Value = serde_json::from_str(stdout.trim())
                .or_else(|_| {
                    stdout
                        .lines()
                        .next()
                        .ok_or_else(|| serde_json::Error::io(std::io::Error::other("empty")))
                        .and_then(serde_json::from_str)
                })
                .map_err(|e| {
                    EngineError::Container(format!("unparseable container stats output: {e}"))
                })?;
            let entry = match &value {
                serde_json::Value::Array(arr) => {
                    arr.first().cloned().unwrap_or(serde_json::Value::Null)
                }
                _ => value,
            };
            let cpu = entry
                .get("cpuUsageUsec")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mem = entry
                .get("memoryUsageBytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Ok((cpu, mem))
        };

        let (cpu1, _) = take_sample(&handle.name)?;
        let t0 = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (cpu2, mem) = take_sample(&handle.name)?;
        let elapsed_usec = t0.elapsed().as_micros() as u64;

        let cpu_delta = cpu2.saturating_sub(cpu1);
        let cpu_percent = if elapsed_usec > 0 {
            (cpu_delta as f64 / elapsed_usec as f64) * 100.0
        } else {
            0.0
        };
        let memory_mb = (mem as f64) / (1024.0 * 1024.0);

        Ok(AgentStats {
            name: handle.name.clone(),
            cpu_percent,
            memory_mb,
        })
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        let _ = Command::new("container")
            .args(["stop", &handle.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("container")
            .args(["rm", &handle.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }

    fn exec_args(
        &self,
        container_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        let mut args = vec!["exec".to_string(), "-it".to_string()];
        args.extend(["-w".to_string(), working_dir.to_string()]);
        for (k, v) in env_vars {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push(container_id.to_string());
        args.extend(entrypoint.iter().map(|s| s.to_string()));
        args
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        Ok(Box::new(AppleAttachInstance {
            handle: handle.clone(),
        }))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        // Apple has no server-side name filter; list everything and filter by
        // the prefix client-side.
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_apple_list_output(&stdout, Some(prefix)))
    }

    fn name(&self) -> &'static str {
        "apple-containers"
    }

    fn image_home_dir(&self, tag: &str) -> Option<String> {
        // `container image inspect` emits a JSON array of variants; the env
        // list lives at `[0].variants[*].config.config.Env`. We pick the
        // first variant whose env contains a non-empty `HOME=…` entry, which
        // matches the runtime selection for single-platform images.
        let output = Command::new("container")
            .args(["image", "inspect", tag])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: serde_json::Value = serde_json::from_str(&stdout).ok()?;
        let variants = arr.get(0)?.get("variants")?.as_array()?;
        for variant in variants {
            let env = variant
                .get("config")
                .and_then(|c| c.get("config"))
                .and_then(|c| c.get("Env"))
                .and_then(|v| v.as_array());
            let Some(env) = env else { continue };
            for entry in env {
                if let Some(rest) = entry.as_str().and_then(|s| s.strip_prefix("HOME=")) {
                    let v = rest.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
        None
    }
}

struct AppleContainerInstance {
    id: ContainerId,
    name: ContainerName,
    image: ImageRef,
    options: ResolvedContainerOptions,
    /// Credential-refresh leases, one per file-delivered credential. Moved into
    /// the `AppleExecution` by each spawn fn so the lease's lifetime brackets
    /// the child process exactly (this instance box is dropped before the spawn
    /// fn returns).
    leases: Vec<CredentialLease>,
}

impl AgentInstance for AppleContainerInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.id.0.clone(),
            name: self.name.0.clone(),
            image: self.image.0.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        let argv = build_run_argv(&self.name, &self.image, &self.options);
        let started_at = chrono::Utc::now();
        let seeded = self.options.seeded_prompt.clone();
        let handle = handle_now(&self.id, &self.name, &self.image);

        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: self.name.0.clone(),
            },
        );

        // Read per-frontend timeouts before draining `take_io`.
        let grace_timeout = frontend.grace_timeout();
        let stuck_timeout = frontend.stuck_timeout();
        let io = frontend.take_io();

        let bridge_cfg = bridge_config_for(&self.name, grace_timeout, stuck_timeout);

        // PTY-bridged path
        if io.initial_size.is_some() {
            return spawn_pty_bridged_apple(self, io, argv, seeded, started_at, handle, bridge_cfg);
        }

        // ACP path: persistent piped stdio (no PTY), stdin kept open for the
        // whole JSON-RPC session. Parity with the Docker backend is mandatory —
        // shipping ACP only under Docker would be a platform-inconsistent
        // regression (WI 0104 Edge Case Considerations).
        if self.options.acp {
            return spawn_piped_interactive_apple(
                self, io, argv, seeded, started_at, handle, bridge_cfg,
            );
        }

        // Piped path
        spawn_piped_apple(self, io, argv, seeded, started_at, handle, bridge_cfg)
    }
}

/// Build a `BridgeConfig` for this container. The cancel callback runs
/// `container stop <name>` so the startup-grace detector can kill a
/// container that never produced output.
fn bridge_config_for(
    name: &ContainerName,
    grace_timeout: std::time::Duration,
    stuck_timeout: std::time::Duration,
) -> crate::engine::container::io_bridge::BridgeConfig {
    let container_name = name.0.clone();
    let cancel: crate::engine::container::io_bridge::CancelFn = std::sync::Arc::new(move || {
        let _ = Command::new("container")
            .args(["stop", &container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    crate::engine::container::io_bridge::BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: crate::engine::container::timing::APPLE_CONTAINER_START_DELAY,
        cancel_on_grace_expired: Some(cancel),
        output_tail: std::sync::Arc::new(
            crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
        ),
    }
}

/// Spawn the Apple `container run -it` binary via `portable-pty` and bridge
/// the PTY master to the frontend's `AgentIo` channels via the shared
/// I/O bridge.
fn spawn_pty_bridged_apple(
    mut instance: Box<AppleContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    _seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    // Move the credential leases out of the instance (dropped before this fn
    // returns) and into the execution backend, so each lease brackets the child
    // process it credentials.
    let leases = std::mem::take(&mut instance.leases);

    let (cols, rows) = io.initial_size.expect("PTY path requires initial_size");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| EngineError::Container(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new("container");
    for arg in &argv {
        cmd.arg(arg);
    }
    // Agent credentials are passed as name-only `-e KEY` in argv; set their
    // values on the `container` child's environment so the CLI resolves them
    // without the secret ever touching the argument vector.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }

    // INV-6: a credentialed container must hold its lease before the child is
    // spawned. This closes the startup race where a token could expire between
    // staging and the container's first request.
    debug_assert!(
        instance.options.refreshable_credentials.is_empty()
            || !leases.is_empty()
            || crate::engine::credential_refresh::global().is_none(),
        "file-delivered credential spawned without a lease"
    );

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn container via pty: {e}")))?;

    // Interactive PTY runs pass the seeded prompt as a CLI positional arg
    // (appended by `build_run_argv`), so it must NOT also be written to stdin.
    // Writing it here would cause the PTY to echo the prompt text into the
    // terminal output, painting it over the TUI before the agent starts.

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let backend = AppleExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
        leases,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `container run` with piped stdio and bridge through `AgentIo`.
fn spawn_piped_apple(
    mut instance: Box<AppleContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = Command::new("container");
    cmd.args(&argv);
    // Agent credentials are passed as name-only `-e KEY` in argv; set their
    // values on the `container` child's environment so the CLI resolves them
    // without the secret ever touching the argument vector.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Move leases into the backend; assert one exists before spawn (INV-6).
    let leases = std::mem::take(&mut instance.leases);
    debug_assert!(
        instance.options.refreshable_credentials.is_empty()
            || !leases.is_empty()
            || crate::engine::credential_refresh::global().is_none(),
        "file-delivered credential spawned without a lease"
    );

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "container".into(),
            }
        } else {
            EngineError::Container(format!("spawn container: {e}"))
        }
    })?;

    // Write seeded prompt into stdin channel before the writer task starts.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\n".to_vec());
    }

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);

    // Non-interactive (piped) path: drop the engine's stdin_injector so the
    // writer task sees EOF after draining the seeded prompt and closes the
    // child's stdin pipe. See docker.rs::spawn_piped_docker for rationale.
    drop(bridge.stdin_injector);

    let backend = AppleExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        container_name: instance.name.0.clone(),
        started_at,
        leases,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `container run -i` (no PTY) with piped stdio for an ACP session and
/// bridge through `AgentIo`.
///
/// The Apple-backend sibling of `docker.rs::spawn_piped_interactive_docker`,
/// and the persistent-piped counterpart of [`spawn_piped_apple`]. The one
/// difference from `spawn_piped_apple` — and the whole point — is that it does
/// **not** `drop(bridge.stdin_injector)`: the stdin channel is retained on the
/// `AppleExecution` for the session's entire lifetime, exactly as
/// [`spawn_pty_bridged_apple`] keeps its PTY master alive. That keeps the
/// container's stdin pipe open so the ACP driver can write JSON-RPC request
/// lines (via `try_inject_stdin`) across a full bidirectional exchange.
///
/// No seeded prompt is written to stdin: an ACP session delivers its prompt
/// over the JSON-RPC channel (`session/prompt`), so writing raw text to stdin
/// would corrupt the newline-delimited framing. `_seeded` is accepted only to
/// keep the signature parallel with `spawn_piped_apple`.
///
/// Security: identical wiring to `spawn_piped_apple` — the JSON-RPC bytes ride
/// the stdio pipes `-i` already wires up. No ports, no `--network`, no new
/// mounts (see `aspec/architecture/security.md`).
fn spawn_piped_interactive_apple(
    mut instance: Box<AppleContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    _seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = Command::new("container");
    cmd.args(&argv);
    // Agent credentials are passed as name-only `-e KEY` in argv; set their
    // values on the `container` child's environment so the CLI resolves them
    // without the secret ever touching the argument vector.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Move leases into the backend; assert one exists before spawn (INV-6).
    let leases = std::mem::take(&mut instance.leases);
    debug_assert!(
        instance.options.refreshable_credentials.is_empty()
            || !leases.is_empty()
            || crate::engine::credential_refresh::global().is_none(),
        "file-delivered credential spawned without a lease"
    );

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "container".into(),
            }
        } else {
            EngineError::Container(format!("spawn container: {e}"))
        }
    })?;

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);

    // Persistent-piped (ACP) path: KEEP the stdin_injector alive (do NOT drop
    // it, unlike `spawn_piped_apple`). Retaining the sender both enables
    // `try_inject_stdin` and prevents the writer task from ever seeing EOF, so
    // the container's stdin pipe stays open for the whole JSON-RPC session.
    let backend = AppleExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
        leases,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

struct AppleExecution {
    /// Set when running with piped stdio.
    child: Option<std::process::Child>,
    /// Set when running PTY-bridged via portable-pty.
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Held alive so the resize task and PTY writer keep working until exit.
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    /// Sender side of the stdin channel — used by `try_inject_stdin` to push
    /// a workflow continue-in-current prompt into the running PTY.
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    container_name: String,
    started_at: chrono::DateTime<chrono::Utc>,
    /// Credential-refresh leases held for the container's whole life. Dropped
    /// (deregistering each lease) when this backend is consumed by
    /// `wait_blocking` on child exit, or when the execution is dropped unwaited.
    /// The monitor stops writing this container's staged file the moment these
    /// drop. Not read directly — held purely for its `Drop`.
    #[allow(dead_code)]
    leases: Vec<CredentialLease>,
}

impl ExecutionBackend for AppleExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        // PTY-bridged path: wait on the portable-pty child.
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait container (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }

        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Container("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait container: {e}")))?;
        let exit_code = status.code().unwrap_or(-1);
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Ok(AgentExitInfo {
            exit_code,
            signal,
            started_at: self.started_at,
            ended_at: chrono::Utc::now(),
        })
    }

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.stdin_injector {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        let _ = Command::new("container")
            .args(["stop", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("container")
            .args(["rm", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let name = self.container_name.clone();
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                let _ = Command::new("container")
                    .args(["stop", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = Command::new("container")
                    .args(["rm", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                Ok(())
            },
        ))
    }
}

// ─── Attach (re-attach into a foreign, already-running container) ───────────

/// Configured-but-not-running Apple attach handle. It reconnects to the
/// running container's primary process rather than execing a shell alongside
/// it, and never stops the target when the local client closes.
struct AppleAttachInstance {
    handle: AgentHandle,
}

impl AgentInstance for AppleAttachInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        use crate::engine::container::docker::attach_bridge_config;

        let started_at = chrono::Utc::now();
        let handle = self.handle.clone();

        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: handle.name.clone(),
            },
        );

        let grace_timeout = frontend.grace_timeout();
        let stuck_timeout = frontend.stuck_timeout();
        let io = frontend.take_io();
        let bridge_cfg = attach_bridge_config(grace_timeout, stuck_timeout);

        let argv = vec!["attach".to_string(), handle.id.clone()];

        if io.initial_size.is_some() {
            return spawn_pty_bridged_apple_attach(io, argv, started_at, handle, bridge_cfg);
        }

        spawn_piped_apple_attach(io, argv, started_at, handle, bridge_cfg)
    }
}

/// Spawn `container attach` via `portable-pty` and bridge through `AgentIo`.
fn spawn_pty_bridged_apple_attach(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let (cols, rows) = io.initial_size.expect("PTY path requires initial_size");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| EngineError::Container(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new("container");
    for arg in &argv {
        cmd.arg(arg);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn container attach via pty: {e}")))?;
    let child_pid = child.process_id();

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let backend = AppleAttachExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        child_pid,
        started_at,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `container attach` with piped stdio and bridge through `AgentIo`.
fn spawn_piped_apple_attach(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = Command::new("container");
    cmd.args(&argv);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "container".into(),
            }
        } else {
            EngineError::Container(format!("spawn container attach: {e}"))
        }
    })?;
    let child_pid = Some(child.id());

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);
    drop(bridge.stdin_injector);

    let backend = AppleAttachExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        child_pid,
        started_at,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Execution backend for an Apple attach session. Cancellation kills only the
/// local `container attach` client — never `container stop <name>`, because the
/// target container belongs to another process.
struct AppleAttachExecution {
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    child_pid: Option<u32>,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for AppleAttachExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait container attach (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }

        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Container("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait container attach: {e}")))?;
        let exit_code = status.code().unwrap_or(-1);
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Ok(AgentExitInfo {
            exit_code,
            signal,
            started_at: self.started_at,
            ended_at: chrono::Utc::now(),
        })
    }

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.stdin_injector {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        crate::engine::container::docker::kill_local_exec(self.child_pid);
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let pid = self.child_pid;
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                crate::engine::container::docker::kill_local_exec(pid);
                Ok(())
            },
        ))
    }
}

/// Parse a memory-usage string like `"123.4MiB"`, `"1.2GB"`, `"512KB"` into
/// megabytes. Unrecognized units fall back to assuming MB (consistent with
/// the legacy parser at `oldsrc/runtime/docker.rs`).
fn parse_memory_mb(s: &str) -> f64 {
    let trimmed = s.trim();
    let split_at = trimmed
        .find(|c: char| c.is_alphabetic())
        .unwrap_or(trimmed.len());
    let (num, unit) = trimmed.split_at(split_at);
    let value: f64 = num.parse().unwrap_or(0.0);
    let unit_norm: String = unit.trim().to_ascii_lowercase();
    let factor_to_mb: f64 = match unit_norm.as_str() {
        "b" => 1.0 / (1024.0 * 1024.0),
        "k" | "kb" | "kib" => 1.0 / 1024.0,
        "m" | "mb" | "mib" | "" => 1.0,
        "g" | "gb" | "gib" => 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0,
        _ => 1.0,
    };
    value * factor_to_mb
}

#[cfg(test)]
mod apple_tests {
    use super::*;

    #[test]
    fn parse_memory_mb_handles_common_units() {
        assert!((parse_memory_mb("128MiB") - 128.0).abs() < 0.001);
        assert!((parse_memory_mb("128MB") - 128.0).abs() < 0.001);
        assert!((parse_memory_mb("1.5GB") - 1536.0).abs() < 0.001);
        assert!((parse_memory_mb("512KB") - 0.5).abs() < 0.001);
        assert!((parse_memory_mb("1024B") - (1024.0 / (1024.0 * 1024.0))).abs() < 0.001);
        assert!((parse_memory_mb("64") - 64.0).abs() < 0.001);
    }

    #[test]
    fn parse_memory_mb_unknown_unit_assumes_mb() {
        assert!((parse_memory_mb("128wat") - 128.0).abs() < 0.001);
    }

    #[test]
    fn parse_apple_list_picks_up_running_awman_containers() {
        let json = r#"[
            {
                "status": "running",
                "configuration": {
                    "id": "awman-12345-999",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1715000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "awman-claws-controller",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1715000100.5
            },
            {
                "status": "stopped",
                "configuration": {
                    "id": "awman-old-stopped",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1714000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "unrelated-container",
                    "image": {"repository": "nginx", "tag": "latest"}
                },
                "startedDate": 1715000200.0
            }
        ]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].name, "awman-12345-999");
        assert_eq!(handles[0].id, "awman-12345-999");
        assert_eq!(handles[1].name, "awman-claws-controller");
    }

    #[test]
    fn parse_apple_list_handles_nanoclaw_containers() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "nanoclaw-worker-1",
                "image": {"repository": "awman/dev"}
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].name, "nanoclaw-worker-1");
    }

    #[test]
    fn parse_apple_list_empty_array() {
        let handles = parse_apple_list_output("[]", None);
        assert!(handles.is_empty());
    }

    #[test]
    fn parse_apple_list_skips_non_running() {
        let json = r#"[{
            "status": "stopping",
            "configuration": { "id": "awman-dying" },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert!(handles.is_empty());
    }

    #[test]
    fn extract_apple_image_formats_repo_and_tag() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"repository": "awman/dev", "tag": "latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_repo_only_without_tag() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": {"repository": "awman/dev"}}}"#)
                .unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev");
    }

    #[test]
    fn extract_apple_image_plain_string() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": "awman/dev:latest"}}"#).unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_reference_field() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"reference": "ghcr.io/awman/dev:latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "ghcr.io/awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_descriptor_annotations() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{
            "configuration": {
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "awman-myproj-claude:latest"
                        }
                    }
                }
            }
        }"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "awman-myproj-claude:latest");
    }

    #[test]
    fn parse_apple_list_formats_image_correctly() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "awman-test",
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "awman-myproj-claude:latest"
                        }
                    }
                }
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].image_tag, "awman-myproj-claude:latest");
    }

    #[test]
    fn image_home_dir_returns_none_for_unknown_image() {
        // `container image inspect` exits non-zero for an unknown tag, and
        // the helper must collapse that to `None` rather than panic — also
        // covers the case where the `container` CLI itself isn't installed.
        let backend = AppleBackend::new();
        let bogus = "awman-test-image-that-does-not-exist:tag-xyz123";
        assert!(backend.image_home_dir(bogus).is_none());
    }

    #[test]
    fn extract_apple_started_at_from_float() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"startedDate": 1715000000.5}"#).unwrap();
        let dt = extract_apple_started_at(&row);
        assert_eq!(dt.timestamp(), 1715000000);
    }
}
