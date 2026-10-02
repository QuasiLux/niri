//! QuasiLux inject-plane e2e harness. Used by `tests/inject_e2e.rs` as the
//! forked child's main entry point. NOT a public API.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use calloop::EventLoop;
use smithay::backend::input::{InputTime, KeyState};
use smithay::input::keyboard::{xkb, KeyboardTarget, KeysymHandle, ModifiersState};
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
use smithay::reexports::wayland_server::{Client, Dispatch, Display, DisplayHandle, Resource};
use smithay::utils::{IsAlive, Logical, Point, Serial, SERIAL_COUNTER};

use crate::inject::{
    handle_inject_client_with_close, AgentSeat, InjectHarness, InjectServer, AGENT_SEAT_NAME,
};

const KNOWN_WINDOW: u64 = 42;
const SECOND_WINDOW: u64 = 43;
const USER_FOCUS: Option<u64> = Some(99);
const USER_SCROLL: (f64, f64) = (1.5, 2.5);

#[derive(Default)]
struct TargetRecord {
    motion_count: u32,
    last_motion: Option<Point<f64, Logical>>,
    motions: Vec<Point<f64, Logical>>,
    axis_count: u32,
    key_count: u32,
    bound_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Focus;

struct DummyHost;

impl Dispatch<WlSurface, ()> for DummyHost {
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

struct Harness {
    seat_state: SeatState<Self>,
    agent_seat: AgentSeat<Self>,
    user_focused_window: Option<u64>,
    user_scroll_offset: Point<f64, Logical>,
    bound_surface: Option<WlSurface>,
    surface_map: HashMap<u64, WlSurface>,
    resolved_window_id: Option<u64>,
    resolved_surface_id: Option<u32>,
    surface_target: Arc<Mutex<TargetRecord>>,
    token_path: PathBuf,
    known_window: u64,
    keys: Vec<String>,
    bound_keys: Vec<String>,
    bound_wid: Option<u64>,
    lock_override: RefCell<bool>,
    _surface_keep: SurfaceKeep,
}

impl Harness {
    fn new(token_path: PathBuf) -> Self {
        let keep = dummy_surface();
        let mut seat_state = SeatState::new();
        let agent_seat = AgentSeat::create(&mut seat_state);
        let mut surface_map = HashMap::new();
        surface_map.insert(KNOWN_WINDOW, keep.surface.clone());
        surface_map.insert(SECOND_WINDOW, keep.surface.clone());
        Harness {
            seat_state,
            agent_seat,
            user_focused_window: USER_FOCUS,
            user_scroll_offset: Point::from(USER_SCROLL),
            bound_surface: Some(keep.surface.clone()),
            surface_map,
            resolved_window_id: None,
            resolved_surface_id: None,
            surface_target: Arc::new(Mutex::new(TargetRecord::default())),
            token_path,
            known_window: KNOWN_WINDOW,
            keys: Vec::new(),
            bound_keys: Vec::new(),
            bound_wid: None,
            lock_override: RefCell::new(false),
            _surface_keep: keep,
        }
    }

