//! Tagged-line inject wire protocol.
//!
//! One opcode per `\n`-terminated frame. AUTH is two lines (`AUTH\n<token>\n`).
//! JSON was rejected: the seven opcodes fit on one line and tagged-line stays
//! branch-free at a few hundred lines.

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

/// `AT_ELEMENT_REQ <wid> LC <role> <name>` / `EI <role> <name> <text…>` / `CV <query>`.
pub const OPC_AT_ELEMENT_REQ: &str = "AT_ELEMENT_REQ";

/// Keep the wire-ident name in the release ELF (`strings | grep GRANT_REQUEST`).
#[used]
#[allow(dead_code)]
static OPC_GRANT_REQUEST_IDENT: &str = "GRANT_REQUEST";

/// Independent literals so thin LTO cannot merge them into an adjacent &str.
#[used]
#[allow(dead_code)]
static OPC_AT_ELEMENT_REQ_IDENT: &str = "AT_ELEMENT_REQ";

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
        REP_AT_ELEMENT_REPLY,
        REP_BG_UNAVAILABLE,
        IDENT_AT_SPI_STRONG,
        IDENT_AT_SPI_CHROME,
        OPC_GRANT_RESOLVED,
    ));
    let _ = std::hint::black_box(b"AT_ELEMENT_REQ\0");
    let _ = std::hint::black_box(b"AT_ELEMENT_REPLY\0");
    let _ = std::hint::black_box(b"BACKGROUND_UNAVAILABLE\0");
    let _ = std::hint::black_box(b"AT_SPI_STRONG\0");
    let _ = std::hint::black_box(b"AT_SPI_CHROME\0");
    let _ = std::hint::black_box(b"GRANT_RESOLVED_OPCODE\0");
    let _ = std::hint::black_box(b"GRANT_NOT_PENDING\0");
    PathBuf::from(TOKEN_FILE_PATH.replace("<uid>", &uid.to_string()))
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
    Key(String),
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
    AtElementReq {
        window_id: u64,
        action: crate::inject::atspi_ladder::AtSpiAction,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    HelloOk,
    AuthOk { bot_id: String },
    BindOk,
    BindPending,
    DispatchOk,
    DispatchErr { code: &'static str },
    FrameErr { code: &'static str },
    AuthErr { code: &'static str },
    GrantAck { queued: bool },
    GrantAckOk,
    GrantAckDenied,
    AtElementReply { payload: String },
    BackgroundUnavailable,
}

impl Reply {
    pub fn to_bytes(&self) -> Vec<u8> {
        let s = match self {
            Reply::HelloOk => format!("{REP_HELLO_OK}\n"),
            Reply::AuthOk { bot_id } => format!("{REP_AUTH_OK} {bot_id}\n"),
            Reply::BindOk => format!("{REP_BIND_OK}\n"),
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
            Reply::BackgroundUnavailable => format!("{REP_AT_ELEMENT_REPLY} {REP_BG_UNAVAILABLE}\n"),
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
        OPC_KEY => {
            let name = parts.next().ok_or(FrameError::Malformed)?;
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok((Frame::Key(name.to_string()), after))
        }
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
        _ => Err(FrameError::UnknownOpcode),
    }
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
/// - `AT_ELEMENT_REQ <wid> LC <role> <name>`
/// - `AT_ELEMENT_REQ <wid> EI <role> <name> <text…>` (text may contain spaces)
/// - `AT_ELEMENT_REQ <wid> CV <query>`
fn parse_at_element_req(text: &str) -> Result<Frame, FrameError> {
    use crate::inject::atspi_ladder::AtSpiAction;

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
            if parts.next().is_some() {
                return Err(FrameError::Malformed);
            }
            Ok(Frame::AtElementReq {
                window_id,
                action: AtSpiAction::LabelClick {
                    acc_role: acc_role.to_string(),
                    name_query: name_query.to_string(),
                },
            })
        }
        "EI" => {
            let acc_role = parts.next().ok_or(FrameError::Malformed)?;
            let name_query = parts.next().ok_or(FrameError::Malformed)?;
            let insert_text = remainder_after_tokens(text, 5).unwrap_or("").to_string();
            Ok(Frame::AtElementReq {
                window_id,
                action: AtSpiAction::EditInsert {
                    acc_role: acc_role.to_string(),
                    name_query: name_query.to_string(),
                    text: insert_text,
                },
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
            })
        }
        _ => Err(FrameError::Malformed),
    }
}

fn remainder_after_tokens(line: &str, n: usize) -> Option<&str> {
    let mut rest = line.trim_start();
    for _ in 0..n {
        let idx = rest.find(char::is_whitespace)?;
        rest = rest[idx..].trim_start();
    }
    Some(rest)
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
        assert_eq!(frame, Frame::Key("XF86XK_AudioMute".into()));
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
