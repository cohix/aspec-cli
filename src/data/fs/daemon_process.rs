//! `DaemonProcess` — PID-file lifecycle, background spawn, and server-meta
//! persistence for a long-lived awman daemon (the API server or amie).
//!
//! Ported from the former `api_process.rs` module of free functions. The
//! path-bearing operations become methods on a typed object owning a
//! `DaemonPaths` plus a systemd unit name / launchd plist label, so two
//! daemons no longer collide on `--unit=awman-api` or the `io.awman.api` plist.
//! The genuinely stateless process-identity helpers (`is_process_alive`,
//! `pid_is_awman`) stay free functions — the grand architecture permits that
//! category.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;

/// systemd unit name / launchd plist label for the API daemon.
pub const API_UNIT_NAME: &str = "awman-api";
pub const API_PLIST_LABEL: &str = "io.awman.api";

/// systemd unit name / launchd plist label for the amie daemon.
pub const AMIE_UNIT_NAME: &str = "awman-amie";
pub const AMIE_PLIST_LABEL: &str = "io.awman.amie";

/// Sidecar metadata for a running daemon. Written next to the PID file when
/// the server boots so other commands (status, kill) can locate the bound
/// endpoint without re-parsing flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerMeta {
    pub port: u16,
    pub bind_ip: String,
    pub scheme: String,
    /// True when the daemon was started with `--dangerously-skip-auth` and is
    /// therefore serving unauthenticated. Clients read this to avoid minting a
    /// bearer key (and writing a key hash) the running daemon will never check.
    /// Absent in sidecars written by older versions, which always required auth.
    #[serde(default)]
    pub auth_disabled: bool,
}

/// Typed owner of one daemon's PID / meta / spawn lifecycle.
pub struct DaemonProcess {
    paths: DaemonPaths,
    unit_name: &'static str,
    plist_label: &'static str,
}

impl DaemonProcess {
    /// Construct over a daemon's paths and its systemd/launchd identity.
    pub fn new(paths: DaemonPaths, unit_name: &'static str, plist_label: &'static str) -> Self {
        Self {
            paths,
            unit_name,
            plist_label,
        }
    }

    /// The daemon's paths.
    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    /// Return the running daemon PID only when the process is alive AND looks
    /// like an awman server. Stale or wrong-process PIDs are cleaned up.
    /// (Was `check_already_running`.)
    pub fn running_pid(&self) -> Result<Option<u32>, DataError> {
        check_already_running(&self.paths.pid_file())
    }

    /// Raw PID read with no liveness check.
    pub fn read_pid(&self) -> Result<Option<u32>, DataError> {
        read_pid(&self.paths.pid_file())
    }

    /// Race-safe exclusive PID claim (`O_CREAT|O_EXCL`). Returns `Ok(false)`
    /// when the file already exists. (Was `write_pid_exclusive`.)
    pub fn claim_pidfile(&self, pid: u32) -> Result<bool, DataError> {
        write_pid_exclusive(&self.paths.pid_file(), pid)
    }

    /// Truncating PID overwrite. (Was `write_pid`.)
    pub fn force_write_pidfile(&self, pid: u32) -> Result<(), DataError> {
        write_pid(&self.paths.pid_file(), pid)
    }

    /// Remove the PID file (idempotent). (Was `clear_pid`.)
    pub fn release_pidfile(&self) -> Result<(), DataError> {
        clear_pid(&self.paths.pid_file())
    }

    /// Spawn the daemon binary in the background, returning the child PID.
    /// Threads this daemon's unit name / plist label / log path through, so
    /// two daemons never collide on the systemd unit or the launchd plist.
    pub fn spawn_detached(&self, binary: &Path, args: &[String]) -> Result<u32, DataError> {
        spawn_background(
            binary,
            args,
            &self.paths.log_file(),
            self.unit_name,
            self.plist_label,
        )
    }

    /// Terminate the running daemon: read the PID, and if it is a live awman
    /// process, send SIGTERM, then release the PID file. A stale/absent PID is
    /// a no-op that still clears the file.
    pub fn terminate(&self) -> Result<(), DataError> {
        self.terminate_running()?;
        Ok(())
    }

