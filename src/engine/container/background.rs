//! `BackgroundContainer` — a long-running container that accepts `exec` calls.
//!
//! Used by setup and teardown phases to run sequential shell commands inside a
//! single container instance. The container is started with an idle entrypoint
//! (`sleep infinity`) and kept alive for the duration of the phase.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::engine::agent_runtime::background::{AgentExec, ExecOutput};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::options::OverlaySpec;
use crate::engine::error::EngineError;

/// A running background container that accepts `exec` calls.
///
/// Created by `ContainerRuntime::start_background`. Must be explicitly killed
/// via `kill()` when the phase is complete. Implements `Drop` as a safety net
/// for best-effort cleanup.
pub struct BackgroundContainer {
    container_id: String,
    backend: Arc<dyn ContainerBackend>,
    working_dir: String,
    killed: bool,
}

impl BackgroundContainer {
    pub(super) fn new(
        container_id: String,
        backend: Arc<dyn ContainerBackend>,
        working_dir: String,
    ) -> Self {
        Self {
            container_id,
            backend,
            working_dir,
            killed: false,
        }
    }

    /// Execute a shell command inside the running container. Routes through
    /// the backend so a future runtime with diverging exec syntax can override.
    pub fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.backend
            .exec_in_background(&self.container_id, command, &self.working_dir, env)
    }

    /// Stop and remove the background container. Must always be called, even if
    /// exec steps failed.
    pub fn kill(mut self) -> Result<(), EngineError> {
        self.do_kill()
    }

    fn do_kill(&mut self) -> Result<(), EngineError> {
        if self.killed {
            return Ok(());
        }
        self.killed = true;
        self.backend.stop_and_remove(&self.container_id)
    }

    pub fn container_id(&self) -> &str {
        &self.container_id
    }
}

impl Drop for BackgroundContainer {
    fn drop(&mut self) {
        if !self.killed {
            if let Err(e) = self.do_kill() {
                tracing::warn!(
                    container_id = %self.container_id,
                    error = %e,
                    "failed to kill background container"
                );
            }
        }
    }
}

impl AgentExec for BackgroundContainer {
    fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        // Delegates to the inherent method; inherent takes priority in method
        // resolution so this does not recurse.
        BackgroundContainer::exec(self, command, env)
    }

    fn exec_streaming(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        self.backend.exec_in_background_streaming(
            &self.container_id,
            command,
            &self.working_dir,
            env,
            on_line,
        )
    }
}

// ─── Default backend implementations ─────────────────────────────────────────
//
// The Docker and Apple Containers CLIs share identical argv for these
// operations. The default `ContainerBackend` trait methods delegate here.

pub(super) fn default_start_background(
    cli_bin: &str,
    image: &str,
    workdir: &Path,
    env: &HashMap<String, String>,
    overlays: &[OverlaySpec],
) -> Result<String, EngineError> {
    let name = crate::engine::container::naming::generate_container_name();
    let workdir_str = workdir.display().to_string();

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.clone(),
        "--label".into(),
        "awman=true".into(),
        "-w".into(),
        workdir_str.clone(),
    ];

    // Mount the working directory as read-write.
    args.push("-v".into());
    args.push(format!("{ws}:{ws}", ws = workdir_str));

    // Apply overlay mounts.
    for overlay in overlays {
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

    // Env vars set at start time — inherited by all subsequent exec calls.
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }

    // Image and idle entrypoint.
    args.push(image.to_string());
    args.push("sleep".into());
    args.push("infinity".into());

    let output = Command::new(cli_bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::ContainerRuntimeUnavailable {
                    binary: cli_bin.into(),
                }
            } else {
                EngineError::Container(format!("spawn {cli_bin} run -d: {e}"))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such image")
            || stderr.contains("not found")
            || stderr.contains("pull access denied")
            || stderr.contains("manifest unknown")
        {
            return Err(EngineError::ContainerImageNotFound {
                image: image.to_string(),
            });
        }
        return Err(EngineError::Container(format!(
            "failed to start background container: {}",
            stderr.trim()
        )));
    }

    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if container_id.is_empty() {
        Ok(name)
    } else {
        Ok(container_id)
    }
}

pub(super) fn default_exec_in_background(
    cli_bin: &str,
    container_id: &str,
    command: &str,
    working_dir: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<ExecOutput, EngineError> {
    let mut args = vec!["exec".to_string()];
    args.extend(["-w".to_string(), working_dir.to_string()]);

    if let Some(env_map) = env {
        for (k, v) in env_map {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
    }

    args.push(container_id.to_string());
    args.extend(["sh".to_string(), "-c".to_string(), command.to_string()]);

    let output = Command::new(cli_bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            EngineError::Container(format!("exec in background container {container_id}: {e}"))
        })?;

    let exit_code = output.status.code().unwrap_or(-1);
    Ok(ExecOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code,
    })
}

pub(super) fn default_exec_in_background_streaming(
    cli_bin: &str,
    container_id: &str,
    command: &str,
    working_dir: &str,
    env: Option<&HashMap<String, String>>,
    on_line: &mut dyn FnMut(&str),
) -> Result<ExecOutput, EngineError> {
    let mut args = vec!["exec".to_string()];
    args.extend(["-w".to_string(), working_dir.to_string()]);

    if let Some(env_map) = env {
        for (k, v) in env_map {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
    }

    args.push(container_id.to_string());
    args.extend(["sh".to_string(), "-c".to_string(), command.to_string()]);

    let mut child = Command::new(cli_bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            EngineError::Container(format!("exec in background container {container_id}: {e}"))
        })?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stderr_thread = std::thread::spawn(move || {
        let mut stderr_buf = String::new();
        if let Some(pipe) = stderr_pipe {
            let reader = std::io::BufReader::new(pipe);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        stderr_buf.push_str(&l);
                        stderr_buf.push('\n');
                    }
                    Err(_) => break,
                }
            }
        }
        stderr_buf
    });

    let mut stdout_buf = String::new();
    if let Some(pipe) = stdout_pipe {
        let reader = std::io::BufReader::new(pipe);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    on_line(&l);
                    stdout_buf.push_str(&l);
                    stdout_buf.push('\n');
                }
                Err(_) => break,
            }
        }
    }

    let stderr_buf = stderr_thread.join().unwrap_or_default();
    for line in stderr_buf.lines() {
        on_line(line);
    }

    let status = child.wait().map_err(|e| {
        EngineError::Container(format!(
            "waiting for exec in background container {container_id}: {e}"
        ))
    })?;

    let exit_code = status.code().unwrap_or(-1);
    Ok(ExecOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code,
    })
}

