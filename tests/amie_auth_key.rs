//! amie bearer-key provisioning and disclosure.
//!
//! The daemon has always minted an `amie_key.hash` on its first start, but the
//! plaintext was printed as a bare line with no indication that the CLI and TUI
//! read it back out of `AWMAN_AMIE_KEY`. These tests pin the two halves that
//! make the key usable: the export snippet `awman amie start` emits, and the
//! `--dangerously-skip-auth` path that opts out of a key entirely.

use std::path::Path;
use std::sync::{Arc, Mutex};

use awman::command::commands::amie::commands::{AmieCommandFrontend, AmieServeConfig};
use awman::command::commands::amie::daemon::{
    AmieDaemonCommand, AmieDaemonSubcommand, AmieStartFlags,
};
use awman::command::commands::amie::key_setup::{render_key_setup, ShellFlavor};
use awman::command::commands::Command as AwmanCommand;
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::config::env::{
    Env, EnvSnapshot, AWMAN_AMIE_KEY, AWMAN_AMIE_ROOT, AWMAN_API_ROOT, AWMAN_CONFIG_HOME, SHELL,
};
use awman::data::fs::daemon_process::ServerMeta;
use awman::data::fs::{AmiePaths, ApiPaths};
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

/// Captures every user-facing message and returns immediately from the serve
/// call, so `run_start` runs end to end without binding a port.
#[derive(Clone, Default)]
struct RecordingFrontend {
    messages: Arc<Mutex<Vec<String>>>,
}

impl RecordingFrontend {
    fn text(&self) -> String {
        self.messages.lock().unwrap().join("\n")
    }
}

