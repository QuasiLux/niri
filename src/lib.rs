#[macro_use]
extern crate tracing;

// Trap 45: build-time gate. The literal below MUST be present in the
// resulting `niri` ELF (verified via `strings ... | grep -c IDENT`).
// Source-only marker; has no runtime effect.
#[used]
static V40_44_M1_REBASE_SIDE_STAGE_V40_44: &[u8] =
    b"V40_44_M1_REBASE_SIDE_STAGE_V40_44\0";

#[cfg(feature = "dbus")]
pub mod a11y;
pub mod animation;
pub mod backend;
pub mod cli;
pub mod cursor;
#[cfg(feature = "dbus")]
pub mod dbus;
pub mod frame_clock;
pub mod handlers;
pub mod input;
pub mod inject;
pub mod inject_e2e;
pub mod ipc;
pub mod layer;
pub mod layout;
pub mod niri;
pub mod protocols;
pub mod render_helpers;
pub mod rubber_band;
#[cfg(feature = "xdp-gnome-screencast")]
pub mod screencasting;
pub mod ui;
pub mod utils;
pub mod window;

#[cfg(test)]
mod tests;
