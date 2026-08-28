//! Layer 1: engine
//!
//! Built on top of Layer 0 (`src/data/`). Exposes typed objects that own
//! every concern Layer 2 commands need to compose: container runtime,
//! workflow execution, git operations, overlays, auth, agent management,
//! and the multi-phase `ready`/`init` engines.
//!
//! No upward calls. When an engine needs user I/O, it accepts a frontend
//! trait *defined here* and Layer 3 implements it.

pub mod agent;
pub mod agent_runtime;
pub mod amie;
pub mod auth;
pub mod container;
pub mod context_prompt;
pub mod error;
pub mod git;
pub mod init;
pub mod overlay;
pub mod ready;
pub mod sandbox;
pub mod step_status;
pub mod workflow;

pub use error::EngineError;
pub use step_status::StepStatus;
