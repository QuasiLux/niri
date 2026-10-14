//! v40.21 layer widget tree. Pure fixtures — no display, no socket, no dbus.

use std::hint::black_box;

use niri::inject::protocol::{
    parse_frame, Frame, FrameError, Reply, LAYER_WIDGET_TREE_V40_21,
};
use niri::input::layer_hit_test::LayerHitSurface;
use niri::input::layer_widget_tree::{
    widget_tree_from_surfaces, LayerWidget, WidgetNode,
};

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
fn hit_noctalia_bar_center_names_panel() {
    let reply = widget_tree_from_surfaces(&[bar(), launcher()], &[], 960, 16);
    assert!(reply.found);
    assert_eq!(reply.app_id, "quickshell");
    assert_eq!(reply.layer, "quickshell:panel");
    assert!(reply
        .widgets
        .iter()
        .any(|w| w.name == "quickshell:panel"));
}

#[test]
fn hit_launcher_button_role_or_name() {
    let reply = widget_tree_from_surfaces(&[bar(), launcher()], &[], 16, 12);
    assert!(reply.found);
    assert!(reply.widgets.iter().any(|w| w.role == "launcher" || w.name == "launcher"));
}

#[test]
fn hit_window_area_is_not_a_layer() {
    let reply = widget_tree_from_surfaces(&[bar(), launcher()], &[], 400, 400);
    assert!(!reply.found);
    assert!(reply.widgets.is_empty());
    assert!(reply.app_id.is_empty());
}

#[test]
fn hit_outside_any_layer() {
    let reply = widget_tree_from_surfaces(&[bar(), launcher()], &[], 5000, 5000);
    assert!(!reply.found);
}

#[test]
fn hit_occluded_layer_is_hidden() {
    let mut hidden = bar();
    hidden.occluded = true;
    let reply = widget_tree_from_surfaces(&[hidden], &[], 960, 16);
    assert!(!reply.found);
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
    let reply = widget_tree_from_surfaces(&[panel], &[], 100, -10);
    assert!(reply.found);
    assert_eq!(reply.widgets[0].bounds, (0, -40, 1920, 40));
    assert_eq!(reply.layer, "quickshell:panel");
}

#[test]
fn child_widget_parent_index_follows_hit_path() {
    let roots = vec![WidgetNode {
        name: "quickshell:panel".into(),
        role: "panel".into(),
        bounds: (0, 0, 1920, 32),
        children: vec![WidgetNode {
            name: "launcher".into(),
            role: "launcher".into(),
            bounds: (8, 4, 24, 24),
            children: Vec::new(),
        }],
    }];
    let reply = widget_tree_from_surfaces(&[bar()], &roots, 16, 12);
    assert!(reply.found);
    assert!(reply.widgets.len() >= 2);
    assert_eq!(reply.widgets[0].parent_index, None);
    let launcher = reply
        .widgets
        .iter()
        .find(|w| w.role == "launcher" || w.name == "launcher")
        .expect("launcher widget");
    assert_eq!(launcher.parent_index, Some(0));
}

#[test]
fn layer_widget_tree_req_parses_and_reply_bytes_match_cua_driver() {
    let _ = black_box(LAYER_WIDGET_TREE_V40_21);
    let (frame, rest) = parse_frame(b"LAYER_WIDGET_TREE 100 -10\n").expect("parse");
    assert!(rest.is_empty());
    assert_eq!(frame, Frame::LayerWidgetTreeReq { x: 100, y: -10 });
    assert!(parse_frame(b"LAYER_WIDGET_TREE 1 2\n").is_ok());
    assert!(!matches!(
        parse_frame(b"LAYER_WIDGET_TREE 1 2\n"),
        Err(FrameError::UnknownOpcode)
    ));

    let miss = Reply::LayerWidgetTreeReply {
        found: false,
        app_id: String::new(),
        layer: String::new(),
        widgets: Vec::new(),
    }
    .to_bytes();
    assert_eq!(miss, b"LAYER_WIDGET_TREE_REPLY 0\n");

    let hit = Reply::LayerWidgetTreeReply {
        found: true,
        app_id: "quickshell".into(),
        layer: "quickshell:panel".into(),
        widgets: vec![
            LayerWidget {
                name: "quickshell:panel".into(),
                role: "panel".into(),
                bounds: (0, 0, 1920, 32),
                parent_index: None,
            },
            LayerWidget {
                name: "launcher".into(),
                role: "launcher".into(),
                bounds: (8, 4, 24, 24),
                parent_index: Some(0),
            },
        ],
    }
    .to_bytes();
    assert_eq!(
        hit,
        b"LAYER_WIDGET_TREE_REPLY 1 quickshell quickshell:panel 2\n\
[panel] \"quickshell:panel\" - 0 0 1920 32\n\
[launcher] \"launcher\" 0 8 4 24 24\n"
    );
}
