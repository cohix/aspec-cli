//! Part 0 daemon primitive regression tests.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Mutex;

use awman::data::config::env::{EnvSnapshot, AWMAN_API_ROOT, AWMAN_SQUAD_ROOT};
use awman::data::fs::daemon_process::{
    DaemonProcess, ServerMeta, API_PLIST_LABEL, API_UNIT_NAME, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME,
};
use awman::data::fs::{DaemonGuard, DaemonKind, DaemonPaths, SquadPaths};

/// A few daemon-spawn tests temporarily replace `PATH` to inject a platform
/// launcher.  Keep that process-global state serialized across the whole test
/// module so one injection cannot change another test's meaning.
#[cfg(any(target_os = "linux", target_os = "macos"))]
static PATH_LOCK: Mutex<()> = Mutex::new(());

fn paths_env(api_root: &Path, squad_root: &Path) -> EnvSnapshot {
    EnvSnapshot::with_overrides([
        (AWMAN_API_ROOT, api_root.to_string_lossy().to_string()),
        (AWMAN_SQUAD_ROOT, squad_root.to_string_lossy().to_string()),
    ])
}

fn daemon(root: &Path, kind: DaemonKind) -> DaemonProcess {
    match kind {
        DaemonKind::Api => DaemonProcess::new(
            DaemonPaths::new(root, "api_key"),
            API_UNIT_NAME,
            API_PLIST_LABEL,
        ),
        DaemonKind::Squad => DaemonProcess::new(
            DaemonPaths::new(root, "squad_key"),
            SQUAD_UNIT_NAME,
            SQUAD_PLIST_LABEL,
        ),
    }
}

#[test]
fn daemon_paths_preserve_api_filenames_and_isolate_squad_key() {
    let api = DaemonPaths::new("/tmp/api", "api_key");
    assert_eq!(api.pid_file(), PathBuf::from("/tmp/api/awman.pid"));
    assert_eq!(api.log_file(), PathBuf::from("/tmp/api/awman.log"));
    assert_eq!(
        api.server_meta_file(),
        PathBuf::from("/tmp/api/server.json")
    );
    assert_eq!(api.key_hash_file(), PathBuf::from("/tmp/api/api_key.hash"));

    let squad = SquadPaths::from_root("/tmp/squad").daemon();
    assert_eq!(squad.pid_file(), PathBuf::from("/tmp/squad/awman.pid"));
    assert_eq!(squad.log_file(), PathBuf::from("/tmp/squad/awman.log"));
    assert_eq!(
        squad.server_meta_file(),
        PathBuf::from("/tmp/squad/server.json")
    );
    assert_eq!(
        squad.key_hash_file(),
        PathBuf::from("/tmp/squad/squad_key.hash")
    );
    assert_ne!(api.key_hash_file(), squad.key_hash_file());
}

#[test]
fn daemon_process_pidfile_and_server_meta_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let process = daemon(tmp.path(), DaemonKind::Api);

    assert_eq!(process.read_pid().unwrap(), None);
    assert!(process.claim_pidfile(4242).unwrap());
    assert!(!process.claim_pidfile(9999).unwrap());
    assert_eq!(process.read_pid().unwrap(), Some(4242));
    process.release_pidfile().unwrap();
    assert_eq!(process.read_pid().unwrap(), None);

    let meta = ServerMeta {
        port: 3210,
        bind_ip: "127.0.0.1".into(),
        scheme: "http".into(),
        auth_disabled: false,
    };
    assert_eq!(process.read_meta().unwrap(), None);
    process.write_meta(&meta).unwrap();
    assert_eq!(process.read_meta().unwrap(), Some(meta));
    process.clear_meta().unwrap();
    assert_eq!(process.read_meta().unwrap(), None);
}

