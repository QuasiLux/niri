//! In-memory per-bot × per-window inject BIND grants.
//!
//! Compositor restart drops the map by construction (no persistence). Overlay
//! or HUD click is not approval — Row 11 owns the confirmation surface.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared `(bot_id, window_id)` grant set. Cheap to clone (`Arc`).
#[derive(Clone, Default)]
pub struct GrantMap(Arc<Mutex<HashMap<(String, u64), ()>>>);

impl GrantMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_arc(inner: Arc<Mutex<HashMap<(String, u64), ()>>>) -> Self {
        Self(inner)
    }

    pub fn apply_grant(&self, bot_id: impl Into<String>, window_id: u64) {
        self.0
            .lock()
            .unwrap()
            .insert((bot_id.into(), window_id), ());
    }

    pub fn has_grant(&self, bot_id: &str, window_id: u64) -> bool {
        self.0
            .lock()
            .unwrap()
            .contains_key(&(bot_id.to_string(), window_id))
    }

    #[allow(dead_code)]
    pub fn clear_grants(&self) {
        self.0.lock().unwrap().clear();
    }

    pub fn arc(&self) -> Arc<Mutex<HashMap<(String, u64), ()>>> {
        Arc::clone(&self.0)
    }
}
