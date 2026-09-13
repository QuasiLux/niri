//! Tagged-line inject wire protocol.
//!
//! One opcode per `\n`-terminated frame. AUTH is two lines (`AUTH\n<token>\n`).
//! JSON was rejected: the seven opcodes fit on one line and tagged-line stays
//! branch-free at a few hundred lines.

use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

use smithay::input::SeatHandler;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::input::agent_seat::AgentSeat;

pub const OPC_HELLO: &str = "HELLO";
pub const OPC_AUTH: &str = "AUTH";
pub const OPC_BIND: &str = "BIND";
pub const OPC_CLICK: &str = "CLICK";
pub const OPC_KEY: &str = "KEY";
pub const OPC_SCROLL: &str = "SCROLL";
pub const OPC_DRAG: &str = "DRAG";

pub const REP_HELLO_OK: &str = "HELLO OK";
pub const REP_AUTH_OK: &str = "AUTH OK";
pub const REP_BIND_OK: &str = "BIND OK";
pub const REP_DISPATCH_OK: &str = "DISPATCH OK";
pub const REP_DISPATCH_ERR: &str = "DISPATCH ERR";
pub const REP_FRAME_ERR: &str = "FRAME ERR";
pub const REP_AUTH_ERR: &str = "AUTH ERR";

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

pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Literal placeholder path. Production resolution is owned by the spawn card.
pub const TOKEN_FILE_PATH: &str = "/run/user/<uid>/qlux/inject-tokens";

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello,
    Auth(String),
    Bind(u64),
    Click { x: f64, y: f64 },
    Key(String),
    Scroll { dx: f64, dy: f64 },
    Drag { from: (f64, f64), to: (f64, f64) },
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
    DispatchOk,
    DispatchErr { code: &'static str },
    FrameErr { code: &'static str },
    AuthErr { code: &'static str },
}

impl Reply {
    pub fn to_bytes(&self) -> Vec<u8> {
        let s = match self {
            Reply::HelloOk => format!("{REP_HELLO_OK}\n"),
            Reply::AuthOk { bot_id } => format!("{REP_AUTH_OK} {bot_id}\n"),
            Reply::BindOk => format!("{REP_BIND_OK}\n"),
            Reply::DispatchOk => format!("{REP_DISPATCH_OK}\n"),
            Reply::DispatchErr { code } => format!("{REP_DISPATCH_ERR} {code}\n"),
            Reply::FrameErr { code } => format!("{REP_FRAME_ERR} {code}\n"),
            Reply::AuthErr { code } => format!("{REP_AUTH_ERR} {code}\n"),
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
    fn is_locked(&self) -> bool {
        false
    }
    fn outputs_off(&self) -> bool {
        false
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
    let nl = input.iter().position(|&b| b == b'\n').ok_or(FrameError::Incomplete)?;
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
            let nl2 = after.iter().position(|&b| b == b'\n').ok_or(FrameError::Incomplete)?;
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
        OPC_DRAG => parse_four_f64(parts).map(|from_to| {
            let (from, to) = from_to;
            (Frame::Drag { from, to }, after)
        }),
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

/// Compare hex via `==` (constant-time is not required for v3).
pub fn validate_token(token_hex: &str, path: &Path) -> Result<String, TokenError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(TokenError::NoFile),
        Err(e) => return Err(TokenError::Io(e)),
    };
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
        let path = std::env::temp_dir().join(format!(
            "qlux-inject-tokens-{}-{nanos}",
            std::process::id()
        ));
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
}
