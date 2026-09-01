//! `DaemonGuard` — cross-daemon mutual exclusion.
//!
//! `awman api` and the squad daemon both open the shared sqlite database. Only
//! one long-lived process may hold it at a time, which removes multi-process
//! SQLite contention on squad data by construction. `DaemonGuard` enforces that:
//! before opening the database, a starting daemon checks that the *other*
//! daemon is not alive, claims its own pidfile, then checks again to close the
//! window where both pass an initial check before either commits.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::data::config::env::EnvSnapshot;
use crate::data::error::DataError;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::fs::daemon_process::{
    DaemonProcess, API_PLIST_LABEL, API_UNIT_NAME, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME,
};
use crate::data::fs::data_paths::DataPaths;
use crate::data::fs::squad_paths::SquadPaths;

/// Name of the shared startup-arbitration lock. It lives beside the shared
/// database because that is the one directory both daemons already agree on.
const STARTUP_LOCK_FILENAME: &str = ".daemon-startup.lock";

/// How long a startup lock may sit before it is treated as abandoned. Acquiring
/// it covers only a check → claim → check sequence, so anything older is the
/// residue of a process that died mid-start.
const STALE_STARTUP_LOCK_AGE: Duration = Duration::from_secs(30);

/// How long a starting daemon waits for the other one to finish arbitrating.
const STARTUP_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Why a daemon could not claim the machine.
#[derive(Debug)]
pub enum AcquireError {
    /// *This* daemon is already running under `pid`. Callers map this to their
    /// own already-running error so existing messages are preserved.
    AlreadyRunning { pid: u32 },
    /// The other daemon holds the machine, or the claim itself failed.
    Blocked(DataError),
}

impl AcquireError {
    /// Collapse back to a `DataError` for callers that have no distinct
    /// already-running error of their own.
    pub fn into_data_error(self) -> DataError {
        match self {
            AcquireError::AlreadyRunning { pid } => {
                DataError::Other(format!("this daemon is already running (PID {pid})"))
            }
            AcquireError::Blocked(error) => error,
        }
    }
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AlreadyRunning { pid } => {
                write!(f, "this daemon is already running (PID {pid})")
            }
            AcquireError::Blocked(error) => write!(f, "{error}"),
        }
    }
}

impl From<DataError> for AcquireError {
    fn from(error: DataError) -> Self {
        AcquireError::Blocked(error)
    }
}

/// An exclusively-created marker file held across one daemon's whole
/// check → claim → check sequence.
///
/// The double check alone leaves a window in which two simultaneous starters
/// each observe the other and both back off, so a clean race could end with
/// *zero* winners. Serialising the sequence behind this lock makes the outcome
/// exactly one winner: the second starter does not begin checking until the
/// first has committed its pidfile.
struct StartupLock {
    path: PathBuf,
}

impl StartupLock {
    fn acquire(root: &Path) -> Result<Self, DataError> {
        std::fs::create_dir_all(root).map_err(|e| DataError::io(root, e))?;
        let path = root.join(STARTUP_LOCK_FILENAME);
        let deadline = SystemTime::now() + STARTUP_LOCK_WAIT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_stale(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if SystemTime::now() >= deadline {
                        return Err(DataError::Other(format!(
                            "another awman daemon is starting (lock held at {}); \
                             retry in a moment",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(DataError::io(&path, e)),
            }
        }
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .and_then(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_err(|_| std::io::Error::other("clock skew"))
        })
        .map(|age| age > STALE_STARTUP_LOCK_AGE)
        .unwrap_or(false)
}

/// Which daemon a guard is being acquired for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonKind {
    Api,
    Squad,
}

impl DaemonKind {
    /// Human-facing daemon name for error text.
    pub fn display_name(&self) -> &'static str {
        match self {
            DaemonKind::Api => "awman api",
            DaemonKind::Squad => "the squad daemon",
        }
    }

    /// The command that stops this daemon (for the "run X first" hint).
    pub fn stop_hint(&self) -> &'static str {
        match self {
            DaemonKind::Api => "awman api kill",
            DaemonKind::Squad => "awman squad stop",
        }
    }
}

/// Guards a daemon start against the other daemon already running.
pub struct DaemonGuard {
    this: DaemonKind,
    api: DaemonProcess,
    squad: DaemonProcess,
    /// Directory holding the shared startup-arbitration lock.
    arbiter_root: PathBuf,
}

impl DaemonGuard {
    /// Build both daemon processes from the environment.
    pub fn for_daemon(this: DaemonKind, env: &EnvSnapshot) -> Result<Self, DataError> {
        let api_paths = ApiPaths::from_env(env)?;
        let squad_paths = SquadPaths::from_env(env)?;
        Ok(Self::with_paths(this, &api_paths, &squad_paths))
    }

    /// Build from explicitly-resolved paths.
    ///
    /// Callers that already thread an `ApiPaths` (the `awman api` command does)
    /// must use this, so the pidfile the guard claims is the same one the rest
    /// of the command reads. The shared arbitration lock is scoped to those
    /// paths' data root for the same reason.
    pub fn with_paths(this: DaemonKind, api_paths: &ApiPaths, squad_paths: &SquadPaths) -> Self {
        let api = DaemonProcess::new(api_paths.daemon(), API_UNIT_NAME, API_PLIST_LABEL);
        let squad = DaemonProcess::new(squad_paths.daemon(), SQUAD_UNIT_NAME, SQUAD_PLIST_LABEL);
        let arbiter_root = api_paths.data_paths().root().to_path_buf();
        Self {
            this,
            api,
            squad,
            arbiter_root,
        }
    }

