//! Per-bot pending grant-confirmation queue and compositor-owned confirm surface (Row 11b).
//!
//! The "layer-shell OVERLAY" is a pure data structure ([`GrantConfirmSurface`]) — no
//! `WlLayer` is created. Clicks are resolved by [`GrantConfirmState::on_seat_click`].
//!
//! architecture-delta (Row 11b):
//! - what changed: `GrantConfirmSurface` / `ConfirmRow` / seat filter / click handler
//!   calling `GrantMap::apply_grant` then `take(bot_id)` (Grant) or `take` only (Deny).
//!   Follow-up wire variants `Reply::GrantAckOk` / `Reply::GrantAckDenied` exist.
//! - why: compositor-owned confirmation is a data structure, not a Wayland surface;
//!   HUD (`hermes-desktop`) and `niri-` app_ids must not be grantable.
//! - supersedes: Row 11a queue-only (`772001ed`); 11a `architecture-delta` next-cards.
//! - next cards must: worker-loop 50ms calloop `Timer` to poll `pending_resolution`
//!   and write the follow-up `GRANT ACK OK|DENIED` on the same inject stream. This
//!   card's five tests drive `on_seat_click` directly (bypass the wire follow-up).
//!   Do not add a real `WlLayer`. Do not persist grants.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::inject::grants::GrantMap;

/// Slot width for Grant / Deny buttons in the data-structure surface.
pub const GRANT_SLOT_W: f64 = 100.0;
/// Row height for one toplevel's Grant+Deny pair.
pub const GRANT_ROW_H: f64 = 40.0;

/// Single-slot pending grant request. Alloc-light: `String` + `[u8; 8]`.
#[derive(Debug, Clone)]
pub struct PendingGrant {
    pub bot_id: String,
    pub requested_at: Instant,
    pub token_fingerprint: [u8; 8],
}

/// Which seat produced the pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatKind {
    User,
    Agent,
}

/// Follow-up resolution recorded for the worker to read after `take`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantResolution {
    Granted,
    Denied,
}

/// Which button on a confirm row was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantSlot {
    Grant,
    Deny,
}

/// Result of [`GrantConfirmState::on_seat_click`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClickOutcome {
    Granted { wid: u64 },
    Denied,
    IgnoredAgentSeat,
    IgnoredNoRow,
    IgnoredNoPending,
}

/// Axis-aligned hit rectangle for a confirm slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// One Grant or Deny slot on the confirm surface.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmRow {
    pub wid: u64,
    pub app_id: String,
    pub title: String,
    pub rect: Rect,
    pub slot: GrantSlot,
}

/// Compositor-owned grant-confirmation surface (data structure, not a `WlLayer`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GrantConfirmSurface {
    pub rows: Vec<ConfirmRow>,
}

/// Pointer click against the confirm surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerClick {
    pub x: f64,
    pub y: f64,
    pub seat: SeatKind,
}

/// Host that can insert a `(bot_id, window_id)` grant. [`GrantMap::apply_grant`]
/// is the only insert path.
pub trait GrantClickHost {
    fn apply_grant(&self, bot_id: &str, window_id: u64);
}

impl GrantClickHost for GrantMap {
    fn apply_grant(&self, bot_id: &str, window_id: u64) {
        GrantMap::apply_grant(self, bot_id.to_string(), window_id);
    }
}

impl GrantClickHost for Arc<Mutex<HashMap<(String, u64), ()>>> {
    fn apply_grant(&self, bot_id: &str, window_id: u64) {
        GrantMap::from_arc(Arc::clone(self)).apply_grant(bot_id.to_string(), window_id);
    }
}

/// Window list for [`GrantConfirmState::build_surface`]. Production implements this for Niri.
pub trait GrantSurfaceSource {
    fn grant_toplevels(&self) -> Vec<(u64, String, String)>;
}

/// True when the toplevel must not appear on the confirm surface (HUD / compositor UI).
pub fn is_blocked_app_id(app_id: &str) -> bool {
    app_id == "hermes-desktop" || app_id.starts_with("niri-")
}

fn grant_rect(index: usize) -> Rect {
    Rect {
        x: 0.0,
        y: index as f64 * GRANT_ROW_H,
        w: GRANT_SLOT_W,
        h: GRANT_ROW_H,
    }
}

fn deny_rect(index: usize) -> Rect {
    Rect {
        x: GRANT_SLOT_W,
        y: index as f64 * GRANT_ROW_H,
        w: GRANT_SLOT_W,
        h: GRANT_ROW_H,
    }
}

impl GrantConfirmSurface {
    /// Filter blocked app_ids, then lay out Grant+Deny slots top-to-bottom.
    pub fn from_candidates(candidates: impl IntoIterator<Item = (u64, String, String)>) -> Self {
        let mut rows = Vec::new();
        let mut idx = 0usize;
        for (wid, app_id, title) in candidates {
            if is_blocked_app_id(&app_id) {
                continue;
            }
            rows.push(ConfirmRow {
                wid,
                app_id: app_id.clone(),
                title: title.clone(),
                rect: grant_rect(idx),
                slot: GrantSlot::Grant,
            });
            rows.push(ConfirmRow {
                wid,
                app_id,
                title,
                rect: deny_rect(idx),
                slot: GrantSlot::Deny,
            });
            idx += 1;
        }
        Self { rows }
    }

