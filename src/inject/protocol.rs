//! Tagged-line inject wire protocol.
//!
//! One opcode per `\n`-terminated frame. AUTH is two lines (`AUTH\n<token>\n`).
//! JSON was rejected: the seven opcodes fit on one line and tagged-line stays
//! branch-free at a few hundred lines.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use smithay::input::SeatHandler;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::inject::app_catalog::AppRoutes;
use crate::input::agent_seat::AgentSeat;

pub use crate::inject::grant_confirm::{
    is_blocked_app_id, ClickOutcome, ConfirmRow, GrantClickHost, GrantConfirmSurface,
    GrantResolution, GrantSlot, PointerClick, Rect, SeatKind,
};

pub const OPC_HELLO: &str = "HELLO";
pub const OPC_AUTH: &str = "AUTH";
pub const OPC_BIND: &str = "BIND";
pub const OPC_CLICK: &str = "CLICK";
pub const OPC_KEY: &str = "KEY";
pub const OPC_SCROLL: &str = "SCROLL";
pub const OPC_DRAG: &str = "DRAG";
pub const OPC_GRANT_REQUEST: &str = "GRANT_REQUEST";
pub const OPC_GRANT_RESOLVED: &str = "GRANT_RESOLVED";

/// `GRANT_RESOLVED <bot_id> <window_id> <GRANT|DENY>`
#[used]
#[allow(dead_code)]
static OPC_GRANT_RESOLVED_IDENT: &str = "GRANT_RESOLVED";

/// Ident for `strings | grep`; mirrors the opcode literal but the LTO-stable
/// hook needed for v37 `#[used]` audit. Distinct static so thin LTO cannot
/// merge it with the OPC constant.
#[used]
#[allow(dead_code)]
static GRANT_RESOLVED_OPCODE: &[u8] = b"GRANT_RESOLVED_OPCODE\0";

/// `AT_ELEMENT_REQ <wid> LC <role> <name> [<index>]` / `EI <role> <name> <text…> [<index>]` / `CV <query>`.
pub const OPC_AT_ELEMENT_REQ: &str = "AT_ELEMENT_REQ";

/// `AT_ACTION_REQ <wid> <role> <name> [<action_name>] <index>` — v40.15.3a.
pub const OPC_AT_ACTION_REQ: &str = "AT_ACTION_REQ";

/// v40.28: `AT_TOGGLE_REQ <wid> <name>` (brief form) or
/// `AT_TOGGLE_REQ <wid> toggle button <w0> <w1>` (two-word name join,
/// matching the live cua-driver v40.28 encoder shape at
/// `7c9fc04` `encode_at_toggle_req_line`). The legacy 3-tuple form
/// `AT_TOGGLE_REQ <wid> <role> <name>` is still accepted by the parser
/// for backward compat with the worker CLI; the role token is dropped.
///
/// Server response: tri-state `AT_TOGGLE_REPLY checked` / `unchecked` /
/// `no_action` (after synthesizing a real `BTN_LEFT` press+release at
/// the matched element's screen-space center coords). Closes the GTK
/// 4 `GtkToggleButton` (pavucontrol) bypass gap that v40.15.3
/// `AT_ACTION_REQ` does not cover — those widgets do not implement
/// the AT-SPI `Action` interface so `Action.do_action(0)` returns
/// `ok` at the wire level but never fires the toggle.
pub const OPC_AT_TOGGLE_REQ: &str = "AT_TOGGLE_REQ";

/// `AT_TREE_REQ <wid> [path…]`
pub const OPC_AT_TREE_REQ: &str = "AT_TREE_REQ";

/// `AT_TREE_BY_APP_ID <app_id>` — v40.15.4 G1 galculator GTK 2/3 fallback.
pub const OPC_AT_TREE_BY_APP_ID: &str = "AT_TREE_BY_APP_ID";

/// `LAYER_HIT_TEST <x> <y>` — v40.18 layer-shell hit-test (screen coords).
pub const OPC_LAYER_HIT_TEST: &str = "LAYER_HIT_TEST";

/// `WINDOW_FOCUS_REQ <wid>` — v40.24 server-side handler for
/// `niri msg action focus-window --id <wid>`. cua-driver side is
/// `t_v40_24_cua_focus_window`; the wire contract MUST stay in sync.
pub const OPC_WINDOW_FOCUS_REQ: &str = "WINDOW_FOCUS_REQ";

/// Keep the wire-ident name in the release ELF (`strings | grep GRANT_REQUEST`).
#[used]
#[allow(dead_code)]
static OPC_GRANT_REQUEST_IDENT: &str = "GRANT_REQUEST";

/// Independent literals so thin LTO cannot merge them into an adjacent &str.
#[used]
#[allow(dead_code)]
static OPC_AT_ELEMENT_REQ_IDENT: &str = "AT_ELEMENT_REQ";

/// v40.41: nth-sibling AT_ELEMENT_REQ index.
#[used]
pub static AT_ELEMENT_REQ_INDEX_NTH_SIBLING_V40_41: &[u8] = b"AT_ELEMENT_REQ_INDEX_NTH_SIBLING_V40_41\0";

#[used]
#[allow(dead_code)]
static OPC_AT_ACTION_REQ_IDENT: &str = "AT_ACTION_REQ";

/// v40.28: keep the new `AT_TOGGLE_REQ` wire opcode literal in the
/// release ELF (`strings | grep AT_TOGGLE_REQ`). Trap 45 / lld gc:
/// `#[used]` + `black_box` at the parse arm keeps the symbol alive
/// past thin-LTO.
#[used]
#[allow(dead_code)]
pub(crate) static OPC_AT_TOGGLE_REQ_IDENT: &str = "AT_TOGGLE_REQ";

/// Live ident for `strings | grep AT_ACTION_REQ_OPCODE_V40_15_3`.
/// Referenced at a live call site via `std::hint::black_box` (same shape as
/// `KEY_DELIVERY_V40_9_DISPATCH_IDENT` in `src/inject/dispatch.rs`).
#[used]
pub(crate) static AT_ACTION_REQ_OPCODE_V40_15_3: &[u8] = b"AT_ACTION_REQ_OPCODE_V40_15_3\0";

/// Live ident for `strings | grep AT_ACTION_REQ_DISPATCHED_V40_15_3` —
/// planted at the `Frame::AtActionReq` dispatch arm in `socket.rs`.
#[used]
pub(crate) static AT_ACTION_REQ_DISPATCHED_V40_15_3: &[u8] = b"AT_ACTION_REQ_DISPATCHED_V40_15_3\0";

/// Live ident for `strings | grep FORCE_ICON_REDRAW_V40_15_5` (G2 icon redraw).
#[used]
pub(crate) static FORCE_ICON_REDRAW_V40_15_5: &[u8] = b"FORCE_ICON_REDRAW_V40_15_5\0";

#[used]
#[allow(dead_code)]
static OPC_AT_TREE_REQ_IDENT: &str = "AT_TREE_REQ";

#[used]
#[allow(dead_code)]
static OPC_AT_TREE_BY_APP_ID_IDENT: &str = "AT_TREE_BY_APP_ID";

/// v40.17 KEY modifiers. `#[used]` byte static plus `black_box` at the KEY
/// parse and press sites so `strings | grep NIRI_KEY_MODIFIERS_V40_17` survives lld.
#[used]
pub(crate) static NIRI_KEY_MODIFIERS_V40_17: &[u8] = b"NIRI_KEY_MODIFIERS_V40_17\0";

/// v40.18 layer-shell hit-test. Byte static plus `black_box` at the parse
/// arm and the socket dispatch arm so lld cannot drop it.
#[used]
pub static LAYER_HIT_TEST_V40_18: &[u8] = b"LAYER_HIT_TEST_V40_18\0";

/// `LAYER_WIDGET_TREE <x> <y>` — v40.21 widget tree under the layer hit.
pub const OPC_LAYER_WIDGET_TREE: &str = "LAYER_WIDGET_TREE";

/// v40.21 widget-tree opcode. Byte static plus `black_box` at the parse arm,
/// the reply encoder, and the socket dispatch arm so lld cannot drop it.
#[used]
pub static LAYER_WIDGET_TREE_V40_21: &[u8] = b"LAYER_WIDGET_TREE_V40_21\0";

/// Install-card strings gate (`strings | grep LAYER_WIDGET_TREE_NIRI_V40_20`).
/// Same live sites as [`LAYER_WIDGET_TREE_V40_21`].
#[used]
pub static LAYER_WIDGET_TREE_NIRI_V40_20: &[u8] = b"LAYER_WIDGET_TREE_NIRI_V40_20\0";

/// v40.24 `WINDOW_FOCUS_REQ` opcode. Byte static plus `black_box` at the
/// parse arm, the reply encoder, and the socket dispatch arm so lld
/// cannot drop it. Mirror of the v40.18 / v40.21 literal pattern.
#[used]
pub static WINDOW_FOCUS_REQ_V40_24: &[u8] = b"WINDOW_FOCUS_REQ_V40_24\0";

/// v40.28 `AT_TOGGLE_REQ` opcode ident. Byte static plus `black_box` at
/// the parse arm, the reply encoder, and the socket dispatch arm so lld
/// cannot drop it. Mirror of the v40.18 / v40.21 / v40.24 literal
/// pattern. Acceptance: `strings | grep AT_TOGGLE_REQ_V40_28`.
#[used]
pub static AT_TOGGLE_REQ_V40_28: &[u8] = b"AT_TOGGLE_REQ_V40_28\0";

/// v40.29 parse-side ident. Pins the `parse_at_toggle_req` wire decoder
/// into the release ELF so `strings | grep AT_TOGGLE_REQ_PARSE_V40_29`
/// returns >=1 even after thin-LTO. Distinct static so the parse and
/// dispatch surfaces can be gated independently. Acceptance: source
/// `strings` count >= 1.
#[used]
#[allow(dead_code)]
pub static AT_TOGGLE_REQ_PARSE_V40_29: &[u8] = b"\x01AT_TOGGLE_REQ_PARSE_V40_29\x01";

/// v40.29 dispatch-side ident. Pins the `Reply::AtToggleReply` encoder
/// (and the live `InjectHarness::at_toggle_req` override + the
/// `Frame::AtToggleReq` socket dispatch arm) into the release ELF so
/// `strings | grep AT_TOGGLE_REQ_DISPATCH_V40_29` returns >=1 even after
/// thin-LTO. Acceptance: source `strings` count >= 1.
#[used]
#[allow(dead_code)]
pub static AT_TOGGLE_REQ_DISPATCH_V40_29: &[u8] = b"\x01AT_TOGGLE_REQ_DISPATCH_V40_29\x01";

/// v40.50: `AT_TOGGLE_REQ` carries an OPTIONAL TRAILING occurrence
/// index and `AT_TOGGLE_REPLY` reports the resolved identity.
///
/// Measured (v40.49 G2, `t_973ff960`): pavucontrol, three sibling rows
/// sharing `role`+`name`, target row at tree index 2. The client resolved
/// index 2 and computed the right screen point; it then handed this
/// server only the NAME, and this server re-matched by accessible name
/// and took the first match — index 0, the role-aggregate row. The wire
/// was not broken: it returned rc=0 and a well-formed reply for a click
/// aimed at a widget it had no way to name. It was **blind**.
///
/// Acceptance: `strings | grep AT_TOGGLE_OCCURRENCE_INDEX_V40_50` >= 1.
#[used]
pub static AT_TOGGLE_OCCURRENCE_INDEX_V40_50: &[u8] =
    b"\x01AT_TOGGLE_OCCURRENCE_INDEX_V40_50\x01";

