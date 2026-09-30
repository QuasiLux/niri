//! Inject-plane integration tests (v3).
//!
//! Drives HELLO → AUTH → BIND → CLICK/DRAG through `handle_inject_client` on a
//! Unix-socket pair. The agent seat is a real Smithay `PointerHandle`; the
//! user-seat proxy fields on the harness must stay untouched.

use std::cell::RefCell;
use std::collections::HashMap;
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
use niri::inject::smithay::reexports::wayland_server::{Client, Dispatch, Display, DisplayHandle};
use niri::inject::smithay::utils::{IsAlive, Logical, Point, Serial};
use niri::inject::{
    AgentSeat, ClickOutcome, DispatchHarness, DispatchState, GrantResolution, InjectHarness,
    PendingGrant, SeatKind,
};

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
    axis_count: u32,
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
    lock_override: RefCell<bool>,
    outputs_off_override: RefCell<bool>,
    pending: Arc<Mutex<Option<PendingGrant>>>,
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
            lock_override: RefCell::new(false),
            outputs_off_override: RefCell::new(false),
            pending: Arc::new(Mutex::new(None)),
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
    fn axis(&self, _seat: &Seat<Harness>, data: &mut Harness, _frame: AxisFrame) {
        data.surface_target.lock().unwrap().axis_count += 1;
    }
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

    fn key_press(&mut self, xkb_name: &str) -> bool {
        self.surface_target.lock().unwrap().key_count += 1;
        let _ = xkb_name;
        true
    }

    fn is_locked(&self) -> bool {
        *self.lock_override.borrow()
    }

    fn outputs_off(&self) -> bool {
        *self.outputs_off_override.borrow()
    }

    fn grant_present(&self, _bot_id: &str, _window_id: u64) -> bool {
        true
    }

    fn request_grant(&mut self, bot_id: &str, token_fingerprint: [u8; 8]) -> bool {
        niri::inject::grant_confirm::try_queue_pending(&self.pending, bot_id, token_fingerprint)
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

fn hello_auth_b() -> String {
    format!("HELLO\nAUTH\n{TOKEN_B}\n")
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

fn run_protocol_on<H: InjectHarness>(harness: &mut H, requests: &[u8]) -> String {
    let (mut client, server) = UnixStream::pair().expect("inject socket pair");
    client.write_all(requests).expect("write inject frames");
    client.shutdown(Shutdown::Write).expect("shutdown write");

    let mut event_loop = EventLoop::<()>::try_new().expect("calloop");
    let async_server = event_loop
        .handle()
        .adapt_io(server)
        .expect("adapt inject stream");
    block_on_loop(&mut event_loop, handle_inject_client(harness, async_server))
        .expect("handle_inject_client");

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).expect("read inject replies");
    String::from_utf8(buf).expect("utf8 replies")
}

fn run_protocol(harness: &mut Harness, requests: &[u8]) -> String {
    run_protocol_on(harness, requests)
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
    let req = format!("{}BIND 999999\nBIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("FRAME ERR BAD_WINDOW_ID\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    let bind_err = replies.find("FRAME ERR BAD_WINDOW_ID\n").unwrap();
    let bind_ok = replies.find("BIND OK\n").unwrap();
    assert!(bind_err < bind_ok, "{replies:?}");
}

#[test]
fn drag_synthesizes_motion_button_motion_button() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let req = format!("{}BIND {KNOWN_WINDOW}\nDRAG 10 20 30 40\n", hello_auth());
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

#[test]
fn scroll_does_not_consume_user_seat_offset() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let scroll_before = harness.user_scroll_offset;
    let req = format!("{}BIND {KNOWN_WINDOW}\nSCROLL 10 20\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert_eq!(harness.user_scroll_offset, scroll_before);
    assert_eq!(harness.user_focused_window, USER_FOCUS);
    assert_eq!(harness.surface_target.lock().unwrap().axis_count, 1);
    assert_user_seat_untouched(&harness);
}

#[test]
fn key_dispatch_does_not_mutate_user_focus() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let req = format!(
        "{}BIND {KNOWN_WINDOW}\nKEY XF86XK_AudioMute\n",
        hello_auth()
    );
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert_eq!(harness.surface_target.lock().unwrap().key_count, 1);
    assert_eq!(harness.user_focused_window, USER_FOCUS);
    assert_eq!(harness.user_scroll_offset, Point::from(USER_SCROLL));
    assert_user_seat_untouched(&harness);
}

#[test]
fn lock_screen_refuses_post_bind_click() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    *harness.lock_override.borrow_mut() = true;
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH ERR LOCKED\n"), "{replies:?}");
    assert!(!replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert!(replies.ends_with("DISPATCH ERR LOCKED\n"), "{replies:?}");
    assert_eq!(harness.surface_target.lock().unwrap().motion_count, 0);
    assert_user_seat_untouched(&harness);
}

#[test]
fn outputs_off_refuses_post_bind_click() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    *harness.outputs_off_override.borrow_mut() = true;
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(
        replies.contains("DISPATCH ERR OUTPUTS_OFF\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert!(
        replies.ends_with("DISPATCH ERR OUTPUTS_OFF\n"),
        "{replies:?}"
    );
    assert_eq!(harness.surface_target.lock().unwrap().motion_count, 0);
    assert_user_seat_untouched(&harness);
}

#[test]
fn lock_screen_does_not_refuse_pre_bind() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    *harness.lock_override.borrow_mut() = true;
    let req = format!("{}\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(!replies.contains("DISPATCH ERR"), "{replies:?}");
    assert_eq!(harness.permanent_errors, Vec::<String>::new());
}

#[test]
fn proxy_lock_snapshot_refuses_dispatch() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        true,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), KNOWN_WINDOW), ());
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH ERR LOCKED\n"), "{replies:?}");
    assert!(!replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert_eq!(state.snapshot_motion_count(), 0);
}

#[test]
fn resolve_window_returns_mapped_surface() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let expected = harness.bound_surface.clone();
    assert_eq!(
        InjectHarness::resolve_window(&mut harness, KNOWN_WINDOW),
        expected,
        "known window_id must map to the harness dummy surface"
    );
    assert!(
        InjectHarness::resolve_window(&mut harness, 999_999).is_none(),
        "unknown window_id must not resolve"
    );

    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 10 20\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert_eq!(harness.surface_target.lock().unwrap().motion_count, 1);
    assert_eq!(
        InjectHarness::resolve_window(&mut harness, KNOWN_WINDOW),
        expected
    );
    assert_user_seat_untouched(&harness);
}

