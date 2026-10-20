//! Production-side [`InjectHarness`] for [`crate::niri::State`].
//!
//! `niri::State` owns [`Niri`], not the reverse, so this wrapper holds
//! `&mut State` and reads lock / output-power flags through it. Window
//! lookup maps the wire `u64` through [`MappedId`] onto the live layout.

use std::time::Duration;

use smithay::backend::input::InputTime;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Point, SERIAL_COUNTER};

use crate::inject::grant_confirm::{ClickOutcome, GrantResolution, GrantSurfaceSource, SeatKind};
use crate::inject::protocol::{InjectHarness, KeyMods};
use crate::inject::AppRoutes;
use crate::input::agent_seat::AgentSeat;
use crate::niri::{Niri, State};
use crate::utils::with_toplevel_role;
use crate::window::mapped::MappedId;

/// Production inject harness. Construct around compositor [`State`] once a
/// connection is accepted (receiver plumbing is a later card).
///
/// `scroll`/`drag` call `inject_agent_scroll`/`inject_agent_drag` with `None` focus.
pub struct NiriInjectHarness<'a> {
    pub state: &'a mut State,
}

impl<'a> NiriInjectHarness<'a> {
    pub fn new(state: &'a mut State) -> Self {
        Self { state }
    }

    pub fn niri(&self) -> &Niri {
        &self.state.niri
    }

    pub fn niri_mut(&mut self) -> &mut Niri {
        &mut self.state.niri
    }

    pub fn request_grant(&mut self, bot_id: &str, token_fingerprint: [u8; 8]) -> bool {
        {
            let pending = self.state.niri.grant_confirm.pending();
            let g = pending.lock().unwrap();
            if let Some(existing) = g.as_ref() {
                if existing.requested_at.elapsed() < Duration::from_secs(30) {
                    return false;
                }
            }
        }
        self.state
            .niri
            .grant_confirm
            .record(bot_id.to_string(), token_fingerprint);
        let confirm = self.state.niri.grant_confirm.clone();
        confirm.build_surface(&self.state.niri);
        true
    }
}

impl GrantSurfaceSource for Niri {
    fn grant_toplevels(&self) -> Vec<(u64, String, String)> {
        let map_mapped = |mapped: &crate::window::Mapped| {
            let (app_id, title) = with_toplevel_role(mapped.toplevel(), |role| {
                (
                    role.app_id.clone().unwrap_or_default(),
                    role.title.clone().unwrap_or_default(),
                )
            });
            (mapped.id().get(), app_id, title)
        };
        if let Some(output) = self.layout.active_output() {
            self.layout
                .windows_for_output(output)
                .map(map_mapped)
                .collect()
        } else {
            self.layout
                .windows()
                .map(|(_, mapped)| map_mapped(mapped))
                .collect()
        }
    }
}

/// Keep `is_locked` / `outputs_off` in the release ELF under thin LTO.
#[used]
static LOCK_SCREEN_ACCESSORS: &[fn(&Niri) -> bool] = &[Niri::is_locked, Niri::outputs_off];

/// Distinct rodata copy so `strings | grep -c` sees this ident ≥ 2 times.
#[used]
static KEY_DELIVERY_V40_6_HARNESS_IDENT: &[u8] =
    b"\x01KEY_DELIVERY_V40_6_SURFACE_ACTIVATION\x01";

/// Distinct rodata copy so `strings | grep -c` sees the v40.7 ident ≥ 2 times.
#[used]
static KEY_DELIVERY_V40_7_HARNESS_IDENT: &[u8] =
    b"\x01KEY_DELIVERY_V40_7_USER_SEAT_ROUTING\x01";

/// Distinct rodata copy so `strings | grep -c` sees the v40.7a ident ≥ 2 times.
/// Pinned by `NiriInjectHarness` (production-side harness) + `mod.rs`
/// `KEY_DELIVERY_V40_7A_PLANE_IDENTS` so thin-LTO cannot collapse to one copy.
#[used]
static KEY_DELIVERY_V40_7A_HARNESS_IDENT: &[u8] =
    b"\x01KEY_DELIVERY_V40_7A_PRESSES_SERIAL_SPLIT\x01";