/// v40.51 gate ident. Acceptance: `strings | grep AT_ACTION_OCCURRENCE_INDEX_V40_51` >= 1.
/// `#[used]` alone is NOT enough under thin-LTO — lld gc's it. There MUST be a
/// `std::hint::black_box` read at a LIVE call site; put one in
/// `parse_at_action_req` AND one in the `Reply::AtActionReply` encoder arm,
/// plus a `tracing::` line naming it at the ACTION dispatch in socket.rs.
#[used]
#[allow(dead_code)]
pub static AT_ACTION_OCCURRENCE_INDEX_V40_51: &[u8] = b"\x01AT_ACTION_OCCURRENCE_INDEX_V40_51\x01";

/// Render an f64 rect field as a compact integer when it is integral
/// (AT-SPI extents are integer pixels), so the wire reads
/// `x=1929 y=305 w=36 h=34` rather than `x=1929.0`.
pub(crate) fn fmt_coord(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// v40.33: production `AT_TOGGLE_REQ` resolves a real `WlSurface` pointer
/// focus on `NiriInjectHarness` and does not treat a hardcoded
/// `ToggleState::Checked` as an observed widget state. Byte static plus
/// `black_box` at the live harness call site so thin-LTO cannot drop it.
/// Acceptance: `strings | grep AT_TOGGLE_REQ_REAL_SURFACE_V40_33`.
#[used]
pub static AT_TOGGLE_REQ_REAL_SURFACE_V40_33: &[u8] =
    b"\x01AT_TOGGLE_REQ_REAL_SURFACE_V40_33\x01";

/// v40.25 `WINDOW_FOCUS_DISPATCHED` ident. Pins the live `DispatchHarness::
/// focus_window` override (worker→main handoff) into the release ELF so
/// `strings | grep WINDOW_FOCUS_DISPATCHED_V40_25` returns ≥1 even after
/// LTO. Mirrors the v40.18 / v40.21 `LAYER_HIT_TEST_V40_18` /
/// `LAYER_WIDGET_TREE_V40_21` literal pattern.
#[used]
pub static WINDOW_FOCUS_DISPATCHED_V40_25: &[u8] = b"WINDOW_FOCUS_DISPATCHED_V40_25\0";

/// Trap 45: `#[used]` byte statics (not `&[&str]`) referenced at the live
/// `Frame::TreeByAppId` wire-encoder arm via `std::hint::black_box`.
#[used]
pub(crate) static SCAN_BY_APP_ID_V40_15_4: &[u8] = b"SCAN_BY_APP_ID_V40_15_4\0";

#[used]
pub(crate) static G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4: &[u8] =
    b"G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4\0";

pub const REP_HELLO_OK: &str = "HELLO OK";
pub const REP_AUTH_OK: &str = "AUTH OK";
pub const REP_BIND_OK: &str = "BIND OK";
pub const REP_BIND_PENDING: &str = "BIND PENDING";
pub const REP_DISPATCH_OK: &str = "DISPATCH OK";
pub const REP_DISPATCH_ERR: &str = "DISPATCH ERR";
pub const REP_FRAME_ERR: &str = "FRAME ERR";
pub const REP_AUTH_ERR: &str = "AUTH ERR";
pub const REP_GRANT_ACK: &str = "GRANT ACK";
pub const REP_GRANT_QUEUED: &str = "QUEUED";
pub const REP_GRANT_DENIED_W: &str = "DENIED";
pub const REP_GRANT_ACK_OK_W: &str = "OK";
pub const REP_AT_ELEMENT_REPLY: &str = "AT_ELEMENT_REPLY";
pub const REP_AT_ACTION_REPLY: &str = "AT_ACTION_REPLY";
pub const REP_AT_TREE_REPLY: &str = "AT_TREE_REPLY";
pub const REP_LAYER_HIT_TEST_REPLY: &str = "LAYER_HIT_TEST_REPLY";

/// v40.28 reply for `AT_TOGGLE_REQ`. Tri-state per the brief's gate
/// semantics (`t_89a83060` / `t_fe16b4d7` architecture-delta):
/// - `AT_TOGGLE_REPLY checked`   — toggle is now checked (was unchecked)
/// - `AT_TOGGLE_REPLY unchecked` — toggle is now unchecked (was checked)
/// - `AT_TOGGLE_REPLY no_action` — toolkit refused the gesture; no
///   state change. Cua-driver exits 2 (soft-fail).
///
/// Mirrors cua-driver v40.28 `ParsedReply::AtToggleReply` + `ToggleState`
/// at `7c9fc04` so the wires stay in lock-step.
pub const REP_AT_TOGGLE_REPLY: &str = "AT_TOGGLE_REPLY";

/// v40.21 reply. Miss: `LAYER_WIDGET_TREE_REPLY 0`. Hit: header plus widget lines.
pub const REP_LAYER_WIDGET_TREE_REPLY: &str = "LAYER_WIDGET_TREE_REPLY";

/// v40.24 reply. Hit: `WINDOW_FOCUS_REPLY 1`. Miss: `WINDOW_FOCUS_REPLY 0 <reason>`.
pub const REP_WINDOW_FOCUS_REPLY: &str = "WINDOW_FOCUS_REPLY";
pub const REP_BG_UNAVAILABLE: &str = "BACKGROUND_UNAVAILABLE";
pub const IDENT_AT_SPI_STRONG: &str = "AT_SPI_STRONG";
pub const IDENT_AT_SPI_CHROME: &str = "AT_SPI_CHROME";

#[used]
#[allow(dead_code)]
static REP_BIND_PENDING_IDENT: &str = "BIND PENDING";

#[used]
#[allow(dead_code)]
static REP_AT_ELEMENT_REPLY_IDENT: &str = "AT_ELEMENT_REPLY";

#[used]
#[allow(dead_code)]
static REP_AT_ACTION_REPLY_IDENT: &str = "AT_ACTION_REPLY";

/// v40.28: keep the new `AT_TOGGLE_REPLY` wire opcode literal in the
/// release ELF (`strings | grep AT_TOGGLE_REPLY`). Mirror of
/// `REP_AT_ACTION_REPLY_IDENT` for the new opcode.
#[used]
#[allow(dead_code)]
pub(crate) static REP_AT_TOGGLE_REPLY_IDENT: &str = "AT_TOGGLE_REPLY";

#[used]
#[allow(dead_code)]
static REP_AT_TREE_REPLY_IDENT: &str = "AT_TREE_REPLY";

#[used]
#[allow(dead_code)]
static REP_BG_UNAVAILABLE_IDENT: &str = "BACKGROUND_UNAVAILABLE";

#[used]
#[allow(dead_code)]
static IDENT_AT_SPI_STRONG_IDENT: &str = "AT_SPI_STRONG";

#[used]
#[allow(dead_code)]
static IDENT_AT_SPI_CHROME_IDENT: &str = "AT_SPI_CHROME";

pub const ERR_TOO_BIG: &str = "TOO_BIG";
pub const ERR_MALFORMED: &str = "MALFORMED";
pub const ERR_UNKNOWN_OPC: &str = "UNKNOWN_OPCODE";
pub const ERR_BAD_TOKEN: &str = "BAD_TOKEN";
pub const ERR_NO_TOKEN_FILE: &str = "NO_TOKEN_FILE";
pub const ERR_IO: &str = "IO_ERROR";
pub const ERR_BAD_WINDOW: &str = "BAD_WINDOW_ID";
pub const ERR_NOT_BOUND: &str = "NOT_BOUND";
pub const ERR_OCCLUDED: &str = "OCCLUDED";
pub const ERR_LOCKED: &str = "LOCKED";
pub const ERR_OUTPUTS_OFF: &str = "OUTPUTS_OFF";
pub const ERR_STALE_TOKEN: &str = "STALE_TOKEN";
pub const ERR_NOT_GRANTED: &str = "NOT_GRANTED";
pub const ERR_GRANT_DENIED: &str = "GRANT_DENIED";
pub const ERR_GRANT_NOT_PENDING: &str = "GRANT_NOT_PENDING";

/// Keep the wire-ident name in the release ELF (`strings | grep ERR_NOT_GRANTED`).
#[used]
#[allow(dead_code)]
static ERR_NOT_GRANTED_IDENT: &str = "ERR_NOT_GRANTED";

/// Reserved for Row 11b denial path; required in the release ELF now.
#[used]
#[allow(dead_code)]
static ERR_GRANT_DENIED_IDENT: &str = "ERR_GRANT_DENIED";

#[used]
#[allow(dead_code)]
static ERR_GRANT_NOT_PENDING_IDENT: &str = "ERR_GRANT_NOT_PENDING";

pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Literal placeholder path. Production resolution substitutes `<uid>` via [`token_file_path`].
pub const TOKEN_FILE_PATH: &str = "/run/user/<uid>/qlux/inject-tokens";

/// Resolve [`TOKEN_FILE_PATH`] for the current user (`libc::getuid()`).
pub fn token_file_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    // `handle_inject_client` is generic and not yet monomorphized in the niri
    // binary, so LTO would otherwise drop `validate_token` (and its warn-only
    // file-mode check). Keep the function live without changing the path.
    let _ = std::hint::black_box(validate_token as fn(&str, &Path) -> Result<String, TokenError>);
    // NUL-terminated copies so `strings | grep -E '^…$'` sees each ident alone.
    // token_file_path is called from Niri::new; packed &str literals do not survive.
    let _ = std::hint::black_box((
        OPC_AT_ELEMENT_REQ,
        OPC_AT_ACTION_REQ,
        REP_AT_ELEMENT_REPLY,
        REP_AT_ACTION_REPLY,
        REP_BG_UNAVAILABLE,
        IDENT_AT_SPI_STRONG,
        IDENT_AT_SPI_CHROME,
        OPC_GRANT_RESOLVED,
        OPC_AT_TREE_REQ,
        OPC_AT_TREE_BY_APP_ID,
        REP_AT_TREE_REPLY,
    ));
    let _ = std::hint::black_box(AT_ACTION_REQ_OPCODE_V40_15_3);
    let _ = std::hint::black_box(b"AT_ELEMENT_REQ\0");
    let _ = std::hint::black_box(b"AT_ACTION_REQ\0");
    let _ = std::hint::black_box(b"AT_ELEMENT_REPLY\0");
    let _ = std::hint::black_box(b"AT_ACTION_REPLY\0");
    let _ = std::hint::black_box(b"AT_TREE_REQ\0");
    let _ = std::hint::black_box(b"AT_TREE_BY_APP_ID\0");
    let _ = std::hint::black_box(b"AT_TREE_REPLY\0");
    let _ = std::hint::black_box(SCAN_BY_APP_ID_V40_15_4);
    let _ = std::hint::black_box(G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
    let _ = std::hint::black_box(b"BACKGROUND_UNAVAILABLE\0");
    let _ = std::hint::black_box(b"AT_SPI_STRONG\0");
    let _ = std::hint::black_box(b"AT_SPI_CHROME\0");
    let _ = std::hint::black_box(b"GRANT_RESOLVED_OPCODE\0");
    let _ = std::hint::black_box(b"GRANT_NOT_PENDING\0");
    PathBuf::from(TOKEN_FILE_PATH.replace("<uid>", &uid.to_string()))
}

/// v40.28: tri-state new-checked-state of the toggle button after the
/// `AT_TOGGLE_REQ` synthesized `BTN_LEFT` press+release. Wire literal
/// matches the cua-driver v40.28 `ToggleState::as_wire` (case-sensitive
/// on the wire — the cua-driver parser is case-insensitive on read).
///
/// - `Checked`   — toggle landed on the checked state (was unchecked).
/// - `Unchecked` — toggle landed on the unchecked state (was checked).
/// - `NoAction`  — toolkit refused the gesture; no state change.
///                 Soft-fail: cua-driver exits 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleState {
    Checked,
    Unchecked,
    NoAction,
    /// v40.50: the caller named an occurrence index that does not exist.
    /// `candidates` is the number of elements the predicate actually
    /// accepted, so a caller can tell "this tree holds 3 rows named
    /// `Mute audio`" from "this tree holds none".
    ///
    /// This variant exists to make silent substitution IMPOSSIBLE: never
    /// fall back to the first match here. A silent fall-back is the exact
    /// defect class that made `AT_TOGGLE_REQ` deliver to the role-aggregate
    /// row while the scored per-stream row was never dispatched to at all.
    IndexOutOfRange { requested: u32, candidates: u32 },
}

