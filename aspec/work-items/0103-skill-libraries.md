# Work Item: Feature

Title: skill libraries
Issue: issuelink

## Summary:
- as an extension to the builtin global skills directory and skills overlay, awman now gains the ability to pull premade skills libraries from github that others have published such as superpowers, gstack, etc.

as an addition to `awman new skill`, a `--pull` flag should be added which accepts a github repo, e.g. `awman new skill --pull github.com/obra/superpowers` which will pull that repo into the global awman skills repo, and make the containing `skills` directory available for mounting via overlay

then, i can set `skill(superpowers)` on any agent/repo/workflow/etc. as per usual, and the skills contained within superpowers/skills folder will be added. individual skills can also be referenced via `skill(superpowers/brainstorming)` to get only a single skill from that library.

Running `awman new skill --pull superpowers` (or the full github slug again) will cause awman to fetch the latest version from github.

Two additional flags: `--subdir` allows a specific subdirectory within the cloned repo to be used instead of the default `skills/` (persist that choice with a `.awman.json` file added to the cloned repo or some similar method). --pull-all (instead of --pull <repo>) iterates over all of the skills libraries that have been pulled from git and sequentially pulls them all.

Skills libraries pulled from github should live in ~/.awman/skills/.library/{repo slug} whereas normally-created skills should continue to live at ~/.awman/skills/{name}

## User Stories

### User Story 1:
As a: user

I want to:
run `awman new skill --pull github.com/obra/superpowers` to clone a published skills library into my global awman skills store

So I can:
use a well-maintained, shared set of skills (like `superpowers` or `gstack`) without hand-copying files or maintaining my own fork.

### User Story 2:
As a: user

I want to:
reference the whole library with `skill(superpowers)` or a single skill inside it with `skill(superpowers/brainstorming)` in my overlay config (CLI flag, env var, repo/global config, or workflow step), exactly like any other named skill

So I can:
mount only what a given agent session actually needs, keeping the same overlay syntax and mental model I already use for hand-authored skills.

### User Story 3:
As a: user

I want to:
run `awman new skill --pull superpowers` (short name) or `awman new skill --pull-all` periodically

So I can:
refresh a previously-pulled library (or every library I've pulled) to the latest upstream version with a single command, without re-typing the full GitHub slug each time.

## Implementation Details:

### 1. Directory layout

- Hand-authored skills continue to live at `~/.awman/skills/<name>/SKILL.md` (unchanged).
- Pulled libraries live at `~/.awman/skills/.library/<slug>/`, where `<slug>` is the sanitized repo name (final path segment of the GitHub repo, e.g. `superpowers` for `github.com/obra/superpowers`), **not** `owner/repo`. This is what lets `skill(superpowers)` resolve without the user having to remember the owner.
- Each library directory is a full `git clone` of the upstream repo (so `.git/` is present for later `--pull`/`--pull-all` refresh), plus a persisted metadata file `~/.awman/skills/.library/<slug>/.awman.json`:
  ```json
  {
    "source": "https://github.com/obra/superpowers.git",
    "owner": "obra",
    "repo": "superpowers",
    "subdir": "skills"
  }
  ```
  `subdir` defaults to `"skills"` and is overridden by `--subdir`. This file is the single source of truth for "what did the user pull this from" and "which subdirectory inside it holds skills" — required for `--pull-all` (which has no other way to know the origin URL) and for repeat `--pull <slug>` calls that omit `--subdir`.
- `.library` is a dot-prefixed directory specifically so it is never picked up implicitly by `skill(*)` (see §4) or shown as a normal skill.

### 2. `SkillDirs` additions (`src/data/fs/skill_dirs.rs`)

Add library-path helpers alongside the existing `global_dir()` / `repo_dir()`:

```rust
pub const LIBRARY_SUBDIR: &str = ".library";

impl SkillDirs {
    /// `~/.awman/skills/.library/`
    pub fn library_root(&self) -> PathBuf {
        self.global_dir().join(LIBRARY_SUBDIR)
    }

    /// `~/.awman/skills/.library/<slug>/`
    pub fn library_dir(&self, slug: &str) -> PathBuf {
        self.library_root().join(slug)
    }

    /// List the slugs of all currently-pulled libraries (directories under
    /// `.library/` that contain a `.awman.json`). Skips and logs a
    /// `tracing::warn!` for any entry that is not a readable library.
    pub fn list_libraries(&self) -> Vec<String> { /* ... */ }
}
```

### 3. Library metadata (`src/data/fs/skill_library.rs`, new module)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillLibraryMeta {
    pub source: String,
    pub owner: String,
    pub repo: String,
    #[serde(default = "default_subdir")]
    pub subdir: String,
}

