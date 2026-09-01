//! Daemon lifecycle and the shared daemon-discovery supervisor for squad.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::http_core::HttpCore;
use crate::command::commands::squad::commands::{SquadCommandFrontend, SquadServeConfig};
use crate::command::commands::squad::gateway::{DaemonStatus, RemoteTaskGateway};
use crate::command::commands::squad::key_setup;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::fs::daemon_process::{SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME};
use crate::data::fs::{
    AcquireError, DaemonGuard, DaemonKind, DaemonProcess, SquadPaths, Termination,
};
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::auth::ApiKey;

#[derive(Debug, Clone)]
pub struct SquadStartFlags {
    pub port: u16,
    pub background: bool,
    pub refresh_key: bool,
    pub dangerously_skip_auth: bool,
}
#[derive(Debug, Clone)]
pub struct SquadStopFlags;
#[derive(Debug, Clone)]
pub struct SquadStatusFlags;
#[derive(Debug, Clone)]
pub struct SquadLogsFlags {
    pub follow: bool,
}

#[derive(Debug, Clone)]
pub enum SquadDaemonSubcommand {
    Start(SquadStartFlags),
    Stop(SquadStopFlags),
    Status(SquadStatusFlags),
    Logs(SquadLogsFlags),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum SquadDaemonOutcome {
    Started {
        port: u16,
        background: bool,
        refreshed_key: bool,
    },
    Stopped {
        stopped_pid: Option<u32>,
    },
    Status(DaemonStatus),
    Logs {
        log_path: String,
    },
}

pub struct SquadDaemonCommand {
    sub: SquadDaemonSubcommand,
    engines: Engines,
}

impl SquadDaemonCommand {
    pub fn new(sub: SquadDaemonSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }
}

#[async_trait]
impl Command for SquadDaemonCommand {
    type Frontend = Box<dyn SquadCommandFrontend>;
    type Outcome = SquadDaemonOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let env = Env::from_process();
        let paths = SquadPaths::from_env(&env)?;
        let process = squad_process(&paths);
        let guard = DaemonGuard::for_daemon(DaemonKind::Squad, &env)?;
        let outcome = match self.sub {
            SquadDaemonSubcommand::Start(flags) => {
                run_start(
                    flags,
                    &self.engines,
                    &paths,
                    &process,
                    &guard,
                    &mut *frontend,
                )
                .await?
            }
            SquadDaemonSubcommand::Stop(_) => run_stop(&process, &mut *frontend)?,
            SquadDaemonSubcommand::Status(_) => run_status(&process).await?,
            SquadDaemonSubcommand::Logs(flags) => run_logs(&process, flags, &mut *frontend).await?,
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

pub struct SquadSupervisor {
    process: DaemonProcess,
    guard: DaemonGuard,
    paths: SquadPaths,
    env: EnvSnapshot,
    /// A bearer key minted by this process because none existed yet. It is
    /// deliberately never printed from here — see `provision_key`. Callers that
    /// own a terminal drain it via [`SquadSupervisor::take_generated_key_setup`].
    generated_key: std::sync::Mutex<Option<ApiKey>>,
    /// Set once the minted key has been handed to a frontend for display.
    key_disclosed: std::sync::atomic::AtomicBool,
}

impl SquadSupervisor {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, CommandError> {
        let paths = SquadPaths::from_env(env)?;
        Ok(Self {
            process: squad_process(&paths),
            guard: DaemonGuard::for_daemon(DaemonKind::Squad, env)?,
            paths,
            env: env.clone(),
            generated_key: std::sync::Mutex::new(None),
            key_disclosed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The key this supervisor minted during `ensure_running`, if any. A caller
    /// that owns a terminal (the CLI, the TUI) displays it; nothing else may.
    pub fn generated_key(&self) -> Option<ApiKey> {
        self.generated_key
            .lock()
            .expect("squad generated-key mutex poisoned")
            .clone()
    }

    /// Take the setup snippet for a key this supervisor minted, if it minted
    /// one and has not handed it out yet. `None` on every later call, so a
    /// caller may print the result unconditionally.
    ///
    /// The key itself stays in place — this supervisor still needs it to
    /// authenticate — but the *disclosure* happens exactly once, so two
    /// frontends sharing a supervisor cannot both print the same secret.
    pub fn take_generated_key_setup(&self) -> Option<String> {
        let key = self.generated_key()?;
        if self
            .key_disclosed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        Some(key_setup::render_key_setup(
            key.as_str(),
            key_setup::ShellFlavor::from_env(&self.env),
        ))
    }

    /// Resolve the bearer key this process will authenticate with.
    ///
    /// On a first run there is no `squad_key.hash` yet. The key MUST be minted
    /// here, in the process that is about to spawn the daemon — never inside
    /// the detached child, whose stdout is redirected to `~/.awman/squad/awman.log`
    /// (launchd) or the journal (systemd-run) and would persist the plaintext
    /// key in a file `awman squad logs` prints verbatim.
    fn provision_key(&self) -> Result<Option<ApiKey>, CommandError> {
        if let Some(key) = self.env.squad_key() {
            return Ok(Some(ApiKey::from_string(key.to_string())));
        }
        if let Some(key) = self.generated_key() {
            return Ok(Some(key));
        }
        // A daemon started with `--dangerously-skip-auth` checks no bearer
        // token, so minting one here would write an `squad_key.hash` whose
        // plaintext nobody holds — and the next auth-enabled start would then
        // demand a key the user was never shown.
        if self.daemon_auth_disabled()? {
            return Ok(None);
        }
        if self.process.paths().read_key_hash()?.is_some() {
            // A hash exists but this process was given no key; the request will
            // be refused by the daemon with the standard auth error.
            return Ok(None);
        }
        let auth_engine = crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::AuthPathResolver::from_process_env()?,
            crate::data::fs::ApiPaths::from_process_env()?,
        );
        let key = auth_engine.generate_api_key()?;
        let hash = auth_engine.hash_api_key(&key);
        self.process.paths().write_key_hash(hash.as_str())?;
        *self
            .generated_key
            .lock()
            .expect("squad generated-key mutex poisoned") = Some(key.clone());
        tracing::info!("squad daemon key minted for automatic daemon startup");
        Ok(Some(key))
    }

    /// Whether the daemon that is currently running published a sidecar saying
    /// it serves unauthenticated. `false` when no daemon is running, when it
    /// published no sidecar, or when the sidecar predates the flag — every one
    /// of which means "assume auth is required".
    fn daemon_auth_disabled(&self) -> Result<bool, CommandError> {
        if self.process.running_pid()?.is_none() {
            return Ok(false);
        }
        Ok(self
            .process
            .read_meta()?
            .is_some_and(|meta| meta.auth_disabled))
    }

    /// Discover the existing daemon endpoint, if its metadata sidecar is present.
    pub fn gateway_from_meta(&self) -> Result<Option<RemoteTaskGateway>, CommandError> {
        let key = self.provision_key()?;
        self.gateway_from_meta_with(key.as_ref())
    }

    fn gateway_from_meta_with(
        &self,
        key: Option<&ApiKey>,
    ) -> Result<Option<RemoteTaskGateway>, CommandError> {
        let Some(meta) = self.process.read_meta()? else {
            return Ok(None);
        };
        let address = format!("{}://{}:{}", meta.scheme, meta.bind_ip, meta.port);
        Ok(Some(RemoteTaskGateway::new(HttpCore::new(
            &address, "v1", key,
        )?)))
    }

    /// Return a remote gateway, starting the daemon only when needed. The
    /// cross-daemon guard is intentionally first, before a PID check or spawn.
    pub async fn ensure_running(&self) -> Result<RemoteTaskGateway, CommandError> {
        tracing::info!("squad supervisor ensure-running requested");
        self.guard.check()?;
        // Mint the key here, before any spawn, so the detached child always
        // finds a hash already on disk and never emits a key to its log.
        let key = self.provision_key()?;
        if self.process.running_pid()?.is_some() {
            tracing::info!("squad supervisor found an already-running daemon");
            return self.gateway_from_meta_with(key.as_ref())?.ok_or_else(|| {
                CommandError::Other(format!(
                    "squad daemon is running but has not published its endpoint; check {}",
                    self.paths.daemon().log_file().display()
                ))
            });
        }
        let binary = std::env::current_exe()
            .map_err(|e| CommandError::Other(format!("cannot determine awman binary: {e}")))?;
        self.process
            .spawn_detached(&binary, &["squad".into(), "start".into()])?;
        tracing::info!("squad supervisor spawned daemon process");
        for _ in 0..100 {
            if let Some(gateway) = self.gateway_from_meta_with(key.as_ref())? {
                return Ok(gateway);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(CommandError::Other(format!(
            "squad daemon did not become ready within 10 seconds; check {}",
            self.paths.daemon().log_file().display()
        )))
    }
}

fn squad_process(paths: &SquadPaths) -> DaemonProcess {
    DaemonProcess::new(paths.daemon(), SQUAD_UNIT_NAME, SQUAD_PLIST_LABEL)
}

async fn run_start(
    flags: SquadStartFlags,
    engines: &Engines,
    paths: &SquadPaths,
    process: &DaemonProcess,
    guard: &DaemonGuard,
    frontend: &mut dyn SquadCommandFrontend,
) -> Result<SquadDaemonOutcome, CommandError> {
    // Must precede every actual daemon launch, including a detached launch.
    guard.check()?;
    if let Some(pid) = process.running_pid()? {
        return Err(CommandError::Other(format!(
            "squad daemon is already running (PID {pid})"
        )));
    }
    tracing::info!(
        port = flags.port,
        background = flags.background,
        refresh_key = flags.refresh_key,
        "squad administrator requested daemon start"
    );
    // `--dangerously-skip-auth` mints nothing and writes no hash: the flag is
    // for this run only, so a later plain `start` still finds whatever hash was
    // on disk before. It is tolerable because squad binds 127.0.0.1 exclusively.
    if flags.dangerously_skip_auth {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "Authentication is DISABLED (--dangerously-skip-auth). Any process \
                   on this machine can drive squad. The daemon still binds to loopback \
                   (127.0.0.1) only, so it is unreachable from the network."
                .into(),
        });
    }
    if flags.refresh_key
        || (!flags.dangerously_skip_auth && process.paths().read_key_hash()?.is_none())
    {
        let key = engines.auth_engine.generate_api_key()?;
        let hash = engines.auth_engine.hash_api_key(&key);
        process.paths().write_key_hash(hash.as_str())?;
        tracing::info!(
            refresh = flags.refresh_key,
            "squad daemon key minted or refreshed"
        );
        // The plaintext key is disclosed exactly here, in the foreground
        // process that owns a terminal — never in the detached child, whose
        // stdout lands in a log file `awman squad logs` prints verbatim.
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: key_setup::render_key_setup(
                key.as_str(),
                key_setup::ShellFlavor::from_env(&Env::from_process()),
            ),
        });
        if flags.refresh_key {
            return Ok(SquadDaemonOutcome::Started {
                port: flags.port,
                background: false,
                refreshed_key: true,
            });
        }
    }
    if flags.background {
        let binary = std::env::current_exe()
            .map_err(|e| CommandError::Other(format!("cannot determine awman binary: {e}")))?;
        let mut args = vec![
            "squad".into(),
            "start".into(),
            "--port".into(),
            flags.port.to_string(),
        ];
        if flags.dangerously_skip_auth {
            args.push("--dangerously-skip-auth".into());
        }
        let pid = process.spawn_detached(&binary, &args)?;
        tracing::info!(pid, port = flags.port, "squad daemon started in background");
        frontend.write_message(UserMessage {
            level: MessageLevel::Success,
            text: format!("squad daemon started in background (PID {pid})."),
        });
        return Ok(SquadDaemonOutcome::Started {
            port: flags.port,
            background: true,
            refreshed_key: false,
        });
    }
    guard
        .acquire(std::process::id())
        .map_err(|error| match error {
            AcquireError::AlreadyRunning { pid } => {
                CommandError::Other(format!("squad daemon is already running (PID {pid})"))
            }
            other => CommandError::Data(other.into_data_error()),
        })?;
    tracing::info!(port = flags.port, "squad daemon starting in foreground");
    let result = frontend
        .serve_squad_daemon(SquadServeConfig {
            port: flags.port,
            dangerously_skip_auth: flags.dangerously_skip_auth,
        })
        .await;
    let _ = process.clear_meta();
    let _ = guard.release();
    result?;
    let _ = paths;
    Ok(SquadDaemonOutcome::Started {
        port: flags.port,
        background: false,
        refreshed_key: false,
    })
}

fn run_stop(
    process: &DaemonProcess,
    frontend: &mut dyn SquadCommandFrontend,
) -> Result<SquadDaemonOutcome, CommandError> {
    tracing::info!("squad administrator requested daemon stop");
    let pid = match process.terminate_running()? {
        Termination::Terminated { pid } => pid,
        // Absent, stale, or another process' pidfile: the pidfile has been
        // cleaned up either way, so the daemon is simply not running.
        _ => return Err(CommandError::Other("squad daemon is not running".into())),
    };
    let _ = process.clear_meta();
    tracing::info!(pid, "squad daemon stopped");
    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("squad daemon (PID {pid}) stopped."),
    });
    Ok(SquadDaemonOutcome::Stopped {
        stopped_pid: Some(pid),
    })
}

async fn run_logs(
    process: &DaemonProcess,
    flags: SquadLogsFlags,
    frontend: &mut dyn SquadCommandFrontend,
) -> Result<SquadDaemonOutcome, CommandError> {
    let path = process.paths().log_file();
    // Initial dump: emit every existing line and remember where the file ends
    // so follow-mode only streams what is appended after this point.
    let mut offset = match tail_new_lines(&path, 0)? {
        Some((lines, end)) => {
            for line in lines {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: line,
                });
            }
            end
        }
        None => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("Log file not found: {}", path.display()),
            });
            0
        }
    };

    // Follow mode: tail appended lines every 250 ms until the user interrupts
    // (Ctrl-C). This is a local file read — the daemon still exposes no log
    // route on the network.
    if flags.follow {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if let Some((lines, end)) = tail_new_lines(&path, offset)? {
                        for line in lines {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Info,
                                text: line,
                            });
                        }
                        offset = end;
                    }
                }
            }
        }
    }

    Ok(SquadDaemonOutcome::Logs {
        log_path: path.display().to_string(),
    })
}

