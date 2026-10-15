//! v40.25 WINDOW_FOCUS_REQ real impl — wire parse, ident survival,
//! and worker→main handoff proof.
//!
//! Pre-v40.25 the cua-driver raw-socket test of `WINDOW_FOCUS_REQ 2`
//! returned `WINDOW_FOCUS_REPLY 0 no such window` because
//! `DispatchHarness::focus_window` inherited the trait default stub.
//! v40.25 wires `focus_window_tx` so the worker posts a `FocusDispatchReq`
//! onto the main compositor loop, which calls
//! `crate::input::focus_window::focus_window_niri` on the live `State`.

use niri::inject::protocol::{
    parse_frame, InjectHarness, Frame, Reply, WINDOW_FOCUS_DISPATCHED_V40_25, WINDOW_FOCUS_REQ_V40_24,
};

#[test]
fn focus_window_v40_25_ident_is_strings_gated() {
    // LTO-strip guard: the v40.25 ident must remain greppable in the
    // release ELF. Same shape as the v40.18 / v40.21 idents.
    let _ = std::hint::black_box(WINDOW_FOCUS_DISPATCHED_V40_25);
    let _ = std::hint::black_box(WINDOW_FOCUS_REQ_V40_24);
    let v25 = WINDOW_FOCUS_DISPATCHED_V40_25;
    assert!(v25.windows(30).any(|w| w == b"WINDOW_FOCUS_DISPATCHED_V40_25"));
    let v24 = WINDOW_FOCUS_REQ_V40_24;
    assert!(v24.windows(23).any(|w| w == b"WINDOW_FOCUS_REQ_V40_24"));
}

#[test]
fn window_focus_req_parses() {
    // Parse arm wires `WINDOW_FOCUS_REQ <wid>` to `Frame::WindowFocusReq { wid }`.
    let _ = std::hint::black_box(WINDOW_FOCUS_REQ_V40_24);
    let (frame, rest) = parse_frame(b"WINDOW_FOCUS_REQ 2\n").unwrap();
    assert!(rest.is_empty());
    assert_eq!(frame, Frame::WindowFocusReq { wid: 2 });
}

#[test]
fn window_focus_reply_ok_serializes_1() {
    let bytes = Reply::WindowFocusReply {
        ok: true,
        reason: String::new(),
    }
    .to_bytes();
    assert_eq!(bytes, b"WINDOW_FOCUS_REPLY 1\n");
}

#[test]
fn window_focus_reply_fail_serializes_0_reason() {
    let bytes = Reply::WindowFocusReply {
        ok: false,
        reason: "no such window".into(),
    }
    .to_bytes();
    assert_eq!(bytes, b"WINDOW_FOCUS_REPLY 0 no such window\n");
}

#[test]
fn dispatch_harness_focus_window_invalid_wid_is_client_side_guard() {
    // `wid == 0` returns "invalid wid" without crossing the
    // worker→main boundary. Same prelude as
    // `crate::input::focus_window::focus_window_niri`. This is the
    // client-side guard so the worker thread never asks the compositor
    // loop to look up a sentinel "no selection" wid.
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use niri::inject::dispatch::{DispatchHarness, DispatchState};
    use niri::inject::smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    use niri::inject::PendingGrant;
    use niri::input::focus_window::FocusWindowReply;

    let mut state = DispatchState::new(
        false,
        false,
        PathBuf::from("/dev/null"),
        Arc::new(Mutex::new(HashMap::<u64, WlSurface>::new())),
        Arc::new(Mutex::new(HashMap::<(String, u64), ()>::new())),
        Arc::new(Mutex::new(None::<PendingGrant>)),
    );
    // No `focus_window_tx` installed: the post path would be a no-op
    // miss anyway, but the `wid == 0` short-circuit must fire first.
    let mut harness = DispatchHarness::new(&mut state);
    let reply = InjectHarness::focus_window(&mut harness, 0);
    assert!(!reply.ok);
    assert_eq!(reply.reason, "invalid wid");
    // Compile-check the failure variant shape (vs `reply: FocusWindowReply`).
    let _: FocusWindowReply = reply;
}

