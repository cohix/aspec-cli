#![forbid(unsafe_code)]
//! Layer 4 — the `awman` binary entrypoint.
//!
//! Per `aspec/architecture/2026-grand-architecture.md`, `main.rs`
//! contains no business logic: it builds clap from `CommandCatalogue`,
//! parses argv, constructs the engines + session, and dispatches to either
//! the CLI frontend (when a subcommand is present) or the TUI frontend
//! (bare invocation).

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};

use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::Engines;
use awman::data::config::env::Env;
use awman::data::config::global::GlobalConfig;
use awman::data::error::DataError;
use awman::data::migration;
use awman::data::session::{GitRootResolver, Session, SessionOpenOptions};
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::cli::{self, RuntimeContext};
use awman::frontend::tui;

#[tokio::main]
async fn main() -> Result<ExitCode> {
    // Retired flags (e.g. `--mount-ssh`) are intercepted before clap renders
    // its generic "unexpected argument" message, so the user sees a migration
    // hint instead. The removed-flag list and matching live in the catalogue;
    // adding a future removal needs no `main.rs` change.
    if let Some(hint) = CommandCatalogue::get().removed_flag_hint(std::env::args()) {
        eprintln!("error: {hint}");
        return Ok(ExitCode::from(2));
    }

    let clap_cmd = CommandCatalogue::get().build_clap_command();
    let matches = clap_cmd.get_matches();

    init_tracing();

    // One-time migration from legacy amux paths and env vars.
    if let Some(msg) = migration::migrate_global_dir() {
        eprintln!("{msg}");
    }
    for warning in migration::check_deprecated_env_vars() {
        eprintln!("{warning}");
    }

    let global_config = GlobalConfig::load().unwrap_or_default();
    // Runtime detection + the CLI/TUI fallback policy live on the Layer 2
    // `Engines` type. `fatal_runtime_error` is `Some` only when the configured
    // `runtime:` is invalid and the TUI is about to start: the TUI boots just
    // far enough to present a fatal modal with this message and quits on Enter.
    let path = cli::command_path_from_matches(&matches);
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let (detected, fatal_runtime_error) =
        match Engines::detect(CommandCatalogue::get(), &global_config, &path_refs) {
            Ok(pair) => pair,
            Err(e @ awman::engine::error::EngineError::UnknownRuntime { .. }) => {
                eprintln!("awman: {e}");
                return Ok(ExitCode::from(2));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context("failed to detect agent runtime"));
            }
        };
    let runtime = detected.engine();
    let container_runtime = detected.container_runtime();
    let sandbox_runtime = detected.sandbox_runtime();
    let git_engine = Arc::new(GitEngine::new());

    let working_dir = std::env::current_dir().context("could not read current directory")?;

    // Resolve git root first so we can migrate the repo-local `.amux/` → `.awman/`
    // BEFORE `Session::open` reads `RepoConfig` from disk. If we deferred this,
    // a user's first post-rename run would silently fall back to default repo
    // config because the load would miss the legacy `.amux/config.json`.
    let git_root = match git_engine.resolve(&working_dir) {
        Ok(root) => root,
        Err(DataError::GitRootNotFound { .. }) => working_dir.clone(),
        Err(other) => return Err(anyhow::Error::new(other).context("failed to resolve git root")),
    };
    if let Some(msg) = migration::migrate_repo_dir(&git_root) {
        eprintln!("{msg}");
    }

    let session = Session::open_at_git_root(
        working_dir.clone(),
        git_root,
        SessionOpenOptions {
            env: Some(Env::from_process()),
            ..Default::default()
        },
    )
    .context("failed to open session")?;

    let overlay_engine =
        Arc::new(OverlayEngine::new(&session).context("failed to construct overlay engine")?);
    let auth_engine =
        Arc::new(AuthEngine::new(&session).context("failed to construct auth engine")?);
    // AgentEngine is container-paradigm-specific. Under a sandbox-class
    // runtime it receives an inert Docker handle that is never exercised:
    // every container-paradigm flow guards via
    // `Engines::require_container_runtime()` first (sandbox flows land in
    // WI 0090).
    let agent_engine = Arc::new(AgentEngine::new(
        overlay_engine.clone(),
        container_runtime
            .clone()
            .unwrap_or_else(|| Arc::new(ContainerRuntime::docker())),
    ));
    let workflow_state_store = Arc::new(awman::data::EngineWorkflowStateStore::at_git_root(
        session.git_root().to_path_buf(),
    ));

    let engines = Engines {
        runtime,
        container_runtime,
        sandbox_runtime,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };

    let tab_session = session.clone();
    let ctx = RuntimeContext::new(session, engines);

    let initial_tab = if matches.subcommand_name().is_none() {
        Some(tui::InitialTab::Normal(tab_session))
    } else if cli::is_bare_amie_tui_invocation(&matches) {
        Some(tui::InitialTab::Amie)
    } else {
        None
    };

    match initial_tab {
        Some(tab) => Ok(tui::run(matches, ctx, fatal_runtime_error, tab).await),
        None => Ok(cli::run(matches, ctx).await),
    }
}

