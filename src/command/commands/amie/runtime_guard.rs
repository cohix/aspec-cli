//! Runtime-tier admission for amie.

use crate::command::dispatch::Engines;
use crate::command::error::CommandError;

/// The common refusal text used at daemon startup and condition creation.
pub const AMIE_SANDBOX_REFUSAL: &str = "amie requires a container runtime. The configured runtime \"{runtime}\" cannot mount amie's condition directories or run workflow setup/teardown steps. Set runtime to \"docker\" or \"apple-containers\" to use amie.";

/// Admit only container-class runtimes. This deliberately keys off the tier,
/// not capability flags: Docker and Apple Containers share the tier while the
/// sandbox's advertised capability set is not a reliable admission signal.
pub fn require_container_tier(engines: &Engines) -> Result<(), CommandError> {
    if engines.container_runtime.is_some() {
        return Ok(());
    }
    Err(CommandError::Other(
        AMIE_SANDBOX_REFUSAL.replace("{runtime}", engines.runtime.runtime_name()),
    ))
}
