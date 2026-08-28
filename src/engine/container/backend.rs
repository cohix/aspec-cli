//! Internal `ContainerBackend` trait — NOT pub outside `src/engine/container/`.
//!
//! Implementations: `docker::DockerBackend`, `apple::AppleBackend`.

use std::collections::HashMap;
use std::path::Path;

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::background::ExecOutput;
use crate::engine::agent_runtime::execution::{AgentInstance, AgentStats};
use crate::engine::container::options::{OverlaySpec, ResolvedContainerOptions};
use crate::engine::error::EngineError;

/// What every container backend must support. The concrete type is hidden
/// behind `Box<dyn ContainerBackend>` and never escapes this module.
pub(super) trait ContainerBackend: Send + Sync {
    /// Build an `AgentInstance` from resolved options. The image is NOT
    /// pulled or built here — that's a separate concern handled by
    /// higher-level engines (e.g. `AgentEngine::ensure_available`).
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError>;

    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError>;

    /// List all running awman containers without requiring a session.
    /// Default falls back to an empty list.
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// List stopped (exited/dead) awman containers. Backends that cannot
    /// enumerate stopped containers fall back to an empty list.
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }

    /// List dangling awman images eligible for cleanup. Default: empty.
    fn list_dangling_images(
        &self,
    ) -> Result<Vec<crate::engine::container::runtime::ContainerImageInfo>, EngineError> {
        Ok(Vec::new())
    }

    /// Build the CLI arguments for `docker exec -it` (or equivalent) into a
    /// running container. Used by TUI re-attach.
    fn exec_args(
        &self,
        container_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String>;

    /// Attach to an already-running container this process did not start,
    /// via `<cli> exec` (argv from `exec_args`). The returned instance's
    /// execution never issues `stop`/`rm` on grace-expiry — the container
    /// belongs to another process.
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError>;

    /// List running awman containers whose name starts with `prefix`.
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError>;

    /// Static name used by `ContainerRuntime::runtime_name`.
    fn name(&self) -> &'static str;

    /// Read the image's effective `$HOME` from its baked-in config. Used by
    /// `AgentEngine::build_options` to mount agent settings overlays at the
    /// path the running container's user actually reads — which can diverge
    /// from the on-disk `Dockerfile.<agent>` after a Dockerfile change that
    /// hasn't been followed by an image rebuild. Returns `None` when the
    /// image is missing, the CLI is unreachable, or the image config has no
    /// `HOME` env entry.
    fn image_home_dir(&self, _tag: &str) -> Option<String> {
        None
    }

    /// CLI binary for this backend (`docker` or `container`). Default maps
    /// the well-known `name()` values; override when adding a backend whose
    /// binary differs from its name.
    fn cli_binary(&self) -> &'static str {
        match self.name() {
            "apple-containers" => "container",
            _ => "docker",
        }
    }

    // ─── Background container lifecycle ─────────────────────────────────────
    //
    // Default impls in this trait shell out to `cli_binary()`. Docker and
    // Apple Containers share identical argv shape for these operations;
    // future backends with diverging syntax can override.

    fn start_background(
        &self,
        image: &str,
        workdir: &Path,
        env: &HashMap<String, String>,
        overlays: &[OverlaySpec],
    ) -> Result<String, EngineError> {
        super::background::default_start_background(
            self.cli_binary(),
            image,
            workdir,
            env,
            overlays,
        )
    }

    fn exec_in_background(
        &self,
        container_id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        super::background::default_exec_in_background(
            self.cli_binary(),
            container_id,
            command,
            working_dir,
            env,
        )
    }

    fn exec_in_background_streaming(
        &self,
        container_id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        super::background::default_exec_in_background_streaming(
            self.cli_binary(),
            container_id,
            command,
            working_dir,
            env,
            on_line,
        )
    }

    fn stop_and_remove(&self, container_id: &str) -> Result<(), EngineError> {
        super::background::default_stop_and_remove(self.cli_binary(), container_id);
        Ok(())
    }
}