impl UserMessageSink for RecordingFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.lock().unwrap().push(msg.text);
    }
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AmieCommandFrontend for RecordingFrontend {
    async fn serve_amie_daemon(&mut self, _config: AmieServeConfig) -> Result<(), CommandError> {
        // Stand in for a served-then-shut-down daemon: `run_start` continues
        // through its cleanup path exactly as it would after Ctrl-C.
        Ok(())
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

/// Scope the process environment to an isolated fixture, then restore it.
/// `AmieDaemonCommand` reads `Env::from_process()`.
struct ScopedEnv(Vec<(&'static str, Option<String>)>);

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
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

async fn run_start(home: &Path, flags: AmieStartFlags) -> (RecordingFrontend, AmiePaths) {
    let amie_root = home.join("amie");
    std::fs::create_dir_all(&amie_root).unwrap();
    let _scoped = ScopedEnv::set(&[
        (AWMAN_API_ROOT, home.join("api").to_str().unwrap()),
        (AWMAN_AMIE_ROOT, amie_root.to_str().unwrap()),
        (AWMAN_CONFIG_HOME, home.to_str().unwrap()),
        (SHELL, "/bin/zsh"),
    ]);
    let frontend = RecordingFrontend::default();
    let command = AmieDaemonCommand::new(AmieDaemonSubcommand::Start(flags), engines_at(home));
    AwmanCommand::run_with_frontend(command, Box::new(frontend.clone()))
        .await
        .expect("amie start must succeed on a clean fixture");
    (frontend, AmiePaths::from_root(&amie_root))
}

/// The first start mints a key AND tells the user how to spend it. Printing the
/// key alone — the previous behaviour — left every later process unable to
/// authenticate, because nothing documented `AWMAN_AMIE_KEY`.
#[tokio::test]
async fn first_start_emits_a_shell_export_snippet_for_the_minted_key() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let (frontend, paths) = run_start(
        home.path(),
        AmieStartFlags {
            port: 0,
            background: false,
            refresh_key: false,
            dangerously_skip_auth: false,
        },
    )
    .await;

    let text = frontend.text();
    assert!(
        text.contains("export AWMAN_AMIE_KEY="),
        "start must emit a copy-pasteable export line; got:\n{text}"
    );
    assert!(
        text.contains("~/.zshrc"),
        "the snippet must name the shell startup file; got:\n{text}"
    );
    assert!(
        paths.daemon().key_hash_file().exists(),
        "the key hash must be persisted so the daemon can verify it"
    );

    // The snippet carries the real key, not a placeholder: the hash on disk is
    // never the plaintext, so a wrong key here would be undetectable later.
    let exported = text
        .split("export AWMAN_AMIE_KEY=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("export line must carry a value");
    assert!(
        exported.len() >= 32,
        "exported value must be the key itself; got {exported:?}"
    );
    assert!(
        !std::fs::read_to_string(paths.daemon().key_hash_file())
            .unwrap()
            .contains(exported),
        "the plaintext key must never be what lands in the hash file"
    );
}

/// `--dangerously-skip-auth` is the documented alternative to holding a key.
/// It must mint nothing — a hash whose plaintext nobody holds would lock the
/// user out of the next auth-enabled start.
#[tokio::test]
async fn skip_auth_mints_no_key_and_warns_that_auth_is_disabled() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let (frontend, paths) = run_start(
        home.path(),
        AmieStartFlags {
            port: 0,
            background: false,
            refresh_key: false,
            dangerously_skip_auth: true,
        },
    )
    .await;

    assert!(
        !paths.daemon().key_hash_file().exists(),
        "--dangerously-skip-auth must write no key hash"
    );
    let text = frontend.text();
    assert!(
        !text.contains("AWMAN_AMIE_KEY"),
        "no key may be disclosed when none was minted; got:\n{text}"
    );
    assert!(
        text.contains("DISABLED") && text.contains("--dangerously-skip-auth"),
        "the user must be told authentication is off; got:\n{text}"
    );
    assert!(
        text.contains("127.0.0.1"),
        "the warning must say why this is tolerable — loopback only; got:\n{text}"
    );
}

/// `--refresh-key` replaces the key and re-emits the same snippet, so a user
/// who lost the original has a documented way back in.
#[tokio::test]
async fn refresh_key_re_emits_the_export_snippet() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let (frontend, _paths) = run_start(
        home.path(),
        AmieStartFlags {
            port: 0,
            background: false,
            refresh_key: true,
            dangerously_skip_auth: false,
        },
    )
    .await;
    let text = frontend.text();
    assert!(
        text.contains("export AWMAN_AMIE_KEY="),
        "--refresh-key must emit the setup snippet too; got:\n{text}"
    );
}

/// The env var the snippet exports is the one the client actually reads.
/// These two drifting apart is precisely the gap this work closes.
#[test]
fn the_exported_variable_is_the_one_the_client_reads() {
    let snippet = render_key_setup("secret-key", ShellFlavor::Zsh);
    let env = EnvSnapshot::with_overrides([(AWMAN_AMIE_KEY, "secret-key")]);
    assert_eq!(env.amie_key(), Some("secret-key"));
    assert!(
        snippet.contains(&format!("export {AWMAN_AMIE_KEY}=secret-key")),
        "snippet must export the variable `EnvSnapshot::amie_key` reads; got:\n{snippet}"
    );
}

/// An empty `AWMAN_AMIE_KEY` is not a key. Treating `""` as one would send an
/// empty bearer token and produce a confusing 401 rather than the usual
/// "no key supplied" path.
#[test]
fn an_empty_amie_key_is_treated_as_unset() {
    let env = EnvSnapshot::with_overrides([(AWMAN_AMIE_KEY, "")]);
    assert_eq!(env.amie_key(), None);
}

/// `Env::from_process` must actually capture the variable — the snapshot only
/// reads a fixed list of names, so an omission here silently disables the key.
#[tokio::test]
async fn amie_key_is_captured_from_the_real_process_environment() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let _scoped = ScopedEnv::set(&[(AWMAN_AMIE_KEY, "from-the-environment")]);
    assert_eq!(Env::from_process().amie_key(), Some("from-the-environment"));
}

/// Clients read `auth_disabled` off the sidecar to decide whether to provision
/// a key at all. A sidecar written before the field existed must read as
/// "auth required" — the safe direction.
#[test]
fn a_legacy_server_sidecar_reads_as_auth_required() {
    let legacy = r#"{"port":9876,"bind_ip":"127.0.0.1","scheme":"http"}"#;
    let meta: ServerMeta = serde_json::from_str(legacy).expect("legacy sidecar must still parse");
    assert!(
        !meta.auth_disabled,
        "an absent flag must not be read as disabled auth"
    );
}

#[test]
fn a_skip_auth_daemon_publishes_that_fact_in_its_sidecar() {
    let meta = ServerMeta {
        port: 9876,
        bind_ip: "127.0.0.1".into(),
        scheme: "http".into(),
        auth_disabled: true,
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert_eq!(serde_json::from_str::<ServerMeta>(&json).unwrap(), meta);
}
