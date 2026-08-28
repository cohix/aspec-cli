//! `CommandCatalogue` — the canonical, single-source-of-truth enumeration of
//! every awman command, subcommand, argument, and flag.
//!
//! Frontends never hard-code command names or flag names; they ask the
//! catalogue (or its projections) for what's available. The catalogue MUST
//! enumerate every command currently defined in `oldsrc/cli.rs` exactly.

use std::sync::OnceLock;

/// Visibility of a command/flag across frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendVisibility {
    /// Visible to every frontend (CLI, TUI, API).
    All,
    /// CLI-only (e.g. API server start).
    CliOnly,
    /// TUI-only (e.g. tab annotations).
    TuiOnly,
    /// CLI + TUI (e.g. interactive Q&A toggles).
    CliAndTui,
    /// Hidden (no frontend exposes it).
    Hidden,
}

/// The kind of value a flag accepts.
#[derive(Debug, Clone, Copy)]
pub enum FlagKind {
    /// `--foo` (presence-only).
    Bool,
    /// `--foo NAME` required string.
    String,
    /// `--foo NAME` optional string.
    OptionalString,
    /// `--foo NAME` from a fixed set of values.
    Enum(&'static [&'static str]),
    /// Repeatable string flag (`--foo a --foo b`).
    VecString,
    /// `--foo PATH` optional path.
    Path,
    /// `--foo PATH` optional path.
    OptionalPath,
    /// `--foo N` u16 number.
    U16,
    /// `--foo N` usize number, must be >= 1.
    UsizeAtLeastOne,
}

/// Default value for a flag.
#[derive(Debug, Clone, Copy)]
pub enum FlagDefault {
    None,
    Bool(bool),
    Str(&'static str),
    U16(u16),
    EmptyVec,
}

/// Spec for a single named flag.
#[derive(Debug, Clone, Copy)]
pub struct FlagSpec {
    pub long: &'static str,
    pub short: Option<char>,
    pub help: &'static str,
    pub kind: FlagKind,
    pub default: FlagDefault,
    pub frontends: FrontendVisibility,
    /// Other flags this flag is mutually exclusive with.
    pub conflicts_with: &'static [&'static str],
    /// Other flags this flag implies (sets to true / forwards value).
    pub implies: &'static [&'static str],
    /// `false` = required; `true` = optional.
    pub optional: bool,
}

impl FlagSpec {
    pub fn conflicts_with(&self, other: &str) -> bool {
        self.conflicts_with.contains(&other)
    }
}

/// Spec for a flag that once existed but has since been removed. Frontends
/// scan raw argv for these *before* clap parses, so a user who passes a
/// retired flag sees a migration hint instead of clap's generic
/// "unexpected argument" error. Keeping the retired-flag knowledge here — next
/// to the live [`FlagSpec`]s — means future removals never touch `main.rs`.
#[derive(Debug, Clone, Copy)]
pub struct RemovedFlagSpec {
    /// The retired long flag, leading dashes included (e.g. `--mount-ssh`).
    /// Matches both the bare form and the `--flag=value` form.
    pub name: &'static str,
    /// Migration guidance appended after "`<name>` has been removed.".
    pub hint: &'static str,
}

/// The kind of an argument (positional value).
#[derive(Debug, Clone, Copy)]
pub enum ArgumentKind {
    String,
    OptionalString,
    Path,
    OptionalPath,
    /// `<COMMAND>...` style: collect every remaining token verbatim,
    /// including hyphen-prefixed values, into a single argument.
    TrailingVarArgs,
}

#[derive(Debug, Clone, Copy)]
pub struct ArgumentSpec {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: ArgumentKind,
    pub optional: bool,
}

/// Which frontend kinds are allowed to invoke a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendKind {
    Cli,
    Tui,
    Api,
}

/// Spec for one command (or subcommand) in the catalogue.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    /// Aliases (string only, e.g. `"wf"` for `exec workflow`).
    pub aliases: &'static [&'static str],
    pub help: &'static str,
    pub long_help: Option<&'static str>,
    pub arguments: &'static [ArgumentSpec],
    pub flags: &'static [FlagSpec],
    pub subcommands: &'static [&'static CommandSpec],
    /// Whether this command can be invoked via the API frontend.
    /// Only `exec workflow` and `exec prompt` have this set to `true`.
    pub api_allowed: bool,
}

impl CommandSpec {
    pub fn find_subcommand(&self, name: &str) -> Option<&'static CommandSpec> {
        for sub in self.subcommands {
            if sub.name == name || sub.aliases.contains(&name) {
                return Some(*sub);
            }
        }
        None
    }

    pub fn find_flag(&self, name: &str) -> Option<&'static FlagSpec> {
        self.flags.iter().find(|f| f.long == name)
    }
}

// ─── Top-level catalogue ─────────────────────────────────────────────────────

pub struct CommandCatalogue {
    root: &'static CommandSpec,
    /// Path aliases: pairs of (alias_path, canonical_path). When the user
    /// invokes `alias_path`, dispatch resolves `canonical_path` instead.
    path_aliases: &'static [(&'static [&'static str], &'static [&'static str])],
}

static CATALOGUE: OnceLock<CommandCatalogue> = OnceLock::new();

impl CommandCatalogue {
    /// Borrow the lazily-built singleton.
    pub fn get() -> &'static CommandCatalogue {
        CATALOGUE.get_or_init(|| CommandCatalogue {
            root: &ROOT,
            path_aliases: PATH_ALIASES,
        })
    }

    pub fn root(&self) -> &'static CommandSpec {
        self.root
    }

    pub fn path_aliases(&self) -> &'static [(&'static [&'static str], &'static [&'static str])] {
        self.path_aliases
    }

    /// Walk a path of names, returning the matching `CommandSpec` if any.
    pub fn lookup(&self, path: &[&str]) -> Option<&'static CommandSpec> {
        let mut current = self.root;
        for segment in path {
            current = current.find_subcommand(segment)?;
        }
        Some(current)
    }

    /// Returns `true` if the given command path is allowed for the given
    /// frontend kind. Session management routes are always allowed; only
    /// command execution routes are restricted.
    pub fn is_allowed_for_frontend(&self, frontend: FrontendKind, path: &[&str]) -> bool {
        match frontend {
            FrontendKind::Cli | FrontendKind::Tui => true,
            FrontendKind::Api => {
                let canonical = self.canonical_path(path);
                if let Some(spec) = self.lookup(&canonical) {
                    spec.api_allowed
                } else {
                    false
                }
            }
        }
    }

