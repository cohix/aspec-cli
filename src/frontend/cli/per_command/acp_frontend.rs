//! `AcpFrontend` impl for the CLI.
//!
//! Renders ACP session updates as short, human-readable lines on stdout —
//! never raw JSON — and drives permission prompts / follow-up turns from
//! stdin. This is the line-oriented, cooked-mode counterpart to the TUI's
//! agent window: no raw mode, no PTY, just read/print like the rest of the
//! CLI's interactive Q&A (see `per_command/helpers.rs`).
//!
//! Headless runs (`-n`/`--non-interactive`, and `--yolo`/`--auto` which the
//! engine driver already handles by never calling `request_permission` at
//! all) must never block on stdin: `non_interactive` degrades
//! `request_permission` to auto-approve and `next_prompt` to "no follow-up",
//! exactly mirroring how `take_non_interactive_io` degrades the stdio
//! `AgentFrontend` path in `container_frontend_marker.rs`.

use std::io::Write;

use crate::engine::acp::protocol::{sanitize_terminal_text, ContentChunk, ToolCallLocation};
use crate::engine::acp::{
    AcpFrontend, AvailableCommand, ContentBlock, PermissionDecision, PermissionOption,
    PermissionRequest, PlanEntry, SessionUpdate, ToolCall, ToolCallUpdate,
};

use crate::frontend::cli::command_frontend::CliFrontend;

impl AcpFrontend for CliFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        match render_update_text(&update) {
            RenderedUpdate::Stream(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            RenderedUpdate::Block(text) => {
                // Block text mixes awman's own labels (▸, brackets — all
                // printable) with agent-provided titles/paths/URIs; sanitising
                // the whole line strips any control chars the agent injected
                // without touching awman's formatting.
                println!("{}", sanitize_terminal_text(&text));
            }
        }
    }

    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision {
        if self.non_interactive {
            return PermissionDecision::approve(&request.options);
        }
        println!("{}", sanitize_terminal_text(&format_permission_request(&request)));
        loop {
            print!("choice: ");
            let _ = std::io::stdout().flush();
            match read_stdin_line() {
                None => return PermissionDecision::Cancelled,
                Some(input) => match match_permission_option(&request.options, &input) {
                    Some(option_id) => return PermissionDecision::Selected { option_id },
                    None => println!("awman: unrecognized choice {input:?}; try again"),
                },
            }
        }
    }

    fn next_prompt(&mut self) -> Option<String> {
        if self.non_interactive {
            return None;
        }
        print!("\n> ");
        let _ = std::io::stdout().flush();
        read_stdin_line()
    }
}

/// A single rendered update, split so the streamed-text case (message/thought
/// chunks, printed with no trailing newline as they arrive) is distinct from
/// the block case (tool calls, plans, available commands — each a complete,
/// newline-terminated line).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RenderedUpdate {
    Stream(String),
    Block(String),
}

pub(crate) fn render_update_text(update: &SessionUpdate) -> RenderedUpdate {
    match update {
        SessionUpdate::AgentMessageChunk { chunk } => render_content_chunk(chunk, false),
        SessionUpdate::AgentThoughtChunk { chunk } => render_content_chunk(chunk, true),
        SessionUpdate::ToolCall { tool_call } => RenderedUpdate::Block(format_tool_call(tool_call)),
        SessionUpdate::ToolCallUpdate { tool_call } => {
            RenderedUpdate::Block(format_tool_call_update(tool_call))
        }
        SessionUpdate::Plan { entries } => RenderedUpdate::Block(format_plan(entries)),
        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            RenderedUpdate::Block(format_available_commands(available_commands))
        }
    }
}

