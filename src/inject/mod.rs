//! QuasiLux inject plane (scaffold).
//!
//! Wire format (HELLO / AUTH / BIND / CLICK / KEY / SCROLL / DRAG), token
//! rotation, and compositor-startup wiring are deferred to v3.

pub mod socket;

pub use crate::input::agent_seat::{AgentSeat, AGENT_SEAT_NAME};
pub use socket::{InjectServer, INJECT_SOCKET_SUFFIX};

/// Idents kept in the release ELF so the inject-plane scaffold can be grepped.
#[used]
pub static INJECT_PLANE_IDENTS: &[&str] = &[
    AGENT_SEAT_NAME,
    INJECT_SOCKET_SUFFIX,
    "PointerHandle",
    "KeyboardHandle",
    "InjectPlaceholder",
];
