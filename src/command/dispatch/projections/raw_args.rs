//! Raw-args projection: catalogue-driven parsing of a pre-tokenized argv.
//!
//! Frontends that receive already-tokenized argument vectors — chiefly the API
//! frontend, which takes an `args: [String]` array straight off an HTTP request
//! body — hand those strings to [`CommandCatalogue::parse_raw_args`] instead of
//! hand-rolling their own flag parser. Every decision (which flags exist, what
//! type each flag/positional carries, which positional is a greedy trailing
//! argument) is derived from the canonical [`CommandSpec`] data, so API-mode
//! parsing can never drift from the clap (CLI) or command-box (TUI) projections.
//!
//! This is the third projection alongside `clap.rs` (CLI) and `tui_hints.rs` /
//! `parsed_input.rs` (TUI). Unlike `parsed_input::parse`, the command path is
//! supplied by the caller rather than tokenized from a raw string, and values
//! are coerced to their declared types (u16/usize/path/enum) with a structured
//! [`CommandError`] on mismatch — mirroring how clap rejects the same input in
//! CLI mode.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::command::dispatch::catalogue::{
    ArgumentKind, CommandCatalogue, CommandSpec, FlagDefault, FlagKind, FlagSpec, FrontendKind,
};
use crate::command::error::CommandError;

/// A single parsed flag value, typed per the catalogue's declared [`FlagKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedFlag {
    Bool(bool),
    Str(String),
    Strings(Vec<String>),
    Path(PathBuf),
    U16(u16),
    Usize(usize),
}

/// Typed result of [`CommandCatalogue::parse_raw_args`]. Frontends read values
/// out of it through the accessor methods, which map 1:1 onto the
/// `CommandFrontend` trait surface.
#[derive(Debug, Clone, Default)]
pub struct ParsedArgs {
    flags: BTreeMap<String, ParsedFlag>,
    /// Positional arguments, keyed by their catalogue name. Stored as a vector
    /// so a greedy trailing argument keeps every token; single-valued
    /// positionals hold a one-element vector.
    arguments: BTreeMap<String, Vec<String>>,
    /// Names of positional arguments whose declared kind is a path. These are
    /// also reachable via [`ParsedArgs::flag_path`] so a command that reads a
    /// path positional through `flag_path` (e.g. `exec workflow`) still works.
    path_arguments: BTreeSet<String>,
}

impl ParsedArgs {
    pub fn flag_bool(&self, flag: &str) -> Option<bool> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn flag_string(&self, flag: &str) -> Option<String> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Str(s)) => Some(s.clone()),
            _ => None,
        }
    }

    pub fn flag_strings(&self, flag: &str) -> Vec<String> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Strings(v)) => v.clone(),
            _ => Vec::new(),
        }
    }

    pub fn flag_path(&self, flag: &str) -> Option<PathBuf> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Path(p)) => Some(p.clone()),
            // A path-typed positional is reachable via flag_path too, matching
            // the pre-refactor API behavior and Dispatch's flag_path-then-
            // argument fallback for `exec workflow` / `remote exec workflow`.
            _ if self.path_arguments.contains(flag) => {
                self.arguments.get(flag).map(|v| PathBuf::from(v.join(" ")))
            }
            _ => None,
        }
    }

    pub fn flag_enum(&self, flag: &str) -> Option<String> {
        // Enum values are stored as strings (as in the clap projection).
        self.flag_string(flag)
    }

    pub fn flag_u16(&self, flag: &str) -> Option<u16> {
        match self.flags.get(flag) {
            Some(ParsedFlag::U16(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn flag_usize(&self, flag: &str) -> Option<usize> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Usize(n)) => Some(*n),
            _ => None,
        }
    }

    /// A single positional argument. A greedy trailing argument collapses to
    /// its tokens joined by single spaces (preserving the historical
    /// `exec prompt` behavior spec-driven rather than special-cased).
    pub fn argument(&self, name: &str) -> Option<String> {
        self.arguments.get(name).map(|v| v.join(" "))
    }

    pub fn arguments(&self, name: &str) -> Vec<String> {
        self.arguments.get(name).cloned().unwrap_or_default()
    }
}

