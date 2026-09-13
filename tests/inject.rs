//! Inject-plane integration tests (v3).
//!
//! Drives HELLO → AUTH → BIND → CLICK/DRAG through `handle_inject_client` on a
//! Unix-socket pair. The agent seat is a real Smithay `PointerHandle`; the
//! user-seat proxy fields on the harness must stay untouched.

use std::fs;
use std::future::Future;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use niri::inject::calloop::EventLoop;
use niri::inject::handle_inject_client;
use niri::inject::smithay::backend::input::KeyState;
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
use niri::inject::smithay::reexports::wayland_server::{
    Client, Dispatch, Display, DisplayHandle,
};
use niri::inject::smithay::utils::{IsAlive, Logical, Point, Serial};
use niri::inject::{AgentSeat, InjectHarness};

const KNOWN_WINDOW: u64 = 42;
const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const USER_FOCUS: Option<u64> = Some(99);
const USER_SCROLL: (f64, f64) = (1.5, 2.5);

#[derive(Default)]
struct TargetRecord {
    motion_count: u32,
    last_motion: Option<Point<f64, Logical>>,
    key_count: u32,
    motions: Vec<Point<f64, Logical>>,
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

struct Harness {
    seat_state: SeatState<Self>,
    agent_seat: AgentSeat<Self>,
    user_focused_window: Option<u64>,
    user_scroll_offset: Point<f64, Logical>,
    bound_surface: Option<WlSurface>,
    surface_target: Arc<Mutex<TargetRecord>>,
    token_path: PathBuf,
    known_window: u64,
    transient_errors: Vec<String>,
    permanent_errors: Vec<String>,
    _surface_keep: SurfaceKeep,
}

impl Harness {
    fn new(token_path: PathBuf) -> Self {
        let keep = dummy_surface();
        let mut seat_state = SeatState::new();
        let agent_seat = AgentSeat::create(&mut seat_state);
        Harness {
            seat_state,
            agent_seat,
            user_focused_window: USER_FOCUS,
            user_scroll_offset: Point::from(USER_SCROLL),
            bound_surface: Some(keep.surface.clone()),
            surface_target: Arc::new(Mutex::new(TargetRecord::default())),
            token_path,
            known_window: KNOWN_WINDOW,
            transient_errors: Vec::new(),
            permanent_errors: Vec::new(),
            _surface_keep: keep,
        }
    }

    fn reported_bad_token_count(&self) -> usize {
        self.transient_errors
            .iter()
            .chain(self.permanent_errors.iter())
            .filter(|c| *c == "BAD_TOKEN")
            .count()
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
    fn axis(&self, _seat: &Seat<Harness>, _data: &mut Harness, _frame: AxisFrame) {}
    fn frame(&self, _seat: &Seat<Harness>, _data: &mut Harness) {}
    fn leave(&self, _seat: &Seat<Harness>, _data: &mut Harness, _serial: Serial, _time: u32) {}
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
        _key: KeysymHandle<'_>,
        _state: KeyState,
        _serial: Serial,
        _time: u32,
    ) {
        data.surface_target.lock().unwrap().key_count += 1;
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
        if window_id == self.known_window {
            self.bound_surface.clone()
        } else {
            None
        }
    }

    fn resolve_pointer_focus(&mut self, window_id: u64) -> Option<Focus> {
        if window_id == self.known_window {
            Some(Focus)
        } else {
            None
        }
    }

    fn report_permanent_error(&mut self, code: &str) {
        self.permanent_errors.push(code.to_string());
    }

    fn report_transient_error(&mut self, code: &str) {
        self.transient_errors.push(code.to_string());
    }

    fn token_file_path(&self) -> &Path {
        &self.token_path
    }
}

fn token_file() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "qlux-inject-itokens-{}-{nanos}",
        std::process::id()
    ));
    fs::write(&path, format!("bot-a:{TOKEN_A}\nbot-b:{TOKEN_B}\n")).unwrap();
    path
}

fn hello_auth() -> String {
    format!("HELLO\nAUTH\n{TOKEN_A}\n")
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

fn run_protocol(harness: &mut Harness, requests: &[u8]) -> String {
    let (mut client, server) = UnixStream::pair().expect("inject socket pair");
    client.write_all(requests).expect("write inject frames");
    client.shutdown(Shutdown::Write).expect("shutdown write");

    let mut event_loop = EventLoop::<()>::try_new().expect("calloop");
    let async_server = event_loop
        .handle()
        .adapt_io(server)
        .expect("adapt inject stream");
    block_on_loop(
        &mut event_loop,
        handle_inject_client(harness, async_server),
    )
    .expect("handle_inject_client");

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).expect("read inject replies");
    String::from_utf8(buf).expect("utf8 replies")
}

fn assert_user_seat_untouched(harness: &Harness) {
    assert_eq!(harness.user_focused_window, USER_FOCUS);
    assert_eq!(harness.user_scroll_offset, Point::from(USER_SCROLL));
}

#[test]
fn bind_click_drives_pointer_motion_on_agent_seat() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");

    let rec = harness.surface_target.lock().unwrap();
    assert_eq!(rec.motion_count, 1);
    assert_eq!(rec.last_motion, Some(Point::from((100.0, 200.0))));
    drop(rec);
    assert_user_seat_untouched(&harness);
}

#[test]
fn auth_mismatch_closes_after_retry() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let req = format!("HELLO\nAUTH\nBEEF\nAUTH\nBEEF\n");
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert_eq!(
        replies.matches("AUTH ERR BAD_TOKEN\n").count(),
        2,
        "{replies:?}"
    );
    assert_eq!(harness.reported_bad_token_count(), 2);
    assert!(
        replies.contains("BIND OK") == false && replies.contains("DISPATCH OK") == false,
        "{replies:?}"
    );
}

#[test]
fn bind_unknown_window_returns_err_and_keeps_open() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let req = format!(
        "{}BIND 999999\nBIND {KNOWN_WINDOW}\n",
        hello_auth()
    );
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(
        replies.contains("FRAME ERR BAD_WINDOW_ID\n"),
        "{replies:?}"
    );
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    let bind_err = replies.find("FRAME ERR BAD_WINDOW_ID\n").unwrap();
    let bind_ok = replies.find("BIND OK\n").unwrap();
    assert!(bind_err < bind_ok, "{replies:?}");
}

#[test]
fn drag_synthesizes_motion_button_motion_button() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let req = format!(
        "{}BIND {KNOWN_WINDOW}\nDRAG 10 20 30 40\n",
        hello_auth()
    );
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");

    let rec = harness.surface_target.lock().unwrap();
    assert_eq!(rec.motion_count, 2);
    assert_eq!(rec.motions.len(), 2);
    assert_eq!(rec.motions[0], Point::from((10.0, 20.0)));
    assert_eq!(rec.motions[1], Point::from((30.0, 40.0)));
    drop(rec);
    assert_user_seat_untouched(&harness);
}