/// Read complete lines appended to `path` after byte `from`. Returns the lines
/// and the new byte offset (advanced only past the last complete line, so a
/// partial trailing line is re-read on the next call), or `None` when the file
/// does not exist yet.
fn tail_new_lines(
    path: &std::path::Path,
    from: u64,
) -> Result<Option<(Vec<String>, u64)>, CommandError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::data::error::DataError::io(path, error).into()),
    };
    let len = file
        .metadata()
        .map_err(|error| crate::data::error::DataError::io(path, error))?
        .len();
    // A truncated/rotated file (shorter than our offset) restarts from 0.
    let start = if from > len { 0 } else { from };
    if start == len {
        return Ok(Some((Vec::new(), len)));
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|error| crate::data::error::DataError::io(path, error))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .map_err(|error| crate::data::error::DataError::io(path, error))?;
    // Advance only past the last newline so a partial final line is not emitted.
    let consumed = match buf.rfind('\n') {
        Some(idx) => idx + 1,
        None => 0,
    };
    let lines = buf[..consumed].lines().map(str::to_string).collect();
    Ok(Some((lines, start + consumed as u64)))
}

async fn run_status(process: &DaemonProcess) -> Result<SquadDaemonOutcome, CommandError> {
    let pid = process.running_pid()?;
    let meta = process.read_meta()?;
    let bound_addr = meta
        .as_ref()
        .map(|m| format!("{}://{}:{}", m.scheme, m.bind_ip, m.port));
    Ok(SquadDaemonOutcome::Status(DaemonStatus {
        running: pid.is_some(),
        pid,
        bound_addr,
        task_count: 0,
        active_count: 0,
        last_tick: None,
        in_flight: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::tail_new_lines;

    #[test]
    fn tail_reads_the_whole_file_on_the_first_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("awman.log");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let (lines, end) = tail_new_lines(&path, 0).unwrap().unwrap();
        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(end, 8);
    }

    #[test]
    fn tail_streams_only_lines_appended_after_the_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("awman.log");
        std::fs::write(&path, "one\n").unwrap();
        let (_, end) = tail_new_lines(&path, 0).unwrap().unwrap();
        // Append more; a follow tick from `end` yields only the new line.
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let (lines, end2) = tail_new_lines(&path, end).unwrap().unwrap();
        assert_eq!(lines, vec!["two".to_string()]);
        assert_eq!(end2, 8);
    }

    #[test]
    fn tail_does_not_emit_a_partial_final_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("awman.log");
        // No trailing newline: the partial line must be withheld and re-read.
        std::fs::write(&path, "complete\npartial").unwrap();
        let (lines, end) = tail_new_lines(&path, 0).unwrap().unwrap();
        assert_eq!(lines, vec!["complete".to_string()]);
        assert_eq!(end, 9, "offset advances only past the last newline");
        // Once the line is completed, the next tick emits it in full.
        std::fs::write(&path, "complete\npartial done\n").unwrap();
        let (lines, _) = tail_new_lines(&path, end).unwrap().unwrap();
        assert_eq!(lines, vec!["partial done".to_string()]);
    }

    #[test]
    fn tail_reports_a_missing_file_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.log");
        assert!(tail_new_lines(&path, 0).unwrap().is_none());
    }
}
