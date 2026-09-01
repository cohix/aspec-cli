//! The per-run leader verdict file (Layer 1).
//!
//! A task's leader agent has to tell the daemon one thing every run: was the
//! task's triggering condition met *this run*? Before WI 0106 that answer was
//! inferred from whether `workflow.toml` existed in the task directory, which
//! only worked because the directory was wiped before every run. Now that the
//! task workspace is durable ([`Task`]'s workspace semantics), a `workflow.toml`
//! left behind by a run three evaluations ago is no evidence at all — and the
//! leader is explicitly allowed to *reuse* one rather than rewrite it.
//!
//! So the leader writes a small, mandatory, run-scoped file instead:
//!
//! ```text
//! ~/.awman/squad/tasks/<task>/runs/<run-id>/verdict.json
//! ```
//!
//! ```json
//! {"triggered": true, "reason": "3 new issues since the last run"}
//! ```
//!
//! It lives in the per-run directory, never in the durable workspace root, so
//! it can never be confused with the leader's own durable output and never
//! needs staleness cleanup: a fresh file in a fresh directory is inherently
//! unstale. A missing or unparseable verdict is a *broken run*, not a
//! legitimately-unmet condition — see [`read_verdict`].
//!
//! [`Task`]: crate::data::fs::Task

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The verdict file's name inside the run directory.
pub const VERDICT_FILE_NAME: &str = "verdict.json";

/// Where the run directory is mounted inside the leader's container.
///
/// Stable and documented so the leader prompt can name an absolute path the
/// agent can write without discovering anything.
pub const RUN_DIR_CONTAINER_PATH: &str = "/awman/squad/run";

/// The leader's verdict for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunVerdict {
    /// Whether the task's triggering condition was met on this run.
    pub triggered: bool,
    /// An optional short explanation, logged by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Why a run's verdict could not be read.
#[derive(Debug)]
pub enum VerdictError {
    /// The leader never wrote one: it ignored the protocol, crashed before
    /// writing, or its container was killed.
    Missing(PathBuf),
    /// A file exists but is not a verdict this build understands.
    Unparseable { path: PathBuf, reason: String },
}

impl std::fmt::Display for VerdictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(path) => write!(
                f,
                "the leader agent wrote no verdict file at {}: an evaluation that does not \
                 report a verdict is a failed run, not an unmet task",
                path.display()
            ),
            Self::Unparseable { path, reason } => write!(
                f,
                "the leader agent's verdict file at {} could not be read: {reason}",
                path.display()
            ),
        }
    }
}

/// The verdict file's host path for one run directory.
pub fn verdict_path(run_log_dir: &Path) -> PathBuf {
    run_log_dir.join(VERDICT_FILE_NAME)
}

/// Read the verdict the leader wrote for this run.
///
/// Deliberately has no "absent means not triggered" fallback: a missing or
/// malformed verdict returns an error, which the evaluator maps to a failed
/// run so the task backs off and alerts like any other evaluation failure,
/// rather than going silent forever.
pub fn read_verdict(run_log_dir: &Path) -> Result<RunVerdict, VerdictError> {
    let path = verdict_path(run_log_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(VerdictError::Missing(path));
        }
        Err(error) => {
            return Err(VerdictError::Unparseable {
                path,
                reason: error.to_string(),
            });
        }
    };
    serde_json::from_str(&raw).map_err(|error| VerdictError::Unparseable {
        path,
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_verdict_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(verdict_path(tmp.path()), r#"{"triggered": true}"#).unwrap();
        let verdict = read_verdict(tmp.path()).unwrap();
        assert!(verdict.triggered);
        assert_eq!(verdict.reason, None);
    }

    #[test]
    fn a_reason_is_optional_but_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            verdict_path(tmp.path()),
            r#"{"triggered": false, "reason": "no new issues"}"#,
        )
        .unwrap();
        let verdict = read_verdict(tmp.path()).unwrap();
        assert!(!verdict.triggered);
        assert_eq!(verdict.reason.as_deref(), Some("no new issues"));
    }

    #[test]
    fn a_missing_verdict_is_an_error_not_a_not_triggered_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_verdict(tmp.path()),
            Err(VerdictError::Missing(_))
        ));
    }

    #[test]
    fn an_unparseable_verdict_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(verdict_path(tmp.path()), "not json").unwrap();
        assert!(matches!(
            read_verdict(tmp.path()),
            Err(VerdictError::Unparseable { .. })
        ));
    }
}
