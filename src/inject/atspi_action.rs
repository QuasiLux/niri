//! v40.14.15: AT-SPI Action.DoAction before agent-seat pointer_click.
//!
//! Live bus calls are `#[cfg(feature = "dbus")]` and go through the a11y bus
//! helper in [`crate::inject::atspi_ladder`]. Tests inject an override probe
//! and must compile with or without the dbus feature.

use std::cell::RefCell;

use crate::inject::atspi_ladder::{
    ACTION_NAME_FALLBACK_CHAIN_V40_14_15, ACTION_NAME_FALLBACK_CHAIN_V40_14_15_IDENT,
    INNER_SWITCH_RESOLUTION_V40_14_15_IDENT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleOutcome {
    Flipped,
    GrabFocusFallbackUsed,
    PointerClickFallback,
    NoActionExposed,
    Empty,
}

pub struct ActionProbe {
    pub names: Vec<String>,
    pub do_action: fn(i32) -> bool,
    pub grab_focus: fn() -> bool,
}

thread_local! {
    static ACTION_OVERRIDE: RefCell<Option<ActionProbe>> = const { RefCell::new(None) };
}

pub fn set_action_override(v: Option<ActionProbe>) {
    ACTION_OVERRIDE.with(|slot| *slot.borrow_mut() = v);
}

pub(crate) fn clear_action_override() {
    set_action_override(None);
}

#[used]
static LBL_CLICK_ACTION_V40_14_15_IDENT: &[u8] = b"\x01LBL_CLICK_ACTION_V40_14_15\x01";

pub fn toggle_matched_node(dest: &str, obj_path: &str) -> ToggleOutcome {
    let _ = std::hint::black_box(ACTION_NAME_FALLBACK_CHAIN_V40_14_15_IDENT);
    let _ = std::hint::black_box(INNER_SWITCH_RESOLUTION_V40_14_15_IDENT);
    let _ = std::hint::black_box(LBL_CLICK_ACTION_V40_14_15_IDENT);
    let _ = std::hint::black_box("LBL_CLICK_ACTION_V40_14_15");

    if let Some((names, do_action, grab_focus)) = ACTION_OVERRIDE.with(|slot| {
        slot.borrow().as_ref().map(|p| (p.names.clone(), p.do_action, p.grab_focus))
    }) {
        return dispatch_action_chain(&names, do_action, grab_focus);
    }

    if dest.is_empty() || obj_path.is_empty() {
        return ToggleOutcome::Empty;
    }

    #[cfg(feature = "dbus")]
    {
        return live_toggle(dest, obj_path);
    }
    #[cfg(not(feature = "dbus"))]
    {
        ToggleOutcome::PointerClickFallback
    }
}

fn dispatch_action_chain(
    names: &[String],
    do_action: fn(i32) -> bool,
    grab_focus: fn() -> bool,
) -> ToggleOutcome {
    if try_fallback_chain(names, do_action) {
        return ToggleOutcome::Flipped;
    }
    if grab_focus() && try_fallback_chain(names, do_action) {
        return ToggleOutcome::GrabFocusFallbackUsed;
    }
    ToggleOutcome::PointerClickFallback
}

fn try_fallback_chain(names: &[String], do_action: fn(i32) -> bool) -> bool {
    let _ = std::hint::black_box(ACTION_NAME_FALLBACK_CHAIN_V40_14_15);
    for (canonical, _) in ACTION_NAME_FALLBACK_CHAIN_V40_14_15 {
        if let Some(idx) = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(canonical))
        {
            if do_action(idx as i32) {
                return true;
            }
        }
    }
    false
}

#[cfg(feature = "dbus")]
fn live_toggle(dest: &str, obj_path: &str) -> ToggleOutcome {
    let dest = dest.to_owned();
    let obj_path = obj_path.to_owned();
    match std::thread::spawn(move || live_toggle_on_a11y_bus(&dest, &obj_path)).join() {
        Ok(outcome) => outcome,
        Err(_) => ToggleOutcome::PointerClickFallback,
    }
}

#[cfg(feature = "dbus")]
fn live_toggle_on_a11y_bus(dest: &str, obj_path: &str) -> ToggleOutcome {
    let conn = match crate::inject::atspi_ladder::connect_a11y_bus() {
        Ok(c) => c,
        Err(_) => return ToggleOutcome::PointerClickFallback,
    };

    let n_actions = match get_n_actions(&conn, dest, obj_path) {
        Ok(n) if n > 0 => n,
        _ => return ToggleOutcome::NoActionExposed,
    };

    let mut names = Vec::with_capacity(n_actions as usize);
    for i in 0..n_actions {
        names.push(get_action_name(&conn, dest, obj_path, i).unwrap_or_default());
    }

    if try_fallback_chain_dyn(&names, |idx| call_do_action(&conn, dest, obj_path, idx)) {
        return ToggleOutcome::Flipped;
    }
    if call_grab_focus(&conn, dest, obj_path)
        && try_fallback_chain_dyn(&names, |idx| call_do_action(&conn, dest, obj_path, idx))
    {
        return ToggleOutcome::GrabFocusFallbackUsed;
    }
    ToggleOutcome::PointerClickFallback
}

