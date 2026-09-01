//! `NewCommand` — `new spec`, `new workflow`, `new skill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::prompt_templates::{
    render_skill_interview_prompt, render_workflow_interview_prompt,
};
use crate::command::commands::skill_library::{
    pull_all_libraries, pull_library, resolve_pull_target, PullOutcome,
};
use crate::command::commands::{resolve_agent, Command};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::{SkillDirs, WorkflowDirs};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::ContainerOption;

#[derive(Debug, Clone)]
pub struct NewSpecFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub issue_source: crate::data::issue::IssueSourceFlags,
}

#[derive(Debug, Clone)]
pub struct NewWorkflowFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct NewSkillFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub pull: Option<String>,
    pub pull_all: bool,
    pub subdir: Option<String>,
}

#[derive(Debug, Clone)]
pub enum NewSubcommand {
    Spec(NewSpecFlags),
    Workflow(NewWorkflowFlags),
    Skill(NewSkillFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub interview: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewWorkflowOutcome {
    pub interview: bool,
    pub global: bool,
    pub format: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PullLibraryOutcome {
    pub slug: String,
    pub dir: String,
    pub updated: bool,
    pub skills_found: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewSkillOutcome {
    pub interview: bool,
    pub global: bool,
    pub path: Option<String>,
    /// `true` when this run was a `--pull`/`--pull-all` library operation
    /// rather than skill creation. `libraries` alone cannot carry that
    /// distinction: `--pull-all` with nothing pulled yet also yields an empty
    /// list, and rendering it as a created skill would be wrong.
    pub pull: bool,
    pub libraries: Vec<PullLibraryOutcome>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum NewOutcome {
    Spec(NewSpecOutcome),
    Workflow(NewWorkflowOutcome),
    Skill(NewSkillOutcome),
}

/// A single step collected from the user during `new workflow`.
#[derive(Debug, Clone)]
pub struct WorkflowStepInput {
    pub name: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
}

/// `NewCommandFrontend` extends `SpecsCommandFrontend` so the `Spec`
/// subcommand can drive the same Q&A (kind / title / summary).
pub trait NewCommandFrontend:
    UserMessageSink + crate::command::commands::specs::SpecsCommandFrontend + Send + Sync
{
    /// Prompt for a workflow name. CLI implementations gate on stdin TTY.
    fn ask_workflow_name(&mut self) -> Result<String, CommandError> {
        Ok("workflow".to_string())
    }
    /// Prompt for a human-readable workflow title.
    fn ask_workflow_title(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for a one-line summary for the new workflow (used in interview mode).
    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for a step name.
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for the optional agent override for the current step.
    fn ask_workflow_step_agent(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Prompt for the optional model override for the current step.
    fn ask_workflow_step_model(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Prompt for the step prompt text.
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Ask whether to add another workflow step.
    fn ask_add_another_step(&mut self) -> Result<bool, CommandError> {
        Ok(false)
    }
    /// Prompt for a skill name.
    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        Ok("skill".to_string())
    }
    /// Prompt for a one-line summary for the new skill (used in interview mode).
    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for the body content of the new skill.
    fn ask_skill_body(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
}

// ─── Serde structs for workflow serialization ────────────────────────────────

#[derive(Debug, Serialize)]
struct WorkflowFileToml<'a> {
    title: &'a str,
    #[serde(rename = "step", skip_serializing_if = "Vec::is_empty")]
    steps_toml: Vec<WorkflowStepSerde<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowFileYaml<'a> {
    title: &'a str,
    steps: Vec<WorkflowStepSerde<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowStepSerde<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    prompt: &'a str,
}

fn serialize_steps(steps: &[WorkflowStepInput]) -> Vec<WorkflowStepSerde<'_>> {
    steps
        .iter()
        .map(|s| WorkflowStepSerde {
            name: &s.name,
            agent: s.agent.as_deref(),
            model: s.model.as_deref(),
            prompt: &s.prompt,
        })
        .collect()
}

fn serialize_workflow_toml(title: &str, steps: &[WorkflowStepInput]) -> String {
    let file = WorkflowFileToml {
        title,
        steps_toml: serialize_steps(steps),
    };
    toml::to_string_pretty(&file).unwrap_or_else(|_| format!("title = \"{title}\"\n"))
}

fn serialize_workflow_yaml(title: &str, steps: &[WorkflowStepInput]) -> String {
    let file = WorkflowFileYaml {
        title,
        steps: serialize_steps(steps),
    };
    serde_yaml::to_string(&file).unwrap_or_else(|_| format!("title: \"{title}\"\nsteps: []\n"))
}

/// Subdirectory under the git root for user-authored workflow definitions.
const REPO_WORKFLOW_DEFINITIONS_DIR: &str = "aspec/workflows";

pub struct NewCommand {
    sub: NewSubcommand,
    engines: Engines,
    session: Session,
}

impl NewCommand {
    pub fn new(sub: NewSubcommand, engines: Engines, session: Session) -> Self {
        Self {
            sub,
            engines,
            session,
        }
    }

    pub fn subcommand(&self) -> &NewSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for NewCommand {
    type Frontend = Box<dyn NewCommandFrontend>;
    type Outcome = NewOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let outcome = match self.sub {
            NewSubcommand::Spec(f) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "new spec: starting work item creation".into(),
                });
                let new_outcome = match crate::command::commands::specs::create_new_spec(
                    &self.engines,
                    self.session.clone(),
                    f.interview,
                    f.non_interactive,
                    f.issue_source,
                    frontend.as_mut(),
                )
                .await
                {
                    Ok(o) => o,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("new spec: failed to create spec: {e}"),
                        });
                        return Err(e);
                    }
                };
                NewOutcome::Spec(NewSpecOutcome {
                    interview: new_outcome.interview,
                    path: new_outcome.created_path,
                })
            }
            NewSubcommand::Workflow(f) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "new workflow: starting workflow creation".into(),
                });
                let name = frontend
                    .ask_workflow_name()
                    .unwrap_or_else(|_| "workflow".into());
                let extension = match f.format.as_str() {
                    "yaml" => "yaml",
                    "yml" => "yml",
                    _ => "toml",
                };
                let session = if !f.global || f.interview {
                    Some(self.session.clone())
                } else {
                    None
                };

