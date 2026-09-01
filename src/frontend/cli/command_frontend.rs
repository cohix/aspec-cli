//! `CliFrontend` — the single Layer 3 struct that implements every
//! per-command frontend trait for the CLI execution mode.
//!
//! The CLI frontend is intentionally small: it pulls flag values from a
//! parsed `clap::ArgMatches`, queues `UserMessage`s while a container PTY
//! owns the terminal, and prompts on stdin for the small number of
//! interactive decisions that the catalogue requires.
//!
//! The full per-command rendering (dialog widgets, progress UIs, etc.) is
//! handled by the TUI; the CLI uses safe non-interactive defaults for any
//! interactive Q&A when stdin is not a TTY.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use crate::command::commands::status::StatusCommandFrontend;
use crate::command::commands::{
    auth::AuthCommandFrontend,
    config::ConfigCommandFrontend,
    download::DownloadCommandFrontend,
    new::NewCommandFrontend,
    remote::RemoteCommandFrontend,
    specs::{SpecsCommandFrontend, WorkItemKind},
};
use crate::command::dispatch::catalogue::{ArgumentKind, CommandCatalogue, FlagKind};
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentFrontend;

use super::user_message::CliUserMessageQueue;

/// Single CLI frontend struct. Implements every per-command frontend trait
/// in `src/frontend/cli/per_command/`.
pub struct CliFrontend {
    matches: ArgMatches,
    /// Cached canonical command path (resolved via `command_path_from_matches`).
    pub(crate) command_path: Vec<String>,
    pub(crate) messages: CliUserMessageQueue,
    /// Effective non-interactive mode: true when explicitly requested via
    /// `--non-interactive` OR when stdin is not a TTY.
    pub(crate) non_interactive: bool,
    /// Receiver end of the background stdin-reader thread spawned for yolo
    /// countdown input. `None` until the first `yolo_countdown_tick` call on a
    /// TTY; consumed lines are mapped to `YoloTickOutcome` by the
    /// `WorkflowFrontend` impl. Wrapped in `Mutex` to satisfy the `Sync` bound
    /// on frontend traits (access is single-threaded in practice).
    pub(crate) yolo_stdin_rx: Option<std::sync::Mutex<std::sync::mpsc::Receiver<String>>>,
    /// Throttle: last time a yolo countdown message was emitted to stderr.
    pub(crate) last_sink_message_time: Option<std::time::Instant>,
    /// RAII guard that restores cooked mode when dropped. Present while a
    /// full-screen interactive container owns the terminal.
    pub(crate) raw_mode_guard: Option<RawModeGuard>,
    /// Shutdown flag for the interactive `/dev/stdin` reader thread. Set
    /// by `report_step_status` on a terminal status so the thread exits
    /// without waiting for a final keystroke; the thread polls this on
    /// every iteration of its `poll(2)` loop.
    pub(crate) stdin_reader_shutdown: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// JoinHandle of the interactive `/dev/stdin` reader thread. Joining it
    /// after setting `stdin_reader_shutdown` guarantees the host stdin lock
    /// is released before any subsequent cooked-mode `read_line` call (e.g.
    /// the workflow control board, worktree finalize prompts).
    pub(crate) stdin_reader_handle: Option<std::thread::JoinHandle<()>>,
    /// Clone of the channel sender the stdin reader thread uses to forward
    /// bytes into the currently-active container's PTY. Retained so the
    /// workflow control board can temporarily unbind stdio for an
    /// interactive prompt and rebind by spawning a fresh reader thread that
    /// shares the same channel. Cleared when the active step ends.
    pub(crate) container_stdin_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

#[async_trait::async_trait]
impl crate::command::commands::squad::commands::SquadCommandFrontend for CliFrontend {
    /// The CLI runs in the user's own shell, so the process's current
    /// directory is theirs and the mount-scope question can be put to them.
    fn is_local_user_session(&self) -> bool {
        true
    }

    async fn serve_squad_daemon(
        &mut self,
        config: crate::command::commands::squad::commands::SquadServeConfig,
    ) -> Result<(), CommandError> {
        crate::frontend::squad::serve(config).await
    }

    fn ask_task_name(&mut self) -> Result<String, CommandError> {
        require_named_input("task name (slug)?")
    }

