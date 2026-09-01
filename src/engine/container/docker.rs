//! Docker backend — `pub(super)`. Concrete type is invisible outside
//! `src/engine/container/`.
//!
//! Builds a `docker run` argv from `ResolvedContainerOptions`, spawns the
//! subprocess, and captures the exit code.
//!
//! All container I/O is mediated through the `AgentIo` channels
//! provided by the frontend. When the frontend provides PTY fields
//! (`initial_size`/`resize` are `Some`), the engine opens a PTY via
//! `portable-pty`. Otherwise it uses `Stdio::piped()`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, AgentStats, ExecutionBackend,
};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::instance::{handle_now, ContainerId};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::error::EngineError;

/// Docker label applied to every amux-spawned container so `list_running`
/// can filter to ours.
const AWMAN_LABEL: &str = "awman=true";

#[derive(Debug, Default)]
pub(super) struct DockerBackend;

impl DockerBackend {
    pub(super) fn new() -> Self {
        Self
    }

    /// Probe whether the docker daemon is reachable. Returns `false` quietly
    /// when the binary is missing, the daemon is down, or the probe times out.
    pub(super) fn is_available() -> bool {
        let child = Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => {
                super::runtime::wait_with_timeout(child, std::time::Duration::from_secs(10))
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }
}

impl ContainerBackend for DockerBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        let image = options
            .image
            .clone()
            .ok_or_else(|| EngineError::MissingRequiredOption("Image".into()))?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        Ok(Box::new(DockerContainerInstance {
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
        }))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        // Query by label AND by name prefix so old-amux containers (which may
        // lack the label) are included. Results from all queries are merged and
        // deduplicated by container ID.
        let format = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";
        let queries: &[&[&str]] = &[
            &["ps", "--filter", "label=awman=true", "--format", format],
            &["ps", "--filter", "name=awman-", "--format", format],
        ];

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut handles: Vec<AgentHandle> = Vec::new();

