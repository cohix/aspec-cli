//! Runtime-tier admission for squad.

use crate::command::dispatch::Engines;
use crate::command::error::CommandError;

/// The common refusal text used at daemon startup and task creation.
pub const SQUAD_SANDBOX_REFUSAL: &str = "squad requires a container runtime. The configured runtime \"{runtime}\" cannot mount squad's task directories or run workflow setup/teardown steps. Set runtime to \"docker\" or \"apple-containers\" to use squad.";

/// Admit only container-class runtimes. This deliberately keys off the tier,
/// not capability flags: Docker and Apple Containers share the tier while the
/// sandbox's advertised capability set is not a reliable admission signal.
pub fn require_container_tier(engines: &Engines) -> Result<(), CommandError> {
    if engines.container_runtime.is_some() {
        return Ok(());
    }
    Err(CommandError::Other(
        SQUAD_SANDBOX_REFUSAL.replace("{runtime}", engines.runtime.runtime_name()),
    ))
}