fn default_subdir() -> String { "skills".to_string() }

pub const LIBRARY_META_FILENAME: &str = ".awman.json";

pub fn read_library_meta(library_dir: &Path) -> Result<SkillLibraryMeta, DataError>;
pub fn write_library_meta(library_dir: &Path, meta: &SkillLibraryMeta) -> Result<(), DataError>;
```

Follows the existing `serde(rename_all)`-free, plain-field convention used by other small config structs in `src/data/config/`.

### 4. GitHub slug parsing (`src/data/fs/skill_library.rs`)

```rust
pub struct GithubSlug {
    pub owner: String,
    pub repo: String,   // sanitized, no trailing `.git`
}

pub fn parse_github_slug(input: &str) -> Result<GithubSlug, String>;
```

Accepts, in order of precedence:
- `https://github.com/<owner>/<repo>` / `http://github.com/<owner>/<repo>` (optionally with trailing `.git` or `/`)
- `github.com/<owner>/<repo>`
- `<owner>/<repo>` (bare short form)

A bare single-segment input (no `/`, e.g. `superpowers`) is **not** a `GithubSlug` — it is a re-pull-by-name request, resolved separately (see §5) against already-pulled libraries, not parsed as an owner/repo pair.

`repo` is sanitized: lowercased, `.git` suffix stripped, and validated against the same "alphanumeric, `-`, `_`" character set already used for skill/workflow name validation (WI 0064) so it is safe to use as a directory name and inside `skill()` overlay expressions.

Clone URL is always normalized to `https://github.com/<owner>/<repo>.git` regardless of input form (no SSH remotes — keeps the feature usable without the user having SSH keys configured, and avoids adding an `ssh()`-overlay dependency to a plain `git clone`).

### 5. Pull orchestration (`src/command/commands/skill_library.rs`, new module)

```rust
pub enum PullTarget {
    Slug(GithubSlug),       // fresh pull or explicit re-pull by full slug
    ExistingByName(String), // `--pull superpowers`: re-pull an already-pulled library by its short name
}

pub fn resolve_pull_target(input: &str) -> Result<PullTarget, String> {
    if input.contains('/') || input.contains("github.com") {
        parse_github_slug(input).map(PullTarget::Slug)
    } else {
        Ok(PullTarget::ExistingByName(input.to_string()))
    }
}

pub struct PullOutcome {
    pub slug: String,
    pub dir: PathBuf,
    pub subdir: String,
    pub skills_found: Vec<String>, // skill names discovered under dir/subdir
    pub was_update: bool,          // true = existing clone was refreshed
}

pub fn pull_library(
    git_engine: &GitEngine,
    skill_dirs: &SkillDirs,
    target: PullTarget,
    subdir_override: Option<&str>,
) -> Result<PullOutcome, CommandError>;

pub fn pull_all_libraries(
    git_engine: &GitEngine,
    skill_dirs: &SkillDirs,
) -> Vec<Result<PullOutcome, CommandError>>; // one entry per library, in slug-sorted order; never short-circuits
```

`pull_library` logic:
1. Resolve `target` to a concrete `(owner, repo, slug, clone_url)`:
   - `PullTarget::Slug(s)` → straightforward.
   - `PullTarget::ExistingByName(name)` → `dir = skill_dirs.library_dir(&name)`; if it doesn't exist or has no `.awman.json`, error: `"library '{name}' has not been pulled yet; use the full GitHub slug, e.g. --pull github.com/<owner>/{name}"`. Otherwise read the persisted `owner`/`repo`/`source` from `.awman.json`.
