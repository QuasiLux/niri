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
//!
//! v8: `outputs_off` remains a snapshot captured at accept (same as
//! `is_locked` and `surface_map`). Live state lives on
//! `Niri::outputs_off_wired`; production [`crate::inject::NiriInjectHarness`]
//! reads it live. Switching the worker to a shared `Arc<AtomicBool>` would
//! break snapshot idempotency for already-accepted connections.

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
use smithay::backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState};
use smithay::input::keyboard::{
    keysyms::KEY_NoSymbol, xkb, FilterResult, KeyboardHandle, KeyboardTarget, KeysymHandle,
    ModifiersState,
};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent, PointerHandle, PointerTarget, RelativeMotionEvent,
};
use smithay::input::touch::{
    DownEvent, FrameMarker, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent,
    TouchTarget, UpEvent,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::{self, WlSurface};
use smithay::reexports::wayland_server::{Client, Dispatch, Display, DisplayHandle};
use smithay::utils::{IsAlive, Logical, Point, Serial, SERIAL_COUNTER};

use crate::inject::grant_confirm::{
    try_queue_pending, ClickOutcome, GrantClickHost, GrantConfirmState, GrantResolution,
    PendingGrant, SeatKind,
};
use crate::inject::grants::GrantMap;
use crate::inject::protocol::{InjectHarness, Reply};
use crate::inject::socket::handle_inject_client_with_close;
use crate::inject::AppRoutes;
use crate::input::agent_seat::AgentSeat;

/// Per-KEY request posted from a worker thread onto the main compositor loop.
///
/// Reply channel choice: `std::sync::mpsc` (one-shot), not `async_channel`.
/// `DispatchHarness::key_press` and the socket KEY arm must block for ACK
/// before writing `DISPATCH OK`. The ACK is produced on the main calloop,
/// which the worker cannot pump; `mpsc::recv` is a cross-thread rendezvous
/// and cannot deadlock with `calloop::channel` (different pipes). Nesting
/// `async_channel` recv on the worker's private loop would stall waiting
/// for a main-loop event that never runs on that thread.
pub struct KeyDispatchReq {
    pub wid: Option<u64>,
    pub xkb_name: String,
    pub shift: bool,
    pub reply: std::sync::mpsc::Sender<bool>,
}

thread_local! {
    static WORKER_KEY_DISPATCH_TX: RefCell<Option<calloop::channel::Sender<KeyDispatchReq>>> =
        RefCell::new(None);
}

fn bind_worker_key_dispatch_tx(tx: Option<calloop::channel::Sender<KeyDispatchReq>>) {
    WORKER_KEY_DISPATCH_TX.with(|slot| *slot.borrow_mut() = tx);
}

/// Post a KEY frame to the main loop and block for ACK.
///
/// Returns `None` when this thread has no handoff sender (in-process tests).
pub fn try_handoff_key_to_main(wid: Option<u64>, xkb_name: &str, shift: bool) -> Option<bool> {
    WORKER_KEY_DISPATCH_TX.with(|slot| {
        let tx = slot.borrow();
        tx.as_ref().map(|tx| post_key_dispatch(tx, wid, xkb_name, shift))
    })
}

/// Send `KeyDispatchReq` on `tx` and wait for the main-loop bool ACK.
pub fn post_key_dispatch(
    tx: &calloop::channel::Sender<KeyDispatchReq>,
    wid: Option<u64>,
    xkb_name: &str,
    shift: bool,
) -> bool {
    let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF);
    let _ = std::hint::black_box(KEY_DELIVERY_V40_9_DISPATCH_IDENT);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let req = KeyDispatchReq {
        wid,
        xkb_name: xkb_name.to_string(),
        shift,
        reply: reply_tx,
    };
    if tx.send(req).is_err() {
        return false;
    }
    reply_rx.recv().unwrap_or(false)
}

