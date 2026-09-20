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
use niri::inject::smithay::reexports::wayland_server::{
    Client, Dispatch, Display, DisplayHandle,
};
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
    block_on_loop(
        &mut event_loop,
        handle_inject_client(harness, async_server),
    )
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
    let req = format!("{}BIND {KNOWN_WINDOW}\nKEY XF86XK_AudioMute\n", hello_auth());
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

    assert!(replies.contains("DISPATCH ERR OUTPUTS_OFF\n"), "{replies:?}");
    assert!(!replies.contains("DISPATCH OK\n"), "{replies:?}");
    assert!(replies.ends_with("DISPATCH ERR OUTPUTS_OFF\n"), "{replies:?}");
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
    assert!(!replies.contains("DISPATCH ERR OUTPUTS_OFF\n"), "{replies:?}");
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
    let req = format!("{}BIND {KNOWN_WINDOW}\nKEY XF86XK_AudioMute\n", hello_auth());
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
    assert!(
        replies.ends_with("DISPATCH ERR NOT_BOUND\n"),
        "{replies:?}"
    );
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
    assert_eq!(rec.motion_count, 2, "drag fires pointer.motion twice (from then to)");
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
    assert!(niri::inject::grant_confirm::is_blocked_app_id("hermes-desktop"));
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
    assert!(niri::inject::grant_confirm::is_blocked_app_id("hermes-desktop"));
    assert!(niri::inject::grant_confirm::is_blocked_app_id("niri-config"));
    assert!(niri::inject::grant_confirm::is_blocked_app_id("niri-ipc"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id("zen"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id("gnome-calculator"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id("Alacritty"));
    assert!(!niri::inject::grant_confirm::is_blocked_app_id("org.gnome.Nautilus"));
}