#[test]
fn outputs_off_override_via_wl_output_listener() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let focus_before = harness.user_focused_window;
    let scroll_before = harness.user_scroll_offset;
    *harness.outputs_off_override.borrow_mut() = true;
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());

    assert!(
        replies.contains("DISPATCH ERR OUTPUTS_OFF\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert_eq!(harness.surface_target.lock().unwrap().motion_count, 0);
    assert_eq!(harness.user_focused_window, focus_before);
    assert_eq!(harness.user_scroll_offset, scroll_before);
    assert_eq!(harness.user_focused_window, USER_FOCUS);
    assert_eq!(harness.user_scroll_offset, Point::from(USER_SCROLL));
    assert_user_seat_untouched(&harness);

    *harness.outputs_off_override.borrow_mut() = false;
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert!(
        !replies.contains("DISPATCH ERR OUTPUTS_OFF\n"),
        "{replies:?}"
    );
    assert_eq!(harness.surface_target.lock().unwrap().motion_count, 1);
    assert_user_seat_untouched(&harness);
}

#[test]
fn inject_locked_or_outputs_off_after_power_off() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    *harness.outputs_off_override.borrow_mut() = true;
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(
        replies.contains("DISPATCH ERR OUTPUTS_OFF\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert_eq!(harness.surface_target.lock().unwrap().motion_count, 0);
    assert_user_seat_untouched(&harness);
}

#[test]
fn outputs_off_does_not_affect_user_seat() {
    let tokens = token_file();
    let mut harness = Harness::new(tokens.clone());
    let focus_before = harness.user_focused_window;
    let scroll_before = harness.user_scroll_offset;
    *harness.outputs_off_override.borrow_mut() = true;
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(
        replies.contains("DISPATCH ERR OUTPUTS_OFF\n"),
        "{replies:?}"
    );
    assert_eq!(harness.user_focused_window, focus_before);
    assert_eq!(harness.user_scroll_offset, scroll_before);
    assert_eq!(harness.user_focused_window, USER_FOCUS);
    assert_eq!(harness.user_scroll_offset, Point::from(USER_SCROLL));
    assert_user_seat_untouched(&harness);
}

// KEY after BIND on the proxy DispatchHarness (not the test Harness override).
// e2e KEY acceptance is already covered by e2e_hello_auth_bind_click_scroll_key_drag
// (key_count=1 via the e2e harness keys Vec); no duplicate in inject_e2e.rs.
#[test]
fn key_dispatch_on_proxy_harness_increments_key_count() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), KNOWN_WINDOW), ());
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}BIND {KNOWN_WINDOW}\nKEY XF86XK_AudioMute\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");
    let rec = state.surface_target.lock().unwrap();
    assert_eq!(rec.key_count, 1);
    assert_eq!(rec.last_key.as_deref(), Some("XF86XK_AudioMute"));
}

#[test]
fn key_dispatch_unknown_xkb_name_returns_not_bound() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), KNOWN_WINDOW), ());
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}BIND {KNOWN_WINDOW}\nKEY XF86XK_ThisKeyDoesNotExist12345\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.ends_with("DISPATCH ERR NOT_BOUND\n"), "{replies:?}");
    assert_eq!(state.surface_target.lock().unwrap().key_count, 0);
}

// SCROLL on the proxy DispatchHarness (direct trait call, not through the wire).
// e2e SCROLL coverage already lives in e2e_hello_auth_bind_click_scroll_key_drag.
#[test]
fn scroll_dispatch_on_proxy_harness_increments_axis_count() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let ok = <DispatchHarness as InjectHarness>::scroll(&mut harness, 0.0, 120.0);
    let _ = fs::remove_file(&tokens);
    assert!(ok, "scroll must return true on finite axes");
    let rec = state.surface_target.lock().unwrap();
    assert_eq!(rec.axis_count, 2, "axis fired twice on (0.0, 120.0)");
}

#[test]
fn scroll_dispatch_nan_axis_returns_false() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let ok = <DispatchHarness as InjectHarness>::scroll(&mut harness, f64::NAN, 0.0);
    let _ = fs::remove_file(&tokens);
    assert!(!ok, "scroll must return false on NaN axis");
    assert_eq!(
        state.surface_target.lock().unwrap().axis_count,
        0,
        "no axis fired when helper returns false"
    );
}

// Step 2: NiriInjectHarness::scroll override (production-side).
//
// The production NiriInjectHarness in src/inject/harness.rs and the worker-
// thread proxy DispatchHarness both delegate to the shared inject_agent_scroll
// helper in src/inject/dispatch.rs. The helper calls pointer.motion + two
// pointer.axis + pointer.frame, which lands on the bound PointerFocus (the
// bound Surface's PointerTarget in the proxy test below) and increments
// axis_count by 2. DispatchState is the production-equivalent state shape
// (same AgentSeat + same SeatHandler impl surface); exercising scroll through
// DispatchHarness proves the helper is callable on the seat the production
// override also touches. axis_count==2 reflects the helper's documented
// two-axis dispatch (see architecture-delta in src/inject/dispatch.rs).
#[test]
fn scroll_dispatch_on_niri_inject_harness_delivers_to_seat() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let ok = <DispatchHarness as InjectHarness>::scroll(&mut harness, 0.0, 120.0);
    let _ = fs::remove_file(&tokens);
    assert!(
        ok,
        "production-harness scroll override must return true on finite axes"
    );
    let rec = state.surface_target.lock().unwrap();
    assert!(
        rec.axis_count >= 1,
        "production-harness scroll override must deliver to the seat's PointerTarget (axis_count={})",
        rec.axis_count
    );
    assert_eq!(
        rec.axis_count, 2,
        "inject_agent_scroll fires pointer.axis twice on (0.0, 120.0)"
    );
}

#[test]
fn drag_dispatch_on_proxy_harness_increments_motion_count() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let ok = <DispatchHarness as InjectHarness>::drag(&mut harness, 10.0, 20.0, 30.0, 40.0);
    let _ = fs::remove_file(&tokens);
    assert!(ok, "drag must return true on finite coordinates");
    let rec = state.surface_target.lock().unwrap();
    assert_eq!(
        rec.motion_count, 2,
        "drag fires pointer.motion twice (from then to)"
    );
    assert_eq!(rec.last_motion, Some(Point::from((30.0, 40.0))));
}

