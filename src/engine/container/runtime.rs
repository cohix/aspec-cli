//! `ContainerRuntime` — the container-class `AgentRuntimeEngine` impl.
//!
//! Holds a `Box<dyn ContainerBackend>` chosen by the `docker()` / `apple()`
//! constructors (selection between runtimes happens in
//! `agent_runtime::detect`). The concrete backend is invisible outside this
//! module.
//!
//! Container-paradigm-specific operations — `build_image`, `image_exists`,
//! `image_home_dir`, `start_background` — are inherent methods only; they
//! deliberately do NOT appear on the `AgentRuntimeEngine` trait. Code that
//! needs them must hold a typed `Arc<ContainerRuntime>`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::{
    AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use crate::engine::container::apple::AppleBackend;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::background::BackgroundContainer;
use crate::engine::container::docker::DockerBackend;
use crate::engine::container::options::{OverlaySpec, ResolvedContainerOptions};
use crate::engine::error::EngineError;

/// A container image row returned by image-listing queries. Used by
/// `awman clean` to enumerate dangling awman images eligible for removal.
#[derive(Debug, Clone)]
pub struct ContainerImageInfo {
    /// Image ID (short or full).
    pub id: String,
    /// `repository:tag` label, or `<none>:<none>` for untagged images.
    pub repo_tag: String,
    /// Human-readable size string reported by the runtime (e.g. "1.2GB").
    pub size: String,
}

/// Capabilities shared by container-class backends (Docker, Apple
/// Containers): image-based, ephemeral, arbitrary mounts/env, label-based
/// session attribution.
static CONTAINER_CAPABILITIES: Capabilities = Capabilities {
    arbitrary_env_vars: true,
    arbitrary_host_mounts: true,
    cpu_limits: true,
    per_resource_stats: true,
    persistent_lifecycle: false,
    kit_declarative: false,
    dind: DindSupport::OnRequest,
    host_paths_visible: true,
    session_label_supported: true,
};

pub struct ContainerRuntime {
    backend: Arc<dyn ContainerBackend>,
}

impl ContainerRuntime {
    /// Construct with the Docker backend.
    pub fn docker() -> Self {
        Self {
            backend: Arc::new(DockerBackend::new()),
        }
    }

    /// Construct with the Apple Containers backend. The macOS platform guard
    /// lives in `agent_runtime::detect`; constructing this directly on a
    /// non-mac host yields a runtime whose probes simply fail.
    pub fn apple() -> Self {
        Self {
            backend: Arc::new(AppleBackend::new()),
        }
    }