pub(super) fn default_stop_and_remove(cli_bin: &str, container_id: &str) {
    let _ = Command::new(cli_bin)
        .args(["stop", container_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new(cli_bin)
        .args(["rm", container_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::{AgentHandle, Session};
    use crate::engine::agent_runtime::execution::{AgentInstance, AgentStats};
    use crate::engine::container::backend::ContainerBackend;
    use crate::engine::container::options::ResolvedContainerOptions;
    use std::sync::Mutex;

    /// Recording backend used to assert the lifecycle order of
    /// `BackgroundContainer::{exec, kill}` without a live runtime.
    #[derive(Default)]
    struct RecordingBackend {
        events: Mutex<Vec<String>>,
        start_id: Mutex<Option<String>>,
    }

    impl RecordingBackend {
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ContainerBackend for RecordingBackend {
        fn build(
            &self,
            _options: ResolvedContainerOptions,
        ) -> Result<Box<dyn AgentInstance>, EngineError> {
            unimplemented!("not exercised by BackgroundContainer lifecycle tests")
        }
        fn list_running(&self, _s: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            Ok(Vec::new())
        }
        fn stats(&self, _h: &AgentHandle) -> Result<AgentStats, EngineError> {
            unimplemented!()
        }
        fn stop(&self, _h: &AgentHandle) -> Result<(), EngineError> {
            Ok(())
        }
        fn exec_args(
            &self,
            _id: &str,
            _wd: &str,
            _ep: &[&str],
            _env: &[(&str, &str)],
        ) -> Vec<String> {
            Vec::new()
        }
        fn attach(&self, _h: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            unimplemented!("not exercised by BackgroundContainer lifecycle tests")
        }
        fn list_running_with_name_prefix(
            &self,
            _prefix: &str,
        ) -> Result<Vec<AgentHandle>, EngineError> {
            Ok(Vec::new())
        }
        fn name(&self) -> &'static str {
            "recording"
        }
        fn start_background(
            &self,
            _image: &str,
            _workdir: &std::path::Path,
            _env: &HashMap<String, String>,
            _overlays: &[OverlaySpec],
        ) -> Result<String, EngineError> {
            let id = "bg-test".to_string();
            self.events.lock().unwrap().push("start".into());
            *self.start_id.lock().unwrap() = Some(id.clone());
            Ok(id)
        }
        fn exec_in_background(
            &self,
            container_id: &str,
            command: &str,
            _wd: &str,
            _env: Option<&HashMap<String, String>>,
        ) -> Result<ExecOutput, EngineError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("exec[{container_id}]: {command}"));
            Ok(ExecOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
        fn stop_and_remove(&self, container_id: &str) -> Result<(), EngineError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("kill[{container_id}]"));
            Ok(())
        }
    }

    #[test]
    fn explicit_kill_records_one_stop_event() {
        let backend = Arc::new(RecordingBackend::default());
        let id = backend
            .start_background("img", std::path::Path::new("/w"), &HashMap::new(), &[])
            .unwrap();
        let bg = BackgroundContainer::new(
            id,
            backend.clone() as Arc<dyn ContainerBackend>,
            "/w".into(),
        );
        let _ = bg.exec("echo hi", None).unwrap();
        bg.kill().unwrap();

        let evts = backend.events();
        assert_eq!(
            evts,
            vec!["start", "exec[bg-test]: echo hi", "kill[bg-test]"],
            "must record start, then exec, then kill in order"
        );
    }

    #[test]
    fn drop_without_explicit_kill_still_kills_once() {
        let backend = Arc::new(RecordingBackend::default());
        let id = backend
            .start_background("img", std::path::Path::new("/w"), &HashMap::new(), &[])
            .unwrap();
        {
            let _bg = BackgroundContainer::new(
                id,
                backend.clone() as Arc<dyn ContainerBackend>,
                "/w".into(),
            );
        } // dropped here
        let kill_events: Vec<_> = backend
            .events()
            .into_iter()
            .filter(|e| e.starts_with("kill"))
            .collect();
        assert_eq!(
            kill_events,
            vec!["kill[bg-test]"],
            "Drop must trigger kill exactly once"
        );
    }

    #[test]
    fn explicit_kill_then_drop_kills_only_once() {
        let backend = Arc::new(RecordingBackend::default());
        let id = backend
            .start_background("img", std::path::Path::new("/w"), &HashMap::new(), &[])
            .unwrap();
        let bg = BackgroundContainer::new(
            id,
            backend.clone() as Arc<dyn ContainerBackend>,
            "/w".into(),
        );
        bg.kill().unwrap();
        let kill_events: Vec<_> = backend
            .events()
            .into_iter()
            .filter(|e| e.starts_with("kill"))
            .collect();
        assert_eq!(
            kill_events,
            vec!["kill[bg-test]"],
            "explicit kill marks the container killed; Drop must not call again"
        );
    }
}