    /// The same multi-line stdin read `new spec --interview` uses, so the CLI
    /// interview collects the same freeform description the TUI's multiline
    /// dialog does.
    fn ask_task_description(&mut self) -> Result<String, CommandError> {
        require_multiline_input(
            "describe the new squad task including its triggering conditions and how squad \
             should handle the task each time it is triggered?",
        )
    }

    fn ask_task_workspace_choice(
        &mut self,
    ) -> Result<crate::command::commands::squad::commands::TaskWorkspaceChoice, CommandError> {
        use crate::command::commands::squad::commands::TaskWorkspaceChoice;
        let choice = super::per_command::helpers::pick_numbered(
            "task workspace?",
            &["Default Task Workspace", "Custom Folder / Repo"],
            1,
        );
        Ok(if choice == 2 {
            TaskWorkspaceChoice::CustomFolderOrRepo
        } else {
            TaskWorkspaceChoice::DefaultTaskWorkspace
        })
    }

    fn ask_task_overlay(&mut self, existing: &[String]) -> Result<Option<String>, CommandError> {
        let prompt = format!(
            "add an overlay ({} so far)? [dir()/ssh()/env()/skill() syntax, blank to finish]",
            existing.len()
        );
        match super::per_command::helpers::read_line(&prompt) {
            Some(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            _ => Ok(None),
        }
    }

    fn confirm_non_git_workspace(&mut self, path: &Path) -> Result<bool, CommandError> {
        Ok(super::per_command::helpers::yes_no(
            &format!(
                "{} is not the root of a Git repository. Keep this path? \
                 (no = choose a different one)",
                path.display()
            ),
            false,
        ))
    }

    fn confirm_parent_directory_workspace(
        &mut self,
        path: &Path,
        current_dir: &Path,
    ) -> Result<bool, CommandError> {
        Ok(super::per_command::helpers::yes_no(
            &format!(
                "{} is a parent directory of {}. Mount it anyway? \
                 (no = choose a different one)",
                path.display(),
                current_dir.display()
            ),
            false,
        ))
    }

    fn ask_task_interval(&mut self) -> Result<String, CommandError> {
        // Blank keeps the documented default; a value is parsed & validated in
        // Layer 1/2, never here.
        match super::per_command::helpers::read_line("evaluation interval [6h]?") {
            Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Some(_) => Ok("6h".to_string()),
            None => Err(CommandError::InteractiveInputUnavailable {
                prompt: "evaluation interval".into(),
            }),
        }
    }

    fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
        match super::per_command::helpers::read_line("repository directory [current dir]?") {
            Some(s) if !s.trim().is_empty() => Ok(PathBuf::from(s.trim())),
            Some(_) => std::env::current_dir().map_err(|error| {
                CommandError::Other(format!("cannot resolve current dir: {error}"))
            }),
            None => Err(CommandError::InteractiveInputUnavailable {
                prompt: "repository directory".into(),
            }),
        }
    }

