//! Embedded static assets for dynamic workflows (`exec workflow --dynamic`).
//!
//! These are compiled into the binary from `src/assets/dynamic/` and are
//! always regenerated from this embedded content at runtime — never read from
//! the host filesystem. See WI-0092.
//!
//! - [`EXAMPLE_WORKFLOW_TOML`] and [`WORKFLOW_USAGE_MD`] are written into the
//!   leader's workflow context directory as reference material.
//! - [`LEADER_PROMPT_MD`] is the leader prompt template; it is substituted in
//!   code (never written to disk) before being delivered to the leader agent.
//! - [`LEADER_REPAIR_PROMPT`] is the repair prompt template used when the
//!   leader's `workflow.toml` fails validation.

/// A complete example workflow shown to the leader agent as reference. Written
/// to `<context_dir>/example-workflow.toml`.
pub const EXAMPLE_WORKFLOW_TOML: &str = include_str!("../assets/dynamic/example-workflow.toml");

/// The complete workflow file-format documentation. Written to
/// `<context_dir>/workflow-usage.md`.
pub const WORKFLOW_USAGE_MD: &str = include_str!("../assets/dynamic/workflow-usage.md");

/// The leader prompt template. Substituted with `{{work_item_number}}`,
/// `{{work_item_path}}`, `{{available_agents}}`,
/// `{{max_concurrent_steps_note}}`, and `{{developer_guidance}}` before being
/// delivered.
pub const LEADER_PROMPT_MD: &str = include_str!("../assets/dynamic/leader-prompt.md");

/// The repair prompt template. Substituted with `{{validation_error}}`.
pub const LEADER_REPAIR_PROMPT: &str = include_str!("../assets/dynamic/leader-repair-prompt.md");

/// The condition-evaluation leader prompt. Unlike a dynamic work-item leader,
/// this agent first reports whether the condition is met.
pub const AMIE_LEADER_PROMPT_MD: &str = include_str!("../assets/dynamic/amie-leader-prompt.md");

/// Construct the amie evaluation-leader prompt.
pub fn build_amie_leader_prompt(
    condition_name: &str,
    condition_description: &str,
    repo_mount_path: &str,
    available_agents: &str,
    guidance: Option<&[String]>,
) -> String {
    AMIE_LEADER_PROMPT_MD
        .replace("{{condition_name}}", condition_name)
        .replace("{{condition_description}}", condition_description)
        .replace("{{repo_mount_path}}", repo_mount_path)
        .replace("{{available_agents}}", available_agents)
        .replace(
            "{{developer_guidance}}",
            &build_developer_guidance(guidance),
        )
}

/// Construct the leader prompt by substituting the runtime template variables
/// into [`LEADER_PROMPT_MD`].
pub fn build_leader_prompt(
    work_item_number: &str,
    work_item_path: &str,
    available_agents: &str,
    max_concurrent_steps: Option<usize>,
    guidance: Option<&[String]>,
) -> String {
    let max_concurrent_steps_note = match max_concurrent_steps {
        Some(n) => format!(
            "Note: the repository configuration advises a maximum of {n} concurrent steps. \
             Plan your workflow accordingly."
        ),
        None => String::new(),
    };
    let developer_guidance = build_developer_guidance(guidance);
    LEADER_PROMPT_MD
        .replace("{{work_item_number}}", work_item_number)
        .replace("{{work_item_path}}", work_item_path)
        .replace("{{available_agents}}", available_agents)
        .replace("{{max_concurrent_steps_note}}", &max_concurrent_steps_note)
        .replace("{{developer_guidance}}", &developer_guidance)
}

/// Render the `## Developer Guidance` block for the leader prompt from the
/// repo config's `dynamicWorkflows.guidance` entries. Returns an empty string
/// when guidance is absent or empty so the `{{developer_guidance}}` placeholder
/// disappears cleanly. Whitespace-only entries are skipped and any literal
/// newlines within an entry are flattened to spaces so each entry stays a
/// single bullet point (WI-0099).
fn build_developer_guidance(guidance: Option<&[String]>) -> String {
    let entries = match guidance {
        Some(entries) if !entries.is_empty() => entries,
        _ => return String::new(),
    };
    let bullets: Vec<String> = entries
        .iter()
        .filter(|e| !e.trim().is_empty())
        .map(|e| {
            let flattened = e
                .split(['\n', '\r'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            format!("- {flattened}")
        })
        .collect();
    if bullets.is_empty() {
        return String::new();
    }
    format!(
        "\n## Developer Guidance\nYou MUST follow these project-specific instructions when \
         building the workflow:\n{}\n",
        bullets.join("\n")
    )
}

/// Construct the repair prompt by substituting the verbatim validation error
/// into [`LEADER_REPAIR_PROMPT`].
pub fn build_repair_prompt(validation_error: &str) -> String {
    LEADER_REPAIR_PROMPT.replace("{{validation_error}}", validation_error)
}
