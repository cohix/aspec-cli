//! amie — the always-on condition scheduler (Layer 1).
//!
//! This module owns the *engine* half of amie's evaluation machinery:
//!
//! * [`AmieScheduler`] — the 30s tick loop that selects due conditions
//!   (wholly in SQL, via [`ConditionStore::due_for_evaluation`]), dispatches
//!   each onto a bounded task set, records run rows, and grows an exponential
//!   backoff for repeatedly-failing conditions.
//! * [`ConditionEvaluator`] — the delegation seam. The scheduler calls it for
//!   every due condition; the concrete implementation lives one layer up
//!   (`command::commands::amie`) because evaluating a condition needs
//!   workflow validation, the WI-0092 repair loop, and workflow execution —
//!   all Layer 2 concerns. Layer 1 only ever calls the trait (Tenet 1).
//! * [`AmieAgentLauncher`] — the genuinely Layer 1 work of launching a leader
//!   agent in a container: seeding the condition directory, resolving agent
//!   options, attaching amie's two container labels, and running through
//!   `Arc<dyn AgentRuntimeEngine>`.
//!
//! [`ConditionStore::due_for_evaluation`]: crate::data::fs::ConditionStore::due_for_evaluation

pub mod evaluator;
pub mod launcher;
pub mod scheduler;

pub use evaluator::{
    ConditionEvaluator, EvaluationOutcome, EvaluationRequest, NoRunProgress, RunProgress,
};
pub use launcher::{AmieAgentLauncher, LeaderRunSpec};
pub use scheduler::{AmieScheduler, SchedulerStatus, TICK_INTERVAL};