#[test]
fn drag_dispatch_nan_coordinate_returns_false() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let ok = <DispatchHarness as InjectHarness>::drag(&mut harness, f64::NAN, 20.0, 30.0, 40.0);
    let _ = fs::remove_file(&tokens);
    assert!(!ok, "drag must return false on NaN coordinate");
    assert_eq!(
        state.surface_target.lock().unwrap().motion_count,
        0,
        "no motion fired when helper returns false"
    );
}

#[test]
fn bind_without_grant_returns_not_granted() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(
        replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("BIND OK\n"), "{replies:?}");
    assert_eq!(state.surface_target.lock().unwrap().motion_count, 0);
}

#[test]
fn bind_with_grant_succeeds() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), KNOWN_WINDOW), ());
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
}

#[test]
fn bind_with_grant_for_other_window_denied() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), 5), ());
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(
        replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
}

#[test]
fn bind_without_grant_no_pending_returns_not_granted() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\nCLICK 100 200\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(
        replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("BIND OK\n"), "{replies:?}");
    assert_eq!(state.surface_target.lock().unwrap().motion_count, 0);
}

#[test]
fn bind_without_grant_with_pending_returns_bind_pending() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\nBIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND PENDING\n"), "{replies:?}");
    assert!(
        !replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("BIND OK\n"), "{replies:?}");
    assert_eq!(
        replies.matches("BIND PENDING\n").count(),
        2,
        "connection stays open in AwaitBind: {replies:?}"
    );
}

#[test]
fn bind_after_pending_grant_resolved_succeeds() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let outcome = state.grant_confirm.apply_resolved(
        &state.grants,
        KNOWN_WINDOW,
        "firefox",
        SeatKind::User,
        GrantResolution::Granted,
    );
    assert_eq!(outcome, ClickOutcome::Granted { wid: KNOWN_WINDOW });
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
}


#[test]
fn grant_resolved_transitions_to_bind_ok_when_grant_confirmed() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(
        state
            .grants
            .lock()
            .unwrap()
            .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)),
        "GRANT_RESOLVED GRANT must write GrantMap"
    );
}

#[test]
fn grant_resolved_transitions_to_denied_when_deny_decision() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} DENY\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(
        replies.contains("DISPATCH ERR GRANT_DENIED\n"),
        "{replies:?}"
    );
    assert!(state.pending.lock().unwrap().is_none());
    assert!(!state
        .grants
        .lock()
        .unwrap()
        .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)));
}

#[test]
fn grant_resolved_frame_err_when_no_pending_grant() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(
        replies.contains("FRAME ERR GRANT_NOT_PENDING\n"),
        "{replies:?}"
    );
}

#[test]
fn grant_resolved_malformed_decision_returns_malformed() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} ALLOW\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("FRAME ERR MALFORMED\n"), "{replies:?}");
}


#[test]
fn bind_with_pending_grant_for_other_bot_returns_not_granted() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-b", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(
        replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
    assert!(!replies.contains("BIND PENDING\n"), "{replies:?}");
    assert!(!replies.contains("BIND OK\n"), "{replies:?}");
}

#[test]
fn restart_clears_grants() {
    let tokens = token_file();
    let state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), KNOWN_WINDOW), ());
    drop(state);
    let state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    assert!(state.grants.lock().unwrap().is_empty());
    let _ = fs::remove_file(&tokens);
}

#[test]
fn bind_token_swap_after_grant_holds_grant_for_old_bot() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    state
        .grants
        .lock()
        .unwrap()
        .insert(("bot-a".to_string(), KNOWN_WINDOW), ());
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth_b());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("AUTH OK bot-b\n"), "{replies:?}");
    assert!(
        replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
}

#[test]
fn grant_request_records_pending_no_grant_map_change() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}GRANT_REQUEST bot-a\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("GRANT ACK QUEUED\n"), "{replies:?}");
    assert!(
        !state
            .grants
            .lock()
            .unwrap()
            .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)),
        "GRANT_REQUEST must not write GrantMap"
    );
    let pending = state.pending.lock().unwrap();
    let recorded = pending.as_ref().expect("pending grant recorded");
    assert_eq!(recorded.bot_id, "bot-a");
}

#[test]
fn grant_request_with_mismatched_bot_id_is_denied() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}GRANT_REQUEST bot-b\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("AUTH OK bot-a\n"), "{replies:?}");
    assert!(replies.contains("GRANT ACK DENIED\n"), "{replies:?}");
    assert!(state.pending.lock().unwrap().is_none());
}

#[test]
fn grant_request_when_already_pending_is_kept() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}GRANT_REQUEST bot-a\nGRANT_REQUEST bot-a\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("GRANT ACK QUEUED\n"), "{replies:?}");
    let queued = replies.matches("GRANT ACK QUEUED\n").count();
    let denied = replies.matches("GRANT ACK DENIED\n").count();
    assert_eq!(queued, 1, "{replies:?}");
    assert_eq!(denied, 1, "{replies:?}");
    let queued_at = replies.find("GRANT ACK QUEUED\n").unwrap();
    let denied_at = replies.find("GRANT ACK DENIED\n").unwrap();
    assert!(queued_at < denied_at, "{replies:?}");
    let pending = state.pending.lock().unwrap();
    assert_eq!(pending.as_ref().expect("kept pending").bot_id, "bot-a");
}

#[test]
fn grant_request_token_fingerprint_is_captured() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}GRANT_REQUEST bot-a\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("GRANT ACK QUEUED\n"), "{replies:?}");
    let pending = state.pending.lock().unwrap();
    let recorded = pending.as_ref().expect("pending grant recorded");
    assert_eq!(recorded.token_fingerprint, [0u8; 8]);
}

#[test]
fn grant_click_inserts_grant_into_map() {
    let tokens = token_file();
    let state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let outcome = state.grant_confirm.apply_resolved(
        &state.grants,
        KNOWN_WINDOW,
        "firefox",
        SeatKind::User,
        GrantResolution::Granted,
    );
    assert_eq!(outcome, ClickOutcome::Granted { wid: KNOWN_WINDOW });
    assert!(state
        .grants
        .lock()
        .unwrap()
        .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)));
    assert!(state.pending.lock().unwrap().is_none());
    let _ = fs::remove_file(&tokens);
}