                // Resolve destination directory. Non-global workflows go
                // under <git_root>/aspec/workflows/ (the user-facing
                // definitions directory), matching old-amux behaviour.
                let dir = if f.global {
                    let git_root = session.as_ref().map(|s| s.git_root().to_path_buf());
                    let workflow_dirs = match WorkflowDirs::from_process_env(git_root) {
                        Ok(d) => d,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new workflow: failed to resolve workflow dirs: {e}"),
                            });
                            return Err(CommandError::from(e));
                        }
                    };
                    workflow_dirs.global_dir()
                } else {
                    let git_root = session
                        .as_ref()
                        .expect("session required for non-global workflow")
                        .git_root();
                    git_root.join(REPO_WORKFLOW_DEFINITIONS_DIR)
                };
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("{name}.{extension}"));

                if f.interview {
                    // Interview mode: write a skeleton, then launch an agent
                    // to fill it in.
                    let skeleton = match extension {
                        "yaml" | "yml" => format!("title: \"{name}\"\nsteps: []\n"),
                        _ => format!("title = \"{name}\"\n"),
                    };
                    let _ = std::fs::write(&path, skeleton);

                    let session = session.as_ref().unwrap();
                    let agent = match resolve_agent(&None, session) {
                        Ok(a) => a,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new workflow: failed to resolve agent: {e}"),
                            });
                            return Err(e);
                        }
                    };
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!(
                            "new workflow: launching interview agent '{}'",
                            agent.as_str()
                        ),
                    });
                    let credentials =
                        match self.engines.auth_engine.resolve_agent_auth(session, &agent) {
                            Ok(c) => c,
                            Err(e) => {
                                frontend.write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!(
                                        "new workflow: failed to resolve agent auth: {e}"
                                    ),
                                });
                                return Err(CommandError::from(e));
                            }
                        };
                    let summary = frontend.ask_workflow_summary().unwrap_or_default();
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&name)
                        .to_string();
                    let path_str = path.display().to_string();
                    let prompt = render_workflow_interview_prompt(&filename, &path_str, &summary);
                    let run_opts = AgentRunOptions {
                        initial_prompt: Some(prompt),
                        non_interactive: f.non_interactive,
                        env_passthrough: None,
                        ..Default::default()
                    };
                    // Sandbox-class runtimes: agent spawn lands in WI 0090.
                    self.engines
                        .require_container_runtime()
                        .map_err(CommandError::from)?;
                    let mut options = match self
                        .engines
                        .agent_engine
                        .build_options_with_credentials(session, &agent, &run_opts, &credentials)
                    {
                        Ok(o) => o,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new workflow: failed to build agent options: {e}"),
                            });
                            return Err(CommandError::from(e));
                        }
                    };
                    if !credentials.env_vars.is_empty()
                        && matches!(
                            credentials.delivery,
                            crate::engine::auth::CredentialDelivery::Env
                        )
                    {
                        options.push(ContainerOption::AgentCredentials {
                            env_vars: credentials.env_vars,
                        });
                    }
                    let instance =
                        match crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
                            .and_then(|o| self.engines.runtime.build(o))
                        {
                            Ok(i) => i,
                            Err(e) => {
                                frontend.write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!("new workflow: failed to build container: {e}"),
                                });
                                return Err(CommandError::from(e));
                            }
                        };
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: "Launching agent container…".into(),
                    });
                    frontend.set_pty_active(true);
                    let cf = frontend.container_frontend_for_pty();
                    let mut execution = match instance.run_with_frontend(cf) {
                        Ok(e) => e,
                        Err(e) => {
                            frontend.set_pty_active(false);
                            frontend.replay_queued();
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new workflow: failed to run container: {e}"),
                            });
                            return Err(CommandError::from(e));
                        }
                    };
                    let _ = execution.wait().await;
                    frontend.set_pty_active(false);
                    frontend.replay_queued();
                } else {
                    // Non-interview: collect title and steps from the user,
                    // then serialize the complete workflow to disk.
                    let title = frontend
                        .ask_workflow_title()
                        .unwrap_or_else(|_| name.clone());
                    let title = if title.is_empty() {
                        name.clone()
                    } else {
                        title
                    };

                    let mut steps: Vec<WorkflowStepInput> = Vec::new();
                    loop {
                        let step_name = frontend.ask_workflow_step_name()?;
                        if step_name.is_empty() {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: "Step name cannot be empty.".into(),
                            });
                            continue;
                        }
                        let agent = frontend.ask_workflow_step_agent()?;
                        let model = frontend.ask_workflow_step_model()?;
                        let prompt = frontend.ask_workflow_step_prompt()?;
                        steps.push(WorkflowStepInput {
                            name: step_name,
                            agent,
                            model,
                            prompt,
                        });
                        match frontend.ask_add_another_step() {
                            Ok(true) => continue,
                            _ => break,
                        }
                    }

                    if steps.is_empty() {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: "At least one step is required.".into(),
                        });
                        return Err(CommandError::Aborted);
                    }

                    let body = match extension {
                        "yaml" | "yml" => serialize_workflow_yaml(&title, &steps),
                        _ => serialize_workflow_toml(&title, &steps),
                    };
                    let _ = std::fs::write(&path, body);
                }

                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!("Created workflow: {}", path.display()),
                });

                NewOutcome::Workflow(NewWorkflowOutcome {
                    interview: f.interview,
                    global: f.global,
                    format: f.format,
                    path: Some(path.display().to_string()),
                })
            }
            NewSubcommand::Skill(f) => {
                if f.pull_all {
                    let skill_dirs = match SkillDirs::from_process_env(None) {
                        Ok(dirs) => dirs,
                        Err(error) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new skill: failed to resolve skill dirs: {error}"),
                            });
                            return Err(CommandError::from(error));
                        }
                    };
                    let slugs = skill_dirs.list_libraries();
                    let results = pull_all_libraries(&self.engines.git_engine, &skill_dirs);
                    let mut libraries = Vec::with_capacity(results.len());
                    let mut successes = 0usize;
                    let mut failures = 0usize;

                    for (slug, result) in slugs.into_iter().zip(results) {
                        match result {
                            Ok(outcome) => {
                                frontend.write_message(pull_success_message(&outcome));
                                successes += 1;
                                libraries.push(pull_library_outcome(outcome));
                            }
                            Err(error) => {
                                frontend.write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!("Failed to pull '{slug}': {error}"),
                                });
                                failures += 1;
                                let dir = skill_dirs.library_dir(&slug).display().to_string();
                                libraries.push(PullLibraryOutcome {
                                    slug,
                                    dir,
                                    updated: false,
                                    skills_found: Vec::new(),
                                    error: Some(error.to_string()),
                                });
                            }
                        }
                    }

                    if libraries.is_empty() {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: "no skill libraries pulled yet".to_string(),
                        });
                    } else {
                        frontend.write_message(UserMessage {
                            level: if failures == 0 {
                                MessageLevel::Info
                            } else {
                                MessageLevel::Error
                            },
                            text: format!(
                                "Skill library refresh complete: {successes} succeeded, {failures} failed."
                            ),
                        });
                    }

                    NewOutcome::Skill(NewSkillOutcome {
                        interview: false,
                        global: false,
                        path: None,
                        pull: true,
                        libraries,
                    })
                } else if let Some(target) = &f.pull {
                    let skill_dirs = match SkillDirs::from_process_env(None) {
                        Ok(dirs) => dirs,
                        Err(error) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new skill: failed to resolve skill dirs: {error}"),
                            });
                            return Err(CommandError::from(error));
                        }
                    };
                    let outcome = match resolve_pull_target(target)
                        .map_err(CommandError::Other)
                        .and_then(|target| {
                            pull_library(
                                &self.engines.git_engine,
                                &skill_dirs,
                                target,
                                f.subdir.as_deref(),
                            )
                        }) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new skill: failed to pull skill library: {error}"),
                            });
                            return Err(error);
                        }
                    };
                    frontend.write_message(pull_success_message(&outcome));
                    let path = outcome.dir.display().to_string();
                    NewOutcome::Skill(NewSkillOutcome {
                        interview: false,
                        global: false,
                        path: Some(path),
                        pull: true,
                        libraries: vec![pull_library_outcome(outcome)],
                    })
                } else {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: "new skill: starting skill creation".into(),
                    });
                    let name = frontend.ask_skill_name().unwrap_or_else(|_| "skill".into());
                    let session = if !f.global || f.interview {
                        Some(self.session.clone())
                    } else {
                        None
                    };
                    let git_root = session.as_ref().map(|s| s.git_root().to_path_buf());
                    let skill_dirs = match SkillDirs::from_process_env(git_root) {
                        Ok(d) => d,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new skill: failed to resolve skill dirs: {e}"),
                            });
                            return Err(CommandError::from(e));
                        }
                    };
                    let dir = if f.global {
                        skill_dirs.global_dir().join(&name)
                    } else {
                        skill_dirs.repo_dir().unwrap().join(&name)
                    };
                    let _ = std::fs::create_dir_all(&dir);
                    let path = dir.join("SKILL.md");

                    if f.interview {
                        let skeleton = format!("# Skill: {name}\n\n## Description\n\n## Body\n");
                        let _ = std::fs::write(&path, skeleton);
                        let session = session.as_ref().unwrap();
                        let agent = match resolve_agent(&None, session) {
                            Ok(a) => a,
                            Err(e) => {
                                frontend.write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!("new skill: failed to resolve agent: {e}"),
                                });
                                return Err(e);
                            }
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!(
                                "new skill: launching interview agent '{}'",
                                agent.as_str()
                            ),
                        });
                        let credentials =
                            match self.engines.auth_engine.resolve_agent_auth(session, &agent) {
                                Ok(c) => c,
                                Err(e) => {
                                    frontend.write_message(UserMessage {
                                        level: MessageLevel::Error,
                                        text: format!(
                                            "new skill: failed to resolve agent auth: {e}"
                                        ),
                                    });
                                    return Err(CommandError::from(e));
                                }
                            };
                        let summary = frontend.ask_skill_summary().unwrap_or_default();
                        let path_str = path.display().to_string();
                        let prompt = render_skill_interview_prompt(&path_str, &summary);
                        let run_opts = AgentRunOptions {
                            initial_prompt: Some(prompt),
                            non_interactive: f.non_interactive,
                            env_passthrough: None,
                            ..Default::default()
                        };
                        // Sandbox-class runtimes: agent spawn lands in WI 0090.
                        self.engines
                            .require_container_runtime()
                            .map_err(CommandError::from)?;
                        let mut options = match self
                            .engines
                            .agent_engine
                            .build_options_with_credentials(
                                session,
                                &agent,
                                &run_opts,
                                &credentials,
                            ) {
                            Ok(o) => o,
                            Err(e) => {
                                frontend.write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!("new skill: failed to build agent options: {e}"),
                                });
                                return Err(CommandError::from(e));
                            }
                        };
                        if !credentials.env_vars.is_empty()
                            && matches!(
                                credentials.delivery,
                                crate::engine::auth::CredentialDelivery::Env
                            )
                        {
                            options.push(ContainerOption::AgentCredentials {
                                env_vars: credentials.env_vars,
                            });
                        }
                        let instance =
                            match crate::engine::agent_runtime::ResolvedAgentOptions::container(
                                options,
                            )
                            .and_then(|o| self.engines.runtime.build(o))
                            {
                                Ok(i) => i,
                                Err(e) => {
                                    frontend.write_message(UserMessage {
                                        level: MessageLevel::Error,
                                        text: format!("new skill: failed to build container: {e}"),
                                    });
                                    return Err(CommandError::from(e));
                                }
                            };
                        frontend.set_pty_active(true);
                        let cf = frontend.container_frontend_for_pty();
                        let mut execution = match instance.run_with_frontend(cf) {
                            Ok(e) => e,
                            Err(e) => {
                                frontend.set_pty_active(false);
                                frontend.replay_queued();
                                frontend.write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!("new skill: failed to run container: {e}"),
                                });
                                return Err(CommandError::from(e));
                            }
                        };
                        let _ = execution.wait().await;
                        frontend.set_pty_active(false);
                        frontend.replay_queued();
                    } else {
                        let body = frontend.ask_skill_body().unwrap_or_default();
                        let content = if body.is_empty() {
                            format!("# Skill: {name}\n\n## Description\n\n## Body\n")
                        } else {
                            format!("# Skill: {name}\n\n{body}\n")
                        };
                        let _ = std::fs::write(&path, content);
                    }

                    NewOutcome::Skill(NewSkillOutcome {
                        interview: f.interview,
                        global: f.global,
                        path: Some(path.display().to_string()),
                        pull: false,
                        libraries: Vec::new(),
                    })
                }
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