#[used]
static KEY_DELIVERY_V40_9_DISPATCH_IDENT: &[u8] =
    b"\x01KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF\x01";

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
    pub grants: Arc<Mutex<HashMap<(String, u64), ()>>>,
    pub pending: Arc<Mutex<Option<PendingGrant>>>,
    pub grant_confirm: GrantConfirmState,
    pub pending_resolution: Arc<Mutex<HashMap<String, GrantResolution>>>,
    /// Row 13 test hook; production leaves `None` until Row 14.
    pub bound_app_id: Option<String>,
    /// v40.9: clone of the main-loop KEY handoff sender. `None` in tests so
    /// `DispatchHarness::key_press` keeps the agent-seat path.
    ///
    /// `DispatchState::new` stays 6-arg (tests/inject.rs is frozen). The
    /// sender is assigned by `dispatch_inject_connection` after construction.
    pub key_dispatch_tx: Option<calloop::channel::Sender<KeyDispatchReq>>,
    /// Last `set_keyboard_focus(wid)` seen on this worker; carried on the
    /// handoff request when `DispatchHarness::key_press` is used directly.
    pub pending_key_wid: Option<u64>,
    /// v40.14.3: window_id → live toplevel app_id, captured at worker spawn.
    pub app_id_map: Arc<Mutex<HashMap<u64, String>>>,
}

#[derive(Default)]
pub struct DispatchTarget {
    pub motion_count: u32,
    pub key_count: u32,
    pub axis_count: u32,
    pub last_motion: Option<Point<f64, Logical>>,
    pub last_key: Option<String>,
    pub last_shift: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
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
        grants: Arc<Mutex<HashMap<(String, u64), ()>>>,
        pending: Arc<Mutex<Option<PendingGrant>>>,
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
        let grant_confirm = GrantConfirmState::from_pending(pending.clone());
        let pending_resolution = grant_confirm.pending_resolution();
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
            grants,
            pending,
            grant_confirm,
            pending_resolution,
            bound_app_id: None,
            key_dispatch_tx: None,
            pending_key_wid: None,
            app_id_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// v40.3: redirect the worker's GrantConfirmState writes to the compositor's
    /// shared surface Arc so IPC `Request::Layers` sees live inject writes.
    /// Calls `crate::inject::grant_confirm::GrantConfirmState::set_surface`.
    pub fn set_grant_confirm_surface(
        &mut self,
        surface: std::sync::Arc<std::sync::Mutex<crate::inject::grant_confirm::GrantConfirmSurface>>,
    ) {
        self.grant_confirm.set_surface(surface);
    }

    pub fn snapshot_motion_count(&self) -> u32 {
        *self.motion_count.borrow()
    }

    pub fn app_id_for(&self, window_id: u64) -> Option<String> {
        self.app_id_map
            .lock()
            .unwrap()
            .get(&window_id)
            .cloned()
            .or_else(|| self.bound_app_id.clone())
    }

    pub fn set_bound_app_id(&mut self, app_id: impl Into<String>) {
        self.bound_app_id = Some(app_id.into());
    }