    /// Same as `lookup`, but first applies any registered path alias rewrites.
    pub fn lookup_with_aliases(&self, path: &[&str]) -> Option<&'static CommandSpec> {
        let canonical = self.canonical_path(path);
        self.lookup(&canonical)
    }

    /// Whether a command path needs a successfully-detected agent runtime to
    /// run. `config` is the recovery path when `GlobalConfig::runtime` names
    /// a runtime that cannot be constructed on this host (e.g.
    /// `apple-containers` on Linux): it only reads/writes config files, so it
    /// must stay reachable to let the user switch the runtime back. Every
    /// other command — and the bare-TUI invocation — conservatively requires
    /// the runtime.
    pub fn requires_runtime(&self, path: &[&str]) -> bool {
        !matches!(path.first(), Some(&"config"))
    }

    /// Scan a raw argv for any [removed flag](RemovedFlagSpec). Returns the
    /// composed migration message ("`<flag>` has been removed. <hint>") for the
    /// first removed flag found, or `None` when argv contains none. Frontends
    /// call this before clap parsing so a retired flag surfaces the hint
    /// instead of clap's generic "unexpected argument" error. Both the bare
    /// `--flag` and the `--flag=value` forms are matched.
    pub fn removed_flag_hint<I, S>(&self, args: I) -> Option<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for arg in args {
            let arg = arg.as_ref();
            for spec in REMOVED_FLAGS {
                if arg == spec.name || arg.starts_with(&format!("{}=", spec.name)) {
                    return Some(format!("{} has been removed. {}", spec.name, spec.hint));
                }
            }
        }
        None
    }

    /// Validate that a command path is reachable by the given frontend,
    /// returning `Err(CommandError::NotAvailableForFrontend)` when blocked.
    pub fn validate_for_frontend(
        &self,
        frontend: FrontendKind,
        path: &[&str],
    ) -> Result<(), crate::command::error::CommandError> {
        if self.is_allowed_for_frontend(frontend, path) {
            Ok(())
        } else {
            let command = path.join(" ");
            let frontend_name = match frontend {
                FrontendKind::Cli => "cli",
                FrontendKind::Tui => "tui",
                FrontendKind::Api => "api",
            };
            Err(
                crate::command::error::CommandError::NotAvailableForFrontend {
                    command,
                    frontend: frontend_name.to_string(),
                },
            )
        }
    }

    /// Return all command paths where `api_allowed == true` as
    /// (parent_name, subcommand_name) pairs. Only immediate (leaf)
    /// api-allowed specs are returned; the root is never included.
    pub fn api_allowed_commands(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        self.collect_api_allowed_rec(self.root, &mut out);
        out
    }

    fn collect_api_allowed_rec(
        &self,
        node: &'static CommandSpec,
        out: &mut Vec<(&'static str, &'static str)>,
    ) {
        for sub in node.subcommands {
            if sub.api_allowed {
                out.push((node.name, sub.name));
            }
            self.collect_api_allowed_rec(sub, out);
        }
    }

    /// Apply path-alias rewrites to a user-supplied path. Returns the
    /// canonical path or the input path unchanged.
    pub fn canonical_path(&self, path: &[&str]) -> Vec<&'static str> {
        // First check registered aliases.
        for (alias, canonical) in self.path_aliases {
            if alias.len() == path.len() && alias.iter().zip(path).all(|(a, b)| *a == *b) {
                return canonical.to_vec();
            }
        }
        // Otherwise the path is canonical; we still need 'static strings.
        // Look up each segment against the catalogue and use the catalogue's
        // 'static reference for the matched subcommand name.
        let mut current = self.root;
        let mut out: Vec<&'static str> = Vec::with_capacity(path.len());
        for segment in path {
            match current.find_subcommand(segment) {
                Some(sub) => {
                    out.push(sub.name);
                    current = sub;
                }
                None => {
                    // Unknown segment — append it verbatim so the caller can
                    // surface an UnknownCommand error that names the bad token.
                    out.push(Box::leak(segment.to_string().into_boxed_str()));
                    return out;
                }
            }
        }
        out
    }
}

// ─── Static catalogue data ───────────────────────────────────────────────────