    fn this_process(&self) -> &DaemonProcess {
        match self.this {
            DaemonKind::Api => &self.api,
            DaemonKind::Squad => &self.squad,
        }
    }

    fn other(&self) -> (&DaemonProcess, DaemonKind) {
        match self.this {
            DaemonKind::Api => (&self.squad, DaemonKind::Squad),
            DaemonKind::Squad => (&self.api, DaemonKind::Api),
        }
    }

    /// Error if the *other* daemon is alive, naming it and its PID. A stale or
    /// absent pidfile is `Ok(())` — `running_pid` cleans stale files itself.
    pub fn check(&self) -> Result<(), DataError> {
        let (other, other_kind) = self.other();
        if let Some(pid) = other.running_pid()? {
            return Err(DataError::Other(format!(
                "{} is already running (PID {pid}). Run `{}` first; \
                 awman api and the squad daemon cannot run at the same time.",
                other_kind.display_name(),
                other_kind.stop_hint(),
            )));
        }
        Ok(())
    }

    /// Claim the machine for this daemon: take the shared startup lock, then
    /// `check()` → claim this daemon's pidfile → `check()` again.
    ///
    /// The shared lock is what makes a clean concurrent start produce *exactly*
    /// one winner: the second starter cannot begin checking until the first has
    /// committed (or released) its pidfile. The second check is retained as a
    /// defence against a daemon that started outside this protocol; a loser
    /// releases its own pidfile rather than leaving a stale one.
    pub fn acquire(&self, pid: u32) -> Result<(), AcquireError> {
        let _lock = StartupLock::acquire(&self.arbiter_root)?;
        self.check()?;

        if !self.this_process().claim_pidfile(pid)? {
            // Our pidfile already exists. If it names a live awman process,
            // this daemon is already running; otherwise it was stale (and
            // running_pid just cleaned it) so we retake it.
            if let Some(existing) = self.this_process().running_pid()? {
                return Err(AcquireError::AlreadyRunning { pid: existing });
            }
            self.this_process().force_write_pidfile(pid)?;
        }

        // Second check closes the both-passed-initial-check window. On failure,
        // release our pidfile before surfacing the error.
        if let Err(e) = self.check() {
            let _ = self.this_process().release_pidfile();
            return Err(AcquireError::Blocked(e));
        }
        Ok(())
    }

    /// Release this daemon's pidfile.
    pub fn release(&self) -> Result<(), DataError> {
        self.this_process().release_pidfile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{AWMAN_API_ROOT, AWMAN_CONFIG_HOME, AWMAN_SQUAD_ROOT};

    /// `AWMAN_CONFIG_HOME` is set as well so the shared startup-arbitration
    /// lock (which lives beside the shared database) also lands in the
    /// fixture, never in the developer's real `~/.awman`.
    fn env_for(api_root: &std::path::Path, squad_root: &std::path::Path) -> EnvSnapshot {
        EnvSnapshot::with_overrides([
            (AWMAN_API_ROOT, api_root.to_str().unwrap()),
            (AWMAN_SQUAD_ROOT, squad_root.to_str().unwrap()),
            (
                AWMAN_CONFIG_HOME,
                api_root.parent().unwrap_or(api_root).to_str().unwrap(),
            ),
        ])
    }

    #[test]
    fn kind_names_and_hints_differ() {
        assert_ne!(
            DaemonKind::Api.display_name(),
            DaemonKind::Squad.display_name()
        );
        assert_ne!(DaemonKind::Api.stop_hint(), DaemonKind::Squad.stop_hint());
    }

    #[test]
    fn acquire_then_release_round_trip() {
        let api = tempfile::tempdir().unwrap();
        let squad = tempfile::tempdir().unwrap();
        let env = env_for(api.path(), squad.path());
        let guard = DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap();
        guard.acquire(std::process::id()).unwrap();
        // Our pidfile now exists.
        assert!(ApiPaths::from_env(&env).unwrap().pid_file().exists());
        guard.release().unwrap();
        assert!(!ApiPaths::from_env(&env).unwrap().pid_file().exists());
    }

    #[test]
    fn check_errors_when_other_daemon_alive() {
        let api = tempfile::tempdir().unwrap();
        let squad = tempfile::tempdir().unwrap();
        let env = env_for(api.path(), squad.path());

        // Squad daemon "runs" — claim its pidfile with our own (live, awman) PID.
        let squad_guard = DaemonGuard::for_daemon(DaemonKind::Squad, &env).unwrap();
        squad_guard.acquire(std::process::id()).unwrap();

        // The API guard's cross-check must now fail, naming the squad daemon.
        let api_guard = DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap();
        match api_guard.check() {
            Err(DataError::Other(msg)) => {
                assert!(
                    msg.contains("squad"),
                    "message must name the squad daemon: {msg}"
                );
                assert!(
                    msg.contains(&std::process::id().to_string()),
                    "message must include the PID: {msg}"
                );
            }
            other => panic!("expected cross-daemon error, got {other:?}"),
        }
        squad_guard.release().unwrap();
    }

    #[test]
    fn absent_other_pidfile_is_ok() {
        let api = tempfile::tempdir().unwrap();
        let squad = tempfile::tempdir().unwrap();
        let env = env_for(api.path(), squad.path());
        let guard = DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap();
        assert!(guard.check().is_ok());
    }
}
