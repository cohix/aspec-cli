//! End-to-end command-dispatch coverage for pulled skill libraries (WI 0103).
//!
//! The public `--pull github.com/<owner>/<repo>` form is deliberately kept in
//! these tests.  Git's `url.*.insteadOf` setting redirects those URLs to local
//! `file://` repositories, so neither cloning nor refreshing can reach GitHub.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awman::command::commands::collect_all_overlay_specs;
use awman::command::commands::new::{NewOutcome, NewSkillOutcome};
use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::{BuiltCommand, CommandOutcome, Dispatch, Engines};
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::fs::skill_library::{read_library_meta, LIBRARY_META_FILENAME};
use awman::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::options::OverlayPermission;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman::frontend::api::event_bus::EventBus;
use awman::frontend::cli::CliFrontend;

/// `AWMAN_CONFIG_HOME` and Git's process-wide config injection are shared by
/// every test in this target, so the local-remotes fixture must be serialized.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn local_home_and_remotes(home: &Path, remotes: &[(&str, &Path)]) -> Self {
        let keys = [
            "AWMAN_CONFIG_HOME",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_KEY_1",
            "GIT_CONFIG_VALUE_1",
        ];
        let saved = keys
            .into_iter()
            .map(|key| (key, std::env::var(key).ok()))
            .collect();

        std::env::set_var("AWMAN_CONFIG_HOME", home);
        for (index, (github_url, local_repo)) in remotes.iter().enumerate() {
            std::env::set_var(
                format!("GIT_CONFIG_KEY_{index}"),
                format!("url.file://{}.insteadOf", local_repo.display()),
            );
            std::env::set_var(format!("GIT_CONFIG_VALUE_{index}"), github_url);
        }
        // Last, and dropped first: these vars are process-global, so a `git`
        // spawned by any concurrently-running test inherits them mid-write.
        // While `GIT_CONFIG_COUNT` is unset the indexed keys are inert, so
        // publishing it only once they are all written means no such git ever
        // sees a count promising a key that is missing — which git rejects
        // outright rather than ignoring.
        std::env::set_var("GIT_CONFIG_COUNT", remotes.len().to_string());
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

struct LocalLibraryRemote {
    repo: tempfile::TempDir,
    github_url: String,
}

impl LocalLibraryRemote {
    fn new(owner: &str, repo: &str, skills_subdir: &str) -> Self {
        let worktree = tempfile::tempdir().expect("local git worktree");
        git(worktree.path(), &["init", "--quiet"]);
        git(
            worktree.path(),
            &["config", "user.email", "tests@example.invalid"],
        );
        git(
            worktree.path(),
            &["config", "user.name", "awman integration tests"],
        );
        write_skill(worktree.path(), skills_subdir, "brainstorming");
        git(worktree.path(), &["add", "."]);
        git(
            worktree.path(),
            &["commit", "--quiet", "-m", "initial skills"],
        );
        Self {
            repo: worktree,
            github_url: format!("https://github.com/{owner}/{repo}.git"),
        }
    }

    fn path(&self) -> &Path {
        self.repo.path()
    }

    fn update_with_skill(&self, skills_subdir: &str, skill: &str) {
        write_skill(self.repo.path(), skills_subdir, skill);
        git(self.repo.path(), &["add", "."]);
        git(
            self.repo.path(),
            &["commit", "--quiet", "-m", "refresh skills"],
        );
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run local git command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_skill(repo: &Path, subdir: &str, skill: &str) {
    let dir = repo.join(subdir).join(skill);
    std::fs::create_dir_all(&dir).expect("create local library skill directory");
    std::fs::write(dir.join("SKILL.md"), format!("# {skill}\n")).expect("write SKILL.md");
}

fn make_session(root: &Path) -> Session {
    let resolver = StaticGitRootResolver::new(root);
    Session::open(root.to_path_buf(), &resolver, SessionOpenOptions::default())
        .expect("open test session")
}

fn make_engines(home: &Path, root: &Path) -> Engines {
    let api_paths = ApiPaths::from_root(home.join("api"));
    api_paths.ensure_root().expect("create API paths");
    let auth_paths = AuthPathResolver::at_home(home);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths)),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(root)),
    }
}

