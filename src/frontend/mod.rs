//! Layer 3 — frontends.
//!
//! Three independent implementations consume `Dispatch` (Layer 2),
//! `SessionManager` (Layer 0), and the per-command frontend traits
//! (Layers 1 + 2):
//!
//! - [`cli`]    — argv-driven, stdout/stderr/stdin rendering.
//! - [`tui`]    — Ratatui-based interactive terminal UI.
//! - [`api`] — HTTP server for programmatic / remote access.
//!
//! Frontends contain NO business logic; every behavioral decision lives in
//! Layer 2.

pub mod amie;
pub mod api;
pub mod cli;
pub mod tui;

use std::io::IsTerminal;

/// Resolve the effective non-interactive flag.
/// Returns true when the caller explicitly requested it OR when stdin
/// is not a TTY (headless environment, HTTP server, CI/CD pipeline).
pub fn effective_non_interactive(explicitly_requested: bool) -> bool {
    let no_tty = !std::io::stdin().is_terminal();
    if !explicitly_requested && no_tty {
        tracing::info!("auto-detected non-interactive mode (no TTY on stdin)");
    }
    explicitly_requested || no_tty
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When `explicitly_requested` is true, the function must return true
    /// unconditionally — regardless of whether stdin is a TTY or not.
    #[test]
    fn effective_non_interactive_explicit_true_always_returns_true() {
        assert!(
            effective_non_interactive(true),
            "explicit non-interactive flag must always win"
        );
    }

    /// In `cargo test` stdin is never a TTY (piped), so the auto-detection
    /// path must return true even when the explicit flag is false.
    #[test]
    fn effective_non_interactive_false_in_headless_test_env() {
        // cargo test always runs with stdin piped — not a TTY.
        assert!(
            effective_non_interactive(false),
            "in cargo test (no TTY on stdin) effective_non_interactive(false) must be true"
        );
    }

    /// Calling the function twice with the same argument returns the same
    /// result (no hidden mutable state).
    #[test]
    fn effective_non_interactive_is_idempotent() {
        let first = effective_non_interactive(true);
        let second = effective_non_interactive(true);
        assert_eq!(first, second);

        let first = effective_non_interactive(false);
        let second = effective_non_interactive(false);
        assert_eq!(first, second);
    }
}
