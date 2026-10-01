//! QuasiLux extra Smithay seat (inject plane).
//!
//! This seat is created with [`SeatState::new_seat`] (no `wl_seat` global) and
//! holds a [`PointerHandle`] plus a [`KeyboardHandle`]. Construction at
//! compositor startup (`Niri::new`) is deferred to a follow-up card; this
//! module only provides the typed handle and constructor.

use smithay::input::keyboard::KeyboardHandle;
use smithay::input::pointer::PointerHandle;
use smithay::input::{Seat, SeatHandler, SeatState};

/// Seat name advertised via [`Seat::name`]. Lowercase; not a `wl_seat` global.
pub const AGENT_SEAT_NAME: &str = "agent_seat";

/// Extra seat used by the inject plane.
///
/// Fields exist so the Smithay handle types stay part of the public surface.
/// The production call site `seat_state.new_seat(AGENT_SEAT_NAME)` lands in a
/// later card once the host install exists.
#[allow(dead_code)]
pub struct AgentSeat<D: SeatHandler> {
    pub seat: Seat<D>,
    pub pointer: PointerHandle<D>,
    pub keyboard: KeyboardHandle<D>,
}

impl<D: SeatHandler + 'static> AgentSeat<D> {
    /// Build pointer + keyboard capabilities on a new extra seat.
    ///
    /// Not wired from `Niri::new` this turn.
    pub fn create(seat_state: &mut SeatState<D>) -> Self {
        let mut seat = seat_state.new_seat(AGENT_SEAT_NAME);
        let pointer = seat.add_pointer();
        let keyboard = seat
            .add_keyboard(Default::default(), 200, 25)
            .expect("agent seat: default xkb keymap");
        Self {
            seat,
            pointer,
            keyboard,
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::input::{InputTime, KeyState};
    use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
    use smithay::input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
        GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
    };
    use smithay::input::touch::{
        DownEvent, FrameMarker, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent,
        TouchTarget, UpEvent,
    };
    use smithay::input::{Seat, SeatHandler, SeatState};
    use smithay::utils::{IsAlive, Serial};

    use super::{AgentSeat, AGENT_SEAT_NAME};

    // Smithay `add_pointer` / `add_keyboard` are pure handle construction.
    // libinput is bound only in `src/input/mod.rs::process_libinput_event`.
    // This test must not open `/dev/input/*`.

    #[derive(Debug, Clone, PartialEq)]
    struct Target;

    impl IsAlive for Target {
        fn alive(&self) -> bool {
            true
        }
    }

    impl PointerTarget<MinimalState> for Target {
        fn enter(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _event: &MotionEvent) {}
        fn motion(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _event: &MotionEvent) {}
        fn relative_motion(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &RelativeMotionEvent,
        ) {
        }
        fn button(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _event: &ButtonEvent) {}
        fn axis(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _frame: AxisFrame) {}
        fn frame(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState) {}
        fn leave(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _serial: Serial,
            _time: InputTime,
        ) {
        }
        fn gesture_swipe_begin(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GestureSwipeBeginEvent,
        ) {
        }
        fn gesture_swipe_update(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GestureSwipeUpdateEvent,
        ) {
        }
        fn gesture_swipe_end(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GestureSwipeEndEvent,
        ) {
        }
        fn gesture_pinch_begin(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GesturePinchBeginEvent,
        ) {
        }
        fn gesture_pinch_update(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GesturePinchUpdateEvent,
        ) {
        }
        fn gesture_pinch_end(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GesturePinchEndEvent,
        ) {
        }
        fn gesture_hold_begin(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GestureHoldBeginEvent,
        ) {
        }
        fn gesture_hold_end(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &GestureHoldEndEvent,
        ) {
        }
    }

    impl KeyboardTarget<MinimalState> for Target {
        fn enter(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _keys: Vec<KeysymHandle<'_>>,
            _serial: Serial,
        ) {
        }
        fn leave(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _serial: Serial) {}
        fn key(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _key: KeysymHandle<'_>,
            _state: KeyState,
            _serial: Serial,
            _time: InputTime,
        ) {
        }
        fn modifiers(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _modifiers: ModifiersState,
            _serial: Serial,
        ) {
        }
    }

    impl TouchTarget<MinimalState> for Target {
        fn down(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _event: &DownEvent) {}
        fn up(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _event: &UpEvent) {}
        fn motion(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &TouchMotionEvent,
        ) {
        }
        fn frame(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _marker: FrameMarker) {}
        fn cancel(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _marker: FrameMarker) {}
        fn shape(&self, _seat: &Seat<MinimalState>, _data: &mut MinimalState, _event: &ShapeEvent) {}
        fn orientation(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
            _event: &OrientationEvent,
        ) {
        }
        fn last_frame(
            &self,
            _seat: &Seat<MinimalState>,
            _data: &mut MinimalState,
        ) -> Option<FrameMarker> {
            None
        }
    }

    struct MinimalState {
        seat_state: SeatState<Self>,
    }

    impl SeatHandler for MinimalState {
        type KeyboardFocus = Target;
        type PointerFocus = Target;
        type TouchFocus = Target;

        fn seat_state(&mut self) -> &mut SeatState<Self> {
            &mut self.seat_state
        }
    }

    #[test]
    fn extra_seat_has_pointer_and_keyboard_handles() {
        let mut state = MinimalState {
            seat_state: SeatState::new(),
        };
        let agent = AgentSeat::create(&mut state.seat_state);
        assert_eq!(agent.seat.name(), AGENT_SEAT_NAME);
        assert!(agent.seat.get_pointer().is_some());
        assert!(agent.seat.get_keyboard().is_some());
    }
}
