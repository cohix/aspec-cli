//! `SandboxRuntime` — the sandbox-class `AgentRuntimeEngine` impl.
//!
//! Holds an `Arc<dyn SandboxBackend>`. The concrete driver is invisible
//! outside this module. Platform guards live in the constructors: a user on
//! an unsupported platform gets `BackendUnsupportedOnPlatform` from the
//! constructor and never reaches the backend.

use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::agent_runtime::execution::{AgentExitInfo, ExecutionBackend, StuckEvent};
use crate::engine::agent_runtime::{
    AgentExecution, AgentFrontend, AgentHandle, AgentHandlePreview, AgentInstance,
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::SandboxBackend;
use crate::engine::sandbox::dsbx::DSbxBackend;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Capabilities shared by sandbox-class runtimes: kit-declarative,
/// persistent, workspace-only mounts, private DinD per VM.
static SANDBOX_CAPABILITIES: Capabilities = Capabilities {
    arbitrary_env_vars: false,
    arbitrary_host_mounts: false,
    cpu_limits: false,
    per_resource_stats: false,
    persistent_lifecycle: true,
    kit_declarative: true,
    dind: DindSupport::Always,
    host_paths_visible: false,
    session_label_supported: false,
};

pub struct SandboxRuntime {
    backend: Arc<dyn SandboxBackend>,
}

impl SandboxRuntime {
    /// Construct with the Docker Sandbox (`sbx`) backend.
    ///
    /// Platform guards: Docker Sandboxes are not available on Linux, and not
    /// on Intel Macs. Erroring here (rather than from the first backend
    /// call) gives the user an actionable platform message up front.
    pub fn dsbx() -> Result<Self, EngineError> {
        if cfg!(target_os = "linux") {
            return Err(EngineError::BackendUnsupportedOnPlatform {
                backend: "docker-sbx-experimental".into(),
                platform: "linux — blocked until the Docker Sandboxes virtiofs \
                           file-creation bug (sbx-releases Issue #51) is fixed upstream"
                    .into(),
            });
        }
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            return Err(EngineError::BackendUnsupportedOnPlatform {
                backend: "docker-sbx-experimental".into(),
                platform: "macos (x86_64) — Docker Sandboxes requires Apple Silicon \
                           (arm64). Intel Macs are not supported"
                    .into(),
            });
        }
        Ok(Self {
            backend: Arc::new(DSbxBackend::new()),
        })
    }
}

impl AgentRuntimeEngine for SandboxRuntime {
    fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    fn display_name(&self) -> &'static str {
        match self.backend.name() {
            "docker-sbx-experimental" => "Docker Sandboxes (experimental)",
            _ => "Sandbox",
        }
    }

    fn capabilities(&self) -> &Capabilities {
        &SANDBOX_CAPABILITIES
    }

    fn is_available(&self) -> bool {
        // Probes `sbx ls` (per WI 0090): a missing binary and a logged-out
        // session both make the runtime unusable, and `sbx ls` fails for both.
        use std::process::Stdio;
        let child = std::process::Command::new(self.backend.cli_binary())
            .arg("ls")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => crate::engine::container::runtime::wait_with_timeout(
                child,
                std::time::Duration::from_secs(10),
            )
            .map(|s| s.success())
            .unwrap_or(false),
            Err(_) => false,
        }
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        match options {
            ResolvedAgentOptions::Sandbox(opts) => {
                Ok(Box::new(SandboxAgentInstance { options: opts }))
            }
            other => Err(EngineError::OptionVariantMismatch {
                runtime: self.runtime_name().to_string(),
                got: other.paradigm(),
            }),
        }
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        // Sandboxes have no session label; attribution is by name (WI 0090).
        self.backend.list_running()
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running()
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.backend.stats(handle)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }

    fn exec_args(
        &self,
        agent_id: &str,
        _working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        // `sbx exec -it [--env K=V…] <sandbox-name> <entrypoint…>`.
        //
        // `agent_id` carries the deterministic sandbox name for re-attach.
        // Per Phase 0 #3 (Issue #63), the caller passes COLUMNS/LINES through
        // `env_vars` so TUI apps inside the VM see a real terminal size; they
        // are emitted as `--env` like any other variable. The kit's default
        // working directory is the mounted workspace, so `working_dir` needs
        // no translation here.
        let mut args = vec!["exec".to_string(), "-it".to_string()];
        for (k, v) in env_vars {
            args.push("--env".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push(agent_id.to_string());
        args.extend(entrypoint.iter().map(|s| s.to_string()));
        args
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        Ok(Box::new(SandboxAttachInstance {
            handle: handle.clone(),
            cli_binary: self.backend.cli_binary(),
        }))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        // Sandboxes have no server-side name filter; list and filter by name.
        let mut running = self.backend.list_running()?;
        running.retain(|h| h.name.starts_with(prefix));
        Ok(running)
    }

    fn cli_binary(&self) -> &'static str {
        self.backend.cli_binary()
    }
}