#[test]
fn spawn_detached_identity_is_distinct_for_api_and_squad() {
    let api_root = tempfile::tempdir().unwrap();
    let squad_root = tempfile::tempdir().unwrap();
    let api = daemon(api_root.path(), DaemonKind::Api);
    let squad = daemon(squad_root.path(), DaemonKind::Squad);

    assert_ne!(API_UNIT_NAME, SQUAD_UNIT_NAME);
    assert_ne!(API_PLIST_LABEL, SQUAD_PLIST_LABEL);
    assert_ne!(api.paths().log_file(), squad.paths().log_file());

    #[cfg(target_os = "linux")]
    {
        let _lock = PATH_LOCK.lock().unwrap();
        let bin_dir = api_root.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let log = api_root.path().join("systemd-run-args");
        let fake_systemd = bin_dir.join("systemd-run");
        std::fs::write(
            &fake_systemd,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nprintf '%s\\n' \"$*\" >> \"$AWMAN_TEST_SYSTEMD_LOG\"\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_systemd, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var_os("PATH");
        let new_path = match old_path.as_ref() {
            Some(path) => format!("{}:{}", bin_dir.display(), path.to_string_lossy()),
            None => bin_dir.display().to_string(),
        };
        std::env::set_var("PATH", new_path);
        std::env::set_var("AWMAN_TEST_SYSTEMD_LOG", &log);
        assert_eq!(api.spawn_detached(Path::new("/bin/true"), &[]).unwrap(), 0);
        assert_eq!(
            squad.spawn_detached(Path::new("/bin/true"), &[]).unwrap(),
            0
        );
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("AWMAN_TEST_SYSTEMD_LOG");
        let args = std::fs::read_to_string(log).unwrap();
        assert!(
            args.contains("--unit=awman-api"),
            "API unit was not threaded: {args}"
        );
        assert!(
            args.contains("--unit=awman-squad"),
            "squad unit was not threaded: {args}"
        );
    }

    // The launchd path is the macOS equivalent: the plist filename *and* its
    // `Label` key must both be the daemon's own, or the second daemon would
    // overwrite the first's launch agent. `try_launchd` resolves the plist
    // under `dirs::home_dir()` and shells out to `launchctl load`, so a
    // scoped `HOME` plus a stub `launchctl` on `PATH` exercises it for real.
    #[cfg(target_os = "macos")]
    {
        // The module-wide lock, not a private one: this block installs a
        // `launchctl` stub that reports *success*, and the spawn-failure test
        // installs one that reports failure. Two locks over one process-global
        // `PATH` serialize nothing, and whichever stub happened to be
        // installed would decide the other test's result.
        let _lock = PATH_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake_launchctl = bin_dir.join("launchctl");
        std::fs::write(&fake_launchctl, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var_os("PATH");
        let old_home = std::env::var_os("HOME");
        std::env::set_var(
            "PATH",
            match old_path.as_ref() {
                Some(path) => format!("{}:{}", bin_dir.display(), path.to_string_lossy()),
                None => bin_dir.display().to_string(),
            },
        );
        std::env::set_var("HOME", home.path());

        api.spawn_detached(Path::new("/usr/bin/true"), &[]).unwrap();
        squad
            .spawn_detached(Path::new("/usr/bin/true"), &[])
            .unwrap();

        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }

        let agents = home.path().join("Library/LaunchAgents");
        let api_plist = std::fs::read_to_string(agents.join(format!("{API_PLIST_LABEL}.plist")))
            .expect("the API daemon must write its own plist");
        let squad_plist =
            std::fs::read_to_string(agents.join(format!("{SQUAD_PLIST_LABEL}.plist")))
                .expect("the squad daemon must write its own plist, not overwrite the API's");
        assert!(api_plist.contains(&format!("<string>{API_PLIST_LABEL}</string>")));
        assert!(squad_plist.contains(&format!("<string>{SQUAD_PLIST_LABEL}</string>")));
        // Each daemon's stdout goes to its own log, never the other's.
        assert!(api_plist.contains(&api.paths().log_file().display().to_string()));
        assert!(squad_plist.contains(&squad.paths().log_file().display().to_string()));
        assert!(!api_plist.contains(&squad.paths().log_file().display().to_string()));
    }
}

#[test]
fn squad_spawn_failure_is_wrapped_with_an_attributable_daemon_message() {
    let tmp = tempfile::tempdir().unwrap();
    let process = daemon(tmp.path(), DaemonKind::Squad);

    // Force the portable `double_fork_spawn` path.  Both platform launchers
    // *accept* a job before its executable fails — a real systemd-run accepts
    // the unit, and `launchctl load` accepts a plist naming a binary that does
    // not exist — which is correct production behavior but not a synchronous
    // failure injection.  Stubbing the launcher so it reports itself
    // unavailable is what makes the missing binary observable here.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _path_guard = {
        let _lock = PATH_LOCK.lock().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let launcher = bin_dir.join(if cfg!(target_os = "macos") {
            "launchctl"
        } else {
            "systemd-run"
        });
        std::fs::write(&launcher, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", &bin_dir);
        PathGuard { _lock, old_path }
    };

    let error = process
        .spawn_detached(Path::new("/definitely-not-an-awman-daemon-binary"), &[])
        .expect_err("a missing daemon binary must surface a spawn failure");
    let text = error.to_string();
    assert!(
        text.starts_with("failed to start the squad daemon:"),
        "daemon startup errors must be attributable instead of raw OS diagnostics: {text}"
    );
    assert!(
        !text.starts_with("Load failed:"),
        "the raw launchd diagnostic must never be surfaced verbatim: {text}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PathGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    old_path: Option<std::ffi::OsString>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.old_path.take() {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
}

#[test]
fn daemon_guard_check_accepts_absent_and_stale_pidfiles_in_both_directions() {
    let tmp = tempfile::tempdir().unwrap();
    let api_root = tmp.path().join("api");
    let squad_root = tmp.path().join("squad");
    let env = paths_env(&api_root, &squad_root);

    for kind in [DaemonKind::Api, DaemonKind::Squad] {
        let guard = DaemonGuard::for_daemon(kind, &env).unwrap();
        assert!(
            guard.check().is_ok(),
            "absent pidfile must be allowed: {kind:?}"
        );

        let other_kind = match kind {
            DaemonKind::Api => DaemonKind::Squad,
            DaemonKind::Squad => DaemonKind::Api,
        };
        let other_root = match other_kind {
            DaemonKind::Api => &api_root,
            DaemonKind::Squad => &squad_root,
        };
        daemon(other_root, other_kind)
            .force_write_pidfile(u32::MAX - 1)
            .unwrap();
        assert!(
            guard.check().is_ok(),
            "stale pidfile must be allowed: {kind:?}"
        );
        assert!(!daemon(other_root, other_kind).paths().pid_file().exists());
    }
}

#[cfg(unix)]
fn awman_named_test_process() -> Child {
    // The guard deliberately rejects a live PID belonging to an unrelated
    // process. Copying the test harness to an `awman` basename gives the
    // child the same process identity a real daemon has without starting a
    // server or opening a database.
    let source = std::env::current_exe().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let helper = tmp.path().join("awman");
    std::fs::copy(source, &helper).unwrap();
    // A concurrent test function forking anywhere in this process can inherit
    // the still-open write descriptor on `helper`, which makes `execve` report
    // the file as busy (`ETXTBSY`) until that fork execs and drops it. The
    // window is short, so retry rather than force `--test-threads=1`.
    let mut attempt = 0;
    let mut child = loop {
        let spawned = Command::new(&helper)
            .args([
                "--exact",
                "daemon_guard_helper_process_stays_alive",
                "--nocapture",
            ])
            .env("AWMAN_GUARD_HELPER", "1")
            .spawn();
        match spawned {
            Ok(child) => break child,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("spawn awman-named helper process: {e}"),
        }
    };
    // Wait on the *identity*, not merely on liveness. `spawn` returns once the
    // child exists, and a forked-but-not-yet-exec'd child is already alive
    // while still reporting the command name it inherited from this thread —
    // so `is_process_alive` can pass a whole tick before the child looks like
    // an awman process to the guard under test.
    for _ in 0..500 {
        if awman::data::fs::daemon_process::pid_is_awman(child.id()) {
            // Exec has landed, so the child no longer needs the copied file.
            // Dropping the TempDir avoids leaking a test binary.
            drop(tmp);
            return child;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("awman-named helper process never presented an awman command name");
}

#[cfg(unix)]
fn assert_guard_rejects_live_other(kind: DaemonKind) {
    let tmp = tempfile::tempdir().unwrap();
    let api_root = tmp.path().join("api");
    let squad_root = tmp.path().join("squad");
    let env = paths_env(&api_root, &squad_root);
    let other_kind = match kind {
        DaemonKind::Api => DaemonKind::Squad,
        DaemonKind::Squad => DaemonKind::Api,
    };
    let other_root = match other_kind {
        DaemonKind::Api => &api_root,
        DaemonKind::Squad => &squad_root,
    };
    let mut child = awman_named_test_process();
    for _ in 0..40 {
        if awman::data::fs::daemon_process::is_process_alive(child.id()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(awman::data::fs::daemon_process::is_process_alive(
        child.id()
    ));
    daemon(other_root, other_kind)
        .force_write_pidfile(child.id())
        .unwrap();

    let error = DaemonGuard::for_daemon(kind, &env)
        .unwrap()
        .check()
        .expect_err("a live other daemon must block startup");
    let text = error.to_string();
    assert!(
        text.contains(&child.id().to_string()),
        "missing PID: {text}"
    );
    match other_kind {
        DaemonKind::Api => assert!(text.contains("awman api"), "missing API name: {text}"),
        DaemonKind::Squad => assert!(text.contains("squad"), "missing squad name: {text}"),
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn daemon_guard_check_rejects_live_squad_when_checking_api() {
    assert_guard_rejects_live_other(DaemonKind::Api);
}

#[cfg(unix)]
#[test]
fn daemon_guard_check_rejects_live_api_when_checking_squad() {
    assert_guard_rejects_live_other(DaemonKind::Squad);
}

#[test]
fn daemon_guard_helper_process_stays_alive() {
    if std::env::var_os("AWMAN_GUARD_HELPER").is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
