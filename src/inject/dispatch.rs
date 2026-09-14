//! Per-connection inject-dispatch proxy and worker-thread entry point.
//!
//! v4.7. The compositor event loop spawns one OS thread per accepted stream
//! and feeds it `handle_inject_client_with_close` via a private calloop.
//! The harness on the worker thread wraps this small proxy struct — it has
//! no access to `niri::State`. Lock-screen, outputs-off, and window-id →
//! [`WlSurface`] snapshots are captured at the moment of accept (the spec
//! calls for "idempotency check on is_locked()"); subsequent events are
//! still gated on the cached flag. `resolve_window` reads the snapshot map
//! rather than calling into production state from the worker thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use calloop::EventLoop;
use smithay::backend::input::KeyState;
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
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::{self, WlSurface};
use smithay::reexports::wayland_server::{Client, Dispatch, Display, DisplayHandle};
use smithay::utils::{IsAlive, Logical, Point, Serial};

use crate::input::agent_seat::AgentSeat;
use crate::inject::protocol::InjectHarness;
use crate::inject::socket::handle_inject_client_with_close;

/// Niri-shaped fields the v4.6 harness reads via `self.state.niri`.
/// On the worker thread we don't have `niri::State`; we hold only the
/// snapshot the dispatch loop needs.
pub struct DispatchState {
    pub is_locked: bool,
    pub outputs_off: bool,
    pub surface_map: Arc<Mutex<HashMap<u64, WlSurface>>>,
    pub motion_count: Rc<RefCell<u32>>,
    pub agent_seat: AgentSeat<DispatchState>,
    pub seat_state: SeatState<DispatchState>,
    pub token_path: PathBuf,
    pub bound_surface: Option<WlSurface>,
    pub surface_target: Arc<Mutex<DispatchTarget>>,
    pub _display: Display<DispatchHost>,
    _client_stream: UnixStream,
    _client: Client,
}

#[derive(Default)]
pub struct DispatchTarget {
    pub motion_count: u32,
    pub key_count: u32,
    pub axis_count: u32,
    pub last_motion: Option<Point<f64, Logical>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchFocus;

pub struct DispatchHost;

impl Dispatch<WlSurface, ()> for DispatchHost {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlSurface,
        _request: wl_surface::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut smithay::reexports::wayland_server::DataInit<'_, Self>,
    ) {
    }
}

pub struct NoopClientData;
impl ClientData for NoopClientData {}

impl DispatchState {
    pub fn new(
        is_locked: bool,
        outputs_off: bool,
        token_path: PathBuf,
        surface_map: Arc<Mutex<HashMap<u64, WlSurface>>>,
    ) -> Self {
        let display = Display::<DispatchHost>::new().expect("display");
        let mut dh = display.handle();
        let (server, client_stream) = UnixStream::pair().expect("surface socket pair");
        let client = dh
            .insert_client(server, Arc::new(NoopClientData))
            .expect("client");
        let surface = client
            .create_resource::<WlSurface, (), DispatchHost>(&dh, 6, ())
            .expect("surface");
        let mut seat_state = SeatState::new();
        let agent_seat = AgentSeat::create(&mut seat_state);
        Self {
            is_locked,
            outputs_off,
            surface_map,
            motion_count: Rc::new(RefCell::new(0)),
            agent_seat,
            seat_state,
            token_path,
            bound_surface: Some(surface),
            surface_target: Arc::new(Mutex::new(DispatchTarget::default())),
            _display: display,
            _client_stream: client_stream,
            _client: client,
        }
    }

    pub fn snapshot_motion_count(&self) -> u32 {
        *self.motion_count.borrow()
    }
}

impl IsAlive for DispatchFocus {
    fn alive(&self) -> bool {
        true
    }
}

impl PointerTarget<DispatchState> for DispatchFocus {
    fn enter(&self, seat: &Seat<DispatchState>, data: &mut DispatchState, event: &MotionEvent) {
        PointerTarget::motion(self, seat, data, event);
    }
    fn motion(&self, _seat: &Seat<DispatchState>, data: &mut DispatchState, event: &MotionEvent) {
        let mut rec = data.surface_target.lock().unwrap();
        rec.motion_count += 1;
        rec.last_motion = Some(event.location);
        *data.motion_count.borrow_mut() = rec.motion_count;
    }
    fn relative_motion(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &RelativeMotionEvent,
    ) {
    }
    fn button(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _event: &ButtonEvent) {}
    fn axis(&self, _seat: &Seat<DispatchState>, data: &mut DispatchState, _frame: AxisFrame) {
        data.surface_target.lock().unwrap().axis_count += 1;
    }
    fn frame(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState) {}
    fn leave(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _serial: Serial, _time: u32) {
    }
    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl KeyboardTarget<DispatchState> for DispatchFocus {
    fn enter(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _keys: Vec<KeysymHandle<'_>>,
        _serial: Serial,
    ) {
    }
    fn leave(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _serial: Serial) {}
    fn key(
        &self,
        _seat: &Seat<DispatchState>,
        data: &mut DispatchState,
        _key: KeysymHandle<'_>,
        _state: KeyState,
        _serial: Serial,
        _time: u32,
    ) {
        data.surface_target.lock().unwrap().key_count += 1;
    }
    fn modifiers(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _modifiers: ModifiersState,
        _serial: Serial,
    ) {
    }
}

impl TouchTarget<DispatchState> for DispatchFocus {
    fn down(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _event: &DownEvent) {}
    fn up(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _event: &UpEvent) {}
    fn motion(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &TouchMotionEvent,
    ) {
    }
    fn frame(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _marker: FrameMarker) {}
    fn cancel(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _marker: FrameMarker) {}
    fn shape(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _event: &ShapeEvent) {}
    fn orientation(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _event: &OrientationEvent,
    ) {
    }
    fn last_frame(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
    ) -> Option<FrameMarker> {
        None
    }
}

impl SeatHandler for DispatchState {
    type KeyboardFocus = DispatchFocus;
    type PointerFocus = DispatchFocus;
    type TouchFocus = DispatchFocus;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

pub struct DispatchHarness<'a> {
    pub state: &'a mut DispatchState,
}

impl<'a> DispatchHarness<'a> {
    pub fn new(state: &'a mut DispatchState) -> Self {
        Self { state }
    }
}

impl<'a> InjectHarness for DispatchHarness<'a> {
    type AgentState = DispatchState;

