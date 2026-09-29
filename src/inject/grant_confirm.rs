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
//!
//! architecture-delta (Row 37+):
//! - what changed: `GrantConfirmState::overlays` (`HashMap<u64, OverlayHandle>`)
//!   + `GrantConfirmSurface::register_layer_shell` / `commit_paint` / `destroy` /
//!   `paint_grant_deny_half` / `paint_label` (in-process layer-shell config +
//!   shm-buffer paint). Inserted on `ClickOutcome::Granted { wid }` so the
//!   bar can be tested as a data-structure overlay before any real `WlLayer`
//!   exists.
//! - why: brief §10 second test (`t_cb2dc567`) wanted the bar visible on
//!   eDP-1; v37 source had no compositor-side `register_layer_shell` API and
//!   the sealed `InjectHarness` trait blocks a generic hook from this card.
//!   This row records the per-window overlay as data so a follow-up card
//!   can attach a real `WlrLayerShellState` factory without re-shaping the
//!   trait.
//! - supersedes: pure-Row-11b design (no overlay map; commit message
//!   "v37+ register qlux_grant_confirm zwlr-layer-shell-v1 OVERLAY Exclusive
//!   on AwaitBind->Dispatch transition").
//! - next cards must: when wiring a real `WlrLayerShellState`, replace
//!   `OverlayHandle` body with a `WlrLayerSurface` and commit the shm buffer
//!   via `smithay::wayland::shm::with_buffer_contents`. The data-structure
//!   API stays.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::inject::app_catalog;
use crate::inject::grants::GrantMap;

#[path = "overlay_client.rs"]
mod overlay_client;
pub use overlay_client::{
    OverlayClientHandle, GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED,
};

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
///
/// Pure HUD skip-list: the catalog's `hud_skip` field is consulted as a parallel truth
/// source, but `hermes-desktop` and `niri-*` are always blocked regardless.
pub fn is_blocked_app_id(app_id: &str) -> bool {
    if app_id == "hermes-desktop" || app_id.starts_with("niri-") {
        return true;
    }
    app_catalog::app_id_routes(app_id).hud_skip
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
    /// Row 37+ — per-window layer-shell overlay registrations
    /// (data structure, not a real `WlrLayerSurface`).
    pub overlays: Arc<Mutex<HashMap<u64, OverlayHandle>>>,
    /// Wayland display socket the overlay-client thread should connect to.
    /// `None` in unit tests (no compositor); production / v38 tests set `Some`.
    pub socket_name: Arc<Mutex<Option<OsString>>>,
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
            overlays: Arc::new(Mutex::new(HashMap::new())),
            socket_name: Arc::new(Mutex::new(None)),
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

    /// Build via [`GrantConfirmSurface::from_candidates`] and install.
    pub fn install_surface_from_candidates(
        &self,
        candidates: impl IntoIterator<Item = (u64, String, String)>,
    ) {
        self.install_surface(GrantConfirmSurface::from_candidates(candidates));
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
                // Row 37+: register the layer-shell overlay for `wid` on
                // FIRST grant. Idempotent: a second grant for the same
                // window keeps the existing handle.
                self.register_overlay(wid, true);
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

// ===========================================================================
// Row 37+ — in-process `zwlr-layer-shell-v1` overlay registration.
//
// The overlay is a pure data structure (NOT a `WlLayer`). It records the
// layer-shell config + a 2560x40 BGRA shm-buffer per (window_id). The data
// lives inside `GrantConfirmState::overlays` so a follow-up card can attach
// a real `WlrLayerShellState` factory without re-shaping the `InjectHarness`
// trait.
// ===========================================================================

/// Ident marker for the overlay-registration call site (LTO-retained).
#[allow(dead_code)]
pub static GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED: &str =
    "GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED";
/// Ident marker for the shm-buffer allocation call site (LTO-retained).
#[allow(dead_code)]
pub static GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED: &str =
    "GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED";
/// Ident marker for the green-left / red-right paint call site (LTO-retained).
#[allow(dead_code)]
pub static GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED: &str =
    "GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED";
/// Ident marker for the v38 in-process overlay-client thread spawn (LTO-retained).
#[used]
#[allow(dead_code)]
static GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED_PIN: &str =
    GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED;

/// v40.1b: render-walk painted-bar wire-in ident (LTO-retained).
pub static GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK: &str =
    "GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK";
#[used]
#[allow(dead_code)]
static GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK_PIN: &str =
    GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK;

/// v40.2: live-inject (DispatchHarness::request_grant) now installs a
/// GrantConfirmSurface after try_queue_pending, so installed_surface()
/// carries rows for the IPC layers handler and painted_overlay_framebuffer()
/// has source data to push. LTO-retained.
pub static GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT: &str =
    "GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT";
#[used]
#[allow(dead_code)]
pub static GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT_PIN: &str =
    GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT;

/// Ident marker for the v39 Niri::new socket-name pin (LTO-retained).
#[used]
#[allow(dead_code)]
pub static NIRI_SOCKET_NAME_PINNED_INTO_GRANT_CONFIRM_PIN: &str =
    "niri_socket_name_pinned_into_grant_confirm";

/// Layer kind for the grant-confirm overlay (mirrors wlr-layer-shell `Layer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayLayerKind {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// Anchor sides for the grant-confirm overlay (mirrors `zwlr_layer_surface_v1` anchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayAnchor {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl OverlayAnchor {
    /// `Top | Left | Right` — default anchor for the grant-confirm bar.
    pub const TOP_LEFT_RIGHT: Self = Self {
        top: true,
        bottom: false,
        left: true,
        right: true,
    };
}

/// Keyboard interactivity mode for the grant-confirm overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKeyboardMode {
    None,
    OnDemand,
    Exclusive,
}

