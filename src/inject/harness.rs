//! Production-side [`InjectHarness`] for [`crate::niri::State`].
//!
//! `niri::State` owns [`Niri`], not the reverse, so this wrapper holds
//! `&mut State` and reads lock / output-power flags through it. Window
//! lookup maps the wire `u64` through [`MappedId`] onto the live layout.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::input::agent_seat::AgentSeat;
use crate::inject::protocol::InjectHarness;
use crate::niri::{Niri, State};
use crate::window::mapped::MappedId;

/// Production inject harness. Construct around compositor [`State`] once a
/// connection is accepted (receiver plumbing is a later card).
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
        let off = self.state.niri.outputs_off();
        trace!(off, "outputs_off");
        off
    }

    fn is_occluded(&mut self) -> bool {
        trace!("is_occluded");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::protocol::{ERR_LOCKED, ERR_OUTPUTS_OFF};
    use crate::inject::INJECT_PLANE_IDENTS;

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
}