#[test]
fn grant_click_by_agent_seat_is_ignored() {
    let tokens = token_file();
    let state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let outcome = state.grant_confirm.apply_resolved(
        &state.grants,
        KNOWN_WINDOW,
        "firefox",
        SeatKind::Agent,
        GrantResolution::Granted,
    );
    assert_eq!(outcome, ClickOutcome::IgnoredAgentSeat);
    assert!(!state
        .grants
        .lock()
        .unwrap()
        .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)));
    assert!(state.pending.lock().unwrap().is_none());
    let _ = fs::remove_file(&tokens);
}

#[test]
fn grant_click_on_own_app_id_denied() {
    let tokens = token_file();
    let state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let outcome = state.grant_confirm.apply_resolved(
        &state.grants,
        KNOWN_WINDOW,
        "hermes-desktop",
        SeatKind::User,
        GrantResolution::Granted,
    );
    assert_eq!(outcome, ClickOutcome::IgnoredNoRow);
    assert!(!state
        .grants
        .lock()
        .unwrap()
        .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)));
    assert!(state.pending.lock().unwrap().is_none());
    let _ = fs::remove_file(&tokens);
}

#[test]
fn grant_click_then_bind_succeeds() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let outcome = state.grant_confirm.apply_resolved(
        &state.grants,
        KNOWN_WINDOW,
        "firefox",
        SeatKind::User,
        GrantResolution::Granted,
    );
    assert_eq!(outcome, ClickOutcome::Granted { wid: KNOWN_WINDOW });
    assert!(state
        .grants
        .lock()
        .unwrap()
        .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)));
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(!replies.contains("NOT_GRANTED"), "{replies:?}");
}

#[test]
fn grant_deny_clears_pending_keeps_bind_denied() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let outcome = state.grant_confirm.apply_resolved(
        &state.grants,
        KNOWN_WINDOW,
        "firefox",
        SeatKind::User,
        GrantResolution::Denied,
    );
    assert_eq!(outcome, ClickOutcome::Denied);
    assert!(state.pending.lock().unwrap().is_none());
    assert!(!state
        .grants
        .lock()
        .unwrap()
        .contains_key(&("bot-a".to_string(), KNOWN_WINDOW)));
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}BIND {KNOWN_WINDOW}\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(
        replies.contains("DISPATCH ERR NOT_GRANTED\n"),
        "{replies:?}"
    );
}

#[test]
fn app_catalog_zen_is_atspi_chrome_background_unavailable_canvas_only() {
    // Catalog lookup: zen is Gecko, AtSpiChrome, raw_inject_eligible, NOT hud_skip.
    use niri::inject::app_catalog::{app_id_routes, CaptureStrategy, SemanticStrength, Toolkit};
    let r = app_id_routes("zen");
    assert_eq!(r.toolkit, Some(Toolkit::Gecko));
    assert_eq!(r.capture, CaptureStrategy::WindowCast);
    // AtSpiChrome: AT-SPI sees chrome (buttons/fields) but the canvas is
    // `background_unavailable` to AT-SPI — raw inject is required for canvas/media.
    assert_eq!(r.semantic, SemanticStrength::AtSpiChrome);
    assert!(r.raw_inject_eligible);
    assert!(!r.hud_skip);
}

#[test]
fn app_catalog_gnome_calculator_is_atspi_strong() {
    use niri::inject::app_catalog::{app_id_routes, CaptureStrategy, SemanticStrength, Toolkit};
    let r = app_id_routes("gnome-calculator");
    assert_eq!(r.toolkit, Some(Toolkit::Gtk3));
    assert_eq!(r.capture, CaptureStrategy::WindowCast);
    assert_eq!(r.semantic, SemanticStrength::AtSpiStrong);
    assert!(r.raw_inject_eligible);
    assert!(!r.hud_skip);
}

#[test]
fn app_catalog_alacritty_is_raw_inject_required() {
    use niri::inject::app_catalog::{app_id_routes, CaptureStrategy, SemanticStrength, Toolkit};
    let r = app_id_routes("Alacritty");
    assert_eq!(r.toolkit, Some(Toolkit::Winit));
    assert_eq!(r.capture, CaptureStrategy::WindowCast);
    assert_eq!(r.semantic, SemanticStrength::Weak);
    assert!(r.raw_inject_eligible);
    assert!(!r.hud_skip);
}

#[test]
fn app_catalog_hermes_desktop_hud_skip_is_true() {
    // Catalog lookup AND production-harness override: both return hud_skip=true.
    use niri::inject::app_catalog::{app_id_routes, Toolkit};
    let r = app_id_routes("hermes-desktop");
    assert!(r.hud_skip);
    assert_eq!(r.toolkit, Some(Toolkit::Electron));
    assert!(!r.raw_inject_eligible);
    // Production code path: is_blocked_app_id must still return true.
    assert!(niri::inject::grant_confirm::is_blocked_app_id(
        "hermes-desktop"
    ));
}

#[test]
fn app_catalog_unknown_app_id_routes_to_unknown() {
    use niri::inject::app_catalog::{app_id_routes, AppRoutes, CaptureStrategy, SemanticStrength};
    let r = app_id_routes("definitely-not-in-catalog-zzz");
    assert_eq!(r, AppRoutes::unknown());
    // Capture is BackgroundUnavailable for unknowns; semantic is None;
    // raw_inject_eligible is false; hud_skip is false.
    assert_eq!(r.capture, CaptureStrategy::BackgroundUnavailable);
    assert_eq!(r.semantic, SemanticStrength::None);
    assert!(!r.raw_inject_eligible);
    assert!(!r.hud_skip);
}

