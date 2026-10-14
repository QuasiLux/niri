//! QuasiLux inject plane.
//!
//! Wire format (HELLO / AUTH / BIND / CLICK / KEY / SCROLL / DRAG) and token
//! validation live here. Compositor-startup wiring is gated on
//! [`QLUX_INJECT_ENABLE`] inside `Niri::new`.
//!
//! Override [`InjectHarness::key_press`] in production harnesses; the trait
//! default returns false and produces `DISPATCH ERR NOT_BOUND`.

pub mod app_catalog;
pub mod atspi_action;
pub mod atspi_ladder;
pub mod dispatch;
pub mod grant_confirm;
pub mod grants;
pub mod harness;
pub mod overlay_client;
pub mod protocol;
pub mod socket;

pub use crate::input::agent_seat::{AgentSeat, AGENT_SEAT_NAME};
pub use app_catalog::{
    app_id_routes, AppCatalogEntry, AppCatalogMarker, AppRoutes, CaptureStrategy, SemanticStrength,
    Toolkit,
};
pub use atspi_ladder::{route as atspi_route, AtSpiAction, AtSpiRoute, REASON_BG_UNAVAILABLE};
pub use dispatch::{
    dispatch_inject_connection, DispatchHarness, DispatchState, KeyDispatchReq,
};
pub use grant_confirm::{
    is_blocked_app_id, ClickOutcome, ConfirmRow, GrantClickHost, GrantConfirmState,
    GrantConfirmSurface, GrantResolution, GrantSlot, OverlayAnchor, OverlayHandle,
    OverlayKeyboardMode, OverlayLayerConfig, OverlayLayerKind, PendingGrant, PointerClick, Rect,
    SeatKind, GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED,
    GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED, GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED,
    GRANT_ROW_H, GRANT_SLOT_W,
};
pub use grants::{GrantEntry, GrantKind, GrantMap};
pub use harness::NiriInjectHarness;
pub use protocol::{
    token_file_path, validate_token, Frame, FrameError, InjectHarness, Reply, ERR_GRANT_DENIED,
    MAX_FRAME_LEN, OPC_GRANT_REQUEST, TOKEN_FILE_PATH,
};
pub use socket::{
    apply_inject_socket_ownership, handle_inject_client, handle_inject_client_with_close,
    inject_spawn_enabled, inject_spawn_enabled_from, lookup_inject_group_gid,
    resolve_inject_socket_path, InjectServer, TokenRegistry, INJECT_SOCKET_SUFFIX,
    QLUX_INJECT_ENABLE, QLUX_INJECT_GROUP, QLUX_INJECT_SOCKET_SUFFIX,
};

// Re-export so `tests/inject.rs` can impl Smithay traits without adding a
// smithay/calloop dev-dep (Cargo.toml is frozen aside from futures-executor).
pub use calloop;
pub use smithay;

/// Idents kept in the release ELF so the inject-plane scaffold can be grepped.
#[used]
pub static INJECT_PLANE_IDENTS: &[&str] = &[
    AGENT_SEAT_NAME,
    INJECT_SOCKET_SUFFIX,
    "PointerHandle",
    "KeyboardHandle",
    "InjectPlaceholder",
    // v3 opcodes:
    "HELLO",
    "AUTH",
    "BIND",
    "CLICK",
    "KEY",
    "SCROLL",
    "DRAG",
    // v3 reply / error tokens:
    "HELLO OK",
    "AUTH OK",
    "BIND OK",
    "DISPATCH OK",
    "DISPATCH ERR",
    "FRAME ERR",
    "AUTH ERR",
    "TOO_BIG",
    "MALFORMED",
    "UNKNOWN_OPCODE",
    "BAD_TOKEN",
    "NO_TOKEN_FILE",
    "BAD_WINDOW_ID",
    "NOT_BOUND",
    "OCCLUDED",
    "LOCKED",
    "OUTPUTS_OFF",
    "STALE_TOKEN",
    TOKEN_FILE_PATH,
    "run/user/<uid>/qlux/inject-tokens",
    "/run/user/",
    QLUX_INJECT_ENABLE,
    QLUX_INJECT_SOCKET_SUFFIX,
    QLUX_INJECT_GROUP,
    "InjectServer::start",
    "TokenRegistry",
    // v4.6 lock-screen refusal (production InjectHarness reads):
    "NiriInjectHarness",
    "is_locked",
    "outputs_off",
    "ERR_LOCKED",
    "ERR_OUTPUTS_OFF",
    // v37 grant-confirmation programmatic path:
    "GRANT_RESOLVED",
    "GRANT_RESOLVED_OPCODE",
    "GRANT_NOT_PENDING",
    "AT_SPI_BRIDGE_LIVE",
    "AT_SPI_BRIDGE_REAL",
    "AT_TREE_DUMP",
    "CROSS_APP_HANDOFF",
];

