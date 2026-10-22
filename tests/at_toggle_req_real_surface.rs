//! v40.33: `AT_TOGGLE_REQ` must click the real client `WlSurface`.
//!
//! Production path is `State::dispatch_at_toggle_request` →
//! `NiriInjectHarness::at_toggle_req` (`PointerFocus = WlSurface`) via the
//! user seat. `DispatchHarness` (`PointerFocus = DispatchFocus`) is
//! test-only; a stub press does not satisfy the gate.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use niri::inject::atspi_action::{set_action_override, ActionProbe, ToggleOutcome};
use niri::inject::atspi_ladder::{set_observe_toggle_override, set_scan_override, ElementBounds};
use niri::inject::dispatch::{
    inject_agent_button_click, toggle_state_from_outcome, DispatchFocus, DispatchHarness,
    DispatchState,
};
use niri::inject::protocol::{InjectHarness, ToggleReply, ToggleState};
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
use niri::inject::smithay::reexports::wayland_server::{Client, Dispatch, Display, DisplayHandle, Resource};
use niri::inject::smithay::utils::{IsAlive, Logical, Point, Serial};
use niri::inject::{AgentSeat, Rect};

const KNOWN_WID: u64 = 42;
const UNKNOWN_WID: u64 = 99;
const TOGGLE_NAME: &str = "Mute";
const RECT_X: f64 = 10.0;
const RECT_Y: f64 = 20.0;
const RECT_W: f64 = 40.0;
const RECT_H: f64 = 60.0;

fn element_center() -> (f64, f64) {
    (RECT_X + RECT_W / 2.0, RECT_Y + RECT_H / 2.0)
}

thread_local! {
    static REAL_SURFACE_BUTTONS: Cell<u32> = const { Cell::new(0) };
}

fn reset_real_surface_buttons() {
    REAL_SURFACE_BUTTONS.with(|c| c.set(0));
}

fn real_surface_buttons() -> u32 {
    REAL_SURFACE_BUTTONS.with(|c| c.get())
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

fn fixture_bounds(wid: u64, _role: &str, _name: &str, _index: u32) -> Option<ElementBounds> {
    if wid != KNOWN_WID {
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
        dbus_dest: String::new(),
        dbus_path: String::new(),
    })
}

#[derive(Default)]
struct ClickRecord {
    motion_count: u32,
    button_count: u32,
    last_motion: Option<Point<f64, Logical>>,
    button_locations: Vec<Point<f64, Logical>>,
}

/// Pointer focus that *is* the window's `WlSurface`, with recording.
/// Distinct from `DispatchFocus` (the worker stub whose `button()` is empty).
#[derive(Debug, Clone, PartialEq)]
struct RealFocus {
    surface: WlSurface,
}

impl IsAlive for RealFocus {
    fn alive(&self) -> bool {
        true
    }
}

impl PointerTarget<RealSurfaceState> for RealFocus {
    fn enter(&self, seat: &Seat<RealSurfaceState>, data: &mut RealSurfaceState, event: &MotionEvent) {
        PointerTarget::motion(self, seat, data, event);
    }
    fn motion(&self, _seat: &Seat<RealSurfaceState>, data: &mut RealSurfaceState, event: &MotionEvent) {
        let mut rec = data.record.lock().unwrap();
        rec.motion_count += 1;
        rec.last_motion = Some(event.location);
    }
    fn relative_motion(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &RelativeMotionEvent,
    ) {
    }
    fn button(&self, _seat: &Seat<RealSurfaceState>, data: &mut RealSurfaceState, _event: &ButtonEvent) {
        REAL_SURFACE_BUTTONS.with(|c| c.set(c.get() + 1));
        let mut rec = data.record.lock().unwrap();
        rec.button_count += 1;
        let loc = rec.last_motion.unwrap_or_else(|| Point::from((0.0, 0.0)));
        rec.button_locations.push(loc);
    }
    fn axis(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _frame: AxisFrame) {}
    fn frame(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState) {}
    fn leave(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _serial: Serial,
        _time: InputTime,
    ) {
    }
    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl KeyboardTarget<RealSurfaceState> for RealFocus {
    fn enter(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _keys: Vec<KeysymHandle<'_>>,
        _serial: Serial,
    ) {
    }
    fn leave(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _serial: Serial) {}
    fn key(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _key: KeysymHandle<'_>,
        _state: KeyState,
        _serial: Serial,
        _time: InputTime,
    ) {
    }
    fn modifiers(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _modifiers: ModifiersState,
        _serial: Serial,
    ) {
    }
}

impl TouchTarget<RealSurfaceState> for RealFocus {
    fn down(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _event: &DownEvent) {}
    fn up(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _event: &UpEvent) {}
    fn motion(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &TouchMotionEvent,
    ) {
    }
    fn frame(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _marker: FrameMarker) {}
    fn cancel(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _marker: FrameMarker) {
    }
    fn shape(&self, _seat: &Seat<RealSurfaceState>, _data: &mut RealSurfaceState, _event: &ShapeEvent) {}
    fn orientation(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
        _event: &OrientationEvent,
    ) {
    }
    fn last_frame(
        &self,
        _seat: &Seat<RealSurfaceState>,
        _data: &mut RealSurfaceState,
    ) -> Option<FrameMarker> {
        None
    }
}

struct RealSurfaceState {
    seat_state: SeatState<Self>,
    agent_seat: AgentSeat<Self>,
    windows: HashMap<u64, WlSurface>,
    record: Arc<Mutex<ClickRecord>>,
    last_focus_surface: Option<WlSurface>,
    _keep: SurfaceKeep,
}

impl RealSurfaceState {
    fn new() -> Self {
        let keep = dummy_surface();
        let mut seat_state = SeatState::new();
        let agent_seat = AgentSeat::create(&mut seat_state);
        let mut windows = HashMap::new();
        windows.insert(KNOWN_WID, keep.surface.clone());
        Self {
            seat_state,
            agent_seat,
            windows,
            record: Arc::new(Mutex::new(ClickRecord::default())),
            last_focus_surface: None,
            _keep: keep,
        }
    }
}

impl SeatHandler for RealSurfaceState {
    type KeyboardFocus = RealFocus;
    type PointerFocus = RealFocus;
    type TouchFocus = RealFocus;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

struct RealSurfaceHarness<'a> {
    state: &'a mut RealSurfaceState,
}

impl InjectHarness for RealSurfaceHarness<'_> {
    type AgentState = RealSurfaceState;