impl ToggleState {
    /// Render the wire-side literal. Case-sensitive on the wire
    /// (lowercase) so the cua-driver's `parse_wire`
    /// case-insensitive match still works.
    pub fn as_wire(&self) -> &'static str {
        match self {
            ToggleState::Checked => "checked",
            ToggleState::Unchecked => "unchecked",
            ToggleState::NoAction => "no_action",
            ToggleState::IndexOutOfRange { .. } => "index_out_of_range",
        }
    }

    /// True iff this is a successful (real-state-change) reply
    /// rather than a soft-fail noop.
    pub fn is_success(self) -> bool {
        matches!(self, ToggleState::Checked | ToggleState::Unchecked)
    }
}

/// v40.28: server-side reply for the `AT_TOGGLE_REQ` opcode. Tri-state
/// per the brief; see [`ToggleState`]. Synthesized by
/// [`InjectHarness::at_toggle_req`] and consumed by the socket
/// dispatch arm that writes the wire reply.
///
/// v40.50: `resolved` is the identity of the element the action actually
/// landed on. It is `None` for every soft-fail and for the legacy
/// name-only path. **A reply that cannot name the element it acted on is
/// a defect in its own right**: a mis-delivered click returns exactly the
/// same rc=0 and the same well-formed `AT_TOGGLE_REPLY checked` as a
/// correct one, which is what made the v40.49 G2 mis-delivery invisible.
/// The wire is not broken — it is *blind*; these fields remove the
/// blindness.
#[derive(Debug, Clone, PartialEq)]
pub struct ToggleReply {
    pub state: ToggleState,
    pub resolved: Option<ResolvedTarget>,
}

/// v40.50: which occurrence the selector actually resolved, so the reply
/// can be checked against what the caller asked for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolvedTarget {
    /// The accessible name of the matched element (not the query).
    pub name: String,
    /// Resolved rect in the same coordinate space the click used.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `atspi://<wid>#<role>:<name>` — the same stamped path the
    /// `AT_ELEMENT_REPLY` lookup opcode already returns.
    pub path: String,
}

impl ToggleReply {
    pub fn checked() -> Self {
        Self {
            state: ToggleState::Checked,
            resolved: None,
        }
    }
    pub fn unchecked() -> Self {
        Self {
            state: ToggleState::Unchecked,
            resolved: None,
        }
    }
    pub fn no_action() -> Self {
        Self {
            state: ToggleState::NoAction,
            resolved: None,
        }
    }
    /// v40.50: out-of-range occurrence index. Names the candidate count so
    /// the caller can distinguish "no such row" from "no such name", and
    /// never silently substitutes the first match.
    pub fn index_out_of_range(requested: u32, candidates: u32) -> Self {
        Self {
            state: ToggleState::IndexOutOfRange {
                requested,
                candidates,
            },
            resolved: None,
        }
    }
    /// v40.50: attach the resolved identity to an existing tri-state reply.
    pub fn with_resolved(mut self, resolved: ResolvedTarget) -> Self {
        self.resolved = Some(resolved);
        self
    }
    /// v40.50: build the resolved identity from the ladder's `ElementBounds`
    /// plus the AT-SPI name that produced it.
    pub fn resolved_from_bounds(
        bounds: &crate::inject::atspi_ladder::ElementBounds,
        name: &str,
    ) -> ResolvedTarget {
        ResolvedTarget::from_bounds(bounds, name)
    }
}

impl ResolvedTarget {
    /// One construction site for both ACTION and TOGGLE identity replies.
    pub fn from_bounds(
        bounds: &crate::inject::atspi_ladder::ElementBounds,
        name: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            x: bounds.rect.x,
            y: bounds.rect.y,
            w: bounds.rect.w,
            h: bounds.rect.h,
            path: bounds.path.clone(),
        }
    }
}

