//! The WI-0092 leader/repair budget, shared by every caller that asks an agent
//! to design a `workflow.toml`.
//!
//! `exec workflow --dynamic` and the squad task evaluator both launch a
//! leader agent, validate whatever it wrote, and — on a validation failure —
//! relaunch it with a repair prompt carrying the verbatim error, up to three
//! times. That decision core is what lives here: the attempt budget, the
//! attempt label, the repair-prompt substitution, and the exhaustion message.
//!
//! What deliberately does *not* live here is how a leader is launched or
//! driven. `exec workflow --dynamic` drives an interactive container through
//! the stuck → yolo-countdown → control-board pipeline; squad runs one
//! unattended. Those are genuinely different, and folding them together would
//! buy nothing. Keeping the budget in one place is what stops the two callers
//! from disagreeing about what "3 repair attempts" means.

use std::path::{Path, PathBuf};

use crate::data::dynamic_workflow_assets::build_repair_prompt;
use crate::data::workflow_definition::Workflow;

/// What the loop decided after one leader attempt was validated.
#[derive(Debug)]
pub enum RepairDecision {
    /// The workflow validated; run it.
    Accepted(Box<Workflow>),
    /// Validation failed and budget remains: relaunch with
    /// [`WorkflowRepairLoop::prompt`], which now carries the repair prompt.
    Retry { attempt: usize, error: String },
    /// The budget is spent. The string is the final user-facing error.
    Exhausted(String),
}

/// The attempt budget for one leader/repair sequence.
pub struct WorkflowRepairLoop {
    generated_path: PathBuf,
    initial_prompt: String,
    current_prompt: String,
    attempt: usize,
}

impl WorkflowRepairLoop {
    /// The number of *repair* attempts allowed after the initial leader run.
    pub const MAX_REPAIR_ATTEMPTS: usize = 3;

    pub fn new(generated_path: impl Into<PathBuf>, initial_prompt: impl Into<String>) -> Self {
        let initial_prompt = initial_prompt.into();
        Self {
            generated_path: generated_path.into(),
            current_prompt: initial_prompt.clone(),
            initial_prompt,
            attempt: 0,
        }
    }

    /// The prompt for the next launch: the original leader prompt on attempt 0,
    /// a repair prompt carrying the last validation error afterwards.
    pub fn prompt(&self) -> &str {
        &self.current_prompt
    }

    /// How many repair attempts have been consumed so far.
    pub fn attempt(&self) -> usize {
        self.attempt
    }

    /// Whether the next launch is the first (original-prompt) one.
    pub fn is_first_attempt(&self) -> bool {
        self.attempt == 0
    }

    /// The label for the next launch, used for container/step naming.
    pub fn label(&self) -> String {
        if self.attempt == 0 {
            "leader".to_string()
        } else {
            format!("leader-repair-{}", self.attempt)
        }
    }

    /// Where the leader is expected to write its workflow.
    pub fn generated_path(&self) -> &Path {
        &self.generated_path
    }

    /// Discard any partial output and restart from the original prompt with a
    /// fresh budget. A user-requested restart is not a validation failure.
    pub fn restart(&mut self) {
        let _ = std::fs::remove_file(&self.generated_path);
        self.attempt = 0;
        self.current_prompt = self.initial_prompt.clone();
    }

    /// Record one attempt's validation result and decide what happens next.
    pub fn record(&mut self, validation: Result<Workflow, String>) -> RepairDecision {
        match validation {
            Ok(workflow) => RepairDecision::Accepted(Box::new(workflow)),
            Err(error) => {
                self.attempt += 1;
                if self.attempt > Self::MAX_REPAIR_ATTEMPTS {
                    return RepairDecision::Exhausted(format!(
                        "leader agent failed to produce a valid workflow.toml after {} repair \
                         attempts; last error: {error}; file is at {}",
                        Self::MAX_REPAIR_ATTEMPTS,
                        self.generated_path.display()
                    ));
                }
                self.current_prompt = build_repair_prompt(&error);
                RepairDecision::Retry {
                    attempt: self.attempt,
                    error,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow() -> Workflow {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("w.toml");
        std::fs::write(
            &path,
            "[[steps]]\nname = \"s\"\nagent = \"claude\"\nprompt = \"p\"\n",
        )
        .unwrap();
        Workflow::load(&path).unwrap()
    }

    #[test]
    fn first_attempt_uses_the_original_prompt_and_leader_label() {
        let loop_ = WorkflowRepairLoop::new("/tmp/workflow.toml", "original");
        assert!(loop_.is_first_attempt());
        assert_eq!(loop_.prompt(), "original");
        assert_eq!(loop_.label(), "leader");
    }

    #[test]
    fn a_validation_failure_switches_to_a_repair_prompt_carrying_the_error() {
        let mut loop_ = WorkflowRepairLoop::new("/tmp/workflow.toml", "original");
        match loop_.record(Err("missing agent 'nope'".into())) {
            RepairDecision::Retry { attempt, error } => {
                assert_eq!(attempt, 1);
                assert_eq!(error, "missing agent 'nope'");
            }
            other => panic!("expected retry, got {other:?}"),
        }
        assert!(!loop_.is_first_attempt());
        assert_eq!(loop_.label(), "leader-repair-1");
        assert!(
            loop_.prompt().contains("missing agent 'nope'"),
            "the repair prompt must carry the verbatim validation error: {}",
            loop_.prompt()
        );
    }

    #[test]
    fn the_budget_is_exactly_three_repair_attempts() {
        let mut loop_ = WorkflowRepairLoop::new("/tmp/workflow.toml", "original");
        for expected in 1..=WorkflowRepairLoop::MAX_REPAIR_ATTEMPTS {
            match loop_.record(Err("bad".into())) {
                RepairDecision::Retry { attempt, .. } => assert_eq!(attempt, expected),
                other => panic!("attempt {expected} must retry, got {other:?}"),
            }
        }
        match loop_.record(Err("still bad".into())) {
            RepairDecision::Exhausted(message) => {
                assert!(message.contains("3 repair attempts"), "{message}");
                assert!(message.contains("still bad"), "{message}");
                assert!(message.contains("/tmp/workflow.toml"), "{message}");
            }
            other => panic!("the fourth failure must exhaust the budget, got {other:?}"),
        }
    }

    #[test]
    fn a_restart_discards_the_partial_workflow_and_resets_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workflow.toml");
        std::fs::write(&path, "partial").unwrap();
        let mut loop_ = WorkflowRepairLoop::new(&path, "original");
        let _ = loop_.record(Err("bad".into()));
        assert_eq!(loop_.attempt(), 1);

        loop_.restart();

        assert!(
            !path.exists(),
            "a restart must discard the partial workflow"
        );
        assert!(loop_.is_first_attempt());
        assert_eq!(loop_.prompt(), "original");
        assert_eq!(loop_.label(), "leader");
    }

    #[test]
    fn a_valid_workflow_is_accepted_without_consuming_budget() {
        let mut loop_ = WorkflowRepairLoop::new("/tmp/workflow.toml", "original");
        match loop_.record(Ok(workflow())) {
            RepairDecision::Accepted(_) => {}
            other => panic!("expected acceptance, got {other:?}"),
        }
        assert_eq!(loop_.attempt(), 0);
    }
}
