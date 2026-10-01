//! Production-side [`InjectHarness`] for [`crate::niri::State`].
//!
//! `niri::State` owns [`Niri`], not the reverse, so this wrapper holds
//! `&mut State` and reads lock / output-power flags through it. Window
//! lookup maps the wire `u64` through [`MappedId`] onto the live layout.

use std::time::Duration;

use smithay::backend::input::InputTime;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::inject::grant_confirm::{ClickOutcome, GrantResolution, GrantSurfaceSource, SeatKind};
use crate::inject::protocol::InjectHarness;
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

    fn key_press(&mut self, xkb_name: &str) -> bool {
        let keyboard = self.agent_seat().keyboard.clone();
        let time = self.now_ms();
        crate::inject::dispatch::inject_agent_key_press(
            &keyboard,
            self.agent_state(),
            xkb_name,
            InputTime::from_millis(time),
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
}