    fn ask_task_agent(&mut self) -> Result<Option<String>, CommandError> {
        match super::per_command::helpers::read_line("leader agent (optional, Enter to skip)?") {
            Some(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            _ => Ok(None),
        }
    }

    fn ask_task_model(&mut self) -> Result<Option<String>, CommandError> {
        match super::per_command::helpers::read_line("leader model (optional, Enter to skip)?") {
            Some(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            _ => Ok(None),
        }
    }

    fn ask_task_mount_scope(
        &mut self,
    ) -> Result<crate::data::fs::task_store::MountScope, CommandError> {
        use crate::data::fs::task_store::MountScope;
        match super::per_command::helpers::read_line("mount scope: [gitroot]/cwd?") {
            Some(s) if s.trim().eq_ignore_ascii_case("cwd") => Ok(MountScope::Cwd),
            Some(_) => Ok(MountScope::GitRoot),
            None => Err(CommandError::InteractiveInputUnavailable {
                prompt: "mount scope".into(),
            }),
        }
    }

    fn ask_delete_task_dir(
        &mut self,
        _name: &str,
        path: &std::path::Path,
    ) -> Result<bool, CommandError> {
        // No TTY (piped / --non-interactive) keeps the safe default: do not
        // delete the persistent directory.
        if self.non_interactive || !super::output::stdin_is_tty() {
            return Ok(false);
        }
        eprintln!(
            "awman: also delete the task directory {}? [y/N]",
            path.display()
        );
        match super::per_command::helpers::read_line("choice [y/N]:") {
            Some(s) => Ok(matches!(s.trim().to_lowercase().as_str(), "y" | "yes")),
            None => Ok(false),
        }
    }
}

/// RAII guard: enables raw mode on creation, disables it on drop.
pub(crate) struct RawModeGuard;

impl RawModeGuard {
    pub fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl CliFrontend {
    pub fn new(matches: ArgMatches) -> Self {
        let command_path = command_path_from_matches(&matches);
        let explicit_flag = {
            let path_strs: Vec<&str> = command_path.iter().map(|s| s.as_str()).collect();
            let mut m = &matches;
            for seg in &path_strs {
                match m.subcommand_matches(seg) {
                    Some(sub) => m = sub,
                    None => break,
                }
            }
            m.try_get_one::<bool>("non-interactive")
                .ok()
                .flatten()
                .copied()
                .unwrap_or(false)
        };
        let non_interactive = crate::frontend::effective_non_interactive(explicit_flag);
        Self {
            matches,
            command_path,
            messages: CliUserMessageQueue::new(),
            non_interactive,
            yolo_stdin_rx: None,
            last_sink_message_time: None,
            raw_mode_guard: None,
            stdin_reader_shutdown: None,
            stdin_reader_handle: None,
            container_stdin_tx: None,
        }
    }

    /// Returns `true` when the `--json` flag is active for the current
    /// command. Used by per-command frontends to suppress human-readable
    /// output (e.g. the ready summary box) when structured JSON is requested.
    pub(crate) fn is_json_mode(&self) -> bool {
        let path_strs: Vec<&str> = self.command_path.iter().map(|s| s.as_str()).collect();
        self.matches_for(&path_strs)
            .and_then(|m| m.try_get_one::<bool>("json").ok().flatten().copied())
            .unwrap_or(false)
    }

    /// Resolve the [`ArgMatches`] sub-tree corresponding to `command_path`.
    fn matches_for(&self, command_path: &[&str]) -> Option<&ArgMatches> {
        let mut current = &self.matches;
        for seg in command_path {
            current = current.subcommand_matches(seg)?;
        }
        Some(current)
    }
}

// Returns true if the given command path declares a Bool-kind flag with
// the given long name. Used by `flag_bool` to differentiate "flag not in
// catalogue at all" (return `None`) from "flag absent on argv" (return
// `Some(false)` so the catalogue's default takes over).
fn command_has_bool_flag(command_path: &[&str], flag: &str) -> bool {
    let cat = CommandCatalogue::get();
    cat.lookup(command_path)
        .and_then(|spec| spec.find_flag(flag))
        .map(|f| matches!(f.kind, FlagKind::Bool))
        .unwrap_or(false)
}

// ─── command path extraction ───────────────────────────────────────────────

/// Extract the canonical command path from a parsed `clap::ArgMatches`.
///
/// `clap` records subcommands recursively via `subcommand_name`; the CLI
/// frontend translates that chain into the catalogue path that
/// [`Dispatch::run_command`] consumes.
pub fn command_path_from_matches(matches: &ArgMatches) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, sub)) = current.subcommand() {
        path.push(name.to_string());
        current = sub;
    }
    path
}

// ─── UserMessageSink (delegates to the queue) ──────────────────────────────

impl UserMessageSink for CliFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.messages.replay_queued();
    }
}

// ─── CommandFrontend ───────────────────────────────────────────────────────