    /// Static name of the chosen backend (e.g. `"docker"`).
    pub fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    /// User-facing display name for the chosen backend
    /// (e.g. `"Docker"`, `"Apple Containers"`).
    pub fn display_name(&self) -> &'static str {
        match self.backend.name() {
            "apple-containers" => "Apple Containers",
            _ => "Docker",
        }
    }

    /// Static description of what container-class runtimes can do.
    pub fn capabilities(&self) -> &Capabilities {
        &CONTAINER_CAPABILITIES
    }

    /// Build a fully-configured `AgentInstance` from pre-resolved options.
    pub fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.backend.build(options)
    }

    pub fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running(session)
    }

    /// Shell out to the underlying CLI to build a container image. Streams
    /// stdout+stderr line-by-line through `on_line`. Returns an error when the
    /// build fails.
    pub fn build_image(
        &self,
        tag: &str,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
        no_cache: bool,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let cli = self.backend.name();
        // Both "docker" and "container" share the same `build` argv shape.
        let cli_bin = match cli {
            "apple-containers" => "container",
            _ => "docker",
        };
        let mut args: Vec<String> = vec!["build".into()];
        if no_cache {
            args.push("--no-cache".into());
        }
        args.extend([
            "-t".into(),
            tag.to_string(),
            "-f".into(),
            dockerfile.display().to_string(),
            context.display().to_string(),
        ]);
        let mut child = Command::new(cli_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| EngineError::Container(format!("spawn {cli_bin} build: {e}")))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Combine stdout + stderr into a single sequenced stream by spawning two
        // threads that funnel into a channel.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx_out = tx.clone();
        let stdout_handle = std::thread::spawn(move || {
            if let Some(out) = stdout {
                let r = BufReader::new(out);
                for line in r.lines().map_while(Result::ok) {
                    let _ = tx_out.send(line);
                }
            }
        });
        let stderr_handle = std::thread::spawn(move || {
            if let Some(err) = stderr {
                let r = BufReader::new(err);
                for line in r.lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            }
        });
        for line in rx {
            on_line(&line);
        }
        let _ = stdout_handle.join();
        let _ = stderr_handle.join();
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait {cli_bin} build: {e}")))?;
        if !status.success() {
            return Err(EngineError::ImageBuildExitNonzero {
                tag: tag.to_string(),
                exit_code: status.code().unwrap_or(-1),
            });
        }
        Ok(())
    }

    /// Read the image's baked-in `$HOME` from its config. Used by
    /// `AgentEngine::build_options` to mount agent settings overlays at the
    /// path the running container's user actually reads — when the
    /// `Dockerfile.<agent>` has been changed but the image hasn't been
    /// rebuilt, the image's User/HOME is the authority, not the Dockerfile.
    /// Returns `None` when the image is missing or the runtime CLI is
    /// unreachable.
    pub fn image_home_dir(&self, tag: &str) -> Option<String> {
        self.backend.image_home_dir(tag)
    }

    /// Best-effort check whether an image tag exists locally on the runtime.
    /// Times out after 10 seconds to avoid hanging when the daemon is unresponsive.
    pub fn image_exists(&self, tag: &str) -> bool {
        use std::process::{Command, Stdio};
        let cli_bin = match self.backend.name() {
            "apple-containers" => "container",
            _ => "docker",
        };
        let child = Command::new(cli_bin)
            .args(["image", "inspect", tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => wait_with_timeout(child, std::time::Duration::from_secs(10))
                .map(|s| s.success())
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// List all running awman containers without requiring a session.
    /// Used by the TUI event loop for stats polling.
    pub fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running_all()
    }

    pub fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.backend.stats(handle)
    }

    pub fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }

    /// List stopped (exited/dead) awman containers eligible for cleanup.
    /// Running and paused containers are never returned. Used by `awman clean`.
    pub fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_stopped()
    }

    /// List dangling awman images (superseded by a newer build of the same
    /// tag). Used by `awman clean`.
    pub fn list_dangling_images(&self) -> Result<Vec<ContainerImageInfo>, EngineError> {
        self.backend.list_dangling_images()
    }

    /// Remove a container by id/name. Returns an error when the runtime refuses
    /// (e.g. the container transitioned back to running between discovery and
    /// deletion). Used by `awman clean` for per-item failure handling.
    pub fn remove_container(&self, id: &str) -> Result<(), EngineError> {
        run_removal(self.cli_binary(), "rm", id)
    }

    /// Remove an image by id. Returns an error when the runtime refuses (e.g.
    /// the image is still referenced by a container). Used by `awman clean`.
    pub fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        run_removal(self.cli_binary(), "rmi", id)
    }

    /// Build CLI arguments for `docker exec -it` (or equivalent) into a running
    /// container. Returns args suitable for `Command::new(cli_binary).args(...)`.
    pub fn exec_args(
        &self,
        container_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        self.backend
            .exec_args(container_id, working_dir, entrypoint, env_vars)
    }

    /// Attach to an already-running container this process did not start.
    /// Delegates to the backend; the returned instance runs `<cli> exec`
    /// through the existing `run_with_frontend` path.
    pub fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.backend.attach(handle)
    }

    /// List running awman containers whose name starts with `prefix`.
    pub fn list_running_with_name_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running_with_name_prefix(prefix)
    }

    /// The CLI binary name for this runtime (`"docker"` or `"container"`).
    pub fn cli_binary(&self) -> &'static str {
        match self.backend.name() {
            "apple-containers" => "container",
            _ => "docker",
        }
    }

    /// Start a background container for setup/teardown execution.
    ///
    /// Delegates to the backend's `start_background` (default impl in
    /// `ContainerBackend` shells out to the runtime's CLI). The returned
    /// `BackgroundContainer` retains a shared reference to the backend so
    /// later `exec` and `kill` calls flow through the same trait.
    pub fn start_background(
        &self,
        image: &str,
        workdir: &Path,
        env: &HashMap<String, String>,
        overlays: &[OverlaySpec],
    ) -> Result<BackgroundContainer, EngineError> {
        let container_id = self
            .backend
            .start_background(image, workdir, env, overlays)?;
        let workdir_str = workdir.display().to_string();
        Ok(BackgroundContainer::new(
            container_id,
            Arc::clone(&self.backend),
            workdir_str,
        ))
    }

    /// Best-effort check whether the container runtime daemon is reachable.
    /// Returns `false` when `docker info` (or equivalent) fails or times out.
    pub fn is_available(&self) -> bool {
        use std::process::Stdio;
        let (cli_bin, args): (&str, &[&str]) = match self.backend.name() {
            "apple-containers" => ("container", &["system", "status"]),
            _ => ("docker", &["info", "--format", "{{.ServerVersion}}"]),
        };
        let child = std::process::Command::new(cli_bin)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => wait_with_timeout(child, std::time::Duration::from_secs(10))
                .map(|s| s.success())
                .unwrap_or(false),
            Err(_) => false,
        }
    }
}