2. `dir = skill_dirs.library_dir(&slug)`.
3. If `dir` does not exist: `git_engine.clone_repo(&clone_url, None, &dir)`, then write `.awman.json` with `subdir = subdir_override.unwrap_or("skills")`. `was_update = false`.
4. If `dir` exists:
   - If it has no `.awman.json` or no `.git/`: error — `"{dir} exists but is not an awman-managed skill library; remove it manually before pulling '{slug}' here"`. Never delete or overwrite user content automatically.
   - If `.awman.json` exists but its `owner`/`repo` differ from the resolved target: error — `"library '{slug}' was already pulled from {existing_owner}/{existing_repo}; refusing to overwrite with {owner}/{repo}. Remove ~/.awman/skills/.library/{slug} first if you want to replace it."` (name collision between two different upstream repos sharing a final path segment).
   - Otherwise, refresh in place: add `GitEngine::pull_latest(dir: &Path) -> Result<(), EngineError>` (new method, §6) which does `git fetch origin` + `git reset --hard origin/HEAD` (equivalent to `git -C <dir> fetch && git -C <dir> reset --hard origin/HEAD`) so local edits some user made by hand inside a pulled library are intentionally discarded on refresh — this directory is described to the user as fetched/managed content, not a personal workspace. Update `subdir` in `.awman.json` **only** if `--subdir` was explicitly passed this invocation; otherwise preserve the previously persisted value. `was_update = true`.
5. Compute `effective_subdir` (the value now in `.awman.json`). If `dir.join(effective_subdir)` does not exist, error: `"subdirectory '{effective_subdir}' not found in {slug}; pass --subdir to point at the folder containing SKILL.md directories"`.
6. Enumerate immediate child directories of `dir.join(effective_subdir)` that contain a `SKILL.md` → `skills_found` (informational only; used for the success message, not validated further here).

### 6. `GitEngine::pull_latest` (`src/engine/git/mod.rs`)

```rust
pub fn pull_latest(&self, repo_dir: &Path) -> Result<(), EngineError> {
    // git -C <repo_dir> fetch origin
    // git -C <repo_dir> reset --hard origin/HEAD
}
```

Mirrors the existing `clone_repo` pattern (`Command::new("git")`, non-zero exit → `EngineError::Git(stderr)`). Add a `_logged` variant (`pull_latest_logged`) consistent with every other `GitEngine` method's `_logged` counterpart, writing progress via `UserMessageSink`.

### 7. `NewSkillFlags` extension (`src/command/commands/new.rs`)

```rust
#[derive(Debug, Clone)]
pub struct NewSkillFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub pull: Option<String>,   // NEW — repo slug or short library name
    pub pull_all: bool,         // NEW
    pub subdir: Option<String>, // NEW
}
```

In `NewCommand::run_with_frontend`, at the top of the `NewSubcommand::Skill(f)` arm, branch before any of the existing name/interview/global logic:

```rust
if f.pull_all {
    // ignore f.pull / f.subdir entirely (validated earlier at parse time — see §9)
    let results = pull_all_libraries(&self.engines.git_engine, &skill_dirs);
    // write one UserMessage per library (Info on success, Error on failure);
    // never early-return on an individual failure.
    // Build NewOutcome::Skill with path = None and report success/failure counts
    // via a new `libraries: Vec<PullLibraryOutcome>` field on NewSkillOutcome.
} else if let Some(target) = &f.pull {
    let outcome = pull_library(&self.engines.git_engine, &skill_dirs,
        resolve_pull_target(target)?, f.subdir.as_deref())?;
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "Pulled '{}' into {} ({} skill(s) found under {}/): {}",
            outcome.slug, outcome.dir.display(), outcome.skills_found.len(),
            outcome.subdir, outcome.skills_found.join(", ")
        ),
    });
    return Ok(NewOutcome::Skill(NewSkillOutcome {
        interview: false, global: f.global,
        path: Some(outcome.dir.display().to_string()),
        libraries: vec![],
    }));
} else {
    // existing name/interview/body flow, unchanged
}
```

`NewSkillOutcome` gains a `libraries: Vec<PullLibraryOutcome>` field (empty except for `--pull`/`--pull-all` runs) so API-frontend JSON output can report structured per-library results; `PullLibraryOutcome { slug: String, dir: String, updated: bool, skills_found: Vec<String>, error: Option<String> }`.

`--pull`/`--pull-all` never launch a container or touch `interview`/`global`/skill-name prompting — they are pure host-side git operations. No agent is invoked, so this path does not need `require_container_runtime()` and is unaffected by the "never execute an agent on the host" security constraint (`aspec/architecture/security.md`) — it's `git clone`/`git fetch`, not an agent invocation.