/// Message chunks stream raw text inline. Thought chunks stream too (ANSI
/// "dim" so they read as an aside, not the agent's actual reply) rather than
/// repeating a "[thinking]" label on every token. Non-text content within
/// either kind of chunk is rare and atomic, so it renders as its own block.
fn render_content_chunk(chunk: &ContentChunk, dim: bool) -> RenderedUpdate {
    match &chunk.content {
        ContentBlock::Text { text } => {
            // Strip agent-provided terminal-control characters (ANSI/OSC/etc.)
            // BEFORE wrapping with awman's own "dim" escape, so a malicious
            // agent can't smuggle escape sequences through the structured
            // renderer — but awman's intentional formatting is preserved.
            let text = sanitize_terminal_text(text);
            let text = if dim {
                format!("\x1b[2m{text}\x1b[0m")
            } else {
                text
            };
            RenderedUpdate::Stream(text)
        }
        other => RenderedUpdate::Block(content_block_label(other)),
    }
}

fn content_block_label(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image { mime_type, .. } => format!("[image: {mime_type}]"),
        ContentBlock::Audio { mime_type, .. } => format!("[audio: {mime_type}]"),
        ContentBlock::ResourceLink { name, uri, .. } => format!("[resource: {name} ({uri})]"),
        ContentBlock::Resource { .. } => "[resource]".to_string(),
    }
}

fn format_tool_call(tool_call: &ToolCall) -> String {
    tool_line(
        "tool",
        &tool_call.title,
        tool_call.kind.as_deref(),
        tool_call.status.as_deref(),
        &tool_call.locations,
    )
}

fn format_tool_call_update(update: &ToolCallUpdate) -> String {
    tool_line(
        "tool update",
        &tool_call_update_title(update),
        update.kind.as_deref(),
        update.status.as_deref(),
        update.locations.as_deref().unwrap_or(&[]),
    )
}

fn tool_call_update_title(update: &ToolCallUpdate) -> String {
    update
        .title
        .clone()
        .unwrap_or_else(|| format!("tool {}", update.tool_call_id))
}

fn tool_line(
    prefix: &str,
    title: &str,
    kind: Option<&str>,
    status: Option<&str>,
    locations: &[ToolCallLocation],
) -> String {
    let mut line = format!("▸ {prefix}: {title}");
    if let Some(kind) = kind {
        line.push_str(&format!(" [{kind}]"));
    }
    if let Some(status) = status {
        line.push_str(&format!(" — {status}"));
    }
    if !locations.is_empty() {
        let locs: Vec<String> = locations
            .iter()
            .map(|l| match l.line {
                Some(line_no) => format!("{}:{line_no}", l.path),
                None => l.path.clone(),
            })
            .collect();
        line.push_str(&format!(" ({})", locs.join(", ")));
    }
    line
}

fn format_plan(entries: &[PlanEntry]) -> String {
    let mut out = String::from("plan:");
    for entry in entries {
        out.push_str(&format!(
            "\n  [{}] {} ({})",
            entry.status, entry.content, entry.priority
        ));
    }
    out
}

fn format_available_commands(commands: &[AvailableCommand]) -> String {
    if commands.is_empty() {
        return "available commands: (none)".to_string();
    }
    let mut out = String::from("available commands:");
    for command in commands {
        out.push_str(&format!("\n  /{} — {}", command.name, command.description));
    }
    out
}

fn format_permission_request(request: &PermissionRequest) -> String {
    let mut out = format!(
        "permission requested: {}",
        tool_line(
            "tool",
            &tool_call_update_title(&request.tool_call),
            request.tool_call.kind.as_deref(),
            request.tool_call.status.as_deref(),
            request.tool_call.locations.as_deref().unwrap_or(&[]),
        )
    );
    for (i, option) in request.options.iter().enumerate() {
        out.push_str(&format!("\n  [{}] {} ({})", i + 1, option.name, option.kind));
    }
    out
}

fn match_permission_option(options: &[PermissionOption], input: &str) -> Option<String> {
    let trimmed = input.trim();
    if let Ok(index) = trimmed.parse::<usize>() {
        if index >= 1 {
            if let Some(option) = options.get(index - 1) {
                return Some(option.option_id.clone());
            }
        }
    }
    options
        .iter()
        .find(|o| o.option_id.eq_ignore_ascii_case(trimmed) || o.name.eq_ignore_ascii_case(trimmed))
        .map(|o| o.option_id.clone())
}

