//! v40.18 layer-shell hit-test. Pure fixture tests — no display, no socket.

use niri::inject::protocol::{parse_frame, Frame, Reply, LAYER_HIT_TEST_V40_18};
use niri::input::layer_hit_test::{hit_test, identity_from_namespace, LayerHitSurface};

fn bar() -> LayerHitSurface {
    LayerHitSurface {
        app_id: "quickshell".into(),
        role: "panel".into(),
        name: "quickshell:panel".into(),
        bounds: (0, 0, 1920, 32),
        z: 2,
        occluded: false,
    }
}

fn launcher() -> LayerHitSurface {
    LayerHitSurface {
        app_id: "quickshell".into(),
        role: "launcher".into(),
        name: "quickshell:launcher".into(),
        bounds: (8, 4, 24, 24),
        z: 3,
        occluded: false,
    }
}

#[test]
fn hit_noctalia_bar_center() {
    assert_eq!(
        identity_from_namespace("quickshell:panel"),
        (
            "quickshell".to_string(),
            "panel".to_string(),
            "quickshell:panel".to_string()
        )
    );
    let hit = hit_test(&[bar(), launcher()], 960, 16);
    assert!(hit.found);
    assert_eq!(hit.app_id, "quickshell");
    assert_eq!(hit.role, "panel");
    assert_eq!(hit.name, "quickshell:panel");
    assert_eq!(hit.bounds, (0, 0, 1920, 32));
}

#[test]
fn hit_launcher_button_same_app_id() {
    let hit = hit_test(&[bar(), launcher()], 16, 12);
    assert!(hit.found);
    assert_eq!(hit.app_id, "quickshell");
    assert_eq!(hit.role, "launcher");
    assert_eq!(hit.name, "quickshell:launcher");
    assert_eq!(hit.bounds, (8, 4, 24, 24));
}

#[test]
fn hit_window_area_is_not_a_layer() {
    let hit = hit_test(&[bar(), launcher()], 400, 400);
    assert!(!hit.found);
    assert!(hit.app_id.is_empty());
}

#[test]
fn hit_outside_any_layer() {
    let hit = hit_test(&[bar(), launcher()], 5000, 5000);
    assert!(!hit.found);
}

#[test]
fn hit_occluded_layer_is_hidden() {
    let mut hidden = bar();
    hidden.occluded = true;
    let hit = hit_test(&[hidden], 960, 16);
    assert!(!hit.found);
}

#[test]
fn hit_panel_above_output_edges() {
    let panel = LayerHitSurface {
        app_id: "quickshell".into(),
        role: "panel".into(),
        name: "quickshell:panel".into(),
        bounds: (0, -40, 1920, 40),
        z: 2,
        occluded: false,
    };
    let hit = hit_test(&[panel], 100, -10);
    assert!(hit.found);
    assert_eq!(hit.bounds, (0, -40, 1920, 40));
    assert_eq!(hit.app_id, "quickshell");
    assert_eq!(hit.role, "panel");
}

#[test]
fn layer_hit_test_req_parses_negative_coords() {
    let _ = std::hint::black_box(LAYER_HIT_TEST_V40_18);
    let (frame, rest) = parse_frame(b"LAYER_HIT_TEST 100 -10\n").unwrap();
    assert!(rest.is_empty());
    assert_eq!(frame, Frame::LayerHitTestReq { x: 100, y: -10 });
    let bytes = Reply::LayerHitTestReply {
        found: true,
        app_id: "quickshell".into(),
        role: "panel".into(),
        name: "quickshell:panel".into(),
        bounds: (0, -40, 1920, 40),
    }
    .to_bytes();
    assert_eq!(
        bytes,
        b"LAYER_HIT_TEST_REPLY 1 quickshell panel quickshell:panel 0 -40 1920 40\n"
    );
    let miss = Reply::LayerHitTestReply {
        found: false,
        app_id: String::new(),
        role: String::new(),
        name: String::new(),
        bounds: (0, 0, 0, 0),
    }
    .to_bytes();
    assert_eq!(miss, b"LAYER_HIT_TEST_REPLY 0\n");
}

#[test]
fn dispatch_harness_layer_hit_test_hands_off_instead_of_default_miss() {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use niri::inject::calloop;
    use niri::inject::dispatch::{DispatchHarness, DispatchState, LayerHitDispatchReq};
    use niri::inject::protocol::InjectHarness;
    use niri::inject::PendingGrant;
    use niri::input::layer_hit_test::LayerHitReply;
    use niri::inject::smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

    let (tx, rx) = calloop::channel::channel::<LayerHitDispatchReq>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let _pump = std::thread::spawn(move || {
        let mut event_loop = calloop::EventLoop::<()>::try_new().expect("loop");
        event_loop
            .handle()
            .insert_source(rx, |event, _, _| {
                if let calloop::channel::Event::Msg(req) = event {
                    let _ = req.reply.send(LayerHitReply {
                        found: true,
                        app_id: "handoff-proof".into(),
                        role: "panel".into(),
                        name: "quickshell:panel".into(),
                        bounds: (req.x, req.y, 8, 8),
                    });
                }
            })
            .expect("insert layer-hit source");
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
    state.layer_hit_tx = Some(tx);
    let mut harness = DispatchHarness::new(&mut state);
    let hit = InjectHarness::layer_hit_test(&mut harness, 12, 34);
    assert!(
        hit.found,
        "worker path must not return the trait-default miss"
    );
    assert_eq!(hit.app_id, "handoff-proof");
    assert_eq!(hit.role, "panel");
    assert_eq!(hit.bounds, (12, 34, 8, 8));
}
