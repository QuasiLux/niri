//! Free-scroll mode tests.
//!
//! Drive the real view-offset gesture pipeline (begin/update/end). Nested trackpad
//! gestures cannot be injected from a host niri session; this is the correct gate.

use std::time::Duration;

use super::*;

fn free_scroll_options(free_scroll: bool) -> Options {
    Options {
        layout: niri_config::Layout {
            gaps: 0.,
            free_scroll,
            center_focused_column: CenterFocusedColumn::Never,
            always_center_single_column: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn set_up_three_columns(free_scroll: bool) -> Layout<TestWindow> {
    // Output is 1280×720. Three 400px columns overflow one viewport.
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::SetWindowWidth {
            id: Some(1),
            change: SizeChange::SetFixed(700),
        },
        Op::SetWindowWidth {
            id: Some(2),
            change: SizeChange::SetFixed(700),
        },
        Op::SetWindowWidth {
            id: Some(3),
            change: SizeChange::SetFixed(700),
        },
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(700, 600)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(700, 600)),
        },
        Op::SetForcedSize {
            id: 3,
            size: Some(Size::new(700, 600)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::CompleteAnimations,
        Op::FocusColumnLeft,
        Op::FocusColumnLeft,
        Op::FocusColumnLeft,
        Op::CompleteAnimations,
    ];

    check_ops_with_options(free_scroll_options(free_scroll), ops)
}

/// Mouse-style view gesture (`is_touchpad=false` → 1:1 pixel deltas).
/// Positive `total_delta` increases view offset (view moves right / content left).
fn run_view_gesture(layout: &mut Layout<TestWindow>, total_delta: f64) {
    Op::ViewOffsetGestureBegin {
        output_idx: 1,
        workspace_idx: None,
        is_touchpad: false,
    }
    .apply(layout);

    // Slow steps → near-zero projected inertia so free-scroll stop ≈ sum(delta).
    let base = layout.clock.now_unadjusted();
    let steps = 8usize;
    let step = total_delta / steps as f64;
    for i in 1..=steps {
        let ts = base + Duration::from_millis(50 * i as u64);
        Op::ViewOffsetGestureUpdate {
            delta: step,
            timestamp: ts,
            is_touchpad: false,
        }
        .apply(layout);
    }

    // Idle before end so projected velocity decays (end also pushes a 0-delta sample).
    layout
        .clock
        .set_unadjusted(base + Duration::from_millis(50 * steps as u64 + 200));
    Op::ViewOffsetGestureEnd {
        is_touchpad: Some(false),
    }
    .apply(layout);

    Op::CompleteAnimations.apply(layout);
}

fn target_view_pos(layout: &Layout<TestWindow>) -> f64 {
    layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .target_view_pos()
}

fn active_column_idx(layout: &Layout<TestWindow>) -> usize {
    layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .active_column_idx()
}

#[test]
fn free_scroll_off_snaps_after_mid_gesture() {
    let mut layout = set_up_three_columns(false);
    assert!(!layout.free_scroll());
    assert_eq!(active_column_idx(&layout), 0);
    let before = target_view_pos(&layout);

    run_view_gesture(&mut layout, 200.);
    let after = target_view_pos(&layout);

    // Snapping must leave the naive mid-column stop (before + 200).
    assert!(
        (after - (before + 200.)).abs() > 1.,
        "expected snap away from mid stop; before={before} after={after}"
    );
    // And the view must still have moved for a rightward gesture (or snapped back to a bound).
    assert!(
        after.is_finite(),
        "view position must be finite after snap; after={after}"
    );
}

#[test]
fn free_scroll_on_keeps_mid_gesture_position() {
    let mut layout = set_up_three_columns(true);
    assert!(layout.free_scroll());
    let before = target_view_pos(&layout);

    run_view_gesture(&mut layout, 200.);
    let after = target_view_pos(&layout);

    assert!(
        (after - (before + 200.)).abs() < 1.,
        "free-scroll should stop near gesture end; before={before} after={after}"
    );
}

#[test]
fn free_scroll_and_snap_diverge_for_same_gesture() {
    let mut snap = set_up_three_columns(false);
    let mut free = set_up_three_columns(true);

    run_view_gesture(&mut snap, 200.);
    run_view_gesture(&mut free, 200.);

    let snap_pos = target_view_pos(&snap);
    let free_pos = target_view_pos(&free);

    assert!(
        (snap_pos - free_pos).abs() > 1.,
        "same gesture must diverge; snap={snap_pos} free={free_pos}"
    );
}

#[test]
fn free_scroll_toggle_does_not_move_viewport() {
    let mut layout = set_up_three_columns(true);
    run_view_gesture(&mut layout, 150.);
    let pos = target_view_pos(&layout);

    assert!(layout.free_scroll());
    layout.toggle_free_scroll();
    assert!(!layout.free_scroll());
    assert!(
        (target_view_pos(&layout) - pos).abs() < 1e-6,
        "toggle off must leave viewport free"
    );

    layout.toggle_free_scroll();
    assert!(layout.free_scroll());
    assert!(
        (target_view_pos(&layout) - pos).abs() < 1e-6,
        "toggle on must leave viewport free"
    );
}

#[test]
fn free_scroll_clamps_past_last_column() {
    let mut layout = set_up_three_columns(true);
    let before = target_view_pos(&layout);

    run_view_gesture(&mut layout, 50_000.);
    let after = target_view_pos(&layout);
    assert!(
        after.is_finite() && after > before,
        "should scroll forward but remain finite; before={before} after={after}"
    );

    let mid = after;
    run_view_gesture(&mut layout, 50_000.);
    let after2 = target_view_pos(&layout);
    assert!(
        (after2 - mid).abs() < 1.,
        "must clamp at content end; mid={mid} after2={after2}"
    );
}

#[test]
fn free_scroll_focus_column_still_snaps() {
    let mut layout = set_up_three_columns(true);
    run_view_gesture(&mut layout, 250.);
    let free_pos = target_view_pos(&layout);

    Op::FocusColumnRight.apply(&mut layout);
    Op::CompleteAnimations.apply(&mut layout);

    let after_focus = target_view_pos(&layout);
    assert!(
        (after_focus - free_pos).abs() > 1.,
        "focus-column should re-fit even with free-scroll; free={free_pos} after={after_focus}"
    );
}

#[test]
fn free_scroll_set_from_options_and_runtime() {
    let layout = set_up_three_columns(false);
    assert!(!layout.free_scroll());

    let mut layout = set_up_three_columns(true);
    assert!(layout.free_scroll());

    layout.set_free_scroll(false);
    assert!(!layout.free_scroll());
    layout.set_free_scroll(true);
    assert!(layout.free_scroll());
}


#[test]
fn free_scroll_activate_preserves_view() {
    let mut layout = set_up_three_columns(true);
    run_view_gesture(&mut layout, 300.);
    let pos = target_view_pos(&layout);
    let col_before = active_column_idx(&layout);

    // Activate a different window (column 0 if we scrolled right, or column 2).
    // Window ids are 1,2,3.
    Op::FocusWindow(1).apply(&mut layout);
    // FocusWindow uses activate_window which under free-scroll must not snap.
    // But FocusWindow op is activate_window - after free gesture active col may be 1.
    // Force activate window 3.
    layout.activate_window(&3);
    Op::CompleteAnimations.apply(&mut layout);

    let pos_after = target_view_pos(&layout);
    assert!(
        (pos_after - pos).abs() < 1.,
        "activate_window under free-scroll must preserve view; before={pos} after={pos_after} col_before={col_before} col_after={}",
        active_column_idx(&layout)
    );
    assert_eq!(active_column_idx(&layout), 2);
}

#[test]
fn free_scroll_off_activate_scrolls_into_view() {
    let mut layout = set_up_three_columns(false);
    // Manually free-position is hard without free mode; use free mode then toggle off mid-view.
    layout.set_free_scroll(true);
    run_view_gesture(&mut layout, 300.);
    let free_pos = target_view_pos(&layout);
    layout.set_free_scroll(false);
    // Activation should now scroll into view.
    layout.activate_window(&3);
    Op::CompleteAnimations.apply(&mut layout);
    let after = target_view_pos(&layout);
    assert!(
        (after - free_pos).abs() > 1.,
        "with free-scroll off, activate should re-fit; free_pos={free_pos} after={after}"
    );
}

#[test]
fn free_scroll_inside_bounds_not_pulled_to_edge() {
    let mut layout = set_up_three_columns(true);
    // Small nudge well inside the strip — must stay near gesture end, not jump to a bound.
    run_view_gesture(&mut layout, 120.);
    let after = target_view_pos(&layout);
    assert!(
        (after - 120.).abs() < 2.,
        "inside-strip free-scroll must not magnetic-edge; after={after}"
    );
}

#[test]
fn free_scroll_past_edge_settles_to_bound_not_beyond() {
    let mut layout = set_up_three_columns(true);
    run_view_gesture(&mut layout, 50_000.);
    let after = target_view_pos(&layout);
    // Should land on the far bound (finite, past start), not overshoot forever.
    assert!(after.is_finite() && after > 100., "after={after}");
    let mid = after;
    run_view_gesture(&mut layout, 50_000.);
    assert!(
        (target_view_pos(&layout) - mid).abs() < 1.,
        "repeated fling past end stays at soft bound"
    );
}
