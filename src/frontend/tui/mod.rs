//! TUI frontend — Ratatui-based interactive terminal UI.
//!
//! Captures the terminal (raw mode, alternate screen, mouse), constructs
//! `App` state, enters the event loop, and restores the terminal on exit.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_tui_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

use tokio::sync::RwLock;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::data::session_manager::SessionManager;
use crate::frontend::cli::RuntimeContext;

pub mod amie_attach;
pub mod amie_poll;
pub mod app;
pub mod command_box;
pub mod command_frontend;
pub mod container_view;
mod dialog_router;
pub mod dialogs;
mod event_loop;
pub mod git_sidebar;
pub mod hints;
mod key_handler;
pub mod keymap;
mod mouse;
mod mouse_handler;
pub mod per_command;
pub mod pty;
mod region_scroll;
pub mod render;
pub mod tabs;
pub mod text_edit;
pub mod user_message;
pub mod workflow_view;

pub use per_command::remote::{
    AmieConditionWorkflowSource, RemoteApiWorkflowSource, RemoteWorkflowPoller, WorkflowStateSource,
};

#[cfg(test)]
mod tests;

use app::App;
use dialogs::Dialog;
use tabs::Tab;

/// What the TUI opens with. `Normal` carries the session `main.rs` already
/// resolved from the working directory; `Amie` opens the singleton amie tab and
/// no directory-bound tab at all.
///
/// The shape is pinned by the WI 0102 contract (§1). `Normal(Session)` is
/// intentionally unboxed — the enum is constructed once and consumed
/// immediately in [`run`], so the size difference between variants never
/// materialises as a real cost.
#[allow(clippy::large_enum_variant)]
pub enum InitialTab {
    Normal(crate::data::session::Session),
    Amie,
}

/// Entry point invoked by `main.rs` for bare (no-subcommand) launches and for
/// bare `awman amie` in a TTY.
///
/// `fatal_runtime_error` carries the invalid-runtime config message when the
/// global config names a runtime awman doesn't recognize. In that case the
/// TUI presents only a fatal modal (Enter quits) — no startup command runs.
///
/// `initial_tab` selects the opening tab: `Normal(session)` is today's
/// behaviour, including the `ready` / `status --watch` startup auto-spawn;
/// `Amie` opens the singleton amie tab (§2.2) with no auto-spawn.
pub async fn run(
    _matches: clap::ArgMatches,
    ctx: RuntimeContext,
    fatal_runtime_error: Option<String>,
    initial_tab: InitialTab,
) -> ExitCode {
    let catalogue = CommandCatalogue::get();
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
    let runtime_handle = tokio::runtime::Handle::current();

    // Build the App and decide whether the normal startup auto-spawn runs — it
    // does only for a directory-bound tab, never for the amie tab.
    let (mut app, run_startup_spawn) = match initial_tab {
        InitialTab::Normal(session) => {
            let tab = Tab::new(session);
            let app = App::new(catalogue, ctx.engines, session_manager, tab, runtime_handle);
            (app, true)
        }
        InitialTab::Amie => match App::build_amie_tab(&ctx.engines, &runtime_handle) {
            Ok(build) => {
                let key_setup = build.key_setup;
                let mut app = App::new(
                    catalogue,
                    ctx.engines,
                    session_manager,
                    build.tab,
                    runtime_handle,
                );
                app.amie_gateway = Some(build.gateway);
                // First run: the bearer key was minted a moment ago and lives
                // only in memory. Show it before the event loop starts.
                if let Some(body) = key_setup {
                    app.active_dialog = Some(Dialog::Notice {
                        title: "amie authentication".to_string(),
                        body,
                    });
                }
                (app, false)
            }
            Err(message) => {
                // `main.rs` calls `ensure_running` before routing here, so this
                // should not happen; degrade to a normal tab on the cwd session
                // and surface the specific error rather than failing to open.
                let session = ctx.session.read().await.clone();
                let tab = Tab::new(session);
                let mut app =
                    App::new(catalogue, ctx.engines, session_manager, tab, runtime_handle);
                app.status_bar.text = message;
                (app, false)
            }
        },
    };

    if let Some(message) = fatal_runtime_error {
        app.active_dialog = Some(Dialog::FatalError {
            title: "Invalid Runtime Configuration".to_string(),
            body: format!(
                "{message}\n\nUpdate the 'runtime' value in $HOME/.awman/config.json \
                 and restart awman."
            ),
        });
        return match event_loop::run_event_loop(&mut app) {
            Ok(()) => ExitCode::from(2),
            Err(e) => {
                eprintln!("awman: TUI error: {e}");
                ExitCode::from(1)
            }
        };
    }

    // Auto-spawn startup command: `ready` for git repos, `status --watch`
    // for non-git directories. Skipped entirely for the amie tab.
    if run_startup_spawn {
        let is_git = app.active_tab().session.git_root().join(".git").exists();
        if is_git {
            app.spawn_command(
                "ready",
                ParsedCommandBoxInput {
                    path: vec!["ready".into()],
                    flags: Default::default(),
                    arguments: Default::default(),
                },
            );
        } else {
            let mut flags = std::collections::BTreeMap::new();
            flags.insert(
                "watch".to_string(),
                crate::command::dispatch::parsed_input::FlagValue::Bool(true),
            );
            app.spawn_command(
                "status --watch",
                ParsedCommandBoxInput {
                    path: vec!["status".into()],
                    flags,
                    arguments: Default::default(),
                },
            );
        }
    }

    match event_loop::run_event_loop(&mut app) {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("awman: TUI error: {e}");
            ExitCode::from(1)
        }
    }
}
