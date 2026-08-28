//! Layer 2 command dispatch tests (WI 0073).
//!
//! Tests `CommandCatalogue` completeness without starting engines or
//! containers. All tests pass under `make test-fast`.

#[path = "../helpers/mod.rs"]
mod helpers;

mod dispatch_real_engines;
mod skill_libraries;