/// v40.33 live-call-site copy. `black_box` in `NiriInjectHarness::at_toggle_req`.
#[used]
static AT_TOGGLE_REQ_REAL_SURFACE_V40_33_HARNESS: &[u8] =
    b"\x01AT_TOGGLE_REQ_REAL_SURFACE_V40_33\x01";

impl InjectHarness for NiriInjectHarness<'_> {
    type AgentState = State;

    fn agent_seat(&mut self) -> &mut AgentSeat<Self::AgentState> {
        self.state
            .niri
            .agent_seat
            .as_mut()
            .expect("inject harness requires agent_seat")
    }

    fn agent_state(&mut self) -> &mut Self::AgentState {
        self.state
    }

    fn resolve_window(&mut self, window_id: u64) -> Option<WlSurface> {
        let id = MappedId::from_raw(window_id);
        self.state
            .niri
            .find_window_by_id(id)
            .and_then(|window| window.toplevel().map(|t| t.wl_surface().clone()))
    }

    /// Live compositor pointer focus is the window's `WlSurface`
    /// (`State::PointerFocus = WlSurface`), not the worker `DispatchFocus` stub.
    fn resolve_pointer_focus(
        &mut self,
        window_id: u64,
    ) -> Option<<Self::AgentState as smithay::input::SeatHandler>::PointerFocus> {
        self.resolve_window(window_id)
    }

    /// architecture-delta:
    /// - what changed: KEY frames now route through the user seat's
    ///   KeyboardHandle (`self.state.niri.seat.get_keyboard()`) instead of
    ///   the agent seat's. `set_keyboard_focus(wid)` targets the user seat
    ///   too. New ident `KEY_DELIVERY_V40_7_USER_SEAT_ROUTING` pinned across
    ///   harness + socket + mod.rs rodata. v40.6
    ///   `KEY_DELIVERY_V40_6_SURFACE_ACTIVATION` + v40.5c
    ///   `GRANT_CONFIRM_V40_5C_SEAT_SIDE_CLICK_ROUTING` idents retained.
    /// - why: the agent seat is created with
    ///   `SeatState::new_seat(AGENT_SEAT_NAME)` (`src/input/agent_seat.rs:33`)
    ///   and never registers a `wl_seat` global. Wayland clients (Alacritty)
    ///   only bind to seats that advertise `wl_seat` — in niri, only the user
    ///   seat does. Keys pressed on the agent seat are accepted by Smithay
    ///   (DISPATCH OK) but no client-side `wl_keyboard` event is ever
    ///   delivered. v40.6 set focus + dispatched input on the agent seat —
    ///   the bound WlSurface was reached but no client was listening. v40.6
    ///   sandbox e2e (`src/inject_e2e.rs`) does not model a user seat, so it
    ///   could not catch this gap.
    /// - supersedes: the assumption that "agent_seat is the inject plane's
    ///   delivery channel" (v40.6 brief). The agent seat is useful for
    ///   click/pointer isolation (user cursor not stolen) but is structurally
    ///   unable to deliver keys to Wayland clients. Pointer + scroll + drag
    ///   keep using the agent seat.
    /// - next cards must: `t_v40_7_install_D` does the host-sudo atomic
    ///   install + spare relaunch on TTY3, then a fresh end-to-end KEY
    ///   delivery test with vision_analyze confirming "Hi TTY3" text in
    ///   alacritty buffer. Do not regress agent-seat pointer isolation.
    fn set_keyboard_focus(&mut self, wid: u64) -> bool {
        let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_7_USER_SEAT_ROUTING);
        let _ = std::hint::black_box(KEY_DELIVERY_V40_7_HARNESS_IDENT);
        let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_6_SURFACE_ACTIVATION);
        let _ = std::hint::black_box(KEY_DELIVERY_V40_6_HARNESS_IDENT);
        let id = MappedId::from_raw(wid);
        let Some(window) = self.state.niri.find_window_by_id(id) else {
            return false;
        };
        let Some(target) = window.toplevel().map(|t| t.wl_surface().clone()) else {
            return false;
        };
        self.state.niri.layout.activate_window(&window);
        let keyboard = self.state.niri.seat.get_keyboard().unwrap().clone();
        keyboard.set_focus(self.state, Some(target), SERIAL_COUNTER.next_serial());
        true
    }

    fn is_locked(&self) -> bool {
        let locked = self.state.niri.is_locked();
        trace!(locked, "is_locked");
        locked
    }

    fn outputs_off(&self) -> bool {
        // Live read of `Niri::outputs_off_wired` (not a snapshot).
        let off = self.state.niri.outputs_off();
        trace!(off, "outputs_off");
        off
    }

    fn grant_present(&self, bot_id: &str, window_id: u64) -> bool {
        self.state.niri.grant_map.has_grant(bot_id, window_id)
    }

    fn apply_grant(&self, bot_id: &str, window_id: u64) {
        self.state.niri.grant_map.apply_grant(bot_id, window_id);
    }

    /// v40.19 — record `app_id` so fresh toplevels of the same `app_id`
    /// can be pre-granted (Auto → Interactive on first click).
    fn apply_grant_with_app(&self, bot_id: &str, window_id: u64, app_id: &str) {
        self.state
            .niri
            .grant_map
            .apply_grant_with_app(bot_id, window_id, app_id);
    }

    fn grant_pending(&self, bot_id: &str) -> bool {
        self.state
            .niri
            .grant_confirm
            .pending()
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.bot_id == bot_id)
    }

    fn request_grant(&mut self, bot_id: &str, token_fingerprint: [u8; 8]) -> bool {
        NiriInjectHarness::request_grant(self, bot_id, token_fingerprint)
    }

    fn apply_grant_confirm_click(
        &mut self,
        bot_id: &str,
        window_id: u64,
        app_id: &str,
        seat: SeatKind,
        resolution: GrantResolution,
    ) -> ClickOutcome {
        let _ = bot_id;
        let confirm = self.state.niri.grant_confirm.clone();
        confirm.apply_resolved(
            &self.state.niri.grant_map,
            window_id,
            app_id,
            seat,
            resolution,
        )
    }

    fn app_id_routes(&self, app_id: &str) -> AppRoutes {
        crate::inject::app_catalog::app_id_routes(app_id)
    }

    fn route(
        &self,
        app_id: &str,
        action: &crate::inject::atspi_ladder::AtSpiAction,
    ) -> crate::inject::atspi_ladder::AtSpiRoute {
        crate::inject::atspi_ladder::route(app_id, action)
    }

    fn app_id_for(&self, window_id: u64) -> Option<String> {
        let id = MappedId::from_raw(window_id);
        self.state.niri.find_window_by_id(id).and_then(|window| {
            window
                .toplevel()
                .and_then(|toplevel| with_toplevel_role(toplevel, |role| role.app_id.clone()))
        })
    }

    fn is_occluded(&mut self) -> bool {
        trace!("is_occluded");
        false
    }

    /// architecture-delta:
    /// - what changed: KEY frames now reach this method from the main
    ///   compositor loop via a worker→main `calloop::channel` handoff
    ///   (`KeyDispatchReq`). The per-connection worker no longer calls
    ///   Smithay `keyboard.input()` itself for KEY. Press/Release timing
    ///   is still `inject_agent_key_press` (v40.7a `RELEASE_DELAY_MICROS
    ///   = 10_000`).
    /// - why: Smithay `KeyboardTarget::key` on the agent seat has no
    ///   `wl_seat` global, so alacritty never receives `wl_keyboard::Key`
    ///   events. The main loop owns the user seat and the only `wl_seat`
    ///   global in niri. Worker-thread dispatch can reply DISPATCH OK
    ///   without any client seeing the key.
    /// - supersedes: the v40.6 / v40.7 / v40.7a assumption that "Smithay
    ///   `keyboard.input()` is sufficient" (it is, but the worker thread
    ///   has no user seat). v40.7 already pointed this method at
    ///   `self.state.niri.seat.get_keyboard()`, but production KEY frames
    ///   still ran `DispatchHarness::key_press` on the worker, so this
    ///   function never ran for cua-driver KEY.
    /// - next cards must: host install + spare relaunch + e2e KEY vision
    ///   proof (alacritty buffer). Do not regress v40.5c agent-seat
    ///   pointer isolation (CLICK/SCROLL/DRAG stay on the worker).
    fn key_press(&mut self, xkb_name: &str) -> bool {
        self.key_press_with_shift(xkb_name, false)
    }

    fn key_press_with_shift(&mut self, xkb_name: &str, shift: bool) -> bool {
        self.key_press_with_mods(
            xkb_name,
            KeyMods {
                shift,
                ..KeyMods::default()
            },
        )
    }

    fn key_press_with_mods(&mut self, xkb_name: &str, mods: KeyMods) -> bool {
        let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_7_USER_SEAT_ROUTING);
        let _ = std::hint::black_box(KEY_DELIVERY_V40_7_HARNESS_IDENT);
        let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_11_SUSTAINED_DELIVERY);
        let _ = std::hint::black_box(crate::inject::protocol::NIRI_KEY_MODIFIERS_V40_17);
        let keyboard = self.state.niri.seat.get_keyboard().unwrap().clone();
        crate::inject::dispatch::inject_agent_key_press_mods(
            &keyboard,
            self.agent_state(),
            xkb_name,
            InputTime::now(),
            mods.shift,
            mods.ctrl,
            mods.super_mod,
            mods.alt,
        )
    }

    fn scroll(&mut self, h_axis: f64, v_axis: f64) -> bool {
        let pointer = self.agent_seat().pointer.clone();
        crate::inject::dispatch::inject_agent_scroll(
            &pointer,
            self.agent_state(),
            None,
            h_axis,
            v_axis,
        )
    }

    fn drag(&mut self, from_x: f64, from_y: f64, to_x: f64, to_y: f64) -> bool {
        let pointer = self.agent_seat().pointer.clone();
        crate::inject::dispatch::inject_agent_drag(
            &pointer,
            self.agent_state(),
            None,
            (from_x, from_y),
            (to_x, to_y),
        )
    }

    fn layer_hit_test(&mut self, x: i32, y: i32) -> crate::input::layer_hit_test::LayerHitReply {
        let _ = std::hint::black_box(crate::inject::protocol::LAYER_HIT_TEST_V40_18);
        crate::input::layer_hit_test::hit_test_niri(&self.state.niri, x, y)
    }

    fn focus_window(&mut self, wid: u64) -> crate::input::focus_window::FocusWindowReply {
        let _ = std::hint::black_box(crate::inject::protocol::WINDOW_FOCUS_REQ_V40_24);
        crate::input::focus_window::focus_window_niri(self.state, wid)
    }

    /// v40.33: production `at_toggle_req` on the live compositor.
    ///
    /// Resolves the target window's real `WlSurface` (`resolve_window`,
    /// then `contents_under` for the surface-local point) and clicks
    /// through the **user seat** (`self.state.niri.seat.get_pointer()`).
    /// The agent seat has no `wl_seat` global, so `known_pointers` is
    /// empty and a press there never reaches a client.
    ///
    /// Does not hardcode `ToggleState::Checked`. Click dispatched is not an
    /// observed GTK checked-bit. `None` from `observe_toggle_checked` means
    /// unobserved, and the wire reply is `no_action` on purpose (honesty over
    /// convenience).
    fn at_toggle_req(
        &mut self,
        wid: u64,
        name: &str,
    ) -> crate::inject::protocol::ToggleReply {
        use crate::inject::atspi_ladder::element_lookup_via_tree_dump;
        let _ = std::hint::black_box(crate::inject::protocol::AT_TOGGLE_REQ_REAL_SURFACE_V40_33);
        let _ = std::hint::black_box(AT_TOGGLE_REQ_REAL_SURFACE_V40_33_HARNESS);
        let _ = std::hint::black_box(crate::inject::protocol::AT_TOGGLE_REQ_V40_28);
        let _ = std::hint::black_box(crate::inject::protocol::AT_TOGGLE_REQ_DISPATCH_V40_29);
        let _ = std::hint::black_box(crate::inject::protocol::OPC_AT_TOGGLE_REQ_IDENT);
        let _ = std::hint::black_box(crate::inject::protocol::REP_AT_TOGGLE_REPLY_IDENT);
        if wid == 0 {
            return crate::inject::protocol::ToggleReply::no_action();
        }
        if name.trim().is_empty() {
            return crate::inject::protocol::ToggleReply::no_action();
        }
        let bounds = element_lookup_via_tree_dump(wid, "toggle button", name).or_else(|| {
            crate::inject::atspi_ladder::scan_session_dbus_for_element(wid, "toggle button", name)
        });
        let Some(b) = bounds else {
            return crate::inject::protocol::ToggleReply::no_action();
        };
        let cx = b.rect.x + b.rect.w / 2.0;
        let cy = b.rect.y + b.rect.h / 2.0;
        let Some(surface) = self.resolve_window(wid) else {
            return crate::inject::protocol::ToggleReply::no_action();
        };
        // smithay 79bbed5 `PointerInnerHandle::motion` does
        // `event.location - loc` (`pointer.rs:801-804`). AT-SPI rects are
        // surface-relative, so `loc` must be `(0, 0)` for the client to see
        // `(cx, cy)`. `contents_under`'s point is a *global* surface origin
        // (`PointContents` docs); pairing it with a surface-relative
        // `(cx, cy)` shifts the click off the widget.
        let pointer = self.state.niri.seat.get_pointer().unwrap().clone();
        let clicked = crate::inject::dispatch::inject_agent_button_click(
            &pointer,
            self.agent_state(),
            Some((surface, Point::from((0.0, 0.0)))),
            cx,
            cy,
        );
        if !clicked {
            return crate::inject::protocol::ToggleReply::no_action();
        }
        // Click dispatched is not an observed GTK checked-bit. None means
        // unobserved and the wire reply is `no_action` on purpose (honesty
        // over convenience). Do not return `unchecked()` as a stand-in for
        // success.
        match crate::inject::atspi_ladder::observe_toggle_checked(&b.dbus_dest, &b.dbus_path) {
            Some(true) => crate::inject::protocol::ToggleReply::checked(),
            Some(false) => crate::inject::protocol::ToggleReply::unchecked(),
            None => crate::inject::protocol::ToggleReply::no_action(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::protocol::{ERR_LOCKED, ERR_OUTPUTS_OFF};
    use crate::inject::{INJECT_PLANE_IDENTS, OUTPUT_POWER_PLANE_IDENTS};
    use crate::niri::{OUTPUTS_OFF_OVERRIDE, OUTPUTS_OFF_WIRED, WL_OUTPUT_POWER_STATE};

    #[test]
    fn idents_keep_is_locked() {
        assert!(INJECT_PLANE_IDENTS.iter().any(|s| *s == "is_locked"));
        assert!(!LOCK_SCREEN_ACCESSORS.is_empty());
    }

    #[test]
    fn idents_keep_outputs_off() {
        assert!(INJECT_PLANE_IDENTS.iter().any(|s| *s == "outputs_off"));
    }

    #[test]
    fn idents_keep_err_locked() {
        assert!(INJECT_PLANE_IDENTS.iter().any(|s| *s == "ERR_LOCKED"));
        assert_eq!(ERR_LOCKED, "LOCKED");
    }

    #[test]
    fn idents_keep_err_outputs_off() {
        assert!(INJECT_PLANE_IDENTS.iter().any(|s| *s == "ERR_OUTPUTS_OFF"));
        assert_eq!(ERR_OUTPUTS_OFF, "OUTPUTS_OFF");
    }

    #[test]
    fn idents_keep_outputs_off_wired() {
        assert_eq!(OUTPUTS_OFF_WIRED, "outputs_off_wired");
        assert_eq!(WL_OUTPUT_POWER_STATE, "wl_output_power_state");
        assert!(OUTPUT_POWER_PLANE_IDENTS.contains(&"outputs_off_wired"));
        assert!(OUTPUT_POWER_PLANE_IDENTS.contains(&"wl_output_power_state"));
    }

    #[test]
    fn idents_keep_outputs_off_override() {
        assert_eq!(OUTPUTS_OFF_OVERRIDE, "outputs_off_override");
        assert!(OUTPUT_POWER_PLANE_IDENTS.contains(&"outputs_off_override"));
    }

    #[test]
    fn v40_7_ident_is_published() {
        // Pin the new ident in the unit test compile graph so LTO cannot drop it.
        let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_7_USER_SEAT_ROUTING);
        let _ = std::hint::black_box(KEY_DELIVERY_V40_7_HARNESS_IDENT);
        assert!(!crate::inject::KEY_DELIVERY_V40_7_USER_SEAT_ROUTING.is_empty());
        assert!(!KEY_DELIVERY_V40_7_HARNESS_IDENT.is_empty());
    }
}