fn run_cli(root: &Path, home: &Path, args: &[&str]) -> NewSkillOutcome {
    let matches = CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(std::iter::once("awman").chain(args.iter().copied()))
        .expect("CLI flags must parse");
    let frontend = CliFrontend::new(matches);
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(make_session(root))),
        make_engines(home, root),
    );
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    match runtime
        .block_on(dispatch.run_command(&["new", "skill"]))
        .expect("CLI new skill dispatch")
    {
        CommandOutcome::New(NewOutcome::Skill(outcome)) => outcome,
        other => panic!("expected new-skill outcome, got {other:?}"),
    }
}

fn run_api(root: &Path, home: &Path, args: &[&str]) -> NewSkillOutcome {
    let bus = EventBus::new(16);
    let argv = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    let frontend = ApiDispatchFrontend::new("new skill", &argv, bus.sender());
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(make_session(root))),
        make_engines(home, root),
    );
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    match runtime
        .block_on(dispatch.run_command(&["new", "skill"]))
        .expect("API new skill dispatch")
    {
        CommandOutcome::New(NewOutcome::Skill(outcome)) => outcome,
        other => panic!("expected new-skill outcome, got {other:?}"),
    }
}

fn assert_success_record(outcome: &NewSkillOutcome, slug: &str, updated: bool, skills: &[&str]) {
    assert_eq!(outcome.libraries.len(), 1, "outcome: {outcome:?}");
    let library = &outcome.libraries[0];
    assert_eq!(library.slug, slug);
    assert_eq!(library.updated, updated);
    assert_eq!(library.skills_found, skills);
    assert_eq!(library.error, None);
}

fn assert_cli_overlay_flag_reaches_command(
    root: &Path,
    home: &Path,
    argv: &[&str],
    command_path: &[&str],
    expected_overlay: &str,
) {
    let matches = CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(std::iter::once("awman").chain(argv.iter().copied()))
        .expect("agent command flags must parse");
    let dispatch = Dispatch::new(
        CliFrontend::new(matches),
        Arc::new(tokio::sync::RwLock::new(make_session(root))),
        make_engines(home, root),
    );
    match dispatch.build_command(command_path).expect("build command") {
        BuiltCommand::Chat(command) => {
            assert_eq!(command.flags().overlay, vec![expected_overlay.to_string()])
        }
        BuiltCommand::ExecPrompt(command) => {
            assert_eq!(command.flags().overlay, vec![expected_overlay.to_string()])
        }
        _ => panic!("expected chat or exec prompt command"),
    }
}

#[test]
fn real_git_cli_pull_creates_managed_library_and_short_name_refreshes_it() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let remote = LocalLibraryRemote::new("obra", "superpowers", "skills");
    let _env = EnvGuard::local_home_and_remotes(
        home.path(),
        &[(remote.github_url.as_str(), remote.path())],
    );

    let initial = run_cli(
        workdir.path(),
        home.path(),
        &["new", "skill", "--pull", "github.com/obra/superpowers"],
    );
    assert_success_record(&initial, "superpowers", false, &["brainstorming"]);

    let library = home.path().join("skills/.library/superpowers");
    assert!(
        library.join(".git").is_dir(),
        "must retain a full git clone"
    );
    assert!(library.join("skills/brainstorming/SKILL.md").is_file());
    let meta = read_library_meta(&library).expect("valid persisted .awman.json");
    assert_eq!(meta.source, remote.github_url);
    assert_eq!(meta.owner, "obra");
    assert_eq!(meta.repo, "superpowers");
    assert_eq!(meta.subdir, "skills");
    assert!(library.join(LIBRARY_META_FILENAME).is_file());

    remote.update_with_skill("skills", "refreshed");
    let refreshed = run_cli(
        workdir.path(),
        home.path(),
        &["new", "skill", "--pull", "superpowers"],
    );
    assert_success_record(
        &refreshed,
        "superpowers",
        true,
        &["brainstorming", "refreshed"],
    );
    assert!(library.join("skills/refreshed/SKILL.md").is_file());
}

