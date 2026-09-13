//! QuasiLux inject plane.
//!
//! Wire format (HELLO / AUTH / BIND / CLICK / KEY / SCROLL / DRAG) and token
//! validation live here. Compositor-startup wiring (`Niri::new`) is a sibling card.

pub mod protocol;
pub mod socket;

pub use crate::input::agent_seat::{AgentSeat, AGENT_SEAT_NAME};
pub use protocol::{
    validate_token, Frame, FrameError, InjectHarness, Reply, MAX_FRAME_LEN, TOKEN_FILE_PATH,
};
pub use socket::{handle_inject_client, InjectServer, INJECT_SOCKET_SUFFIX};

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
    "run/user/<uid>/qlux/inject-tokens",
];