// ─── Per-frontend flag-default policy (Finding D) ────────────────────────────

/// One declarative per-frontend flag default. `forced` distinguishes a policy
/// that overrides any request-supplied value from one that only fills a default
/// when the caller omitted the flag.
struct FrontendFlagDefault {
    flag: &'static str,
    value: FlagDefault,
    /// `true`: override even an explicit request value (technical requirement).
    /// `false`: apply only when the flag was not supplied (overridable default).
    forced: bool,
}

/// API-profile flag defaults.
///
/// `non-interactive` is FORCED: HTTP workers have no TTY, and on Apple's
/// `container` CLI a PTY request with piped stdin fails with `ENOTTY`, so this
/// is a technical requirement, not a preference.
///
/// `yolo` is an overridable DEFAULT: it stays `true` for backward compatibility
/// (every historical API caller ran with auto-approval), but a request payload
/// that passes `--yolo`/`--plan` etc. can now change it. The policy lives here,
/// next to the profile, instead of being an unadvertised side effect buried in
/// the API frontend. See work item 0097, Finding D.
const API_FLAG_DEFAULTS: &[FrontendFlagDefault] = &[
    FrontendFlagDefault {
        flag: "non-interactive",
        value: FlagDefault::Bool(true),
        forced: true,
    },
    FrontendFlagDefault {
        flag: "yolo",
        value: FlagDefault::Bool(true),
        forced: false,
    },
];

fn frontend_flag_defaults(frontend: FrontendKind) -> &'static [FrontendFlagDefault] {
    match frontend {
        FrontendKind::Api => API_FLAG_DEFAULTS,
        // CLI and TUI resolve flag defaults through clap / the command layer;
        // no profile overrides apply, guaranteeing this mechanism cannot alter
        // their behavior.
        FrontendKind::Cli | FrontendKind::Tui => &[],
    }
}

fn flag_default_to_parsed(value: &FlagDefault) -> Option<ParsedFlag> {
    match value {
        FlagDefault::Bool(b) => Some(ParsedFlag::Bool(*b)),
        FlagDefault::Str(s) => Some(ParsedFlag::Str((*s).to_string())),
        FlagDefault::U16(n) => Some(ParsedFlag::U16(*n)),
        FlagDefault::None | FlagDefault::EmptyVec => None,
    }
}