        for args in queries {
            let output = Command::new("docker")
                .args(*args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let output = match output {
                Ok(o) if o.status.success() => o,
                // Docker missing or query failed: skip this filter, try next.
                _ => continue,
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() < 4 {
                    continue;
                }
                let id = parts[0].to_string();
                if !seen.insert(id.clone()) {
                    continue; // already added from a previous query
                }
                let name = parts[1].to_string();
                let image_tag = parts[2].to_string();
                let created = parts[3];
                // Docker's "CreatedAt" format is locale-formatted; fall back to
                // now() when parsing fails — better to surface the row than drop it.
                let started_at =
                    chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                handles.push(AgentHandle {
                    id,
                    image_tag,
                    name,
                    started_at,
                });
            }
        }

        Ok(handles)
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        let format = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";
        let queries: &[&[&str]] = &[
            &["ps", "--filter", "label=awman=true", "--format", format],
            &["ps", "--filter", "name=awman-", "--format", format],
        ];

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut handles: Vec<AgentHandle> = Vec::new();

        for args in queries {
            let output = Command::new("docker")
                .args(*args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let output = match output {
                Ok(o) if o.status.success() => o,
                _ => continue,
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() < 4 {
                    continue;
                }
                let id = parts[0].to_string();
                if !seen.insert(id.clone()) {
                    continue;
                }
                let name = parts[1].to_string();
                if id.is_empty() && name.is_empty() {
                    continue;
                }
                let image_tag = parts[2].to_string();
                let created = parts[3];
                let started_at =
                    chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                handles.push(AgentHandle {
                    id,
                    image_tag,
                    name,
                    started_at,
                });
            }
        }

        Ok(handles)
    }

    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        // Two-query deduplicated approach mirroring `list_running_all`: query
        // by the awman label and by the legacy `awman-` name prefix. Each query
        // adds `status=exited` and `status=dead` filters (OR'd within the same
        // filter type) so only stopped containers are returned — running or
        // paused containers are never included.
        let format = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";
        let queries: &[&[&str]] = &[
            &[
                "ps",
                "-a",
                "--filter",
                "label=awman=true",
                "--filter",
                "status=exited",
                "--filter",
                "status=dead",
                "--format",
                format,
            ],
            &[
                "ps",
                "-a",
                "--filter",
                "name=awman-",
                "--filter",
                "status=exited",
                "--filter",
                "status=dead",
                "--format",
                format,
            ],
        ];

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut handles: Vec<AgentHandle> = Vec::new();

        for args in queries {
            let output = Command::new("docker")
                .args(*args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let output = match output {
                Ok(o) if o.status.success() => o,
                _ => continue,
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() < 4 {
                    continue;
                }
                let id = parts[0].to_string();
                if id.is_empty() {
                    continue;
                }
                if !seen.insert(id.clone()) {
                    continue;
                }
                let name = parts[1].to_string();
                let image_tag = parts[2].to_string();
                let created = parts[3];
                let started_at =
                    chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                handles.push(AgentHandle {
                    id,
                    image_tag,
                    name,
                    started_at,
                });
            }
        }

        Ok(handles)
    }

    fn list_dangling_images(
        &self,
    ) -> Result<Vec<crate::engine::container::runtime::ContainerImageInfo>, EngineError> {
        use crate::engine::container::runtime::ContainerImageInfo;
        let format = "{{.ID}}\t{{.Repository}}:{{.Tag}}\t{{.Size}}";
        let output = Command::new("docker")
            .args([
                "images",
                "--filter",
                "label=awman=true",
                "--filter",
                "dangling=true",
                "--format",
                format,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            // Docker missing or query failed: return an empty list. Callers use
            // `is_available()` to decide whether Docker is reachable at all.
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut images: Vec<ContainerImageInfo> = Vec::new();
        for line in stdout.lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let id = parts[0].to_string();
            if id.is_empty() {
                continue;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            images.push(ContainerImageInfo {
                id,
                repo_tag: parts[1].to_string(),
                size: parts[2].to_string(),
            });
        }
        Ok(images)
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        let output = Command::new("docker")
            .args([
                "stats",
                "--no-stream",
                "--format",
                "{{.Name}}|{{.CPUPerc}}|{{.MemUsage}}",
                &handle.name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    EngineError::ContainerRuntimeUnavailable {
                        binary: "docker".into(),
                    }
                } else {
                    EngineError::Container(format!("docker stats: {e}"))
                }
            })?;
        if !output.status.success() {
            return Err(EngineError::Container(format!(
                "docker stats failed for container {}",
                handle.name
            )));
        }
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        parse_stats_line(&line, &handle.name)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        // Best-effort: stop, then rm. A nonzero exit (already gone) is fine.
        let _ = Command::new("docker")
            .args(["stop", &handle.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("docker")
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
        Ok(Box::new(AttachInstance {
            handle: handle.clone(),
        }))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        let format = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";
        let output = Command::new("docker")
            .args([
                "ps",
                "--filter",
                &format!("name={prefix}"),
                "--format",
                format,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            // Docker missing or query failed: report nothing found rather than
            // erroring — the caller treats an empty list as "no such agents".
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut handles: Vec<AgentHandle> = Vec::new();
        for line in stdout.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() < 4 {
                continue;
            }
            let id = parts[0].to_string();
            let name = parts[1].to_string();
            if id.is_empty() && name.is_empty() {
                continue;
            }
            let image_tag = parts[2].to_string();
            let created = parts[3];
            let started_at = chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            handles.push(AgentHandle {
                id,
                image_tag,
                name,
                started_at,
            });
        }
        Ok(handles)
    }

    fn name(&self) -> &'static str {
        "docker"
    }

    fn image_home_dir(&self, tag: &str) -> Option<String> {
        // Print one env entry per line so we can scan for `HOME=…` without
        // parsing JSON. `docker image inspect` exits 0 even when User/Env are
        // empty; the format expansion just produces nothing then.
        let output = Command::new("docker")
            .args([
                "image",
                "inspect",
                "--format",
                "{{range .Config.Env}}{{println .}}{{end}}",
                tag,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(rest) = line.strip_prefix("HOME=") {
                let v = rest.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    }
}

struct DockerContainerInstance {
    id: ContainerId,
    name: ContainerName,
    image: ImageRef,
    options: ResolvedContainerOptions,
}

impl AgentInstance for DockerContainerInstance {
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

        // Read per-frontend timeouts before draining `take_io`,
        // which leaves the frontend in a state where any further calls are
        // implementation-defined.
        let grace_timeout = frontend.grace_timeout();
        let stuck_timeout = frontend.stuck_timeout();
        let io = frontend.take_io();

        let bridge_cfg = bridge_config_for(&self.name, grace_timeout, stuck_timeout);

        // PTY path: frontend requested interactive PTY bridging.
        if io.initial_size.is_some() {
            return spawn_pty_bridged_docker(
                self, io, argv, seeded, started_at, handle, bridge_cfg,
            );
        }

        // ACP path: persistent piped stdio (no PTY). Unlike the one-shot piped
        // path below, the stdin channel is kept open for the whole session so
        // the ACP driver can carry a full bidirectional JSON-RPC exchange.
        if self.options.acp {
            return spawn_piped_interactive_docker(
                self, io, argv, seeded, started_at, handle, bridge_cfg,
            );
        }

        // Piped path: non-interactive or no PTY.
        spawn_piped_docker(self, io, argv, seeded, started_at, handle, bridge_cfg)
    }
}

/// Build a `BridgeConfig` for this container, including a cancel callback
/// that runs `docker stop <name>` so the startup-grace detector can kill a
/// container that never produced output. We construct the same `docker stop`
/// invocation the backend's `cancel_handle` would issue.
fn bridge_config_for(
    name: &ContainerName,
    grace_timeout: std::time::Duration,
    stuck_timeout: std::time::Duration,
) -> crate::engine::container::io_bridge::BridgeConfig {
    let container_name = name.0.clone();
    let cancel: crate::engine::container::io_bridge::CancelFn = std::sync::Arc::new(move || {
        let _ = Command::new("docker")
            .args(["stop", &container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    crate::engine::container::io_bridge::BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: std::time::Duration::ZERO,
        cancel_on_grace_expired: Some(cancel),
        output_tail: std::sync::Arc::new(
            crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
        ),
    }
}

/// Spawn `docker run -it` via `portable-pty` and bridge the PTY master to
/// the frontend's `AgentIo` channels via the shared I/O bridge.
fn spawn_pty_bridged_docker(
    instance: Box<DockerContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    _seeded: Option<String>,
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

    let mut cmd = CommandBuilder::new("docker");
    for arg in &argv {
        cmd.arg(arg);
    }
    // Agent credentials are passed as name-only `-e KEY` in argv; set their
    // values on the docker child's environment so the CLI resolves them
    // without the secret ever touching the argument vector.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn docker via pty: {e}")))?;

    // Interactive PTY runs pass the seeded prompt as a CLI positional arg
    // (appended by `build_run_argv`), so it must NOT also be written to stdin.
    // Writing it here would cause the PTY to echo the prompt text into the
    // terminal output, painting it over the TUI before the agent starts.

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let backend = DockerExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `docker run` with piped stdio and bridge through `AgentIo`.
fn spawn_piped_docker(
    instance: Box<DockerContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = Command::new("docker");
    cmd.args(&argv);
    // Agent credentials are passed as name-only `-e KEY` in argv; set their
    // values on the docker child's environment so the CLI resolves them
    // without the secret ever touching the argument vector.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "docker".into(),
            }
        } else {
            EngineError::Container(format!("spawn docker: {e}"))
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
    // child's stdin pipe. Without this, an agent that probes stdin for EOF
    // would hang waiting for input that will never come.
    // `try_inject_stdin` falls back to launching a fresh container — which
    // is the correct behaviour for a non-interactive run that has already
    // consumed its single prompt.
    drop(bridge.stdin_injector);

    let backend = DockerExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        container_name: instance.name.0.clone(),
        started_at,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `docker run -i` (no PTY) with piped stdio for an ACP session and
/// bridge through `AgentIo`.
///
/// This is the persistent-piped sibling of [`spawn_piped_docker`]. The one
/// difference — and the whole point — is that it does **not**
/// `drop(bridge.stdin_injector)`: the stdin channel is retained on the
/// `DockerExecution` for the session's entire lifetime, exactly as
/// [`spawn_pty_bridged_docker`] keeps its PTY master alive. That keeps the
/// container's stdin pipe open so the ACP driver can write JSON-RPC request
/// lines (via `try_inject_stdin`) across a full bidirectional exchange,
/// instead of the one-write-then-EOF an ordinary non-interactive run performs.
///
/// No seeded prompt is written to stdin here: an ACP session delivers its
/// prompt over the JSON-RPC channel (`session/prompt`), so writing raw text to
/// stdin would corrupt the newline-delimited JSON-RPC framing. `_seeded` is
/// accepted only to keep the signature parallel with `spawn_piped_docker`.
///
/// Security: identical wiring to `spawn_piped_docker` — the bytes ride the
/// stdio pipes `-i` already wires up. No ports, no `--network`, no new mounts
/// (see `aspec/architecture/security.md`).
fn spawn_piped_interactive_docker(
    instance: Box<DockerContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    _seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    spawn_piped_interactive_docker_with_bin(
        instance,
        io,
        argv,
        started_at,
        handle,
        bridge_cfg,
        std::path::Path::new("docker"),
    )
}

fn spawn_piped_interactive_docker_with_bin(
    instance: Box<DockerContainerInstance>,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
    cli_bin: &std::path::Path,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = Command::new(cli_bin);
    cmd.args(&argv);
    // Agent credentials are passed as name-only `-e KEY` in argv; set their
    // values on the docker child's environment so the CLI resolves them
    // without the secret ever touching the argument vector.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "docker".into(),
            }
        } else {
            EngineError::Container(format!("spawn docker: {e}"))
        }
    })?;

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);

    // Persistent-piped (ACP) path: KEEP the stdin_injector alive (do NOT drop
    // it, unlike `spawn_piped_docker`). Retaining the sender both enables
    // `try_inject_stdin` and prevents the writer task from ever seeing EOF, so
    // the container's stdin pipe stays open for the whole JSON-RPC session.
    let backend = DockerExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

struct DockerExecution {
    /// Set when running with piped stdio.
    child: Option<std::process::Child>,
    /// Set when running PTY-bridged. `portable_pty::Child` has its own wait
    /// API and cannot be unified with `std::process::Child`.
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Master PTY end. Held alive so the resize task can call into it and so
    /// the PTY isn't torn down before the child has finished writing.
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    /// Stdin sender — same channel the writer task drains. Used by
    /// `try_inject_stdin` so workflow `ContinueInCurrentContainer` can push a
    /// fresh prompt into the running container.
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    container_name: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for DockerExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        // PTY-bridged path: wait on the portable-pty child.
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait docker (pty): {e}")))?;
            // Drop the master AFTER the child exits so the reader thread sees
            // EOF cleanly.
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }

        // Piped path: wait on std::process::Child.
        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Container("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait docker: {e}")))?;

        // After interactive runs, docker may leave stdio in O_NONBLOCK mode
        // on Unix. Restore it.
        #[cfg(unix)]
        clear_stdio_nonblocking();

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
        // Best-effort: docker stop will SIGTERM then SIGKILL after a grace
        // period. Then docker rm to clean up.
        let _ = Command::new("docker")
            .args(["stop", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("docker")
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
                let _ = Command::new("docker")
                    .args(["stop", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = Command::new("docker")
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

/// `BridgeConfig` for an attach session. Identical to `bridge_config_for`
/// except `cancel_on_grace_expired` is `None`: the grace-expiry cancel issues
/// `docker stop <name>`, which would be wrong for a container this process
/// does not own. An attach session's own exit is authoritative.
pub(super) fn attach_bridge_config(
    grace_timeout: std::time::Duration,
    stuck_timeout: std::time::Duration,
) -> crate::engine::container::io_bridge::BridgeConfig {
    crate::engine::container::io_bridge::BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: std::time::Duration::ZERO,
        cancel_on_grace_expired: None,
        output_tail: std::sync::Arc::new(
            crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
        ),
    }
}

/// Kill only the local runtime attach client process. Never
/// touches the target container — attach must never stop a container another
/// process owns.
pub(super) fn kill_local_exec(pid: Option<u32>) {
    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(not(unix))]
        {
            let _ = pid; // best-effort no-op; the container is never touched
        }
    }
}

/// Configured-but-not-running attach handle. `run_with_frontend` opens a
/// `docker attach` session to PID 1's existing terminal and bridges it through
/// the same PTY/piped machinery a fresh `docker run` uses.
struct AttachInstance {
    handle: AgentHandle,
}

impl AgentInstance for AttachInstance {
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

        // `docker attach` reconnects to the container's primary process (the
        // agent launched by `docker run -it`), rather than creating a sibling
        // shell with `docker exec`. The outer bridge PTY carries terminal size
        // and resize signals to the attach client.
        let argv = vec![
            "attach".to_string(),
            "--sig-proxy=false".to_string(),
            handle.id.clone(),
        ];
        if io.initial_size.is_some() {
            return spawn_pty_bridged_attach(io, argv, started_at, handle, bridge_cfg);
        }

        spawn_piped_attach(io, argv, started_at, handle, bridge_cfg)
    }
}

/// Spawn `docker attach` via `portable-pty` and bridge the PTY master to the
/// frontend's `AgentIo`. Mirrors `spawn_pty_bridged_docker` but substitutes
/// the exec argv and produces an `AttachExecution`.
fn spawn_pty_bridged_attach(
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

    let mut cmd = CommandBuilder::new("docker");
    for arg in &argv {
        cmd.arg(arg);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn docker attach via pty: {e}")))?;
    let child_pid = child.process_id();

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let backend = AttachExecution {
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

/// Spawn `docker attach` with piped stdio and bridge through `AgentIo`. Mirrors
/// `spawn_piped_docker` but substitutes the exec argv and produces an
/// `AttachExecution`.
fn spawn_piped_attach(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = Command::new("docker");
    cmd.args(&argv);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "docker".into(),
            }
        } else {
            EngineError::Container(format!("spawn docker attach: {e}"))
        }
    })?;
    let child_pid = Some(child.id());

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);
    // Non-interactive attach: close the child's stdin after the bridge wires up
    // so a shell that reads to EOF exits cleanly (matches the run piped path).
    drop(bridge.stdin_injector);

    let backend = AttachExecution {
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

/// Execution backend for an attach session. Identical to `DockerExecution`
/// except cancellation kills only the local `docker attach` client — never
/// `docker stop <name>`, because the target container belongs to another
/// process.
struct AttachExecution {
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// PID of the local `docker attach` client, captured at spawn.
    child_pid: Option<u32>,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for AttachExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait docker attach (pty): {e}")))?;
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
            .map_err(|e| EngineError::Container(format!("wait docker attach: {e}")))?;

        #[cfg(unix)]
        clear_stdio_nonblocking();

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
        kill_local_exec(self.child_pid);
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let pid = self.child_pid;
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                kill_local_exec(pid);
                Ok(())
            },
        ))
    }
}