#[test]
fn real_git_pull_all_refreshes_every_library_and_returns_per_library_summary() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let alpha = LocalLibraryRemote::new("owner", "alpha", "skills");
    let beta = LocalLibraryRemote::new("owner", "beta", "skills");
    let _env = EnvGuard::local_home_and_remotes(
        home.path(),
        &[
            (alpha.github_url.as_str(), alpha.path()),
            (beta.github_url.as_str(), beta.path()),
        ],
    );

    run_cli(
        workdir.path(),
        home.path(),
        &["new", "skill", "--pull", "owner/alpha"],
    );
    run_cli(
        workdir.path(),
        home.path(),
        &["new", "skill", "--pull", "owner/beta"],
    );
    alpha.update_with_skill("skills", "alpha-refresh");
    beta.update_with_skill("skills", "beta-refresh");

    let outcome = run_cli(workdir.path(), home.path(), &["new", "skill", "--pull-all"]);
    assert_eq!(outcome.path, None);
    assert_eq!(
        outcome.libraries.len(),
        2,
        "per-library outcome is the summary"
    );
    assert_eq!(
        outcome
            .libraries
            .iter()
            .map(|library| library.slug.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "pull-all summaries must be slug-sorted"
    );
    for library in &outcome.libraries {
        assert!(library.updated, "{library:?}");
        assert!(library.error.is_none(), "{library:?}");
    }
    assert!(home
        .path()
        .join("skills/.library/alpha/skills/alpha-refresh/SKILL.md")
        .is_file());
    assert!(home
        .path()
        .join("skills/.library/beta/skills/beta-refresh/SKILL.md")
        .is_file());
}

#[test]
fn real_git_subdir_is_persisted_and_library_and_single_skill_overlay_mounts_are_ro() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let remote = LocalLibraryRemote::new("owner", "custom-library", "custom-skills");
    let _env = EnvGuard::local_home_and_remotes(
        home.path(),
        &[(remote.github_url.as_str(), remote.path())],
    );

    let outcome = run_cli(
        workdir.path(),
        home.path(),
        &[
            "new",
            "skill",
            "--pull",
            "owner/custom-library",
            "--subdir",
            "custom-skills",
        ],
    );
    assert_success_record(&outcome, "custom-library", false, &["brainstorming"]);

    let library = home.path().join("skills/.library/custom-library");
    assert_eq!(
        read_library_meta(&library).unwrap().subdir,
        "custom-skills",
        "the override must survive beyond initial clone"
    );

    // Both public command paths pass `--overlay skill(...)` into the shared
    // typed-overlay collector before the runtime builds its Docker options.
    assert_cli_overlay_flag_reaches_command(
        workdir.path(),
        home.path(),
        &["chat", "--overlay", "skill(custom-library)"],
        &["chat"],
        "skill(custom-library)",
    );
    assert_cli_overlay_flag_reaches_command(
        workdir.path(),
        home.path(),
        &[
            "exec",
            "prompt",
            "--overlay",
            "skill(custom-library/brainstorming)",
            "use the library skill",
        ],
        &["exec", "prompt"],
        "skill(custom-library/brainstorming)",
    );

    let session = make_session(workdir.path());
    let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home.path()));
    for (overlay, expected_host, expected_container) in [
        (
            "skill(custom-library)",
            library.join("custom-skills"),
            PathBuf::from("/root/.claude/commands/custom-library"),
        ),
        (
            "skill(custom-library/brainstorming)",
            library.join("custom-skills/brainstorming"),
            PathBuf::from("/root/.claude/commands/custom-library/brainstorming"),
        ),
    ] {
        let collected = collect_all_overlay_specs(
            &session,
            awman::command::commands::parse_overlay_list(overlay).unwrap(),
            None,
            None,
        )
        .expect("chat/exec prompt overlay collection");
        let specs = engine
            .build_overlays(
                &session,
                &awman::engine::overlay::OverlayRequest {
                    named_skills: collected.named_skills,
                    agent: Some(awman::data::session::AgentName::new("claude").unwrap()),
                    ..Default::default()
                },
            )
            .expect("pulled skill overlay resolves");
        let spec = specs
            .iter()
            .find(|spec| spec.container_path == expected_container)
            .unwrap_or_else(|| panic!("{overlay} mount missing from {specs:?}"));
        assert_eq!(spec.host_path, expected_host.canonicalize().unwrap());
        assert_eq!(spec.container_path, expected_container);
        assert_eq!(spec.permission, OverlayPermission::ReadOnly);
        let docker_volume = format!(
            "{}:{}:{}",
            spec.host_path.display(),
            spec.container_path.display(),
            spec.permission.as_str()
        );
        let expected_volume = format!(
            "{}:{}:ro",
            expected_host.canonicalize().unwrap().display(),
            expected_container.display()
        );
        assert_eq!(docker_volume, expected_volume, "{overlay} Docker -v mount");
    }
}