impl AgentRuntimeEngine for ContainerRuntime {
    fn runtime_name(&self) -> &'static str {
        ContainerRuntime::runtime_name(self)
    }

    fn display_name(&self) -> &'static str {
        ContainerRuntime::display_name(self)
    }

    fn capabilities(&self) -> &Capabilities {
        ContainerRuntime::capabilities(self)
    }

    fn is_available(&self) -> bool {
        ContainerRuntime::is_available(self)
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        match options {
            ResolvedAgentOptions::Container(opts) => ContainerRuntime::build(self, opts),
            other => Err(EngineError::OptionVariantMismatch {
                runtime: self.runtime_name().to_string(),
                got: other.paradigm(),
            }),
        }
    }

    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running(self, session)
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running_all(self)
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        ContainerRuntime::stats(self, handle)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        ContainerRuntime::stop(self, handle)
    }

    fn exec_args(
        &self,
        agent_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        ContainerRuntime::exec_args(self, agent_id, working_dir, entrypoint, env_vars)
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        ContainerRuntime::attach(self, handle)
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running_with_name_prefix(self, prefix)
    }

    fn cli_binary(&self) -> &'static str {
        ContainerRuntime::cli_binary(self)
    }
}

/// Shell out to the runtime CLI to remove a container (`rm`) or image (`rmi`).
/// Returns an error on a non-zero exit so callers can count per-item failures.
fn run_removal(cli_bin: &str, subcommand: &str, target: &str) -> Result<(), EngineError> {
    use std::process::{Command, Stdio};
    let output = Command::new(cli_bin)
        .args([subcommand, target])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::ContainerRuntimeUnavailable {
                    binary: cli_bin.to_string(),
                }
            } else {
                EngineError::Container(format!("{cli_bin} {subcommand} {target}: {e}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(EngineError::Container(format!(
            "{cli_bin} {subcommand} {target} failed: {stderr}"
        )));
    }
    Ok(())
}

/// Wait for a child process with a timeout. Kills the process and returns
/// `None` if the deadline elapses. Prevents unit tests and readiness checks
/// from hanging indefinitely when the Docker daemon is unresponsive.
pub(crate) fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::ResolvedAgentOptions;
    use crate::engine::sandbox::options::ResolvedSandboxOptions;

    #[test]
    fn build_requires_image_option() {
        let rt = ContainerRuntime::docker();
        let resolved = ResolvedContainerOptions::resolve([]).unwrap();
        match rt.build(resolved) {
            Err(EngineError::MissingRequiredOption(opt)) => {
                assert_eq!(opt, "Image");
            }
            Err(e) => panic!("expected MissingRequiredOption, got: {e:?}"),
            Ok(_) => panic!("expected error from missing Image option"),
        }
    }

    /// The `AgentRuntimeEngine` trait impl must reject sandbox-paradigm options
    /// with a clear `OptionVariantMismatch` error — never silently fall back
    /// or panic.
    #[test]
    fn container_runtime_via_trait_rejects_sandbox_options() {
        use crate::engine::agent_runtime::AgentRuntimeEngine;

        let rt = ContainerRuntime::docker();
        let opts = ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::default());
        match <ContainerRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker");
                assert_eq!(got, "sandbox");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn apple_runtime_via_trait_rejects_sandbox_options() {
        use crate::engine::agent_runtime::AgentRuntimeEngine;

        let rt = ContainerRuntime::apple();
        let opts = ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::default());
        match <ContainerRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "apple-containers");
                assert_eq!(got, "sandbox");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
