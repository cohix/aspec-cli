//! Parsed TUI command-box input.
//!
//! The TUI submits a raw user string; Dispatch tokenizes it against the
//! catalogue and returns a typed [`ParsedCommandBoxInput`] the TUI feeds back
//! through a `TuiCommandFrontend`.

use std::collections::BTreeMap;

use crate::command::dispatch::catalogue::{ArgumentKind, CommandCatalogue, CommandSpec, FlagKind};
use crate::command::error::CommandError;

/// Result of `parse_command_box_input`. `path` is the resolved canonical
/// command path; `flags` and `arguments` are typed string maps the TUI hands
/// back to Dispatch via a `CommandFrontend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommandBoxInput {
    pub path: Vec<String>,
    pub flags: BTreeMap<String, FlagValue>,
    pub arguments: BTreeMap<String, ArgValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    Bool(bool),
    String(String),
    Strings(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValue {
    Single(String),
    Multi(Vec<String>),
}

/// Tokenize `raw` against the catalogue.
pub fn parse(
    raw: &str,
    catalogue: &CommandCatalogue,
) -> Result<ParsedCommandBoxInput, CommandError> {
    let tokens = shell_words::split(raw)
        .map_err(|e| CommandError::CommandBoxParse(format!("tokenize failed: {e}")))?;
    if tokens.is_empty() {
        return Err(CommandError::CommandBoxParse("empty input".into()));
    }

    // Walk the catalogue resolving subcommands, collecting flags and
    // positional args along the way.
    let mut current: &CommandSpec = catalogue.root();
    let mut path: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.starts_with('-') || tok == "--" {
            break;
        }
        match current.find_subcommand(tok) {
            Some(sub) => {
                path.push(sub.name.to_string());
                current = sub;
                idx += 1;
            }
            None => break,
        }
    }
    if path.is_empty() {
        return Err(CommandError::unknown_command(&[tokens[0].as_str()]));
    }

    let mut flags: BTreeMap<String, FlagValue> = BTreeMap::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut consume_var_args_remaining = false;

    while idx < tokens.len() {
        let tok = &tokens[idx];
        if consume_var_args_remaining {
            positionals.push(tok.clone());
            idx += 1;
            continue;
        }
        if tok == "--" {
            // Trailing var-args boundary marker.
            consume_var_args_remaining = true;
            idx += 1;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("--") {
            // Long flag: --name or --name=value
            let (name, inline_value) = match rest.find('=') {
                Some(eq) => (&rest[..eq], Some(rest[eq + 1..].to_string())),
                None => (rest, None),
            };
            let had_inline = inline_value.is_some();
            let flag_spec = current.find_flag(name).ok_or_else(|| {
                let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                CommandError::unknown_flag(&path_strs, name)
            })?;
            // Helper closure to read a value: prefer inline; otherwise advance idx.
            let mut read_value =
                |inline: Option<String>, msg: &str| -> Result<String, CommandError> {
                    if let Some(v) = inline {
                        idx += 1;
                        Ok(v)
                    } else {
                        idx += 1;
                        let v = tokens
                            .get(idx)
                            .cloned()
                            .ok_or_else(|| CommandError::CommandBoxParse(msg.to_string()))?;
                        idx += 1;
                        Ok(v)
                    }
                };
            let _ = had_inline;
            match flag_spec.kind {
                FlagKind::Bool => {
                    flags.insert(name.to_string(), FlagValue::Bool(true));
                    idx += 1;
                }
                FlagKind::String
                | FlagKind::OptionalString
                | FlagKind::Path
                | FlagKind::OptionalPath
                | FlagKind::Enum(_) => {
                    let value = read_value(inline_value, &format!("flag --{name} needs a value"))?;
                    flags.insert(name.to_string(), FlagValue::String(value));
                }
                FlagKind::U16 | FlagKind::UsizeAtLeastOne => {
                    let raw = read_value(inline_value, &format!("flag --{name} needs a number"))?;
                    flags.insert(name.to_string(), FlagValue::String(raw));
                }
                FlagKind::VecString => {
                    let value = read_value(inline_value, &format!("flag --{name} needs a value"))?;
                    flags
                        .entry(name.to_string())
                        .and_modify(|v| match v {
                            FlagValue::Strings(items) => items.push(value.clone()),
                            other => *other = FlagValue::Strings(vec![value.clone()]),
                        })
                        .or_insert_with(|| FlagValue::Strings(vec![value]));
                }
            }
        } else if let Some(short_run) = tok.strip_prefix('-') {
            // Treat short flags one at a time. Only single-char shorts are
            // supported.
            if short_run.len() != 1 {
                return Err(CommandError::CommandBoxParse(format!(
                    "short-flag bundle '-{short_run}' is not supported by the command box"
                )));
            }
            let ch = short_run.chars().next().unwrap();
            let flag_spec = current
                .flags
                .iter()
                .find(|f| f.short == Some(ch))
                .ok_or_else(|| {
                    let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                    CommandError::unknown_flag(&path_strs, format!("-{ch}"))
                })?;
            match flag_spec.kind {
                FlagKind::Bool => {
                    flags.insert(flag_spec.long.to_string(), FlagValue::Bool(true));
                    idx += 1;
                }
                _ => {
                    idx += 1;
                    let value = tokens.get(idx).cloned().ok_or_else(|| {
                        CommandError::CommandBoxParse(format!("-{ch} needs a value"))
                    })?;
                    idx += 1;
                    flags.insert(flag_spec.long.to_string(), FlagValue::String(value));
                }
            }
        } else {
            positionals.push(tok.clone());
            idx += 1;
        }
    }

    // Keep the TUI command box in parity with the clap and API projections:
    // all frontends must reject mutually-exclusive catalogue flags before a
    // command is built or any host-side operation can run.
    for flag in current.flags {
        if !flags.contains_key(flag.long) {
            continue;
        }
        if let Some(conflicting) = flag
            .conflicts_with
            .iter()
            .find(|conflicting| flags.contains_key::<str>(*conflicting))
        {
            let path_strs: Vec<&str> = path.iter().map(|segment| segment.as_str()).collect();
            return Err(CommandError::InvalidFlagValue {
                command: path_strs
                    .iter()
                    .map(|segment| (*segment).to_string())
                    .collect(),
                flag: flag.long.to_string(),
                reason: format!("--{} conflicts with --{}", flag.long, conflicting),
            });
        }
    }

    // Map positional tokens onto declared arguments.
    let mut arguments: BTreeMap<String, ArgValue> = BTreeMap::new();
    let mut pos_idx = 0;
    let mut last_was_var = false;
    for arg in current.arguments {
        match arg.kind {
            ArgumentKind::TrailingVarArgs => {
                let collected: Vec<String> = positionals[pos_idx..].to_vec();
                arguments.insert(arg.name.to_string(), ArgValue::Multi(collected));
                pos_idx = positionals.len();
                last_was_var = true;
            }
            _ => {
                if let Some(v) = positionals.get(pos_idx) {
                    arguments.insert(arg.name.to_string(), ArgValue::Single(v.clone()));
                    pos_idx += 1;
                } else if !arg.optional {
                    let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                    return Err(CommandError::missing_required_argument(
                        &path_strs, arg.name,
                    ));
                }
            }
        }
    }
    let _ = last_was_var;

    // Keep parity with clap and the API projection: a positional the command
    // never declared is a usage error, not a token to drop on the floor.
    if let Some(extra) = positionals.get(pos_idx) {
        let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
        return Err(CommandError::unexpected_argument(&path_strs, extra.clone()));
    }

    Ok(ParsedCommandBoxInput {
        path,
        flags,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exec_workflow_with_path_and_yolo() {
        let cat = CommandCatalogue::get();
        let parsed = parse("exec workflow my-workflow.toml --yolo", cat).unwrap();
        assert_eq!(parsed.path, vec!["exec", "workflow"]);
        assert!(matches!(
            parsed.flags.get("yolo"),
            Some(FlagValue::Bool(true))
        ));
        assert!(matches!(
            parsed.arguments.get("workflow"),
            Some(ArgValue::Single(s)) if s == "my-workflow.toml"
        ));
    }

    #[test]
    fn parse_remote_exec_workflow_with_argument() {
        let cat = CommandCatalogue::get();
        let parsed = parse("remote exec workflow my-workflow.toml", cat).unwrap();
        assert_eq!(parsed.path, vec!["remote", "exec", "workflow"]);
        assert!(matches!(
            parsed.arguments.get("workflow"),
            Some(ArgValue::Single(s)) if s == "my-workflow.toml"
        ));
    }

    #[test]
    fn parse_remote_exec_prompt_with_argument() {
        let cat = CommandCatalogue::get();
        let parsed = parse(r#"remote exec prompt "hello world""#, cat).unwrap();
        assert_eq!(parsed.path, vec!["remote", "exec", "prompt"]);
        // `prompt` is a greedy trailing positional (TrailingVarArgs), so a
        // single quoted token collects into a one-element Multi.
        assert!(matches!(
            parsed.arguments.get("prompt"),
            Some(ArgValue::Multi(v)) if v == &["hello world".to_string()]
        ));
    }

    #[test]
    fn parse_unknown_command_errors() {
        let cat = CommandCatalogue::get();
        let err = parse("not-a-command", cat).unwrap_err();
        assert!(matches!(err, CommandError::UnknownCommand { .. }));
    }

    #[test]
    fn parse_unknown_flag_errors() {
        let cat = CommandCatalogue::get();
        let err = parse("status --bogus", cat).unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    /// The TUI command box must reject a positional the command never declared,
    /// exactly as clap and the API projection do. `new skill` declares none, so
    /// a stray skill name next to `--pull` is a usage error (WI-0103).
    #[test]
    fn parse_rejects_positional_the_command_never_declared() {
        let cat = CommandCatalogue::get();
        let err = parse("new skill --pull owner/library accidental-name", cat)
            .expect_err("new skill takes no positional argument");
        assert!(
            matches!(
                &err,
                CommandError::UnexpectedArgument { argument, .. } if argument == "accidental-name"
            ),
            "expected UnexpectedArgument naming the stray name, got: {err:?}"
        );
    }

    #[test]
    fn parse_empty_string_returns_command_box_parse_error() {
        let cat = CommandCatalogue::get();
        let err = parse("", cat).unwrap_err();
        assert!(
            matches!(err, CommandError::CommandBoxParse(_)),
            "empty input must return CommandBoxParse, got: {err:?}"
        );
    }

    #[test]
    fn parse_quoted_string_argument_is_handled() {
        let cat = CommandCatalogue::get();
        let parsed = parse(r#"exec prompt "do something complex""#, cat).unwrap();
        assert_eq!(parsed.path, vec!["exec", "prompt"]);
        // `prompt` is a greedy trailing positional: a single quoted token
        // collects into a one-element Multi (the TUI frontend joins it back).
        match parsed.arguments.get("prompt") {
            Some(ArgValue::Multi(v)) => {
                assert_eq!(v, &["do something complex".to_string()]);
            }
            other => panic!("expected Multi prompt argument, got: {other:?}"),
        }
    }

    #[test]
    fn parse_short_flag_maps_to_long_name() {
        let cat = CommandCatalogue::get();
        let parsed = parse("ready -n", cat).unwrap();
        assert_eq!(parsed.path, vec!["ready"]);
        assert!(
            matches!(
                parsed.flags.get("non-interactive"),
                Some(FlagValue::Bool(true))
            ),
            "-n must map to non-interactive flag"
        );
    }
}