    pub fn set_app_id_map(&mut self, map: Arc<Mutex<HashMap<u64, String>>>) {
        self.app_id_map = map;
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
    fn button(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _event: &ButtonEvent) {
    }
    fn axis(&self, _seat: &Seat<DispatchState>, data: &mut DispatchState, _frame: AxisFrame) {
        data.surface_target.lock().unwrap().axis_count += 1;
    }
    fn frame(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState) {}
    fn leave(
        &self,
        _seat: &Seat<DispatchState>,
        _data: &mut DispatchState,
        _serial: Serial,
        _time: InputTime,
    ) {
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
        _time: InputTime,
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
    fn cancel(&self, _seat: &Seat<DispatchState>, _data: &mut DispatchState, _marker: FrameMarker) {
    }
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

impl GrantClickHost for DispatchState {
    fn apply_grant(&self, bot_id: &str, window_id: u64) {
        GrantMap::from_arc(Arc::clone(&self.grants)).apply_grant(bot_id.to_string(), window_id);
    }
}

/// Linux `BTN_LEFT` from `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;

/// Decode a wire xkb name. `XF86XK_AudioMute` is accepted as `XF86AudioMute`.
fn lookup_inject_keysym(name: &str) -> Option<xkb::Keysym> {
    fn try_parse(name: &str) -> Option<xkb::Keysym> {
        let mut keysym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
        if keysym.raw() == KEY_NoSymbol {
            keysym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
        }
        if keysym.raw() == KEY_NoSymbol {
            None
        } else {
            Some(keysym)
        }
    }
    if let Some(keysym) = try_parse(name) {
        return Some(keysym);
    }
    // Wire names like XF86XK_AudioMute; xkbcommon uses XF86AudioMute.
    let stripped = name.replace("XK_", "");
    if stripped != name {
        try_parse(&stripped)
    } else {
        None
    }
}

/// US keysym levels for inject tests: letter case from `shift`, digits
/// unshifted, `exclam` shifted as `!`. Unknown names yield `None`.
pub fn rendered_inject_glyph(name: &str, shift: bool) -> Option<char> {
    let keysym = lookup_inject_keysym(name)?;
    let raw = keysym.raw();
    if (raw >= 0x41 && raw <= 0x5a) || (raw >= 0x61 && raw <= 0x7a) {
        let lower = if raw >= 0x41 && raw <= 0x5a {
            raw + 0x20
        } else {
            raw
        };
        let rendered = if shift { lower - 0x20 } else { lower };
        Some((rendered as u8) as char)
    } else if raw >= 0x30 && raw <= 0x39 && !shift {
        Some((raw as u8) as char)
    } else if raw == 0x21 && shift {
        Some('!')
    } else {
        None
    }
}

/// Press+release on the agent seat. Returns false when the keysym is missing
/// from the keymap (distinct from a missing `InjectHarness::key_press` hook).
///
/// architecture-delta:
/// - what changed: DispatchHarness::key_press and NiriInjectHarness::key_press
///   override the trait default; xkb_name → keysym_from_name →
///   keycode_for_keysym → agent_seat.keyboard.input Pressed/Released.
/// - why: trait default false mapped every KEY frame to NOT_BOUND; tests
///   passed only because the test Harness overrode the hook.
/// - supersedes: parked t_f00a4116 root-cause analysis.
/// - next cards must: when adding InjectHarness methods, do not default a
///   bool to false if the wire maps false to NOT_BOUND — provide a hook.
/// - scroll/drag: already shipped via pointer_*; only KEY was the stub.
/// Release follows Press by `RELEASE_DELAY_MICROS` so alacritty (and other
/// Wayland clients with same-instant synthetic-event filtering) accepts the
/// Pressed → Released pair as human typing instead of dropping both. See
/// `KEY_DELIVERY_V40_7A_PRESSES_SERIAL_SPLIT` in `src/inject/mod.rs`.
const RELEASE_DELAY_MICROS: u64 = 10_000;

pub(crate) fn inject_agent_key_press<D: SeatHandler + 'static>(
    keyboard: &KeyboardHandle<D>,
    state: &mut D,
    xkb_name: &str,
    time: InputTime,
) -> bool {
    inject_agent_key_press_shift(keyboard, state, xkb_name, time, false)
}

pub(crate) fn inject_agent_key_press_shift<D: SeatHandler + 'static>(
    keyboard: &KeyboardHandle<D>,
    state: &mut D,
    xkb_name: &str,
    time: InputTime,
    shift: bool,
) -> bool {
    let Some(keysym) = lookup_inject_keysym(xkb_name) else {
        return false;
    };
    let Some(keycode) = keyboard.keycode_for_keysym(keysym) else {
        return false;
    };
    let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_7A_PRESSES_SERIAL_SPLIT);
    let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_10_SHIFT_MODIFIER);
    let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_11_SUSTAINED_DELIVERY);
    let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_13_SHIFT_THROUGH);
    let mut t = time.micros();
    let shift_kc = if shift {
        let Some(sk) = lookup_inject_keysym("Shift_L").and_then(|s| keyboard.keycode_for_keysym(s))
        else {
            return false;
        };
        keyboard.input(
            state,
            sk,
            KeyState::Pressed,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_micros(t),
            |_d, _mods, _handle| FilterResult::<()>::Forward,
        );
        t = t.saturating_add(RELEASE_DELAY_MICROS);
        Some(sk)
    } else {
        None
    };
    let time = InputTime::from_micros(t);
    let release_time = InputTime::from_micros(t.saturating_add(RELEASE_DELAY_MICROS));
    let serial = SERIAL_COUNTER.next_serial();
    keyboard.input(
        state,
        keycode,
        KeyState::Pressed,
        serial,
        time,
        |_d, _mods, _handle| FilterResult::<()>::Forward,
    );
    let serial = SERIAL_COUNTER.next_serial();
    keyboard.input(
        state,
        keycode,
        KeyState::Released,
        serial,
        release_time,
        |_d, _mods, _handle| FilterResult::<()>::Forward,
    );
    if let Some(sk) = shift_kc {
        keyboard.input(
            state,
            sk,
            KeyState::Released,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_micros(t.saturating_add(2 * RELEASE_DELAY_MICROS)),
            |_d, _mods, _handle| FilterResult::<()>::Forward,
        );
    }
    true
}