    fn agent_seat(&mut self) -> &mut AgentSeat<Self::AgentState> {
        &mut self.state.agent_seat
    }

    fn agent_state(&mut self) -> &mut Self::AgentState {
        self.state
    }

    fn resolve_window(&mut self, window_id: u64) -> Option<WlSurface> {
        self.state.windows.get(&window_id).cloned()
    }

    fn resolve_pointer_focus(&mut self, window_id: u64) -> Option<RealFocus> {
        self.resolve_window(window_id).map(|surface| RealFocus { surface })
    }

    fn token_file_path(&self) -> &std::path::Path {
        std::path::Path::new("/dev/null")
    }

    /// Mirrors `NiriInjectHarness::at_toggle_req`: `Some((window surface, pt))`
    /// into `inject_agent_button_click`. `PointerFocus = RealFocus` wraps the
    /// requested window's `WlSurface`; `DispatchFocus` cannot be passed here.
    /// After a successful click the reply is `observe_toggle_checked` on the
    /// bounds dest/path, not `unchecked()` merely because `clicked` is true.
    fn at_toggle_req(&mut self, wid: u64, name: &str, index: Option<u32>) -> ToggleReply {
        if wid == 0 || name.trim().is_empty() {
            return ToggleReply::no_action();
        }
        let bounds = niri::inject::atspi_ladder::element_lookup_via_tree_dump(
            wid,
            "toggle button",
            name,
            index.unwrap_or(0),
        )
        .or_else(|| {
            niri::inject::atspi_ladder::scan_session_dbus_for_element(
                wid,
                "toggle button",
                name,
                index.unwrap_or(0),
            )
        });
        let Some(b) = bounds else {
            return ToggleReply::no_action();
        };
        let cx = b.rect.x + b.rect.w / 2.0;
        let cy = b.rect.y + b.rect.h / 2.0;
        let Some(surface) = self.resolve_window(wid) else {
            return ToggleReply::no_action();
        };
        self.state.last_focus_surface = Some(surface.clone());
        let focus = RealFocus {
            surface: surface.clone(),
        };
        let pointer = self.agent_seat().pointer.clone();
        // Surface origin (0,0): AT-SPI rect is surface-relative, so the
        // PointerTarget sees MotionEvent.location == element center.
        let clicked = inject_agent_button_click(
            &pointer,
            self.agent_state(),
            Some((focus, Point::from((0.0, 0.0)))),
            cx,
            cy,
        );
        if !clicked {
            return ToggleReply::no_action();
        }
        // Click dispatched is not an observed GTK checked-bit. None means
        // unobserved and the wire reply is `no_action` on purpose (honesty
        // over convenience).
        match niri::inject::atspi_ladder::observe_toggle_checked(&b.dbus_dest, &b.dbus_path) {
            Some(true) => ToggleReply::checked(),
            Some(false) => ToggleReply::unchecked(),
            None => ToggleReply::no_action(),
        }
    }
}

