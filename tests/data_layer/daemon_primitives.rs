//! Part 0 daemon primitive regression tests.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use awman::data::config::env::{EnvSnapshot, AWMAN_AMIE_ROOT, AWMAN_API_ROOT};
use awman::data::fs::daemon_process::{
    DaemonProcess, ServerMeta, AMIE_PLIST_LABEL, AMIE_UNIT_NAME, API_PLIST_LABEL, API_UNIT_NAME,
};
use awman::data::fs::{AmiePaths, DaemonGuard, DaemonKind, DaemonPaths};

fn paths_env(api_root: &Path, amie_root: &Path) -> EnvSnapshot {
    EnvSnapshot::with_overrides([
        (AWMAN_API_ROOT, api_root.to_string_lossy().to_string()),
        (AWMAN_AMIE_ROOT, amie_root.to_string_lossy().to_string()),
    ])
}

fn daemon(root: &Path, kind: DaemonKind) -> DaemonProcess {
    match kind {
        DaemonKind::Api => DaemonProcess::new(
            DaemonPaths::new(root, "api_key"),
            API_UNIT_NAME,
            API_PLIST_LABEL,
        ),
        DaemonKind::Amie => DaemonProcess::new(
            DaemonPaths::new(root, "amie_key"),
            AMIE_UNIT_NAME,
            AMIE_PLIST_LABEL,
        ),
    }
}

#[test]
fn daemon_paths_preserve_api_filenames_and_isolate_amie_key() {
    let api = DaemonPaths::new("/tmp/api", "api_key");
    assert_eq!(api.pid_file(), PathBuf::from("/tmp/api/awman.pid"));
    assert_eq!(api.log_file(), PathBuf::from("/tmp/api/awman.log"));
    assert_eq!(
        api.server_meta_file(),
        PathBuf::from("/tmp/api/server.json")
    );
    assert_eq!(api.key_hash_file(), PathBuf::from("/tmp/api/api_key.hash"));

    let amie = AmiePaths::from_root("/tmp/amie").daemon();
    assert_eq!(amie.pid_file(), PathBuf::from("/tmp/amie/awman.pid"));
    assert_eq!(amie.log_file(), PathBuf::from("/tmp/amie/awman.log"));
    assert_eq!(
        amie.server_meta_file(),
        PathBuf::from("/tmp/amie/server.json")
    );
    assert_eq!(
        amie.key_hash_file(),
        PathBuf::from("/tmp/amie/amie_key.hash")
    );
    assert_ne!(api.key_hash_file(), amie.key_hash_file());
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
    };
    assert_eq!(process.read_meta().unwrap(), None);
    process.write_meta(&meta).unwrap();
    assert_eq!(process.read_meta().unwrap(), Some(meta));
    process.clear_meta().unwrap();
    assert_eq!(process.read_meta().unwrap(), None);
}

#[test]
fn spawn_detached_identity_is_distinct_for_api_and_amie() {
    let api_root = tempfile::tempdir().unwrap();
    let amie_root = tempfile::tempdir().unwrap();
    let api = daemon(api_root.path(), DaemonKind::Api);
    let amie = daemon(amie_root.path(), DaemonKind::Amie);

    assert_ne!(API_UNIT_NAME, AMIE_UNIT_NAME);
    assert_ne!(API_PLIST_LABEL, AMIE_PLIST_LABEL);
    assert_ne!(api.paths().log_file(), amie.paths().log_file());

    #[cfg(target_os = "linux")]
    {
        use std::sync::Mutex;
        static PATH_LOCK: Mutex<()> = Mutex::new(());
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
        assert_eq!(amie.spawn_detached(Path::new("/bin/true"), &[]).unwrap(), 0);
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
            args.contains("--unit=awman-amie"),
            "amie unit was not threaded: {args}"
        );
    }

    // The launchd path is the macOS equivalent: the plist filename *and* its
    // `Label` key must both be the daemon's own, or the second daemon would
    // overwrite the first's launch agent. `try_launchd` resolves the plist
    // under `dirs::home_dir()` and shells out to `launchctl load`, so a
    // scoped `HOME` plus a stub `launchctl` on `PATH` exercises it for real.
    #[cfg(target_os = "macos")]
    {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap();
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
        amie.spawn_detached(Path::new("/usr/bin/true"), &[])
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
        let amie_plist = std::fs::read_to_string(agents.join(format!("{AMIE_PLIST_LABEL}.plist")))
            .expect("the amie daemon must write its own plist, not overwrite the API's");
        assert!(api_plist.contains(&format!("<string>{API_PLIST_LABEL}</string>")));
        assert!(amie_plist.contains(&format!("<string>{AMIE_PLIST_LABEL}</string>")));
        // Each daemon's stdout goes to its own log, never the other's.
        assert!(api_plist.contains(&api.paths().log_file().display().to_string()));
        assert!(amie_plist.contains(&amie.paths().log_file().display().to_string()));
        assert!(!api_plist.contains(&amie.paths().log_file().display().to_string()));
    }
}

#[test]
fn daemon_guard_check_accepts_absent_and_stale_pidfiles_in_both_directions() {
    let tmp = tempfile::tempdir().unwrap();
    let api_root = tmp.path().join("api");
    let amie_root = tmp.path().join("amie");
    let env = paths_env(&api_root, &amie_root);

    for kind in [DaemonKind::Api, DaemonKind::Amie] {
        let guard = DaemonGuard::for_daemon(kind, &env).unwrap();
        assert!(
            guard.check().is_ok(),
            "absent pidfile must be allowed: {kind:?}"
        );

        let other_kind = match kind {
            DaemonKind::Api => DaemonKind::Amie,
            DaemonKind::Amie => DaemonKind::Api,
        };
        let other_root = match other_kind {
            DaemonKind::Api => &api_root,
            DaemonKind::Amie => &amie_root,
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
    let mut child = Command::new(&helper)
        .args([
            "--exact",
            "daemon_guard_helper_process_stays_alive",
            "--nocapture",
        ])
        .env("AWMAN_GUARD_HELPER", "1")
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if awman::data::fs::daemon_process::is_process_alive(child.id()) {
            // Once exec has completed, the child no longer needs the copied
            // file. Dropping the TempDir avoids leaking a test binary.
            drop(tmp);
            return child;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("awman-named helper process did not stay alive");
}

#[cfg(unix)]
fn assert_guard_rejects_live_other(kind: DaemonKind) {
    let tmp = tempfile::tempdir().unwrap();
    let api_root = tmp.path().join("api");
    let amie_root = tmp.path().join("amie");
    let env = paths_env(&api_root, &amie_root);
    let other_kind = match kind {
        DaemonKind::Api => DaemonKind::Amie,
        DaemonKind::Amie => DaemonKind::Api,
    };
    let other_root = match other_kind {
        DaemonKind::Api => &api_root,
        DaemonKind::Amie => &amie_root,
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
        DaemonKind::Amie => assert!(text.contains("amie"), "missing amie name: {text}"),
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn daemon_guard_check_rejects_live_amie_when_checking_api() {
    assert_guard_rejects_live_other(DaemonKind::Api);
}

#[cfg(unix)]
#[test]
fn daemon_guard_check_rejects_live_api_when_checking_amie() {
    assert_guard_rejects_live_other(DaemonKind::Amie);
}

#[test]
fn daemon_guard_helper_process_stays_alive() {
    if std::env::var_os("AWMAN_GUARD_HELPER").is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