/// Apply the frontend profile's flag defaults to an already-parsed argv. Only
/// flags the command actually declares are touched, so commands without the
/// flag are left untouched.
fn apply_frontend_defaults(parsed: &mut ParsedArgs, spec: &CommandSpec, frontend: FrontendKind) {
    for entry in frontend_flag_defaults(frontend) {
        let Some(flag_spec) = spec.find_flag(entry.flag) else {
            continue;
        };
        let Some(value) = flag_default_to_parsed(&entry.value) else {
            continue;
        };
        if entry.forced {
            parsed.flags.insert(entry.flag.to_string(), value);
            continue;
        }
        // Overridable default: fill only when absent. An explicitly supplied
        // value wins, and so does a mutually-exclusive flag the caller passed —
        // otherwise the fills-when-absent default would silently reintroduce a
        // conflict (e.g. `--plan` must genuinely suppress the `yolo` default
        // rather than leaving both set for dispatch to reject later).
        if parsed.flags.contains_key(entry.flag) {
            continue;
        }
        let conflict_present = flag_spec
            .conflicts_with
            .iter()
            .any(|other| parsed.flags.contains_key(*other));
        if conflict_present {
            continue;
        }
        parsed.flags.insert(entry.flag.to_string(), value);
    }
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

impl CommandCatalogue {
    /// Parse a pre-tokenized argv against the command at `path`.
    ///
    /// `path` is the resolved command path (e.g. `["exec", "prompt"]`); `args`
    /// is the flag/positional token vector with the command path already
    /// stripped. Flags, positionals, types, and the greedy trailing positional
    /// are all derived from the [`CommandSpec`] — there are no per-command
    /// special cases.
    pub fn parse_raw_args(
        &self,
        path: &[&str],
        args: &[String],
    ) -> Result<ParsedArgs, CommandError> {
        let spec = self
            .lookup_with_aliases(path)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        let canonical = self.canonical_path(path);
        parse_against_spec(spec, &canonical, args)
    }

    /// Like [`parse_raw_args`](Self::parse_raw_args) but additionally applies
    /// the declarative per-frontend flag defaults for `frontend` (Finding D).
    pub fn parse_raw_args_with_profile(
        &self,
        path: &[&str],
        args: &[String],
        frontend: FrontendKind,
    ) -> Result<ParsedArgs, CommandError> {
        let spec = self
            .lookup_with_aliases(path)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        let canonical = self.canonical_path(path);
        let mut parsed = parse_against_spec(spec, &canonical, args)?;
        apply_frontend_defaults(&mut parsed, spec, frontend);
        Ok(parsed)
    }
}

fn parse_against_spec(
    spec: &CommandSpec,
    path: &[&str],
    args: &[String],
) -> Result<ParsedArgs, CommandError> {
    let mut flags: BTreeMap<String, ParsedFlag> = BTreeMap::new();
    let mut positionals: Vec<String> = Vec::new();

    // Whether the command's last positional is a greedy trailing var-arg, and
    // how many fixed positionals precede it. Once positional collection reaches
    // the trailing argument, remaining tokens (including hyphen-prefixed ones)
    // are captured verbatim — matching clap's `trailing_var_arg` semantics.
    let trailing = spec
        .arguments
        .last()
        .is_some_and(|a| matches!(a.kind, ArgumentKind::TrailingVarArgs));
    let fixed_arg_count = spec
        .arguments
        .iter()
        .filter(|a| !matches!(a.kind, ArgumentKind::TrailingVarArgs))
        .count();

    let mut greedy = false;
    let mut after_double_dash = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if greedy || after_double_dash {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }

        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let flag_spec = spec
                .find_flag(name)
                .ok_or_else(|| CommandError::unknown_flag(path, name))?;
            i = apply_flag(flag_spec, inline, args, i, path, &mut flags)?;
        } else if let Some(short) = arg.strip_prefix('-') {
            // Only single-character short flags are supported (clap parity).
            if short.chars().count() != 1 {
                return Err(CommandError::unknown_flag(path, arg.clone()));
            }
            let ch = short.chars().next().unwrap();
            let flag_spec = spec
                .flags
                .iter()
                .find(|f| f.short == Some(ch))
                .ok_or_else(|| CommandError::unknown_flag(path, format!("-{ch}")))?;
            i = apply_flag(flag_spec, None, args, i, path, &mut flags)?;
        } else {
            positionals.push(arg.clone());
            // Begin greedy capture once we have started filling a trailing
            // var-arg (i.e. once positionals exceed the fixed leading ones).
            if trailing && positionals.len() > fixed_arg_count {
                greedy = true;
            }
            i += 1;
        }
    }

    // The clap projection enforces `FlagSpec::conflicts_with` automatically.
    // Raw/API parsing has no clap layer, so enforce the same declarative rule
    // before dispatch can observe a contradictory flag set.
    for flag in spec.flags {
        if !flags.contains_key(flag.long) {
            continue;
        }
        if let Some(conflicting) = flag
            .conflicts_with
            .iter()
            .find(|conflicting| flags.contains_key::<str>(*conflicting))
        {
            return Err(CommandError::InvalidFlagValue {
                command: path.iter().map(|segment| (*segment).to_string()).collect(),
                flag: flag.long.to_string(),
                reason: format!("--{} conflicts with --{}", flag.long, conflicting),
            });
        }
    }

    // Map collected positionals onto the declared arguments in order.
    let mut arguments: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut path_arguments: BTreeSet<String> = BTreeSet::new();
    let mut pos_idx = 0;
    for arg_spec in spec.arguments {
        match arg_spec.kind {
            ArgumentKind::TrailingVarArgs => {
                let collected: Vec<String> = positionals[pos_idx..].to_vec();
                if !collected.is_empty() {
                    arguments.insert(arg_spec.name.to_string(), collected);
                }
                pos_idx = positionals.len();
            }
            ArgumentKind::Path | ArgumentKind::OptionalPath => {
                if let Some(v) = positionals.get(pos_idx) {
                    arguments.insert(arg_spec.name.to_string(), vec![v.clone()]);
                    path_arguments.insert(arg_spec.name.to_string());
                    pos_idx += 1;
                }
            }
            ArgumentKind::String | ArgumentKind::OptionalString => {
                if let Some(v) = positionals.get(pos_idx) {
                    arguments.insert(arg_spec.name.to_string(), vec![v.clone()]);
                    pos_idx += 1;
                }
            }
        }
    }

    // Positionals the command never declared are a usage error, exactly as
    // clap treats them on the CLI. Silently dropping them let a command like
    // `new skill --pull <slug> <name>` — which declares no positional at all —
    // look accepted through the API and TUI while the CLI rejected it.
    if let Some(extra) = positionals.get(pos_idx) {
        return Err(CommandError::unexpected_argument(path, extra.clone()));
    }

    Ok(ParsedArgs {
        flags,
        arguments,
        path_arguments,
    })
}