/// v8 ELF idents (separate #[used] static; original list unchanged).
#[used]
pub static OUTPUT_POWER_PLANE_IDENTS: &[&str] = &[
    "outputs_off_wired",
    "wl_output_power_state",
    "outputs_off_override",
];

/// Row 37+ ELF idents (separate `#[used]` static; `INJECT_PLANE_IDENTS`
/// byte-equal). The release ELF grep should find each entry ≥ 1 time.
#[used]
pub static GRANT_CONFIRM_OVERLAY_PLANE_IDENTS: &[&str] = &[
    "qlux_grant_confirm",
    GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED,
    GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED,
    GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED,
    "GRANT_CONFIRM_OVERLAY_NAMESPACE",
    "GRANT_CONFIRM_OVERLAY_LAYER",
    "GRANT_CONFIRM_OVERLAY_ANCHOR",
    "GRANT_CONFIRM_OVERLAY_EXCLUSIVE_ZONE",
    "GRANT_CONFIRM_OVERLAY_KEYBOARD_INTERACTIVITY",
];

/// v40 IPC surface fix idents (separate `#[used]` static).
/// Surfaces `niri.grant_confirm.pending()` through `Request::Layers`.
pub const GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_ENUMERATION: &str =
    "GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_ENUMERATION";

#[used]
pub static GRANT_CONFIRM_IPC_LAYERS_PLANE_IDENTS: &[&str] = &[
    GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_ENUMERATION,
];

/// v40 IPC surface-contract types. Re-exported so `tests/inject.rs` can
/// assert the `Request::Layers` field mapping without adding `niri-ipc`
/// to `[dev-dependencies]` (Cargo.toml is frozen on this card).
pub use niri_ipc::{Layer, LayerSurface, LayerSurfaceKeyboardInteractivity};

/// v40.19 — fresh-window grant auto-grant path: the compositor subscribes
/// to `wl_compositor.new_surface` and pre-creates a grant entry on FIRST
/// `new_window` event for any bot already granted the parent `app_id`.
/// The grant is `Auto` until the first manual user click on the bar,
/// which promotes it to `Interactive` (Row-11 persistent grant). This
/// closes Gap G from the v5 Mac-parity audit (`freshly spawned windows
/// stay NOT_GRANTED while qlux_grant_confirm stays Exclusive and
/// invisible`) without regressing the Row-11 cross-bot BIND defense.
pub const GRANT_FRESH_WINDOW_AUTO_V40_19: &str =
    "GRANT_FRESH_WINDOW_AUTO_V40_19";

#[used]
pub static GRANT_FRESH_WINDOW_AUTO_V40_19_PLANE_IDENTS: &[&str] = &[
    GRANT_FRESH_WINDOW_AUTO_V40_19,
];

/// v40.6 KEY delivery: agent-seat keyboard focus on the bound WlSurface.
pub const KEY_DELIVERY_V40_6_SURFACE_ACTIVATION: &str =
    "KEY_DELIVERY_V40_6_SURFACE_ACTIVATION";

#[used]
pub static KEY_DELIVERY_V40_6_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_6_SURFACE_ACTIVATION,
];

/// v40.7 KEY delivery: route through the user seat's KeyboardHandle
/// (the agent seat has no `wl_seat` global, so no Wayland client binds to it).
pub const KEY_DELIVERY_V40_7_USER_SEAT_ROUTING: &str =
    "KEY_DELIVERY_V40_7_USER_SEAT_ROUTING";

#[used]
pub static KEY_DELIVERY_V40_7_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_7_USER_SEAT_ROUTING,
];

/// v40.7a KEY delivery: `inject_agent_key_press` now offsets the Released
/// `InputTime` by 10 ms (was: same `InputTime` as Pressed). Alacritty (and any
/// Wayland client filtering same-instant synthetic Pressed+Released pairs as
/// non-human typing) now consumes the events. Distinct serial for Press vs
/// Release was already in place; the missing variable was `time`. v40.5c +
/// v40.6 + v40.7 idents retained.
pub const KEY_DELIVERY_V40_7A_PRESSES_SERIAL_SPLIT: &str =
    "KEY_DELIVERY_V40_7A_PRESSES_SERIAL_SPLIT";