    /// Filter blocked app_ids while keeping caller-supplied rects (gaps stay empty).
    pub fn from_rows(rows: Vec<ConfirmRow>) -> Self {
        Self {
            rows: rows
                .into_iter()
                .filter(|r| !is_blocked_app_id(&r.app_id))
                .collect(),
        }
    }
}

/// Compositor-owned pending-grant slot plus the data-structure confirm surface.
#[derive(Clone)]
pub struct GrantConfirmState {
    pending: Arc<Mutex<Option<PendingGrant>>>,
    surface: Arc<Mutex<GrantConfirmSurface>>,
    pending_resolution: Arc<Mutex<HashMap<String, GrantResolution>>>,
}

impl GrantConfirmState {
    pub fn new() -> Self {
        Self::from_pending(Arc::new(Mutex::new(None)))
    }

    pub fn from_pending(pending: Arc<Mutex<Option<PendingGrant>>>) -> Self {
        Self {
            pending,
            surface: Arc::new(Mutex::new(GrantConfirmSurface::default())),
            pending_resolution: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn pending(&self) -> Arc<Mutex<Option<PendingGrant>>> {
        Arc::clone(&self.pending)
    }

    pub fn pending_resolution(&self) -> Arc<Mutex<HashMap<String, GrantResolution>>> {
        Arc::clone(&self.pending_resolution)
    }

    pub fn installed_surface(&self) -> GrantConfirmSurface {
        self.surface.lock().unwrap().clone()
    }

    pub fn install_surface(&self, surface: GrantConfirmSurface) {
        *self.surface.lock().unwrap() = surface;
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

    /// Enumerate focusable toplevels, drop HUD / `niri-` rows, install the surface.
    pub fn build_surface(&self, niri: &impl GrantSurfaceSource) -> GrantConfirmSurface {
        let surface = GrantConfirmSurface::from_candidates(niri.grant_toplevels());
        self.install_surface(surface.clone());
        surface
    }

    /// Hit-test `event` against the installed surface, apply seat filter, mutate grants.
    pub fn on_seat_click(
        &self,
        state: &impl GrantClickHost,
        event: PointerClick,
    ) -> ClickOutcome {
        let bot_id = match self.pending.lock().unwrap().as_ref() {
            Some(p) => p.bot_id.clone(),
            None => return ClickOutcome::IgnoredNoPending,
        };

        let surface = self.surface.lock().unwrap().clone();
        let hit = surface.rows.iter().find(|row| {
            !is_blocked_app_id(&row.app_id) && row.rect.contains(event.x, event.y)
        });

        let Some(row) = hit else {
            self.record_resolution(&bot_id, GrantResolution::Denied);
            self.take(&bot_id);
            return ClickOutcome::IgnoredNoRow;
        };

        self.finish_click(state, &bot_id, row.wid, event.seat, row.slot)
    }

    /// Resolved-slot path used by [`crate::inject::protocol::InjectHarness::apply_grant_confirm_click`].
    pub fn apply_resolved(
        &self,
        state: &impl GrantClickHost,
        window_id: u64,
        app_id: &str,
        seat: SeatKind,
        resolution: GrantResolution,
    ) -> ClickOutcome {
        let bot_id = match self.pending.lock().unwrap().as_ref() {
            Some(p) => p.bot_id.clone(),
            None => return ClickOutcome::IgnoredNoPending,
        };
        if is_blocked_app_id(app_id) {
            self.record_resolution(&bot_id, GrantResolution::Denied);
            self.take(&bot_id);
            return ClickOutcome::IgnoredNoRow;
        }
        let slot = match resolution {
            GrantResolution::Granted => GrantSlot::Grant,
            GrantResolution::Denied => GrantSlot::Deny,
        };
        self.finish_click(state, &bot_id, window_id, seat, slot)
    }

    fn finish_click(
        &self,
        state: &impl GrantClickHost,
        bot_id: &str,
        wid: u64,
        seat: SeatKind,
        slot: GrantSlot,
    ) -> ClickOutcome {
        if seat == SeatKind::Agent {
            self.record_resolution(bot_id, GrantResolution::Denied);
            self.take(bot_id);
            return ClickOutcome::IgnoredAgentSeat;
        }
        match slot {
            GrantSlot::Grant => {
                state.apply_grant(bot_id, wid);
                self.record_resolution(bot_id, GrantResolution::Granted);
                self.take(bot_id);
                ClickOutcome::Granted { wid }
            }
            GrantSlot::Deny => {
                self.record_resolution(bot_id, GrantResolution::Denied);
                self.take(bot_id);
                ClickOutcome::Denied
            }
        }
    }

    fn record_resolution(&self, bot_id: &str, resolution: GrantResolution) {
        self.pending_resolution
            .lock()
            .unwrap()
            .insert(bot_id.to_string(), resolution);
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
