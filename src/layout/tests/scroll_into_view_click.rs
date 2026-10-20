//! v40.38: an off-viewport full-width column used to miss `contents_under`
//! (`click_contents_under_none`). Scrolling that column into view — not
//! `activate_column_preserve_view` — puts the window-relative center on that
//! window, which is the surface the user-seat click must hit.

use smithay::utils::{Logical, Point, Size};

use super::*;

fn options_free_scroll_off() -> Options {
    Options {
        layout: niri_config::Layout {
            gaps: 0.,
            free_scroll: false,
            center_focused_column: CenterFocusedColumn::Never,
            always_center_single_column: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn window_origin_and_size(
    layout: &Layout<TestWindow>,
    id: usize,
) -> (Point<f64, Logical>, Size<f64, Logical>) {
    let ws = layout.active_workspace().unwrap();
    let (tile, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(tile, _, _)| *tile.window().id() == id)
        .unwrap_or_else(|| panic!("missing window {id}"));
    // Same top-left `window_hit_geom` adds the window-relative center to.
    (pos + tile.window_loc(), tile.animated_window_size())
}

fn contains(origin: Point<f64, Logical>, size: Size<f64, Logical>, pt: Point<f64, Logical>) -> bool {
    pt.x >= origin.x && pt.y >= origin.y && pt.x < origin.x + size.w && pt.y < origin.y + size.h
}

#[test]
fn off_viewport_full_width_column_was_click_contents_under_none_then_scroll_hits_that_window() {
    // 1280×720 output. Two 1200px columns: the left column is entirely off
    // the output when the view is on the right column.
    let mut layout = check_ops_with_options(
        options_free_scroll_off(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::SetWindowWidth {
                id: Some(1),
                change: SizeChange::SetFixed(1200),
            },
            Op::SetWindowWidth {
                id: Some(2),
                change: SizeChange::SetFixed(1200),
            },
            Op::SetForcedSize {
                id: 1,
                size: Some(Size::new(1200, 600)),
            },
            Op::SetForcedSize {
                id: 2,
                size: Some(Size::new(1200, 600)),
            },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::CompleteAnimations,
        ],
    );

    // Live spare: keyboard focus moved to the off-viewport column via
    // activate_column_preserve_view, so the column is already active and
    // activate_column alone would no-op.
    layout.set_free_scroll(true);
    layout.activate_window(&1);
    Op::CompleteAnimations.apply(&mut layout);

    let active = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .active_column_idx();
    assert_eq!(active, 0, "window 1 must be the active column without scrolling");

    let (origin1, size1) = window_origin_and_size(&layout, 1);
    let (origin2, size2) = window_origin_and_size(&layout, 2);
    // Window-relative center, inside the 1200×600 window. Not a scale multiply.
    let center = Point::<f64, Logical>::from((600.0, 100.0));
    let global = origin1 + center;
    assert!(
        contains(origin1, size1, global),
        "center must be inside window 1 before scroll; origin={origin1:?} size={size1:?} global={global:?}"
    );
    assert!(
        global.x < 0.0 || global.x >= 1280.0 || global.y < 0.0 || global.y >= 720.0,
        "off-viewport column must take click_contents_under_none (point not on the 1280×720 output); global={global:?} origin1={origin1:?}"
    );
    assert!(
        !contains(origin2, size2, global),
        "the miss must not already be window 2 (the viewport column); global={global:?} origin2={origin2:?}"
    );
    assert!(
        origin1.x < 0.0,
        "window 1 must start left of the output; origin={origin1:?} size={size1:?}"
    );
    // `window_under` is clipped to the output, same miss as `contents_under`
    // when the point is not on any output. That is `click_contents_under_none`.
    let output = layout.active_output().unwrap().clone();
    assert!(
        layout.window_under(&output, global).is_none(),
        "off-output center must not resolve a surface before the scroll; global={global:?}"
    );

    assert!(layout.scroll_window_column_into_view(&1));
    // No CompleteAnimations: the helper must snap view_offset.current() itself.

    let (origin1, size1) = window_origin_and_size(&layout, 1);
    let (origin2, size2) = window_origin_and_size(&layout, 2);
    let global = origin1 + center;
    assert!(
        global.x >= 0.0 && global.x < 1280.0 && global.y >= 0.0 && global.y < 720.0,
        "after scroll the center must be on the output; global={global:?} origin1={origin1:?}"
    );
    assert!(
        contains(origin1, size1, global),
        "click must land in window 1, the off-viewport column; origin={origin1:?} size={size1:?} global={global:?}"
    );
    assert!(
        !contains(origin2, size2, global),
        "must not click the viewport window; origin2={origin2:?} size={size2:?} global={global:?}"
    );
    // The user-seat click is dispatched to this hit, not to window 2 and not
    // to a `(0, 0)` fallback. `click_atspi_center_on_user_seat` refuses a
    // `contents_under` window that is not `wid`.
    let hit_id = layout
        .window_under(&output, global)
        .map(|(win, _)| *win.id());
    assert_eq!(
        hit_id,
        Some(1),
        "after scroll the click must hit window 1's surface; global={global:?}"
    );
}