#[test]
fn dispatch_harness_focus_window_hands_off_when_no_such_window() {
    // Without a `focus_window_tx` installed (e.g. in-process test), the
    // worker path falls back to the trait-default "no such window" so
    // the harness stays compile-clean. The presence of an installed
    // `focus_window_tx` is what triggers the real handoff (covered by
    // the next test).
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use niri::inject::dispatch::{DispatchHarness, DispatchState};
    use niri::inject::smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    use niri::inject::PendingGrant;

    let mut state = DispatchState::new(
        false,
        false,
        PathBuf::from("/dev/null"),
        Arc::new(Mutex::new(HashMap::<u64, WlSurface>::new())),
        Arc::new(Mutex::new(HashMap::<(String, u64), ()>::new())),
        Arc::new(Mutex::new(None::<PendingGrant>)),
    );
    let mut harness = DispatchHarness::new(&mut state);
    let reply = InjectHarness::focus_window(&mut harness, 999_999);
    assert!(!reply.ok);
    assert_eq!(reply.reason, "no such window");
}

#[test]
fn dispatch_harness_focus_window_hands_off_instead_of_stub_when_tx_installed() {
    // The pre-v40.25 bug: `WINDOW_FOCUS_REQ <wid>` on a worker harness
    // hit the trait default at `protocol.rs::focus_window` and returned
    // "no such window" for every wid, including valid ones. v40.25
    // installs `focus_window_tx` so the worker posts a
    // `FocusDispatchReq` onto the main loop. The pump here stands in
    // for the compositor loop and answers "ok" (mimics the real
    // `focus_window_niri` path on `&mut State`).
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use niri::inject::calloop;
    use niri::inject::dispatch::{DispatchHarness, DispatchState, FocusDispatchReq};
    use niri::inject::smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    use niri::inject::PendingGrant;
    use niri::input::focus_window::FocusWindowReply;

    let (tx, rx) = calloop::channel::channel::<FocusDispatchReq>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let _pump = std::thread::spawn(move || {
        let mut event_loop = calloop::EventLoop::<()>::try_new().expect("loop");
        event_loop
            .handle()
            .insert_source(rx, |event, _, _| {
                if let calloop::channel::Event::Msg(req) = event {
                    // Stand-in for `State::dispatch_focus_request`. A
                    // valid-looking wid answers `ok`; an invalid one
                    // answers `no such window`. The fixture wid=2 maps
                    // to "pavucontrol" in the production e2e test;
                    // here it is just a non-zero wid that the pump
                    // chooses to honour.
                    let reply = if req.wid == 2 || req.wid == 3 {
                        FocusWindowReply::ok()
                    } else if req.wid == 999_999 {
                        FocusWindowReply::fail("no such window")
                    } else {
                        FocusWindowReply::ok()
                    };
                    let _ = req.reply.send(reply);
                }
            })
            .expect("insert focus-window source");
        let _ = ready_tx.send(());
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut ())
                .expect("dispatch");
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("pump ready");

    let mut state = DispatchState::new(
        false,
        false,
        PathBuf::from("/dev/null"),
        Arc::new(Mutex::new(HashMap::<u64, WlSurface>::new())),
        Arc::new(Mutex::new(HashMap::<(String, u64), ()>::new())),
        Arc::new(Mutex::new(None::<PendingGrant>)),
    );
    state.focus_window_tx = Some(tx);
    let mut harness = DispatchHarness::new(&mut state);

    // Pre-v40.25 this returned `FocusWindowReply::fail("no such window")`
    // because the trait default stub fired regardless of wid. v40.25
    // posts the FocusDispatchReq; the pump answers ok for wid=2.
    let reply_pavucontrol = InjectHarness::focus_window(&mut harness, 2);
    assert!(
        reply_pavucontrol.ok,
        "worker path must not return the trait-default miss; got {:?}",
        reply_pavucontrol
    );

    let reply_galculator = InjectHarness::focus_window(&mut harness, 3);
    assert!(
        reply_galculator.ok,
        "worker path must not return the trait-default miss; got {:?}",
        reply_galculator
    );

    let reply_missing = InjectHarness::focus_window(&mut harness, 999_999);
    assert!(!reply_missing.ok);
    assert_eq!(reply_missing.reason, "no such window");
}