/// Reads one line from stdin, distinguishing EOF (Ctrl+D) from an empty
/// line: `None` on EOF or a read error, `Some(trimmed)` otherwise — even
/// `Some(String::new())` for a blank line. `per_command::helpers::read_line`
/// cannot be reused here because it collapses "not a TTY" and "EOF" into the
/// same `None`, while an ACP session must react to those differently (a
/// live-but-non-interactive run degrades before ever calling this; a live
/// interactive run's EOF must end the session).
fn read_stdin_line() -> Option<String> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => None,
        Ok(_) => Some(buf.trim().to_string()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::catalogue::CommandCatalogue;

    /// Build a `CliFrontend` for unit tests. In the test environment stdin
    /// is never a TTY, so `non_interactive` is always `true` — exactly the
    /// degrade path `request_permission`/`next_prompt` must take without
    /// blocking on a read.
    fn make_frontend() -> CliFrontend {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "chat"]).unwrap();
        CliFrontend::new(m)
    }

    fn text_chunk(text: &str) -> ContentChunk {
        ContentChunk {
            content: ContentBlock::Text { text: text.into() },
            message_id: Some("m1".into()),
        }
    }

    #[test]
    fn message_chunk_streams_raw_text() {
        let update = SessionUpdate::AgentMessageChunk {
            chunk: text_chunk("hello"),
        };
        assert_eq!(
            render_update_text(&update),
            RenderedUpdate::Stream("hello".into())
        );
    }

    #[test]
    fn agent_text_is_stripped_of_terminal_control_sequences() {
        // A malicious agent embeds an OSC 52 clipboard-write escape in its
        // message text; the structured renderer must neutralise it rather than
        // hand raw terminal-control bytes to the user's terminal.
        let update = SessionUpdate::AgentMessageChunk {
            chunk: text_chunk("ok\x1b]52;c;ZXZpbA==\x07done"),
        };
        match render_update_text(&update) {
            RenderedUpdate::Stream(text) => {
                assert!(!text.contains('\x1b'), "ESC must be stripped: {text:?}");
                assert!(!text.contains('\x07'), "BEL must be stripped: {text:?}");
                assert!(text.contains("ok") && text.contains("done"));
            }
            other => panic!("expected Stream, got {other:?}"),
        }
    }

    #[test]
    fn thought_chunk_streams_dimmed_text() {
        let update = SessionUpdate::AgentThoughtChunk {
            chunk: text_chunk("pondering"),
        };
        assert_eq!(
            render_update_text(&update),
            RenderedUpdate::Stream("\x1b[2mpondering\x1b[0m".into())
        );
    }

    #[test]
    fn non_text_chunk_renders_as_a_labeled_block_not_json() {
        let chunk = ContentChunk {
            content: ContentBlock::Image {
                data: "base64...".into(),
                mime_type: "image/png".into(),
            },
            message_id: None,
        };
        let update = SessionUpdate::AgentMessageChunk { chunk };
        match render_update_text(&update) {
            RenderedUpdate::Block(text) => {
                assert_eq!(text, "[image: image/png]");
                assert!(!text.contains('{'), "must never dump raw JSON: {text}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_renders_title_kind_status_and_locations() {
        let tool_call = ToolCall {
            tool_call_id: "t1".into(),
            title: "Read file".into(),
            kind: Some("read".into()),
            status: Some("pending".into()),
            content: vec![],
            locations: vec![ToolCallLocation {
                path: "src/lib.rs".into(),
                line: Some(10),
            }],
            raw_input: None,
            raw_output: None,
        };
        let update = SessionUpdate::ToolCall { tool_call };
        match render_update_text(&update) {
            RenderedUpdate::Block(text) => {
                assert!(text.contains("Read file"), "{text}");
                assert!(text.contains("[read]"), "{text}");
                assert!(text.contains("pending"), "{text}");
                assert!(text.contains("src/lib.rs:10"), "{text}");
                assert!(!text.contains('{'), "must never dump raw JSON: {text}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_falls_back_to_tool_call_id_when_title_missing() {
        let update = ToolCallUpdate {
            tool_call_id: "t1".into(),
            kind: None,
            status: Some("completed".into()),
            title: None,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: None,
        };
        let rendered = render_update_text(&SessionUpdate::ToolCallUpdate { tool_call: update });
        match rendered {
            RenderedUpdate::Block(text) => {
                assert!(text.contains("tool t1"), "{text}");
                assert!(text.contains("completed"), "{text}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn plan_renders_every_entry() {
        let entries = vec![
            PlanEntry {
                content: "write tests".into(),
                priority: "high".into(),
                status: "pending".into(),
            },
            PlanEntry {
                content: "ship it".into(),
                priority: "low".into(),
                status: "pending".into(),
            },
        ];
        let text = format_plan(&entries);
        assert!(text.starts_with("plan:"));
        assert!(text.contains("write tests"));
        assert!(text.contains("ship it"));
    }

    #[test]
    fn available_commands_lists_name_and_description() {
        let commands = vec![AvailableCommand {
            name: "build".into(),
            description: "build the project".into(),
            input: None,
        }];
        let text = format_available_commands(&commands);
        assert!(text.contains("/build"));
        assert!(text.contains("build the project"));
    }

    #[test]
    fn available_commands_empty_list_has_no_trailing_garbage() {
        assert_eq!(format_available_commands(&[]), "available commands: (none)");
    }

    #[test]
    fn permission_request_lists_numbered_options() {
        let request = PermissionRequest {
            request_id: Default::default(),
            session_id: "s1".into(),
            tool_call: ToolCallUpdate {
                tool_call_id: "t1".into(),
                kind: Some("execute".into()),
                status: None,
                title: Some("Run tests".into()),
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
            options: vec![
                PermissionOption {
                    option_id: "allow".into(),
                    name: "Allow".into(),
                    kind: "allow_once".into(),
                },
                PermissionOption {
                    option_id: "deny".into(),
                    name: "Deny".into(),
                    kind: "reject_once".into(),
                },
            ],
        };
        let text = format_permission_request(&request);
        assert!(text.contains("Run tests"), "{text}");
        assert!(text.contains("[1] Allow (allow_once)"), "{text}");
        assert!(text.contains("[2] Deny (reject_once)"), "{text}");
    }

    #[test]
    fn match_permission_option_by_index_name_or_id() {
        let options = vec![
            PermissionOption {
                option_id: "allow-once".into(),
                name: "Allow once".into(),
                kind: "allow_once".into(),
            },
            PermissionOption {
                option_id: "deny-once".into(),
                name: "Deny once".into(),
                kind: "reject_once".into(),
            },
        ];
        assert_eq!(
            match_permission_option(&options, "1"),
            Some("allow-once".into())
        );
        assert_eq!(
            match_permission_option(&options, "deny once"),
            Some("deny-once".into())
        );
        assert_eq!(
            match_permission_option(&options, "ALLOW-ONCE"),
            Some("allow-once".into())
        );
        assert_eq!(match_permission_option(&options, "nope"), None);
        assert_eq!(match_permission_option(&options, "0"), None);
        assert_eq!(match_permission_option(&options, "99"), None);
    }

    #[test]
    fn non_interactive_next_prompt_ends_session_without_blocking() {
        let mut fe = make_frontend();
        assert!(fe.non_interactive, "test stdin is never a TTY");
        assert_eq!(AcpFrontend::next_prompt(&mut fe), None);
    }

    #[test]
    fn non_interactive_request_permission_auto_approves_without_blocking() {
        let mut fe = make_frontend();
        assert!(fe.non_interactive, "test stdin is never a TTY");
        let options = vec![
            PermissionOption {
                option_id: "deny".into(),
                name: "Deny".into(),
                kind: "reject_once".into(),
            },
            PermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            },
        ];
        let request = PermissionRequest {
            request_id: Default::default(),
            session_id: "s1".into(),
            tool_call: ToolCallUpdate {
                tool_call_id: "t1".into(),
                kind: None,
                status: None,
                title: None,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
            options,
        };
        let decision = AcpFrontend::request_permission(&mut fe, request);
        assert_eq!(
            decision,
            PermissionDecision::Selected {
                option_id: "allow".into()
            }
        );
    }
}