    /// Terminate this daemon and report what was actually found, so callers can
    /// render their own "stopped" / "stale pidfile" messages without reaching
    /// for a free OS function. The pidfile is released on every outcome.
    pub fn terminate_running(&self) -> Result<Termination, DataError> {
        let outcome = match self.read_pid()? {
            None => Termination::NotRunning,
            Some(pid) if !is_process_alive(pid) => Termination::StalePidFile { pid },
            Some(pid) if !pid_is_awman(pid) => Termination::NotAwman { pid },
            Some(pid) => {
                kill_process(pid)?;
                Termination::Terminated { pid }
            }
        };
        self.release_pidfile()?;
        Ok(outcome)
    }

    /// Persist server bind metadata.
    pub fn write_meta(&self, meta: &ServerMeta) -> Result<(), DataError> {
        write_server_meta(&self.paths.server_meta_file(), meta)
    }

    /// Read server bind metadata, or `None` when absent.
    pub fn read_meta(&self) -> Result<Option<ServerMeta>, DataError> {
        read_server_meta(&self.paths.server_meta_file())
    }

    /// Remove the server metadata file (idempotent).
    pub fn clear_meta(&self) -> Result<(), DataError> {
        clear_server_meta(&self.paths.server_meta_file())
    }
}

// ─── Ported free functions (now pub(crate) implementation details) ──────────

/// Truncating PID write — overwrites whatever is already on disk.
pub(crate) fn write_pid(pid_path: &Path, pid: u32) -> Result<(), DataError> {
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    std::fs::write(pid_path, pid.to_string()).map_err(|e| DataError::io(pid_path, e))
}