/// Translate `ResolvedContainerOptions` into a `docker run` argv (without the
/// leading `docker` binary).
pub(super) fn build_run_argv(
    name: &ContainerName,
    image: &ImageRef,
    options: &ResolvedContainerOptions,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into()];
    if options.remove_on_exit {
        args.push("--rm".into());
    }
    if options.acp {
        // ACP launch: piped stdio, never a PTY — even for an interactive run.
        // A newline-delimited JSON-RPC 2.0 channel must never pass through a
        // PTY's cooked-mode / echo / ANSI layer, so we allocate `-i` (stdin
        // attached, no `-t`). This adds NO new host exposure — no ports, no
        // `--network`, no new mounts (aspec/architecture/security.md): the
        // JSON-RPC bytes ride the exact stdio pipes `-i` already wires up.
        args.push("-i".into());
    } else if options.interactive {
        // Interactive runs always allocate a PTY. When a seeded prompt is also
        // present, the prompt is appended as a positional argv arg below so the
        // agent receives it without piping; stdin stays inherited for the user.
        args.push("-it".into());
    } else if options.seeded_prompt.is_some() {
        // Non-interactive with a seeded prompt: pipe stdin so we can write the
        // prompt, then close it. No PTY — allocating one fails when there is no
        // host TTY (ENOTTY / "Inappropriate ioctl for device").
        args.push("-i".into());
    }

    args.push("--name".into());
    args.push(name.0.clone());

    // Standard awman label so `list_running` can filter.
    args.push("--label".into());
    args.push(AWMAN_LABEL.into());

    // Caller-supplied labels — emitted one `--label key=value` each, in the
    // order they were ingested, immediately after the hardcoded `awman=true`.
    // Lets `list_running` attribute containers to a specific awman session
    // (`awman.session=<id>`) and squad mark its background agents
    // (`awman.squad.task=<name>`). These are for human `docker ps`
    // inspection only — awman never reads a label back.
    for (key, value) in &options.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}"));
    }

    // Working dir.
    if let Some(wd) = &options.working_dir {
        args.push("-w".into());
        args.push(wd.display().to_string());
    }

    // Overlays / volume mounts.
    for overlay in &options.overlays {
        args.push("-v".into());
        let suffix = match overlay.permission {
            crate::engine::container::options::OverlayPermission::ReadOnly => ":ro",
            crate::engine::container::options::OverlayPermission::ReadWrite => "",
        };
        args.push(format!(
            "{}:{}{}",
            overlay.host_path.display(),
            overlay.container_path.display(),
            suffix,
        ));
    }

    // Env passthrough — only emit when the variable is set on the host.
    for envvar in &options.env_passthrough {
        if let Ok(value) = std::env::var(&envvar.0) {
            args.push("-e".into());
            args.push(format!("{}={}", envvar.0, value));
        }
    }
    // Env literals.
    for lit in &options.env_literal {
        args.push("-e".into());
        args.push(format!("{}={}", lit.key, lit.value));
    }
    // Agent credentials are env-vars by another name. Emit the NAME ONLY
    // (`-e KEY`); the value is set on the spawned CLI child's own environment
    // (see the spawn paths below) and the container-runtime CLI resolves a
    // name-only `-e` from its process env. This keeps the secret value out of
    // the argument vector, so it never appears in `ps` / `/proc/<pid>/cmdline`
    // while the client process runs. Both the docker CLI and the Apple
    // `container` CLI (which shares this argv builder) support this form.
    for (k, _v) in &options.agent_credentials {
        args.push("-e".into());
        args.push(k.clone());
    }

    // Allow Docker socket: mount and add docker group.
    if options.allow_docker {
        let socket = docker_socket_path();
        let s = socket.to_string_lossy().to_string();
        #[cfg(target_os = "windows")]
        {
            args.push("--mount".into());
            args.push(format!("type=npipe,source={},target={}", s, s));
        }
        #[cfg(not(target_os = "windows"))]
        {
            args.push("-v".into());
            args.push(format!("{s}:{s}"));
            // Add the host's docker group GID so the container user can talk
            // to the daemon. Best-effort: skip when the group can't be found.
            if let Some(gid) = host_docker_group_gid() {
                args.push("--group-add".into());
                args.push(gid.to_string());
            }
        }
    }

    // Container CPU/memory limits.
    if let Some(cpu) = options.cpu {
        args.push("--cpus".into());
        args.push(format!("{}", cpu.0));
    }
    if let Some(mem) = options.memory {
        args.push("--memory".into());
        args.push(format!("{}m", mem.0));
    }

    // System prompt file: Docker-side mount (before the image arg).
    if let Some((host_path, container_path, _flag)) = &options.system_prompt_file {
        args.push("-v".into());
        args.push(format!(
            "{}:{}:ro",
            host_path.display(),
            container_path.display(),
        ));
    }

    // System prompt env file: Docker-side mount + env var (before the image arg).
    if let Some((env_var, host_path, container_path)) = &options.system_prompt_env_file {
        args.push("-v".into());
        args.push(format!(
            "{}:{}:ro",
            host_path.display(),
            container_path.display(),
        ));
        args.push("-e".into());
        args.push(format!("{}={}", env_var, container_path.display()));
    }

    // The image is the final positional arg.
    args.push(image.0.clone());

    // Entrypoint / agent argv at the end.
    if let Some(ep) = &options.entrypoint {
        for piece in &ep.0 {
            args.push(piece.clone());
        }
    }

    // ACP launches are complete at the entrypoint (`cline --acp`). Everything an
    // ACP agent needs — the prompt, model, tool policy, permission decisions — is
    // delivered over the JSON-RPC 2.0 channel (`session/*`), never as argv. Any
    // stdio-mode flag or positional appended past the entrypoint would be handed
    // to the agent as raw argv and corrupt the launch (e.g. `cline --acp task`,
    // `cline --acp task --yolo`). So emit nothing after the entrypoint for ACP.
    if options.acp {
        return args;
    }

    // Mode flags appended to the agent argv.
    if let Some(flag) = &options.non_interactive_flag {
        // Some agents take a sub-command (e.g. "run") rather than a flag.
        args.push(flag.clone());
    }
    // Per-agent mode flags (yolo, auto, plan) — appended as literal args.
    for flag in &options.agent_mode_flags {
        args.push(flag.clone());
    }

    // Disallowed tools.
    if !options.disallowed_tools.is_empty() {
        if let Some(flag_name) = options.disallowed_tools_flag.as_deref() {
            args.push(flag_name.to_string());
            args.push(options.disallowed_tools.join(","));
        }
    }
    // Allowed tools.
    if !options.allowed_tools.is_empty() {
        if let Some(flag_name) = options.allowed_tools_flag.as_deref() {
            args.push(flag_name.to_string());
            args.push(options.allowed_tools.join(","));
        }
    }

    // Model flag.
    if let Some(model) = &options.model {
        match model {
            crate::engine::container::options::ModelFlagForm::Argument(name) => {
                args.push("--model".into());
                args.push(name.clone());
            }
            crate::engine::container::options::ModelFlagForm::Shorthand(s) => {
                args.push(s.clone());
            }
        }
    }

    // System prompt file: agent-side CLI flag (after the image/entrypoint).
    if let Some((_host_path, container_path, flag)) = &options.system_prompt_file {
        args.push(flag.clone());
        args.push(container_path.display().to_string());
    }

    // System prompt inline: pass the flag + text as agent argv.
    if let Some((flag, text)) = &options.system_prompt_inline {
        args.push(flag.clone());
        args.push(text.clone());
    }

    // Agent add-dir flags.
    for (flag, container_path) in &options.agent_add_dirs {
        args.push(flag.clone());
        args.push(container_path.display().to_string());
    }

    // Interactive + seeded prompt: deliver the prompt so the agent receives it
    // as its initial task. Most agents take it as the final positional arg;
    // agents that declare an `interactive_seed_flag` (e.g. opencode `--prompt`)
    // take it as a flag pair, because their bare positional means something else
    // (opencode treats it as a project directory and `open()`s it → ENAMETOOLONG).
    // Stdin stays inherited. Non-interactive + seeded prompt is handled via
    // stdin piping at spawn time.
    //
    // ACP is excluded: an ACP session delivers its prompt over the JSON-RPC
    // channel (`session/prompt`), never as argv — appending it here would pass
    // raw prompt text to `cline --acp` as a positional and corrupt the launch.
    if options.interactive && !options.acp {
        if let Some(prompt) = &options.seeded_prompt {
            if let Some(flag) = &options.interactive_seed_flag {
                args.push(flag.clone());
            }
            args.push(prompt.clone());
        }
    }

    args
}