    fn agent_seat(&mut self) -> &mut AgentSeat<Self::AgentState> {
        &mut self.state.agent_seat
    }

    fn agent_state(&mut self) -> &mut Self::AgentState {
        self.state
    }

    fn resolve_window(&mut self, window_id: u64) -> Option<WlSurface> {
        let map = self.state.surface_map.lock().unwrap();
        map.get(&window_id).cloned()
    }

    fn resolve_pointer_focus(&mut self, _window_id: u64) -> Option<DispatchFocus> {
        Some(DispatchFocus)
    }

    fn token_file_path(&self) -> &std::path::Path {
        &self.state.token_path
    }

    fn is_locked(&self) -> bool {
        self.state.is_locked
    }
    fn outputs_off(&self) -> bool {
        self.state.outputs_off
    }
    fn is_occluded(&mut self) -> bool {
        false
    }
}

/// Block on a future on this thread's event loop (same pattern as
/// `src/inject_e2e.rs::block_on_loop`). Public so tests can reuse it.
pub fn block_on_loop<F: Future>(event_loop: &mut EventLoop<'static, ()>, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    let start = std::time::Instant::now();
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        event_loop
            .dispatch(Some(Duration::from_millis(20)), &mut ())
            .expect("dispatch inject event loop");
        if start.elapsed() > Duration::from_secs(8) {
            panic!("inject protocol future timed out");
        }
    }
}

/// Worker-thread entry point. Drives one accepted stream on a private
/// event loop against a [`DispatchHarness`]. Closes cleanly when
/// `close_rx` fires (token rotation).
pub fn dispatch_inject_connection(
    stream: UnixStream,
    is_locked: bool,
    outputs_off: bool,
    token_path: PathBuf,
    close_rx: async_channel::Receiver<()>,
    surface_map: Arc<Mutex<HashMap<u64, WlSurface>>>,
) -> anyhow::Result<()> {
    let mut state = DispatchState::new(is_locked, outputs_off, token_path, surface_map);
    let mut harness = DispatchHarness::new(&mut state);
    let mut event_loop = EventLoop::<()>::try_new().expect("dispatch event loop");
    let async_stream = event_loop
        .handle()
        .adapt_io(stream)
        .expect("adapt UnixStream to calloop");
    block_on_loop(
        &mut event_loop,
        handle_inject_client_with_close(&mut harness, async_stream, close_rx),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_surface_map() -> Arc<Mutex<HashMap<u64, WlSurface>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn dispatch_state_caches_is_locked() {
        let s = DispatchState::new(true, false, PathBuf::from("/dev/null"), empty_surface_map());
        assert!(s.is_locked);
    }

    #[test]
    fn dispatch_state_caches_outputs_off() {
        let s = DispatchState::new(false, true, PathBuf::from("/dev/null"), empty_surface_map());
        assert!(s.outputs_off);
    }

    #[test]
    fn dispatch_harness_resolve_window_uses_surface_map() {
        let mut state = DispatchState::new(
            false,
            false,
            PathBuf::from("/dev/null"),
            empty_surface_map(),
        );
        let expected = state.bound_surface.clone().expect("dummy surface");
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(42, expected.clone());
        let mut harness = DispatchHarness::new(&mut state);
        let got = harness
            .resolve_window(42)
            .expect("mapped window_id must resolve");
        assert_eq!(got, expected);
    }

    #[test]
    fn dispatch_harness_resolve_window_unknown_returns_none() {
        let mut state = DispatchState::new(
            false,
            false,
            PathBuf::from("/dev/null"),
            empty_surface_map(),
        );
        let surface = state.bound_surface.clone().expect("dummy surface");
        state.surface_map.lock().unwrap().insert(42, surface);
        let mut harness = DispatchHarness::new(&mut state);
        assert!(harness.resolve_window(999_999).is_none());
        assert!(harness.resolve_window(0).is_none());
    }
}