/// Initialize the global tracing subscriber once at process start.
///
/// Without this, every `tracing::info!`/`warn!`/`error!` call in the
/// codebase (notably the API-server startup messages in `frontend::api`)
/// is silently dropped, which made `awman api start` look like it was
/// hanging until Ctrl-C. We write to stderr (so stdout stays clean for
/// `--json` callers), default to `info`-level for awman code, and honor
/// `RUST_LOG` for overrides. ANSI colors are auto-enabled by the `fmt`
/// layer when stderr is a TTY.
///
/// When the TUI is active, stderr writes would paint raw text over the
/// alternate-screen rendering. The writer gates on `is_tui_active()` and
/// redirects to `io::sink()` while the TUI owns the terminal.
fn init_tracing() {
    use tracing_subscriber::fmt::{format::Writer, time::FormatTime, MakeWriter};
    use tracing_subscriber::{fmt, EnvFilter};

    struct ShortLocalTime;
    impl FormatTime for ShortLocalTime {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            write!(w, "{}", chrono::Local::now().format("%H:%M:%S%.3f"))
        }
    }

    enum MaybeStderr {
        Stderr(std::io::Stderr),
        Sink(std::io::Sink),
    }
    impl std::io::Write for MaybeStderr {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Stderr(w) => w.write(buf),
                Self::Sink(w) => w.write(buf),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                Self::Stderr(w) => w.flush(),
                Self::Sink(w) => w.flush(),
            }
        }
    }

    struct TuiAwareWriter;
    impl<'a> MakeWriter<'a> for TuiAwareWriter {
        type Writer = MaybeStderr;
        fn make_writer(&'a self) -> Self::Writer {
            if tui::is_tui_active() {
                MaybeStderr::Sink(std::io::sink())
            } else {
                MaybeStderr::Stderr(std::io::stderr())
            }
        }
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(TuiAwareWriter)
        .with_target(false)
        .with_timer(ShortLocalTime)
        .compact()
        .try_init();
}

// ─── Layer 4 routing tests ────────────────────────────────────────────────────
//
// `main` is too integrated to call in unit tests (it requires live engines and
// a real session). Instead we test the **routing logic** directly: the condition
// `matches.subcommand_name().is_some()` is what drives the cli-vs-tui branch.
// These tests exercise that predicate with synthetic `ArgMatches`.

#[cfg(test)]
mod tests {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    use awman::frontend::cli::command_path_from_matches;

    /// A subcommand in argv → `subcommand_name().is_some()` → CLI branch.
    #[test]
    fn subcommand_present_signals_cli_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["awman", "status"],
            vec!["awman", "ready"],
            vec!["awman", "chat"],
            vec!["awman", "init"],
            vec!["awman", "exec", "workflow", "wf.toml"],
            vec!["awman", "api", "start"],
            vec!["awman", "remote", "session", "start"],
        ] {
            let m = cmd
                .clone()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|e| panic!("failed to parse {argv:?}: {e}"));
            assert!(
                m.subcommand_name().is_some(),
                "{argv:?} must have a subcommand — routes to CLI"
            );
            // command_path_from_matches must also return a non-empty path.
            let path = command_path_from_matches(&m);
            assert!(!path.is_empty(), "{argv:?} must produce a non-empty path");
        }
    }

    /// Bare `awman` → `subcommand_name().is_none()` → TUI branch.
    #[test]
    fn bare_invocation_signals_tui_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman"]).unwrap();
        assert!(
            m.subcommand_name().is_none(),
            "bare `awman` must have no subcommand — routes to TUI"
        );
        let path = command_path_from_matches(&m);
        assert!(
            path.is_empty(),
            "bare invocation must produce an empty path"
        );
    }

    /// Aliases also route through the CLI branch correctly.
    #[test]
    fn exec_workflow_alias_wf_routes_to_cli() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "wf", "wf.toml"])
            .unwrap();
        assert!(m.subcommand_name().is_some());
        let path = command_path_from_matches(&m);
        // Clap resolves the alias to canonical `workflow`.
        assert_eq!(path, vec!["exec", "workflow"]);
    }
}
