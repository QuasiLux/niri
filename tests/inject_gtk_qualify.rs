//! Row 8 slice B: qualify ONE GTK app via the inject plane.
//!
//! Registers a synthetic WlSurface tagged with `app_id="gnome-calculator"` and
//! drives HELLO → AUTH → BIND → CLICK → KEY → SCROLL through
//! `handle_inject_client`. The agent seat must receive pointer motion, key
//! dispatch, and scroll axis; the user seat's `FocusedWindow` and
//! `scroll_offset` must stay byte-equal to their pre-dispatch values.
//!
//! Mirrors the slice A pattern at `tests/inject_zen_qualify.rs:1-482` (and the
//! v3 base at `tests/inject.rs:1-507`). Card §10 forbids editing
//! `tests/inject.rs` / `tests/inject_zen_qualify.rs`, so this file inlines a
//! private Harness with the four card §2 fields (`app_id`, `bound_app_id`,
//! `known_app_id`, `_surface_keep`) plus the GTK resolution constants.
//!
//! Resolved GTK app_id:
//! - `gnome-calculator` (Calc, GNOME upstream `org.gnome.Calculator.desktop`
//!   `StartupWMClass=gnome-calculator`).
//! - Picked from card §1 candidates list (first option); architecture doc
//!   `wiki/raw/articles/2026-09-09-niri-hud-isolated-computer-use-architecture.md`
//!   §1 names Calc as one of the "qualified raw background" GTK3/Qt targets
//!   ("Calc + Inkscape only").
//! - This sandbox (builder docker) cannot reach the host over SSH to probe
//!   live TTY1 (port 22 refused); slice A similarly inferred `app_id="zen"`
//!   from `wiki/raw/articles/2026-09-12-niri-spare-tty3-zen-window-cast-proof.md`
//!   instead of running `niri msg --json windows` directly. Same read-only
//!   inspection pattern, applied to the GTK-side catalog.

use std::fs;
use std::future::Future;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use niri::inject::calloop::EventLoop;
use niri::inject::handle_inject_client;
use niri::inject::smithay::backend::input::{InputTime, KeyState};
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

