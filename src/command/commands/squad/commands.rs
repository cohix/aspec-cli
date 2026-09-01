//! The single Layer-2 squad command family.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::squad::daemon::{
    SquadDaemonCommand, SquadDaemonOutcome, SquadDaemonSubcommand, SquadLogsFlags, SquadStartFlags,
    SquadStatusFlags, SquadStopFlags,
};
use crate::command::commands::squad::gateway::{
    CreateTask, DaemonStatus, TaskDetail, TaskGateway, DEFAULT_RUN_HISTORY_LIMIT,
};
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::task_store::{MountScope, Task, TaskStatus, TaskWorkspace};
use crate::data::fs::SquadPaths;
use crate::data::message::UserMessageSink;
use crate::engine::git::GitEngine;

/// The two workspace choices offered at task creation.
///
/// Deliberately distinct from [`TaskWorkspace`]: this is what the *choice
/// step* answers, before any path has been collected. The path prompt is a
/// separate step, so a user who picks the default is never shown one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskWorkspaceChoice {
    /// Bind the task to its durable `~/.awman/squad/tasks/<name>/workspace/`.
    DefaultTaskWorkspace,
    /// Bind the task to a folder or repository the user names next.
    CustomFolderOrRepo,
}

#[derive(Debug, Clone)]
pub struct SquadServeConfig {
    pub port: u16,
    pub dangerously_skip_auth: bool,
}