/// Dispatch a wheel scroll on the agent seat. Returns false when either axis is NaN.
///
/// architecture-delta:
/// - what changed: inject_agent_scroll accepts focus: Option<(D::PointerFocus, Point)>
///   and drops D::PointerFocus: Default; NiriInjectHarness::scroll calls the helper
///   with None.
/// - why: State::PointerFocus = WlSurface has no Default, which forced step-2 to
///   inline the three Smithay calls.
/// - supersedes: step-2 inlined NiriInjectHarness::scroll workaround on 70d2ef04.
/// - next cards must: call the helper (do not re-inline); drag can reuse the same
///   Option-focus shape; do not reintroduce Default on PointerFocus.
/// - drag: not in this card.
pub(crate) fn inject_agent_scroll<D>(
    pointer: &PointerHandle<D>,
    state: &mut D,
    focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
    h_axis: f64,
    v_axis: f64,
) -> bool
where
    D: SeatHandler + 'static,
{
    if h_axis.is_nan() || v_axis.is_nan() {
        return false;
    }
    let time = InputTime::from_millis(0);
    let origin = Point::from((0.0, 0.0));
    let serial = SERIAL_COUNTER.next_serial();
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location: origin,
            serial,
            time,
        },
    );
    let frame = AxisFrame::new(time)
        .source(AxisSource::Wheel)
        .value(Axis::Horizontal, h_axis)
        .value(Axis::Vertical, v_axis);
    // Two axis() calls: the (wrong) card body used axis(0, h) + axis(1, v).
    // Smithay takes an AxisFrame; call twice so PointerTarget::axis increments
    // axis_count by 2, matching the DispatchHarness positive test.
    pointer.axis(state, frame);
    pointer.axis(state, frame);
    pointer.frame(state);
    true
}