impl CommandFrontend for CliFrontend {
    fn flag_bool(&self, command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(None);
        };
        // ArgAction::SetTrue stores `false` when the flag is absent from
        // argv. Surface that verbatim — the catalogue's `default` field
        // already encodes the desired absent-value semantics.
        if m.try_get_one::<bool>(flag).ok().flatten().is_none()
            && !command_has_bool_flag(command_path, flag)
        {
            return Ok(None);
        }
        Ok(Some(m.get_flag(flag)))
    }

    fn flag_string(
        &self,
        command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(None);
        };
        Ok(m.get_one::<String>(flag).cloned())
    }

    fn flag_strings(&self, command_path: &[&str], flag: &str) -> Result<Vec<String>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(Vec::new());
        };
        Ok(m.get_many::<String>(flag)
            .map(|vals| vals.cloned().collect())
            .unwrap_or_default())
    }

    fn flag_path(
        &self,
        command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(None);
        };
        Ok(m.get_one::<String>(flag).map(PathBuf::from))
    }

    fn flag_enum(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError> {
        // Enum flags are stored as strings in our clap projection.
        self.flag_string(command_path, flag)
    }

    fn flag_u16(&self, command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(None);
        };
        Ok(m.get_one::<u16>(flag).copied())
    }

    fn flag_usize(&self, command_path: &[&str], flag: &str) -> Result<Option<usize>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(None);
        };
        Ok(m.get_one::<usize>(flag).copied())
    }

    fn argument(&self, command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(None);
        };
        // For trailing-var-args arguments, take the joined string when only
        // a single positional value was provided. For typed positionals,
        // clap stores the single value as a String.
        if let Some(spec) = CommandCatalogue::get().lookup(command_path) {
            if let Some(arg) = spec.arguments.iter().find(|a| a.name == name) {
                if matches!(arg.kind, ArgumentKind::TrailingVarArgs) {
                    let collected: Vec<String> = m
                        .get_many::<String>(name)
                        .map(|v| v.cloned().collect())
                        .unwrap_or_default();
                    return Ok(if collected.is_empty() {
                        None
                    } else {
                        Some(collected.join(" "))
                    });
                }
            }
        }
        Ok(m.get_one::<String>(name).cloned())
    }

    fn arguments(&self, command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        let Some(m) = self.matches_for(command_path) else {
            return Ok(Vec::new());
        };
        Ok(m.get_many::<String>(name)
            .map(|v| v.cloned().collect())
            .unwrap_or_default())
    }
}

// ─── Per-command frontend trait impls ──────────────────────────────────────
//
// Each `*CommandFrontend` trait that has no extra methods (e.g. `Auth`,
// `Specs`, `Config`, `Download`, `New`, `Remote`, `Status`) is satisfied by
// the supertrait `UserMessageSink + Send + Sync` impls already in scope —
// just declare the marker impl here.
//
// The richer per-command traits (`Init`, `Ready`, `Chat`, `ExecPrompt`,
// `ExecWorkflow`, `Api`) gain method bodies in the per-command
// modules under `src/frontend/cli/per_command/`.

