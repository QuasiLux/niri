//! v40.43: AT_TOGGLE_REQ worker→main `at_toggle_tx` production path.
//!
//! Production `DispatchState` has both `pointer_click_tx` (Frame::Click,
//! global coords) and `at_toggle_tx` (Frame::AtToggleReq, window-relative
//! AT-SPI bounds). v40.40 routed the toggle arm through
//! `try_handoff_pointer_click` first, so the main-loop
//! `click_atspi_center_on_user_seat` translate never ran and the click
//! landed at the unmoved window origin (0, 0) + (cx, cy) interpreted as
//! global. This crate drives `InjectHarness::at_toggle_req` against a
//! fully-built `DispatchState` with both senders, and the `at_toggle_tx`
//! pump translates `WINDOW_ORIGIN + (cx, cy)` the way
//! `click_atspi_center_on_user_seat` does.
//!
//! Durable assertions: `last_motion == WINDOW_ORIGIN + (cx, cy)` and
//! `rec.surface_origin == WINDOW_ORIGIN`. Those fail on unmodified
//! `e41efe20` because the v40.40 CLICK handoff steals the request and
//! delivers `(cx, cy)` as global with surface origin `(0, 0)`.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use niri::inject::atspi_ladder::{
    set_observe_toggle_override, set_scan_override, ElementBounds,
};
use niri::inject::calloop;
use niri::inject::dispatch::{
    bind_worker_pointer_click_tx, inject_agent_button_click, AtToggleDispatchReq,
    DispatchHarness, DispatchState, PointerClickDispatchReq,
};
use niri::inject::protocol::{InjectHarness, ToggleReply};
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
use niri::inject::{AgentSeat, PendingGrant, Rect};

const KNOWN_WINDOW: u64 = 42;
const TOGGLE_NAME: &str = "Mute";
const RECT_X: f64 = 80.0;
const RECT_Y: f64 = 150.0;
const RECT_W: f64 = 40.0;
const RECT_H: f64 = 20.0;
/// Non-zero window origin so `(0, 0) + (cx, cy)` is a different point.
const WINDOW_ORIGIN: (f64, f64) = (400.0, 250.0);
const BTN_LEFT: u32 = 0x110;

fn element_center() -> (f64, f64) {
    (RECT_X + RECT_W / 2.0, RECT_Y + RECT_H / 2.0)
}

fn window_origin() -> Point<f64, Logical> {
    Point::from(WINDOW_ORIGIN)
}

fn expected_global() -> Point<f64, Logical> {
    window_origin() + Point::from(element_center())
}

fn fixture_bounds(wid: u64, role: &str, name: &str, _index: u32) -> Option<ElementBounds> {
    if wid != KNOWN_WINDOW {
        return None;
    }
    if role != "toggle button" {
        return None;
    }
    if name != TOGGLE_NAME {
        return None;
    }
    Some(ElementBounds {
        path: "toggle".into(),
        rect: Rect {
            x: RECT_X,
            y: RECT_Y,
            w: RECT_W,
            h: RECT_H,
        },
        n_actions: 1,
        action_name: "toggle".into(),
        dbus_dest: ":1.0".into(),
        dbus_path: "/org/a11y/atspi/accessible/1".into(),
    })
}

fn observe_checked(_dest: &str, _path: &str) -> Option<bool> {
    Some(true)
}

#[derive(Default)]
struct ClickRecord {
    motion_count: u32,
    last_motion: Option<Point<f64, Logical>>,
    buttons: Vec<(u32, ButtonState)>,
    surface_origin: Option<Point<f64, Logical>>,
    pointer_click_tx_calls: u32,
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
fn ident_planted_at_toggle_req_reaches_real_client_surface_v40_43() {
    let src = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/inject/dispatch.rs"
    ));
    assert!(
        src.contains("AT_TOGGLE_REQ_REACHES_REAL_CLIENT_SURFACE_V40_43"),
        "src/inject/dispatch.rs must plant AT_TOGGLE_REQ_REACHES_REAL_CLIENT_SURFACE_V40_43"
    );
}