/// Layer-shell configuration captured at registration time.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayLayerConfig {
    pub namespace: String,
    pub layer: OverlayLayerKind,
    pub anchor: OverlayAnchor,
    pub exclusive_zone: i32,
    pub keyboard_interactivity: OverlayKeyboardMode,
    pub width: u32,
    pub height: u32,
}

impl Default for OverlayLayerConfig {
    fn default() -> Self {
        Self {
            namespace: "qlux_grant_confirm".to_string(),
            layer: OverlayLayerKind::Overlay,
            anchor: OverlayAnchor::TOP_LEFT_RIGHT,
            exclusive_zone: 40,
            keyboard_interactivity: OverlayKeyboardMode::Exclusive,
            width: 2560,
            height: 40,
        }
    }
}

/// In-process handle to the grant-confirm bar overlay (NOT a `WlLayer`).
///
/// `shm_buffer` is laid out as BGRA bytes, `width * height * 4` long.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayHandle {
    pub window_id: u64,
    pub config: OverlayLayerConfig,
    pub shm_buffer: Vec<u8>,
    pub painted: bool,
    /// v38 — `Some` after `spawn_overlay_client` when a socket name was given.
    pub client_handle: Option<OverlayClientHandle>,
}

impl OverlayHandle {
    /// Allocate the BGRA shm-buffer and lock in the layer-shell config.
    pub fn register_layer_shell(window_id: u64, socket_name: Option<&OsStr>) -> Self {
        let config = OverlayLayerConfig::default();
        let shm_buffer = vec![0u8; (config.width * config.height * 4) as usize];
        let _ = std::hint::black_box(GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED);
        let _ = std::hint::black_box(GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED);
        let client_handle =
            socket_name.map(|name| overlay_client::spawn_overlay_client(name, window_id));
        Self {
            window_id,
            config,
            shm_buffer,
            painted: false,
            client_handle,
        }
    }

    /// Paint the green-left (x < 1280) / red-right (x >= 1280) BGRA pattern
    /// plus the "GRANT" / "DENY" labels.
    pub fn paint_grant_deny_half(&mut self) {
        let w = self.config.width as usize;
        let h = self.config.height as usize;
        for y in 0..h {
            for x in 0..w {
                let px = (y * w + x) * 4;
                let (b, g, r, a) = if x < 1280 {
                    (0u8, 255u8, 0u8, 255u8) // BGRA = green
                } else {
                    (0u8, 0u8, 255u8, 255u8) // BGRA = red
                };
                self.shm_buffer[px] = b;
                self.shm_buffer[px + 1] = g;
                self.shm_buffer[px + 2] = r;
                self.shm_buffer[px + 3] = a;
            }
        }
        paint_label(&mut self.shm_buffer, w, h, b"GRANT", true);
        paint_label(&mut self.shm_buffer, w, h, b"DENY", false);
        self.painted = true;
        let _ = std::hint::black_box(GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED);
    }

    /// Return the BGRA pixel at `(x, y)`.
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let w = self.config.width as usize;
        let px = (y * w + x) * 4;
        [
            self.shm_buffer[px],
            self.shm_buffer[px + 1],
            self.shm_buffer[px + 2],
            self.shm_buffer[px + 3],
        ]
    }
}

/// Bitmapped 3x5 glyph table for the GRANT / DENY labels. Pure ASCII subset.
const GRANT_CONFIRM_GLYPHS: &[(&str, [u8; 5])] = &[
    ("G", [0b01110, 0b10001, 0b10000, 0b10111, 0b01110]),
    ("R", [0b11110, 0b10001, 0b11110, 0b10100, 0b10010]),
    ("A", [0b01110, 0b10001, 0b11111, 0b10001, 0b10001]),
    ("N", [0b10001, 0b11001, 0b10101, 0b10011, 0b10001]),
    ("T", [0b11111, 0b00100, 0b00100, 0b00100, 0b00100]),
    ("D", [0b11110, 0b10001, 0b10001, 0b10001, 0b11110]),
    ("E", [0b11111, 0b10000, 0b11110, 0b10000, 0b11111]),
    ("Y", [0b10001, 0b01010, 0b00100, 0b00100, 0b00100]),
];