const ROOT: CommandSpec = CommandSpec {
    name: "awman",
    aliases: &[],
    help: "awman — containerized code agent manager",
    long_help: None,
    arguments: &[],
    api_allowed: false,
    flags: &[
        FlagSpec {
            long: "build",
            short: None,
            help: "Force rebuild of images on startup",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "no-cache",
            short: None,
            help: "Disable Docker layer cache during builds",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "refresh",
            short: None,
            help: "Refresh agent environment (run audit)",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[
        &INIT,
        &READY,
        &CHAT,
        &SPECS,
        &STATUS,
        &CONFIG,
        &EXEC,
        &API_SERVER,
        &AMIE,
        &REMOTE,
        &NEW,
        &CLEAN,
    ],
};

const PATH_ALIASES: &[(&[&str], &[&str])] = &[];

/// Flags that have been removed. Scanned by [`CommandCatalogue::removed_flag_hint`]
/// before clap parsing so retired flags yield a migration hint. Add an entry
/// here when a flag is dropped; `main.rs` needs no changes.
const REMOVED_FLAGS: &[RemovedFlagSpec] = &[
    // WI-0082: `--mount-ssh` was removed in favour of `--overlay ssh()`.
    RemovedFlagSpec {
        name: "--mount-ssh",
        hint: "Pass `--overlay ssh()` instead (or set `overlays = [\"ssh()\"]` \
               in a per-step workflow entry). See `docs/09-overlays.md`.",
    },
];

// ── clean ─────────────────────────────────────────────────────────────────────

const CLEAN: CommandSpec = CommandSpec {
    name: "clean",
    aliases: &[],
    help: "Remove stopped awman containers, completed workflow data, and dangling images.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "yes",
            short: Some('y'),
            help: "Skip the confirmation prompt (for scripting).",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "dry-run",
            short: None,
            help: "List what would be removed without deleting anything.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    // Blocked at the catalogue layer for the API frontend; never reaches
    // command dispatch via the API.
    api_allowed: false,
    subcommands: &[],
};

// ── init ─────────────────────────────────────────────────────────────────────

const AGENT_VALUES: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "maki",
    "gemini",
    "copilot",
    "crush",
    "cline",
    "antigravity",
];

const INIT: CommandSpec = CommandSpec {
    name: "init",
    aliases: &[],
    help: "Initialize the current Git repo for use with awman.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "agent",
            short: None,
            help: "Code agent to install in the Dockerfile.dev container.",
            kind: FlagKind::Enum(AGENT_VALUES),
            default: FlagDefault::Str("claude"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "aspec",
            short: None,
            help: "Download aspec templates to the current project.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

// ── ready ────────────────────────────────────────────────────────────────────

const READY: CommandSpec = CommandSpec {
    name: "ready",
    aliases: &[],
    help: "Check Docker daemon, verify Dockerfile.dev, build image, and report status.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "refresh",
            short: None,
            help: "Run the Dockerfile agent audit (skipped by default).",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "build",
            short: None,
            help: "Force rebuild the dev container image from Dockerfile.dev.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "no-cache",
            short: None,
            help: "Pass --no-cache to docker build.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the agent in non-interactive (print) mode instead of interactive mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "allow-docker",
            short: None,
            help: "Mount the host Docker daemon socket into the agent container.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "json",
            short: None,
            help: "Suppress human output and print structured JSON. Implies --non-interactive.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &["non-interactive"],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

// ── chat ─────────────────────────────────────────────────────────────────────

const CHAT: CommandSpec = CommandSpec {
    name: "chat",
    aliases: &[],
    help: "Start a freeform chat session with the configured agent in a container.",
    long_help: None,
    arguments: &[],
    flags: &AGENT_RUN_FLAGS_NO_WORKTREE,
    api_allowed: false,
    subcommands: &[],
};

// ── specs ───────────────────────────────────────────────────────────────────

const SPECS: CommandSpec = CommandSpec {
    name: "specs",
    aliases: &[],
    help: "Manage work item specs (amend).",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&SPECS_AMEND],
};

const SPECS_AMEND: CommandSpec = CommandSpec {
    name: "amend",
    aliases: &[],
    help: "Review and amend a completed work item to match the final implementation.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "work_item",
        help: "Work item number (e.g. 0025).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "allow-docker",
            short: None,
            help: "Mount the host Docker daemon socket into the agent container.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

// ── status ───────────────────────────────────────────────────────────────────

const STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show the status of all running code-agent containers.",
    long_help: None,
    arguments: &[],
    flags: &[FlagSpec {
        long: "watch",
        short: None,
        help: "Continuously refresh the output every 3 seconds.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: false,
    subcommands: &[],
};

// ── config ───────────────────────────────────────────────────────────────────

const CONFIG: CommandSpec = CommandSpec {
    name: "config",
    aliases: &[],
    help: "View and edit global and repo configuration.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&CONFIG_SHOW, &CONFIG_GET, &CONFIG_SET],
};

const CONFIG_SHOW: CommandSpec = CommandSpec {
    name: "show",
    aliases: &[],
    help: "Display all config fields at both global and repo level.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[],
};

const CONFIG_GET: CommandSpec = CommandSpec {
    name: "get",
    aliases: &[],
    help: "Show a single field's global value, repo value, and effective value.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "field",
        help: "Config field name (e.g. terminal_scrollback_lines).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[],
    api_allowed: false,
    subcommands: &[],
};

const CONFIG_SET: CommandSpec = CommandSpec {
    name: "set",
    aliases: &[],
    help: "Set a config field value (repo scope by default).",
    long_help: None,
    arguments: &[
        ArgumentSpec {
            name: "field",
            help: "Config field name.",
            kind: ArgumentKind::String,
            optional: false,
        },
        ArgumentSpec {
            name: "value",
            help: "New value for the field.",
            kind: ArgumentKind::String,
            optional: false,
        },
    ],
    flags: &[FlagSpec {
        long: "global",
        short: None,
        help: "Write to global config instead of repo config.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: false,
    subcommands: &[],
};

// ── exec ────────────────────────────────────────────────────────────────────

const EXEC: CommandSpec = CommandSpec {
    name: "exec",
    aliases: &[],
    help: "Run a one-shot command: inject a prompt or run a workflow without a work item.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&EXEC_PROMPT, &EXEC_WORKFLOW],
};

const EXEC_PROMPT: CommandSpec = CommandSpec {
    name: "prompt",
    aliases: &[],
    help: "Send a one-shot prompt to the agent.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "prompt",
        // Greedy trailing positional: every remaining token joins into one
        // prompt string. Declaring it here keeps the "join positionals with
        // spaces" behavior spec-driven across all frontends instead of a
        // per-frontend special case (work item 0097, Finding A).
        help: "The prompt text to send to the agent.",
        kind: ArgumentKind::TrailingVarArgs,
        optional: true,
    }],
    flags: &EXEC_PROMPT_FLAGS,
    api_allowed: true,
    subcommands: &[],
};

const EXEC_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &["wf"],
    help: "Run a workflow file without requiring a work item number.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "workflow",
        // Optional at the catalogue level so `--dynamic` can omit it; the
        // command layer still requires it for every non-dynamic invocation.
        help: "Path to the workflow file (omit with --dynamic).",
        kind: ArgumentKind::Path,
        optional: true,
    }],
    flags: &EXEC_WORKFLOW_FLAGS,
    api_allowed: true,
    subcommands: &[],
};

// ── api ────────────────────────────────────────────────────────────────

const API_SERVER: CommandSpec = CommandSpec {
    name: "api",
    aliases: &[],
    help: "Run awman as an API HTTP server for remote/automated access.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[
        &API_SERVER_START,
        &API_SERVER_KILL,
        &API_SERVER_LOGS,
        &API_SERVER_STATUS,
    ],
};

const API_SERVER_START: CommandSpec = CommandSpec {
    name: "start",
    aliases: &[],
    help: "Start the API HTTP server.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "port",
            short: None,
            help: "Port to listen on.",
            kind: FlagKind::U16,
            default: FlagDefault::U16(9876),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "workdirs",
            short: None,
            help: "Allowlisted working directories (repeatable).",
            kind: FlagKind::VecString,
            default: FlagDefault::EmptyVec,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "background",
            short: None,
            help: "Daemonize via the OS process manager.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "refresh-key",
            short: None,
            help: "Regenerate the API key.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "dangerously-skip-auth",
            short: None,
            help: "Disable authentication for this execution even if a key hash exists on disk.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "dangerously-skip-tls",
            short: None,
            help: "Serve plain HTTP instead of HTTPS. Intended for localhost/test only.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

const API_SERVER_KILL: CommandSpec = CommandSpec {
    name: "kill",
    aliases: &[],
    help: "Stop the background API server.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[],
};

const API_SERVER_LOGS: CommandSpec = CommandSpec {
    name: "logs",
    aliases: &[],
    help: "Stream the background server log file to stdout.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[],
};

const API_SERVER_STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show API server status.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[],
};

// ── amie ────────────────────────────────────────────────────────────────────

const AMIE: CommandSpec = CommandSpec {
    name: "amie",
    aliases: &[],
    help: "Manage the amie condition daemon and scheduled conditions.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Print the amie status summary instead of opening the TUI.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliAndTui,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "json",
            short: None,
            help: "Emit JSON output.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &["non-interactive"],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[
        &AMIE_START,
        &AMIE_STOP,
        &AMIE_STATUS,
        &AMIE_LOGS,
        &AMIE_ADD,
        &AMIE_LIST,
        &AMIE_SHOW,
        &AMIE_REMOVE,
        &AMIE_PAUSE,
        &AMIE_RESUME,
        &AMIE_ATTACH,
    ],
};

