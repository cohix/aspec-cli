//! State owned by the squad HTTP daemon.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::RwLock;

use crate::command::commands::squad::gateway::{LocalTaskGateway, TaskGateway};
use crate::command::dispatch::Engines;
use crate::data::fs::TaskStore;
use crate::data::session::Session;
use crate::frontend::api::routes::AuthMode;

/// All daemon-local dependencies presented to the squad router.
pub struct SquadAppState {
    pub store: Arc<TaskStore>,
    pub gateway: Arc<LocalTaskGateway>,
    pub auth_mode: AuthMode,
    pub engines: Engines,
    pub session: Arc<RwLock<Session>>,
    pub started_at: Instant,
    /// Filled only after the listener has successfully bound.
    pub bound_addr: Mutex<Option<String>>,
}

impl SquadAppState {
    pub fn gateway(&self) -> Arc<dyn TaskGateway> {
        self.gateway.clone()
    }
}
