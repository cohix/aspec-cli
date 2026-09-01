//! Shared squad attach discovery.
//!
//! Discovery is deliberately runtime-only: a daemon may enrich the labels a
//! caller displays, but it never decides which running containers exist.

use crate::command::error::CommandError;
use crate::data::session::AgentHandle;
use crate::data::workflow_state::{StepState, WorkflowState};
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::container::naming::SQUAD_NAME_PREFIX;

/// One running squad container, as presented to the user for disambiguation.
#[derive(Debug, Clone)]
pub struct SquadContainer {
    pub handle: AgentHandle,
    /// First 12 characters of the runtime handle ID.
    pub short_id: String,
    /// A workflow step name when known, otherwise the runtime container name.
    pub label: String,
}

/// The non-guessing result of attach target selection.
#[derive(Debug)]
pub enum AttachResolution {
    One(SquadContainer),
    Ambiguous(Vec<SquadContainer>),
}

/// The running-name prefix for one squad task.
pub fn squad_name_prefix(task: &str) -> String {
    format!("{SQUAD_NAME_PREFIX}{task}-")
}

/// Discover running containers for one task through the cross-runtime
/// trait. This is the authoritative candidate list.
pub fn list_task_containers(
    runtime: &dyn AgentRuntimeEngine,
    task: &str,
) -> Result<Vec<SquadContainer>, CommandError> {
    Ok(runtime
        .list_running_with_name_prefix(&squad_name_prefix(task))?
        .into_iter()
        .map(|handle| SquadContainer {
            short_id: handle.id.chars().take(12).collect(),
            label: handle.name.clone(),
            handle,
        })
        .collect())
}

/// Best-effort presentation enrichment from a daemon workflow snapshot.
///
/// This never changes the candidate set or its order.
pub fn label_with_step_names(candidates: &mut [SquadContainer], state: &WorkflowState) {
    for (step_name, step_state) in &state.step_states {
        let StepState::Running {
            container_id: Some(container_id),
        } = step_state
        else {
            continue;
        };
        for candidate in candidates.iter_mut() {
            // The engine records the container *name* as the step's id, while
            // runtime discovery reports real runtime ids — accept either, so
            // step labels actually resolve.
            if candidate.handle.id == container_id.as_str()
                || candidate.handle.name == container_id.as_str()
            {
                candidate.label = step_name.clone();
            }
        }
    }
}

/// Resolve a candidate without guessing.
pub fn resolve_attach_target(
    candidates: Vec<SquadContainer>,
    task: &str,
    requested: Option<&str>,
) -> Result<AttachResolution, CommandError> {
    if candidates.is_empty() {
        return Err(no_run_in_progress(task));
    }

    if let Some(requested) = requested {
        let matches: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                candidate.handle.name == requested
                    || candidate.short_id.starts_with(requested)
                    || requested.starts_with(&candidate.short_id)
            })
            .collect();
        return match matches.as_slice() {
            [candidate] => Ok(AttachResolution::One((*candidate).clone())),
            _ => Err(not_in_task(task, requested, &candidates)),
        };
    }

    match candidates.len() {
        1 => Ok(AttachResolution::One(
            candidates.into_iter().next().expect("one candidate"),
        )),
        _ => Ok(AttachResolution::Ambiguous(candidates)),
    }
}

/// The explicit idle-task error shape shared by both frontends.
pub fn no_run_in_progress(task: &str) -> CommandError {
    CommandError::Other(format!("no run currently in progress for task {task:?}"))
}

/// An explicit `--container` is never allowed to escape the discovered set.
pub fn not_in_task(task: &str, requested: &str, set: &[SquadContainer]) -> CommandError {
    let legal = if set.is_empty() {
        "(none)".to_string()
    } else {
        set.iter()
            .map(|candidate| candidate.short_id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    CommandError::Other(format!(
        "container {requested:?} is not a running container for task {task:?}; legal containers: {legal}"
    ))
}

/// One line per candidate for a caller's ambiguity error.
pub fn format_candidates(candidates: &[SquadContainer]) -> String {
    candidates
        .iter()
        .map(|candidate| format!("{}  {}", candidate.short_id, candidate.label))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, name: &str) -> SquadContainer {
        SquadContainer {
            short_id: id.chars().take(12).collect(),
            label: name.to_string(),
            handle: AgentHandle {
                id: id.to_string(),
                image_tag: "img".to_string(),
                name: name.to_string(),
                started_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            },
        }
    }

    /// The workflow engine publishes the container *name* as the step's
    /// `container_id`, while runtime discovery reports real runtime ids —
    /// labeling must resolve on either, or step names never appear.
    #[test]
    fn step_labels_resolve_when_the_daemon_publishes_a_container_name() {
        use crate::data::workflow_state::{StepState, WorkflowState};
        let mut candidates = vec![candidate("aaaa1111bbbb", "awman-squad-t-00000001")];
        let mut state = WorkflowState::new("wf".to_string(), &[], "hash".to_string(), None);
        state.step_states.insert(
            "build".to_string(),
            StepState::Running {
                container_id: Some("awman-squad-t-00000001".to_string()),
            },
        );
        label_with_step_names(&mut candidates, &state);
        assert_eq!(candidates[0].label, "build");
    }

    #[test]
    fn zero_candidates_fails_fast_with_no_run_in_progress() {
        let error = resolve_attach_target(Vec::new(), "issue-triage", None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("no run currently in progress"), "{text}");
        assert!(text.contains("issue-triage"), "{text}");
    }

    #[test]
    fn one_candidate_resolves_to_it() {
        let candidates = vec![candidate("abc123def456", "leader")];
        match resolve_attach_target(candidates, "c", None).unwrap() {
            AttachResolution::One(container) => assert_eq!(container.handle.id, "abc123def456"),
            other => panic!("expected One, got {other:?}"),
        }
    }

    #[test]
    fn two_candidates_without_a_selector_are_ambiguous_and_attach_nothing() {
        let candidates = vec![
            candidate("aaaa1111", "step-a"),
            candidate("bbbb2222", "step-b"),
        ];
        match resolve_attach_target(candidates, "c", None).unwrap() {
            AttachResolution::Ambiguous(both) => assert_eq!(both.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn a_container_selector_outside_the_set_is_rejected_and_never_attaches() {
        let candidates = vec![
            candidate("aaaa1111", "step-a"),
            candidate("bbbb2222", "step-b"),
        ];
        let error =
            resolve_attach_target(candidates, "issue-triage", Some("cccc3333")).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("not a running container"), "{text}");
        // The legal set is listed so the user can choose one.
        assert!(
            text.contains("aaaa1111") && text.contains("bbbb2222"),
            "{text}"
        );
    }

    #[test]
    fn a_valid_container_selector_resolves_to_its_single_match() {
        let candidates = vec![
            candidate("aaaa1111", "step-a"),
            candidate("bbbb2222", "step-b"),
        ];
        match resolve_attach_target(candidates, "c", Some("bbbb2222")).unwrap() {
            AttachResolution::One(container) => assert_eq!(container.handle.id, "bbbb2222"),
            other => panic!("expected One, got {other:?}"),
        }
    }
}