/// Race-safe PID write via `O_CREAT|O_EXCL`. `Ok(false)` when the file exists.
///
/// Writes the content to a private temp file first, then publishes it with
/// `hard_link` (which fails with `AlreadyExists` exactly like `create_new`
/// would). Publishing this way — rather than `create_new` followed by a
/// separate `write_all` — closes a real race: a concurrent reader (e.g. a
/// second daemon's `DaemonGuard::check` racing this claim) could otherwise
/// observe the freshly-created-but-still-empty pidfile between the two
/// syscalls and fail with a spurious "invalid PID" error instead of either
/// seeing the claim or finding nothing.
pub(crate) fn write_pid_exclusive(pid_path: &Path, pid: u32) -> Result<bool, DataError> {
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    let tmp_path = pid_path.with_file_name(format!(
        "{}.tmp.{}",
        pid_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pidfile"),
        std::process::id()
    ));
    std::fs::write(&tmp_path, pid.to_string()).map_err(|e| DataError::io(&tmp_path, e))?;
    let result = std::fs::hard_link(&tmp_path, pid_path);
    let _ = std::fs::remove_file(&tmp_path);
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

pub(crate) fn read_pid(pid_path: &Path) -> Result<Option<u32>, DataError> {
    match std::fs::read_to_string(pid_path) {
        Ok(content) => {
            let pid: u32 = content
                .trim()
                .parse()
                .map_err(|_| DataError::Other(format!("invalid PID in {}", pid_path.display())))?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

pub(crate) fn clear_pid(pid_path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(pid_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

/// Persist server bind metadata (port, scheme, bind IP).
pub(crate) fn write_server_meta(meta_path: &Path, meta: &ServerMeta) -> Result<(), DataError> {
    if let Some(parent) = meta_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    let json = serde_json::to_string(meta)
        .map_err(|e| DataError::Other(format!("serialize ServerMeta: {e}")))?;
    std::fs::write(meta_path, json).map_err(|e| DataError::io(meta_path, e))
}

pub(crate) fn read_server_meta(meta_path: &Path) -> Result<Option<ServerMeta>, DataError> {
    match std::fs::read_to_string(meta_path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| DataError::Other(format!("parse ServerMeta: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(meta_path, e)),
    }
}

pub(crate) fn clear_server_meta(meta_path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(meta_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(meta_path, e)),
    }
}

/// Whether the OS reports the process is alive. Stateless — stays a free
/// function per the grand architecture's permitted exception.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!(",\"{}\",", pid)))
        .unwrap_or(false)
}

/// Whether the OS reports the process command name contains "awman". Used to
/// disambiguate stale PID files from reused PIDs. Stateless — stays a free
/// function. On platforms where the command name is unreadable, returns `true`
/// (trust the PID file), matching old-awman.
#[cfg(target_os = "linux")]
pub fn pid_is_awman(pid: u32) -> bool {
    let path = format!("/proc/{pid}/comm");
    std::fs::read_to_string(&path)
        .map(|s| s.trim().contains("awman"))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn pid_is_awman(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().contains("awman"))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
pub fn pid_is_awman(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("awman")
        })
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn pid_is_awman(_pid: u32) -> bool {
    true
}

/// Check the PID file. Returns `Some(pid)` only when the process is alive AND
/// looks like an awman server. Stale or wrong-process PIDs are cleaned up.
pub(crate) fn check_already_running(pid_path: &Path) -> Result<Option<u32>, DataError> {
    match read_pid(pid_path)? {
        Some(pid) if is_process_alive(pid) && pid_is_awman(pid) => Ok(Some(pid)),
        Some(_) => {
            clear_pid(pid_path)?;
            Ok(None)
        }
        None => Ok(None),
    }
}

/// What [`DaemonProcess::terminate_running`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// No pidfile at all.
    NotRunning,
    /// The pidfile named a process that is no longer alive.
    StalePidFile { pid: u32 },
    /// The pidfile named a live process that is not an awman daemon.
    NotAwman { pid: u32 },
    /// A live awman daemon was signalled.
    Terminated { pid: u32 },
}

#[cfg(unix)]
fn kill_process(pid: u32) -> Result<(), DataError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| DataError::Other(format!("failed to send SIGTERM to PID {pid}: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn kill_process(pid: u32) -> Result<(), DataError> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .map_err(|e| DataError::Other(format!("failed to terminate PID {pid}: {e}")))?;
    if !status.success() {
        return Err(DataError::Other(format!("taskkill /PID {pid} /F failed")));
    }
    Ok(())
}

/// Spawn the daemon in the background. Returns the child PID. `unit_name` /
/// `plist_label` are threaded into the systemd / launchd happy paths so
/// distinct daemons never share a unit or plist.
pub(crate) fn spawn_background(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
    unit_name: &str,
    plist_label: &str,
) -> Result<u32, DataError> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    // Create the log ourselves so it exists with owner-only permissions before
    // launchd/systemd starts appending to it at the process umask. A daemon log
    // can capture startup diagnostics that should not be world-readable.
    ensure_private_log(log_path)?;

    // Each happy path consumes only its own identity; silence the other on
    // platforms that don't use it.
    #[cfg(target_os = "linux")]
    {
        let _ = plist_label;
        if let Some(pid) = try_systemd_run(binary_path, args, unit_name)? {
            return Ok(pid);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = unit_name;
        if let Some(pid) = try_launchd(binary_path, args, log_path, plist_label)? {
            return Ok(pid);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (unit_name, plist_label);
    }

    double_fork_spawn(binary_path, args)
}

#[cfg(target_os = "linux")]
fn try_systemd_run(
    binary_path: &Path,
    args: &[String],
    unit_name: &str,
) -> Result<Option<u32>, DataError> {
    let check = std::process::Command::new("systemd-run")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match check {
        Ok(s) if s.success() => {}
        _ => return Ok(None),
    }

    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", &format!("--unit={unit_name}"), "--"])
        .arg(binary_path)
        .args(args);

    let status = cmd
        .status()
        .map_err(|e| DataError::Other(format!("systemd-run failed: {e}")))?;
    if !status.success() {
        return Ok(None);
    }
    // systemd-run returns immediately; the actual PID is tracked by the unit.
    // Return 0 as a sentinel — the PID file will be written by the child.
    Ok(Some(0))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn try_launchd(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
    plist_label: &str,
) -> Result<Option<u32>, DataError> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let plist_path = home.join(format!("Library/LaunchAgents/{plist_label}.plist"));
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }

    let mut program_args = format!(
        "    <string>{}</string>\n",
        xml_escape(&binary_path.to_string_lossy())
    );
    for arg in args {
        program_args.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(plist_label),
        log = xml_escape(&log_path.to_string_lossy())
    );

    std::fs::write(&plist_path, plist).map_err(|e| DataError::io(&plist_path, e))?;

    let status = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status()
        .map_err(|e| DataError::Other(format!("launchctl load failed: {e}")))?;

    if !status.success() {
        let _ = std::fs::remove_file(&plist_path);
        return Ok(None);
    }
    Ok(Some(0))
}

/// Create (or tighten) the daemon log file so it is owner-read/write only.
/// Existing files keep their contents; only the mode is enforced.
fn ensure_private_log(log_path: &Path) -> Result<(), DataError> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(log_path)
        .map_err(|e| DataError::io(log_path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(log_path)
            .map_err(|e| DataError::io(log_path, e))?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(log_path, perms).map_err(|e| DataError::io(log_path, e))?;
    }
    Ok(())
}

fn double_fork_spawn(binary_path: &Path, args: &[String]) -> Result<u32, DataError> {
    let mut cmd = std::process::Command::new(binary_path);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // On Unix this matches old-amux exactly: a single Command::spawn. True
    // setsid daemonization would require `pre_exec`, which is unsafe — and this
    // crate is `#![forbid(unsafe_code)]`. The systemd-run / launchd happy paths
    // above handle real detachment when the OS supports it.

    // On Windows, ensure the child gets its own process group so a Ctrl-C
    // delivered to the parent console does not also kill the daemon.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NEW_PROCESS_GROUP = 0x00000200
        cmd.creation_flags(0x00000200);
    }

    let child = cmd
        .spawn()
        .map_err(|e| DataError::Other(format!("failed to spawn background server: {e}")))?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_pid_exclusive_rejects_second_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("excl.pid");
        let r1 = write_pid_exclusive(&pid_path, 100).unwrap();
        assert!(r1, "first exclusive write must succeed");
        let r2 = write_pid_exclusive(&pid_path, 200).unwrap();
        assert!(!r2, "second exclusive write must be rejected");
        let on_disk = read_pid(&pid_path).unwrap();
        assert_eq!(on_disk, Some(100), "first writer's PID must survive");
    }

    #[test]
    fn pid_file_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("test.pid");
        write_pid(&pid_path, 12345).unwrap();
        assert_eq!(read_pid(&pid_path).unwrap(), Some(12345));
        clear_pid(&pid_path).unwrap();
        assert_eq!(read_pid(&pid_path).unwrap(), None);
    }

    #[test]
    fn clear_pid_idempotent_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("nonexistent.pid");
        assert!(clear_pid(&pid_path).is_ok());
    }

    #[test]
    fn is_process_alive_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn pid_is_awman_returns_false_for_a_clearly_non_awman_pid() {
        assert!(!pid_is_awman(1), "PID 1 is not awman");
    }

    #[test]
    fn check_already_running_for_unrelated_alive_pid_treats_as_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("foreign.pid");
        write_pid(&pid_path, 1).unwrap();
        let result = check_already_running(&pid_path).unwrap();
        assert!(
            result.is_none(),
            "unrelated alive PID must be treated as stale"
        );
        assert!(!pid_path.exists(), "stale PID file must be removed");
    }

    #[test]
    fn check_already_running_stale_pid_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("stale.pid");
        write_pid(&pid_path, u32::MAX - 1).unwrap();
        let result = check_already_running(&pid_path).unwrap();
        assert!(result.is_none());
        assert!(!pid_path.exists());
    }

    // ─── DaemonProcess method surface ────────────────────────────────────────

    fn api_daemon(root: &Path) -> DaemonProcess {
        DaemonProcess::new(
            DaemonPaths::new(root, "api_key"),
            API_UNIT_NAME,
            API_PLIST_LABEL,
        )
    }

    #[test]
    fn claim_and_release_pidfile_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        assert!(d.claim_pidfile(4242).unwrap(), "first claim wins");
        assert!(!d.claim_pidfile(9999).unwrap(), "second claim rejected");
        assert_eq!(d.read_pid().unwrap(), Some(4242));
        d.release_pidfile().unwrap();
        assert_eq!(d.read_pid().unwrap(), None);
    }

    #[test]
    fn meta_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        assert_eq!(d.read_meta().unwrap(), None);
        let meta = ServerMeta {
            port: 8080,
            bind_ip: "127.0.0.1".into(),
            scheme: "https".into(),
            auth_disabled: false,
        };
        d.write_meta(&meta).unwrap();
        assert_eq!(d.read_meta().unwrap(), Some(meta));
        d.clear_meta().unwrap();
        assert_eq!(d.read_meta().unwrap(), None);
    }

    #[test]
    fn distinct_unit_and_plist_for_api_and_amie() {
        assert_ne!(API_UNIT_NAME, AMIE_UNIT_NAME);
        assert_ne!(API_PLIST_LABEL, AMIE_PLIST_LABEL);
    }
}