fn parse_stats_line(line: &str, fallback_name: &str) -> Result<AgentStats, EngineError> {
    // Format: "name|cpu%|memUsage" e.g. "awman-x|2.31%|123MiB / 4GiB"
    let parts: Vec<&str> = line.splitn(3, '|').collect();
    if parts.len() < 3 {
        return Err(EngineError::Container(format!(
            "unparseable docker stats line: {line:?}"
        )));
    }
    let name = if parts[0].is_empty() {
        fallback_name.to_string()
    } else {
        parts[0].to_string()
    };
    let cpu_percent = parse_cpu_percent(parts[1]);
    let memory_mb = parse_memory_mb(parts[2]);
    Ok(AgentStats {
        name,
        cpu_percent,
        memory_mb,
    })
}

fn parse_cpu_percent(s: &str) -> f64 {
    s.trim().trim_end_matches('%').parse::<f64>().unwrap_or(0.0)
}

fn parse_memory_mb(s: &str) -> f64 {
    let raw = s.split('/').next().unwrap_or("").trim();
    let (num_str, unit) = raw
        .find(|c: char| c.is_alphabetic())
        .map(|i| raw.split_at(i))
        .unwrap_or((raw, ""));
    let n: f64 = num_str.trim().parse().unwrap_or(0.0);
    match unit.trim().to_ascii_uppercase().as_str() {
        "B" => n / 1_048_576.0,
        "KB" | "KIB" => n / 1024.0,
        "MB" | "MIB" => n,
        "GB" | "GIB" => n * 1024.0,
        "TB" | "TIB" => n * 1024.0 * 1024.0,
        _ => n,
    }
}

