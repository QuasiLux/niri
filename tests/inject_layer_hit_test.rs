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