/// Compile-time gate: production `State::PointerFocus` is `WlSurface`.
/// Wiring `DispatchHarness` (`PointerFocus = DispatchFocus`) into
/// `NiriInjectHarness::at_toggle_req`'s `Some((surface, pt))` cannot type-check.
fn assert_production_pointer_focus_is_wl_surface() {
    fn assert_wl<T: SeatHandler<PointerFocus = WlSurface>>() {}
    assert_wl::<niri::niri::State>();
}

fn assert_dispatch_pointer_focus_is_stub() {
    fn assert_stub<T: SeatHandler<PointerFocus = DispatchFocus>>() {}
    assert_stub::<DispatchState>();
}

fn with_scan_override<R>(f: impl FnOnce() -> R) -> R {
    set_scan_override(Some(fixture_bounds));
    let out = f();
    set_scan_override(None);
    out
}

fn observe_checked(_dest: &str, _path: &str) -> Option<bool> {
    Some(true)
}

fn observe_unchecked(_dest: &str, _path: &str) -> Option<bool> {
    Some(false)
}

fn with_observe_override<R>(f: fn(&str, &str) -> Option<bool>, body: impl FnOnce() -> R) -> R {
    set_observe_toggle_override(Some(f));
    let out = body();
    set_observe_toggle_override(None);
    out
}

#[test]
fn toggled_focus_is_the_real_window_surface_not_a_stub() {
    assert_production_pointer_focus_is_wl_surface();
    reset_real_surface_buttons();
    with_scan_override(|| {
        let mut state = RealSurfaceState::new();
        let expected = state.windows.get(&KNOWN_WID).unwrap().clone();
        let mut harness = RealSurfaceHarness { state: &mut state };
        let reply = harness.at_toggle_req(KNOWN_WID, TOGGLE_NAME, None);
        // Empty dbus dest: the click ran, the checked-bit is unobserved.
        assert_eq!(reply, ToggleReply::no_action());
        let got = harness
            .state
            .last_focus_surface
            .as_ref()
            .expect("focus must be the window surface");
        assert_eq!(
            got.id(),
            expected.id(),
            "focus must be the requested window's WlSurface"
        );
        assert_eq!(
            std::any::type_name::<<RealSurfaceState as SeatHandler>::PointerFocus>(),
            std::any::type_name::<RealFocus>(),
        );
        assert_ne!(
            std::any::type_name::<<RealSurfaceState as SeatHandler>::PointerFocus>(),
            std::any::type_name::<DispatchFocus>(),
            "DispatchFocus stub must not be the harness path's focus type"
        );
        assert!(
            real_surface_buttons() > 0,
            "real-surface PointerTarget::button must run"
        );
    });
}

#[test]
fn at_toggle_dispatches_at_element_center() {
    reset_real_surface_buttons();
    let (cx, cy) = element_center();
    with_scan_override(|| {
        let mut state = RealSurfaceState::new();
        let mut harness = RealSurfaceHarness { state: &mut state };
        let reply = harness.at_toggle_req(KNOWN_WID, TOGGLE_NAME, None);
        // Empty dbus dest: the click ran, the checked-bit is unobserved.
        assert_eq!(reply, ToggleReply::no_action());
        let rec = harness.state.record.lock().unwrap();
        let loc = rec.last_motion.expect("motion must fire");
        assert_eq!(loc, Point::from((cx, cy)), "motion location is element center");
        assert!(rec.button_count >= 2, "press+release, got {}", rec.button_count);
        for pt in &rec.button_locations {
            assert_eq!(*pt, Point::from((cx, cy)), "button at element center");
        }
    });
}

#[test]
fn unknown_wid_returns_no_action_and_dispatches_nothing() {
    reset_real_surface_buttons();
    with_scan_override(|| {
        let mut state = RealSurfaceState::new();
        let mut harness = RealSurfaceHarness { state: &mut state };
        let reply = harness.at_toggle_req(UNKNOWN_WID, TOGGLE_NAME, None);
        assert_eq!(reply, ToggleReply::no_action());
        let rec = harness.state.record.lock().unwrap();
        assert_eq!(rec.button_count, 0, "unknown wid must not button()");
        assert_eq!(rec.motion_count, 0);
        assert_eq!(real_surface_buttons(), 0);
    });
}

#[test]
fn dispatch_focus_stub_does_not_satisfy_the_gate() {
    assert_dispatch_pointer_focus_is_stub();
    reset_real_surface_buttons();
    with_scan_override(|| {
        let tokens = PathBuf::from("/dev/null");
        let mut dispatch = DispatchState::new(
            false,
            false,
            tokens,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(None)),
        );
        let mut harness = DispatchHarness::new(&mut dispatch);
        let reply = harness.at_toggle_req(KNOWN_WID, TOGGLE_NAME, None);
        assert_eq!(
            reply,
            ToggleReply::no_action(),
            "worker without main-loop sender must not click DispatchFocus"
        );
        let pointer = harness.agent_seat().pointer.clone();
        let origin = Point::from((0.0, 0.0));
        let _ = inject_agent_button_click(
            &pointer,
            harness.agent_state(),
            Some((DispatchFocus, origin)),
            RECT_X,
            RECT_Y,
        );
        assert_eq!(
            real_surface_buttons(),
            0,
            "DispatchFocus press must not increment the real-surface button counter"
        );
        assert_ne!(
            std::any::type_name::<<DispatchState as SeatHandler>::PointerFocus>(),
            std::any::type_name::<WlSurface>(),
        );
    });
}