#[cfg(feature = "dbus")]
fn try_fallback_chain_dyn(names: &[String], mut do_action: impl FnMut(i32) -> bool) -> bool {
    let _ = std::hint::black_box(ACTION_NAME_FALLBACK_CHAIN_V40_14_15);
    for (canonical, _) in ACTION_NAME_FALLBACK_CHAIN_V40_14_15 {
        if let Some(idx) = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(canonical))
        {
            if do_action(idx as i32) {
                return true;
            }
        }
    }
    false
}

#[cfg(feature = "dbus")]
const ACTION: &str = "org.a11y.atspi.Action";
#[cfg(feature = "dbus")]
const COMPONENT: &str = "org.a11y.atspi.Component";
#[cfg(feature = "dbus")]
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

#[cfg(feature = "dbus")]
fn get_n_actions(
    conn: &zbus::blocking::Connection,
    dest: &str,
    path: &str,
) -> Result<i32, ()> {
    let message = conn
        .call_method(
            Some(dest),
            path,
            Some(PROPERTIES),
            "Get",
            &(ACTION, "NActions"),
        )
        .map_err(|_| ())?;
    let owned: zbus::zvariant::OwnedValue = message.body().deserialize().map_err(|_| ())?;
    i32::try_from(owned).map_err(|_| ())
}

#[cfg(feature = "dbus")]
fn get_action_name(
    conn: &zbus::blocking::Connection,
    dest: &str,
    path: &str,
    idx: i32,
) -> Option<String> {
    let message = conn
        .call_method(Some(dest), path, Some(ACTION), "GetName", &idx)
        .ok()?;
    message.body().deserialize().ok()
}

#[cfg(feature = "dbus")]
fn call_do_action(
    conn: &zbus::blocking::Connection,
    dest: &str,
    path: &str,
    idx: i32,
) -> bool {
    let Ok(message) = conn.call_method(Some(dest), path, Some(ACTION), "DoAction", &idx) else {
        return false;
    };
    message.body().deserialize::<bool>().unwrap_or(false)
}

#[cfg(feature = "dbus")]
fn call_grab_focus(conn: &zbus::blocking::Connection, dest: &str, path: &str) -> bool {
    let Ok(message) = conn.call_method(Some(dest), path, Some(COMPONENT), "GrabFocus", &())
    else {
        return false;
    };
    message.body().deserialize::<bool>().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use crate::inject::atspi_ladder::{find_inner_switch, TreeNode};

    #[test]
    fn walk_dump_finds_inner_switch_on_gtk4_toggle() {
        let nodes = vec![TreeNode {
            role: "panel".into(),
            name: "row".into(),
            x: 10.0,
            y: 20.0,
            w: 400.0,
            h: 40.0,
            visible: true,
            n_actions: 0,
            children: vec![TreeNode {
                role: "toggle button".into(),
                name: "Mute audio".into(),
                x: 10.0,
                y: 20.0,
                w: 34.0,
                h: 34.0,
                visible: true,
                n_actions: 1,
                action_name: "click".into(),
                children: vec![],
                ..Default::default()
            }],
            ..Default::default()
        }];
        let hit = find_inner_switch(&nodes).expect("inner switch");
        assert_eq!(hit.outer_x, 10.0);
        assert_eq!(hit.outer_y, 20.0);
        assert_eq!(hit.outer_w, 400.0);
        assert_eq!(hit.outer_h, 40.0);
        assert_eq!(hit.n_actions, 1);
        assert_eq!(hit.action_name, "click");
    }

    thread_local! {
        static LAST_DO_ACTION_IDX: Cell<Option<i32>> = const { Cell::new(None) };
    }

    fn record_false(idx: i32) -> bool {
        LAST_DO_ACTION_IDX.with(|c| c.set(Some(idx)));
        false
    }

    fn record_true(idx: i32) -> bool {
        LAST_DO_ACTION_IDX.with(|c| c.set(Some(idx)));
        true
    }

    fn grab_focus_false() -> bool {
        false
    }

    #[test]
    fn action_chain_invokes_toggle_when_click_fails() {
        LAST_DO_ACTION_IDX.with(|c| c.set(None));
        set_action_override(Some(ActionProbe {
            names: vec!["toggle".into()],
            do_action: record_false,
            grab_focus: grab_focus_false,
        }));
        let outcome = toggle_matched_node("unused", "unused");
        assert_eq!(LAST_DO_ACTION_IDX.with(|c| c.get()), Some(0));
        assert_ne!(outcome, ToggleOutcome::Flipped);

        LAST_DO_ACTION_IDX.with(|c| c.set(None));
        set_action_override(Some(ActionProbe {
            names: vec!["toggle".into()],
            do_action: record_true,
            grab_focus: grab_focus_false,
        }));
        let outcome = toggle_matched_node("unused", "unused");
        assert_eq!(LAST_DO_ACTION_IDX.with(|c| c.get()), Some(0));
        assert_eq!(outcome, ToggleOutcome::Flipped);

        set_action_override(None);
    }
}