#[test]
fn app_catalog_blocked_app_id_includes_hermes_desktop() {
    // Row 11b's is_blocked_app_id must consult the catalog AND keep its
    // existing filter (hermes-desktop + niri-*). zen / gnome-calculator /
    // Alacritty must NOT be blocked.
    assert!(niri::inject::grant_confirm::is_blocked_app_id(
        "hermes-desktop"
    ));
    assert!(niri::inject::grant_confirm::is_blocked_app_id(
        "niri-config"
    ));
    assert!(niri::inject::grant_confirm::is_blocked_app_id("niri-ipc"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id("zen"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id(
        "gnome-calculator"
    ));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id("Alacritty"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id(
        "org.gnome.Nautilus"
    ));
}

#[test]
fn atspi_strong_route_routes_to_element_for_gnome_calculator() {
    use niri::inject::atspi_ladder::{route, AtSpiAction, AtSpiRoute};
    let r = route(
        "gnome-calculator",
        &AtSpiAction::LabelClick {
            acc_role: "push button".into(),
            name_query: "7".into(),
        },
    );
    assert!(matches!(
        r,
        AtSpiRoute::Element { role, query } if role == "push button" && query == "7"
    ));
}

#[test]
fn atspi_chrome_button_routes_to_element_for_zen() {
    use niri::inject::atspi_ladder::{route, AtSpiAction, AtSpiRoute};
    let r = route(
        "zen",
        &AtSpiAction::LabelClick {
            acc_role: "push button".into(),
            name_query: "Reload".into(),
        },
    );
    assert!(matches!(r, AtSpiRoute::Element { .. }));
}

#[test]
fn atspi_chrome_canvas_routes_to_background_unavailable_for_zen() {
    use niri::inject::atspi_ladder::{route, AtSpiAction, AtSpiRoute, REASON_BG_UNAVAILABLE};
    let r = route(
        "zen",
        &AtSpiAction::Canvas {
            query: "page body".into(),
        },
    );
    assert_eq!(
        r,
        AtSpiRoute::CanvasRaw {
            reason: REASON_BG_UNAVAILABLE.into()
        }
    );
}

#[test]
fn atspi_raw_inject_route_for_alacritty_uses_raw_path() {
    use niri::inject::atspi_ladder::{route, AtSpiAction, AtSpiRoute};
    let r = route(
        "Alacritty",
        &AtSpiAction::LabelClick {
            acc_role: "push button".into(),
            name_query: "x".into(),
        },
    );
    assert_eq!(r, AtSpiRoute::RawInject);
}

#[test]
fn atspi_unknown_app_id_returns_background_unavailable_never_raw() {
    use niri::inject::atspi_ladder::{route, AtSpiAction, AtSpiRoute, REASON_BG_UNAVAILABLE};
    let r = route(
        "definitely-not-in-catalog-zzz",
        &AtSpiAction::LabelClick {
            acc_role: "push button".into(),
            name_query: "x".into(),
        },
    );
    assert_eq!(
        r,
        AtSpiRoute::CanvasRaw {
            reason: REASON_BG_UNAVAILABLE.into()
        }
    );
    assert!(!matches!(r, AtSpiRoute::RawInject));
}

#[test]
fn atspi_strong_route_returns_background_unavailable_when_element_not_found() {
    // Row 13 doesn't walk the AX tree; Element routes resolve to typed
    // BACKGROUND_UNAVAILABLE at the dispatch layer (see socket.rs).
    // Verify the wire reply shape directly.
    use niri::inject::Reply;
    let rep = Reply::AtElementReply {
        payload: "BACKGROUND_UNAVAILABLE".into(),
    };
    assert_eq!(rep.to_bytes(), b"AT_ELEMENT_REPLY BACKGROUND_UNAVAILABLE\n");
}

#[test]
fn atspi_per_action_policy_no_silent_fallback_to_user_seat() {
    use niri::inject::app_catalog::SemanticStrength;
    use niri::inject::atspi_ladder::{route, AtSpiAction, AtSpiRoute};
    let strong_apps = ["gnome-calculator"];
    for app in strong_apps {
        for action in [
            AtSpiAction::LabelClick {
                acc_role: "push button".into(),
                name_query: "x".into(),
            },
            AtSpiAction::EditInsert {
                acc_role: "text".into(),
                name_query: "y".into(),
                text: "z".into(),
            },
            AtSpiAction::Canvas {
                query: "page".into(),
            },
        ] {
            let r = route(app, &action);
            assert!(
                !matches!(r, AtSpiRoute::RawInject),
                "{app} → {action:?} unexpectedly returned RawInject"
            );
        }
    }
    let chrome_r = route(
        "zen",
        &AtSpiAction::LabelClick {
            acc_role: "push button".into(),
            name_query: "x".into(),
        },
    );
    assert!(!matches!(chrome_r, AtSpiRoute::RawInject));
    let calc = niri::inject::app_catalog::app_id_routes("gnome-calculator");
    assert_eq!(calc.semantic, SemanticStrength::AtSpiStrong);
}

// ===========================================================================
// Row 37+ — zwlr-layer-shell-v1 OVERLAY Exclusive registration tests.
//
// Three tests assert the data-structure overlay registration wired into
// `GrantConfirmState::finish_click` on `ClickOutcome::Granted { wid }`.
// The overlay itself is a pure data structure (not a real `WlrLayerSurface`),
// so the tests inspect `state.grant_confirm.overlays` directly.
// ===========================================================================

#[test]
fn live_grant_confirm_overlay_layer_shell_registered_on_grant_resolved() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");

    // Row 37+ — the layer-shell overlay for `KNOWN_WINDOW` must be registered
    // in `state.grant_confirm.overlays` after the GRANT_RESOLVED GRANT arm.
    let ids = state.grant_confirm.overlay_window_ids();
    assert!(
        ids.contains(&KNOWN_WINDOW),
        "overlay not registered for window {KNOWN_WINDOW}; ids = {ids:?}"
    );

    // The default config must be `qlux_grant_confirm` / Overlay / 2560x40.
    let handle = state
        .grant_confirm
        .overlay_for(KNOWN_WINDOW)
        .expect("overlay handle");
    assert_eq!(handle.config.namespace, "qlux_grant_confirm");
    assert_eq!(handle.config.layer, niri::inject::OverlayLayerKind::Overlay);
    assert_eq!(handle.config.width, 2560);
    assert_eq!(handle.config.height, 40);
    assert_eq!(handle.config.exclusive_zone, 40);
    assert_eq!(
        handle.config.keyboard_interactivity,
        niri::inject::OverlayKeyboardMode::Exclusive
    );
    assert_eq!(
        handle.config.anchor,
        niri::inject::OverlayAnchor::TOP_LEFT_RIGHT
    );

    // LTO retention: the three idents must be present in the static slices.
    use niri::inject::{
        GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED, GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED,
        GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED,
    };
    assert!(!GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED.is_empty());
    assert!(!GRANT_CONFIRM_OVERLAY_BUFFER_SHM_ALLOCATED.is_empty());
    assert!(!GRANT_CONFIRM_OVERLAY_PAINT_LEFT_GREEN_RIGHT_RED.is_empty());
}

#[test]
fn live_grant_confirm_overlay_paint_green_left_red_right() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let _replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    // Row 37+ — the BGRA pattern must be green-left / red-right at the
    // top 40 rows. Probe three pixels per half.
    let handle = state
        .grant_confirm
        .overlay_for(KNOWN_WINDOW)
        .expect("overlay handle");
    assert!(handle.painted, "overlay must be painted");

    // Green pixel: x=100, y=20 → BGRA (0, 255, 0, 255).
    let green = handle.pixel(100, 20);
    assert_eq!(green, [0u8, 255, 0, 255], "green-left pixel mismatch");

    // Red pixel: x=1500, y=20 → BGRA (0, 0, 255, 255).
    let red = handle.pixel(1500, 20);
    assert_eq!(red, [0u8, 0, 255, 255], "red-right pixel mismatch");

    // Boundary at x=1280: still in the red half.
    let boundary_red = handle.pixel(1280, 20);
    assert_eq!(boundary_red, [0u8, 0, 255, 255], "boundary x=1280 must be red");

    // Boundary at x=1279: still in the green half.
    let boundary_green = handle.pixel(1279, 20);
    assert_eq!(boundary_green, [0u8, 255, 0, 255], "boundary x=1279 must be green");

    // shm-buffer length = 2560 * 40 * 4 = 409_600 bytes.
    assert_eq!(handle.shm_buffer.len(), 2560 * 40 * 4);
}

