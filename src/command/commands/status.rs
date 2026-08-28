//! `StatusCommand` — display the status of all running containers.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

#[derive(Debug, Clone)]
pub struct StatusCommandFlags {
    pub watch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ContainerKind {
    Agent,
}

/// Where a container came from, derived from its **name** — never a label —
/// so the marker works identically on every runtime tier (Apple cannot read
/// labels back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum ContainerSource {
    /// An interactive `awman` session container: today's behaviour, unmarked.
    Session,
    /// Launched by amie for the named condition.
    Amie(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusOutcome {
    pub containers: Vec<StatusContainerRow>,
    pub watched: bool,
    /// A randomly-selected hint for the user, refreshed each tick.
    pub tip: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusContainerRow {
    pub id: String,
    pub name: String,
    pub image: String,
    pub started_at: String,
    pub kind: ContainerKind,
    pub source: ContainerSource,
    pub tab_number: Option<u32>,
    pub stuck: bool,
    pub command_label: Option<String>,
    /// Live CPU percent reported by the runtime. `None` when stats lookup
    /// failed for this container (transient errors are not fatal — the row
    /// still renders).
    pub cpu_percent: Option<f64>,
    /// Live memory usage in MB.
    pub memory_mb: Option<f64>,
}

impl StatusContainerRow {
    /// `None` for a plain session container (today's behaviour, unmarked);
    /// `Some("amie:<slug>")` for one amie launched.
    pub fn source_label(&self) -> Option<String> {
        match &self.source {
            ContainerSource::Session => None,
            ContainerSource::Amie(slug) => Some(format!("amie:{slug}")),
        }
    }
}

/// Derive a container's source from its **name**, the one identity channel
/// every runtime tier honours (Docker `--filter name=`, Apple client-side
/// prefix, sandbox `sbx ls` prefix) — never from a label.
fn classify_source(name: &str) -> ContainerSource {
    match crate::engine::container::naming::parse_amie_condition_slug(name) {
        Some(slug) => ContainerSource::Amie(slug.to_string()),
        None => ContainerSource::Session,
    }
}

/// Optional context supplied by the TUI; CLI / API leave this `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusCommandTuiContext {
    pub tabs: Vec<TuiTabSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiTabSnapshot {
    pub tab_number: u32,
    pub container_name: Option<String>,
    pub is_stuck: bool,
    pub command_label: String,
}

pub trait StatusCommandFrontend: UserMessageSink + Send + Sync {
    /// Optional TUI context, returned as an owned clone so implementations
    /// can serve a fresh value from a shared slot on every call.
    /// Defaults to `None` for CLI / API.
    fn tui_context(&self) -> Option<StatusCommandTuiContext> {
        None
    }

    /// Whether the watch loop should continue. Default: stops after one tick.
    fn should_continue_watching(&mut self) -> bool {
        false
    }

    /// Emit a clear-screen marker so the CLI can redraw the status table in
    /// place. No-op for TUI / API frontends.
    fn write_clear_marker(&mut self) {}

    /// Render the status dashboard. Default writes plain text via
    /// `write_message`; the TUI overrides this to store structured data for
    /// a proper `Table` widget.
    fn write_status_dashboard(&mut self, containers: &[StatusContainerRow], tip: &str) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "AWMAN STATUS DASHBOARD".into(),
        });
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: String::new(),
        });
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "CODE AGENTS".into(),
        });
        if containers.is_empty() {
            self.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "  No code agents running.".into(),
            });
            self.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "  To start one: awman exec workflow <file>  or  awman chat".into(),
            });
        } else {
            for c in containers {
                let indicator = if c.stuck { "Y" } else { "G" };
                let cpu = c
                    .cpu_percent
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "-".into());
                let mem = c
                    .memory_mb
                    .map(|v| format!("{v:.0}MB"))
                    .unwrap_or_else(|| "-".into());
                let tab_col = c
                    .tab_number
                    .map(|t| format!(" [tab {t}]"))
                    .unwrap_or_default();
                let source_col = c
                    .source_label()
                    .map(|label| format!(" [{label}]"))
                    .unwrap_or_default();
                self.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!(
                        "  {indicator} {name}  {cpu}  {mem}  {img}{tab_col}{source_col}",
                        name = c.name,
                        img = c.image,
                    ),
                });
            }
        }
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("\nTip: {tip}"),
        });
        self.replay_queued();
    }
}

pub struct StatusCommand {
    flags: StatusCommandFlags,
    engines: Engines,
}