### 8. CLI wiring

**`src/command/dispatch/catalogue.rs`** — extend the `NEW_SKILL` `CommandSpec`'s `flags` array:

```rust
FlagSpec {
    long: "pull",
    short: None,
    help: "Pull (or refresh) a published skills library from GitHub, e.g. github.com/obra/superpowers.",
    kind: FlagKind::OptionalString,
    default: FlagDefault::None,
    frontends: FrontendVisibility::All,
    conflicts_with: &["pull-all", "interview", "global"],
    implies: &[],
    optional: true,
},
FlagSpec {
    long: "pull-all",
    short: None,
    help: "Refresh every previously-pulled skills library.",
    kind: FlagKind::Bool,
    default: FlagDefault::Bool(false),
    frontends: FrontendVisibility::All,
    conflicts_with: &["pull", "subdir", "interview", "global"],
    implies: &[],
    optional: true,
},
FlagSpec {
    long: "subdir",
    short: None,
    help: "Subdirectory inside the pulled repo containing skills (default: skills).",
    kind: FlagKind::OptionalString,
    default: FlagDefault::None,
    frontends: FrontendVisibility::All,
    conflicts_with: &["pull-all"],
    implies: &[],
    optional: true,
},
```

`conflicts_with` on `pull`/`pull-all` against `interview`/`global` reflects that a pull is a distinct mode from interactive/interview skill creation — reject the combination at parse time with the catalogue's existing conflict-checking machinery rather than deep inside `NewCommand`.

**`src/command/dispatch/mod.rs`** — extend the `["new", "skill"]` arm to read the three new flags (`flag_string` for `pull`/`subdir`, `flag_bool` for `pull-all`) and populate `NewSkillFlags`, following the exact pattern already used for `interview`/`global`.

**`src/frontend/cli/command_frontend.rs`** — no changes needed beyond what the catalogue-driven clap wiring already generates, provided the catalogue entry is correct (mirrors how `--global`/`--format` reach clap today for `new workflow`/`new skill`).

### 9. `skill(name)` overlay resolution (`src/engine/overlay/mod.rs::skill_overlays`)

Extend the per-name branch (currently `host_skills_dir.join(name)`, lines ~536-553) to try, in order:

1. **Plain skill** (existing behavior, unchanged priority): `host_skills_dir.join(name)` when `name` contains no `/`. If it exists, mount it exactly as today.
2. **Whole library**: if not found as a plain skill and `name` contains no `/`, try `host_skills_dir.join(".library").join(name)`. If present, read its `.awman.json` for `subdir` and mount `host_skills_dir/.library/<name>/<subdir>` at `{container_path}/<name>` (same container-path convention as today, so `skill(superpowers)` produces the same shape of mount as any other named skill — a directory of `<skill>/SKILL.md` entries — and Claude Code's existing subdirectory traversal namespaces them as `/superpowers:brainstorming` etc. with no agent-side changes required).
3. **Single skill inside a library**: if `name` contains exactly one `/`, split into `(library, skill)`. Look up `host_skills_dir/.library/<library>/<subdir>/<skill>/SKILL.md`; if present, mount just that directory at `{container_path}/<library>/<skill>` (preserves the library namespace even for a single-skill mount, so `skill(superpowers/brainstorming)` and `skill(superpowers)` never collide on container path if both are requested together — see edge cases).
4. If none of the above resolve, keep today's error, updated to mention both locations: `"named skill '{name}' not found in {host_skills_dir} or {host_skills_dir}/.library/"`.
5. `name` containing more than one `/` is a parse-time error (see §10), never reaches this function.

This function already receives `git_root: &Path` and `container_home_override`, unchanged; only the resolution branch inside the `else` arm (currently lines 536-554) changes. `include_all` (`skill(*)`) behavior is explicitly **not** changed by this work item — it continues to mount `host_skills_dir` as-is, and `.library/` deliberately stays out of its namespace requirement below (§10 covers why this needs no code change: dot-prefixed directories already sit outside every agent's SKILL.md traversal expectations, and mounting `.library/<slug>/<subdir>/<skill>/SKILL.md` under `skill(*)`'s single flat mount would nest skills two levels deeper than agents currently expect).

### 10. `TypedOverlay`/parser changes (`src/command/commands/mod.rs`)

`SkillSpec::Named(String)` already accepts arbitrary strings (no character validation today), so `skill(superpowers/brainstorming)` already parses successfully as `SkillSpec::Named("superpowers/brainstorming")`. Add one validation rule to `parse_single_typed_overlay`'s `"skill"` arm: reject a named skill argument containing more than one `/` with a descriptive error (`"skill(name) supports at most one '/' (library/skill); got '{args}'"`) — this is new input validation, not a behavior change, since today such a value would silently fail at mount time with a generic not-found error instead of a clear parse-time one.

## Edge Case Considerations:

- **Bare single-segment `--pull` value that has never been pulled**: `awman new skill --pull superpowers` when no `.library/superpowers/` exists yet → clear error directing the user to pass the full slug (`github.com/<owner>/superpowers`) once.
- **Repo-name collision**: two different GitHub owners publish a repo with the same final path segment (e.g. `alice/superpowers` and `bob/superpowers`). The second `--pull` errors rather than silently overwriting the first; the user must delete `~/.awman/skills/.library/superpowers` manually to replace it (never auto-delete per repo-wide git-safety guidance).
- **`.library/<slug>` exists but isn't an awman-managed clone** (no `.git/` or no `.awman.json` — e.g. a user manually created a directory there): error instructing manual removal; never silently `git init`/overwrite.
- **`--subdir` omitted on a re-pull**: preserve the previously persisted `subdir` from `.awman.json` rather than resetting to the `"skills"` default.
- **`--subdir` points at a path that doesn't exist in the repo** (typo, or repo restructured upstream): error naming the missing path, after the clone/fetch has already succeeded (so the user doesn't lose the clone on a subdir typo — they can re-run with a corrected `--subdir`).
- **Empty or skill-less subdir** (repo exists, subdir exists, but contains no `<name>/SKILL.md` directories): succeed with `skills_found = []` and an informational (not error) message — mirrors "empty skills directory" handling in WI 0075.
- **`--pull` and `--pull-all` combined, or `--pull`/`--pull-all` combined with `--interview`/`--global`/a positional skill name**: rejected at parse time via `conflicts_with` in the catalogue, before any git/network operation runs.
- **`--pull-all` with zero libraries pulled yet**: succeed with an informational "no skill libraries pulled yet" message, not an error.
- **`--pull-all` where one library's upstream repo is now unreachable/deleted/private**: that single library's `pull_latest` fails; `pull_all_libraries` records the error in that entry's `PullLibraryOutcome` and continues to the next library. The command's overall exit code is non-zero if any library failed, but every reachable library still gets refreshed (no short-circuit).
- **Local modifications inside a pulled library directory**: `pull_latest` uses `fetch` + `reset --hard`, which intentionally discards them — the library directory is documented as fetched/managed content the user should not hand-edit; hand-authored skills belong under `~/.awman/skills/<name>/`, not inside `.library/`.
- **`skill(superpowers)` when a plain skill named `superpowers` also exists** at `~/.awman/skills/superpowers/`: the plain skill wins (checked first) — a user's own local skill is never shadowed by a same-named pulled library. Document this precedence.
- **`skill(library/skill)` where `library` exists but `skill` doesn't**: distinct, actionable error naming both the library and the missing skill (not the generic "not found in {dir}" used for plain/whole-library lookups).
- **`skill(a/b/c)` (more than one `/`)**: rejected at parse time (§10), never reaches overlay resolution.
- **`skill(*)` and pulled libraries**: `.library/` is intentionally excluded from what `skill(*)` implicitly surfaces (see §9) — pulled libraries are opt-in per-library/per-skill only. Document this clearly since it is the one place behavior does *not* mirror "just another named skill."
- **Network/DNS failure during `git clone`/`git fetch`**: surfaced as `EngineError::Git` with the raw `git` stderr, same as every existing `GitEngine` clone/fetch failure — no special-casing needed.
- **Repo slug sanitization collides with an existing hand-authored skill directory name** (e.g. user already has `~/.awman/skills/lint/` and someone publishes `github.com/someone/lint`): this is fine and expected — `.library/lint/` and `skills/lint/` are different paths; §9's plain-skill-first precedence rule already covers the reference-resolution side.
- **Very large libraries** (many skills, large repo): no size cap is imposed by this work item; a full `git clone` is used as-is, matching how every other awman git operation works today.