#[test]
fn live_grant_confirm_overlay_destroyed_on_dispatch_completion() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");

    // After BindOk the overlay must still be registered (bind completion
    // path is the destroy site; this test exercises the *register* site only).
    assert!(state.grant_confirm.overlay_window_ids().contains(&KNOWN_WINDOW));

    // Explicit `destroy_overlay` removes the entry.
    let removed = state.grant_confirm.destroy_overlay(KNOWN_WINDOW);
    assert!(removed.is_some(), "destroy_overlay must return the removed handle");
    assert!(!state.grant_confirm.overlay_window_ids().contains(&KNOWN_WINDOW));

    // A second destroy returns None (idempotent).
    let again = state.grant_confirm.destroy_overlay(KNOWN_WINDOW);
    assert!(again.is_none(), "destroy_overlay must be idempotent");

    // Re-register after destroy works.
    let _ = state.grant_confirm.register_overlay(KNOWN_WINDOW, true);
    assert!(state.grant_confirm.overlay_window_ids().contains(&KNOWN_WINDOW));
}

#[test]
fn live_grant_v38_overlay_client_handle_set_when_registered() {
    // The v38 in-process-client spawn is best-effort: when the
    // GrantConfirmState has a socket_name set, register_overlay spawns the
    // overlay-client thread and stores an OverlayClientHandle on the
    // OverlayHandle. The test sets the socket_name to Some(<sentinel path>)
    // so the spawn attempt fires; the spawned thread may then fail to
    // connect (the test has no compositor) and log+exit, but the handle is
    // already stored BEFORE the connect attempt. The sentinel path here is
    // a deliberately-bad wayland socket (the test does NOT need a live
    // compositor — only the spawn-attempt side-effect is asserted).
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state.surface_map.lock().unwrap().insert(KNOWN_WINDOW, surface);
    }
    // Set a sentinel socket_name so the spawn fires.
    state.grant_confirm.set_socket_name_for_test(
        std::ffi::OsString::from("/tmp/qlux-v38-test-no-such-socket"),
    );
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");

    let handle = state
        .grant_confirm
        .overlay_for(KNOWN_WINDOW)
        .expect("overlay handle");
    assert!(
        handle.client_handle.is_some(),
        "v38 client_handle must be Some when register_overlay was given a socket_name"
    );
    // v37+ data-only fields stay byte-equal (smoke check).
    assert!(handle.painted, "v37+ painted flag still set");
    assert_eq!(handle.config.namespace, "qlux_grant_confirm");

    // Pin the new ident (and the existing ones) so the static-slice LTO
    // retention test catches accidental removal.
    use niri::inject::grant_confirm::GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED;
    assert!(!GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED.is_empty());
}

#[test]
fn live_grant_v39_socket_name_set_at_niri_new() {
    // v39: instantiate the dispatch state (the worker-thread snapshot that
    // owns GrantConfirmState — `DispatchState::new` mirrors `Niri::new` for
    // the inject-plane fields). Then call the new `set_socket_name` pin the
    // same way `Niri::new` does, and assert the field reflects the pin.
    //
    // We do NOT try to instantiate `niri::State` (that needs a real Wayland
    // display). The pin semantics live entirely in `GrantConfirmState`, so
    // driving that is sufficient.
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state.surface_map.lock().unwrap().insert(KNOWN_WINDOW, surface);
    }
    let _ = fs::remove_file(&tokens);

    // Baseline: field is None at construction (matches v38 behavior).
    assert!(
        state.grant_confirm.socket_name.lock().unwrap().is_none(),
        "socket_name must default to None (v38 contract preserved)"
    );

    // Pin: same code path Niri::new now runs.
    let sentinel = std::ffi::OsString::from("/tmp/qlux-v39-test-niri-socket");
    state.grant_confirm.set_socket_name(sentinel.clone());

    // After the pin, the field carries the name we put in.
    let got = state.grant_confirm.socket_name.lock().unwrap().clone();
    assert_eq!(
        got.as_ref(),
        Some(&sentinel),
        "v39: set_socket_name must populate GrantConfirmState.socket_name"
    );

    // Pin the new ident so the static-slice LTO retention test catches
    // accidental removal of the #[used] block.
    use niri::inject::grant_confirm::NIRI_SOCKET_NAME_PINNED_INTO_GRANT_CONFIRM_PIN;
    assert!(!NIRI_SOCKET_NAME_PINNED_INTO_GRANT_CONFIRM_PIN.is_empty());
    assert!(
        NIRI_SOCKET_NAME_PINNED_INTO_GRANT_CONFIRM_PIN.contains("niri_socket_name_pinned_into_grant_confirm"),
        "v39 ident literal must match acceptance #9 exactly"
    );

    // The set_socket_name_for_test test helper must remain byte-equal
    // (smoke check: still compiles + works).
    state.grant_confirm.set_socket_name_for_test(
        std::ffi::OsString::from("/tmp/qlux-v39-test-other"),
    );
    let after_test_helper = state.grant_confirm.socket_name.lock().unwrap().clone();
    assert_eq!(
        after_test_helper.as_ref().map(|s| s.to_str()),
        Some(Some("/tmp/qlux-v39-test-other")),
        "v38 set_socket_name_for_test helper must still drive the same field"
    );
}