const EMPTY_GLYPH: [u8; 5] = [0; 5];

/// Render `text` (upper-case ASCII) into `buf` as 3x5 black pixels on the
/// existing background. `left` = true places the label at x=560..640 (GRANT
/// half); false places it at x=1936..2016 (DENY half).
fn paint_label(buf: &mut [u8], w: usize, h: usize, text: &[u8], left: bool) {
    let start_x = if left { 560 } else { 1936 };
    let start_y = 12;
    let mut glyph_idx = 0usize;
    for &raw in text.iter() {
        let ch = (raw as char).to_ascii_uppercase();
        let key: &str = match ch {
            'G' => "G",
            'R' => "R",
            'A' => "A",
            'N' => "N",
            'T' => "T",
            'D' => "D",
            'E' => "E",
            'Y' => "Y",
            _ => "",
        };
        let bits = GRANT_CONFIRM_GLYPHS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, b)| *b)
            .unwrap_or(EMPTY_GLYPH);
        for (row, row_bits) in bits.iter().enumerate() {
            for col in 0..5 {
                if row_bits & (1 << (4 - col)) != 0 {
                    let px = ((start_y + row) * w + start_x + glyph_idx * 8 + col) * 4;
                    if px + 3 < buf.len() {
                        buf[px] = 0;
                        buf[px + 1] = 0;
                        buf[px + 2] = 0;
                        buf[px + 3] = 255;
                    }
                }
            }
        }
        glyph_idx += 1;
    }
    // Suppress unused-warning when the text only contains supported glyphs.
    let _ = h;
}

/// Test-only accessor on `GrantConfirmState` exposing the per-window overlay
/// map. `register_overlay` is idempotent across multiple grants for the same
/// window.
impl GrantConfirmState {
    /// `overlays` map accessor (read-only clone of keys for tests).
    pub fn overlay_window_ids(&self) -> Vec<u64> {
        self.overlays
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect()
    }

    /// Idempotent register: insert the overlay for `window_id` if absent.
    /// `commit_paint=true` runs `paint_grant_deny_half` immediately.
    /// Socket name is read from [`Self::socket_name`] (`None` in unit tests).
    pub fn register_overlay(
        &self,
        window_id: u64,
        commit_paint: bool,
    ) -> OverlayHandle {
        let socket_name = self.socket_name.lock().unwrap().clone();
        self.register_overlay_with_socket(window_id, commit_paint, socket_name.as_deref())
    }

    /// Register with an explicit compositor socket name.
    pub fn register_overlay_with_socket(
        &self,
        window_id: u64,
        commit_paint: bool,
        socket_name: Option<&OsStr>,
    ) -> OverlayHandle {
        let mut map = self.overlays.lock().unwrap();
        if let Some(existing) = map.get(&window_id) {
            return existing.clone();
        }
        let mut handle = OverlayHandle::register_layer_shell(window_id, socket_name);
        if commit_paint {
            handle.paint_grant_deny_half();
        }
        map.insert(window_id, handle.clone());
        handle
    }

    /// Production pin: populate [`Self::socket_name`] from `Niri::new` so
    /// `register_overlay` can spawn the in-process overlay-client thread.
    pub fn set_socket_name(&self, name: OsString) {
        let _ = std::hint::black_box(NIRI_SOCKET_NAME_PINNED_INTO_GRANT_CONFIRM_PIN);
        *self.socket_name.lock().unwrap() = Some(name);
    }

    /// Test helper: populate [`Self::socket_name`] so `register_overlay` spawns.
    pub fn set_socket_name_for_test(&self, name: OsString) {
        *self.socket_name.lock().unwrap() = Some(name);
    }

    /// Remove and return the overlay for `window_id` (the bind-completion
    /// path). Returns `None` when no overlay was registered.
    pub fn destroy_overlay(&self, window_id: u64) -> Option<OverlayHandle> {
        self.overlays.lock().unwrap().remove(&window_id)
    }

    /// Inspect the overlay for `window_id` without removing it.
    pub fn overlay_for(&self, window_id: u64) -> Option<OverlayHandle> {
        self.overlays.lock().unwrap().get(&window_id).cloned()
    }

    /// v40.1b: expose the painted overlay's BGRA bytes + dimensions to the
    /// per-output render walk. Returns the first painted OverlayHandle's
    /// (bytes, width, height) tuple, or None when no overlay has been painted
    /// yet. Idempotent. LTO-retained via the painted-to-render-walk ident.
    pub fn painted_overlay_framebuffer(&self) -> Option<(Vec<u8>, u32, u32)> {
        let _ = std::hint::black_box(GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK_PIN);
        let map = self.overlays.lock().unwrap();
        let handle = map.values().find(|h| h.painted)?;
        Some((handle.shm_buffer.clone(), handle.config.width, handle.config.height))
    }
}