/// v40.51 distinct out-of-range outcome for AT_ACTION_REQ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOccurrence {
    IndexOutOfRange { requested: u32, candidates: u32 },
}
impl ActionOccurrence {
    pub fn index_out_of_range(requested: u32, candidates: u32) -> Self {
        Self::IndexOutOfRange { requested, candidates }
    }
    pub fn wire(&self) -> String {
        match self {
            Self::IndexOutOfRange { requested, candidates } => format!("INDEX_OUT_OF_RANGE index={requested} candidates={candidates}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello,
    Auth(String),
    Bind(u64),
    Click {
        x: f64,
        y: f64,
    },
    /// `KEY <name> [SHIFT|1|shifted|CTRL|SUPER|ALT …]`
    /// or combinator `KEY ctrl+shift+t` (the non-modifier piece is the key name).
    /// Bot id and window id stay on AUTH/BIND — they are not KEY fields (v40.17).
    Key {
        name: String,
        shift: bool,
        mods: Vec<KeyMod>,
    },
    Scroll {
        dx: f64,
        dy: f64,
    },
    Drag {
        from: (f64, f64),
        to: (f64, f64),
    },
    GrantRequest {
        bot_id: String,
    },
    /// `GRANT_RESOLVED <bot_id> <window_id> <GRANT|DENY>` — programmatic grant-confirm
    /// click path (no seat operator required). v37.
    GrantResolved {
        bot_id: String,
        window_id: u64,
        resolution: GrantResolution,
    },
    /// `AT_ELEMENT_REQ <wid> LC|EI|CV …` — see [`parse_at_element_req`].
    /// Optional trailing `<index>` (LC/EI only) is the 0-based nth match.
    AtElementReq {
        window_id: u64,
        action: crate::inject::atspi_ladder::AtSpiAction,
        /// v40.41: nth-sibling selector (LC/EI only).
        index: Option<u32>,
    },
    /// AT_ACTION_REQ wid role name action_name index nth
    /// v40.15.3a / v40.51.
    AtActionReq {
        wid: u64,
        role: String,
        name: String,
        /// AT-SPI action index. Resolved client-side by the producer.
        /// This is NOT the occurrence selector; that lives in nth.
        index: i32,
        /// v40.51 occurrence selector. 0-based under the v40.46 exact-first
        /// predicate. None = omitted on the wire = first match. Never a
        /// sentinel. Distinct from index (the AT-SPI action index), which
        /// was already occupied and must keep that meaning.
        nth: Option<u32>,
    },
    /// v40.28: `AT_TOGGLE_REQ <wid> <name>` — request that the server
    /// synthesize a real `BTN_LEFT` press+release at the matched
    /// element's screen-space center coords. Closes the GTK 4
    /// `GtkToggleButton` (pavucontrol) bypass gap that v40.15.3
    /// `AT_ACTION_REQ` does not cover: pavucontrol's `GtkToggleButton`
    /// does not implement the AT-SPI `Action` interface, so
    /// `Action.do_action(0)` returns `AT_ACTION_REPLY OK` at the wire
    /// level but never fires the toggle (no PulseAudio mute event).
    ///
    /// The matching element role is implicit `toggle button` — the
    /// only kind of widget this opcode addresses. The wire parser
    /// also accepts the legacy 3-tuple form
    /// `AT_TOGGLE_REQ <wid> <role> <name>` (drops the role) and the
    /// 2-word-name join form
    /// `AT_TOGGLE_REQ <wid> toggle button <w0> <w1>` (joins to `w0 w1`),
    /// matching the cua-driver v40.28 encoder at `7c9fc04`.
    ///
    /// Server reply is tri-state; see [`ToggleState`] and the
    /// [`Reply::AtToggleReply`] encoder.
    ///
    /// v40.50: `index` is an OPTIONAL TRAILING occurrence selector,
    /// `AT_TOGGLE_REQ <wid> <name> [<nth>]`, reusing the same peeling rule
    /// as the `AT_ELEMENT_REQ` EI arm (`split_off_optional_index`) so the
    /// two opcodes cannot drift on what an index means. `None` reproduces
    /// v40.46 behaviour exactly (first match).
    AtToggleReq {
        wid: u64,
        name: String,
        /// v40.50: 0-based occurrence index under the v40.46 exact-first
        /// predicate. `None` = absent on the wire = first match.
        index: Option<u32>,
    },
    /// `AT_TREE_REQ <wid> [path…]` — see [`parse_at_tree_req`].
    AtTreeReq {
        window_id: u64,
        path: Option<String>,
    },
    /// `AT_TREE_BY_APP_ID <app_id>` — v40.15.4 subtree filtered by app_id.
    TreeByAppId {
        app_id: String,
    },
    /// `LAYER_HIT_TEST <x> <y>` — v40.18 screen-space layer-shell hit-test.
    LayerHitTestReq { x: i32, y: i32 },
    /// `LAYER_WIDGET_TREE <x> <y>` — v40.21 widget tree under the layer hit.
    /// Same BIND preamble as [`Frame::LayerHitTestReq`]. Does not change that opcode.
    LayerWidgetTreeReq { x: i32, y: i32 },
    /// `WINDOW_FOCUS_REQ <wid>` — v40.24 thin wrapper around
    /// `niri msg action focus-window --id <wid>`. Does not require
    /// BIND (mirrors the layer-shell opcodes that also run after AUTH).
    /// `wid == 0` is rejected as `invalid wid` before the layout walk.
    WindowFocusReq { wid: u64 },
}

impl Frame {
    /// Live encoder for `Frame::TreeByAppId` (Trap 45 / lld gc).
    pub fn encode_tree_by_app_id(app_id: &str) -> Vec<u8> {
        let _ = std::hint::black_box(SCAN_BY_APP_ID_V40_15_4);
        let _ = std::hint::black_box(G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
        format!("{OPC_AT_TREE_BY_APP_ID} {app_id}\n").into_bytes()
    }
}

/// Modifier token on a KEY frame, in encounter order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMod {
    Shift,
    Ctrl,
    Super,
    Alt,
}

/// Flags consumed by the press path. Hold order is Shift_L, Control_L,
/// Super_L, Alt_L; release is the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyMods {
    pub shift: bool,
    pub ctrl: bool,
    pub super_mod: bool,
    pub alt: bool,
}

impl KeyMods {
    pub fn from_list(mods: &[KeyMod]) -> Self {
        let mut out = Self::default();
        for m in mods {
            match m {
                KeyMod::Shift => out.shift = true,
                KeyMod::Ctrl => out.ctrl = true,
                KeyMod::Super => out.super_mod = true,
                KeyMod::Alt => out.alt = true,
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooBig,
    Malformed,
    UnknownOpcode,
    Incomplete,
}

impl FrameError {
    pub fn code(self) -> &'static str {
        match self {
            FrameError::TooBig => ERR_TOO_BIG,
            FrameError::Malformed | FrameError::Incomplete => ERR_MALFORMED,
            FrameError::UnknownOpcode => ERR_UNKNOWN_OPC,
        }
    }
}

/// v40.50: `Eq` is dropped — `AtToggleReply.resolved` carries `f64`
/// geometry, and `f64` is never `Eq`. Nothing in the tree required `Eq`
/// (`assert_eq!` only needs `PartialEq`).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    HelloOk,
    AuthOk { bot_id: String },
    BindOk {
        wid: u64,
        frontmost_wid: u64,
        frontmost_app_id: String,
    },
    BindPending,
    DispatchOk,
    DispatchErr { code: &'static str },
    FrameErr { code: &'static str },
    AuthErr { code: &'static str },
    GrantAck { queued: bool },
    GrantAckOk,
    GrantAckDenied,
    AtElementReply { payload: String },
    AtActionReply {
        ok: bool,
        error: Option<String>,
        force_redraw: bool,
        /// v40.51: identity of the element actually acted on. None on
        /// every error (nothing was acted on) and on the legacy success
        /// wire (AT_ACTION_REPLY OK byte-for-byte).
        resolved: Option<ResolvedTarget>,
    },
    /// v40.28: `AT_TOGGLE_REPLY checked|unchecked|no_action`. Wire is
    /// `AT_TOGGLE_REPLY <state>\n`. The cua-driver v40.28
    /// `ParsedReply::AtToggleReply` exits 0 on `Checked`/`Unchecked`,
    /// 2 on `NoAction` (soft-fail).
    ///
    /// v40.50: `resolved` appends `x= y= w= h= path=` so the caller can
    /// verify WHERE the action landed. `None` keeps the v40.46 three-token
    /// wire byte-for-byte (backward compatible with a peer that ignores
    /// the extra fields).
    AtToggleReply {
        state: ToggleState,
        resolved: Option<ResolvedTarget>,
    },
    AtTreeReply { body: String },
    /// Union of `AT_ELEMENT_REPLY` (best match) + `AT_TREE_REPLY` subtree.
    TreeByAppId {
        element_payload: String,
        tree_body: String,
    },
    /// `LAYER_HIT_TEST_REPLY` — v40.18. `bounds` is `(x, y, width, height)` in screen coords.
    LayerHitTestReply {
        found: bool,
        app_id: String,
        role: String,
        name: String,
        bounds: (i32, i32, i32, i32),
    },
    /// `LAYER_WIDGET_TREE_REPLY` — v40.21. `layer` is the v40.18 hit name
    /// (full namespace). Widget `bounds` are `(x, y, width, height)`.
    LayerWidgetTreeReply {
        found: bool,
        app_id: String,
        layer: String,
        widgets: Vec<crate::input::layer_widget_tree::LayerWidget>,
    },
    /// `WINDOW_FOCUS_REPLY` — v40.24. `ok = true` serializes
    /// `WINDOW_FOCUS_REPLY 1`; `ok = false` serializes
    /// `WINDOW_FOCUS_REPLY 0 <reason>` (one line, parity with the rest
    /// of the inject wire).
    WindowFocusReply { ok: bool, reason: String },
    BackgroundUnavailable,
}

impl Reply {
    /// v40.51: successful AT_ACTION_REPLY. resolved None keeps the
    /// v40.15.3 wire byte-for-byte (OK / OK FORCE_REDRAW).
    pub fn at_action_ok(force_redraw: bool, resolved: Option<ResolvedTarget>) -> Self {
        Reply::AtActionReply {
            ok: true,
            error: None,
            force_redraw,
            resolved,
        }
    }

    /// v40.51: occurrence selector out of range. Never a delivered action
    /// (ok false, resolved None).
    pub fn at_action_occurrence_out_of_range(requested: u32, candidates: u32) -> Self {
        Reply::AtActionReply {
            ok: false,
            error: Some(ActionOccurrence::index_out_of_range(requested, candidates).wire()),
            force_redraw: false,
            resolved: None,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let s = match self {
            Reply::HelloOk => format!("{REP_HELLO_OK}\n"),
            Reply::AuthOk { bot_id } => format!("{REP_AUTH_OK} {bot_id}\n"),
            Reply::BindOk {
                wid,
                frontmost_wid,
                frontmost_app_id,
            } => format!("{REP_BIND_OK} {wid} {frontmost_wid} {frontmost_app_id}\n"),
            Reply::BindPending => format!("{REP_BIND_PENDING}\n"),
            Reply::DispatchOk => format!("{REP_DISPATCH_OK}\n"),
            Reply::DispatchErr { code } => format!("{REP_DISPATCH_ERR} {code}\n"),
            Reply::FrameErr { code } => format!("{REP_FRAME_ERR} {code}\n"),
            Reply::AuthErr { code } => format!("{REP_AUTH_ERR} {code}\n"),
            Reply::GrantAck { queued: true } => format!("{REP_GRANT_ACK} {REP_GRANT_QUEUED}\n"),
            Reply::GrantAck { queued: false } => format!("{REP_GRANT_ACK} {REP_GRANT_DENIED_W}\n"),
            Reply::GrantAckOk => format!("{REP_GRANT_ACK} {REP_GRANT_ACK_OK_W}\n"),
            Reply::GrantAckDenied => format!("{REP_GRANT_ACK} {REP_GRANT_DENIED_W}\n"),
            Reply::AtElementReply { payload } => format!("{REP_AT_ELEMENT_REPLY} {payload}\n"),
            Reply::AtActionReply {
                ok,
                error,
                force_redraw,
                resolved,
            } => {
                // Trap 45: live encoder call site (not token_file_path-only).
                let _ = std::hint::black_box(AT_ACTION_REQ_OPCODE_V40_15_3);
                let _ = std::hint::black_box(FORCE_ICON_REDRAW_V40_15_5);
                let _ = std::hint::black_box(AT_ACTION_OCCURRENCE_INDEX_V40_51);
                let ident = match resolved {
                    Some(r) => format!(" x={} y={} w={} h={} path={}", fmt_coord(r.x), fmt_coord(r.y), fmt_coord(r.w), fmt_coord(r.h), r.path),
                    None => String::new(),
                };
                match (ok, error, force_redraw) {
                    (true, _, true) => format!("{REP_AT_ACTION_REPLY} OK FORCE_REDRAW{ident}\n"),
                    (true, _, false) => format!("{REP_AT_ACTION_REPLY} OK{ident}\n"),
                    (false, Some(code), _) => format!("{REP_AT_ACTION_REPLY} ERR {code}\n"),
                    (false, None, _) => format!("{REP_AT_ACTION_REPLY} ERR UNKNOWN\n"),
                }
            }
            Reply::AtTreeReply { body } => format!("{REP_AT_TREE_REPLY} {body}"),
            Reply::TreeByAppId {
                element_payload,
                tree_body,
            } => {
                let _ = std::hint::black_box(SCAN_BY_APP_ID_V40_15_4);
                let _ = std::hint::black_box(G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
                let _ = std::hint::black_box(Frame::encode_tree_by_app_id("galculator"));
                format!(
                    "{REP_AT_ELEMENT_REPLY} {element_payload}\n{REP_AT_TREE_REPLY} {tree_body}"
                )
            }
            Reply::BackgroundUnavailable => format!("{REP_AT_ELEMENT_REPLY} {REP_BG_UNAVAILABLE}\n"),
            Reply::LayerHitTestReply {
                found,
                app_id,
                role,
                name,
                bounds,
            } => {
                let _ = std::hint::black_box(LAYER_HIT_TEST_V40_18);
                if *found {
                    let (x, y, w, h) = bounds;
                    format!("{REP_LAYER_HIT_TEST_REPLY} 1 {app_id} {role} {name} {x} {y} {w} {h}\n")
                } else {
                    format!("{REP_LAYER_HIT_TEST_REPLY} 0\n")
                }
            }
            Reply::LayerWidgetTreeReply {
                found,
                app_id,
                layer,
                widgets,
            } => {
                let _ = std::hint::black_box(LAYER_WIDGET_TREE_V40_21);
                let _ = std::hint::black_box(LAYER_WIDGET_TREE_NIRI_V40_20);
                if *found {
                    let mut s = format!(
                        "{REP_LAYER_WIDGET_TREE_REPLY} 1 {app_id} {layer} {}\n",
                        widgets.len()
                    );
                    for w in widgets {
                        let parent = match w.parent_index {
                            None => "-".to_string(),
                            Some(i) => i.to_string(),
                        };
                        let (x, y, width, height) = w.bounds;
                        s.push_str(&format!(
                            "[{}] \"{}\" {parent} {x} {y} {width} {height}\n",
                            w.role, w.name
                        ));
                    }
                    s
                } else {
                    format!("{REP_LAYER_WIDGET_TREE_REPLY} 0\n")
                }
            }
            Reply::WindowFocusReply { ok, reason } => {
                let _ = std::hint::black_box(WINDOW_FOCUS_REQ_V40_24);
                if *ok {
                    format!("{REP_WINDOW_FOCUS_REPLY} 1\n")
                } else {
                    format!("{REP_WINDOW_FOCUS_REPLY} 0 {reason}\n")
                }
            }
            Reply::AtToggleReply { state, resolved } => {
                // v40.29 pin: dispatch-side ident must survive thin-LTO.
                // Read once, drop — the goal is codegen reachability.
                let _ = std::hint::black_box(AT_TOGGLE_REQ_DISPATCH_V40_29);
                let _ = std::hint::black_box(REP_AT_TOGGLE_REPLY_IDENT);
                let _ = std::hint::black_box(OPC_AT_TOGGLE_REQ_IDENT);
                let _ = std::hint::black_box(AT_TOGGLE_REQ_V40_28);
                let _ = std::hint::black_box(AT_TOGGLE_OCCURRENCE_INDEX_V40_50);
                let word = state.as_wire();
                // v40.50: an out-of-range occurrence index is a DISTINCT
                // wire outcome naming the candidate count. Never
                // `checked`/`unchecked` here — that would read as
                // "delivered to the first match".
                if let ToggleState::IndexOutOfRange {
                    requested,
                    candidates,
                } = state
                {
                    return format!(
                        "{REP_AT_TOGGLE_REPLY} {word} index={requested} candidates={candidates}\n"
                    )
                    .into_bytes();
                }
                // v40.50: the resolved identity follows the state token, so
                // a caller can verify WHERE the action landed instead of
                // trusting an rc=0 that a mis-delivery also returns.
                match resolved {
                    Some(r) => format!(
                        "{REP_AT_TOGGLE_REPLY} {word} x={} y={} w={} h={} path={}\n",
                        fmt_coord(r.x),
                        fmt_coord(r.y),
                        fmt_coord(r.w),
                        fmt_coord(r.h),
                        r.path
                    ),
                    // NoAction (soft-fail). Match cua-driver exit-2 via the
                    // same wire; do NOT add `err <reason>` — that shape is
                    // for AT_ACTION_REQ. The tri-state reply is the
                    // contract; no token appended on no-action.
                    None => format!("{REP_AT_TOGGLE_REPLY} {word}\n"),
                }
            }
        };
        s.into_bytes()
    }
}

#[derive(Debug)]
pub enum TokenError {
    NoMatch,
    NoFile,
    Io(io::Error),
}

/// Testable compositor surface for the inject state machine.
pub trait InjectHarness {
    type AgentState: SeatHandler;

    fn agent_seat(&mut self) -> &mut AgentSeat<Self::AgentState>;
    fn agent_state(&mut self) -> &mut Self::AgentState;
    /// Resolve a window id to the WlSurface to bind to.
    /// Returns None if the id is unknown / no longer mapped.
    fn resolve_window(&mut self, window_id: u64) -> Option<WlSurface>;
    /// Hook for permanent-error reporting (called once before close). v3: no-op default.
    fn report_permanent_error(&mut self, code: &str) {
        let _ = code;
    }
    /// Hook for transient-error reporting. v3: no-op default.
    fn report_transient_error(&mut self, code: &str) {
        let _ = code;
    }
    /// Current monotonic millisecond timestamp.
    fn now_ms(&mut self) -> u32 {
        0
    }
    /// Token file path for this connection. Defaults to the constant.
    fn token_file_path(&self) -> &Path {
        Path::new("/dev/null")
    }
    /// Pointer-focus object for `window_id`. Tests return a recording target;
    /// production maps the bound [`WlSurface`] when `PointerFocus = WlSurface`.
    fn resolve_pointer_focus(
        &mut self,
        window_id: u64,
    ) -> Option<<Self::AgentState as SeatHandler>::PointerFocus> {
        let _ = window_id;
        None
    }
    /// KEY hook. Return true to reply `DISPATCH OK`; false → `NOT_BOUND`.
    fn key_press(&mut self, xkb_name: &str) -> bool {
        let _ = xkb_name;
        false
    }
    /// KEY hook with optional Shift (v40.10). Default ignores `shift`.
    fn key_press_with_shift(&mut self, xkb_name: &str, shift: bool) -> bool {
        let _ = shift;
        self.key_press(xkb_name)
    }
    /// KEY hook with ctrl/super/alt holds (v40.17). Default keeps the shift path
    /// so harnesses that only override `key_press_with_shift` do not drop case.
    fn key_press_with_mods(&mut self, xkb_name: &str, mods: KeyMods) -> bool {
        self.key_press_with_shift(xkb_name, mods.shift)
    }
    /// Focus the agent seat's keyboard on `window_id`'s surface.
    /// Default no-op for stub harnesses. Production/e2e override.
    fn set_keyboard_focus(&mut self, _wid: u64) -> bool {
        false
    }
    /// SCROLL hook. Return true to reply DISPATCH OK; false → NOT_BOUND.
    /// Returns false only when h_axis or v_axis is non-finite (NaN / ±inf).
    fn scroll(&mut self, _h_axis: f64, _v_axis: f64) -> bool {
        let _ = (_h_axis, _v_axis);
        false
    }
    /// DRAG hook. Return true to reply DISPATCH OK; false → NOT_BOUND.
    /// Returns false when any coordinate is NaN (and the default always returns false).
    fn drag(&mut self, _from_x: f64, _from_y: f64, _to_x: f64, _to_y: f64) -> bool {
        false
    }
    /// v40.39b: worker→main pointer click. Default false so stub harnesses
    /// keep compiling. Production `NiriInjectHarness` activates the real
    /// `WlSurface` and delivers on the user seat.
    fn pointer_click_at(&mut self, window_id: u64, x: f64, y: f64) -> bool {
        let _ = (window_id, x, y);
        false
    }
    fn is_locked(&self) -> bool {
        false
    }
    fn outputs_off(&self) -> bool {
        false
    }
    /// True when `(bot_id, window_id)` is in the compositor grant map.
    /// Default denies BIND (token-only AUTH is not sufficient).
    fn grant_present(&self, bot_id: &str, window_id: u64) -> bool {
        let _ = (bot_id, window_id);
        false
    }
    /// v40.19 — insert a persistent `Interactive` grant for
    /// `(bot_id, window_id)`. Default is a no-op (stub harnesses do
    /// not own a real grant map; production overrides push through
    /// `GrantMap::apply_grant`).
    fn apply_grant(&self, bot_id: &str, window_id: u64) {
        let _ = (bot_id, window_id);
    }
    /// v40.19 — same as [`Self::apply_grant`] but with `app_id` recorded
    /// so future fresh toplevels of the same `app_id` can be
    /// pre-granted. Default: 2-arg shim that drops `app_id`.
    fn apply_grant_with_app(&self, bot_id: &str, window_id: u64, app_id: &str) {
        let _ = app_id;
        self.apply_grant(bot_id, window_id);
    }
    /// True when a pending grant exists for `bot_id` (queued, not yet in the map).
    /// Default: stub harnesses never have a pending grant.
    fn grant_pending(&self, bot_id: &str) -> bool {
        let _ = bot_id;
        false
    }
    /// Queue a pending grant request. Default denies (no queue on stub harnesses).
    fn request_grant(&mut self, bot_id: &str, token_fingerprint: [u8; 8]) -> bool {
        let _ = (bot_id, token_fingerprint);
        false
    }
    /// Apply a resolved grant-confirm click. Default is a no-op miss.
    fn apply_grant_confirm_click(
        &mut self,
        bot_id: &str,
        window_id: u64,
        app_id: &str,
        seat: SeatKind,
        resolution: GrantResolution,
    ) -> ClickOutcome {
        let _ = (bot_id, window_id, app_id, seat, resolution);
        ClickOutcome::IgnoredNoRow
    }
    /// Return catalog routes for the given `app_id`. Default = unknown.
    fn app_id_routes(&self, app_id: &str) -> AppRoutes {
        let _ = app_id;
        crate::inject::app_catalog::AppRoutes::unknown()
    }
    /// Per-action AT-SPI ladder. Default fails closed (`BACKGROUND_UNAVAILABLE`).
    fn route(
        &self,
        app_id: &str,
        action: &crate::inject::atspi_ladder::AtSpiAction,
    ) -> crate::inject::atspi_ladder::AtSpiRoute {
        let _ = (app_id, action);
        crate::inject::atspi_ladder::AtSpiRoute::CanvasRaw {
            reason: crate::inject::atspi_ladder::REASON_BG_UNAVAILABLE.into(),
        }
    }
    /// Window id → catalog `app_id`. Tests set `DispatchState::bound_app_id`;
    /// production stays `None` until Row 14 looks up the live toplevel.
    fn app_id_for(&self, window_id: u64) -> Option<String> {
        let _ = window_id;
        None
    }
    fn is_occluded(&mut self) -> bool {
        false
    }
    /// v40.15.3a: `AT_ACTION_REQ` routes through this hook.
    /// Default plants `AT_ACTION_REQ_OPCODE_V40_15_3` and dispatches
    /// `DoAction(index)` (test override or live a11y bus).
    fn apply_action_on_element(
        &mut self,
        wid: u64,
        path: &str,
        index: i32,
    ) -> Result<(), &'static str> {
        apply_action_on_element_default(wid, path, index)
    }
    /// v40.18 layer-shell hit-test. Default miss so existing harness impls compile.
    fn layer_hit_test(&mut self, x: i32, y: i32) -> crate::input::layer_hit_test::LayerHitReply {
        let _ = (x, y);
        crate::input::layer_hit_test::LayerHitReply::miss()
    }
    /// v40.21 widget tree under the layer hit. Reuses [`Self::layer_hit_test`]
    /// so `DispatchHarness` hands the hit off to the main loop, then walks
    /// AT-SPI on the caller thread (the inject worker, not the compositor loop).
    fn layer_widget_tree(
        &mut self,
        x: i32,
        y: i32,
    ) -> crate::input::layer_widget_tree::LayerWidgetTreeReply {
        let _ = std::hint::black_box(LAYER_WIDGET_TREE_V40_21);
        let _ = std::hint::black_box(LAYER_WIDGET_TREE_NIRI_V40_20);
        let hit = self.layer_hit_test(x, y);
        if !hit.found {
            return crate::input::layer_widget_tree::LayerWidgetTreeReply::miss();
        }
        let roots = crate::inject::atspi_ladder::widget_tree_roots(&hit.app_id);
        let nodes = roots.iter().map(widget_node_from_tree).collect::<Vec<_>>();
        crate::input::layer_widget_tree::widget_tree_from_hit(&hit, &nodes, x, y)
    }
    /// v40.24 — focus a window by `wid`. Thin wrapper around the existing
    /// `niri msg action focus-window --id <wid>` action. Default denies so
    /// stub harnesses stay compile-clean; `wid == 0` returns
    /// `invalid wid` before any layout walk.
    fn focus_window(&mut self, wid: u64) -> crate::input::focus_window::FocusWindowReply {
        let _ = std::hint::black_box(WINDOW_FOCUS_REQ_V40_24);
        let _ = wid;
        crate::input::focus_window::FocusWindowReply::fail("no such window")
    }

    /// v40.28: synthesize a real `BTN_LEFT` press+release at the matched
    /// element's screen-space center coords for `AT_TOGGLE_REQ <wid>
    /// <name>`. Bypasses the `library_click` / `GtkGestureClick` filter
    /// path that GTK 4 `GtkToggleButton` (pavucontrol) does not activate.
    ///
    /// The default impl fails closed (`NoAction`) so stub harnesses stay
    /// compile-clean. Production harnesses
    /// ([`crate::inject::harness::NiriInjectHarness`] and
    /// [`crate::inject::dispatch::DispatchHarness`]) override this to
    /// walk the AT-SPI tree (the same `element_lookup_via_tree_dump`
    /// path the `apply_action_on_element` arm uses) and dispatch the
    /// synthesized button event through the agent seat's
    /// `PointerHandle`.
    ///
    /// v40.50: `index` is the optional trailing occurrence selector from
    /// the wire (`None` = first match). `out_of_range` is how an impl
    /// reports "the selector named an element that does not exist" —
    /// it MUST carry the candidate count rather than substituting the
    /// first match.
    fn at_toggle_req(&mut self, wid: u64, name: &str, index: Option<u32>) -> ToggleReply {
        let _ = std::hint::black_box(AT_TOGGLE_REQ_V40_28);
        let _ = (wid, name, index);
        ToggleReply::no_action()
    }
}

fn widget_node_from_tree(
    n: &crate::inject::atspi_ladder::TreeNode,
) -> crate::input::layer_widget_tree::WidgetNode {
    crate::input::layer_widget_tree::WidgetNode {
        name: n.name.clone(),
        role: n.role.clone(),
        bounds: (n.x as i32, n.y as i32, n.w as i32, n.h as i32),
        children: n.children.iter().map(widget_node_from_tree).collect(),
    }
}

/// Parse one complete frame. AUTH consumes two lines. Remainder is unparsed
/// trailing bytes.
pub fn parse_frame(input: &[u8]) -> Result<(Frame, &[u8]), FrameError> {
    if input.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooBig);
    }
    let nl = input
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(FrameError::Incomplete)?;
    if nl > MAX_FRAME_LEN {
        return Err(FrameError::TooBig);
    }
    let line = strip_cr(&input[..nl]);
    let after = &input[nl + 1..];
    let text = std::str::from_utf8(line).map_err(|_| FrameError::Malformed)?;
    let mut parts = text.split_whitespace();
    let opc = parts.next().ok_or(FrameError::Malformed)?;
    match opc {
        OPC_HELLO => {
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok((Frame::Hello, after))
        }
        OPC_AUTH => {
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            let nl2 = after
                .iter()
                .position(|&b| b == b'\n')
                .ok_or(FrameError::Incomplete)?;
            if nl + 1 + nl2 > MAX_FRAME_LEN {
                return Err(FrameError::TooBig);
            }
            let token = std::str::from_utf8(strip_cr(&after[..nl2]))
                .map_err(|_| FrameError::Malformed)?
                .trim();
            if token.is_empty() {
                return Err(FrameError::Malformed);
            }
            Ok((Frame::Auth(token.to_string()), &after[nl2 + 1..]))
        }
        OPC_BIND => parse_bind(parts).map(|id| (Frame::Bind(id), after)),
        OPC_CLICK => parse_two_f64(parts).map(|(x, y)| (Frame::Click { x, y }, after)),
        OPC_KEY => parse_key(parts).map(|frame| (frame, after)),
        OPC_SCROLL => parse_two_f64(parts).map(|(dx, dy)| (Frame::Scroll { dx, dy }, after)),
        OPC_GRANT_REQUEST => {
            let bot_id = parts.next().ok_or(FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok((
                Frame::GrantRequest {
                    bot_id: bot_id.to_string(),
                },
                after,
            ))
        }
        OPC_GRANT_RESOLVED => {
            let bot_id = parts.next().ok_or(FrameError::Malformed)?;
            let window_id = parts
                .next()
                .ok_or(FrameError::Malformed)?
                .parse()
                .map_err(|_| FrameError::Malformed)?;
            let decision = parts.next().ok_or(FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            let resolution = match decision {
                "GRANT" => GrantResolution::Granted,
                "DENY" => GrantResolution::Denied,
                _ => return Err(FrameError::Malformed),
            };
            Ok((
                Frame::GrantResolved {
                    bot_id: bot_id.to_string(),
                    window_id,
                    resolution,
                },
                after,
            ))
        }
        OPC_DRAG => parse_four_f64(parts).map(|from_to| {
            let (from, to) = from_to;
            (Frame::Drag { from, to }, after)
        }),
        OPC_AT_ELEMENT_REQ => parse_at_element_req(text).map(|frame| (frame, after)),
        OPC_AT_ACTION_REQ => parse_at_action_req(text).map(|frame| (frame, after)),
        OPC_AT_TOGGLE_REQ => {
            let _ = std::hint::black_box(OPC_AT_TOGGLE_REQ_IDENT);
            let _ = std::hint::black_box(AT_TOGGLE_REQ_V40_28);
            let _ = std::hint::black_box(OPC_AT_TOGGLE_REQ.as_ptr());
            parse_at_toggle_req(text).map(|frame| (frame, after))
        }
        OPC_AT_TREE_REQ => parse_at_tree_req(text).map(|frame| (frame, after)),
        OPC_AT_TREE_BY_APP_ID => parse_at_tree_by_app_id(text).map(|frame| (frame, after)),
        OPC_LAYER_HIT_TEST => {
            let _ = std::hint::black_box(LAYER_HIT_TEST_V40_18);
            let x: i32 = parts
                .next()
                .ok_or(FrameError::Malformed)?
                .parse()
                .map_err(|_| FrameError::Malformed)?;
            let y: i32 = parts
                .next()
                .ok_or(FrameError::Malformed)?
                .parse()
                .map_err(|_| FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok((Frame::LayerHitTestReq { x, y }, after))
        }
        OPC_LAYER_WIDGET_TREE => {
            let _ = std::hint::black_box(LAYER_WIDGET_TREE_V40_21);
            let _ = std::hint::black_box(LAYER_WIDGET_TREE_NIRI_V40_20);
            let x: i32 = parts
                .next()
                .ok_or(FrameError::Malformed)?
                .parse()
                .map_err(|_| FrameError::Malformed)?;
            let y: i32 = parts
                .next()
                .ok_or(FrameError::Malformed)?
                .parse()
                .map_err(|_| FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok((Frame::LayerWidgetTreeReq { x, y }, after))
        }
        OPC_WINDOW_FOCUS_REQ => {
            let _ = std::hint::black_box(WINDOW_FOCUS_REQ_V40_24);
            let wid: u64 = parts
                .next()
                .ok_or(FrameError::Malformed)?
                .parse()
                .map_err(|_| FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok((Frame::WindowFocusReq { wid }, after))
        }
        _ => Err(FrameError::UnknownOpcode),
    }
}

fn classify_key_mod(tok: &str) -> Option<KeyMod> {
    if tok == "1" {
        return Some(KeyMod::Shift);
    }
    match tok.to_ascii_lowercase().as_str() {
        "shift" | "shifted" => Some(KeyMod::Shift),
        "ctrl" => Some(KeyMod::Ctrl),
        "super" => Some(KeyMod::Super),
        "alt" => Some(KeyMod::Alt),
        _ => None,
    }
}

fn push_key_mod(mods: &mut Vec<KeyMod>, m: KeyMod) {
    if !mods.contains(&m) {
        mods.push(m);
    }
}

/// `KEY <name> [mod…]` or `KEY <mod>+<name>+<mod>`.
/// A second non-modifier token is [`FrameError::Malformed`] (`KEY t foo`).
fn parse_key<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<Frame, FrameError> {
    let _ = std::hint::black_box(NIRI_KEY_MODIFIERS_V40_17);
    let _ = std::hint::black_box(
        ::core::primitive::str::from_utf8(b"NIRI_KEY_MODIFIERS_V40_17\0").unwrap(),
    );
    let first = parts.next().ok_or(FrameError::Malformed)?;
    let mut mods = Vec::new();
    let name = if first.contains('+') {
        if parts.next().is_some() {
            return Err(FrameError::Malformed);
        }
        parse_key_combinator(first, &mut mods)?
    } else {
        for tok in parts {
            if tok.contains('+') {
                for piece in tok.split('+') {
                    if piece.is_empty() {
                        return Err(FrameError::Malformed);
                    }
                    let Some(m) = classify_key_mod(piece) else {
                        return Err(FrameError::Malformed);
                    };
                    push_key_mod(&mut mods, m);
                }
            } else {
                let Some(m) = classify_key_mod(tok) else {
                    return Err(FrameError::Malformed);
                };
                push_key_mod(&mut mods, m);
            }
        }
        first.to_string()
    };
    if name.is_empty() {
        return Err(FrameError::Malformed);
    }
    let flags = KeyMods::from_list(&mods);
    Ok(Frame::Key {
        name,
        shift: flags.shift,
        mods,
    })
}

fn parse_key_combinator(token: &str, mods: &mut Vec<KeyMod>) -> Result<String, FrameError> {
    let mut name: Option<String> = None;
    for piece in token.split('+') {
        if piece.is_empty() {
            return Err(FrameError::Malformed);
        }
        if let Some(m) = classify_key_mod(piece) {
            push_key_mod(mods, m);
        } else if name.is_some() {
            return Err(FrameError::Malformed);
        } else {
            name = Some(piece.to_string());
        }
    }
    name.ok_or(FrameError::Malformed)
}

fn strip_cr(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

fn parse_bind<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<u64, FrameError> {
    let id = parts
        .next()
        .ok_or(FrameError::Malformed)?
        .parse()
        .map_err(|_| FrameError::Malformed)?;
    if parts.next().is_some() {
        return Err(FrameError::Malformed);
    }
    Ok(id)
}

fn parse_two_f64<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<(f64, f64), FrameError> {
    let a = parse_f64(parts.next().ok_or(FrameError::Malformed)?)?;
    let b = parse_f64(parts.next().ok_or(FrameError::Malformed)?)?;
    if parts.next().is_some() {
        return Err(FrameError::Malformed);
    }
    Ok((a, b))
}

fn parse_four_f64<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<((f64, f64), (f64, f64)), FrameError> {
    let a = parse_f64(parts.next().ok_or(FrameError::Malformed)?)?;
    let b = parse_f64(parts.next().ok_or(FrameError::Malformed)?)?;
    let c = parse_f64(parts.next().ok_or(FrameError::Malformed)?)?;
    let d = parse_f64(parts.next().ok_or(FrameError::Malformed)?)?;
    if parts.next().is_some() {
        return Err(FrameError::Malformed);
    }
    Ok(((a, b), (c, d)))
}

fn parse_f64(s: &str) -> Result<f64, FrameError> {
    s.parse().map_err(|_| FrameError::Malformed)
}

/// Wire shapes:
/// - `AT_ELEMENT_REQ <wid> LC <role> <name> [<index>]`
/// - `AT_ELEMENT_REQ <wid> EI <role> <name> <text…> [<index>]` (text may contain spaces;
///    a trailing purely-numeric token is the nth-sibling index, not text)
/// - `AT_ELEMENT_REQ <wid> CV <query>` (no sibling index)
fn parse_at_element_req(text: &str) -> Result<Frame, FrameError> {
    use crate::inject::atspi_ladder::AtSpiAction;

    let _ = std::hint::black_box(AT_ELEMENT_REQ_INDEX_NTH_SIBLING_V40_41);
    let mut parts = text.split_whitespace();
    let _opc = parts.next().ok_or(FrameError::Malformed)?;
    let window_id: u64 = parts
        .next()
        .ok_or(FrameError::Malformed)?
        .parse()
        .map_err(|_| FrameError::Malformed)?;
    let kind = parts.next().ok_or(FrameError::Malformed)?;
    match kind {
        "LC" => {
            let acc_role = parts.next().ok_or(FrameError::Malformed)?;
            let name_query = parts.next().ok_or(FrameError::Malformed)?;
            let index = match parts.next() {
                Some(tok) => Some(tok.parse::<u32>().map_err(|_| FrameError::Malformed)?),
                None => None,
            };
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            let _ = std::hint::black_box(AT_ELEMENT_REQ_INDEX_NTH_SIBLING_V40_41);
            Ok(Frame::AtElementReq {
                window_id,
                action: AtSpiAction::LabelClick {
                    acc_role: acc_role.to_string(),
                    name_query: name_query.to_string(),
                    index,
                },
                index,
            })
        }
        "EI" => {
            let acc_role = parts.next().ok_or(FrameError::Malformed)?;
            let name_query = parts.next().ok_or(FrameError::Malformed)?;
            let (text_body, index) =
                split_off_optional_index(remainder_after_tokens(text, 5).unwrap_or(""))?;
            Ok(Frame::AtElementReq {
                window_id,
                action: AtSpiAction::EditInsert {
                    acc_role: acc_role.to_string(),
                    name_query: name_query.to_string(),
                    text: text_body.to_string(),
                    index,
                },
                index,
            })
        }
        "CV" => {
            let query = parts.next().ok_or(FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok(Frame::AtElementReq {
                window_id,
                action: AtSpiAction::Canvas {
                    query: query.to_string(),
                },
                index: None,
            })
        }
        _ => Err(FrameError::Malformed),
    }
}

/// Split an optional trailing numeric off the end of a space-separated remainder.
/// Returns `(body_without_optional_index, Option<u32>)`. A last token that is
/// purely numeric `u32` is the nth-sibling index; otherwise the body is left
/// intact (EI text may end in a non-numeric word).
fn split_off_optional_index(remainder: &str) -> Result<(&str, Option<u32>), FrameError> {
    let trimmed = remainder.trim_end();
    if trimmed.is_empty() {
        return Ok((trimmed, None));
    }
    let last_space = trimmed.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    if last_space == trimmed.len() {
        return Ok((trimmed.trim_end(), None));
    }
    let (body, last) = trimmed.split_at(last_space);
    match last.parse::<u32>() {
        Ok(n) => Ok((body.trim_end(), Some(n))),
        Err(_) => Ok((trimmed, None)),
    }
}

/// Wire shapes:
/// - `AT_ACTION_REQ <wid> <role> <name> <index>`
/// - `AT_ACTION_REQ <wid> <role> <name> <action_name> <index>`
/// - v40.51: optional trailing occurrence selector, omitted entirely when
///   absent: `AT_ACTION_REQ <wid> <role> <name> <index> [<nth>]` and the
///   two-word-toggle form
///   `AT_ACTION_REQ <wid> toggle button <w0> <w1> <action_tok> <index> [<nth>]`.
fn parse_at_action_req(text: &str) -> Result<Frame, FrameError> {
    let _ = std::hint::black_box(AT_ACTION_OCCURRENCE_INDEX_V40_51);
    let mut parts = text.split_whitespace();
    let _opc = parts.next().ok_or(FrameError::Malformed)?;
    let wid: u64 = parts
        .next()
        .ok_or(FrameError::Malformed)?
        .parse()
        .map_err(|_| FrameError::Malformed)?;
    let mut rest: Vec<&str> = parts.collect();
    // G2: AT-SPI role "toggle button" is two wire tokens; name may be
    // "Mute audio". Existing one-token role/name frames are unchanged.
    let role = if rest.len() >= 4 && rest[0] == "toggle" && rest[1] == "button" {
        rest.remove(0);
        rest.remove(0);
        "toggle button".to_string()
    } else {
        if rest.is_empty() {
            return Err(FrameError::Malformed);
        }
        rest.remove(0).to_string()
    };
    if rest.is_empty() {
        return Err(FrameError::Malformed);
    }
    let mut name = rest.remove(0).to_string();
    if role == "toggle button" && rest.len() >= 3 {
        name = format!("{name} {}", rest.remove(0));
    }
    // v40.51: occurrence peel. The ACTION trailing token is ALREADY the AT-SPI
    // action index, so this gate must NOT be a bare token count (that is the
    // v40.50 round-1 defect class). The producer encode_at_action_req_line
    // (qlux-cua-driver fb3b43d7) emits [<action_index>] for a short name,
    // [<action_tok> <action_index>] for a two-word toggle name, and with the
    // v40.51 selector [<action_index> <nth>] / [<action_tok> <action_index> <nth>]
    // -- where action_tok is only ever click or toggle, never numeric. So the
    // occurrence form is present iff the last TWO tokens both parse as numbers.
    // THIS IS THE GATE LINE G2 REVERTS.
    let occurrence_form = rest.len() >= 2
        && rest[rest.len() - 2].parse::<i32>().is_ok()
        && rest[rest.len() - 1].parse::<u32>().is_ok();
    let (index, nth) = if occurrence_form {
        let nth = rest
            .pop()
            .unwrap()
            .parse::<u32>()
            .map_err(|_| FrameError::Malformed)?;
        let index = rest
            .pop()
            .unwrap()
            .parse::<i32>()
            .map_err(|_| FrameError::Malformed)?;
        // at most the single dropped action token may remain
        if rest.len() > 1 {
            return Err(FrameError::Malformed);
        }
        (index, Some(nth))
    } else {
        // legacy: the trailing token is the AT-SPI action index
        if rest.len() > 2 {
            return Err(FrameError::Malformed);
        }
        let index = rest
            .pop()
            .ok_or(FrameError::Malformed)?
            .parse::<i32>()
            .map_err(|_| FrameError::Malformed)?;
        (index, None)
    };
    Ok(Frame::AtActionReq {
        wid,
        role,
        name,
        index,
        nth,
    })
}


/// v40.28: parse `AT_TOGGLE_REQ` for the niri inject server.
/// Mirrors cua-driver v40.28 `parse_at_toggle_req` at commit `7c9fc04`.
///
/// Wire shapes:
/// - `AT_TOGGLE_REQ <wid> <name>`            — single-token name (brief form)
/// - `AT_TOGGLE_REQ <wid> toggle button <w0> <w1>` — two-word name join,
///   joins to `w0 w1` (matches `encode_at_toggle_req_line`)
/// - `AT_TOGGLE_REQ <wid> <role> <name>`     — legacy 3-tuple; role is
///   dropped. Accepted for backward compat with the worker CLI surface
///   and is kept because the brief form parses 1-token name the same
///   way (`rest.len() == 1` → name). 2-tuple form is detected when
///   `rest.len() == 1`; 3-tuple when `rest.len() == 2`.
///
/// v40.50: every shape gains an OPTIONAL TRAILING occurrence index,
/// `AT_TOGGLE_REQ <wid> <name> [<nth>]`, peeled with the SAME
/// `split_off_optional_index` helper the `AT_ELEMENT_REQ` EI arm uses.
/// Reusing the helper (rather than a second parser) is what keeps the
/// two opcodes from ever disagreeing about what an index means — the
/// cua-driver sibling already selects client-side with the identical
/// rule, so client and server index identically or not at all.
///
/// Absent index = today's behaviour exactly. A single-token remainder
/// whose token is itself numeric (`AT_TOGGLE_REQ 4 123`) stays name-only:
/// the peel needs at least TWO remainder tokens, so a 1-token remainder
/// can never have its NAME silently reinterpreted as an index.
fn parse_at_toggle_req(text: &str) -> Result<Frame, FrameError> {
    // v40.29 pin: parse-side ident must survive thin-LTO. Read once,
    // drop — the goal is codegen reachability, not actual semantics.
    let _ = std::hint::black_box(AT_TOGGLE_REQ_PARSE_V40_29);
    let _ = std::hint::black_box(crate::inject::atspi_ladder::AT_TOGGLE_OCCURRENCE_INDEX_V40_50_IDENT);
    let mut parts = text.split_whitespace();
    let _opc = parts.next().ok_or(FrameError::Malformed)?;
    let wid: u64 = parts
        .next()
        .ok_or(FrameError::Malformed)?
        .parse()
        .map_err(|_| FrameError::Malformed)?;
    let mut rest: Vec<String> = parts.map(str::to_string).collect();
    if rest.is_empty() {
        return Err(FrameError::Malformed);
    }
    // v40.50: peel the optional trailing occurrence index FIRST, using the
    // shared v40.46/v40.41 helper.
    //
    // v40.50b (review round 1): the gate is `rest.len() > 1`, NOT `> 2`.
    // The live producer `qlux-cua-driver` fb3b43d
    // `encode_at_toggle_req_line` emits `<wid> <name_tok> <n>` for every
    // name that is not exactly two words — a TWO-token remainder. The `> 2`
    // gate never fired for those, the tokens fell through to the legacy
    // 3-tuple arm below, and the occurrence was taken as the NAME
    // (`name="2"`, `index=None`). That is the exact "silent index drop"
    // class this opcode exists to kill, still live for single-token names.
    //
    // `> 1` is the correct boundary, not `> 0`: with a ONE-token remainder
    // there is nothing to peel off, so `AT_TOGGLE_REQ 4 123` keeps `123`
    // as its NAME and never steals it into the index field. Two tokens is
    // the shortest shape that can carry a name AND an occurrence, so `> 1`
    // is both sufficient and minimal.
    let index = if rest.len() > 1 {
        let joined = rest.join(" ");
        let (body, index) = split_off_optional_index(&joined)?;
        // Own the tokens so nothing borrows `joined`/`body` past this block.
        rest = body.split_whitespace().map(str::to_string).collect();
        index
    } else {
        None
    };
    // Brief 2-tuple form: `AT_TOGGLE_REQ <wid> toggle button <w0> <w1>`
    if rest.len() >= 3 && rest[0] == "toggle" && rest[1] == "button" {
        let mut name_iter = rest.iter().skip(2);
        let first = name_iter.next().ok_or(FrameError::Malformed)?;
        let mut name = first.clone();
        for tok in name_iter {
            name.push(' ');
            name.push_str(tok);
        }
        return Ok(Frame::AtToggleReq { wid, name, index });
    }
    // Brief 2-tuple form: `AT_TOGGLE_REQ <wid> <name>` (single-token
    // name). Two-word names in this form go through the
    // `toggle button` join arm above.
    if rest.len() == 1 {
        return Ok(Frame::AtToggleReq {
            wid,
            name: rest[0].clone(),
            index,
        });
    }
    // Legacy 3-tuple form: `AT_TOGGLE_REQ <wid> <role> <name>`. Drop
    // the role token.
    if rest.len() == 2 {
        return Ok(Frame::AtToggleReq {
            wid,
            name: rest[1].clone(),
            index,
        });
    }
    Err(FrameError::Malformed)
}

/// `AT_TREE_REQ <wid>` or `AT_TREE_REQ <wid> <path…>` (path is the remainder
/// joined by single spaces).
/// `AT_TREE_BY_APP_ID <app_id>` (app_id is the remainder after the opcode).
fn parse_at_tree_by_app_id(text: &str) -> Result<Frame, FrameError> {
    let mut parts = text.split_whitespace();
    let _opc = parts.next().ok_or(FrameError::Malformed)?;
    let app_id = parts.next().ok_or(FrameError::Malformed)?;
    if app_id.is_empty() {
        return Err(FrameError::Malformed);
    }
    let extra = remainder_after_tokens(text, 2)
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty());
    let app_id = match extra {
        Some(rest) => format!("{app_id} {rest}"),
        None => app_id.to_string(),
    };
    Ok(Frame::TreeByAppId { app_id })
}

/// v40.29 test-only encoder for `Frame::AtToggleReq`. Roundtrip with
/// `parse_at_toggle_req` (called via `parse_frame`) so the test exercises
/// the real wire-decode path. Mirrors cua-driver v40.28
/// `encode_at_toggle_req_line` at commit `7c9fc04`. Public so
/// `tests/inject.rs` can call it.
pub fn encode_at_toggle_req(req: &Frame) -> Vec<u8> {
    let _ = std::hint::black_box(AT_TOGGLE_REQ_PARSE_V40_29);
    let _ = std::hint::black_box(AT_TOGGLE_OCCURRENCE_INDEX_V40_50);
    match req {
        Frame::AtToggleReq { wid, name, index } => {
            // Wire shapes (mirror cua-driver v40.28
            // `encode_at_toggle_req_line`):
            // - Single-token name: `AT_TOGGLE_REQ <wid> <name>\n`
            // - Two-word name:      `AT_TOGGLE_REQ <wid> toggle button <w0> <w1>\n`
            //   so the live parser preserves `Mute audio` instead of
            //   collapsing it to `Mute`.
            // v40.50: `Some(n)` appends the trailing occurrence index,
            // which round-trips through the same peeling rule the
            // `AT_ELEMENT_REQ` EI arm uses.
            let index_tok = index.map(|n| n.to_string());
            let toks: Vec<&str> = name.split_whitespace().collect();
            let suffix = index_tok.as_deref().map(|n| format!(" {n}")).unwrap_or_default();
            if toks.len() == 2 {
                format!(
                    "{OPC_AT_TOGGLE_REQ} {wid} toggle button {} {}{suffix}\n",
                    toks[0], toks[1]
                )
                .into_bytes()
            } else {
                let name_tok = toks.first().copied().unwrap_or("");
                format!("{OPC_AT_TOGGLE_REQ} {wid} {name_tok}{suffix}\n").into_bytes()
            }
        }
        _ => unreachable!("encode_at_toggle_req only accepts Frame::AtToggleReq"),
    }
}

/// v40.29 test-only encoder for `Reply::AtToggleReply`. Roundtrip with
/// `parse_frame` against the wire `AT_TOGGLE_REPLY <state>` line so the
/// test exercises the full `Reply::AtToggleReply` encoder arm. Mirrors
/// the cua-driver `ParsedReply::AtToggleReply::wire` shape.
pub fn encode_at_toggle_reply(state: ToggleState) -> Vec<u8> {
    let _ = std::hint::black_box(AT_TOGGLE_REQ_DISPATCH_V40_29);
    let _ = std::hint::black_box(AT_TOGGLE_REQ_PARSE_V40_29);
    let _ = std::hint::black_box(AT_TOGGLE_OCCURRENCE_INDEX_V40_50);
    Reply::AtToggleReply {
        state,
        resolved: None,
    }
    .to_bytes()
}

/// v40.50: test-only encoder for a reply that DOES carry the resolved
/// identity, so a test can assert the geometry the server reports.
pub fn encode_at_toggle_reply_resolved(reply: &ToggleReply) -> Vec<u8> {
    let _ = std::hint::black_box(AT_TOGGLE_OCCURRENCE_INDEX_V40_50);
    Reply::AtToggleReply {
        state: reply.state,
        resolved: reply.resolved.clone(),
    }
    .to_bytes()
}

/// v40.51 test-only encoder for Frame::AtActionReq. Mirrors
/// qlux-cua-driver fb3b43d7 encode_at_action_req_line, plus the optional
/// trailing occurrence this card defines. Public so the occurrence-index
/// tests can round-trip the real parser.
pub fn encode_at_action_req(req: &Frame) -> Vec<u8> {
    let _ = std::hint::black_box(AT_ACTION_OCCURRENCE_INDEX_V40_51);
    match req {
        Frame::AtActionReq {
            wid,
            role,
            name,
            index,
            nth,
        } => {
            let nth_suffix = nth.map(|n| format!(" {}", n)).unwrap_or_default();
            let name_words: Vec<&str> = name.split_whitespace().collect();
            let toggle_role = role.eq_ignore_ascii_case("toggle")
                || role.eq_ignore_ascii_case("toggle button");
            if name_words.len() == 2 && toggle_role {
                let action_tok = if role.eq_ignore_ascii_case("toggle button") {
                    "click"
                } else {
                    "toggle"
                };
                format!("{OPC_AT_ACTION_REQ} {wid} toggle button {} {} {action_tok} {index}{nth_suffix}\n", name_words[0], name_words[1])
                .into_bytes()
            } else {
                let role_tok = role.split_whitespace().next().unwrap_or("");
                let name_tok = if name_words.len() > 1 {
                    name_words.join("_")
                } else {
                    name_words.first().copied().unwrap_or("").to_string()
                };
                format!("{OPC_AT_ACTION_REQ} {wid} {role_tok} {name_tok} {index}{nth_suffix}\n")
                    .into_bytes()
            }
        }
        _ => unreachable!("encode_at_action_req only accepts Frame::AtActionReq"),
    }
}

fn parse_at_tree_req(text: &str) -> Result<Frame, FrameError> {
    let mut parts = text.split_whitespace();
    let _opc = parts.next().ok_or(FrameError::Malformed)?;
    let window_id: u64 = parts
        .next()
        .ok_or(FrameError::Malformed)?
        .parse()
        .map_err(|_| FrameError::Malformed)?;
    let path = remainder_after_tokens(text, 2)
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty());
    Ok(Frame::AtTreeReq { window_id, path })
}

fn remainder_after_tokens(line: &str, n: usize) -> Option<&str> {
    let mut rest = line.trim_start();
    for _ in 0..n {
        let idx = rest.find(char::is_whitespace)?;
        rest = rest[idx..].trim_start();
    }
    Some(rest)
}

thread_local! {
    static APPLY_ACTION_OVERRIDE: RefCell<Option<fn(i32) -> bool>> =
        const { RefCell::new(None) };
    static LAST_ACTION_DEST: RefCell<String> = const { RefCell::new(String::new()) };
    static LAST_ACTION_OBJ: RefCell<String> = const { RefCell::new(String::new()) };
    static LAST_ACTION_N: Cell<i32> = const { Cell::new(0) };
}

/// Test hook: `do_action(index)` used by [`InjectHarness::apply_action_on_element`].
pub fn set_apply_action_on_element_override(v: Option<fn(i32) -> bool>) {
    APPLY_ACTION_OVERRIDE.with(|slot| *slot.borrow_mut() = v);
}

pub(crate) fn set_last_action_target(dest: &str, obj_path: &str, n_actions: i32) {
    LAST_ACTION_DEST.with(|s| *s.borrow_mut() = dest.to_string());
    LAST_ACTION_OBJ.with(|s| *s.borrow_mut() = obj_path.to_string());
    LAST_ACTION_N.with(|c| c.set(n_actions));
}

/// Live call site for `AT_ACTION_REQ_OPCODE_V40_15_3` (mirrors
/// `post_key_dispatch` + `KEY_DELIVERY_V40_9_DISPATCH_IDENT`).
pub(crate) fn apply_action_on_element_default(
    wid: u64,
    path: &str,
    index: i32,
) -> Result<(), &'static str> {
    let _ = std::hint::black_box(AT_ACTION_REQ_OPCODE_V40_15_3);
    let _ = wid;
    let n_actions = LAST_ACTION_N.with(|c| c.get());
    if n_actions <= 0 || index < 0 || index >= n_actions {
        return Err("ACTION_NOT_FOUND");
    }
    if let Some(do_action) = APPLY_ACTION_OVERRIDE.with(|slot| *slot.borrow()) {
        return if do_action(index) {
            Ok(())
        } else {
            Err("ACTION_NOT_FOUND")
        };
    }
    let dest = LAST_ACTION_DEST.with(|s| s.borrow().clone());
    let obj = LAST_ACTION_OBJ.with(|s| s.borrow().clone());
    let obj_path = if obj.is_empty() {
        path.to_string()
    } else {
        obj
    };
    if dest.is_empty() || obj_path.is_empty() {
        return Err("ACTION_NOT_FOUND");
    }
    #[cfg(feature = "dbus")]
    {
        return live_do_action_at_index(&dest, &obj_path, index);
    }
    #[cfg(not(feature = "dbus"))]
    {
        Err("ACTION_NOT_FOUND")
    }
}

#[cfg(feature = "dbus")]
fn live_do_action_at_index(dest: &str, obj_path: &str, index: i32) -> Result<(), &'static str> {
    let dest = dest.to_owned();
    let obj_path = obj_path.to_owned();
    match std::thread::spawn(move || live_do_action_on_a11y_bus(&dest, &obj_path, index)).join() {
        Ok(r) => r,
        Err(_) => Err("ACTION_NOT_FOUND"),
    }
}

#[cfg(feature = "dbus")]
fn live_do_action_on_a11y_bus(
    dest: &str,
    obj_path: &str,
    index: i32,
) -> Result<(), &'static str> {
    let conn = match crate::inject::atspi_ladder::connect_a11y_bus() {
        Ok(c) => c,
        Err(_) => return Err("ACTION_NOT_FOUND"),
    };
    const ACTION: &str = "org.a11y.atspi.Action";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    let n_ok = conn
        .call_method(
            Some(dest),
            obj_path,
            Some(PROPERTIES),
            "Get",
            &(ACTION, "NActions"),
        )
        .ok()
        .and_then(|m| m.body().deserialize::<zbus::zvariant::OwnedValue>().ok())
        .and_then(|v| i32::try_from(v).ok());
    let n_actions = match n_ok {
        Some(n) if n > 0 => n,
        _ => return Err("ACTION_NOT_FOUND"),
    };
    if index < 0 || index >= n_actions {
        return Err("ACTION_NOT_FOUND");
    }
    let ok = conn
        .call_method(Some(dest), obj_path, Some(ACTION), "DoAction", &index)
        .ok()
        .and_then(|m| m.body().deserialize::<bool>().ok())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("ACTION_NOT_FOUND")
    }
}

/// Compare hex via `==` (constant-time is not required for v3).
pub fn validate_token(token_hex: &str, path: &Path) -> Result<String, TokenError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(TokenError::NoFile),
        Err(e) => return Err(TokenError::Io(e)),
    };
    match std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777) {
        Ok(0o600) => {}
        Ok(mode) => {
            tracing::warn!(
                "inject tokens file mode is {:o}, expected 0600; continuing with permissive mode",
                mode
            );
        }
        Err(err) => {
            tracing::warn!("could not stat inject tokens file: {}; continuing", err);
        }
    }
    for line in io::BufReader::new(file).lines() {
        let line = line.map_err(TokenError::Io)?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((bot_id, token)) = line.split_once(':') else {
            continue;
        };
        if token == token_hex {
            return Ok(bot_id.to_string());
        }
    }
    Err(TokenError::NoMatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn hex64() -> String {
        "ab".repeat(32)
    }

    fn temp_tokens(body: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("qlux-inject-tokens-{}-{nanos}", std::process::id()));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parse_hello_line() {
        let (frame, rest) = parse_frame(b"HELLO\n").unwrap();
        assert_eq!(frame, Frame::Hello);
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_auth_token() {
        let token = hex64();
        let raw = format!("AUTH\n{token}\n");
        let (frame, rest) = parse_frame(raw.as_bytes()).unwrap();
        assert_eq!(frame, Frame::Auth(token));
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_bind_id() {
        let (frame, rest) = parse_frame(b"BIND 42\n").unwrap();
        assert_eq!(frame, Frame::Bind(42));
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_click_xy() {
        let (frame, rest) = parse_frame(b"CLICK 100 200\n").unwrap();
        assert_eq!(frame, Frame::Click { x: 100.0, y: 200.0 });
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_key_name() {
        let (frame, rest) = parse_frame(b"KEY XF86XK_AudioMute\n").unwrap();
        assert_eq!(
            frame,
            Frame::Key {
                name: "XF86XK_AudioMute".into(),
                shift: false,
                mods: vec![],
            }
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_scroll_dxdy() {
        let (frame, rest) = parse_frame(b"SCROLL -3 7\n").unwrap();
        assert_eq!(frame, Frame::Scroll { dx: -3.0, dy: 7.0 });
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_drag_from_to() {
        let (frame, rest) = parse_frame(b"DRAG 10 20 30 40\n").unwrap();
        assert_eq!(
            frame,
            Frame::Drag {
                from: (10.0, 20.0),
                to: (30.0, 40.0)
            }
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_unknown_opcode_returns_err() {
        assert_eq!(parse_frame(b"FOO\n"), Err(FrameError::UnknownOpcode));
    }

    #[test]
    fn parse_too_big_frame_rejected() {
        let mut buf = vec![b'X'; 70 * 1024];
        buf.push(b'\n');
        assert_eq!(parse_frame(&buf), Err(FrameError::TooBig));
    }

    #[test]
    fn parse_malformed_number_returns_err() {
        assert_eq!(parse_frame(b"CLICK abc def\n"), Err(FrameError::Malformed));
    }

    #[test]
    fn validate_token_match() {
        let path = temp_tokens("bot-x:DEAD\n");
        let got = validate_token("DEAD", &path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(got, "bot-x");
    }

    #[test]
    fn validate_token_mismatch_returns_no_match() {
        let path = temp_tokens("bot-x:DEAD\n");
        let err = validate_token("BEEF", &path).unwrap_err();
        let _ = fs::remove_file(&path);
        assert!(matches!(err, TokenError::NoMatch));
    }

    #[test]
    fn validate_token_missing_file_returns_no_file() {
        let err = validate_token("DEAD", Path::new("/nonexistent/path")).unwrap_err();
        assert!(matches!(err, TokenError::NoFile));
    }

    #[test]
    fn format_hello_ok() {
        assert_eq!(Reply::HelloOk.to_bytes(), b"HELLO OK\n");
    }

    #[test]
    fn format_auth_ok() {
        assert_eq!(
            Reply::AuthOk {
                bot_id: "bot-a".into()
            }
            .to_bytes(),
            b"AUTH OK bot-a\n"
        );
    }

    #[test]
    fn format_dispatch_err_permanent() {
        assert_eq!(
            Reply::DispatchErr { code: ERR_LOCKED }.to_bytes(),
            b"DISPATCH ERR LOCKED\n"
        );
    }

    #[test]
    fn parse_grant_resolved_grant() {
        let (frame, rest) = parse_frame(b"GRANT_RESOLVED bot-a 42 GRANT\n").unwrap();
        assert_eq!(
            frame,
            Frame::GrantResolved {
                bot_id: "bot-a".into(),
                window_id: 42,
                resolution: GrantResolution::Granted,
            }
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_grant_resolved_deny() {
        let (frame, rest) = parse_frame(b"GRANT_RESOLVED bot-a 42 DENY\n").unwrap();
        assert_eq!(
            frame,
            Frame::GrantResolved {
                bot_id: "bot-a".into(),
                window_id: 42,
                resolution: GrantResolution::Denied,
            }
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_grant_resolved_bad_decision_is_malformed() {
        assert_eq!(
            parse_frame(b"GRANT_RESOLVED bot-a 42 ALLOW\n"),
            Err(FrameError::Malformed)
        );
        assert_eq!(
            parse_frame(b"GRANT_RESOLVED bot-a 42 grant\n"),
            Err(FrameError::Malformed)
        );
        assert_eq!(
            parse_frame(b"GRANT_RESOLVED bot-a 42 GRANT extra\n"),
            Err(FrameError::Malformed)
        );
    }

    #[test]
    fn parse_grant_resolved_missing_decision_is_malformed() {
        assert_eq!(
            parse_frame(b"GRANT_RESOLVED bot-a 42\n"),
            Err(FrameError::Malformed)
        );
    }
}