/// Configured-but-not-running sandbox agent — the sandbox tier's half of the
/// two-step build/run pattern.
struct SandboxAgentInstance {
    options: ResolvedSandboxOptions,
}

impl AgentInstance for SandboxAgentInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        let name = self
            .options
            .sandbox_name
            .clone()
            .unwrap_or_else(|| self.options.agent_id.clone());
        AgentHandlePreview {
            id: name.clone(),
            name,
            // Sandboxes boot a kit/template rather than a local image; the
            // kit selector is the closest analogue.
            image: self.options.agent_id.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        // The interactive launch (session config, credential injection, kit
        // selection, PTY-bridged `sbx run`) lives in the dsbx driver.
        super::dsbx::run_interactive(self.options, frontend)
    }
}

// ─── Attach (re-attach into a foreign, already-running sandbox) ─────────────
//
// Sandbox attach exists for cross-tier `AgentRuntimeEngine` completeness and
// the tier-parity tests; amie itself refuses to run under the sandbox tier
// (see Part 5). The sandbox tier carries its own minimal PTY bridge here
// rather than reaching into the dsbx driver's private one, keeping the
// WI-0090 layering rule (no `src/engine/container/` import) intact.

/// Configured-but-not-running sandbox attach handle. `run_with_frontend`
/// opens an `sbx exec -it <name> bash` session and bridges it to the frontend.
struct SandboxAttachInstance {
    handle: AgentHandle,
    cli_binary: &'static str,
}

/// Build the `sbx exec` argv, mirroring `SandboxRuntime::exec_args`:
/// `exec -it [--env K=V…] <name> <entrypoint…>`.
fn sbx_attach_argv(name: &str, env_vars: &[(&str, &str)], entrypoint: &[&str]) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "-it".to_string()];
    for (k, v) in env_vars {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push(name.to_string());
    args.extend(entrypoint.iter().map(|s| s.to_string()));
    args
}

/// Kill only the local `sbx exec` client — never `sbx stop`, because attach
/// does not own the sandbox.
fn kill_local_sbx_exec(pid: Option<u32>) {
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
            let _ = pid;
        }
    }
}

impl AgentInstance for SandboxAttachInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};

        let started_at = chrono::Utc::now();
        let handle = self.handle.clone();

        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: handle.name.clone(),
            },
        );

        let io = frontend.take_io();
        let (stuck_tx, _) = tokio::sync::broadcast::channel::<StuckEvent>(8);
        let stuck_tx = std::sync::Arc::new(stuck_tx);

        // PTY path: pass the frontend's terminal size through as COLUMNS/LINES.
        if let Some((cols, rows)) = io.initial_size {
            let cols_s = cols.to_string();
            let rows_s = rows.to_string();
            let argv = sbx_attach_argv(
                &handle.id,
                &[("COLUMNS", &cols_s), ("LINES", &rows_s)],
                &["bash"],
            );
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| EngineError::Sandbox(format!("openpty: {e}")))?;
            let mut cmd = CommandBuilder::new(self.cli_binary);
            for arg in &argv {
                cmd.arg(arg);
            }
            let child = pair
                .slave
                .spawn_command(cmd)
                .map_err(|e| EngineError::Sandbox(format!("spawn sbx exec via pty: {e}")))?;
            let child_pid = child.process_id();
            let master = bridge_attach_pty(io, pair)?;
            let backend = SandboxAttachExecution {
                child: None,
                pty_child: Some(child),
                pty_master: Some(master),
                child_pid,
                started_at,
            };
            return Ok(AgentExecution::new(
                handle,
                Box::new(backend),
                stuck_tx,
                None,
            ));
        }

        // Piped path.
        let argv = sbx_attach_argv(&handle.id, &[], &["bash"]);
        let mut cmd = std::process::Command::new(self.cli_binary);
        cmd.args(&argv);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| EngineError::Sandbox(format!("spawn sbx exec: {e}")))?;
        let child_pid = Some(child.id());
        bridge_attach_piped(io, &mut child);
        let backend = SandboxAttachExecution {
            child: Some(child),
            pty_child: None,
            pty_master: None,
            child_pid,
            started_at,
        };
        Ok(AgentExecution::new(
            handle,
            Box::new(backend),
            stuck_tx,
            None,
        ))
    }
}

type SbxPtyMaster = std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

