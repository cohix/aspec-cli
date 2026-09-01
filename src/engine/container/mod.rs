//! `engine::container` — `ContainerRuntime`, the container-class
//! `AgentRuntimeEngine` impl, and the typed `ContainerOption` enum.
//!
//! The Docker and Apple backends are `pub(super)` and their concrete types
//! are invisible to callers outside this module. All callers go through
//! `ContainerRuntime::build`.
//!
//! This module re-exports only container-paradigm-specific items.
//! Cross-paradigm types (`AgentInstance`, `AgentExecution`, `AgentFrontend`,
//! `AgentHandle`, …) live in `src/engine/agent_runtime/`.

mod apple;
mod attach_socket;
mod backend;
pub mod background;
pub mod display;
mod docker;
pub mod instance;
pub mod io_bridge;
pub mod naming;
pub mod options;
pub mod runtime;
pub mod timing;

pub use background::BackgroundContainer;
pub use instance::ContainerId;
pub use naming::generate_container_name;
pub use options::{
    AgentSettings, AutoMode, ContainerName, ContainerOption, CpuLimit, Entrypoint, EnvLiteral,
    EnvVar, ImageRef, MemoryLimit, ModelFlagForm, OverlayPermission, OverlaySpec, PlanMode,
    YoloMode,
};
pub use runtime::ContainerRuntime;
