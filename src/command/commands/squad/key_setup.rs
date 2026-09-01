//! The one-time "here is your squad key" snippet.
//!
//! The squad daemon has always minted a bearer key on its first start, but the
//! key was printed as a bare line with no indication of how the CLI or TUI were
//! meant to present it back. This module renders the missing half: the exported
//! environment variable ([`AWMAN_SQUAD_KEY`]) and the shell startup file to put
//! it in, so a first run leaves the user with a working client rather than a
//! secret they cannot spend.
//!
//! Rendering is a pure function of (key, shell) so the wording is unit-testable
//! and the caller decides where the text goes — stdout for the CLI, the squad
//! tab's message area for the TUI.

use crate::data::config::env::{EnvSnapshot, AWMAN_SQUAD_KEY};

/// A user's login shell, insofar as it changes the snippet we print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlavor {
    Zsh,
    Bash,
    Fish,
    /// Anything else, including an unset `$SHELL`. The snippet stays correct by
    /// naming a POSIX `export` and hedging on the file, rather than guessing.
    Unknown,
}

impl ShellFlavor {
    /// Classify from a `$SHELL` value such as `/bin/zsh`. Matching is on the
    /// trailing path component so `/opt/homebrew/bin/fish` resolves too.
    pub fn from_shell_path(shell: Option<&str>) -> Self {
        let Some(shell) = shell.filter(|s| !s.is_empty()) else {
            return Self::Unknown;
        };
        let name = shell.rsplit('/').next().unwrap_or(shell);
        match name {
            "zsh" => Self::Zsh,
            "bash" | "sh" => Self::Bash,
            "fish" => Self::Fish,
            _ => Self::Unknown,
        }
    }

    /// Read `$SHELL` from a captured environment snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Self {
        Self::from_shell_path(env.shell())
    }

    /// The startup file the export belongs in, as displayed to the user.
    fn rc_file(self) -> &'static str {
        match self {
            Self::Zsh => "~/.zshrc",
            Self::Bash => "~/.bashrc",
            Self::Fish => "~/.config/fish/config.fish",
            // Named as an example, not as a fact, when we do not know the shell.
            Self::Unknown => "your shell's startup file (~/.zshrc, ~/.bashrc, …)",
        }
    }

    /// The export statement itself. fish has no `export`.
    fn export_line(self, key: &str) -> String {
        match self {
            Self::Fish => format!("set -gx {AWMAN_SQUAD_KEY} {key}"),
            _ => format!("export {AWMAN_SQUAD_KEY}={key}"),
        }
    }
}

/// The box-drawn key banner plus the shell snippet that makes the key usable.
///
/// Printed exactly once, by whichever process mints the key — never by the
/// detached daemon child, whose stdout is a log file the key must not reach.
pub fn render_key_setup(key: &str, shell: ShellFlavor) -> String {
    let mut out = String::new();
    out.push_str(&render_banner(key));
    out.push_str("\n\n");
    out.push_str(&format!(
        "Add this to {} so the awman CLI and TUI can authenticate to squad:\n\n    {}\n\n",
        shell.rc_file(),
        shell.export_line(key)
    ));
    out.push_str(
        "Until you do, export it in the current shell — `awman squad` commands\n\
         without the key are refused by the daemon with 401 Unauthorized.\n\n",
    );
    out.push_str(
        "Prefer to run without a key? Stop the daemon and start it with\n    \
         awman squad start --dangerously-skip-auth\n\
         which mints no key and accepts unauthenticated requests. squad binds to\n\
         loopback (127.0.0.1) only, so nothing off this machine can reach it.",
    );
    out
}

/// The key on its own, boxed, matching the API server's first-run banner style.
fn render_banner(key: &str) -> String {
    let title = "squad API key (store this — it will not be shown again)";
    // Width follows the longer of title and key so a key of any length fits.
    let inner = title.chars().count().max(key.chars().count()) + 4;
    let bar = "═".repeat(inner);
    let pad = |text: &str| {
        let used = text.chars().count() + 2;
        format!("  {text}{}", " ".repeat(inner.saturating_sub(used)))
    };
    format!("╔{bar}╗\n║{}║\n║{}║\n╚{bar}╝", pad(title), pad(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_flavor_reads_the_trailing_path_component() {
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/bin/zsh")),
            ShellFlavor::Zsh
        );
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/bin/bash")),
            ShellFlavor::Bash
        );
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/opt/homebrew/bin/fish")),
            ShellFlavor::Fish
        );
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/usr/bin/nu")),
            ShellFlavor::Unknown
        );
    }

    #[test]
    fn shell_flavor_of_an_unset_or_empty_shell_is_unknown() {
        assert_eq!(ShellFlavor::from_shell_path(None), ShellFlavor::Unknown);
        assert_eq!(ShellFlavor::from_shell_path(Some("")), ShellFlavor::Unknown);
    }

    #[test]
    fn snippet_exports_the_documented_env_var_with_the_key() {
        let out = render_key_setup("deadbeef", ShellFlavor::Zsh);
        assert!(
            out.contains("export AWMAN_SQUAD_KEY=deadbeef"),
            "snippet must be copy-pasteable; got:\n{out}"
        );
        assert!(
            out.contains("~/.zshrc"),
            "zsh users get ~/.zshrc; got:\n{out}"
        );
    }

    #[test]
    fn fish_gets_set_gx_rather_than_export() {
        let out = render_key_setup("deadbeef", ShellFlavor::Fish);
        assert!(
            out.contains("set -gx AWMAN_SQUAD_KEY deadbeef"),
            "fish has no `export`; got:\n{out}"
        );
        assert!(!out.contains("export AWMAN_SQUAD_KEY"), "got:\n{out}");
        assert!(out.contains("config.fish"), "got:\n{out}");
    }

    #[test]
    fn unknown_shell_still_yields_a_posix_export() {
        let out = render_key_setup("deadbeef", ShellFlavor::Unknown);
        assert!(
            out.contains("export AWMAN_SQUAD_KEY=deadbeef"),
            "got:\n{out}"
        );
    }

    #[test]
    fn snippet_names_the_skip_auth_alternative() {
        let out = render_key_setup("deadbeef", ShellFlavor::Zsh);
        assert!(
            out.contains("--dangerously-skip-auth"),
            "the no-auth escape hatch must be discoverable here; got:\n{out}"
        );
    }

    #[test]
    fn banner_boxes_a_key_longer_than_the_title() {
        let key = "a".repeat(120);
        let out = render_key_setup(&key, ShellFlavor::Zsh);
        assert!(out.contains(&key), "banner must not truncate the key");
        // Every box line is the same display width as the top border.
        let lines: Vec<&str> = out.lines().take(4).collect();
        let width = lines[0].chars().count();
        for line in &lines[1..4] {
            assert_eq!(line.chars().count(), width, "misaligned box line: {line}");
        }
    }
}