#[used]
pub static KEY_DELIVERY_V40_7A_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_7A_PRESSES_SERIAL_SPLIT,
];

/// v40.9 KEY delivery: worker thread posts KEY frames onto the main
/// compositor loop via a `calloop::channel`, so dispatch runs against
/// `niri::State` and the user seat's `KeyboardHandle`.
pub const KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF: &str =
    "KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF";

#[used]
pub static KEY_DELIVERY_V40_9_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF,
];

/// v40.10 KEY delivery: optional second KEY token `SHIFT` (or `1`) sets
/// `shift=true`. Absence keeps `shift=false` (backward-compatible). When
/// true, the press path emits Shift_L then the letter key as two sequential
/// Press+Release pairs. Senders may also encode the keystroke as a
/// `Shift_L`-then-letter sequence.
pub const KEY_DELIVERY_V40_10_SHIFT_MODIFIER: &str =
    "KEY_DELIVERY_V40_10_SHIFT_MODIFIER";

#[used]
pub static KEY_DELIVERY_V40_10_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_10_SHIFT_MODIFIER,
];

/// v40.11 KEY delivery: monotonic InputTime::now() on the main-loop
/// press path, and Shift_L held across the letter Press+Release.
pub const KEY_DELIVERY_V40_11_SUSTAINED_DELIVERY: &str =
    "KEY_DELIVERY_V40_11_SUSTAINED_DELIVERY";

#[used]
pub static KEY_DELIVERY_V40_11_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_11_SUSTAINED_DELIVERY,
];

/// v40.13 KEY delivery: wire token `shifted` on KEY; the parsed bool
/// reaches `inject_agent_key_press_shift` (not the no-arg false wrapper).
pub const KEY_DELIVERY_V40_13_SHIFT_THROUGH: &str =
    "KEY_DELIVERY_V40_13_SHIFT_THROUGH";

#[used]
pub static KEY_DELIVERY_V40_13_PLANE_IDENTS: &[&str] = &[
    KEY_DELIVERY_V40_13_SHIFT_THROUGH,
];

#[used]
pub static AT_SPI_BRIDGE_LIVE_PLANE_IDENTS: &[&str] = &[
    crate::inject::atspi_ladder::AT_SPI_BRIDGE_LIVE,
];

#[used]
pub static AT_SPI_BRIDGE_REAL_PLANE_IDENTS: &[&str] = &[
    crate::inject::atspi_ladder::AT_SPI_BRIDGE_REAL,
];

pub const AT_SPI_BRIDGE_BOUND_APP_ID: &str = "AT_SPI_BRIDGE_BOUND_APP_ID";
#[used]
pub static AT_SPI_BRIDGE_BOUND_APP_ID_PLANE_IDENTS: &[&str] = &[
    AT_SPI_BRIDGE_BOUND_APP_ID,
];

pub const AT_TREE_DUMP: &str = "AT_TREE_DUMP";
#[used]
pub static AT_TREE_DUMP_PLANE_IDENTS: &[&str] = &[AT_TREE_DUMP];

pub const CROSS_APP_HANDOFF: &str = "CROSS_APP_HANDOFF";
#[used]
pub static CROSS_APP_HANDOFF_PLANE_IDENTS: &[&str] = &[CROSS_APP_HANDOFF];

/// v40.14.6 AX-tree walk: `Element { role, query }` lookup now prefers
/// `element_lookup_via_tree_dump` (which filters the published
/// `AT_TREE_DUMP` subtree by `(role, name)` and returns bounds from the
/// dump) over the per-node `Component.GetExtents` round-trip in
/// `scan_session_dbus_for_element`. Bounds from the dump sidestep the
/// `xdg-desktop-portal-gtk` `impl_GetExtents: ATK_IS_COMPONENT
/// (user_data) failed` assertion that blocked G2/G4/G5 of the v40.14.3
/// 6-gate e2e.
pub const ELEMENT_AX_LOOKUP_V40_14_6: &str =
    crate::inject::atspi_ladder::ELEMENT_AX_LOOKUP_V40_14_6;
#[used]
pub static ELEMENT_AX_LOOKUP_V40_14_6_PLANE_IDENTS: &[&str] = &[
    ELEMENT_AX_LOOKUP_V40_14_6,
];