fn pull_success_message(outcome: &PullOutcome) -> UserMessage {
    UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "Pulled '{}' into {} ({} skill(s) found under {}/): {}",
            outcome.slug,
            outcome.dir.display(),
            outcome.skills_found.len(),
            outcome.subdir,
            outcome.skills_found.join(", ")
        ),
    }
}

fn pull_library_outcome(outcome: PullOutcome) -> PullLibraryOutcome {
    PullLibraryOutcome {
        slug: outcome.slug,
        dir: outcome.dir.display().to_string(),
        updated: outcome.was_update,
        skills_found: outcome.skills_found,
        error: None,
    }
}

fn next_work_item_number(dir: &std::path::Path) -> u32 {
    let mut max = 0u32;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.len() >= 5 && s.as_bytes()[4] == b'-' {
                if let Ok(n) = s[..4].parse::<u32>() {
                    if n > max {
                        max = n;
                    }
                }
            }
        }
    }
    max + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_work_item_number_empty_dir_is_one() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(next_work_item_number(tmp.path()), 1);
    }

    #[test]
    fn next_work_item_number_finds_max_number() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("0001-first.md"), "").unwrap();
        std::fs::write(tmp.path().join("0010-tenth.md"), "").unwrap();
        std::fs::write(tmp.path().join("0005-fifth.md"), "").unwrap();
        assert_eq!(next_work_item_number(tmp.path()), 11);
    }

    struct FakeNewFrontend {
        workflow_name: String,
        workflow_title: String,
        steps: Vec<WorkflowStepInput>,
        step_index: std::sync::atomic::AtomicUsize,
        skill_name: String,
        skill_body: String,
    }
    impl FakeNewFrontend {
        fn new(workflow: &str, skill: &str, body: &str) -> Self {
            Self {
                workflow_name: workflow.into(),
                workflow_title: workflow.into(),
                steps: vec![WorkflowStepInput {
                    name: "step-1".into(),
                    agent: None,
                    model: None,
                    prompt: "do something".into(),
                }],
                step_index: std::sync::atomic::AtomicUsize::new(0),
                skill_name: skill.into(),
                skill_body: body.into(),
            }
        }
        fn with_steps(mut self, title: &str, steps: Vec<WorkflowStepInput>) -> Self {
            self.workflow_title = title.into();
            self.steps = steps;
            self
        }
    }
    impl crate::data::message::UserMessageSink for FakeNewFrontend {
        fn write_message(&mut self, _: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl crate::command::commands::mount_scope::MountScopeFrontend for FakeNewFrontend {
        fn ask_mount_scope(
            &mut self,
            _git_root: &std::path::Path,
            _cwd: &std::path::Path,
        ) -> Result<
            crate::command::commands::mount_scope::MountScopeDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::mount_scope::MountScopeDecision::MountGitRoot)
        }
    }
    impl crate::command::commands::agent_setup::AgentSetupFrontend for FakeNewFrontend {
        fn ask_agent_setup(
            &mut self,
            _requested: &crate::data::session::AgentName,
            _default: &crate::data::session::AgentName,
            _default_available: bool,
            _image_only: bool,
        ) -> Result<
            crate::command::commands::agent_setup::AgentSetupDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::agent_setup::AgentSetupDecision::Setup)
        }
        fn record_fallback(
            &mut self,
            _requested: &crate::data::session::AgentName,
            _fallback: &crate::data::session::AgentName,
        ) {
        }
    }
    impl crate::command::commands::agent_auth::AgentAuthFrontend for FakeNewFrontend {
        fn ask_agent_auth_consent(
            &mut self,
            _agent: &crate::data::session::AgentName,
            _env_var_names: &[&str],
        ) -> Result<
            crate::command::commands::agent_auth::AgentAuthDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::agent_auth::AgentAuthDecision::DeclineOnce)
        }
    }
    impl crate::command::commands::specs::SpecsCommandFrontend for FakeNewFrontend {}
    impl NewCommandFrontend for FakeNewFrontend {
        fn ask_workflow_name(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.workflow_name.clone())
        }
        fn ask_workflow_title(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.workflow_title.clone())
        }
        fn ask_workflow_step_name(
            &mut self,
        ) -> Result<String, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].name.clone())
            } else {
                Ok(String::new())
            }
        }
        fn ask_workflow_step_agent(
            &mut self,
        ) -> Result<Option<String>, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].agent.clone())
            } else {
                Ok(None)
            }
        }
        fn ask_workflow_step_model(
            &mut self,
        ) -> Result<Option<String>, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].model.clone())
            } else {
                Ok(None)
            }
        }
        fn ask_workflow_step_prompt(
            &mut self,
        ) -> Result<String, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].prompt.clone())
            } else {
                Ok(String::new())
            }
        }
        fn ask_add_another_step(&mut self) -> Result<bool, crate::command::error::CommandError> {
            let idx = self
                .step_index
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            Ok(idx < self.steps.len())
        }
        fn ask_skill_name(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.skill_name.clone())
        }
        fn ask_skill_body(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.skill_body.clone())
        }
    }

    fn make_engines(root: &std::path::Path) -> Engines {
        use crate::data::fs::api_paths::ApiPaths;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::engine::container::ContainerRuntime;
        use crate::engine::overlay::OverlayEngine;
        use std::sync::Arc;
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(root),
        ));
        let runtime = Arc::new(ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            AuthPathResolver::at_home(root),
            ApiPaths::at_root(root),
        ));
        Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine: Arc::new(crate::engine::git::GitEngine::new()),
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store: Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                root,
            )),
        }
    }

    fn make_session(root: &std::path::Path) -> Session {
        let resolver = crate::data::session::StaticGitRootResolver::new(root);
        Session::open(
            root.to_path_buf(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn new_workflow_toml_writes_file_in_aspec_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let session = make_session(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "toml".into(),
            }),
            engines,
            session,
        );
        let fe = FakeNewFrontend::new("my-wf", "skill", "").with_steps(
            "My Workflow",
            vec![
                WorkflowStepInput {
                    name: "plan".into(),
                    agent: None,
                    model: None,
                    prompt: "Plan the work.".into(),
                },
                WorkflowStepInput {
                    name: "implement".into(),
                    agent: Some("codex".into()),
                    model: Some("claude-opus-4-7".into()),
                    prompt: "Do the work.".into(),
                },
            ],
        );
        let outcome = cmd.run_with_frontend(Box::new(fe)).await.unwrap();
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            let path = std::path::Path::new(&path_str);
            assert!(
                path_str.contains("aspec/workflows/"),
                "path must be under aspec/workflows/: {path_str}"
            );
            assert!(path.exists(), "workflow file must exist: {path_str}");
            let content = std::fs::read_to_string(path).unwrap();
            assert!(
                content.contains("[[step]]"),
                "TOML workflow must contain [[step]]: {content}"
            );
            assert!(content.contains("plan"), "must contain step name 'plan'");
            assert!(
                content.contains("codex"),
                "must contain agent 'codex': {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_workflow_yaml_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let session = make_session(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "yaml".into(),
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
            .await
            .unwrap();
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            assert!(
                path_str.ends_with(".yaml"),
                "path must have .yaml extension: {path_str}"
            );
            assert!(
                path_str.contains("aspec/workflows/"),
                "path must be under aspec/workflows/: {path_str}"
            );
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(
                content.contains("steps:"),
                "YAML workflow must contain steps key: {content}"
            );
            assert!(
                content.contains("step-1"),
                "must contain default step name: {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_workflow_toml_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let session = make_session(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "toml".into(),
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
            .await
            .unwrap();
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            assert!(
                path_str.ends_with(".toml"),
                "path must have .toml extension: {path_str}"
            );
            assert!(
                path_str.contains("aspec/workflows/"),
                "path must be under aspec/workflows/: {path_str}"
            );
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(
                content.contains("[[step]]"),
                "TOML workflow must contain [[step]]: {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_skill_writes_skill_md_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let session = make_session(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Skill(NewSkillFlags {
                interview: false,
                non_interactive: false,
                global: false,
                pull: None,
                pull_all: false,
                subdir: None,
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new(
                "wf",
                "my-skill",
                "Do something useful.",
            )))
            .await
            .unwrap();
        if let NewOutcome::Skill(s) = outcome {
            let path_str = s.path.expect("path must be Some");
            let path = std::path::Path::new(&path_str);
            assert!(path.exists(), "SKILL.md must exist: {path_str}");
            assert!(
                path.file_name().unwrap() == "SKILL.md",
                "file must be named SKILL.md"
            );
            let content = std::fs::read_to_string(path).unwrap();
            assert!(
                content.contains("my-skill"),
                "skill name must appear in SKILL.md"
            );
            assert!(
                content.contains("Do something useful."),
                "body must appear in SKILL.md"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_skill_empty_body_writes_default_skeleton() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let session = make_session(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Skill(NewSkillFlags {
                interview: false,
                non_interactive: false,
                global: false,
                pull: None,
                pull_all: false,
                subdir: None,
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new("wf", "my-skill", "")))
            .await
            .unwrap();
        if let NewOutcome::Skill(s) = outcome {
            let path_str = s.path.expect("path must be Some");
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(
                content.contains("## Body"),
                "empty-body skill must contain ## Body skeleton: {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }
}