impl AuthCommandFrontend for CliFrontend {
    fn ask_consent(
        &mut self,
        default: bool,
    ) -> Result<crate::command::commands::auth::AuthConsentChoice, CommandError> {
        use crate::command::commands::auth::AuthConsentChoice;
        // TTY-aware: when stdin is not a TTY, use the default. Otherwise
        // prompt for [y]es / [n]o / [o]nce.
        if !crate::frontend::cli::output::stdin_is_tty() {
            return Ok(if default {
                AuthConsentChoice::Accept
            } else {
                AuthConsentChoice::Decline
            });
        }
        let suffix = if default { "[Y/n/o]" } else { "[y/N/o]" };
        eprintln!("awman: persist agent auth consent for this repo? {suffix}");
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(if default {
                AuthConsentChoice::Accept
            } else {
                AuthConsentChoice::Decline
            });
        }
        Ok(match buf.trim() {
            "y" | "Y" => AuthConsentChoice::Accept,
            "n" | "N" => AuthConsentChoice::Decline,
            "o" | "O" => AuthConsentChoice::Once,
            _ => {
                if default {
                    AuthConsentChoice::Accept
                } else {
                    AuthConsentChoice::Decline
                }
            }
        })
    }
}
impl ConfigCommandFrontend for CliFrontend {
    fn present_config_table(
        &mut self,
        _rows: &[crate::command::commands::config::ConfigFieldRow],
        _rejected: Option<&crate::command::commands::config::ConfigEditRejection>,
    ) -> Result<
        Option<crate::command::commands::config::ConfigEditRequest>,
        crate::command::error::CommandError,
    > {
        Ok(None)
    }
}
impl DownloadCommandFrontend for CliFrontend {}
impl NewCommandFrontend for CliFrontend {
    fn ask_workflow_name(&mut self) -> Result<String, CommandError> {
        require_named_input("workflow name?")
    }
    fn ask_workflow_title(&mut self) -> Result<String, CommandError> {
        require_named_input("workflow title (human-readable)?")
    }
    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        require_multiline_input("workflow description?")
    }
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        require_named_input("step name?")
    }
    fn ask_workflow_step_agent(&mut self) -> Result<Option<String>, CommandError> {
        match require_optional_input("agent (optional, Enter to skip)?") {
            Ok(s) if s.is_empty() => Ok(None),
            Ok(s) => Ok(Some(s)),
            Err(_) => Ok(None),
        }
    }
    fn ask_workflow_step_model(&mut self) -> Result<Option<String>, CommandError> {
        match require_optional_input("model (optional, Enter to skip)?") {
            Ok(s) if s.is_empty() => Ok(None),
            Ok(s) => Ok(Some(s)),
            Err(_) => Ok(None),
        }
    }
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        require_multiline_input("step prompt?")
    }
    fn ask_add_another_step(&mut self) -> Result<bool, CommandError> {
        match require_optional_input("add another step? [y/N]") {
            Ok(s) => Ok(matches!(s.trim().to_lowercase().as_str(), "y" | "yes")),
            Err(_) => Ok(false),
        }
    }
    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        require_named_input("skill name?")
    }
    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        require_multiline_input("skill description?")
    }
    fn ask_skill_body(&mut self) -> Result<String, CommandError> {
        require_optional_input("skill body (one line)?")
    }
}
impl RemoteCommandFrontend for CliFrontend {}
impl SpecsCommandFrontend for CliFrontend {
    fn ask_spec_kind(&mut self) -> Result<WorkItemKind, CommandError> {
        use super::output::stdin_is_tty;
        if !stdin_is_tty() {
            return Ok(WorkItemKind::Task);
        }
        eprintln!("awman: work item kind?");
        eprintln!("  [1] Feature");
        eprintln!("  [2] Bug");
        eprintln!("  [3] Task");
        eprintln!("  [4] Enhancement");
        Ok(
            match super::per_command::helpers::read_line("choice [1-4]:").as_deref() {
                Some("1") | Some("f") | Some("F") | Some("feature") => WorkItemKind::Feature,
                Some("2") | Some("b") | Some("B") | Some("bug") => WorkItemKind::Bug,
                Some("4") | Some("e") | Some("E") | Some("enhancement") => {
                    WorkItemKind::Enhancement
                }
                _ => WorkItemKind::Task,
            },
        )
    }

    fn ask_spec_title(&mut self) -> Result<String, CommandError> {
        require_named_input("spec title?")
    }

    fn ask_spec_summary(&mut self) -> Result<String, CommandError> {
        require_multiline_input("spec description?")
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::per_command::CliContainerProxy)
    }

    fn set_pty_active(&mut self, active: bool) {
        self.messages.set_pty_active(active);
    }
}

/// Read a non-empty line from stdin, or surface
/// `CommandError::InteractiveInputUnavailable` when stdin is not a TTY (or
/// the user submitted an empty value). Used for prompts where there is no
/// safe default — callers expect *something* to come back.
fn require_named_input(prompt: &str) -> Result<String, CommandError> {
    match super::per_command::helpers::read_line(prompt) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(CommandError::InteractiveInputUnavailable {
            prompt: prompt.to_string(),
        }),
    }
}

/// Read a (possibly empty) line from stdin, but require a TTY so callers that
/// expect a real answer don't silently get `""` from a piped invocation.
fn require_optional_input(prompt: &str) -> Result<String, CommandError> {
    match super::per_command::helpers::read_line(prompt) {
        Some(s) => Ok(s),
        None => Err(CommandError::InteractiveInputUnavailable {
            prompt: prompt.to_string(),
        }),
    }
}