/// Apply a single flag to the flag map, consuming a value from `args` when the
/// flag's kind requires one. Returns the index of the next unconsumed token.
fn apply_flag(
    flag_spec: &FlagSpec,
    inline: Option<String>,
    args: &[String],
    i: usize,
    path: &[&str],
    flags: &mut BTreeMap<String, ParsedFlag>,
) -> Result<usize, CommandError> {
    let key = flag_spec.long.to_string();
    match flag_spec.kind {
        FlagKind::Bool => {
            // clap `SetTrue`: presence implies true and no following value is
            // consumed. An inline `--flag=false` is honored for completeness.
            let value = !matches!(inline.as_deref(), Some("false"));
            flags.insert(key, ParsedFlag::Bool(value));
            Ok(i + 1)
        }
        FlagKind::String | FlagKind::OptionalString => {
            let (val, next) = read_value(inline, args, i, path, flag_spec.long)?;
            flags.insert(key, ParsedFlag::Str(val));
            Ok(next)
        }
        FlagKind::Enum(allowed) => {
            let (val, next) = read_value(inline, args, i, path, flag_spec.long)?;
            if !allowed.contains(&val.as_str()) {
                return Err(CommandError::InvalidFlagValue {
                    command: path.iter().map(|s| s.to_string()).collect(),
                    flag: flag_spec.long.to_string(),
                    reason: format!("'{val}' is not one of {allowed:?}"),
                });
            }
            flags.insert(key, ParsedFlag::Str(val));
            Ok(next)
        }
        FlagKind::VecString => {
            let (val, next) = read_value(inline, args, i, path, flag_spec.long)?;
            match flags.get_mut(&key) {
                Some(ParsedFlag::Strings(items)) => items.push(val),
                _ => {
                    flags.insert(key, ParsedFlag::Strings(vec![val]));
                }
            }
            Ok(next)
        }
        FlagKind::Path | FlagKind::OptionalPath => {
            let (val, next) = read_value(inline, args, i, path, flag_spec.long)?;
            flags.insert(key, ParsedFlag::Path(PathBuf::from(val)));
            Ok(next)
        }
        FlagKind::U16 => {
            let (val, next) = read_value(inline, args, i, path, flag_spec.long)?;
            let n = val
                .parse::<u16>()
                .map_err(|_| CommandError::InvalidFlagValue {
                    command: path.iter().map(|s| s.to_string()).collect(),
                    flag: flag_spec.long.to_string(),
                    reason: format!("'{val}' is not a valid integer (0..=65535)"),
                })?;
            flags.insert(key, ParsedFlag::U16(n));
            Ok(next)
        }
        FlagKind::UsizeAtLeastOne => {
            let (val, next) = read_value(inline, args, i, path, flag_spec.long)?;
            let n = val
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or_else(|| CommandError::InvalidFlagValue {
                    command: path.iter().map(|s| s.to_string()).collect(),
                    flag: flag_spec.long.to_string(),
                    reason: format!("'{val}' is not a positive integer (>= 1)"),
                })?;
            flags.insert(key, ParsedFlag::Usize(n));
            Ok(next)
        }
    }
}

