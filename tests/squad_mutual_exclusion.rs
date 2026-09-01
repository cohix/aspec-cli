//! WI 0101 — `DaemonGuard` cross-daemon mutual exclusion between `awman api`
//! and the squad daemon.
//!
//! `DaemonGuard`'s own in-file unit tests (`src/data/fs/daemon_guard.rs`,
//! owned by `refactor-daemon-primitives`) already cover one direction
//! (squad running → API refused) plus acquire/release and the absent-pidfile
//! case. This file covers the two additional WI-0101 test bullets: the
//! *other* direction, and the concurrent-start race.
//!
//! `pid_is_awman` (`src/data/fs/daemon_process.rs`) disambiguates a stale
//! pidfile from a genuinely running daemon by checking `/proc/<pid>/comm`
//! for the substring "awman". A test binary's own PID does **not** pass that
//! check here — this crate's test binaries are named things like
//! `squad_mutual_exclusion-<hash>`, whose truncated comm never contains
//! "awman" (unlike `cargo test --lib`'s `awman-<hash>` binary, which is why
//! the in-file tests can use `std::process::id()` directly). So every test
//! below stands up a short-lived real child process copied to a filename
//! that *does* contain "awman", giving `pid_is_awman` something genuine to
//! match — exactly the same signal a real `awman api`/`awman squad` process
//! would present.

use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Barrier};

use awman::data::config::env::{EnvSnapshot, AWMAN_API_ROOT, AWMAN_CONFIG_HOME, AWMAN_SQUAD_ROOT};
use awman::data::error::DataError;
use awman::data::fs::daemon_guard::{DaemonGuard, DaemonKind};
use awman::data::fs::{ApiPaths, SquadPaths};

/// `AWMAN_CONFIG_HOME` is scoped to a fixture too: `DaemonGuard`'s shared
/// startup-arbitration lock lives beside the shared database, and a test must
/// never touch the developer's real `~/.awman`.
fn env_for(api_root: &Path, squad_root: &Path) -> EnvSnapshot {
    EnvSnapshot::with_overrides([
        (AWMAN_API_ROOT, api_root.to_str().unwrap()),
        (AWMAN_SQUAD_ROOT, squad_root.to_str().unwrap()),
        (AWMAN_CONFIG_HOME, api_root.to_str().unwrap()),
    ])
}

/// A real, live child process whose executable filename contains "awman", so
/// `pid_is_awman` recognizes it the way it would a genuine daemon. Holds its
/// own `TempDir` so the binary stays resolvable for as long as the child
/// needs it.
struct FakeAwmanProcess {
    _dir: tempfile::TempDir,
    child: Child,
}