#[test]
fn real_git_cli_api_and_tui_dispatch_keep_pull_subdir_and_pull_all_in_parity() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cli_home = tempfile::tempdir().unwrap();
    let api_home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let alpha = LocalLibraryRemote::new("owner", "alpha", "custom-skills");
    let beta = LocalLibraryRemote::new("owner", "beta", "skills");
    let _env = EnvGuard::local_home_and_remotes(
        cli_home.path(),
        &[
            (alpha.github_url.as_str(), alpha.path()),
            (beta.github_url.as_str(), beta.path()),
        ],
    );

    let cli_initial = run_cli(
        workdir.path(),
        cli_home.path(),
        &[
            "new",
            "skill",
            "--pull",
            "owner/alpha",
            "--subdir",
            "custom-skills",
        ],
    );
    // Point the process home at a second isolated store before the API run.
    std::env::set_var("AWMAN_CONFIG_HOME", api_home.path());
    let api_initial = run_api(
        workdir.path(),
        api_home.path(),
        &["--pull", "owner/alpha", "--subdir", "custom-skills"],
    );
    assert_success_record(&cli_initial, "alpha", false, &["brainstorming"]);
    assert_success_record(&api_initial, "alpha", false, &["brainstorming"]);
    let api_json = serde_json::to_value(&api_initial).expect("serialize API outcome");
    assert_eq!(api_json["libraries"][0]["slug"], "alpha");
    assert_eq!(api_json["libraries"][0]["updated"], false);
    assert_eq!(
        api_json["libraries"][0]["skills_found"],
        serde_json::json!(["brainstorming"])
    );
    assert_eq!(
        read_library_meta(&api_home.path().join("skills/.library/alpha"))
            .unwrap()
            .subdir,
        "custom-skills"
    );

    // Seed beta in both stores, then verify `--pull-all` has the same
    // per-library JSON shape through CLI and API dispatch.
    std::env::set_var("AWMAN_CONFIG_HOME", cli_home.path());
    run_cli(
        workdir.path(),
        cli_home.path(),
        &["new", "skill", "--pull", "owner/beta"],
    );
    std::env::set_var("AWMAN_CONFIG_HOME", api_home.path());
    run_api(workdir.path(), api_home.path(), &["--pull", "owner/beta"]);
    let api_all = run_api(workdir.path(), api_home.path(), &["--pull-all"]);
    std::env::set_var("AWMAN_CONFIG_HOME", cli_home.path());
    let cli_all = run_cli(
        workdir.path(),
        cli_home.path(),
        &["new", "skill", "--pull-all"],
    );
    let shape = |outcome: &NewSkillOutcome| {
        outcome
            .libraries
            .iter()
            .map(|library| {
                (
                    library.slug.clone(),
                    library.updated,
                    library.skills_found.clone(),
                    library.error.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&cli_all), shape(&api_all));

    let tui = Dispatch::<CliFrontend>::parse_command_box_input(
        "new skill --pull owner/alpha --subdir custom-skills",
    )
    .expect("TUI shared dispatcher must retain pull flags");
    assert_eq!(tui.path, vec!["new", "skill"]);
    assert!(
        matches!(tui.flags.get("pull"), Some(awman::command::dispatch::parsed_input::FlagValue::String(value)) if value == "owner/alpha")
    );
    assert!(
        matches!(tui.flags.get("subdir"), Some(awman::command::dispatch::parsed_input::FlagValue::String(value)) if value == "custom-skills")
    );
}

#[test]
fn api_and_tui_parsers_reject_pull_conflicts_before_dispatch() {
    let argv = vec![
        "--pull".to_string(),
        "owner/library".to_string(),
        "--global".to_string(),
    ];
    let api_error = CommandCatalogue::get()
        .parse_raw_args_with_profile(
            &["new", "skill"],
            &argv,
            awman::command::dispatch::catalogue::FrontendKind::Api,
        )
        .expect_err("API parser must reject --pull with --global");
    assert!(api_error.to_string().contains("conflicts"), "{api_error}");

    let tui_error = Dispatch::<CliFrontend>::parse_command_box_input(
        "new skill --pull owner/library --interview",
    )
    .expect_err("TUI shared dispatcher must reject --pull with --interview");
    assert!(tui_error.to_string().contains("conflicts"), "{tui_error}");
}