// Resolved GTK app_id. See module docstring for derivation path.
// Card §2 acceptance requires substituting the resolved app_id into the test
// function names below.
const GTK_APP_ID: &str = "gnome-calculator";
const GTK_WINDOW: u64 = 7_888;
const TOKEN_GTK: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const USER_FOCUS: Option<u64> = Some(99);
const USER_SCROLL: (f64, f64) = (1.5, 2.5);

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct GtkTarget {
    motion_count: u32,
    last_motion: Option<Point<f64, Logical>>,
    key_count: u32,
    keys: Vec<String>,
    axis_count: u32,
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

/// Same shape as `tests/inject.rs` `dummy_surface` and slice A's
/// `tests/inject_zen_qualify.rs` `SurfaceKeep`: Display + client stream +
/// `WlSurface` token. App id is a Harness field (synthetic); Smithay does not
/// expose xdg-shell `set_app_id` on this token.
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

/// Slice B Harness: mirrors slice A's `ZenHarness` field-by-field, substituting
/// GTK constants. The four card §2 extra fields beyond slice A's `ZenHarness`
/// are all present here (`app_id`, `bound_app_id`, `known_app_id`,
/// `_surface_keep`). Field ordering matches slice A's `ZenHarness` so a
/// line-by-line diff against `tests/inject_zen_qualify.rs:114-153` shows only
/// the GTK-specific renames + value changes.
struct GtkHarness {
    seat_state: SeatState<Self>,
    agent_seat: AgentSeat<Self>,
    user_focused_window: Option<u64>,
    user_scroll_offset: Point<f64, Logical>,
    bound_surface: Option<WlSurface>,
    target: Arc<Mutex<GtkTarget>>,
    token_path: PathBuf,
    known_window: u64,
    app_id: String,
    bound_app_id: String,
    known_app_id: &'static str,
    transient_errors: Vec<String>,
    permanent_errors: Vec<String>,
    _surface_keep: SurfaceKeep,
}

impl GtkHarness {
    fn new(token_path: PathBuf) -> Self {
        let keep = dummy_surface();
        let mut seat_state = SeatState::new();
        let agent_seat = AgentSeat::create(&mut seat_state);
        GtkHarness {
            seat_state,
            agent_seat,
            user_focused_window: USER_FOCUS,
            user_scroll_offset: Point::from(USER_SCROLL),
            bound_surface: Some(keep.surface.clone()),
            target: Arc::new(Mutex::new(GtkTarget::default())),
            token_path,
            known_window: GTK_WINDOW,
            app_id: GTK_APP_ID.to_string(),
            bound_app_id: String::new(),
            known_app_id: GTK_APP_ID,
            transient_errors: Vec::new(),
            permanent_errors: Vec::new(),
            _surface_keep: keep,
        }
    }
}

impl IsAlive for Focus {
    fn alive(&self) -> bool {
        true
    }
}

impl PointerTarget<GtkHarness> for Focus {
    fn enter(&self, seat: &Seat<GtkHarness>, data: &mut GtkHarness, event: &MotionEvent) {
        PointerTarget::motion(self, seat, data, event);
    }
    fn motion(&self, _seat: &Seat<GtkHarness>, data: &mut GtkHarness, event: &MotionEvent) {
        let mut rec = data.target.lock().unwrap();
        rec.motion_count += 1;
        rec.last_motion = Some(event.location);
    }
    fn relative_motion(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &RelativeMotionEvent,
    ) {
    }
    fn button(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _event: &ButtonEvent) {}
    fn axis(&self, _seat: &Seat<GtkHarness>, data: &mut GtkHarness, _frame: AxisFrame) {
        data.target.lock().unwrap().axis_count += 1;
    }
    fn frame(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness) {}
    fn leave(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _serial: Serial, _time: InputTime) {}
    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl KeyboardTarget<GtkHarness> for Focus {
    fn enter(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _keys: Vec<KeysymHandle<'_>>,
        _serial: Serial,
    ) {
    }
    fn leave(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _serial: Serial) {}
    fn key(
        &self,
        _seat: &Seat<GtkHarness>,
        data: &mut GtkHarness,
        _key: KeysymHandle<'_>,
        _state: KeyState,
        _serial: Serial,
        _time: InputTime,
    ) {
        data.target.lock().unwrap().key_count += 1;
    }
    fn modifiers(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _modifiers: ModifiersState,
        _serial: Serial,
    ) {
    }
}

impl TouchTarget<GtkHarness> for Focus {
    fn down(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _event: &DownEvent) {}
    fn up(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _event: &UpEvent) {}
    fn motion(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _event: &TouchMotionEvent) {}
    fn frame(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _marker: FrameMarker) {}
    fn cancel(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _marker: FrameMarker) {}
    fn shape(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness, _event: &ShapeEvent) {}
    fn orientation(
        &self,
        _seat: &Seat<GtkHarness>,
        _data: &mut GtkHarness,
        _event: &OrientationEvent,
    ) {
    }
    fn last_frame(&self, _seat: &Seat<GtkHarness>, _data: &mut GtkHarness) -> Option<FrameMarker> {
        None
    }
}

impl SeatHandler for GtkHarness {
    type KeyboardFocus = Focus;
    type PointerFocus = Focus;
    type TouchFocus = Focus;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl InjectHarness for GtkHarness {
    type AgentState = Self;

    fn agent_seat(&mut self) -> &mut AgentSeat<Self> {
        &mut self.agent_seat
    }

    fn agent_state(&mut self) -> &mut Self {
        self
    }

    fn resolve_window(&mut self, window_id: u64) -> Option<WlSurface> {
        // Slice B: app_id check at the harness boundary (production would
        // inspect xdg-shell toplevel's app_id of the mapped window and gate
        // BIND on `app_id == known_app_id`). Mirrors slice A's gating at
        // `tests/inject_zen_qualify.rs:312-321`.
        if window_id == self.known_window && self.app_id == self.known_app_id {
            self.bound_app_id = self.app_id.clone();
            self.bound_surface.clone()
        } else {
            None
        }
    }

    fn resolve_pointer_focus(&mut self, window_id: u64) -> Option<Focus> {
        if window_id == self.known_window && self.app_id == self.known_app_id {
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
        let mut rec = self.target.lock().unwrap();
        rec.key_count += 1;
        rec.keys.push(xkb_name.to_string());
        true
    }
}

fn token_file() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(format!(
        "/tmp/inject_gtk_test_tokens-{}-{nanos}-{seq}",
        std::process::id()
    ));
    fs::write(&path, format!("gtk-bot-1:{TOKEN_GTK}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn hello_auth() -> String {
    format!("HELLO\nAUTH\n{TOKEN_GTK}\n")
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

fn run_protocol(harness: &mut GtkHarness, requests: &[u8]) -> String {
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

fn assert_user_seat_byte_eq(
    harness: &GtkHarness,
    focus: Option<u64>,
    scroll: Point<f64, Logical>,
) {
    assert_eq!(harness.user_focused_window, focus);
    assert_eq!(
        harness.user_scroll_offset.x.to_bits(),
        scroll.x.to_bits(),
        "user scroll x mutated"
    );
    assert_eq!(
        harness.user_scroll_offset.y.to_bits(),
        scroll.y.to_bits(),
        "user scroll y mutated"
    );
}

#[test]
fn gnome_calculator_binds_and_clicks_under_agent_seat() {
    let tokens = token_file();
    let mut harness = GtkHarness::new(tokens.clone());
    let req = format!(
        "{}BIND {GTK_WINDOW}\nCLICK 240 320\nKEY Tab\n",
        hello_auth()
    );
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(replies.contains("HELLO OK\n"), "{replies:?}");
    assert!(replies.contains("AUTH OK gtk-bot-1\n"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert_eq!(
        replies.matches("DISPATCH OK\n").count(),
        2,
        "{replies:?}"
    );

    let rec = harness.target.lock().unwrap();
    assert_eq!(rec.motion_count, 1);
    assert_eq!(rec.last_motion, Some(Point::from((240.0, 320.0))));
    assert_eq!(rec.key_count, 1);
    assert_eq!(rec.keys, ["Tab"]);
    drop(rec);
    assert_eq!(harness.bound_app_id, GTK_APP_ID);
    assert_eq!(harness.known_app_id, GTK_APP_ID);
    assert_eq!(harness.app_id, GTK_APP_ID);
}

#[test]
fn gnome_calculator_does_not_mutate_user_focus_or_scroll_offset() {
    let tokens = token_file();
    let mut harness = GtkHarness::new(tokens.clone());
    let focus_before = harness.user_focused_window;
    let scroll_before = harness.user_scroll_offset;
    let req = format!(
        "{}BIND {GTK_WINDOW}\nCLICK 240 320\nKEY Tab\nSCROLL 5 -8\n",
        hello_auth()
    );
    let replies = run_protocol(&mut harness, req.as_bytes());
    let _ = fs::remove_file(&tokens);

    assert!(
        !replies.contains("DISPATCH ERR OCCLUDED"),
        "{replies:?}"
    );
    assert!(!replies.contains("DISPATCH ERR LOCKED"), "{replies:?}");
    assert!(replies.contains("BIND OK\n"), "{replies:?}");
    assert!(replies.contains("DISPATCH OK\n"), "{replies:?}");

    assert_eq!(harness.target.lock().unwrap().axis_count, 1);
    assert_user_seat_byte_eq(&harness, focus_before, scroll_before);
    assert_eq!(harness.bound_app_id, GTK_APP_ID);
}