impl FakeAwmanProcess {
    fn spawn(label: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let exe_path = dir.path().join(format!("awman-fake-{label}"));
        std::fs::copy("/bin/sleep", &exe_path).expect("copy /bin/sleep for the fake daemon");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&exe_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&exe_path, perms).unwrap();
        }
        // Under `cargo test`'s default concurrent-test-function execution,
        // the kernel can transiently report a just-written, already-closed
        // executable as busy (`ETXTBSY`) for a moment before `execve`
        // succeeds. Retry briefly rather than require `--test-threads=1`.
        let mut attempt = 0;
        let child = loop {
            match Command::new(&exe_path).arg("30").spawn() {
                Ok(child) => break child,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("spawn fake awman-named process: {e}"),
            }
        };
        Self { _dir: dir, child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for FakeAwmanProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ─── Both directions ──────────────────────────────────────────────────────

#[test]
fn squad_running_blocks_api_start() {
    let api_root = tempfile::tempdir().unwrap();
    let squad_root = tempfile::tempdir().unwrap();
    let env = env_for(api_root.path(), squad_root.path());

    let fake_squad = FakeAwmanProcess::spawn("squad");
    let squad_guard = DaemonGuard::for_daemon(DaemonKind::Squad, &env).unwrap();
    squad_guard.acquire(fake_squad.pid()).unwrap();

    let api_guard = DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap();
    let err = api_guard
        .check()
        .expect_err("starting awman api while squad is running must be refused");
    assert!(
        matches!(&err, DataError::Other(msg) if msg.contains("squad")),
        "error must name the squad daemon: {err}"
    );

    squad_guard.release().unwrap();
}

#[test]
fn api_running_blocks_squad_start() {
    let api_root = tempfile::tempdir().unwrap();
    let squad_root = tempfile::tempdir().unwrap();
    let env = env_for(api_root.path(), squad_root.path());

    let fake_api = FakeAwmanProcess::spawn("api");
    let api_guard = DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap();
    api_guard.acquire(fake_api.pid()).unwrap();

    let squad_guard = DaemonGuard::for_daemon(DaemonKind::Squad, &env).unwrap();
    let err = squad_guard
        .check()
        .expect_err("starting the squad daemon while awman api is running must be refused");
    assert!(
        matches!(&err, DataError::Other(msg) if msg.contains("awman api")),
        "error must name awman api: {err}"
    );
    assert!(
        err.to_string().contains("awman api kill"),
        "error must hint at the stop command: {err}"
    );

    api_guard.release().unwrap();

    // With API stopped, squad may now start without contention. (Whether the
    // start additionally binds a port or opens the shared database is
    // `require_container_tier`/`serve_with`'s contract, covered by
    // `tests/squad_sandbox_refusal.rs` and `tests/squad_daemon_http.rs`.)
    assert!(squad_guard.check().is_ok());
}

// ─── Concurrent-start race ────────────────────────────────────────────────

/// Start both daemons concurrently from a clean state, with real OS threads
/// synchronised on a barrier so both begin `acquire()` at the same instant —
/// the scenario a single naive check-then-claim would get wrong (both could
/// pass a single check before either commits). The two-phase
/// check→claim→check protocol in `DaemonGuard::acquire` must never let both
/// win; whichever loses must leave no pidfile of its own and its error must
/// name the winner.
///
/// `DaemonGuard::acquire` serialises the whole check → claim → check sequence
/// behind a shared startup-arbitration lock, so a clean race resolves to
/// *exactly* one winner every time — never both (which would defeat the mutual
/// exclusion) and never neither (which the double check alone could produce,
/// leaving the user with two daemons that each refuse to start).
#[test]
fn concurrent_start_race_produces_exactly_one_winner_every_time() {
    for attempt in 0..20 {
        let api_root = tempfile::tempdir().unwrap();
        let squad_root = tempfile::tempdir().unwrap();
        let env = env_for(api_root.path(), squad_root.path());

        let fake_api = FakeAwmanProcess::spawn(&format!("api-{attempt}"));
        let fake_squad = FakeAwmanProcess::spawn(&format!("squad-{attempt}"));
        let api_pid = fake_api.pid();
        let squad_pid = fake_squad.pid();

        let api_guard = Arc::new(DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap());
        let squad_guard = Arc::new(DaemonGuard::for_daemon(DaemonKind::Squad, &env).unwrap());
        let barrier = Arc::new(Barrier::new(2));

        let api_thread = {
            let guard = api_guard.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                guard.acquire(api_pid)
            })
        };
        let squad_thread = {
            let guard = squad_guard.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                guard.acquire(squad_pid)
            })
        };

        let api_result = api_thread.join().unwrap();
        let squad_result = squad_thread.join().unwrap();

        let api_won = api_result.is_ok();
        let squad_won = squad_result.is_ok();

        assert!(
            api_won != squad_won,
            "attempt {attempt}: a clean race must produce exactly one winner \
             (api_won={api_won}, squad_won={squad_won})"
        );

        let api_pidfile = ApiPaths::from_env(&env).unwrap().pid_file();
        let squad_pidfile = SquadPaths::from_env(&env).unwrap().daemon().pid_file();

        if api_won {
            assert!(api_pidfile.exists(), "the winner must retain its pidfile");
            assert!(
                !squad_pidfile.exists(),
                "the loser must leave no pidfile behind"
            );
            let err = squad_result.unwrap_err();
            assert!(
                err.to_string().contains("awman api"),
                "the loser's error must name the winner: {err}"
            );
            api_guard.release().unwrap();
        } else if squad_won {
            assert!(squad_pidfile.exists(), "the winner must retain its pidfile");
            assert!(
                !api_pidfile.exists(),
                "the loser must leave no pidfile behind"
            );
            let err = api_result.unwrap_err();
            assert!(
                err.to_string().contains("squad"),
                "the loser's error must name the winner: {err}"
            );
            squad_guard.release().unwrap();
        }
    }
}