#[async_trait]
pub trait SquadCommandFrontend: UserMessageSink + Send + Sync {
    async fn serve_squad_daemon(&mut self, _config: SquadServeConfig) -> Result<(), CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "squad start".into(),
            frontend: "this".into(),
        })
    }

    // ── Task-creation interview (BLOCKER-3, §9.3) ──────────────────────
    //
    // These COLLECT input; they must not validate or reject an answer — that
    // stays in `LocalTaskGateway::validate_create`. Each defaults to an
    // error so a non-interactive frontend (the daemon's API frontend) refuses
    // an interview rather than inventing answers.

    fn ask_task_name(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_task_description(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    /// Raw interval spec (e.g. `6h`); Layer 2 parses and Layer 1 validates it.
    fn ask_task_interval(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    /// Which workspace the task is bound to: the durable per-task directory,
    /// or a folder/repo the user names. Asked after the description and before
    /// mount scope, so the user is never made to type a path they did not
    /// choose to type.
    fn ask_task_workspace_choice(&mut self) -> Result<TaskWorkspaceChoice, CommandError> {
        Err(interview_unavailable())
    }

    /// The custom workspace path. Only asked when
    /// [`ask_task_workspace_choice`](Self::ask_task_workspace_choice) chose
    /// [`TaskWorkspaceChoice::Custom`].
    fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
        Err(interview_unavailable())
    }

    /// Warn that a chosen custom path is not the root of a git repository, and
    /// ask whether to keep it. `true` keeps the path (the task then runs
    /// directly against that folder with no worktree); `false` loops back to
    /// the path prompt. Refusing by default keeps a non-interactive frontend
    /// from silently accepting a path it never showed anyone.
    fn confirm_non_git_workspace(&mut self, _path: &Path) -> Result<bool, CommandError> {
        Ok(false)
    }

    /// Confirm mounting a custom workspace that is a parent directory of the
    /// session's current directory. This is the same confirmation every other
    /// awman mount-scope flow applies before a parent directory is mounted
    /// (`aspec/architecture/security.md`); squad's custom-folder entry point
    /// does not get to bypass it. Refusing by default is the safe answer for a
    /// frontend that cannot ask.
    fn confirm_parent_directory_workspace(
        &mut self,
        _path: &Path,
        _current_dir: &Path,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }

    /// One overlay spec in `--overlay` syntax, or `None` to finish. Called
    /// repeatedly until it answers `None`. Layer 1 validates the syntax at
    /// creation; this only collects.
    fn ask_task_overlay(&mut self, _existing: &[String]) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    fn ask_task_agent(&mut self) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_task_model(&mut self) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_task_mount_scope(&mut self) -> Result<MountScope, CommandError> {
        Err(interview_unavailable())
    }

    /// Whether this frontend is the user's own session on the host that chose
    /// the paths in the request — and therefore whether the process's current
    /// directory is the user's and a human is there to answer a mount-scope
    /// question.
    ///
    /// `false` for the daemon's API frontend, which re-executes a `squad add`
    /// a client already authorised, from a working directory unrelated to the
    /// caller's. Mount-scope policy that compares against the current directory
    /// is applied on the client, once, not again in the daemon.
    fn is_local_user_session(&self) -> bool {
        false
    }

    /// Ask whether to delete a task's persistent directory on `remove`
    /// (BLOCKER-2, §9.2). Defaults to `Ok(false)` so the daemon's API frontend
    /// never removes a directory; only an interactive frontend answers `true`.
    fn ask_delete_task_dir(&mut self, _name: &str, _path: &Path) -> Result<bool, CommandError> {
        Ok(false)
    }
}

fn interview_unavailable() -> CommandError {
    CommandError::NotAvailableForFrontend {
        command: "squad add --interview".into(),
        frontend: "this".into(),
    }
}

/// Fields for `squad add`. In interview mode Layer 2 collects every field
/// through the frontend's `ask_task_*` methods; otherwise `prefilled`
/// carries the flag-derived task assembled by Dispatch.
pub struct SquadAddRequest {
    pub interview: bool,
    /// `-n/--non-interactive`: never put a question to the user. The one
    /// question scripted creation can still raise is the parent-directory
    /// mount-scope confirmation; under this flag it is refused outright rather
    /// than asked, so a CI invocation cannot block on a prompt or silently
    /// widen a mount.
    pub non_interactive: bool,
    pub prefilled: Option<CreateTask>,
}

pub enum SquadSubcommand {
    Start(SquadStartFlags),
    Stop(SquadStopFlags),
    Status(SquadStatusFlags),
    Logs(SquadLogsFlags),
    Add(SquadAddRequest),
    List,
    Show(String),
    Remove { name: String, yes: bool },
    Pause(String),
    Resume(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum SquadOutcome {
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
    Task(Task),
    Detail(TaskDetail),
    Tasks(Vec<Task>),
    Removed {
        name: String,
        /// The persistent task directory that was deleted, when one was.
        /// `None` means the directory was kept (declined) or absent.
        removed_dir: Option<PathBuf>,
    },
    Ok,
}

pub struct SquadCommand {
    sub: SquadSubcommand,
    gateway: Option<Box<dyn TaskGateway>>,
    engines: Engines,
}

impl SquadCommand {
    pub fn new(
        sub: SquadSubcommand,
        gateway: Option<Box<dyn TaskGateway>>,
        engines: Engines,
    ) -> Self {
        Self {
            sub,
            gateway,
            engines,
        }
    }
}

#[async_trait]
impl Command for SquadCommand {
    type Frontend = Box<dyn SquadCommandFrontend>;
    type Outcome = SquadOutcome;
    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let SquadCommand {
            sub,
            gateway,
            engines,
        } = self;
        match sub {
            SquadSubcommand::Start(flags) => daemon_outcome(
                SquadDaemonCommand::new(SquadDaemonSubcommand::Start(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            SquadSubcommand::Stop(flags) => daemon_outcome(
                SquadDaemonCommand::new(SquadDaemonSubcommand::Stop(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            SquadSubcommand::Status(flags) => {
                // Liveness, PID and bound address come from the pidfile/sidecar
                // (correct even when the daemon is down). A gateway is injected
                // only when the daemon has published its endpoint, so its
                // presence is the "daemon reachable" signal: overlay the live
                // scheduler counts from `gateway.status()`. A stopped daemon
                // means no gateway (no HTTP call); a present-but-failing gateway
                // degrades to the pidfile-only answer rather than failing (§9.4).
                let outcome =
                    SquadDaemonCommand::new(SquadDaemonSubcommand::Status(flags), engines)
                        .run_with_frontend(frontend)
                        .await?;
                let SquadDaemonOutcome::Status(mut status) = outcome else {
                    unreachable!("status subcommand yields a status outcome");
                };
                if let Some(gateway) = &gateway {
                    if let Ok(live) = gateway.status().await {
                        status.running = live.running;
                        status.task_count = live.task_count;
                        status.active_count = live.active_count;
                        status.last_tick = live.last_tick;
                        status.in_flight = live.in_flight;
                    }
                }
                Ok(SquadOutcome::Status(status))
            }
            SquadSubcommand::Logs(flags) => daemon_outcome(
                SquadDaemonCommand::new(SquadDaemonSubcommand::Logs(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            sub => {
                let gateway = gateway.ok_or_else(|| CommandError::Other("squad tasks are served by the squad daemon; start it with `awman squad start`".into()))?;
                match sub {
                    SquadSubcommand::Add(request) => {
                        // In interview mode Layer 2 collects every field through
                        // the frontend; otherwise Dispatch already assembled the
                        // task from flags. Validation stays in Layer 1's
                        // `LocalTaskGateway::validate_create`.
                        let req = match request.prefilled {
                            Some(req) => {
                                // The interview asks this inside its own path
                                // loop (so a refusal can offer a different
                                // path); the scripted path has no loop, so the
                                // same policy is applied here. Both entry
                                // points therefore go through
                                // `workspace_is_parent_of`, and neither can
                                // mount a parent directory unconfirmed.
                                confirm_scripted_workspace_scope(
                                    frontend.as_mut(),
                                    &req.workspace,
                                    std::env::current_dir().ok().as_deref(),
                                    request.non_interactive,
                                )?;
                                req
                            }
                            None => collect_task_interview(
                                frontend.as_mut(),
                                engines.git_engine.as_ref(),
                            )?,
                        };
                        Ok(SquadOutcome::Task(gateway.create(req).await?))
                    }
                    SquadSubcommand::List => Ok(SquadOutcome::Tasks(gateway.list().await?)),
                    SquadSubcommand::Show(name) => {
                        // One response shape for both gateways: the task
                        // and its recent runs travel together, so the remote
                        // façade never has to guess which type came back.
                        let task = gateway.get(&name).await?;
                        let runs = gateway.runs(&name, DEFAULT_RUN_HISTORY_LIMIT).await?;
                        Ok(SquadOutcome::Detail(TaskDetail { task, runs }))
                    }
                    SquadSubcommand::Remove { name, yes } => {
                        gateway.delete(&name).await?;
                        // The persistent directory removal is a filesystem
                        // concern that lives here, in Layer 2, guarded by the
                        // frontend's confirmation answer (or `-y`). The path is
                        // resolved through `SquadPaths::task_dir`, which is
                        // `validate_under_root`-guarded, so a crafted name can
                        // never escape the tasks root.
                        let removed_dir = remove_task_dir(frontend.as_mut(), &name, yes)?;
                        Ok(SquadOutcome::Removed { name, removed_dir })
                    }
                    SquadSubcommand::Pause(name) => {
                        gateway.set_status(&name, TaskStatus::Paused).await?;
                        Ok(SquadOutcome::Ok)
                    }
                    SquadSubcommand::Resume(name) => {
                        gateway.set_status(&name, TaskStatus::Active).await?;
                        Ok(SquadOutcome::Ok)
                    }
                    _ => unreachable!("daemon commands handled above"),
                }
            }
        }
    }
}

/// Collect a full `CreateTask` from the frontend's interview answers.
/// Every field is asked; the frontend supplies the values and Layer 1 validates
/// them, so this reproduces the same task from CLI and TUI given the same
/// answers.
///
/// Nothing is persisted here, and nothing reaches the store until every step —
/// including the workspace choice and the overlay loop — has been answered. An
/// interview abandoned partway (Ctrl-C, a dismissed dialog) propagates its
/// error out of this function, so a partial task can never be written.
fn collect_task_interview(
    frontend: &mut dyn SquadCommandFrontend,
    git_engine: &GitEngine,
) -> Result<CreateTask, CommandError> {
    let name = frontend.ask_task_name()?;
    let description = frontend.ask_task_description()?;
    let interval_raw = frontend.ask_task_interval()?;
    let interval_secs =
        crate::command::dispatch::parse_squad_interval(&["squad", "add"], &interval_raw)?;
    let workspace = collect_workspace_choice(frontend, git_engine)?;
    let overlays = collect_overlays(frontend)?;
    let agent = frontend.ask_task_agent()?;
    let model = frontend.ask_task_model()?;
    // The mount scope only distinguishes anything inside a git repository. A
    // default or non-repo custom workspace has one possible answer, so asking
    // would be a prompt with no alternative; the gateway overrides it with
    // `MountScope::Directory` in that case regardless.
    let mount_scope = match &workspace {
        TaskWorkspace::Default => MountScope::Directory,
        TaskWorkspace::Custom(_) => frontend.ask_task_mount_scope()?,
    };
    Ok(CreateTask {
        name,
        description,
        workspace,
        mount_scope,
        interval_secs,
        agent,
        model,
        overlays,
    })
}

/// The two-choice workspace step, plus the custom path's warn/keep loop.
///
/// A path that does not exist is a hard error (there is nothing to mount, and
/// an arbitrary user-entered path is never silently created — unlike the
/// default workspace, which awman owns). A path that exists but is not a git
/// root only warns, and the user chooses to keep it or name a different one. A
/// path that would mount a parent directory of the session's current directory
/// goes through the same confirmation every other awman mount-scope flow uses.
fn collect_workspace_choice(
    frontend: &mut dyn SquadCommandFrontend,
    git_engine: &GitEngine,
) -> Result<TaskWorkspace, CommandError> {
    if matches!(
        frontend.ask_task_workspace_choice()?,
        TaskWorkspaceChoice::DefaultTaskWorkspace
    ) {
        return Ok(TaskWorkspace::Default);
    }
    let current_dir = std::env::current_dir().ok();
    loop {
        let path = frontend.ask_task_repo()?;
        let canonical = match std::fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            // Not "warn and offer to keep": there is nothing at this path to
            // mount at all, so the task could never run.
            Err(error) => {
                return Err(CommandError::Other(format!(
                    "task workspace {} does not exist: {error}",
                    path.display()
                )));
            }
        };
        if !canonical.is_dir() {
            return Err(CommandError::Other(format!(
                "task workspace {} is not a directory",
                canonical.display()
            )));
        }
        if let Some(cwd) = current_dir.as_deref() {
            if workspace_is_parent_of(&canonical, cwd)
                && !frontend.confirm_parent_directory_workspace(&canonical, cwd)?
            {
                continue;
            }
        }
        // The repository detector is `GitEngine::resolve_root` — the same one
        // the gateway's workspace resolution and every run's session opening
        // use — so the interview's warning, the stored `MountScope`, and what
        // actually happens at launch can never disagree about what counts as a
        // repository root.
        let is_git_root =
            resolved_git_root(git_engine, &canonical).as_deref() == Some(canonical.as_path());
        if is_git_root || frontend.confirm_non_git_workspace(&canonical)? {
            return Ok(TaskWorkspace::Custom(canonical));
        }
        // Declined: loop back to the path prompt.
    }
}

/// Whether `workspace` strictly contains `current_dir` — i.e. mounting it would
/// widen the container's view to a parent of where the user is standing.
///
/// The one place this comparison is made, so the interview's in-loop prompt and
/// the scripted gate below cannot drift apart.
pub(crate) fn workspace_is_parent_of(workspace: &Path, current_dir: &Path) -> bool {
    let cwd = std::fs::canonicalize(current_dir).unwrap_or_else(|_| current_dir.to_path_buf());
    cwd != workspace && cwd.starts_with(workspace)
}

/// Apply the parent-directory mount policy to a scripted (`--workspace <path>`)
/// creation, which has no interview loop to ask inside.
///
/// Only a frontend that actually represents the user's own shell session runs
/// this: the daemon re-executes the very same `squad add` from its own working
/// directory, which has nothing to do with the caller's, so asking there would
/// compare the wrong two paths and (with a frontend that cannot prompt) refuse
/// a request the user already authorised on the client.
fn confirm_scripted_workspace_scope(
    frontend: &mut dyn SquadCommandFrontend,
    workspace: &TaskWorkspace,
    current_dir: Option<&Path>,
    non_interactive: bool,
) -> Result<(), CommandError> {
    if !frontend.is_local_user_session() {
        return Ok(());
    }
    let TaskWorkspace::Custom(path) = workspace else {
        return Ok(());
    };
    let Some(current_dir) = current_dir else {
        return Ok(());
    };
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    if !workspace_is_parent_of(&canonical, current_dir) {
        return Ok(());
    }
    // `-n` means never ask: the widening is refused rather than prompted for,
    // so a scripted run neither blocks on a prompt nor widens a mount silently.
    if !non_interactive && frontend.confirm_parent_directory_workspace(&canonical, current_dir)? {
        return Ok(());
    }
    Err(CommandError::Other(format!(
        "task workspace {} is a parent directory of {}; \
         re-run with a workspace inside it, or confirm the wider mount scope",
        canonical.display(),
        current_dir.display()
    )))
}

/// The git root enclosing `path`, canonicalised so it is comparable with the
/// canonical path the caller resolved. `None` when `path` is not in a
/// repository.
pub(crate) fn resolved_git_root(git_engine: &GitEngine, path: &Path) -> Option<PathBuf> {
    let root = git_engine.resolve_root(path).ok()?;
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// The optional, repeating overlay step. Loops until the frontend answers
/// `None` (a blank submission). Blank entries are skipped rather than stored,
/// and syntax is validated once, in Layer 1, before anything is persisted.
fn collect_overlays(frontend: &mut dyn SquadCommandFrontend) -> Result<Vec<String>, CommandError> {
    let mut overlays: Vec<String> = Vec::new();
    while let Some(spec) = frontend.ask_task_overlay(&overlays)? {
        let spec = spec.trim().to_string();
        if spec.is_empty() {
            break;
        }
        overlays.push(spec);
    }
    Ok(overlays)
}

#[cfg(test)]
mod workspace_choice_tests {
    use super::*;
    use std::collections::VecDeque;

    use crate::data::message::{UserMessage, UserMessageSink};

    struct WorkspaceFrontend {
        paths: VecDeque<PathBuf>,
        keep_non_git: VecDeque<bool>,
        non_git_prompts: usize,
    }

    impl WorkspaceFrontend {
        fn custom(paths: impl IntoIterator<Item = PathBuf>, keep_non_git: Vec<bool>) -> Self {
            Self {
                paths: paths.into_iter().collect(),
                keep_non_git: keep_non_git.into(),
                non_git_prompts: 0,
            }
        }
    }

    impl UserMessageSink for WorkspaceFrontend {
        fn write_message(&mut self, _message: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for WorkspaceFrontend {
        fn ask_task_workspace_choice(&mut self) -> Result<TaskWorkspaceChoice, CommandError> {
            Ok(TaskWorkspaceChoice::CustomFolderOrRepo)
        }

        fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
            self.paths.pop_front().ok_or_else(|| {
                CommandError::Other(
                    "test frontend was asked for an unexpected replacement workspace".into(),
                )
            })
        }

        fn confirm_non_git_workspace(&mut self, _path: &Path) -> Result<bool, CommandError> {
            self.non_git_prompts += 1;
            self.keep_non_git.pop_front().ok_or_else(|| {
                CommandError::Other(
                    "test frontend was asked for an unexpected non-git confirmation".into(),
                )
            })
        }
    }

    fn git_init(dir: &Path) {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git must be available to exercise repository detection");
        assert!(status.success(), "git init failed in {}", dir.display());
    }

    #[test]
    fn a_real_git_repository_root_is_accepted_without_a_non_git_warning() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let mut frontend = WorkspaceFrontend::custom([tmp.path().to_path_buf()], vec![]);

        let workspace = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap();
        assert_eq!(
            workspace,
            TaskWorkspace::Custom(tmp.path().canonicalize().unwrap())
        );
        assert_eq!(frontend.non_git_prompts, 0);
    }

    /// A bare `.git` directory with no repository inside it is what an
    /// ancestor-walking `.git`-exists probe would call a repository. The real
    /// detector (`GitEngine::resolve_root`, the same one the run path uses)
    /// rejects it, so creation must warn rather than silently capturing a
    /// worktree-isolated scope that would fail at launch.
    #[test]
    fn a_malformed_git_marker_is_not_treated_as_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let mut frontend = WorkspaceFrontend::custom([tmp.path().to_path_buf()], vec![true]);

        let workspace = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap();
        assert_eq!(
            workspace,
            TaskWorkspace::Custom(tmp.path().canonicalize().unwrap())
        );
        assert_eq!(
            frontend.non_git_prompts, 1,
            "a malformed .git marker must still raise the not-a-repository warning"
        );
    }

    #[test]
    fn non_git_custom_workspace_warns_and_loops_when_user_changes_their_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut frontend =
            WorkspaceFrontend::custom([first.clone(), second.clone()], vec![false, true]);

        let workspace = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap();
        assert_eq!(
            workspace,
            TaskWorkspace::Custom(second.canonicalize().unwrap())
        );
        assert_eq!(
            frontend.non_git_prompts, 2,
            "the declined warning must return to the path prompt"
        );
    }

    #[test]
    fn nonexistent_custom_workspace_is_rejected_without_a_keep_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let mut frontend = WorkspaceFrontend::custom([missing.clone()], vec![]);

        let error = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
        assert_eq!(frontend.non_git_prompts, 0);
    }
}

#[cfg(test)]
mod scripted_workspace_scope_tests {
    use super::*;

    use crate::data::message::{UserMessage, UserMessageSink};

    /// A frontend that answers like a user's own shell session: it is asked,
    /// and it says no.
    struct RefusingLocalFrontend {
        asked: usize,
    }

    impl UserMessageSink for RefusingLocalFrontend {
        fn write_message(&mut self, _message: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for RefusingLocalFrontend {
        fn is_local_user_session(&self) -> bool {
            true
        }
        fn confirm_parent_directory_workspace(
            &mut self,
            _path: &Path,
            _current_dir: &Path,
        ) -> Result<bool, CommandError> {
            self.asked += 1;
            Ok(false)
        }
    }

    /// The daemon's frontend: it cannot ask, and its working directory has
    /// nothing to do with the caller's, so it must not re-apply the policy.
    struct DaemonFrontend;

    impl UserMessageSink for DaemonFrontend {
        fn write_message(&mut self, _message: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for DaemonFrontend {}

    #[test]
    fn a_scripted_parent_directory_workspace_is_refused_when_the_user_declines() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        let mut frontend = RefusingLocalFrontend { asked: 0 };
        let error = confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Custom(parent.clone()),
            Some(&child),
            false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("parent directory"),
            "scripted creation must refuse an unconfirmed parent mount: {error}"
        );
        assert_eq!(frontend.asked, 1);
    }

    #[test]
    fn a_scripted_workspace_inside_the_current_directory_is_never_questioned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        let mut frontend = RefusingLocalFrontend { asked: 0 };
        confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Custom(child.clone()),
            Some(&child),
            false,
        )
        .unwrap();
        assert_eq!(frontend.asked, 0);
        confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Default,
            Some(&child),
            false,
        )
        .unwrap();
        assert_eq!(frontend.asked, 0);
    }

    /// `-n` means never ask. The widening must be refused outright rather than
    /// prompted for, so a scripted run neither blocks nor widens silently.
    #[test]
    fn non_interactive_refuses_a_parent_directory_workspace_without_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        let mut frontend = RefusingLocalFrontend { asked: 0 };
        let error = confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Custom(parent),
            Some(&child),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("parent directory"), "{error}");
        assert_eq!(
            frontend.asked, 0,
            "--non-interactive must refuse without putting a question to anyone"
        );
    }

    #[test]
    fn the_daemon_does_not_re_apply_the_clients_mount_scope_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        confirm_scripted_workspace_scope(
            &mut DaemonFrontend,
            &TaskWorkspace::Custom(parent),
            Some(&child),
            false,
        )
        .expect("the daemon must accept what the client already authorised");
    }
}

/// Remove a task's persistent directory when confirmed. Resolves the path
/// through the `validate_under_root`-guarded `SquadPaths::task_dir`, asks
/// the frontend (unless `-y`), and deletes only on a `true` answer. A missing
/// directory is not an error. Returns the directory actually removed, if any.
///
/// Removing the task is the *only* thing that may remove its durable workspace
/// (WI 0106 §6a) — nothing on the run path ever deletes it. What is removed
/// here is the whole `tasks/<name>/` tree, not just its `workspace/` leaf, so
/// the task's per-run log directories go with it rather than being orphaned.
fn remove_task_dir(
    frontend: &mut dyn SquadCommandFrontend,
    name: &str,
    yes: bool,
) -> Result<Option<PathBuf>, CommandError> {
    let paths = SquadPaths::from_process_env()?;
    // `task_dir` is the guarded `.../tasks/<name>/workspace`; its parent is the
    // task's whole tree. Deriving it this way keeps the single path guard.
    let workspace = paths.task_dir(name)?;
    let dir = workspace
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(workspace);
    // Deletion requires an explicit `true`: `-y`, or a frontend that confirms.
    // A declined prompt (or one no frontend can answer — the daemon's API
    // frontend, an aborted dialog) keeps the directory rather than failing the
    // remove, whose gateway delete has already succeeded.
    let confirmed = yes || matches!(frontend.ask_delete_task_dir(name, &dir), Ok(true));
    if !confirmed {
        return Ok(None);
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(Some(dir)),
        // A task with no persistent directory is a no-op, not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(crate::data::error::DataError::io(&dir, error).into()),
    }
}

fn daemon_outcome(value: SquadDaemonOutcome) -> Result<SquadOutcome, CommandError> {
    Ok(match value {
        SquadDaemonOutcome::Started {
            port,
            background,
            refreshed_key,
        } => SquadOutcome::Started {
            port,
            background,
            refreshed_key,
        },
        SquadDaemonOutcome::Stopped { stopped_pid } => SquadOutcome::Stopped { stopped_pid },
        SquadDaemonOutcome::Status(status) => SquadOutcome::Status(status),
        SquadDaemonOutcome::Logs { log_path } => SquadOutcome::Logs { log_path },
    })
}
