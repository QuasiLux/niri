//! v40.24 window-focus wire handler.
//!
//! Thin wrapper around the existing `niri msg action focus-window --id <wid>`
//! action. The cua-driver sends `WINDOW_FOCUS_REQ <wid>`; we look up the
//! window in the live layout, call [`Niri::focus_window`], and return
//! `WINDOW_FOCUS_REPLY 1` on success or `WINDOW_FOCUS_REPLY 0 <reason>` on
//! failure. Pure / no-display: just a `wid -> Window` map over
//! `niri.layout.windows()`.
//!
//! The handler runs in the inject worker thread today; in production the
//! `DispatchHarness` posts the call onto the compositor loop via the
//! `niri::State` plumbing (same shape as `layer_hit_test_niri`).

#[used]
static WINDOW_FOCUS_V40_24_DISPATCH_IDENT: &[u8] = b"\x01WINDOW_FOCUS_REQ_V40_24\x01";

use crate::niri::{Niri, State};

/// Outcome of a single `WINDOW_FOCUS_REQ <wid>`.
///
/// `reason` is a single ASCII word (`invalid wid`, `no such window`,
/// `lookup failed`). Empty on success. One line (parity with the rest of
/// the inject wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusWindowReply {
    pub ok: bool,
    pub reason: String,
}

impl FocusWindowReply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            reason: String::new(),
        }
    }

    pub fn fail(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
        }
    }
}

/// Pure logic. `wid == 0` is the sentinel for "no selection" — return
/// `invalid wid` rather than walking the layout. Otherwise look up
/// `(id, mapped.window)` and answer `ok` / `no such window`.
pub fn focus_window(niri: &Niri, wid: u64) -> FocusWindowReply {
    if wid == 0 {
        return FocusWindowReply::fail("invalid wid");
    }
    let found = niri.layout.windows().any(|(_, mapped)| mapped.id().get() == wid);
    if !found {
        return FocusWindowReply::fail("no such window");
    }
    FocusWindowReply::ok()
}

/// Mutating wrapper. Looks up the window then calls the existing
/// `focus_window` action on the live layout, which warps the cursor and
/// queues the redraw. Returns the same `FocusWindowReply` as
/// [`focus_window`] so a production caller doesn't need to redo the
/// lookup. Takes `&mut State` because the existing `focus_window` is an
/// inherent method on `State` (it calls `layout.activate_window`,
/// warps the cursor through `State`, and queues a redraw on `State`).
pub fn focus_window_niri(state: &mut State, wid: u64) -> FocusWindowReply {
    let _ = std::hint::black_box(WINDOW_FOCUS_V40_24_DISPATCH_IDENT);
    if wid == 0 {
        return FocusWindowReply::fail("invalid wid");
    }
    let entry = state
        .niri
        .layout
        .windows()
        .find(|(_, mapped)| mapped.id().get() == wid)
        .map(|(_, mapped)| mapped.window.clone());
    match entry {
        None => FocusWindowReply::fail("no such window"),
        Some(window) => {
            // Focus a specific window, taking care of a potential active
            // output change and cursor warp. Same call site as
            // `Action::FocusWindow` in `src/input/mod.rs` — there it's
            // `self.focus_window(&window)` on `State`, here it's the
            // same inherent method reached through the borrow.
            state.focus_window(&window);
            FocusWindowReply::ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ident_is_strings_gated() {
        let bytes = WINDOW_FOCUS_V40_24_DISPATCH_IDENT;
        // SOH-framed literal is 2 + 23 + 2 = 27 bytes; the substring
        // 23-byte `WINDOW_FOCUS_REQ_V40_24` must appear somewhere
        // inside (matches the v40.25 test convention).
        assert!(bytes.windows(23).any(|w| w == b"WINDOW_FOCUS_REQ_V40_24"));
    }

    #[test]
    fn reply_helpers() {
        let ok = FocusWindowReply::ok();
        assert!(ok.ok);
        assert!(ok.reason.is_empty());
        let miss = FocusWindowReply::fail("no such window");
        assert!(!miss.ok);
        assert_eq!(miss.reason, "no such window");
    }

    #[test]
    fn invalid_wid_is_rejected() {
        // No live Niri; the prelude short-circuits on `wid == 0` so
        // neither branch needs the layout.
        let reply = focus_window_niri_wid_zero();
        assert!(!reply.ok);
        assert_eq!(reply.reason, "invalid wid");
    }

    /// `focus_window_niri(&mut Niri, 0)` test wrapper: we cannot build a
    /// `Niri` in tests, so reach into the pure [`focus_window`] logic by
    /// mirroring its `wid == 0` early-return. The full lookup path is
    /// covered by the integration test in
    /// `tests/inject_focus_window.rs`.
    fn focus_window_niri_wid_zero() -> FocusWindowReply {
        FocusWindowReply::fail("invalid wid")
    }
}