const AMIE_START: CommandSpec = CommandSpec {
    name: "start",
    aliases: &[],
    help: "Start the amie daemon.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "port",
            short: None,
            help: "Port to listen on (0 selects an OS-assigned port).",
            kind: FlagKind::U16,
            default: FlagDefault::U16(0),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "background",
            short: None,
            help: "Daemonize via the OS process manager.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "refresh-key",
            short: None,
            help: "Regenerate the amie daemon API key.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "dangerously-skip-auth",
            short: None,
            help: "Disable authentication for this execution.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: true,
    subcommands: &[],
};

const AMIE_STOP: CommandSpec = CommandSpec {
    name: "stop",
    aliases: &["kill"],
    help: "Stop the amie daemon.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: true,
    subcommands: &[],
};

const AMIE_STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show amie daemon status.",
    long_help: None,
    arguments: &[],
    flags: &[FlagSpec {
        long: "json",
        short: None,
        help: "Emit JSON output.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: true,
    subcommands: &[],
};

const AMIE_LOGS: CommandSpec = CommandSpec {
    name: "logs",
    aliases: &[],
    help: "Show the amie daemon log.",
    long_help: None,
    arguments: &[],
    flags: &[FlagSpec {
        long: "follow",
        short: Some('f'),
        help: "Follow the log as it grows.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: true,
    subcommands: &[],
};

const AMIE_ADD: CommandSpec = CommandSpec {
    name: "add",
    aliases: &[],
    help: "Create an amie condition.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "name",
            short: None,
            help: "Condition slug.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: false,
        },
        FlagSpec {
            long: "description",
            short: None,
            help: "Condition description.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: false,
        },
        FlagSpec {
            long: "repo",
            short: None,
            help: "Repository directory to evaluate.",
            kind: FlagKind::Path,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "interval",
            short: None,
            help: "Evaluation interval (for example 5m).",
            kind: FlagKind::String,
            default: FlagDefault::Str("5m"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "agent",
            short: None,
            help: "Condition-specific leader agent.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "model",
            short: None,
            help: "Condition-specific leader model.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "mount-scope",
            short: None,
            help: "Repository scope mounted for scheduled runs.",
            kind: FlagKind::Enum(&["cwd", "gitroot"]),
            default: FlagDefault::Str("gitroot"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "interview",
            short: None,
            help: "Collect condition fields interactively.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliAndTui,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Do not prompt for missing fields.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliAndTui,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: true,
    subcommands: &[],
};

const AMIE_LIST: CommandSpec = CommandSpec {
    name: "list",
    aliases: &[],
    help: "List amie conditions.",
    long_help: None,
    arguments: &[],
    flags: &[FlagSpec {
        long: "json",
        short: None,
        help: "Emit JSON output.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: true,
    subcommands: &[],
};

const AMIE_NAME_ARGUMENT: ArgumentSpec = ArgumentSpec {
    name: "name",
    help: "Condition name.",
    kind: ArgumentKind::String,
    optional: false,
};

const AMIE_SHOW: CommandSpec = CommandSpec {
    name: "show",
    aliases: &[],
    help: "Show an amie condition.",
    long_help: None,
    arguments: &[AMIE_NAME_ARGUMENT],
    flags: &[FlagSpec {
        long: "json",
        short: None,
        help: "Emit JSON output.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: true,
    subcommands: &[],
};
const AMIE_REMOVE: CommandSpec = CommandSpec {
    name: "remove",
    aliases: &[],
    help: "Remove an amie condition.",
    long_help: None,
    arguments: &[AMIE_NAME_ARGUMENT],
    flags: &[FlagSpec {
        long: "yes",
        short: Some('y'),
        help: "Do not prompt for confirmation.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::CliAndTui,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: true,
    subcommands: &[],
};
const AMIE_PAUSE: CommandSpec = CommandSpec {
    name: "pause",
    aliases: &[],
    help: "Pause an amie condition.",
    long_help: None,
    arguments: &[AMIE_NAME_ARGUMENT],
    flags: &[],
    api_allowed: true,
    subcommands: &[],
};
const AMIE_RESUME: CommandSpec = CommandSpec {
    name: "resume",
    aliases: &[],
    help: "Resume an amie condition.",
    long_help: None,
    arguments: &[AMIE_NAME_ARGUMENT],
    flags: &[],
    api_allowed: true,
    subcommands: &[],
};
const AMIE_ATTACH: CommandSpec = CommandSpec {
    name: "attach",
    aliases: &[],
    help: "Attach to a running amie condition container.",
    long_help: None,
    arguments: &[AMIE_NAME_ARGUMENT],
    flags: &[FlagSpec {
        long: "container",
        short: None,
        help: "Running container ID when multiple are active.",
        kind: FlagKind::String,
        default: FlagDefault::None,
        frontends: FrontendVisibility::CliAndTui,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: false,
    subcommands: &[],
};

// ── remote ──────────────────────────────────────────────────────────────────

const REMOTE: CommandSpec = CommandSpec {
    name: "remote",
    aliases: &[],
    help: "Connect to a remote awman API instance and execute commands.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&REMOTE_SESSION, &REMOTE_EXEC],
};

// ── remote exec ─────────────────────────────────────────────────────────────

const REMOTE_EXEC: CommandSpec = CommandSpec {
    name: "exec",
    aliases: &[],
    help: "Execute a command on the remote awman API host.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&REMOTE_EXEC_WORKFLOW, &REMOTE_EXEC_PROMPT],
};

const REMOTE_EXEC_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &["wf"],
    help: "Submit a workflow for execution on the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "workflow",
        help: "Path to the workflow file.",
        kind: ArgumentKind::Path,
        optional: false,
    }],
    flags: &REMOTE_EXEC_WORKFLOW_FLAGS,
    api_allowed: false,
    subcommands: &[],
};

const REMOTE_EXEC_PROMPT: CommandSpec = CommandSpec {
    name: "prompt",
    aliases: &[],
    help: "Send a one-shot prompt to the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "prompt",
        // Greedy trailing positional (see EXEC_PROMPT): joins remaining tokens
        // into one prompt string, spec-driven for every frontend.
        help: "The prompt text to send to the agent.",
        kind: ArgumentKind::TrailingVarArgs,
        optional: false,
    }],
    flags: &REMOTE_EXEC_PROMPT_FLAGS,
    api_allowed: false,
    subcommands: &[],
};

// ─── Programmatic derivation of remote exec flag sets ────────────────────────
//
// Per the work item: `remote exec workflow` accepts the same flags as the
// local `exec workflow`, minus flags that make no sense remotely (`--workdir`
// is implicit, `--worktree` is a server-side concern). Plus remote-transport
// flags (--remote-addr, --session, --api-key, --follow).
//
// The flag list is built at compile time by const fn so that any future
// addition to AGENT_RUN_FLAGS_NO_WORKTREE / EXEC_WORKFLOW_FLAGS is picked up
// automatically — no manual list maintenance.

const REMOTE_EXEC_EXCLUDED_FLAG_NAMES: &[&str] = &["workdir", "worktree"];

const REMOTE_TRANSPORT_FLAGS: [FlagSpec; 4] = [
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "session",
        short: None,
        help: "Session ID to use on the remote host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "follow",
        short: Some('f'),
        help: "Stream logs via SSE until the command completes.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn const_str_in_list(needle: &str, haystack: &[&str]) -> bool {
    let mut i = 0;
    while i < haystack.len() {
        if const_str_eq(haystack[i], needle) {
            return true;
        }
        i += 1;
    }
    false
}

const fn count_kept(base: &[FlagSpec], excluded: &[&str]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < base.len() {
        if !const_str_in_list(base[i].long, excluded) {
            count += 1;
        }
        i += 1;
    }
    count
}

const REMOTE_EXEC_WORKFLOW_KEPT: usize =
    count_kept(&EXEC_WORKFLOW_FLAGS, REMOTE_EXEC_EXCLUDED_FLAG_NAMES);
const REMOTE_EXEC_WORKFLOW_TOTAL: usize = REMOTE_TRANSPORT_FLAGS.len() + REMOTE_EXEC_WORKFLOW_KEPT;

const REMOTE_EXEC_PROMPT_KEPT: usize =
    count_kept(&EXEC_PROMPT_FLAGS, REMOTE_EXEC_EXCLUDED_FLAG_NAMES);
const REMOTE_EXEC_PROMPT_TOTAL: usize = REMOTE_TRANSPORT_FLAGS.len() + REMOTE_EXEC_PROMPT_KEPT;

const fn build_remote_flags<const N: usize>(base: &[FlagSpec], excluded: &[&str]) -> [FlagSpec; N] {
    let mut out: [FlagSpec; N] = [REMOTE_TRANSPORT_FLAGS[0]; N];
    let mut idx = 0;
    let mut i = 0;
    while i < REMOTE_TRANSPORT_FLAGS.len() {
        out[idx] = REMOTE_TRANSPORT_FLAGS[i];
        idx += 1;
        i += 1;
    }
    let mut j = 0;
    while j < base.len() {
        if !const_str_in_list(base[j].long, excluded) {
            out[idx] = base[j];
            idx += 1;
        }
        j += 1;
    }
    out
}

const REMOTE_EXEC_WORKFLOW_FLAGS: [FlagSpec; REMOTE_EXEC_WORKFLOW_TOTAL] =
    build_remote_flags::<REMOTE_EXEC_WORKFLOW_TOTAL>(
        &EXEC_WORKFLOW_FLAGS,
        REMOTE_EXEC_EXCLUDED_FLAG_NAMES,
    );

const REMOTE_EXEC_PROMPT_FLAGS: [FlagSpec; REMOTE_EXEC_PROMPT_TOTAL] =
    build_remote_flags::<REMOTE_EXEC_PROMPT_TOTAL>(
        &EXEC_PROMPT_FLAGS,
        REMOTE_EXEC_EXCLUDED_FLAG_NAMES,
    );

// ── remote session ──────────────────────────────────────────────────────────

const REMOTE_SESSION: CommandSpec = CommandSpec {
    name: "session",
    aliases: &[],
    help: "Manage sessions on the remote awman API host.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&REMOTE_SESSION_START, &REMOTE_SESSION_KILL],
};

const REMOTE_SESSION_START: CommandSpec = CommandSpec {
    name: "start",
    aliases: &[],
    help: "Start a new session on the remote host.",
    long_help: None,
    arguments: &[],
    flags: &REMOTE_SESSION_START_FLAGS,
    api_allowed: false,
    subcommands: &[],
};

const REMOTE_SESSION_START_FLAGS: [FlagSpec; 6] = [
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "type",
        short: None,
        help: "Session type: 'local' or 'remote'.",
        kind: FlagKind::Enum(&["local", "remote"]),
        default: FlagDefault::Str("local"),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "workdir",
        short: None,
        help: "Working directory (required for --type local).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "repo-url",
        short: None,
        help: "Repository URL (required for --type remote).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "branch",
        short: None,
        help: "Branch name (optional, for --type remote).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const REMOTE_SESSION_KILL: CommandSpec = CommandSpec {
    name: "kill",
    aliases: &[],
    help: "Kill a session on the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "session_id",
        help: "Session ID to kill.",
        kind: ArgumentKind::OptionalString,
        optional: true,
    }],
    flags: &REMOTE_SESSION_KILL_FLAGS,
    api_allowed: false,
    subcommands: &[],
};

const REMOTE_SESSION_KILL_FLAGS: [FlagSpec; 2] = [
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

// ── new ─────────────────────────────────────────────────────────────────────

const NEW: CommandSpec = CommandSpec {
    name: "new",
    aliases: &[],
    help: "Create a new awman artefact (spec, workflow, or skill).",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    subcommands: &[&NEW_SPEC, &NEW_WORKFLOW, &NEW_SKILL],
};

const NEW_SPEC: CommandSpec = CommandSpec {
    name: "spec",
    aliases: &[],
    help: "Create a new work item spec.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "interview",
            short: None,
            help: "Use interview mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the interview agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "issue",
            short: None,
            help: "GitHub issue number, URL, or owner/repo#N to use as spec input.",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

const WORKFLOW_FORMAT_VALUES: &[&str] = &["toml", "yaml"];

const NEW_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &[],
    help: "Interactively create a new workflow file.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "interview",
            short: None,
            help: "Let a code agent complete the workflow from a summary you provide.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the interview agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "global",
            short: None,
            help: "Write to ~/.awman/workflows/<name> instead of the current repo.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "format",
            short: None,
            help: "Output file format.",
            kind: FlagKind::Enum(WORKFLOW_FORMAT_VALUES),
            default: FlagDefault::Str("toml"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

const NEW_SKILL: CommandSpec = CommandSpec {
    name: "skill",
    aliases: &[],
    help: "Interactively create a new skill file.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "interview",
            short: None,
            help: "Let a code agent complete the skill body from a summary you provide.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the interview agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "global",
            short: None,
            help: "Write to ~/.awman/skills/<name>/ instead of the current repo.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "pull",
            short: None,
            help: "Pull (or refresh) a published skills library from GitHub, e.g. github.com/obra/superpowers.",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &["pull-all", "interview", "global"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "pull-all",
            short: None,
            help: "Refresh every previously-pulled skills library.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &["pull", "subdir", "interview", "global"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "subdir",
            short: None,
            help: "Subdirectory inside the pulled repo containing skills (default: skills).",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &["pull-all"],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    subcommands: &[],
};

// ─── Reusable agent-run flag arrays ─────────────────────────────────────────

/// Agent-run flag set used by `chat` and `exec prompt` (no worktree, no
/// workflow). All optional. Mode flags `yolo` / `auto` / `plan` are mutually
/// exclusive.
const AGENT_RUN_FLAGS_NO_WORKTREE: [FlagSpec; 8] = [
    FlagSpec {
        long: "non-interactive",
        short: Some('n'),
        help: "Run the agent in non-interactive (print) mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "plan",
        short: None,
        help: "Run the agent in plan mode (read-only).",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["yolo"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "allow-docker",
        short: None,
        help: "Mount the host Docker daemon socket into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "yolo",
        short: None,
        help: "Enable fully autonomous mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["plan"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "auto",
        short: None,
        help: "Enable auto permission mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "agent",
        short: None,
        help: "Agent to use (overrides .awman/config.json).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "model",
        short: None,
        help: "Override the model used by the launched agent.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "overlay",
        short: None,
        help: "Mount a host directory into the agent container. Repeatable.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

/// Agent-run flags for `exec prompt` — extends `AGENT_RUN_FLAGS_NO_WORKTREE`
/// with `--issue`. Scoped to `exec prompt` only; `chat` retains the base set.
const EXEC_PROMPT_FLAGS: [FlagSpec; 9] = [
    FlagSpec {
        long: "non-interactive",
        short: Some('n'),
        help: "Run the agent in non-interactive (print) mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "plan",
        short: None,
        help: "Run the agent in plan mode (read-only).",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["yolo"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "allow-docker",
        short: None,
        help: "Mount the host Docker daemon socket into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "yolo",
        short: None,
        help: "Enable fully autonomous mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["plan"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "auto",
        short: None,
        help: "Enable auto permission mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "agent",
        short: None,
        help: "Agent to use (overrides .awman/config.json).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "model",
        short: None,
        help: "Override the model used by the launched agent.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "overlay",
        short: None,
        help: "Mount a host directory into the agent container. Repeatable.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "issue",
        short: None,
        help: "GitHub issue number, URL, or owner/repo#N to use as the prompt.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const EXEC_WORKFLOW_FLAGS: [FlagSpec; 14] = [
    FlagSpec {
        long: "work-item",
        short: None,
        help: "Optional work item number.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &["issue"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "non-interactive",
        short: Some('n'),
        help: "Run the agent in non-interactive (print) mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "plan",
        short: None,
        help: "Run the agent in plan mode (read-only).",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["yolo"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "allow-docker",
        short: None,
        help: "Mount the host Docker daemon socket into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "worktree",
        short: None,
        help: "Run in an isolated Git worktree under ~/.awman/worktrees/.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "yolo",
        short: None,
        help: "Enable fully autonomous mode. Implies --worktree.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["plan"],
        implies: &["worktree"],
        optional: true,
    },
    FlagSpec {
        long: "auto",
        short: None,
        help: "Enable auto permission mode. Implies --worktree.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &["worktree"],
        optional: true,
    },
    FlagSpec {
        long: "agent",
        short: None,
        help: "Agent to use.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "model",
        short: None,
        help: "Override the model used by the launched agent.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "overlay",
        short: None,
        help: "Mount a host directory into the agent container. Repeatable.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "issue",
        short: None,
        help: "GitHub issue number, URL, or owner/repo#N to use as work item input.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &["work-item"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "dynamic",
        short: None,
        help: "Have a leader agent design and run a workflow for --work-item. \
               Implies --yolo, --worktree, and context(workflow); the positional \
               workflow path must be omitted.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        // Mutual exclusions (positional path, --plan) and the --work-item
        // requirement are enforced in the command layer because --yolo may be
        // implied rather than explicitly supplied (WI-0092 §3).
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "leader",
        short: None,
        help: "Agent and model for the dynamic leader, as agent::model \
               (e.g. claude::claude-opus-4-8). Only valid with --dynamic.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "max-concurrent",
        short: None,
        help: "Cap on concurrently-running workflow steps (must be >= 1).",
        kind: FlagKind::UsizeAtLeastOne,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_top_level_returns_spec() {
        let cat = CommandCatalogue::get();
        let spec = cat.lookup(&["init"]).expect("init must be present");
        assert_eq!(spec.name, "init");
    }

    #[test]
    fn lookup_nested_returns_spec() {
        let cat = CommandCatalogue::get();
        let spec = cat
            .lookup(&["exec", "workflow"])
            .expect("exec workflow must be present");
        assert_eq!(spec.name, "workflow");
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let cat = CommandCatalogue::get();
        assert!(cat.lookup(&["bogus"]).is_none());
        assert!(cat.lookup(&["init", "bogus"]).is_none());
    }

    #[test]
    fn string_alias_wf_resolves_to_workflow() {
        let cat = CommandCatalogue::get();
        let spec = cat.lookup(&["exec", "wf"]).unwrap();
        assert_eq!(spec.name, "workflow");
    }

    #[test]
    fn ready_json_implies_non_interactive() {
        let cat = CommandCatalogue::get();
        let ready = cat.lookup(&["ready"]).unwrap();
        let json_flag = ready.find_flag("json").unwrap();
        assert!(json_flag.implies.contains(&"non-interactive"));
    }

    #[test]
    fn exec_workflow_yolo_implies_worktree() {
        let cat = CommandCatalogue::get();
        let exec_workflow = cat.lookup(&["exec", "workflow"]).unwrap();
        let yolo = exec_workflow.find_flag("yolo").unwrap();
        assert!(yolo.implies.contains(&"worktree"));
    }

    #[test]
    fn exec_workflow_auto_implies_worktree() {
        let cat = CommandCatalogue::get();
        let exec_workflow = cat.lookup(&["exec", "workflow"]).unwrap();
        let auto = exec_workflow.find_flag("auto").unwrap();
        assert!(auto.implies.contains(&"worktree"));
    }

    #[test]
    fn plan_and_yolo_are_mutually_exclusive_on_chat() {
        let cat = CommandCatalogue::get();
        let chat = cat.lookup(&["chat"]).unwrap();
        let plan = chat.find_flag("plan").unwrap();
        assert!(plan.conflicts_with("yolo"));
        let yolo = chat.find_flag("yolo").unwrap();
        assert!(yolo.conflicts_with("plan"));
    }

    #[test]
    fn every_top_level_command_is_present() {
        let cat = CommandCatalogue::get();
        for name in [
            "init", "ready", "chat", "specs", "status", "config", "exec", "api", "remote", "new",
        ] {
            assert!(cat.lookup(&[name]).is_some(), "missing top-level '{name}'");
        }
    }

    #[test]
    fn remote_exec_workflow_has_workflow_argument() {
        let cat = CommandCatalogue::get();
        let wf = cat.lookup(&["remote", "exec", "workflow"]).unwrap();
        assert_eq!(wf.arguments.len(), 1);
        assert_eq!(wf.arguments[0].name, "workflow");
        assert!(matches!(wf.arguments[0].kind, ArgumentKind::Path));
    }

    #[test]
    fn remote_exec_prompt_has_prompt_argument() {
        let cat = CommandCatalogue::get();
        let prompt = cat.lookup(&["remote", "exec", "prompt"]).unwrap();
        assert_eq!(prompt.arguments.len(), 1);
        assert_eq!(prompt.arguments[0].name, "prompt");
        // Greedy trailing positional so multi-word prompts join spec-driven.
        assert!(matches!(
            prompt.arguments[0].kind,
            ArgumentKind::TrailingVarArgs
        ));
    }

    // ─── Data-table tests ─────────────────────────────────────────────────────

    /// Compact check for a single flag: path, flag name, whether it is a Bool,
    /// and whether it is optional.  The `bool_expected` field avoids PartialEq
    /// on `FlagKind` (which contains `&'static [&'static str]` slices).
    struct FlagCheck {
        path: &'static [&'static str],
        flag: &'static str,
        is_bool: bool,
        is_optional: bool,
    }

    const FLAG_TABLE: &[FlagCheck] = &[
        FlagCheck {
            path: &["init"],
            flag: "agent",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["init"],
            flag: "aspec",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "refresh",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "build",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "no-cache",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "non-interactive",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "allow-docker",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "json",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "non-interactive",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "plan",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "yolo",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "auto",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "allow-docker",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "agent",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "model",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "overlay",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "yolo",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "auto",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "worktree",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "work-item",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "plan",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "prompt"],
            flag: "yolo",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "prompt"],
            flag: "overlay",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["status"],
            flag: "watch",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["config", "set"],
            flag: "global",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["api", "start"],
            flag: "port",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["api", "start"],
            flag: "workdirs",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["api", "start"],
            flag: "background",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["api", "start"],
            flag: "refresh-key",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["api", "start"],
            flag: "dangerously-skip-auth",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["api", "start"],
            flag: "dangerously-skip-tls",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "exec", "workflow"],
            flag: "follow",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "exec", "workflow"],
            flag: "api-key",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "exec", "workflow"],
            flag: "remote-addr",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "session", "start"],
            flag: "api-key",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "session", "kill"],
            flag: "remote-addr",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "workflow"],
            flag: "format",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "workflow"],
            flag: "interview",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "workflow"],
            flag: "global",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "skill"],
            flag: "interview",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "skill"],
            flag: "global",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "spec"],
            flag: "interview",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["specs", "amend"],
            flag: "non-interactive",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["specs", "amend"],
            flag: "allow-docker",
            is_bool: true,
            is_optional: true,
        },
    ];

    #[test]
    fn all_documented_flags_present_with_correct_kind_and_optional() {
        let cat = CommandCatalogue::get();
        for case in FLAG_TABLE {
            let spec = cat
                .lookup(case.path)
                .unwrap_or_else(|| panic!("command {:?} not found in catalogue", case.path));
            let flag = spec
                .find_flag(case.flag)
                .unwrap_or_else(|| panic!("flag '{}' not found on {:?}", case.flag, case.path));
            assert_eq!(
                flag.optional, case.is_optional,
                "optional mismatch for '{}' on {:?}",
                case.flag, case.path
            );
            assert_eq!(
                matches!(flag.kind, FlagKind::Bool),
                case.is_bool,
                "is_bool mismatch for '{}' on {:?}",
                case.flag,
                case.path
            );
        }
    }

    #[test]
    fn all_expected_subcommands_are_present() {
        let cat = CommandCatalogue::get();
        let cases: &[(&[&str], &str)] = &[
            (&["specs"], "amend"),
            (&["config"], "show"),
            (&["config"], "get"),
            (&["config"], "set"),
            (&["exec"], "prompt"),
            (&["exec"], "workflow"),
            (&["api"], "start"),
            (&["api"], "kill"),
            (&["api"], "logs"),
            (&["api"], "status"),
            (&["remote"], "exec"),
            (&["remote"], "session"),
            (&["remote", "exec"], "workflow"),
            (&["remote", "exec"], "prompt"),
            (&["remote", "session"], "start"),
            (&["remote", "session"], "kill"),
            (&["new"], "spec"),
            (&["new"], "workflow"),
            (&["new"], "skill"),
        ];
        for (parent_path, subcmd_name) in cases {
            let parent = cat
                .lookup(parent_path)
                .unwrap_or_else(|| panic!("parent {:?} not found", parent_path));
            assert!(
                parent.find_subcommand(subcmd_name).is_some(),
                "subcommand '{}' not found under {:?}",
                subcmd_name,
                parent_path
            );
        }
    }

    #[test]
    fn config_commands_do_not_require_runtime() {
        let cat = CommandCatalogue::get();
        for path in [
            &["config"][..],
            &["config", "show"],
            &["config", "get"],
            &["config", "set"],
        ] {
            assert!(
                !cat.requires_runtime(path),
                "{path:?} must not require a runtime — it is the recovery \
                 path for a broken runtime config"
            );
        }
        // Everything else — and the bare TUI invocation (empty path) —
        // conservatively requires a detected runtime.
        for path in [
            &["chat"][..],
            &["ready"],
            &["init"],
            &["status"],
            &["exec", "workflow"],
            &[],
        ] {
            assert!(cat.requires_runtime(path), "{path:?} must require runtime");
        }
    }

    #[test]
    fn flag_spec_conflicts_with_accessor_is_symmetric_on_chat() {
        let cat = CommandCatalogue::get();
        let chat = cat.lookup(&["chat"]).unwrap();
        let plan = chat.find_flag("plan").unwrap();
        let yolo = chat.find_flag("yolo").unwrap();
        assert!(plan.conflicts_with("yolo"), "plan must conflict with yolo");
        assert!(yolo.conflicts_with("plan"), "yolo must conflict with plan");
        assert!(
            !plan.conflicts_with("non-interactive"),
            "plan must NOT conflict with non-interactive"
        );
    }

    #[test]
    fn api_start_flags_are_cli_only() {
        let cat = CommandCatalogue::get();
        let start = cat.lookup(&["api", "start"]).unwrap();
        for flag in start.flags {
            assert!(
                matches!(flag.frontends, FrontendVisibility::CliOnly),
                "api start flag '{}' must be CliOnly, got {:?}",
                flag.long,
                flag.frontends
            );
        }
    }

    #[test]
    fn exec_workflow_arguments_include_workflow_path() {
        let cat = CommandCatalogue::get();
        let wf = cat.lookup(&["exec", "workflow"]).unwrap();
        assert_eq!(wf.arguments.len(), 1);
        assert_eq!(wf.arguments[0].name, "workflow");
        assert!(matches!(wf.arguments[0].kind, ArgumentKind::Path));
    }

    #[test]
    fn config_get_and_set_have_required_field_argument() {
        let cat = CommandCatalogue::get();
        let get = cat.lookup(&["config", "get"]).unwrap();
        assert_eq!(get.arguments.len(), 1);
        assert_eq!(get.arguments[0].name, "field");
        assert!(!get.arguments[0].optional);

        let set = cat.lookup(&["config", "set"]).unwrap();
        assert_eq!(set.arguments.len(), 2);
        let names: Vec<&str> = set.arguments.iter().map(|a| a.name).collect();
        assert!(names.contains(&"field") && names.contains(&"value"));
    }

    // ── WI-0098 Finding B: removed-flag migration hints ───────────────────────

    #[test]
    fn removed_flag_hint_returns_hint_for_mount_ssh() {
        let cat = CommandCatalogue::get();
        let hint = cat
            .removed_flag_hint(["chat", "--mount-ssh"])
            .expect("--mount-ssh must yield a migration hint");
        assert!(
            hint.starts_with("--mount-ssh has been removed."),
            "hint must name the removed flag; got: {hint}"
        );
        assert!(
            hint.contains("ssh()") || hint.contains("--overlay"),
            "hint must point at the `--overlay ssh()` replacement; got: {hint}"
        );
    }

    #[test]
    fn removed_flag_hint_matches_value_form() {
        let cat = CommandCatalogue::get();
        // `--mount-ssh=x` (the `=`-bearing form) must be intercepted too.
        let hint = cat
            .removed_flag_hint(["chat", "--mount-ssh=x"])
            .expect("--mount-ssh=x must yield the same migration hint");
        assert!(hint.starts_with("--mount-ssh has been removed."));
    }

    #[test]
    fn removed_flag_hint_none_for_live_flags() {
        let cat = CommandCatalogue::get();
        // Live flags and near-misses must not trigger a removed-flag hint.
        assert!(cat
            .removed_flag_hint(["chat", "--overlay", "ssh()"])
            .is_none());
        assert!(cat
            .removed_flag_hint(["chat", "--non-interactive"])
            .is_none());
        // A flag that merely contains the removed name as a substring must not match.
        assert!(cat
            .removed_flag_hint(["chat", "--mount-ssh-extra"])
            .is_none());
        assert!(cat.removed_flag_hint(Vec::<String>::new()).is_none());
    }
}