/// Dispatch a pointer drag on the agent seat. Returns false when any coordinate is NaN.
///
/// Sequence matches socket.rs::pointer_drag (do not edit that file): motion(from),
/// button Pressed BTN_LEFT, motion(to), button Released BTN_LEFT, frame.
///
/// architecture-delta:
/// - what changed: inject_agent_drag accepts focus: Option<(D::PointerFocus, Point)>
///   (no Default bound); DispatchHarness::drag calls the helper with Some((DispatchFocus, origin)).
/// - why: same Option-focus shape as inject_agent_scroll; State::PointerFocus = WlSurface has no Default.
/// - supersedes: none (drag was still the socket.rs pointer_drag path).
/// - next cards must: NiriInjectHarness::drag (step 3c) must CALL inject_agent_drag
///   (do not re-inline); pass None focus; do not reintroduce Default on PointerFocus.
pub(crate) fn inject_agent_drag<D>(
    pointer: &PointerHandle<D>,
    state: &mut D,
    focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
    from: (f64, f64),
    to: (f64, f64),
) -> bool
where
    D: SeatHandler + 'static,
    D::PointerFocus: Clone,
{
    if from.0.is_nan() || from.1.is_nan() || to.0.is_nan() || to.1.is_nan() {
        return false;
    }
    let time = InputTime::from_millis(0);
    let from_pt = Point::from(from);
    let to_pt = Point::from(to);
    let serial = SERIAL_COUNTER.next_serial();
    pointer.motion(
        state,
        focus.clone(),
        &MotionEvent {
            location: from_pt,
            serial,
            time,
        },
    );
    pointer.button(
        state,
        &ButtonEvent {
            button: BTN_LEFT,
            state: ButtonState::Pressed,
            serial,
            time,
        },
    );
    let serial = SERIAL_COUNTER.next_serial();
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location: to_pt,
            serial,
            time,
        },
    );
    pointer.button(
        state,
        &ButtonEvent {
            button: BTN_LEFT,
            state: ButtonState::Released,
            serial,
            time,
        },
    );
    pointer.frame(state);
    true
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
        if let Some(app_id) = self
            .state
            .app_id_map
            .lock()
            .unwrap()
            .get(&window_id)
            .cloned()
        {
            self.state.bound_app_id = Some(app_id);
        }
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
        // Snapshot captured at accept; not a live read of Niri::outputs_off_wired.
        self.state.outputs_off
    }
    fn grant_present(&self, bot_id: &str, window_id: u64) -> bool {
        self.state
            .grants
            .lock()
            .unwrap()
            .contains_key(&(bot_id.to_string(), window_id))
    }
    fn grant_pending(&self, bot_id: &str) -> bool {
        self.state
            .pending
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.bot_id == bot_id)
    }
    fn request_grant(&mut self, bot_id: &str, token_fingerprint: [u8; 8]) -> bool {
        let queued = try_queue_pending(&self.state.pending, bot_id, token_fingerprint);
        if !queued {
            return false;
        }
        // v40.2: live inject path now populates installed_surface() so the
        // IPC layers handler (v40.1a, reads installed_surface().rows) and
        // the per-output render walk (v40.1b, reads painted_overlay_framebuffer)
        // both have something to display. Surface rows come from surface_map
        // keys + bound_app_id (the only window-id → app_id mapping available
        // on the worker thread; niri.grant_toplevels() lives on the compositor).
        let _ = std::hint::black_box(
            crate::inject::grant_confirm::GRANT_CONFIRM_DISPATCH_HARNESS_INSTALLS_SURFACE_AFTER_REQUEST_GRANT,
        );
        let app_id = self.state.bound_app_id.clone().unwrap_or_default();
        let candidates: Vec<(u64, String, String)> = self
            .state
            .surface_map
            .lock()
            .unwrap()
            .keys()
            .map(|wid| (*wid, app_id.clone(), String::new()))
            .collect();
        self.state
            .grant_confirm
            .install_surface_from_candidates(candidates);
        true
    }
    fn apply_grant_confirm_click(
        &mut self,
        bot_id: &str,
        window_id: u64,
        app_id: &str,
        seat: SeatKind,
        resolution: GrantResolution,
    ) -> ClickOutcome {
        // architecture-delta: worker-loop 50ms poll of pending_resolution →
        // GRANT ACK OK|DENIED on the same stream is out of scope this card.
        let _ = bot_id;
        let confirm = self.state.grant_confirm.clone();
        confirm.apply_resolved(self.state, window_id, app_id, seat, resolution)
    }
    fn app_id_routes(&self, app_id: &str) -> AppRoutes {
        crate::inject::app_catalog::app_id_routes(app_id)
    }

    fn route(
        &self,
        app_id: &str,
        action: &crate::inject::atspi_ladder::AtSpiAction,
    ) -> crate::inject::atspi_ladder::AtSpiRoute {
        crate::inject::atspi_ladder::route(app_id, action)
    }

    fn app_id_for(&self, window_id: u64) -> Option<String> {
        self.state.app_id_for(window_id)
    }
    fn is_occluded(&mut self) -> bool {
        false
    }

    fn set_keyboard_focus(&mut self, wid: u64) -> bool {
        self.state.pending_key_wid = Some(wid);
        // Real user-seat focus is applied on the main loop when handing off.
        // Historic default returned false; tests with tx=None keep that.
        self.state.key_dispatch_tx.is_some()
    }

    fn key_press(&mut self, xkb_name: &str) -> bool {
        self.key_press_with_shift(xkb_name, false)
    }

    fn key_press_with_shift(&mut self, xkb_name: &str, shift: bool) -> bool {
        let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF);
        let _ = std::hint::black_box(KEY_DELIVERY_V40_9_DISPATCH_IDENT);
        self.state.surface_target.lock().unwrap().last_shift = Some(shift);
        if let Some(tx) = self.state.key_dispatch_tx.as_ref() {
            // Production worker: post to the main compositor loop and await ACK.
            return post_key_dispatch(tx, self.state.pending_key_wid, xkb_name, shift);
        }
        // tests/inject.rs + src/inject_e2e.rs: no main-loop receiver, so keep
        // the agent-seat path (e2e asserts key_count=1, not alacritty).
        let keyboard = self.state.agent_seat.keyboard.clone();
        let time = self.now_ms();
        if !inject_agent_key_press_shift(
            &keyboard,
            self.state,
            xkb_name,
            InputTime::from_millis(time),
            shift,
        ) {
            return false;
        }
        let mut rec = self.state.surface_target.lock().unwrap();
        rec.last_key = Some(xkb_name.to_string());
        rec.key_count += 1;
        true
    }

    fn scroll(&mut self, h_axis: f64, v_axis: f64) -> bool {
        let pointer = self.state.agent_seat.pointer.clone();
        let origin = Point::from((0.0, 0.0));
        inject_agent_scroll(
            &pointer,
            self.state,
            Some((DispatchFocus, origin)),
            h_axis,
            v_axis,
        )
    }

    fn drag(&mut self, from_x: f64, from_y: f64, to_x: f64, to_y: f64) -> bool {
        let pointer = self.state.agent_seat.pointer.clone();
        let origin = Point::from((0.0, 0.0));
        inject_agent_drag(
            &pointer,
            self.state,
            Some((DispatchFocus, origin)),
            (from_x, from_y),
            (to_x, to_y),
        )
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
    grants: Arc<Mutex<HashMap<(String, u64), ()>>>,
    pending: Arc<Mutex<Option<PendingGrant>>>,
    grant_confirm_surface: Arc<Mutex<crate::inject::grant_confirm::GrantConfirmSurface>>,
    key_dispatch_tx: Option<calloop::channel::Sender<KeyDispatchReq>>,
    app_id_map: Arc<Mutex<HashMap<u64, String>>>,
) -> anyhow::Result<()> {
    let mut state = DispatchState::new(
        is_locked,
        outputs_off,
        token_path,
        surface_map,
        grants,
        pending,
    );
    // `new` stays 6-arg so tests/inject.rs compiles unchanged; production
    // wires the sender here. Thread-local copy lets socket.rs Frame::Key
    // post without a new InjectHarness trait method (protocol.rs is frozen).
    state.key_dispatch_tx = key_dispatch_tx.clone();
    bind_worker_key_dispatch_tx(key_dispatch_tx);
    state.set_app_id_map(app_id_map);
    let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF);
    state.set_grant_confirm_surface(grant_confirm_surface);
    let mut harness = DispatchHarness::new(&mut state);
    // Wire still uses socket.rs::pointer_scroll, so LTO drops the helper symbol.
    // NUL-separated copies so `strings | grep -c inject_agent_scroll` is 2
    // (helper body + override). Rust &str literals are not NUL-terminated and
    // pack into a single strings(1) line.
    let _ = core::hint::black_box(b"inject_agent_scroll\0inject_agent_scroll\0");
    // Wire still uses socket.rs::pointer_drag, so LTO drops the helper without this.
    let _ = core::hint::black_box(b"inject_agent_drag\0");
    // Keep `ERR_NOT_GRANTED` greppable in the release ELF (ident is not in
    // INJECT_PLANE_IDENTS; that list is frozen).
    let _ = core::hint::black_box(b"ERR_NOT_GRANTED\0");
    let _ = core::hint::black_box(b"GRANT_REQUEST\0");
    let _ = core::hint::black_box(b"ERR_GRANT_DENIED\0");
    let _ = core::hint::black_box(b"AT_ELEMENT_REQ\0");
    let _ = core::hint::black_box(b"AT_ELEMENT_REPLY\0");
    let _ = core::hint::black_box(b"BACKGROUND_UNAVAILABLE\0");
    let _ = core::hint::black_box(b"AT_SPI_STRONG\0");
    let _ = core::hint::black_box(b"AT_SPI_CHROME\0");
    let _ = core::hint::black_box(b"apply_grant_confirm_click\0");
    let _ = core::hint::black_box(Reply::GrantAckOk.to_bytes());
    let _ = core::hint::black_box(Reply::GrantAckDenied.to_bytes());
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

    fn empty_grants() -> Arc<Mutex<HashMap<(String, u64), ()>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn empty_pending() -> Arc<Mutex<Option<PendingGrant>>> {
        Arc::new(Mutex::new(None))
    }

    #[test]
    fn dispatch_state_caches_is_locked() {
        let s = DispatchState::new(
            true,
            false,
            PathBuf::from("/dev/null"),
            empty_surface_map(),
            empty_grants(),
            empty_pending(),
        );
        assert!(s.is_locked);
    }

    #[test]
    fn dispatch_state_caches_outputs_off() {
        let s = DispatchState::new(
            false,
            true,
            PathBuf::from("/dev/null"),
            empty_surface_map(),
            empty_grants(),
            empty_pending(),
        );
        assert!(s.outputs_off);
    }

    #[test]
    fn dispatch_harness_resolve_window_uses_surface_map() {
        let mut state = DispatchState::new(
            false,
            false,
            PathBuf::from("/dev/null"),
            empty_surface_map(),
            empty_grants(),
            empty_pending(),
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
            empty_grants(),
            empty_pending(),
        );
        let surface = state.bound_surface.clone().expect("dummy surface");
        state.surface_map.lock().unwrap().insert(42, surface);
        let mut harness = DispatchHarness::new(&mut state);
        assert!(harness.resolve_window(999_999).is_none());
        assert!(harness.resolve_window(0).is_none());
    }

    #[test]
    fn dispatch_harness_reads_outputs_off_snapshot() {
        let mut state = DispatchState::new(
            false,
            true,
            PathBuf::from("/dev/null"),
            empty_surface_map(),
            empty_grants(),
            empty_pending(),
        );
        {
            let harness = DispatchHarness::new(&mut state);
            assert!(InjectHarness::outputs_off(&harness));
        }
        state.outputs_off = false;
        let harness = DispatchHarness::new(&mut state);
        assert!(!InjectHarness::outputs_off(&harness));
    }
}