#[test]
fn at_toggle_req_reaches_real_client_surface_via_at_toggle_tx() {
    set_scan_override(Some(fixture_bounds));
    set_observe_toggle_override(Some(observe_checked));

    let (cx, cy) = element_center();
    let untranslated = Point::from((cx, cy));
    let expected = expected_global();
    let origin = window_origin();
    assert_ne!(
        expected, untranslated,
        "window origin must be non-zero so last_motion is a translate, not (0,0)+(cx,cy)"
    );

    let record = Arc::new(Mutex::new(ClickRecord::default()));
    let (toggle_tx, toggle_rx) = calloop::channel::channel::<AtToggleDispatchReq>();
    let (click_tx, click_rx) = calloop::channel::channel::<PointerClickDispatchReq>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let pump_record = Arc::clone(&record);
    let _pump = std::thread::spawn(move || {
        let mut fake = FakeClientState::new(pump_record);
        let mut event_loop = calloop::EventLoop::<FakeClientState>::try_new().expect("loop");
        event_loop
            .handle()
            .insert_source(toggle_rx, |event, _, fake| {
                if let calloop::channel::Event::Msg(req) = event {
                    // Stand-in for `State::dispatch_at_toggle_request` →
                    // `NiriInjectHarness::at_toggle_req` →
                    // `click_atspi_center_on_user_seat`: window-relative
                    // AT-SPI center plus the window's global top-left.
                    let Some(b) = fixture_bounds(req.wid, "toggle button", &req.name, 0) else {
                        let _ = req.reply.send(ToggleReply::no_action());
                        return;
                    };
                    let cx = b.rect.x + b.rect.w / 2.0;
                    let cy = b.rect.y + b.rect.h / 2.0;
                    let origin = window_origin();
                    let global = origin + Point::from((cx, cy));
                    fake.record.lock().unwrap().surface_origin = Some(origin);
                    let pointer = fake.agent_seat.pointer.clone();
                    let focus = FakeFocus {
                        surface: fake.surface.clone(),
                    };
                    // smithay PointerTarget::motion subtracts this origin
                    // from MotionEvent.location. Pass (0,0) so last_motion
                    // stays the global point we delivered (WINDOW_ORIGIN+center).
                    let inject_origin = Point::from((0.0, 0.0));
                    let _ok = inject_agent_button_click(
                        &pointer,
                        fake,
                        Some((focus, inject_origin)),
                        global.x,
                        global.y,
                    );
                    let _ = req.reply.send(ToggleReply::unchecked());
                }
            })
            .expect("insert at-toggle source");
        event_loop
            .handle()
            .insert_source(click_rx, |event, _, fake| {
                if let calloop::channel::Event::Msg(req) = event {
                    // Wired so a v40.40 steal is observable. Must not run
                    // on the v40.43 `at_toggle_tx` path. Delivers
                    // window-relative `(cx, cy)` as if it were global.
                    {
                        let mut rec = fake.record.lock().unwrap();
                        rec.pointer_click_tx_calls += 1;
                        rec.surface_origin = Some(Point::from((0.0, 0.0)));
                    }
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

    bind_worker_pointer_click_tx(Some(click_tx.clone()));

    let mut state = DispatchState::new(
        false,
        false,
        PathBuf::from("/dev/null"),
        Arc::new(Mutex::new(HashMap::<u64, WlSurface>::new())),
        Arc::new(Mutex::new(HashMap::<(String, u64), ()>::new())),
        Arc::new(Mutex::new(None::<PendingGrant>)),
    );
    state.pointer_click_tx = Some(click_tx);
    state.at_toggle_tx = Some(toggle_tx);
    let mut harness = DispatchHarness::new(&mut state);

    let _reply = InjectHarness::at_toggle_req(&mut harness, KNOWN_WINDOW, TOGGLE_NAME);

    bind_worker_pointer_click_tx(None);
    set_scan_override(None);
    set_observe_toggle_override(None);

    let rec = record.lock().unwrap();
    assert_eq!(
        rec.pointer_click_tx_calls, 0,
        "v40.43 must post through at_toggle_tx, not the v40.40 pointer_click_tx fallback, got {} calls",
        rec.pointer_click_tx_calls
    );
    assert!(
        rec.motion_count >= 1,
        "AT_TOGGLE_REQ must deliver wl_pointer.motion to the client surface, got motion_count={}",
        rec.motion_count
    );
    assert!(
        rec.buttons.contains(&(BTN_LEFT, ButtonState::Pressed)),
        "AT_TOGGLE_REQ must deliver BTN_LEFT press to the client surface, got {:?}",
        rec.buttons
    );
    assert!(
        rec.buttons.contains(&(BTN_LEFT, ButtonState::Released)),
        "AT_TOGGLE_REQ must deliver BTN_LEFT release to the client surface, got {:?}",
        rec.buttons
    );
    assert_eq!(
        rec.surface_origin,
        Some(origin),
        "toggle arm must deliver with surface_origin == WINDOW_ORIGIN {:?}, not (0,0)+(cx,cy); got {:?}",
        origin,
        rec.surface_origin
    );
    assert_ne!(
        rec.last_motion,
        Some(untranslated),
        "last_motion must not be the unmoved window:(0,0)+(cx,cy) = {:?}",
        untranslated
    );
    assert_eq!(
        rec.last_motion,
        Some(expected),
        "last_motion must be WINDOW_ORIGIN + (cx, cy) = {:?}, got {:?}",
        expected,
        rec.last_motion
    );
}