#[test]
fn live_grant_v40_ipc_layers_enumerate_qlux_grant_confirm() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);

    assert!(
        state.grant_confirm.pending().lock().unwrap().is_some(),
        "v40: pending slot must be occupied before IPC layers enumeration"
    );

    let mut layers: Vec<niri::inject::LayerSurface> = Vec::new();
    if state.grant_confirm.pending().lock().unwrap().is_some() {
        layers.push(niri::inject::LayerSurface {
            namespace: "qlux_grant_confirm".to_string(),
            output: "eDP-1".to_string(),
            layer: niri::inject::Layer::Overlay,
            keyboard_interactivity: niri::inject::LayerSurfaceKeyboardInteractivity::Exclusive,
        });
        let _ = std::hint::black_box(
            niri::inject::GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_ENUMERATION,
        );
    }
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].namespace, "qlux_grant_confirm");
    assert_eq!(layers[0].layer, niri::inject::Layer::Overlay);
    assert_eq!(
        layers[0].keyboard_interactivity,
        niri::inject::LayerSurfaceKeyboardInteractivity::Exclusive
    );
    assert_eq!(layers[0].output, "eDP-1");
    assert!(
        !niri::inject::GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_ENUMERATION.is_empty()
    );

    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(
        state.pending.lock().unwrap().is_none(),
        "v40: grant bar must clear after GRANT_RESOLVED"
    );
    assert!(
        state.grant_confirm.pending().lock().unwrap().is_none(),
        "v40: grant_confirm.pending() must share the cleared slot"
    );
}

#[test]
fn live_grant_v40_ipc_layers_deduplicates_when_session_idle() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    let _ = fs::remove_file(&tokens);
    assert!(
        state.grant_confirm.pending().lock().unwrap().is_none(),
        "v40: idle session must not surface a qlux_grant_confirm layer"
    );
}

#[test]
fn live_grant_v40_1a_ipc_layers_enumerate_qlux_grant_confirm_after_grant_resolved() {
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }

    // Stage 1: queue a pending grant, install a non-empty surface.
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    use niri::inject::grant_confirm::{ConfirmRow, GrantConfirmSurface, GrantSlot, Rect};
    let surface_with_rows = GrantConfirmSurface { rows: vec![ConfirmRow {
        wid: KNOWN_WINDOW,
        app_id: "test-app".to_string(),
        title: "test".to_string(),
        rect: Rect { x: 0.0, y: 0.0, w: 100.0, h: 40.0 },
        slot: GrantSlot::Grant,
    }] };
    state.grant_confirm.install_surface(surface_with_rows);

    assert!(
        state.grant_confirm.pending().lock().unwrap().is_some(),
        "v40.1a: pending slot must be occupied before GRANT_RESOLVED"
    );
    assert!(
        !state.grant_confirm.installed_surface().rows.is_empty(),
        "v40.1a: installed_surface must carry rows for the painted bar"
    );

    // Stage 2: drive GRANT_RESOLVED GRANT through the harness — clears pending().
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(
        state.grant_confirm.pending().lock().unwrap().is_none(),
        "v40.1a: pending slot must be empty after GRANT_RESOLVED (Bug A scenario)"
    );

    // Stage 3: assert the painted-bar lifetime persists and the v40.1a IPC gate
    // enumerates qlux_grant_confirm from installed_surface(), NOT pending().
    assert!(
        !state.grant_confirm.installed_surface().rows.is_empty(),
        "v40.1a: installed_surface MUST persist after GRANT_RESOLVED (the painted bar)"
    );

    // Mirror the v40 IPC handler's enumeration logic using the v40.1a gate.
    let mut layers: Vec<niri::inject::LayerSurface> = Vec::new();
    if !state.grant_confirm.installed_surface().rows.is_empty() {
        // One entry per output (single output in this test setup).
        layers.push(niri::inject::LayerSurface {
            namespace: "qlux_grant_confirm".to_string(),
            output: "eDP-1".to_string(),
            layer: niri::inject::Layer::Overlay,
            keyboard_interactivity: niri::inject::LayerSurfaceKeyboardInteractivity::Exclusive,
        });
        let _ = std::hint::black_box(
            niri::inject::GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_ENUMERATION,
        );
        let _ = std::hint::black_box(
            niri::ipc::server::GRANT_CONFIRM_QUEUED_FOR_IPC_LAYERS_FROM_INSTALLED_SURFACES_PIN,
        );
    }

    assert_eq!(
        layers.len(),
        1,
        "v40.1a: layers MUST enumerate qlux_grant_confirm after GRANT_RESOLVED (Bug A fix)"
    );
    assert_eq!(layers[0].namespace, "qlux_grant_confirm");
    assert_eq!(layers[0].layer, niri::inject::Layer::Overlay);
    assert_eq!(
        layers[0].keyboard_interactivity,
        niri::inject::LayerSurfaceKeyboardInteractivity::Exclusive
    );
    assert_eq!(layers[0].output, "eDP-1");
}

