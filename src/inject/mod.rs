//! QuasiLux inject plane.
//!
//! Wire format (HELLO / AUTH / BIND / CLICK / KEY / SCROLL / DRAG) and token
//! validation live here. Compositor-startup wiring is gated on
//! [`QLUX_INJECT_ENABLE`] inside `Niri::new`.

pub mod protocol;
pub mod socket;

pub use crate::input::agent_seat::{AgentSeat, AGENT_SEAT_NAME};
pub use protocol::{
    token_file_path, validate_token, Frame, FrameError, InjectHarness, Reply, MAX_FRAME_LEN,
    TOKEN_FILE_PATH,
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
    TOKEN_FILE_PATH,
    "run/user/<uid>/qlux/inject-tokens",
    "/run/user/",
    QLUX_INJECT_ENABLE,
    QLUX_INJECT_SOCKET_SUFFIX,
    QLUX_INJECT_GROUP,
    "InjectServer::start",
    "TokenRegistry",
];