/// Read multi-line input from stdin (until a blank line or EOF), but require a
/// TTY so callers don't silently get `""` from a piped invocation.
fn require_multiline_input(prompt: &str) -> Result<String, CommandError> {
    match super::per_command::helpers::read_multiline(prompt) {
        Some(s) => Ok(s),
        None => Err(CommandError::InteractiveInputUnavailable {
            prompt: prompt.to_string(),
        }),
    }
}
// ApiServerCommandFrontend for CliFrontend is in per_command/api_server.rs

impl StatusCommandFrontend for CliFrontend {
    /// Watch loop continues until the user presses Ctrl+C.
    ///
    /// First invocation spawns a tokio task that awaits a SIGINT and flips a
    /// process-global atomic; subsequent invocations only read the flag, so
    /// the loop exits cleanly on the next tick.
    fn should_continue_watching(&mut self) -> bool {
        use std::sync::atomic::Ordering;
        ensure_watch_signal_handler_installed();
        !WATCH_INTERRUPTED.load(Ordering::Relaxed)
    }

    /// Clear the screen between watch ticks (ANSI clear + cursor home).
    fn write_clear_marker(&mut self) {
        use std::io::Write;
        let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
        let _ = std::io::stdout().flush();
    }
}

// ─── Watch-loop Ctrl+C handler ───────────────────────────────────────────────

/// Process-global flag flipped to `true` when SIGINT arrives. Only consulted
/// by `StatusCommandFrontend::should_continue_watching` for the CLI; other
/// frontends manage their own interrupt semantics.
static WATCH_INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the SIGINT-watcher task has been spawned yet (it must be spawned
/// inside an async runtime, and we only want one instance).
static WATCH_HANDLER_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Install a tokio task that awaits Ctrl+C and flips `WATCH_INTERRUPTED`.
/// Idempotent — safe to call on every tick.
fn ensure_watch_signal_handler_installed() {
    use std::sync::atomic::Ordering;
    if WATCH_HANDLER_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // Spawn only succeeds when called inside a tokio runtime, which is
        // always the case at this point (the StatusCommand body is async).
        tokio::spawn(async {
            let _ = tokio::signal::ctrl_c().await;
            WATCH_INTERRUPTED.store(true, Ordering::SeqCst);
        });
    }
}

// `ApiServerStartCommandFrontend` requires a `serve_until_shutdown` method
// — provided in `per_command::api_server`.

// Check that flag_bool returns sensible values for SetTrue actions:
// when not present, clap fills `false`; we surface that as `Some(false)`
// so the catalogue's default field carries through.
#[cfg(test)]
mod tests {
    use super::*;

    // ─── command_path_from_matches ─────────────────────────────────────────────