/// Read a value for a value-taking flag: the inline `--flag=value` form if
/// present, otherwise the following token. A following token that looks like a
/// flag (or a missing token) is a structured error, matching clap's refusal to
/// consume a `-`-prefixed token as a value by default.
fn read_value(
    inline: Option<String>,
    args: &[String],
    i: usize,
    path: &[&str],
    flag: &str,
) -> Result<(String, usize), CommandError> {
    if let Some(v) = inline {
        return Ok((v, i + 1));
    }
    match args.get(i + 1) {
        Some(v) if !v.starts_with('-') => Ok((v.clone(), i + 2)),
        _ => Err(CommandError::InvalidFlagValue {
            command: path.iter().map(|s| s.to_string()).collect(),
            flag: flag.to_string(),
            reason: "flag requires a value".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> &'static CommandCatalogue {
        CommandCatalogue::get()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exec_prompt_joins_trailing_positionals() {
        let p = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["hello", "brave", "world"]))
            .unwrap();
        assert_eq!(p.argument("prompt").as_deref(), Some("hello brave world"));
        assert_eq!(p.arguments("prompt"), vec!["hello", "brave", "world"]);
    }

    #[test]
    fn exec_workflow_path_positional_reachable_via_flag_path_and_argument() {
        let p = cat()
            .parse_raw_args(&["exec", "workflow"], &argv(&["build.toml"]))
            .unwrap();
        assert_eq!(p.argument("workflow").as_deref(), Some("build.toml"));
        assert_eq!(p.flag_path("workflow"), Some(PathBuf::from("build.toml")));
    }

    /// Clap rejects positionals a command never declared; the raw-args
    /// projection must agree (WI-0097 parity) rather than silently dropping
    /// them. `exec workflow` declares exactly one positional.
    #[test]
    fn extra_positional_beyond_declared_arguments_is_rejected() {
        let err = cat()
            .parse_raw_args(&["exec", "workflow"], &argv(&["build.toml", "stray"]))
            .expect_err("an undeclared extra positional must be a usage error");
        match err {
            CommandError::UnexpectedArgument { command, argument } => {
                assert_eq!(command, vec!["exec", "workflow"]);
                assert_eq!(argument, "stray", "the error must name the stray token");
            }
            other => panic!("expected UnexpectedArgument, got {other:?}"),
        }
    }

    /// `new skill` declares no positional at all, so a stray skill name next to
    /// `--pull` must be rejected before any git work can start (WI-0103).
    #[test]
    fn new_skill_rejects_a_positional_name_alongside_pull() {
        let err = cat()
            .parse_raw_args(
                &["new", "skill"],
                &argv(&["--pull", "owner/library", "accidental-name"]),
            )
            .expect_err("new skill takes no positional argument");
        assert!(
            matches!(
                &err,
                CommandError::UnexpectedArgument { argument, .. } if argument == "accidental-name"
            ),
            "expected UnexpectedArgument naming the stray name, got {err:?}"
        );
    }

    #[test]
    fn flag_equals_and_space_forms_are_equivalent() {
        let a = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--agent", "claude"]))
            .unwrap();
        let b = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--agent=claude"]))
            .unwrap();
        assert_eq!(a.flag_string("agent").as_deref(), Some("claude"));
        assert_eq!(b.flag_string("agent").as_deref(), Some("claude"));
    }

    #[test]
    fn bool_flag_is_set_true_by_presence() {
        let p = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--plan"]))
            .unwrap();
        assert_eq!(p.flag_bool("plan"), Some(true));
    }

    #[test]
    fn repeated_vec_flag_accumulates() {
        let p = cat()
            .parse_raw_args(
                &["exec", "prompt"],
                &argv(&["--overlay", "/a", "--overlay", "/b"]),
            )
            .unwrap();
        assert_eq!(p.flag_strings("overlay"), vec!["/a", "/b"]);
    }

    #[test]
    fn double_dash_forces_positionals() {
        let p = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--", "--not-a-flag"]))
            .unwrap();
        assert_eq!(p.argument("prompt").as_deref(), Some("--not-a-flag"));
    }

    #[test]
    fn unknown_flag_is_structured_error() {
        let err = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--bogus"]))
            .unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    #[test]
    fn u16_type_mismatch_is_structured_error() {
        let err = cat()
            .parse_raw_args(&["api", "start"], &argv(&["--port", "not-a-number"]))
            .unwrap_err();
        assert!(matches!(err, CommandError::InvalidFlagValue { .. }));
    }

    #[test]
    fn u16_flag_coerces_value() {
        let p = cat()
            .parse_raw_args(&["api", "start"], &argv(&["--port", "9876"]))
            .unwrap();
        assert_eq!(p.flag_u16("port"), Some(9876));
    }

    #[test]
    fn bad_enum_value_is_structured_error() {
        let err = cat()
            .parse_raw_args(
                &["remote", "session", "start"],
                &argv(&["--type", "banana"]),
            )
            .unwrap_err();
        assert!(matches!(err, CommandError::InvalidFlagValue { .. }));
    }

    #[test]
    fn api_profile_forces_non_interactive_and_defaults_yolo() {
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["hi"]), FrontendKind::Api)
            .unwrap();
        assert_eq!(p.flag_bool("non-interactive"), Some(true));
        assert_eq!(p.flag_bool("yolo"), Some(true));
    }

    #[test]
    fn api_profile_yolo_default_is_overridable_by_plan() {
        // A request that opts into --plan (mutually exclusive with yolo in the
        // catalogue) must genuinely override the overridable yolo default: the
        // fills-when-absent default is suppressed because a conflicting flag is
        // present, so yolo stays unset and dispatch sees a coherent request.
        // non-interactive stays forced regardless.
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["--plan"]), FrontendKind::Api)
            .unwrap();
        assert_eq!(p.flag_bool("plan"), Some(true));
        assert_eq!(p.flag_bool("non-interactive"), Some(true));
        // yolo default suppressed by the conflicting --plan (real override).
        assert_eq!(p.flag_bool("yolo"), None);
    }

    #[test]
    fn cli_and_tui_profiles_apply_no_overrides() {
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["hi"]), FrontendKind::Cli)
            .unwrap();
        // No profile override: non-interactive/yolo are absent unless supplied.
        assert_eq!(p.flag_bool("non-interactive"), None);
        assert_eq!(p.flag_bool("yolo"), None);
    }

    #[test]
    fn unknown_command_path_is_structured_error() {
        let err = cat().parse_raw_args(&["nope"], &argv(&[])).unwrap_err();
        assert!(matches!(err, CommandError::UnknownCommand { .. }));
    }

    #[test]
    fn config_set_maps_two_positionals() {
        let p = cat()
            .parse_raw_args(&["config", "set"], &argv(&["agent", "claude"]))
            .unwrap();
        assert_eq!(p.argument("field").as_deref(), Some("agent"));
        assert_eq!(p.argument("value").as_deref(), Some("claude"));
    }

    #[test]
    fn explicit_yolo_value_is_kept_under_api_profile() {
        // yolo is an overridable default, so if the request already set it the
        // profile must not clobber it.
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["--yolo"]), FrontendKind::Api)
            .unwrap();
        assert_eq!(p.flag_bool("yolo"), Some(true));
    }
}
