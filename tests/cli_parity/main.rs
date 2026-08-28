//! CLI parity tests (WI 0073).
//!
//! catalogue_completeness.rs — catalogue correctness checks (no subprocess).
//! json_outputs.rs — verifies JSON flags exist in the catalogue.
//!
//! Subprocess-based tests (help_text, exit codes) include "binary_smoke" in
//! their name and live in tests/binary_smoke/ instead.

#[path = "../helpers/mod.rs"]
mod helpers;

mod amie_catalogue;
mod catalogue_completeness;
mod json_outputs;