thread_local! {
    static DO_ACTION_CALLS: RefCell<u32> = const { RefCell::new(0) };
}

fn do_action_ok(_: i32) -> bool {
    true
}
fn do_action_fail(_: i32) -> bool {
    false
}
fn do_action_second_ok(_: i32) -> bool {
    DO_ACTION_CALLS.with(|c| {
        let n = *c.borrow();
        *c.borrow_mut() = n + 1;
        n >= 1
    })
}
fn grab_ok() -> bool {
    true
}
fn grab_fail() -> bool {
    false
}

#[test]
fn toggle_outcome_maps_to_toggle_state_without_inventing_checked() {
    use ToggleOutcome::*;
    assert_eq!(toggle_state_from_outcome(Flipped), ToggleState::NoAction);
    assert_eq!(
        toggle_state_from_outcome(GrabFocusFallbackUsed),
        ToggleState::NoAction
    );
    assert_eq!(
        toggle_state_from_outcome(PointerClickFallback),
        ToggleState::NoAction
    );
    assert_eq!(
        toggle_state_from_outcome(NoActionExposed),
        ToggleState::NoAction
    );
    assert_eq!(toggle_state_from_outcome(Empty), ToggleState::NoAction);

    set_action_override(Some(ActionProbe {
        names: vec!["toggle".into()],
        do_action: do_action_ok,
        grab_focus: grab_fail,
    }));
    assert_eq!(
        toggle_state_from_outcome(niri::inject::atspi_action::toggle_matched_node("d", "/p")),
        ToggleState::NoAction
    );

    DO_ACTION_CALLS.with(|c| *c.borrow_mut() = 0);
    set_action_override(Some(ActionProbe {
        names: vec!["toggle".into()],
        do_action: do_action_second_ok,
        grab_focus: grab_ok,
    }));
    assert_eq!(
        toggle_state_from_outcome(niri::inject::atspi_action::toggle_matched_node("d", "/p")),
        ToggleState::NoAction
    );

    set_action_override(Some(ActionProbe {
        names: vec!["toggle".into()],
        do_action: do_action_fail,
        grab_focus: grab_fail,
    }));
    assert_eq!(
        toggle_state_from_outcome(niri::inject::atspi_action::toggle_matched_node("d", "/p")),
        ToggleState::NoAction
    );

    set_action_override(None);
    assert_eq!(
        toggle_state_from_outcome(niri::inject::atspi_action::toggle_matched_node("", "")),
        ToggleState::NoAction
    );
}

#[test]
fn reply_is_the_observation_not_the_click() {
    reset_real_surface_buttons();
    with_scan_override(|| {
        with_observe_override(observe_checked, || {
            let mut state = RealSurfaceState::new();
            let mut harness = RealSurfaceHarness { state: &mut state };
            let reply = harness.at_toggle_req(KNOWN_WID, TOGGLE_NAME, None);
            assert_eq!(reply, ToggleReply::checked());
            assert!(
                real_surface_buttons() > 0,
                "checked reply still requires the real-surface click"
            );
        });
        reset_real_surface_buttons();
        with_observe_override(observe_unchecked, || {
            let mut state = RealSurfaceState::new();
            let mut harness = RealSurfaceHarness { state: &mut state };
            let reply = harness.at_toggle_req(KNOWN_WID, TOGGLE_NAME, None);
            assert_eq!(reply, ToggleReply::unchecked());
            assert!(
                real_surface_buttons() > 0,
                "unchecked reply still requires the real-surface click"
            );
        });
        reset_real_surface_buttons();
        set_observe_toggle_override(None);
        let bounds = fixture_bounds(KNOWN_WID, "toggle button", TOGGLE_NAME, 0).expect("fixture");
        assert!(
            bounds.dbus_dest.is_empty() && bounds.dbus_path.is_empty(),
            "fixture dest/path stay empty so a live GetState must not run"
        );
        let mut state = RealSurfaceState::new();
        let mut harness = RealSurfaceHarness { state: &mut state };
        let reply = harness.at_toggle_req(KNOWN_WID, TOGGLE_NAME, None);
        assert_eq!(reply, ToggleReply::no_action());
        assert!(
            real_surface_buttons() > 0,
            "successful click with empty dest is unobserved, not unchecked"
        );
    });
}