impl StatusCommand {
    pub fn new(flags: StatusCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &StatusCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for StatusCommand {
    type Frontend = Box<dyn StatusCommandFrontend>;
    type Outcome = StatusOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let mut last_containers: Vec<StatusContainerRow>;
        let mut tick: u32 = 0;

        loop {
            let handles = match self.engines.runtime.list_running_all() {
                Ok(h) => h,
                Err(e) => {
                    let err = CommandError::from(e);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("status: failed to list running containers: {err}"),
                    });
                    return Err(err);
                }
            };
            let context = frontend.tui_context();
            let containers: Vec<StatusContainerRow> = handles
                .into_iter()
                .map(|h| {
                    // Best-effort live stats; transient runtime errors are
                    // recorded as `None` rather than failing the row.
                    let stats = self.engines.runtime.stats(&h).ok();
                    let mut row = StatusContainerRow {
                        id: h.id.clone(),
                        name: h.name.clone(),
                        image: h.image_tag.clone(),
                        started_at: h.started_at.to_rfc3339(),
                        kind: ContainerKind::Agent,
                        source: classify_source(&h.name),
                        tab_number: None,
                        stuck: false,
                        command_label: None,
                        cpu_percent: stats.as_ref().map(|s| s.cpu_percent),
                        memory_mb: stats.as_ref().map(|s| s.memory_mb),
                    };
                    if let Some(ctx) = context.as_ref() {
                        if let Some(t) = ctx
                            .tabs
                            .iter()
                            .find(|t| t.container_name.as_deref() == Some(&h.name))
                        {
                            row.tab_number = Some(t.tab_number);
                            row.stuck = t.is_stuck;
                            row.command_label = Some(t.command_label.clone());
                        }
                    }
                    row
                })
                .collect();

            if tick > 0 {
                frontend.write_clear_marker();
            }
            tick = tick.saturating_add(1);

            let tip = crate::command::commands::status_tips::select_random_tip();
            frontend.write_status_dashboard(&containers, tip);

            last_containers = containers;

            if !self.flags.watch && !frontend.should_continue_watching() {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Ok(StatusOutcome {
            containers: last_containers,
            watched: self.flags.watch,
            tip: crate::command::commands::status_tips::select_random_tip().to_string(),
        })
    }
}

impl StatusCommandTuiContext {
    pub fn new(tabs: Vec<TuiTabSnapshot>) -> Self {
        Self { tabs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_context_new_stores_tabs() {
        let tabs = vec![
            TuiTabSnapshot {
                tab_number: 1,
                container_name: Some("awman-abc".into()),
                is_stuck: false,
                command_label: "chat".into(),
            },
            TuiTabSnapshot {
                tab_number: 2,
                container_name: None,
                is_stuck: true,
                command_label: "exec workflow".into(),
            },
        ];
        let ctx = StatusCommandTuiContext::new(tabs.clone());
        assert_eq!(ctx.tabs.len(), 2);
        assert_eq!(ctx.tabs[0].tab_number, 1);
        assert_eq!(ctx.tabs[1].tab_number, 2);
    }

    #[test]
    fn tui_context_enriches_row_with_matching_tab() {
        // Simulate the enrichment logic from run_with_frontend by building
        // a row and applying the TUI context logic directly.
        let ctx = StatusCommandTuiContext::new(vec![TuiTabSnapshot {
            tab_number: 3,
            container_name: Some("awman-mycontainer".into()),
            is_stuck: true,
            command_label: "exec workflow 0042".into(),
        }]);
        let name = "awman-mycontainer".to_string();
        let mut row = StatusContainerRow {
            id: "deadbeef1234".into(),
            name: name.clone(),
            image: "awman/dev:latest".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            tab_number: None,
            stuck: false,
            kind: ContainerKind::Agent,
            source: ContainerSource::Session,
            command_label: None,
            cpu_percent: None,
            memory_mb: None,
        };
        // Apply the same matching logic used in run_with_frontend.
        if let Some(t) = ctx
            .tabs
            .iter()
            .find(|t| t.container_name.as_deref() == Some(&row.name))
        {
            row.tab_number = Some(t.tab_number);
            row.stuck = t.is_stuck;
            row.command_label = Some(t.command_label.clone());
        }
        assert_eq!(row.tab_number, Some(3));
        assert!(row.stuck);
        assert_eq!(row.command_label.as_deref(), Some("exec workflow 0042"));
    }

    #[test]
    fn no_tui_context_leaves_row_fields_none() {
        // When no TUI context is supplied, tab_number, stuck, and
        // command_label stay at their default values.
        let row = StatusContainerRow {
            id: "abc".into(),
            name: "awman-x".into(),
            image: "img".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            kind: ContainerKind::Agent,
            source: ContainerSource::Session,
            tab_number: None,
            stuck: false,
            command_label: None,
            cpu_percent: None,
            memory_mb: None,
        };
        assert_eq!(row.tab_number, None);
        assert!(!row.stuck);
        assert_eq!(row.command_label, None);
    }

    #[test]
    fn tui_context_no_match_leaves_row_unchanged() {
        let ctx = StatusCommandTuiContext::new(vec![TuiTabSnapshot {
            tab_number: 1,
            container_name: Some("awman-other".into()),
            is_stuck: false,
            command_label: "chat".into(),
        }]);
        let mut row = StatusContainerRow {
            id: "abc".into(),
            name: "awman-mine".into(),
            image: "img".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            kind: ContainerKind::Agent,
            source: ContainerSource::Session,
            tab_number: None,
            stuck: false,
            command_label: None,
            cpu_percent: None,
            memory_mb: None,
        };
        // The name doesn't match → row stays unchanged.
        if let Some(t) = ctx
            .tabs
            .iter()
            .find(|t| t.container_name.as_deref() == Some(&row.name))
        {
            row.tab_number = Some(t.tab_number);
            row.stuck = t.is_stuck;
            row.command_label = Some(t.command_label.clone());
        }
        assert_eq!(row.tab_number, None, "no match must leave tab_number None");
    }

    #[test]
    fn classify_agent_containers() {
        assert_eq!(classify_source("awman-123-456"), ContainerSource::Session);
        assert_eq!(classify_source("awman-abc"), ContainerSource::Session);
    }

    #[test]
    fn classify_source_marks_amie_containers() {
        assert_eq!(
            classify_source("awman-amie-issue-triage-12ab34cd"),
            ContainerSource::Amie("issue-triage".to_string())
        );
    }

    #[test]
    fn source_label_is_none_for_session_and_marked_for_amie() {
        let mut row = StatusContainerRow {
            id: "abc".into(),
            name: "awman-x".into(),
            image: "img".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            kind: ContainerKind::Agent,
            source: ContainerSource::Session,
            tab_number: None,
            stuck: false,
            command_label: None,
            cpu_percent: None,
            memory_mb: None,
        };
        assert_eq!(row.source_label(), None);
        row.source = ContainerSource::Amie("issue-triage".into());
        assert_eq!(row.source_label().as_deref(), Some("amie:issue-triage"));
    }
}