// ─── The real startup paths, not just `check()` ─────────────────────────────
//
// The two tests above prove the guard's decision in both directions. These
// prove the decision is actually *wired into* daemon startup: with the other
// daemon alive, `awman squad start` fails before it opens the shared database,
// claims a pidfile, or binds a port — the properties the work item's
// "fails without binding a port or opening the database" bullet asks for.

use awman::command::commands::squad::commands::{SquadCommandFrontend, SquadServeConfig};
use awman::command::commands::squad::daemon::{
    SquadDaemonCommand, SquadDaemonSubcommand, SquadStartFlags,
};
use awman::command::commands::Command as AwmanCommand;
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use tokio::sync::Mutex as AsyncMutex;

/// Serialises the tests that mutate the real process environment.
static PROCESS_ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// A frontend that fails the test loudly if the daemon ever reaches the point
/// of serving. Refusal must happen strictly before that.
struct NeverServesFrontend;

impl UserMessageSink for NeverServesFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl SquadCommandFrontend for NeverServesFrontend {
    async fn serve_squad_daemon(&mut self, _config: SquadServeConfig) -> Result<(), CommandError> {
        panic!("the squad daemon must never start while awman api is running");
    }
}

fn engines_at(root: &Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = awman::data::fs::AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let runtime = Arc::new(ContainerRuntime::docker());
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

/// Scope the process environment to an isolated fixture for the duration of a
/// call, then restore it. `SquadDaemonCommand` reads `Env::from_process()`.
struct ScopedEnv(Vec<(&'static str, Option<String>)>);

impl ScopedEnv {
    fn set(vars: &[(&'static str, &Path)]) -> Self {
        let saved = vars
            .iter()
            .map(|(key, value)| {
                let previous = std::env::var(key).ok();
                std::env::set_var(key, value);
                (*key, previous)
            })
            .collect();
        Self(saved)
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, previous) in &self.0 {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[tokio::test]
async fn a_live_api_daemon_stops_squad_startup_before_any_store_pidfile_or_port() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let api_root = home.path().join("api");
    let squad_root = home.path().join("squad");
    std::fs::create_dir_all(&api_root).unwrap();
    std::fs::create_dir_all(&squad_root).unwrap();

    // A live, awman-named process holding the API pidfile — what a running
    // `awman api` looks like to the guard.
    let fake_api = FakeAwmanProcess::spawn("api-live");
    let env = env_for(&api_root, &squad_root);
    let api_guard = DaemonGuard::for_daemon(DaemonKind::Api, &env).unwrap();
    api_guard.acquire(fake_api.pid()).unwrap();

    let _scoped = ScopedEnv::set(&[
        (AWMAN_API_ROOT, &api_root),
        (AWMAN_SQUAD_ROOT, &squad_root),
        (AWMAN_CONFIG_HOME, home.path()),
    ]);

    let command = SquadDaemonCommand::new(
        SquadDaemonSubcommand::Start(SquadStartFlags {
            port: 0,
            background: false,
            refresh_key: false,
            dangerously_skip_auth: true,
        }),
        engines_at(home.path()),
    );
    let error = AwmanCommand::run_with_frontend(command, Box::new(NeverServesFrontend))
        .await
        .expect_err("squad must refuse to start while awman api is running");

    let text = error.to_string();
    assert!(
        text.contains("awman api"),
        "error must name awman api: {text}"
    );
    assert!(
        text.contains(&fake_api.pid().to_string()),
        "error must name the running PID: {text}"
    );

    // Nothing downstream of the guard ran.
    let squad_daemon = SquadPaths::from_root(&squad_root).daemon();
    assert!(
        !squad_daemon.pid_file().exists(),
        "no squad pidfile may be claimed"
    );
    assert!(
        !squad_daemon.server_meta_file().exists(),
        "no port may be bound or published"
    );
    assert!(
        !squad_daemon.key_hash_file().exists(),
        "no bearer key may be minted before the guard passes"
    );
    assert!(
        !home.path().join("data").join("awman.db").exists(),
        "the shared database must never be opened"
    );

    api_guard.release().unwrap();
}
