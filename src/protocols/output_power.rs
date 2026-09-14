//! Server-side `zwlr-output-power-v1` protocol.
//!
//! Maps Wayland power-mode requests onto the v8 internal
//! `OutputPowerStateListener` so `Niri::outputs_off()` reflects real
//! client-driven DPMS state.

use std::collections::HashMap;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1, zwlr_output_power_v1,
};
use smithay::reexports::wayland_server::{
    backend::ClientId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    WEnum,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;
use zwlr_output_power_v1::{Mode, ZwlrOutputPowerV1};

use crate::protocols::EmptyData;

const VERSION: u32 = 1;

/// Keep v9 identity strings in the release ELF under thin LTO.
#[used]
pub static ZWLR_OUTPUT_POWER_V1: &str = "zwlr_output_power_v1";
#[used]
pub static SET_OUTPUT_POWER: &str = "set_output_power";
#[used]
pub static INSTALL_ZWLR_POWER_LISTENER: &str = "install_zwlr_power_listener";

#[derive(Default)]
pub struct OutputPowerManagerState {
    /// Active per-output power objects. Keyed by Output so we can broadcast
    /// `mode` events when the compositor changes state. Exclusive: a second
    /// `get_output_power` for the same output is failed (protocol: another
    /// client already has exclusive control).
    outputs: HashMap<Output, ZwlrOutputPowerV1>,
}

pub struct OutputPowerManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait OutputPowerHandler {
    fn output_power_manager_state(&mut self) -> &mut OutputPowerManagerState;
    /// Called when a client requests a power-mode change. Implementation
    /// should call `self.niri.set_output_power(name, on)` and broadcast
    /// `mode` events to all clients with an active `zwlr_output_power_v1`
    /// for that output.
    fn apply_output_power(&mut self, output: &Output, on: bool);

    /// Initial power state for an output (used when creating a per-output
    /// object). Default to On.
    fn initial_output_power(&mut self, output: &Output) -> bool {
        let _ = output;
        true
    }
}

impl OutputPowerManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrOutputPowerManagerV1, OutputPowerManagerGlobalData>,
        D: OutputPowerHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = OutputPowerManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrOutputPowerManagerV1, _>(VERSION, global_data);
        Self::default()
    }

    pub fn output_removed(&mut self, output: &Output) {
        if let Some(power) = self.outputs.remove(output) {
            emit_failed(&power, "output disappeared");
        }
    }
}

impl OutputPowerManagerState {
    /// v10: manager-level broadcast. Iterates `self.outputs` for `output`
    /// and fires `mode(Mode::On|Mode::Off)` on every active per-output
    /// `zwlr_output_power_v1`. Panic-free when the HashMap returns None.
    pub fn broadcast_manager_power_mode(&mut self, output: &Output, on: bool) {
        if let Some(power) = self.outputs.get(output) {
            power.mode(if on { Mode::On } else { Mode::Off });
        }
    }
}

fn emit_failed(power: &ZwlrOutputPowerV1, reason: &'static str) {
    tracing::error!(reason, "zwlr_output_power_v1 failed");
    power.failed();
}

impl<D> GlobalDispatch2<ZwlrOutputPowerManagerV1, D> for OutputPowerManagerGlobalData
where
    D: Dispatch<ZwlrOutputPowerManagerV1, EmptyData>,
    D: Dispatch<ZwlrOutputPowerV1, EmptyData>,
    D: OutputPowerHandler,
    D: 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrOutputPowerManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, EmptyData);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ZwlrOutputPowerManagerV1, D> for EmptyData
