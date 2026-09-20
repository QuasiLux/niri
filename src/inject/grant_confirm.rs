//! Per-bot pending grant-confirmation queue (Row 11a).
//!
//! `GRANT_REQUEST` occupies a single in-memory slot on [`GrantConfirmState`].
//! The confirmation surface (Row 11b) claims that slot via [`GrantConfirmState::take`].
//! This queue never writes [`crate::inject::grants::GrantMap`].
//!
//! architecture-delta (Row 11a):
//! - what changed: pending-grant queue on Niri / DispatchState; wire opcode is separate.
//! - why: split surface vs queue so 11a can land without the confirmation surface.
//! - supersedes: Row 11 body (split into 11a/11b).
//! - next cards must: 11b claims via `take(bot_id)` on Grant or Deny.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Single-slot pending grant request. Alloc-light: `String` + `[u8; 8]`.
#[derive(Debug, Clone)]
pub struct PendingGrant {
    pub bot_id: String,
    pub requested_at: Instant,
    pub token_fingerprint: [u8; 8],
}

/// Compositor-owned pending-grant slot. Cheap to share (`Arc` behind [`Self::pending`]).
#[derive(Clone)]
pub struct GrantConfirmState {
    pending: Arc<Mutex<Option<PendingGrant>>>,
}

impl GrantConfirmState {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(None)),
        }
    }

    pub fn pending(&self) -> Arc<Mutex<Option<PendingGrant>>> {
        Arc::clone(&self.pending)
    }

    pub fn record(&self, bot_id: impl Into<String>, token_fingerprint: [u8; 8]) {
        *self.pending.lock().unwrap() = Some(PendingGrant {
            bot_id: bot_id.into(),
            requested_at: Instant::now(),
            token_fingerprint,
        });
    }

    pub fn take(&self, bot_id: &str) -> Option<PendingGrant> {
        let mut g = self.pending.lock().unwrap();
        match g.as_ref() {
            Some(p) if p.bot_id == bot_id => g.take(),
            _ => None,
        }
    }

    pub fn clear(&self) {
        *self.pending.lock().unwrap() = None;
    }
}

impl Default for GrantConfirmState {
    fn default() -> Self {
        Self::new()
    }
}

/// Occupy the shared slot. Returns false when a fresh pending request is kept.
pub fn try_queue_pending(
    pending: &Mutex<Option<PendingGrant>>,
    bot_id: &str,
    token_fingerprint: [u8; 8],
) -> bool {
    let mut g = pending.lock().unwrap();
    if let Some(existing) = g.as_ref() {
        if existing.requested_at.elapsed() < Duration::from_secs(30) {
            return false;
        }
        *g = None;
    }
    *g = Some(PendingGrant {
        bot_id: bot_id.to_string(),
        requested_at: Instant::now(),
        token_fingerprint,
    });
    true
}