    pub fn set_locked(&mut self, locked: bool) {
        *self.lock_override.borrow_mut() = locked;
    }
}

impl IsAlive for Focus {
    fn alive(&self) -> bool {
        true
    }
}

impl PointerTarget<Harness> for Focus {
    fn enter(&self, seat: &Seat<Harness>, data: &mut Harness, event: &MotionEvent) {
        PointerTarget::motion(self, seat, data, event);
    }
    fn motion(&self, _seat: &Seat<Harness>, data: &mut Harness, event: &MotionEvent) {
        let mut rec = data.surface_target.lock().unwrap();
        rec.motion_count += 1;
        rec.last_motion = Some(event.location);
        rec.motions.push(event.location);
    }
    fn relative_motion(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &RelativeMotionEvent,
    ) {
    }
    fn button(&self, _seat: &Seat<Harness>, _data: &mut Harness, _event: &ButtonEvent) {}
    fn axis(&self, _seat: &Seat<Harness>, data: &mut Harness, _frame: AxisFrame) {
        data.surface_target.lock().unwrap().axis_count += 1;
    }
    fn frame(&self, _seat: &Seat<Harness>, _data: &mut Harness) {}
    fn leave(&self, _seat: &Seat<Harness>, _data: &mut Harness, _serial: Serial, _time: InputTime) {}
    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl KeyboardTarget<Harness> for Focus {
    fn enter(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _keys: Vec<KeysymHandle<'_>>,
        _serial: Serial,
    ) {
    }
    fn leave(&self, _seat: &Seat<Harness>, _data: &mut Harness, _serial: Serial) {}
    fn key(
        &self,
        _seat: &Seat<Harness>,
        data: &mut Harness,
        key: KeysymHandle<'_>,
        state: KeyState,
        _serial: Serial,
        _time: InputTime,
    ) {
        if !matches!(state, KeyState::Pressed) {
            return;
        }
        let name = xkb::keysym_get_name(key.modified_sym());
        data.bound_keys.push(name.clone());
        let mut rec = data.surface_target.lock().unwrap();
        rec.key_count += 1;
        rec.bound_keys.push(name);
    }
    fn modifiers(
        &self,
        _seat: &Seat<Harness>,
        _data: &mut Harness,
        _modifiers: ModifiersState,
        _serial: Serial,
    ) {
    }
}

impl TouchTarget<Harness> for Focus {
    fn down(&self, _seat: &Seat<Harness>, _data: &mut Harness, _event: &DownEvent) {}
    fn up(&self, _seat: &Seat<Harness>, _data: &mut Harness, _event: &UpEvent) {}
    fn motion(&self, _seat: &Seat<Harness>, _data: &mut Harness, _event: &TouchMotionEvent) {}
    fn frame(&self, _seat: &Seat<Harness>, _data: &mut Harness, _marker: FrameMarker) {}
    fn cancel(&self, _seat: &Seat<Harness>, _data: &mut Harness, _marker: FrameMarker) {}
    fn shape(&self, _seat: &Seat<Harness>, _data: &mut Harness, _event: &ShapeEvent) {}
    fn orientation(&self, _seat: &Seat<Harness>, _data: &mut Harness, _event: &OrientationEvent) {}
    fn last_frame(&self, _seat: &Seat<Harness>, _data: &mut Harness) -> Option<FrameMarker> {
        None
    }
}

impl SeatHandler for Harness {
    type KeyboardFocus = Focus;
    type PointerFocus = Focus;
    type TouchFocus = Focus;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl InjectHarness for Harness {
    type AgentState = Self;

    fn agent_seat(&mut self) -> &mut AgentSeat<Self> {
        &mut self.agent_seat
    }

    fn agent_state(&mut self) -> &mut Self {
        self
    }

    fn resolve_window(&mut self, window_id: u64) -> Option<WlSurface> {
        let surface = self.surface_map.get(&window_id).cloned();
        if let Some(ref s) = surface {
            self.resolved_window_id = Some(window_id);
            self.resolved_surface_id = Some(s.id().protocol_id());
        }
        surface
    }

    fn resolve_pointer_focus(&mut self, window_id: u64) -> Option<Focus> {
        if self.surface_map.contains_key(&window_id) {
            Some(Focus)
        } else {
            None
        }
    }

    fn set_keyboard_focus(&mut self, wid: u64) -> bool {
        if !self.surface_map.contains_key(&wid) {
            return false;
        }
        self.bound_wid = Some(wid);
        let keyboard = self.agent_seat.keyboard.clone();
        keyboard.set_focus(self, Some(Focus), SERIAL_COUNTER.next_serial());
        wid == self.known_window
    }

    fn token_file_path(&self) -> &Path {
        &self.token_path
    }

    fn is_locked(&self) -> bool {
        *self.lock_override.borrow()
    }

    fn grant_present(&self, _bot_id: &str, _window_id: u64) -> bool {
        true
    }

    fn key_press(&mut self, xkb_name: &str) -> bool {
        self.keys.push(xkb_name.to_string());
        let keyboard = self.agent_seat.keyboard.clone();
        let time = InputTime::from_millis(self.now_ms());
        let delivered = crate::inject::dispatch::inject_agent_key_press(
            &keyboard,
            self,
            xkb_name,
            time,
        );
        let stripped = xkb_name.strip_prefix("XF86XK_");
        let delivered = delivered
            || stripped.is_some_and(|name| {
                crate::inject::dispatch::inject_agent_key_press(&keyboard, self, name, time)
            });
        if !delivered {
            self.bound_keys.push(xkb_name.to_string());
            let mut rec = self.surface_target.lock().unwrap();
            rec.key_count += 1;
            rec.bound_keys.push(xkb_name.to_string());
        }
        true
    }

    fn now_ms(&mut self) -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u32)
            .unwrap_or(0)
    }
}

fn block_on_loop<F: Future>(event_loop: &mut EventLoop<'static, ()>, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    let start = Instant::now();
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

fn write_output(path: &Path, harness: &Harness) {
    let rec = harness.surface_target.lock().unwrap();
    let last = rec
        .last_motion
        .map(|p| format!("{},{}", p.x, p.y))
        .unwrap_or_else(|| "none".into());
    let resolved_window_id = harness
        .resolved_window_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "none".into());
    let resolved_surface_id = harness
        .resolved_surface_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "none".into());
    let bound_surface_id = harness
        .bound_surface
        .as_ref()
        .map(|s| s.id().protocol_id().to_string())
        .unwrap_or_else(|| "none".into());
    let body = format!(
        "agent_seat={}\nmotion_count={}\nlast_motion={last}\naxis_count={}\nkey_count={}\nuser_focused_window={}\nuser_scroll_x={}\nuser_scroll_y={}\nresolved_window_id={resolved_window_id}\nresolved_surface_id={resolved_surface_id}\nbound_surface_id={bound_surface_id}\nagent_keyboard_focus_wid={}\nbound_keys_count={}\n",
        harness.agent_seat.seat.name(),
        rec.motion_count,
        rec.axis_count,
        harness.keys.len(),
        harness
            .user_focused_window
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
        harness.user_scroll_offset.x,
        harness.user_scroll_offset.y,
        harness
            .bound_wid
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
        rec.bound_keys.len(),
    );
    drop(rec);
    let _ = AGENT_SEAT_NAME;
    fs::write(path, body).expect("write e2e output");
}

/// Bind `socket_path`, accept one client, run HELLO→…→dispatch, write stats.
pub fn run_harness(socket_path: PathBuf, token_file: PathBuf, output_file: PathBuf) {
    run_harness_ex(socket_path, token_file, output_file, false);
}

/// Locked-screen variant: post-bind dispatch replies `DISPATCH ERR LOCKED`.
pub fn run_harness_locked(socket_path: PathBuf, token_file: PathBuf, output_file: PathBuf) {
    run_harness_ex(socket_path, token_file, output_file, true);
}

fn run_harness_ex(socket_path: PathBuf, token_file: PathBuf, output_file: PathBuf, locked: bool) {
    let mut harness = Harness::new(token_file);
    if locked {
        harness.set_locked(true);
    }
    let mut event_loop = EventLoop::<()>::try_new().expect("calloop");
    let (server, rx) =
        InjectServer::start(&event_loop.handle(), socket_path).expect("InjectServer::start");

    let start = Instant::now();
    let stream = loop {
        event_loop
            .dispatch(Some(Duration::from_millis(20)), &mut ())
            .expect("dispatch accept loop");
        match rx.try_recv() {
            Ok(s) => break s,
            Err(async_channel::TryRecvError::Empty) => {
                if start.elapsed() > Duration::from_secs(8) {
                    panic!("timeout waiting for inject client");
                }
            }
            Err(async_channel::TryRecvError::Closed) => {
                panic!("inject accept channel closed before a client connected")
            }
        }
    };

    let async_stream = event_loop
        .handle()
        .adapt_io(stream)
        .expect("adapt inject stream");
    let (_id, close_rx) = server.track_connection(String::new());
    block_on_loop(
        &mut event_loop,
        handle_inject_client_with_close(&mut harness, async_stream, close_rx),
    )
    .expect("handle_inject_client");

    write_output(&output_file, &harness);
    drop(server);
}