    #[test]
    fn command_path_from_matches_extracts_nested_subcommand() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml"])
            .unwrap();
        let path = command_path_from_matches(&m);
        assert_eq!(path, vec!["exec", "workflow"]);
    }

    #[test]
    fn command_path_from_matches_top_level_subcommand() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let path = command_path_from_matches(&m);
        assert_eq!(path, vec!["status"]);
    }

    #[test]
    fn command_path_from_matches_bare_invocation_is_empty() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman"]).unwrap();
        let path = command_path_from_matches(&m);
        assert!(path.is_empty());
    }

    #[test]
    fn command_path_from_matches_three_level_deep() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "remote", "session", "start"])
            .unwrap();
        let path = command_path_from_matches(&m);
        assert_eq!(path, vec!["remote", "session", "start"]);
    }

    // ─── flag_bool ────────────────────────────────────────────────────────────

    #[test]
    fn flag_bool_reads_set_true_flag_from_arg_matches() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml", "--yolo"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_bool(&["exec", "workflow"], "yolo").unwrap();
        assert_eq!(v, Some(true));
    }

    #[test]
    fn flag_bool_absent_returns_some_false_for_known_bool_flag() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        // ArgAction::SetTrue stores false when the flag is absent.
        let v = frontend.flag_bool(&["exec", "workflow"], "yolo").unwrap();
        assert_eq!(v, Some(false));
    }

    #[test]
    fn flag_bool_wrong_path_returns_none() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let frontend = CliFrontend::new(m);
        // Querying a flag on a different subcommand path returns None.
        let v = frontend.flag_bool(&["init"], "aspec").unwrap();
        assert_eq!(v, None);
    }

    /// Data-table: (argv, command_path, flag, expected_value)
    #[test]
    fn flag_bool_data_table() {
        struct Case {
            argv: &'static [&'static str],
            path: &'static [&'static str],
            flag: &'static str,
            expected: Option<bool>,
        }
        let cases = [
            Case {
                argv: &["awman", "init", "--aspec"],
                path: &["init"],
                flag: "aspec",
                expected: Some(true),
            },
            Case {
                argv: &["awman", "init"],
                path: &["init"],
                flag: "aspec",
                expected: Some(false),
            },
            Case {
                argv: &["awman", "ready", "--build"],
                path: &["ready"],
                flag: "build",
                expected: Some(true),
            },
            Case {
                argv: &["awman", "ready", "--no-cache"],
                path: &["ready"],
                flag: "no-cache",
                expected: Some(true),
            },
            Case {
                argv: &["awman", "ready"],
                path: &["ready"],
                flag: "no-cache",
                expected: Some(false),
            },
            Case {
                argv: &["awman", "chat", "--yolo"],
                path: &["chat"],
                flag: "yolo",
                expected: Some(true),
            },
            Case {
                argv: &["awman", "chat"],
                path: &["chat"],
                flag: "yolo",
                expected: Some(false),
            },
            Case {
                argv: &["awman", "status", "--watch"],
                path: &["status"],
                flag: "watch",
                expected: Some(true),
            },
            Case {
                argv: &["awman", "config", "set", "agent", "claude"],
                path: &["config", "set"],
                flag: "global",
                expected: Some(false),
            },
            Case {
                argv: &["awman", "config", "set", "agent", "claude", "--global"],
                path: &["config", "set"],
                flag: "global",
                expected: Some(true),
            },
        ];
        let cat = CommandCatalogue::get();
        let clap_cmd = cat.build_clap_command();
        for (i, case) in cases.iter().enumerate() {
            let m = clap_cmd
                .clone()
                .try_get_matches_from(case.argv)
                .unwrap_or_else(|e| panic!("case {i}: failed to parse {:?}: {e}", case.argv));
            let frontend = CliFrontend::new(m);
            let got = frontend
                .flag_bool(case.path, case.flag)
                .unwrap_or_else(|e| panic!("case {i}: flag_bool error: {e}"));
            assert_eq!(got, case.expected, "case {i}: argv={:?}", case.argv);
        }
    }

    // ─── flag_string / flag_enum ───────────────────────────────────────────────

    #[test]
    fn flag_enum_reads_agent_on_init() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "init", "--agent", "codex"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_enum(&["init"], "agent").unwrap();
        assert_eq!(v, Some("codex".to_string()));
    }

    #[test]
    fn flag_enum_default_returns_catalogue_default() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "init"]).unwrap();
        let frontend = CliFrontend::new(m);
        // The catalogue default for `--agent` on `init` is "claude".
        let v = frontend.flag_enum(&["init"], "agent").unwrap();
        assert_eq!(v, Some("claude".to_string()));
    }

    #[test]
    fn flag_string_optional_agent_absent_returns_none() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "chat"]).unwrap();
        let frontend = CliFrontend::new(m);
        // `--agent` on chat is OptionalString with no default.
        let v = frontend.flag_string(&["chat"], "agent").unwrap();
        assert_eq!(v, None);
    }

    #[test]
    fn flag_string_optional_agent_present() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "chat", "--agent", "gemini"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_string(&["chat"], "agent").unwrap();
        assert_eq!(v, Some("gemini".to_string()));
    }

    #[test]
    fn flag_string_wrong_path_returns_none() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_string(&["init"], "agent").unwrap();
        assert_eq!(v, None);
    }

    // ─── flag_strings (VecString) ─────────────────────────────────────────────

    #[test]
    fn flag_strings_reads_single_overlay() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "chat", "--overlay", "/src"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_strings(&["chat"], "overlay").unwrap();
        assert_eq!(v, vec!["/src".to_string()]);
    }

    #[test]
    fn flag_strings_reads_repeated_overlay_flags() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "chat", "--overlay", "/a", "--overlay", "/b"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_strings(&["chat"], "overlay").unwrap();
        assert_eq!(v, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn flag_strings_returns_empty_when_flag_absent() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "chat"]).unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_strings(&["chat"], "overlay").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn flag_strings_wrong_path_returns_empty() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_strings(&["chat"], "overlay").unwrap();
        assert!(v.is_empty());
    }

    // ─── flag_path (Path / OptionalPath) ─────────────────────────────────────

    #[test]
    fn flag_path_reads_path_argument_for_exec_workflow() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "/path/to/wf.toml"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend
            .argument(&["exec", "workflow"], "workflow")
            .unwrap();
        assert_eq!(v, Some("/path/to/wf.toml".to_string()));
    }

    #[test]
    fn flag_path_reads_first_positional_argument_for_path_args() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend
            .argument(&["exec", "workflow"], "workflow")
            .unwrap();
        assert_eq!(v, Some("wf.toml".to_string()));
    }

    // ─── flag_u16 ─────────────────────────────────────────────────────────────

    #[test]
    fn flag_u16_reads_port_flag() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "api", "start", "--port", "1234"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_u16(&["api", "start"], "port").unwrap();
        assert_eq!(v, Some(1234u16));
    }

    #[test]
    fn flag_u16_default_value_when_absent() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "api", "start"]).unwrap();
        let frontend = CliFrontend::new(m);
        // Default for `--port` on `api start` is 9876.
        let v = frontend.flag_u16(&["api", "start"], "port").unwrap();
        assert_eq!(v, Some(9876u16));
    }

    #[test]
    fn flag_u16_wrong_path_returns_none() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.flag_u16(&["api", "start"], "port").unwrap();
        assert_eq!(v, None);
    }

    // ─── argument ─────────────────────────────────────────────────────────────

    #[test]
    fn argument_reads_work_item_positional() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "specs", "amend", "0069"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.argument(&["specs", "amend"], "work_item").unwrap();
        assert_eq!(v, Some("0069".to_string()));
    }

    #[test]
    fn argument_remote_exec_prompt_reads_prompt() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "remote", "exec", "prompt", "hello world"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend
            .argument(&["remote", "exec", "prompt"], "prompt")
            .unwrap();
        assert_eq!(v, Some("hello world".to_string()));
    }

    #[test]
    fn argument_remote_exec_workflow_reads_workflow() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "remote", "exec", "workflow", "my-wf.toml"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend
            .argument(&["remote", "exec", "workflow"], "workflow")
            .unwrap();
        assert_eq!(v, Some("my-wf.toml".to_string()));
    }

    #[test]
    fn argument_wrong_path_returns_none() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.argument(&["specs", "amend"], "work_item").unwrap();
        assert_eq!(v, None);
    }

    // ─── arguments (plural) ───────────────────────────────────────────────────

    #[test]
    fn arguments_wrong_path_returns_empty_vec() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        let frontend = CliFrontend::new(m);
        let v = frontend.arguments(&["remote", "run"], "command").unwrap();
        assert!(v.is_empty());
    }

    // ─── Cross-flag interaction tests ─────────────────────────────────────────

    #[test]
    fn multiple_flags_extracted_independently_from_same_invocation() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from([
                "awman",
                "chat",
                "--yolo",
                "--agent",
                "gemini",
                "--overlay",
                "/src",
                "--overlay",
                "/etc",
            ])
            .unwrap();
        let frontend = CliFrontend::new(m);
        assert_eq!(frontend.flag_bool(&["chat"], "yolo").unwrap(), Some(true));
        assert_eq!(
            frontend.flag_string(&["chat"], "agent").unwrap(),
            Some("gemini".to_string())
        );
        assert_eq!(
            frontend.flag_strings(&["chat"], "overlay").unwrap(),
            vec!["/src".to_string(), "/etc".to_string()]
        );
    }

    #[test]
    fn querying_flags_on_parent_path_when_child_was_invoked_returns_none() {
        // Invoked `exec workflow`, querying on `exec` (parent) returns None
        // because `exec` itself has no ArgMatches with those flags.
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml", "--yolo"])
            .unwrap();
        let frontend = CliFrontend::new(m);
        // Querying the `exec` path (not the `exec workflow` path).
        let v = frontend.flag_bool(&["exec"], "yolo").unwrap();
        assert_eq!(v, None);
    }
}