/// Wire a PTY master to the frontend's channels: reader thread (PTY→stdout),
/// writer task (stdin→PTY), resize task.
fn bridge_attach_pty(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    pair: portable_pty::PtyPair,
) -> Result<SbxPtyMaster, EngineError> {
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Sandbox(format!("clone sbx pty reader: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Sandbox(format!("take sbx pty writer: {e}")))?;

    let stdout_tx = io.stdout;
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut stdin_rx = io.stdin_rx;
    tokio::spawn(async move {
        use std::io::Write;
        while let Some(bytes) = stdin_rx.recv().await {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    let master_arc: SbxPtyMaster = std::sync::Arc::new(std::sync::Mutex::new(pair.master));
    if let Some(mut resize_rx) = io.resize {
        let master_for_resize = std::sync::Arc::clone(&master_arc);
        tokio::spawn(async move {
            use portable_pty::PtySize;
            while let Some((cols, rows)) = resize_rx.recv().await {
                if let Ok(master) = master_for_resize.lock() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        });
    }
    Ok(master_arc)
}

/// Wire a piped child's stdio to the frontend's channels.
fn bridge_attach_piped(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    child: &mut std::process::Child,
) {
    if let Some(child_stdout) = child.stdout.take() {
        let stdout_tx = io.stdout;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stdout;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    if let Some(child_stderr) = child.stderr.take() {
        let stderr_tx = io.stderr;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stderr;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stderr_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    if let Some(child_stdin) = child.stdin.take() {
        let mut stdin_rx = io.stdin_rx;
        tokio::spawn(async move {
            use std::io::Write;
            let mut writer = child_stdin;
            while let Some(bytes) = stdin_rx.recv().await {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }
}

/// Execution backend for a sandbox attach session. Cancellation kills only the
/// local `sbx exec` client — never `sbx stop`, because the target sandbox
/// belongs to another process.
struct SandboxAttachExecution {
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<SbxPtyMaster>,
    child_pid: Option<u32>,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for SandboxAttachExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Sandbox(format!("wait sbx exec (pty): {e}")))?;
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
            .ok_or_else(|| EngineError::Sandbox("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Sandbox(format!("wait sbx exec: {e}")))?;
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

    fn cancel(&self) -> Result<(), EngineError> {
        kill_local_sbx_exec(self.child_pid);
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let pid = self.child_pid;
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                kill_local_sbx_exec(pid);
                Ok(())
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
    use crate::engine::container::options::ResolvedContainerOptions;
    use crate::engine::error::EngineError;

    // ─── Platform guards ──────────────────────────────────────────────────────

    #[test]
    fn dsbx_errors_on_linux() {
        if cfg!(target_os = "linux") {
            match SandboxRuntime::dsbx() {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                    assert_eq!(backend, "docker-sbx-experimental");
                    assert!(
                        platform.starts_with("linux"),
                        "platform should name linux, got: {platform}"
                    );
                    assert!(
                        platform.contains("Issue #51"),
                        "platform should explain the upstream blocker, got: {platform}"
                    );
                }
                Err(e) => panic!("expected BackendUnsupportedOnPlatform on linux, got: {e:?}"),
                Ok(_) => panic!("dsbx() must fail on linux"),
            }
        }
    }

    #[test]
    fn dsbx_errors_on_x86_64_macos() {
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            match SandboxRuntime::dsbx() {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                    assert_eq!(backend, "docker-sbx-experimental");
                    assert!(
                        platform.contains("macos"),
                        "platform should mention macos, got: {platform}"
                    );
                    assert!(
                        platform.contains("x86_64"),
                        "platform should mention x86_64, got: {platform}"
                    );
                    assert!(
                        platform.contains("Apple Silicon"),
                        "platform should explain the arm64 requirement, got: {platform}"
                    );
                }
                Err(e) => {
                    panic!("expected BackendUnsupportedOnPlatform on x86_64 macos, got: {e:?}")
                }
                Ok(_) => panic!("dsbx() must fail on x86_64 macos"),
            }
        }
    }

    // ─── Option-variant mismatch via SandboxRuntime ───────────────────────────

    /// `SandboxRuntime::build` must reject container-paradigm options with a
    /// clear `OptionVariantMismatch` error on platforms where dsbx is
    /// supported. Skipped via early-return on unsupported platforms.
    #[test]
    fn sandbox_runtime_via_trait_rejects_container_options() {
        let rt = match SandboxRuntime::dsbx() {
            Ok(rt) => rt,
            Err(_) => return, // unsupported platform — platform guard test covers this
        };
        let opts = ResolvedAgentOptions::Container(ResolvedContainerOptions::resolve([]).unwrap());
        match <SandboxRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker-sbx-experimental");
                assert_eq!(got, "container");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ─── runtime_name and display_name ────────────────────────────────────────

    #[test]
    fn dsbx_runtime_name_and_display_name() {
        // dsbx() errors on unsupported platforms; the guard tests above
        // cover that path.
        if let Ok(rt) = SandboxRuntime::dsbx() {
            assert_eq!(rt.runtime_name(), "docker-sbx-experimental");
            assert!(
                rt.display_name().contains("experimental"),
                "display_name should mention experimental, got: {}",
                rt.display_name()
            );
        }
    }
}