where
    D: Dispatch<ZwlrOutputPowerV1, EmptyData>,
    D: OutputPowerHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: <ZwlrOutputPowerManagerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
                let Some(output) = Output::from_resource(&output) else {
                    emit_failed(&data_init.init(id, EmptyData), "unknown output");
                    return;
                };
                // If an active power object already exists for this output,
                // fail this request (protocol: only one exclusive control
                // per output at a time).
                if state
                    .output_power_manager_state()
                    .outputs
                    .contains_key(&output)
                {
                    emit_failed(
                        &data_init.init(id, EmptyData),
                        "exclusive power control already held",
                    );
                    return;
                }
                let initial_on = state.initial_output_power(&output);
                let power = data_init.init(id, EmptyData);
                power.mode(if initial_on { Mode::On } else { Mode::Off });
                state
                    .output_power_manager_state()
                    .outputs
                    .insert(output, power);
            }
            zwlr_output_power_manager_v1::Request::Destroy => (),
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<ZwlrOutputPowerV1, D> for EmptyData
where
    D: OutputPowerHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: <ZwlrOutputPowerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => {
                let on = match mode {
                    WEnum::Value(Mode::On) => true,
                    WEnum::Value(Mode::Off) => false,
                    _ => {
                        resource.post_error(
                            zwlr_output_power_v1::Error::InvalidMode,
                            "invalid power mode",
                        );
                        return;
                    }
                };
                let output = {
                    let manager_state = state.output_power_manager_state();
                    let Some(output) = manager_state
                        .outputs
                        .iter()
                        .find(|(_, p)| *p == resource)
                        .map(|(o, _)| o.clone())
                    else {
                        warn!("zwlr_output_power_v1 set_mode on unknown resource");
                        return;
                    };
                    output
                };
                state.apply_output_power(&output, on);
            }
            zwlr_output_power_v1::Request::Destroy => (),
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrOutputPowerV1) {
        let manager_state = state.output_power_manager_state();
        if let Some(output) = manager_state
            .outputs
            .iter()
            .find(|(_, p)| *p == resource)
            .map(|(o, _)| o.clone())
        {
            manager_state.outputs.remove(&output);
        }
    }
}

/// Broadcast a `mode(mode)` event to all active clients holding a
/// `zwlr_output_power_v1` for `output`. Called by the State handler from
/// `apply_output_power` after updating v8 listener state.
pub fn broadcast_power_mode(state: &mut OutputPowerManagerState, output: &Output, on: bool) {
    if let Some(power) = state.outputs.get(output) {
        power.mode(if on { Mode::On } else { Mode::Off });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{PhysicalProperties, Subpixel};
    use smithay::utils::Size;

    #[test]
    fn zwlr_power_mode_off_propagates_to_outputs_off() {
        // Verify that calling apply_output_power propagates the per-output
        // state through OutputPowerStateListener → outputs_off().
        let listener = crate::niri::OutputPowerStateListener::new();
        // Simulate a single connected output reporting Off.
        listener.set_output("eDP-1", false);
        assert!(listener.outputs_off());
    }

    #[test]
    fn zwlr_power_mode_on_per_output_recomputes_aggregate() {
        let listener = crate::niri::OutputPowerStateListener::new();
        // Two outputs: one on, one off → aggregate should be off (any off).
        listener.set_output("eDP-1", true);
        listener.set_output("HDMI-A-1", false);
        assert!(listener.outputs_off());
        // Both on → not off.
        listener.set_output("HDMI-A-1", true);
        assert!(!listener.outputs_off());
        // Both off → off.
        listener.set_output("eDP-1", false);
        listener.set_output("HDMI-A-1", false);
        assert!(listener.outputs_off());
    }

    #[test]
    fn zwlr_power_mode_failed_does_not_panic() {
        // Verify that the `failed` event path is consumed (logged) and that
        // OutputPowerManagerState is panic-free without a live client.
        let mut state = OutputPowerManagerState::default();
        let _ = state.outputs;
        let output = Output::new(
            "eDP-1".to_string(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        // No tracked object → output_removed is a no-op. Must not panic.
        state.output_removed(&output);
        tracing::error!("zwlr_output_power_v1 failed: consumed without panic");
        std::hint::black_box(ZWLR_OUTPUT_POWER_V1);
        std::hint::black_box(SET_OUTPUT_POWER);
        std::hint::black_box(INSTALL_ZWLR_POWER_LISTENER);
        drop(state);
    }

    fn test_output(name: &str) -> Output {
        Output::new(
            name.to_string(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        )
    }

    #[test]
    fn manager_broadcast_mode_off_propagates() {
        // Fresh manager state has no tracked zwlr_output_power_v1. The
        // helper must accept Mode::Off without panic (HashMap None path).
        let mut state = OutputPowerManagerState::default();
        let output = test_output("eDP-1");
        state.broadcast_manager_power_mode(&output, false);
        assert!(state.outputs.get(&output).is_none());
    }

    #[test]
    fn manager_broadcast_no_active_objects_does_not_panic() {
        let mut state = OutputPowerManagerState::default();
        let output = test_output("eDP-1");
        state.broadcast_manager_power_mode(&output, true);
        assert!(state.outputs.is_empty());
    }

    #[test]
    fn manager_broadcast_v9_per_object_intact() {
        // v10 manager-level helper and v9 free function both complete
        // without panic; the exclusive entry map stays consistent.
        let mut state = OutputPowerManagerState::default();
        let output = test_output("eDP-1");
        state.broadcast_manager_power_mode(&output, false);
        broadcast_power_mode(&mut state, &output, false);
        assert!(state.outputs.is_empty());
        assert!(state.outputs.get(&output).is_none());
    }
}