fn docker_socket_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"\\.\pipe\docker_engine")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/var/run/docker.sock")
    }
}

/// Best-effort lookup of the host's `docker` group GID by parsing
/// `/etc/group`. Returns `None` when the group is absent (rootless docker,
/// macOS Docker Desktop where the socket is owned by the user, etc.).
#[cfg(not(target_os = "windows"))]
fn host_docker_group_gid() -> Option<u32> {
    let contents = std::fs::read_to_string("/etc/group").ok()?;
    for line in contents.lines() {
        // Format: name:passwd:gid:user_list
        let mut parts = line.splitn(4, ':');
        let name = parts.next()?;
        if name != "docker" {
            continue;
        }
        let _passwd = parts.next()?;
        let gid_str = parts.next()?;
        if let Ok(gid) = gid_str.parse::<u32>() {
            return Some(gid);
        }
    }
    None
}

/// Clear O_NONBLOCK from stdin/stdout/stderr after an interactive Docker run.
///
/// Docker's `-it` flag sets O_NONBLOCK on the inherited stdio fds and does not
/// reliably restore them on exit. Without this, the next read/write returns
/// EAGAIN ("Resource temporarily unavailable", os error 35 on macOS / 11 on
/// Linux).
#[cfg(unix)]
fn clear_stdio_nonblocking() {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    fn clear_fd(fd: impl std::os::fd::AsFd) {
        if let Ok(flags) = fcntl(&fd, FcntlArg::F_GETFL) {
            let mut o = OFlag::from_bits_truncate(flags);
            if o.contains(OFlag::O_NONBLOCK) {
                o.remove(OFlag::O_NONBLOCK);
                let _ = fcntl(&fd, FcntlArg::F_SETFL(o));
            }
        }
    }
    clear_fd(std::io::stdin());
    clear_fd(std::io::stdout());
    clear_fd(std::io::stderr());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{
        ContainerOption, EnvVar, ImageRef, OverlayPermission, OverlaySpec, ResolvedContainerOptions,
    };
    use std::path::PathBuf;

    fn resolve(opts: Vec<ContainerOption>) -> ResolvedContainerOptions {
        ResolvedContainerOptions::resolve(opts).unwrap()
    }

    #[test]
    fn build_run_argv_minimal() {
        let resolved = resolve(vec![ContainerOption::Image(ImageRef::new("img:latest"))]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"--rm".to_string()));
        assert!(argv.contains(&"--label".to_string()));
        assert!(argv.contains(&AWMAN_LABEL.to_string()));
        // Image is the final positional arg.
        assert_eq!(argv.last().map(String::as_str), Some("img:latest"));
    }

    /// WI 0101 §2.2: `ContainerOption::SessionLabel` became the general
    /// `Label { key, value }`. `awman=true` must still come first, then exactly
    /// one `--label k=v` per accumulated entry, in ingest order.
    #[test]
    fn build_run_argv_emits_the_awman_label_then_one_flag_per_supplied_label() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: "sid-1".into(),
            },
            ContainerOption::Label {
                key: "awman.squad.task".into(),
                value: "issue-triage".into(),
            },
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        let labels: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(i, a)| a.as_str() == "--label" && *i + 1 < argv.len())
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert_eq!(
            labels,
            vec![
                &AWMAN_LABEL.to_string(),
                &"awman.session=sid-1".to_string(),
                &"awman.squad.task=issue-triage".to_string(),
            ],
            "argv was: {argv:?}"
        );
        assert_eq!(
            argv.iter().filter(|a| a.as_str() == "--label").count(),
            3,
            "one --label flag per label, no more"
        );
    }

    /// Regression guard for the pre-refactor behaviour: a lone session label
    /// still renders exactly as it did when it had its own dedicated field.
    #[test]
    fn build_run_argv_renders_a_lone_session_label_exactly_as_before_the_refactor() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: "abc-123".into(),
            },
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        let awman = argv
            .iter()
            .position(|a| a == AWMAN_LABEL)
            .expect("the hardcoded awman=true label must be present");
        assert_eq!(argv[awman + 1], "--label");
        assert_eq!(
            argv[awman + 2],
            "awman.session=abc-123",
            "the session label must immediately follow awman=true"
        );
    }

    #[test]
    fn build_run_argv_emits_only_the_awman_label_when_no_labels_are_supplied() {
        let resolved = resolve(vec![ContainerOption::Image(ImageRef::new("img:latest"))]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert_eq!(argv.iter().filter(|a| a.as_str() == "--label").count(), 1);
    }

    #[test]
    fn build_run_argv_includes_overlay_volumes() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Overlay(OverlaySpec {
                host_path: PathBuf::from("/h/p"),
                container_path: PathBuf::from("/c/p"),
                permission: OverlayPermission::ReadOnly,
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-v" && w[1] == "/h/p:/c/p:ro"));
    }

    #[test]
    fn build_run_argv_env_passthrough_only_when_set() {
        std::env::set_var("AWMAN_TEST_ENV_DOCKER", "v1");
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar("AWMAN_TEST_ENV_DOCKER".into())),
            ContainerOption::EnvPassthrough(EnvVar("AWMAN_TEST_NEVER_SET_DOCKER".into())),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(argv.contains(&"AWMAN_TEST_ENV_DOCKER=v1".to_string()));
        assert!(!argv
            .iter()
            .any(|a| a.contains("AWMAN_TEST_NEVER_SET_DOCKER")));
        std::env::remove_var("AWMAN_TEST_ENV_DOCKER");
    }

    #[test]
    fn build_run_argv_allow_docker_mounts_socket() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::AllowDocker(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(argv
            .iter()
            .any(|a| a.contains("docker.sock") || a.contains("docker_engine")));
    }

    #[test]
    fn build_run_argv_entrypoint_appended_after_image() {
        use crate::engine::container::options::Entrypoint;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Entrypoint(Entrypoint::new(["claude", "--print"])),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        let img_pos = argv.iter().position(|a| a == "img:latest").unwrap();
        let claude_pos = argv.iter().position(|a| a == "claude").unwrap();
        let print_pos = argv.iter().position(|a| a == "--print").unwrap();
        assert!(img_pos < claude_pos, "entrypoint must come after image");
        assert!(claude_pos < print_pos, "entrypoint args must be in order");
    }

    #[test]
    fn build_run_argv_rw_overlay_has_no_ro_suffix() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Overlay(OverlaySpec {
                host_path: PathBuf::from("/h/rw"),
                container_path: PathBuf::from("/c/rw"),
                permission: OverlayPermission::ReadWrite,
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        let vol_arg = argv
            .windows(2)
            .find(|w| w[0] == "-v")
            .map(|w| w[1].clone())
            .unwrap();
        assert_eq!(
            vol_arg, "/h/rw:/c/rw",
            "RW overlay must not have :ro suffix"
        );
    }

    #[test]
    fn build_run_argv_env_literal_always_included() {
        use crate::engine::container::options::EnvLiteral;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvLiteral(EnvLiteral {
                key: "MY_KEY".into(),
                value: "my_value".into(),
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-e" && w[1] == "MY_KEY=my_value"));
    }

    #[test]
    fn build_run_argv_seeded_prompt_adds_i_flag_not_it() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::SeededPrompt("hello world".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.contains(&"-i".to_string()),
            "seeded prompt needs -i flag"
        );
        assert!(
            !argv.contains(&"-it".to_string()),
            "seeded prompt must NOT add -it"
        );
    }

    #[test]
    fn build_run_argv_seeded_prompt_with_interactive_uses_it_and_positional_arg() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::SeededPrompt("hello".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.contains(&"-it".to_string()),
            "interactive+seeded must use -it for PTY"
        );
        assert!(
            !argv.contains(&"-i".to_string()),
            "interactive+seeded must NOT use bare -i"
        );
        assert_eq!(
            argv.last().map(|s| s.as_str()),
            Some("hello"),
            "seeded prompt must be last positional arg"
        );
    }

    #[test]
    fn build_run_argv_interactive_seed_flag_delivers_prompt_via_flag_not_positional() {
        // opencode-shaped delivery: a seed flag is present, so the prompt must
        // be emitted as `<flag> <text>` rather than a bare positional (which
        // opencode would treat as a project dir and open() → ENAMETOOLONG).
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::SeededPrompt("do the task".into()),
            ContainerOption::InteractiveSeedFlag("--prompt".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--prompt" && w[1] == "do the task"),
            "seed flag must deliver the prompt as `--prompt <text>`; got {argv:?}"
        );
        // The prompt must never appear as a lone positional immediately after
        // the image (which is what breaks opencode).
        let img_idx = argv.iter().position(|a| a == "img:latest").unwrap();
        assert_ne!(
            argv.get(img_idx + 1).map(|s| s.as_str()),
            Some("do the task"),
            "prompt must not be a bare positional after the image; got {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_interactive_acp_emits_i_not_it() {
        // ACP framing must never pass through a PTY, so an interactive ACP run
        // allocates `-i` (piped stdin, no `-t`) rather than `-it`.
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::Acp(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.contains(&"-i".to_string()),
            "interactive ACP run needs -i; argv: {argv:?}"
        );
        assert!(
            !argv.contains(&"-it".to_string()),
            "interactive ACP run must NOT allocate a PTY via -it; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_non_interactive_acp_still_emits_i() {
        // A headless (`-n`) ACP run still needs stdin piped open for the
        // bidirectional JSON-RPC exchange.
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Acp(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.contains(&"-i".to_string()),
            "non-interactive ACP run still needs -i; argv: {argv:?}"
        );
        assert!(
            !argv.contains(&"-it".to_string()),
            "ACP run must never use -it; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_acp_does_not_deliver_seeded_prompt_as_positional() {
        // The ACP prompt travels over JSON-RPC (`session/prompt`), never argv.
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::Acp(true),
            ContainerOption::SeededPrompt("do the task".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            !argv.iter().any(|a| a == "do the task"),
            "ACP must not append the seeded prompt as an argv positional; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_acp_emits_nothing_after_the_entrypoint() {
        // Regression for the "corrupted ACP argv" blocker: an ACP launch that
        // also carries a non-interactive subcommand flag and agent mode flags
        // (as every `exec workflow` / `--non-interactive` / `--yolo` ACP launch
        // does) must NOT graft any of them onto `cline --acp` — the ACP argv is
        // complete at the entrypoint. Before the fix this produced
        // `cline --acp task --yolo`, silently corrupting the JSON-RPC launch.
        use crate::engine::container::options::Entrypoint;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Entrypoint(Entrypoint::new(["cline", "--acp"])),
            ContainerOption::Acp(true),
            ContainerOption::NonInteractivePrintFlag("task".into()),
            ContainerOption::AgentModeFlags(vec!["--yolo".into()]),
            ContainerOption::SeededPrompt("do the task".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        let img_pos = argv.iter().position(|a| a == "img:latest").unwrap();
        assert_eq!(
            &argv[img_pos + 1..],
            &["cline".to_string(), "--acp".to_string()],
            "ACP argv must end exactly at the entrypoint, with no stdio flags \
             (task/--yolo/prompt) appended; argv: {argv:?}"
        );
    }

    /// Regression guard for the security constraint (aspec/architecture/
    /// security.md): the ACP argv path must introduce NO new host exposure —
    /// no published ports, no host/custom `--network`, no added mounts beyond
    /// what a non-ACP run of the same options would already emit.
    #[test]
    fn build_run_argv_acp_introduces_no_ports_or_network_flags() {
        let acp = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::Acp(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &acp,
        );
        for banned in ["-p", "--publish", "--network", "--net", "--add-host"] {
            assert!(
                !argv.iter().any(|a| a == banned),
                "ACP argv must never contain {banned}; argv: {argv:?}"
            );
        }

        // And the ACP flag must not add any `-v` mount that a plain
        // interactive run of the same options wouldn't already produce.
        let plain = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
        ]);
        let plain_argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &plain,
        );
        let count_v = |v: &[String]| v.iter().filter(|a| a.as_str() == "-v").count();
        assert_eq!(
            count_v(&argv),
            count_v(&plain_argv),
            "ACP must not introduce any new -v mount; acp: {argv:?} plain: {plain_argv:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_piped_interactive_keeps_stdin_injector_after_seeded_write() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::mpsc;

        // A stand-in for `docker run`: keep stdin open long enough for this
        // test to distinguish the persistent ACP path from the one-shot
        // `spawn_piped_docker` path, which drops its injector after seeding.
        let tmp = tempfile::tempdir().unwrap();
        let fake_cli = tmp.path().join("fake-docker");
        std::fs::write(&fake_cli, "#!/bin/sh\nsleep 1\n").unwrap();
        let mut permissions = std::fs::metadata(&fake_cli).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_cli, permissions).unwrap();

        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        let io = crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };
        let image = ImageRef::new("img:latest");
        let name = ContainerName::new("acp-stdin-test");
        let instance = Box::new(DockerContainerInstance {
            id: ContainerId::new(name.as_str()),
            name: name.clone(),
            image: image.clone(),
            options: resolve(vec![
                ContainerOption::Image(image.clone()),
                ContainerOption::Interactive(true),
                ContainerOption::Acp(true),
            ]),
        });
        let handle = handle_now(&instance.id, &name, &image);
        let bridge_cfg = crate::engine::container::io_bridge::BridgeConfig {
            grace_timeout: Duration::from_secs(60),
            stuck_timeout: Duration::from_secs(60),
            container_start_delay: Duration::ZERO,
            cancel_on_grace_expired: None,
            output_tail: Arc::new(
                crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
            ),
        };

        let mut execution = spawn_piped_interactive_docker_with_bin(
            instance,
            io,
            vec!["run".into(), "-i".into(), "img:latest".into()],
            chrono::Utc::now(),
            handle,
            bridge_cfg,
            &fake_cli,
        )
        .unwrap();

        // The optional seed is accepted by the production wrapper for
        // signature parity; a later injection must still work in ACP mode.
        assert!(
            execution.try_inject_stdin(b"seeded\n").unwrap(),
            "ACP spawn must retain stdin_injector after the optional seed"
        );
        let exit = execution.wait().await.unwrap();
        assert_eq!(exit.exit_code, 0);
    }

    #[test]
    fn build_run_argv_interactive_adds_it_flag() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.contains(&"-it".to_string()),
            "interactive run needs -it flag"
        );
    }

    #[test]
    fn build_run_argv_working_dir_adds_w_flag() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::WorkingDir(PathBuf::from("/workspace")),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-w" && w[1] == "/workspace"));
    }

    #[test]
    fn build_run_argv_container_name_present_in_argv() {
        use crate::engine::container::options::ContainerName as CN;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Name(CN::new("my-container")),
        ]);
        let argv = build_run_argv(
            &CN::new("my-container"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--name" && w[1] == "my-container"),
            "container name must appear as --name <name>"
        );
    }

    #[test]
    fn build_run_argv_yolo_does_not_add_extra_docker_flag() {
        // Yolo mode is encoded in the agent's overlay settings (settings.json),
        // NOT as a docker run flag. The argv builder must not add any flag for it.
        use crate::engine::container::options::YoloMode;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Yolo(YoloMode::Enabled),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            !argv.iter().any(|a| a.contains("yolo")),
            "yolo must not add a docker flag"
        );
        assert!(
            !argv.iter().any(|a| a.contains("bypass")),
            "yolo must not add a bypass flag"
        );
    }

    #[test]
    fn image_home_dir_returns_none_for_unknown_image() {
        // Missing image: `docker image inspect` exits non-zero so the helper
        // must surface `None` rather than panic. Works regardless of whether
        // the docker daemon is reachable, because `Command::output` errors
        // collapse to `None` too.
        let backend = DockerBackend::new();
        let bogus = "awman-test-image-that-does-not-exist:tag-xyz123";
        assert!(backend.image_home_dir(bogus).is_none());
    }

    #[test]
    fn parse_memory_mb_handles_various_units() {
        assert!((parse_memory_mb("200MiB / 1GiB") - 200.0).abs() < 0.1);
        assert!((parse_memory_mb("1.5GiB / 4GiB") - 1536.0).abs() < 0.1);
    }

    #[test]
    fn parse_cpu_percent_strips_percent() {
        assert!((parse_cpu_percent("5.23%") - 5.23).abs() < 0.001);
    }

    // ── WI-0098 Finding A: agent credential values must never enter argv ──────
    //
    // `build_run_argv` emits credentials as the NAME-ONLY form `-e KEY`; the
    // value is set on the spawned CLI child's own environment (see the spawn
    // paths) so nothing secret is visible via `ps` / `/proc/<pid>/cmdline`.
    // These tests assert the value is absent from the built argv while still
    // being carried out-of-band on the resolved options (the source the spawn
    // code feeds to `Command::env`). `build_run_argv` is shared verbatim by the
    // Apple backend, so this coverage applies to both container backends.

    fn credential_opts(pairs: &[(&str, &str)]) -> ResolvedContainerOptions {
        resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::AgentCredentials {
                env_vars: pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
        ])
    }

    #[test]
    fn build_run_argv_agent_credentials_use_name_only_form() {
        let resolved = credential_opts(&[("ANTHROPIC_API_KEY", "sk-secret-value")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        // The name-only `-e KEY` pair must be present.
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "ANTHROPIC_API_KEY"),
            "credential must be emitted as name-only `-e ANTHROPIC_API_KEY`; argv: {argv:?}"
        );
        // The secret value must appear nowhere in argv, in any form.
        assert!(
            !argv.iter().any(|a| a.contains("sk-secret-value")),
            "credential VALUE must never appear in argv; argv: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|a| a == "ANTHROPIC_API_KEY=sk-secret-value"),
            "the `KEY=VALUE` argv form must not be used for credentials; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_credential_value_with_equals_stays_out_of_argv() {
        // A value containing `=` must survive the env-inheritance path and must
        // never leak into argv.
        let resolved = credential_opts(&[("TOKEN", "a=b=c")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "-e" && w[1] == "TOKEN"),
            "credential name must still be present as `-e TOKEN`; argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("a=b=c")),
            "credential value containing `=` must not appear in argv; argv: {argv:?}"
        );
        // Carried out-of-band, verbatim, for the child-process env map.
        assert_eq!(
            resolved.agent_credentials,
            vec![("TOKEN".to_string(), "a=b=c".to_string())],
            "the value must be preserved verbatim on the options for `Command::env`"
        );
    }

    #[test]
    fn build_run_argv_credential_value_with_newline_stays_out_of_argv() {
        let resolved = credential_opts(&[("MULTILINE", "line1\nline2")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "-e" && w[1] == "MULTILINE"),
            "credential name must be present as `-e MULTILINE`; argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains('\n')),
            "no argv element may contain a newline from the credential value; argv: {argv:?}"
        );
        assert_eq!(
            resolved.agent_credentials,
            vec![("MULTILINE".to_string(), "line1\nline2".to_string())],
            "the newline-bearing value must be preserved verbatim for `Command::env`"
        );
    }

    #[test]
    fn build_run_argv_multiple_credentials_all_name_only() {
        let resolved = credential_opts(&[("KEY_A", "aaa"), ("KEY_B", "bbb")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        for name in ["KEY_A", "KEY_B"] {
            assert!(
                argv.windows(2).any(|w| w[0] == "-e" && w[1] == name),
                "{name} must be emitted name-only; argv: {argv:?}"
            );
        }
        for value in ["aaa", "bbb"] {
            assert!(
                !argv.iter().any(|a| a.contains(value)),
                "credential value {value} must never appear in argv; argv: {argv:?}"
            );
        }
    }
}
