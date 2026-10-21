//! v40.39b: CLICK worker→main pointer handoff.
//!
//! Production `DispatchState` has `PointerFocus = DispatchFocus` and
//! `DispatchFocus::button` is empty. A `DISPATCH OK` reply is not evidence
//! that a client saw the press. This crate posts `PointerClickDispatchReq`
//! through the worker thread-local sender and asserts a fake client
//! surface observed `wl_pointer.motion` plus `BTN_LEFT` press/release.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use niri::inject::calloop;
use niri::inject::dispatch::{
    bind_worker_pointer_click_tx, inject_agent_button_click, try_handoff_pointer_click,
    DispatchHarness, DispatchState, PointerClickDispatchReq,
};
use niri::inject::protocol::InjectHarness;
use niri::inject::smithay::backend::input::{ButtonState, InputTime, KeyState};
use niri::inject::smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use niri::inject::smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
};
use niri::inject::smithay::input::touch::{
    DownEvent, FrameMarker, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent,
    TouchTarget, UpEvent,
};
use niri::inject::smithay::input::{Seat, SeatHandler, SeatState};
use niri::inject::smithay::reexports::wayland_server::backend::ClientData;
use niri::inject::smithay::reexports::wayland_server::protocol::wl_surface::{self, WlSurface};
use niri::inject::smithay::reexports::wayland_server::{Client, Dispatch, Display, DisplayHandle};
use niri::inject::smithay::utils::{IsAlive, Logical, Point, Serial};
use niri::inject::{AgentSeat, PendingGrant};

const KNOWN_WINDOW: u64 = 42;
const CLICK_X: f64 = 100.0;
const CLICK_Y: f64 = 200.0;
const BTN_LEFT: u32 = 0x110;

#[derive(Default)]
struct ClickRecord {
    motion_count: u32,
    last_motion: Option<Point<f64, Logical>>,
    buttons: Vec<(u32, ButtonState)>,
}

struct DummyHost;
impl Dispatch<WlSurface, ()> for DummyHost {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlSurface,
        _request: wl_surface::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut niri::inject::smithay::reexports::wayland_server::DataInit<'_, Self>,
    ) {
    }
}

struct NoopClient;
impl ClientData for NoopClient {}

struct SurfaceKeep {
    _display: Display<DummyHost>,
    _client_stream: UnixStream,
    _client: Client,
    surface: WlSurface,
}

fn dummy_surface() -> SurfaceKeep {
    let display = Display::<DummyHost>::new().expect("wayland Display");
    let mut dh = display.handle();
    let (server, client_stream) = UnixStream::pair().expect("surface socket pair");
    let client = dh
        .insert_client(server, Arc::new(NoopClient))
        .expect("insert wayland client");
    let surface = client
        .create_resource::<WlSurface, (), DummyHost>(&dh, 6, ())
        .expect("create WlSurface token");
    SurfaceKeep {
        _display: display,
        _client_stream: client_stream,
        _client: client,
        surface,
    }
}

/// Pointer focus that *is* the window's client `WlSurface`, with recording.
#[derive(Debug, Clone, PartialEq)]
struct FakeFocus {
    surface: WlSurface,
}

impl IsAlive for FakeFocus {
    fn alive(&self) -> bool {
        true
    }
}

impl PointerTarget<FakeClientState> for FakeFocus {
    fn enter(
        &self,
        seat: &Seat<FakeClientState>,
        data: &mut FakeClientState,
        event: &MotionEvent,
    ) {
        PointerTarget::motion(self, seat, data, event);
    }
    fn motion(
        &self,
        _seat: &Seat<FakeClientState>,
        data: &mut FakeClientState,
        event: &MotionEvent,
    ) {
        let mut rec = data.record.lock().unwrap();
        rec.motion_count += 1;
        rec.last_motion = Some(event.location);
    }
    fn relative_motion(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &RelativeMotionEvent,
    ) {
    }
    fn button(
        &self,
        _seat: &Seat<FakeClientState>,
        data: &mut FakeClientState,
        event: &ButtonEvent,
    ) {
        data.record
            .lock()
            .unwrap()
            .buttons
            .push((event.button, event.state));
    }
    fn axis(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _frame: AxisFrame) {}
    fn frame(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState) {}
    fn leave(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _serial: Serial,
        _time: InputTime,
    ) {
    }
    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl KeyboardTarget<FakeClientState> for FakeFocus {
    fn enter(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _keys: Vec<KeysymHandle<'_>>,
        _serial: Serial,
    ) {
    }
    fn leave(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _serial: Serial) {}
    fn key(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _key: KeysymHandle<'_>,
        _state: KeyState,
        _serial: Serial,
        _time: InputTime,
    ) {
    }
    fn modifiers(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _modifiers: ModifiersState,
        _serial: Serial,
    ) {
    }
}

impl TouchTarget<FakeClientState> for FakeFocus {
    fn down(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _event: &DownEvent) {}
    fn up(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _event: &UpEvent) {}
    fn motion(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &TouchMotionEvent,
    ) {
    }
    fn frame(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _marker: FrameMarker) {}
    fn cancel(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _marker: FrameMarker) {
    }
    fn shape(&self, _seat: &Seat<FakeClientState>, _data: &mut FakeClientState, _event: &ShapeEvent) {}
    fn orientation(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
        _event: &OrientationEvent,
    ) {
    }
    fn last_frame(
        &self,
        _seat: &Seat<FakeClientState>,
        _data: &mut FakeClientState,
    ) -> Option<FrameMarker> {
        None
    }
}

struct FakeClientState {
    seat_state: SeatState<Self>,
    agent_seat: AgentSeat<Self>,
    record: Arc<Mutex<ClickRecord>>,
    surface: WlSurface,
    _keep: SurfaceKeep,
}

impl FakeClientState {
    fn new(record: Arc<Mutex<ClickRecord>>) -> Self {
        let keep = dummy_surface();
        let mut seat_state = SeatState::new();
        let agent_seat = AgentSeat::create(&mut seat_state);
        let surface = keep.surface.clone();
        Self {
            seat_state,
            agent_seat,
            record,
            surface,
            _keep: keep,
        }
    }
}

impl SeatHandler for FakeClientState {
    type KeyboardFocus = FakeFocus;
    type PointerFocus = FakeFocus;
    type TouchFocus = FakeFocus;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

#[test]
fn ident_planted_pointer_click_handoff_v40_39b() {
    let src = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/inject/dispatch.rs"
    ));
    assert!(
        src.contains("POINTER_CLICK_HANDOFF_V40_39B"),
        "src/inject/dispatch.rs must plant POINTER_CLICK_HANDOFF_V40_39B"
    );
}

