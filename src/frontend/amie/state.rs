//! State owned by the amie HTTP daemon.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::RwLock;

use crate::command::commands::amie::gateway::{ConditionGateway, LocalConditionGateway};
use crate::command::dispatch::Engines;
use crate::data::fs::ConditionStore;
use crate::data::session::Session;
use crate::frontend::api::routes::AuthMode;

/// All daemon-local dependencies presented to the amie router.
pub struct AmieAppState {
    pub store: Arc<ConditionStore>,
    pub gateway: Arc<LocalConditionGateway>,
    pub auth_mode: AuthMode,
    pub engines: Engines,
    pub session: Arc<RwLock<Session>>,
    pub started_at: Instant,
    /// Filled only after the listener has successfully bound.
    pub bound_addr: Mutex<Option<String>>,
}

impl AmieAppState {
    pub fn gateway(&self) -> Arc<dyn ConditionGateway> {
        self.gateway.clone()
    }
}