## Test Considerations:

### Unit tests — slug parsing (`src/data/fs/skill_library.rs`)
- `parse_github_slug` accepts `https://github.com/obra/superpowers`, `https://github.com/obra/superpowers.git`, `github.com/obra/superpowers`, and `obra/superpowers`, all producing `owner = "obra"`, `repo = "superpowers"`.
- `parse_github_slug` rejects a bare single-segment input (no owner) with a descriptive error.
- `parse_github_slug` rejects a repo name containing invalid characters (spaces, path separators beyond `owner/repo`).
- `resolve_pull_target("superpowers")` → `PullTarget::ExistingByName`; `resolve_pull_target("obra/superpowers")` → `PullTarget::Slug`.

### Unit tests — library metadata (`src/data/fs/skill_library.rs`)
- `write_library_meta` then `read_library_meta` round-trips `source`/`owner`/`repo`/`subdir`.
- `read_library_meta` on a `.awman.json` missing the `subdir` key defaults to `"skills"`.
- `SkillDirs::list_libraries` returns only directories containing a valid `.awman.json`; skips (with a `tracing::warn!`, not an error) a directory under `.library/` that lacks one.

### Unit tests — `pull_library` (`src/command/commands/skill_library.rs`, using a local `file://` or temp bare repo as the git remote instead of real GitHub network calls)
- Fresh pull (`dir` absent): clones, writes `.awman.json` with `subdir = "skills"` by default, `was_update = false`.
- Fresh pull with `--subdir custom`: `.awman.json.subdir == "custom"`.
- Re-pull of an existing, matching-origin library: `pull_latest` invoked, `was_update = true`, persisted `subdir` unchanged when `--subdir` omitted.
- Re-pull with a new `--subdir`: persisted `subdir` updated.
- Re-pull where existing `.awman.json` owner/repo differs from the resolved target: returns a collision error; directory on disk is untouched.
- Pull into a `.library/<slug>` that exists without `.git`/`.awman.json`: returns an error; directory untouched.
- `--subdir` pointing at a path absent from the clone: returns an error after the clone has already landed on disk (verify the clone/files still exist post-error).
- `skills_found` correctly lists only immediate child directories of `dir/subdir` containing a `SKILL.md`.
- `PullTarget::ExistingByName` against a name with no prior pull returns a descriptive "not pulled yet" error.

### Unit tests — `pull_all_libraries`
- Zero libraries pulled: returns an empty vec (caller renders the "nothing pulled yet" message; not treated as an error case).
- Multiple libraries, one with an unreachable remote: the failing library's result is `Err`, all other libraries still return `Ok` — verify by asserting the successful libraries' `.awman.json` mtimes/content actually changed.
- Libraries are visited in a deterministic (slug-sorted) order.

### Unit tests — `GitEngine::pull_latest`
- Against a local temp git remote: after a commit is pushed to the remote and `pull_latest` runs, the working tree reflects the new commit.
- A dirty/locally-modified working tree is discarded (hard reset) after `pull_latest`.
- Non-existent `repo_dir` / not a git repo → `EngineError::Git`.

### Unit tests — parser (`src/command/commands/mod.rs`)
- `skill(superpowers/brainstorming)` parses to `TypedOverlay::Skill(SkillSpec::Named("superpowers/brainstorming"))`.
- `skill(a/b/c)` (two slashes) is rejected with a descriptive parse error.
- Existing `skill(*)` / `skill(name)` / error-message tests continue to pass unmodified.

### Unit tests — `OverlayEngine::skill_overlays` named resolution (`src/engine/overlay/mod.rs`)
- `skill(name)` resolves to a plain skill when both a plain skill and a same-named library exist (plain wins).
- `skill(name)` resolves to `.library/<name>/<persisted-subdir>` when no plain skill of that name exists, mounted at `{container_path}/<name>`.
- `skill(library/skill)` resolves to `.library/<library>/<persisted-subdir>/<skill>`, mounted at `{container_path}/<library>/<skill>`.
- `skill(library/skill)` where `library` exists but `skill` doesn't → descriptive error naming both.
- `skill(nonexistent)` where neither a plain skill nor a library of that name exists → error mentioning both search locations.
- `skill(*)` behavior is unchanged by a coexisting `.library/` directory (still mounts `host_skills_dir` as a single spec, `.library/` included as an ordinary subdirectory of that mount — assert the emitted `OverlaySpec` list is identical with and without a populated `.library/`).