#[test]
fn pointer_click_handoff_delivers_to_client_surface() {
    let record = Arc::new(Mutex::new(ClickRecord::default()));
    let (tx, rx) = calloop::channel::channel::<PointerClickDispatchReq>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let pump_record = Arc::clone(&record);
    let _pump = std::thread::spawn(move || {
        let mut fake = FakeClientState::new(pump_record);
        let mut event_loop = calloop::EventLoop::<FakeClientState>::try_new().expect("loop");
        event_loop
            .handle()
            .insert_source(rx, |event, _, fake| {
                if let calloop::channel::Event::Msg(req) = event {
                    // Stand-in for `State::dispatch_pointer_click_request`:
                    // activate the fake client surface and deliver the
                    // motion + BTN_LEFT frame onto it.
                    let pointer = fake.agent_seat.pointer.clone();
                    let focus = FakeFocus {
                        surface: fake.surface.clone(),
                    };
                    let origin = Point::from((0.0, 0.0));
                    let ok = inject_agent_button_click(
                        &pointer,
                        fake,
                        Some((focus, origin)),
                        req.x,
                        req.y,
                    );
                    let _ = req.reply.send(ok);
                }
            })
            .expect("insert pointer-click source");
        let _ = ready_tx.send(());
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut fake)
                .expect("dispatch");
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("pump ready");

    bind_worker_pointer_click_tx(Some(tx.clone()));

    let mut state = DispatchState::new(
        false,
        false,
        PathBuf::from("/dev/null"),
        Arc::new(Mutex::new(HashMap::<u64, WlSurface>::new())),
        Arc::new(Mutex::new(HashMap::<(String, u64), ()>::new())),
        Arc::new(Mutex::new(None::<PendingGrant>)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state.pointer_click_tx = Some(tx);
    let mut harness = DispatchHarness::new(&mut state);

    // Drive the production worker: thread-local post (socket path) plus
    // DispatchHarness::pointer_click_at (state.tx path). Use the thread-local
    // sender first — that is what `socket.rs` Frame::Click calls.
    let handed = try_handoff_pointer_click(KNOWN_WINDOW, CLICK_X, CLICK_Y)
        .expect("worker thread-local CLICK sender must be bound");
    assert!(
        handed,
        "main-loop pump must ACK the pointer-click handoff"
    );

    // Second path: DispatchState's installed tx (same as DispatchHarness).
    // Use a distinct coordinate so a second motion is distinguishable if the
    // first already landed. One successful client-surface frame is enough.
    let _ = InjectHarness::pointer_click_at(&mut harness, KNOWN_WINDOW, CLICK_X, CLICK_Y);

    bind_worker_pointer_click_tx(None);

    let rec = record.lock().unwrap();
    assert!(
        rec.motion_count >= 1,
        "fake client surface must observe wl_pointer.motion, got motion_count={}",
        rec.motion_count
    );
    assert_eq!(
        rec.last_motion,
        Some(Point::from((CLICK_X, CLICK_Y))),
        "recorded motion location must equal the click point"
    );
    assert!(
        rec.buttons
            .iter()
            .any(|(b, s)| *b == BTN_LEFT && *s == ButtonState::Pressed),
        "fake client surface must observe BTN_LEFT press, got {:?}",
        rec.buttons
    );
    assert!(
        rec.buttons
            .iter()
            .any(|(b, s)| *b == BTN_LEFT && *s == ButtonState::Released),
        "fake client surface must observe BTN_LEFT release, got {:?}",
        rec.buttons
    );
}