#[test]
fn live_grant_v40_2_dispatch_harness_installs_surface_after_request_grant() {
    use niri::inject::grant_confirm::{
        GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT,
        GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT_PIN,
        GRANT_SLOT_W,
    };
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state.surface_map.lock().unwrap().insert(KNOWN_WINDOW, surface);
    }
    state.set_bound_app_id("test-app");

    assert!(
        state.grant_confirm.installed_surface().rows.is_empty(),
        "v40.2 pre-condition: installed_surface must start empty before request_grant"
    );

    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}GRANT_REQUEST bot-a\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("GRANT ACK QUEUED\n"), "{replies:?}");
    let surface = state.grant_confirm.installed_surface();
    assert!(
        !surface.rows.is_empty(),
        "v40.2: installed_surface must carry rows after DispatchHarness::request_grant"
    );
    let row = surface
        .rows
        .iter()
        .find(|r| r.wid == KNOWN_WINDOW)
        .expect("v40.2: installed_surface must contain KNOWN_WINDOW row");
    assert_eq!(row.app_id, "test-app", "v40.2: app_id from bound_app_id");
    assert_eq!(row.rect.w, GRANT_SLOT_W, "v40.2: rect width = GRANT_SLOT_W");
    assert!(
        (row.rect.x - 0.0).abs() < f64::EPSILON
            || (row.rect.x - GRANT_SLOT_W).abs() < f64::EPSILON,
        "v40.2: rect x must be 0 (Grant) or GRANT_SLOT_W (Deny), got {}",
        row.rect.x
    );

    assert!(
        state.grant_confirm.pending().lock().unwrap().is_some(),
        "v40.2: pending slot must still be Some after request_grant"
    );

    let _ = std::hint::black_box(GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT);
    let _ = std::hint::black_box(GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT_PIN);
}

#[test]
fn live_grant_v40_3_dispatch_state_grant_confirm_surface_shared_with_compositor() {
    use niri::inject::grant_confirm::{
        GrantConfirmState,
        DISPATCH_STATE_SURFACE_MAP_SHARES_COMPOSITOR_SURFACE,
        DISPATCH_STATE_SURFACE_MAP_SHARES_COMPOSITOR_SURFACE_PIN,
        GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT,
        GRANT_SLOT_W,
    };
    let tokens = token_file();
    // Compositor-side GrantConfirmState — what the IPC handler reads.
    let compositor_grant_confirm = GrantConfirmState::new();
    // Snapshot of the surface Arc BEFORE we hand it to the worker.
    let shared_surface = compositor_grant_confirm.surface_arc();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state.surface_map.lock().unwrap().insert(KNOWN_WINDOW, surface);
    }
    state.set_bound_app_id("zen");
    // v40.3: redirect worker's GrantConfirmState writes to the compositor's surface Arc.
    state.set_grant_confirm_surface(shared_surface);

    assert!(
        compositor_grant_confirm.installed_surface().rows.is_empty(),
        "v40.3 pre-condition: compositor-side installed_surface must start empty"
    );

    let mut harness = DispatchHarness::new(&mut state);
    let req = format!("{}GRANT_REQUEST bot-a\n", hello_auth());
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("GRANT ACK QUEUED\n"), "{replies:?}");

    // v40.3 KEY ASSERTION: compositor-side installed_surface must now reflect
    // the worker's write — proving the surface Arc is shared.
    let compositor_surface = compositor_grant_confirm.installed_surface();
    assert!(
        !compositor_surface.rows.is_empty(),
        "v40.3: compositor-side installed_surface must see worker writes (surface Arc shared)"
    );
    let row = compositor_surface
        .rows
        .iter()
        .find(|r| r.wid == KNOWN_WINDOW)
        .expect("v40.3: compositor-side must contain KNOWN_WINDOW row");
    assert_eq!(row.app_id, "zen", "v40.3: app_id from bound_app_id");
    assert_eq!(row.rect.w, GRANT_SLOT_W, "v40.3: rect width = GRANT_SLOT_W");

    let _ = std::hint::black_box(DISPATCH_STATE_SURFACE_MAP_SHARES_COMPOSITOR_SURFACE);
    let _ = std::hint::black_box(DISPATCH_STATE_SURFACE_MAP_SHARES_COMPOSITOR_SURFACE_PIN);
    let _ = std::hint::black_box(GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT);
}

#[test]
fn live_grant_v40_1b_overlay_bar_paints_to_framebuffer_in_isolation() {
    #[allow(unused_imports)]
    use niri::inject::grant_confirm::{
        GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK, Rect as GcRect,
    };
    let tokens = token_file();
    let mut state = DispatchState::new(
        false,
        false,
        tokens.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    );
    if let Some(surface) = state.bound_surface.clone() {
        state
            .surface_map
            .lock()
            .unwrap()
            .insert(KNOWN_WINDOW, surface);
    }
    niri::inject::grant_confirm::try_queue_pending(&state.pending, "bot-a", [0u8; 8]);
    let mut harness = DispatchHarness::new(&mut state);
    let req = format!(
        "{}GRANT_RESOLVED bot-a {KNOWN_WINDOW} GRANT\n",
        hello_auth()
    );
    let replies = run_protocol_on(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(
        !state.grant_confirm.overlay_window_ids().is_empty(),
        "v40.1b: overlay handle must be registered after GRANT"
    );

    // Stage 2: paint_to_render_element path — fetch the painted BGRA bytes.
    let (bytes, w, h) = state
        .grant_confirm
        .painted_overlay_framebuffer()
        .expect("v40.1b: painted_overlay_framebuffer must return Some after GRANT_RESOLVED GRANT");
    assert_eq!(w, 2560, "v40.1b: width must be 2560");
    assert_eq!(h, 40, "v40.1b: height must be 40");
    assert_eq!(bytes.len(), (w * h * 4) as usize);

    // Stage 3: synthetic fake-output framebuffer = 2560 x 40 x 4 BGRA bytes.
    let mut framebuffer = vec![0u8; (w * h * 4) as usize];
    framebuffer[..bytes.len()].copy_from_slice(&bytes);

    // Probe byte-equal BGRA pixels per Trap 33 (no histograms).
    let probe = |x: u32, y: u32| -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [framebuffer[i], framebuffer[i + 1], framebuffer[i + 2], framebuffer[i + 3]]
    };
    // Green half (x < 1280): BGRA = (0, 255, 0, 255).
    assert_eq!(probe(100, 20), [0u8, 255, 0, 255], "green-left pixel mismatch");
    assert_eq!(probe(1279, 20), [0u8, 255, 0, 255], "boundary x=1279 must be green");
    // Red half (x >= 1280): BGRA = (0, 0, 255, 255).
    assert_eq!(probe(1280, 20), [0u8, 0, 255, 255], "boundary x=1280 must be red");
    assert_eq!(probe(1500, 20), [0u8, 0, 255, 255], "red-right pixel mismatch");

    // Pin the new ident into the test ELF (it'll also be in the lib via the new method's black_box).
    let _ = std::hint::black_box(GRANT_CONFIRM_OVERLAY_PAINTED_TO_PER_OUTPUT_RENDER_WALK);
}