### Integration tests
- `awman new skill --pull github.com/obra/superpowers` (against a local test git remote) creates `~/.awman/skills/.library/superpowers/` with a valid `.awman.json` and a populated `skills/` subdir.
- `awman new skill --pull superpowers` (short form, after the above) refreshes the existing clone without prompting for owner/repo again.
- `awman new skill --pull-all` with two previously-pulled libraries refreshes both and reports a per-library summary.
- `awman new skill --pull ... --subdir custom-skills` persists and honors the override on both the initial pull and subsequent overlay resolution.
- `--overlay "skill(superpowers)"` on `awman chat`/`awman exec prompt` produces the expected Docker `-v` mount for a pulled library, using the same assertion style as the existing `skill()` integration tests.
- `--overlay "skill(superpowers/brainstorming)"` produces a single-skill mount.
- `--pull` combined with `--interview` or `--global` is rejected before any container/git operation runs (verify no network call and no container launch occur).

### Parity tests (CLI ↔ TUI ↔ Headless/API)
- `--pull`/`--pull-all`/`--subdir` are available and behave identically from the CLI flag path and the API frontend's JSON-outcome path (`NewSkillOutcome.libraries`); explicitly out of scope for a dedicated TUI dialog in this work item (see Codebase Integration) but must not be silently swallowed if typed into the TUI command box — the shared dispatcher in `src/command/dispatch/mod.rs` already makes this automatic once the catalogue entry exists.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Reuse `GitEngine::clone_repo`/`clone_repo_logged` (`src/engine/git/mod.rs`) for the initial pull rather than reimplementing `git clone` invocation; add `pull_latest`/`pull_latest_logged` alongside them following the exact same `Command::new("git")` + stderr-on-failure pattern.
- `SkillDirs` (`src/data/fs/skill_dirs.rs`) is the single source of truth for skill-related paths; add `library_root`/`library_dir`/`list_libraries` there rather than hardcoding `.library` path segments elsewhere.
- New pure data/parsing logic (`GithubSlug`, `SkillLibraryMeta`, `parse_github_slug`) belongs in `src/data/fs/skill_library.rs` (data layer, no engine/command dependencies), matching the existing `src/data/fs/` module boundary (`skill_dirs.rs`, `overlay_paths.rs`, `context_dirs.rs`).
- New orchestration logic (`pull_library`, `pull_all_libraries`, `resolve_pull_target`) belongs in `src/command/commands/skill_library.rs`, called from `NewCommand` in `src/command/commands/new.rs` — do not inline git/network orchestration directly into `new.rs`'s already-large match arm.
- CLI surface changes go through the existing catalogue-driven flag system (`src/command/dispatch/catalogue.rs` + `src/command/dispatch/mod.rs`) — this repo does not use ad hoc `clap::Parser` derives per command; follow the `FlagSpec`/`conflicts_with` pattern already used for every other command's mutually-exclusive flags (e.g. `yolo`/`auto`/`plan` in the shared agent-run flag arrays).
- `--pull`/`--pull-all` are pure host-side git operations with no agent/container involvement — do not call `require_container_runtime()` or launch any container for these code paths, and do not add them to any agent-entrypoint or credential-resolution logic.
- Use `tracing::warn!`/`tracing::debug!` (never `eprintln!`) for non-fatal diagnostics (skipped malformed library dirs, empty subdirs), matching WI 0075's conventions.
- All new public structs derive `Debug`, `Clone`, `Serialize`/`Deserialize` where they cross the data layer, `PartialEq` where tested by equality, matching existing conventions.
- Name/slug validation reuses the same "alphanumeric, `-`, `_`" character-class check already established for skill/workflow names in WI 0064 rather than introducing a second regex/validator.
- No TUI dialog is introduced for `--pull`/`--pull-all`/`--subdir` in this work item — they are flag-driven, non-interactive operations (no prompts to collect), so they need no `NewCommandFrontend` additions or `Dialog::` state; the existing TUI command box already reaches them once the catalogue entry exists, per WI 0064's shared-dispatcher pattern.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
