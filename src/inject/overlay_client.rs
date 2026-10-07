//! In-process `wayland-client` thread that creates a real `zwlr_layer_surface_v1`
//! for the `qlux_grant_confirm` grant-confirm bar (v38).
//!
//! Connect / bind failures log to stderr and return; they must not panic
//! (unit tests run without a compositor).

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use smithay_client_toolkit::reexports::client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};

/// Ident marker for the overlay-client thread spawn (LTO-retained).
pub static GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED: &str =
    "GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED";

/// Ident marker for the v40.5a configure redraw-on-size-change fix (LTO-retained).
pub const OVERLAY_CLIENT_CONFIGURE_REDRAW_ON_SIZE_CHANGE: &str =
    "OVERLAY_CLIENT_CONFIGURE_REDRAW_ON_SIZE_CHANGE";

#[used]
pub static _OVERLAY_CLIENT_CONFIGURE_REDRAW_ON_SIZE_CHANGE_BYTES: &[u8] =
    b"v40.5a_overlay_client_configure_redraw_on_size_change\0";

/// Ident marker for the v40.5a.1 draw() shm-pool resize-on-size-change fix (LTO-retained).
pub const OVERLAY_CLIENT_DRAW_SHM_POOL_RESIZE_ON_SIZE_CHANGE: &str =
    "OVERLAY_CLIENT_DRAW_SHM_POOL_RESIZE_ON_SIZE_CHANGE";

#[used]
pub static _OVERLAY_CLIENT_DRAW_SHM_POOL_RESIZE_ON_SIZE_CHANGE_BYTES: &[u8] =
    b"v40.5a.1_overlay_client_draw_shm_pool_resize_on_size_change\0";

#[used]
pub static SCT_CONNECTION_LIFETIME_V40_14_9: &[u8] = b"SCT_CONNECTION_LIFETIME_V40_14_9\0";

/// Ident marker for the v40.14.9 overlay-client keepalive fix (Fix-A).
/// The SCT `Connection` is preserved for the grant lifetime by holding an
/// `SctJoin` (JoinHandle + wakeup + shutdown flag) in `OverlayHandle`; drop
/// signals shutdown, wakes the thread, and joins. `#[used]` + `black_box`
/// below keep this literal in the release ELF past lld's gc.
#[used]
pub static OVERLAY_CLIENT_KEEPALIVE_V40_14_9: &[u8] = b"OVERLAY_CLIENT_KEEPALIVE_V40_14_9\0";

/// Sentinel that `spawn_overlay_client` started the SCT thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayClientHandle {
    pub is_spawned: bool,
}

/// Owns the overlay-client thread. Last `Arc` drop signals shutdown and joins.
#[derive(Debug)]
pub struct SctJoin {
    join: Mutex<Option<JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
    wakeup_tx: UnixStream,
}

impl Drop for SctJoin {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.wakeup_tx.write(&[1u8]);
        if let Some(handle) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a dedicated overlay-client thread. The handle is returned
/// immediately; connect/bind happens inside the thread and must not panic.
/// The `JoinHandle` is stored in [`SctJoin`] so the Wayland `Connection`
/// lives until the last `Arc<SctJoin>` is dropped.
pub fn spawn_overlay_client(
    socket_name: &OsStr,
    window_id: u64,
) -> (OverlayClientHandle, Option<Arc<SctJoin>>) {
    let socket_name = socket_name.to_os_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (wakeup_rx, wakeup_tx) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(_) => {
            return (
                OverlayClientHandle { is_spawned: false },
                None,
            );
        }
    };
    let shutdown_for_thread = Arc::clone(&shutdown);
    let spawned = std::thread::Builder::new()
        .name("niri-overlay-cl".into())
        .spawn(move || {
            run_overlay_client(socket_name, window_id, shutdown_for_thread, wakeup_rx)
        });
    match spawned {
        Ok(join) => {
            let _ = std::hint::black_box(GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED);
            let _ = std::hint::black_box(SCT_CONNECTION_LIFETIME_V40_14_9);
            let _ = std::hint::black_box(OVERLAY_CLIENT_KEEPALIVE_V40_14_9);
            (
                OverlayClientHandle { is_spawned: true },
                Some(Arc::new(SctJoin {
                    join: Mutex::new(Some(join)),
                    shutdown,
                    wakeup_tx,
                })),
            )
        }
        Err(_) => (OverlayClientHandle { is_spawned: false }, None),
    }
}

fn connect(socket_name: &OsStr) -> Result<Connection, String> {
    let path = Path::new(socket_name);
    if let Ok(stream) = UnixStream::connect(path) {
        return Connection::from_socket(stream)
            .map_err(|e| format!("Connection::from_socket: {e}"));
    }
    // Relative WAYLAND_DISPLAY names resolve under XDG_RUNTIME_DIR, then env.
    // Absolute sentinels (tests) must not fall through to a live compositor.
    if !path.is_absolute() {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let mut full = PathBuf::from(dir);
            full.push(socket_name);
            if let Ok(stream) = UnixStream::connect(&full) {
                return Connection::from_socket(stream)
                    .map_err(|e| format!("Connection::from_socket: {e}"));
            }
        }
        return Connection::connect_to_env().map_err(|e| format!("connect_to_env: {e}"));
    }
    Err(format!("cannot connect to {}", path.display()))
}

fn wait_for_shutdown(wakeup_rx: &UnixStream, shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::SeqCst) {
        let mut fds = [libc::pollfd {
            fd: wakeup_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("niri-overlay-client: poll: {err}");
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
    }
}

fn run_overlay_client(
    socket_name: std::ffi::OsString,
    window_id: u64,
    shutdown: Arc<AtomicBool>,
    wakeup_rx: UnixStream,
) {
    let _ = std::hint::black_box(window_id);
    let conn = match connect(&socket_name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("niri-overlay-client: {e}");
            wait_for_shutdown(&wakeup_rx, &shutdown);
            return;
        }
    };
    let (globals, mut event_queue) = match registry_queue_init(&conn) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("niri-overlay-client: registry_queue_init: {e}");
            wait_for_shutdown(&wakeup_rx, &shutdown);
            return;
        }
    };
    let qh = event_queue.handle();
    let compositor = match CompositorState::bind(&globals, &qh) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("niri-overlay-client: wl_compositor bind: {e}");
            wait_for_shutdown(&wakeup_rx, &shutdown);
            return;
        }
    };
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("niri-overlay-client: layer shell bind: {e}");
            wait_for_shutdown(&wakeup_rx, &shutdown);
            return;
        }
    };
    let shm = match Shm::bind(&globals, &qh) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("niri-overlay-client: wl_shm bind: {e}");
            wait_for_shutdown(&wakeup_rx, &shutdown);
            return;
        }
    };

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh, surface, Layer::Overlay, Some("qlux_grant_confirm"), None,
    );
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(40);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(2560, 40);
    layer.commit();

    let pool = match SlotPool::new(2560 * 40 * 4, &shm) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("niri-overlay-client: SlotPool: {e}");
            wait_for_shutdown(&wakeup_rx, &shutdown);
            return;
        }
    };

    let mut state = SimpleLayer {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        first_configure: true,
        pool,
        width: 2560,
        height: 40,
        last_drawn_w: 0,
        last_drawn_h: 0,
        layer,
        keyboard: None,
        pointer: None,
    };
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if let Err(e) = event_queue.dispatch_pending(&mut state) {
            eprintln!("niri-overlay-client: dispatch: {e}");
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            wait_for_shutdown(&wakeup_rx, &shutdown);
            continue;
        }
        if let Err(e) = event_queue.flush() {
            eprintln!("niri-overlay-client: flush: {e}");
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            wait_for_shutdown(&wakeup_rx, &shutdown);
            continue;
        }
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };
        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wakeup_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if shutdown.load(Ordering::SeqCst) || fds[1].revents != 0 {
            break;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                eprintln!("niri-overlay-client: poll: {err}");
            }
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }
        match guard.read() {
            Ok(_) => {
                if let Err(e) = event_queue.dispatch_pending(&mut state) {
                    eprintln!("niri-overlay-client: dispatch: {e}");
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    wait_for_shutdown(&wakeup_rx, &shutdown);
                    continue;
                }
            }
            Err(e) => {
                eprintln!("niri-overlay-client: read: {e}");
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                wait_for_shutdown(&wakeup_rx, &shutdown);
                continue;
            }
        }
    }
    let _ = conn;
}


struct SimpleLayer {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    first_configure: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    last_drawn_w: u32,
    last_drawn_h: u32,
    layer: LayerSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
}

impl SimpleLayer {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let (width, height) = (self.width, self.height);
        if should_resize_shm_pool_on_draw(self.last_drawn_w, self.last_drawn_h, width, height) {
            let new_pool_bytes = (width as usize).saturating_mul(height as usize).saturating_mul(4);
            if let Err(e) = self.pool.resize(new_pool_bytes) {
                eprintln!("niri-overlay-client: pool.resize({new_pool_bytes}): {e}");
                return;
            }
            // v40.5a.1 — pin the resize idents into the ELF so the regression guard can grep for them.
            let _ = std::hint::black_box(OVERLAY_CLIENT_DRAW_SHM_POOL_RESIZE_ON_SIZE_CHANGE);
            let _ = std::hint::black_box(_OVERLAY_CLIENT_DRAW_SHM_POOL_RESIZE_ON_SIZE_CHANGE_BYTES);
        }
        let (buffer, canvas) = match self.pool.create_buffer(
            width as i32, height as i32, width as i32 * 4, wl_shm::Format::Argb8888,
        ) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("niri-overlay-client: create_buffer: {e}");
                return;
            }
        };
        paint_grant_deny_canvas(canvas, width, height);
        self.layer.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        self.layer.wl_surface().frame(qh, self.layer.wl_surface().clone());
        if buffer.attach_to(self.layer.wl_surface()).is_err() {
            eprintln!("niri-overlay-client: buffer attach failed");
            return;
        }
        self.layer.commit();
        self.last_drawn_w = width;
        self.last_drawn_h = height;
    }
}


pub fn should_redraw_on_configure(
    old_w: u32, old_h: u32, new_w: u32, new_h: u32, first_configure: bool,
) -> bool {
    first_configure || (old_w, old_h) != (new_w, new_h)
}

pub fn should_resize_shm_pool_on_draw(
    last_drawn_w: u32, last_drawn_h: u32, current_w: u32, current_h: u32,
) -> bool {
    (last_drawn_w, last_drawn_h) != (current_w, current_h)
}

pub fn paint_grant_deny_canvas(canvas: &mut [u8], width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    for y in 0..h {
        for x in 0..w {
            let px = (y * w + x) * 4;
            if px + 3 >= canvas.len() {
                continue;
            }
            let (b, g, r, a) = if x < 1280 {
                (0u8, 255u8, 0u8, 255u8)
            } else {
                (0u8, 0u8, 255u8, 255u8)
            };
            canvas[px] = b;
            canvas[px + 1] = g;
            canvas[px + 2] = r;
            canvas[px + 3] = a;
        }
    }
    paint_label(canvas, w, h, b"GRANT", true);
    paint_label(canvas, w, h, b"DENY", false);
}

/// Bitmapped 3x5 glyph table for the GRANT / DENY labels. Pure ASCII subset.
const GRANT_CONFIRM_GLYPHS: &[(&str, [u8; 5])] = &[
    ("G", [0b01110, 0b10001, 0b10000, 0b10111, 0b01110]),
    ("R", [0b11110, 0b10001, 0b11110, 0b10100, 0b10010]),
    ("A", [0b01110, 0b10001, 0b11111, 0b10001, 0b10001]),
    ("N", [0b10001, 0b11001, 0b10101, 0b10011, 0b10001]),
    ("T", [0b11111, 0b00100, 0b00100, 0b00100, 0b00100]),
    ("D", [0b11110, 0b10001, 0b10001, 0b10001, 0b11110]),
    ("E", [0b11111, 0b10000, 0b11110, 0b10000, 0b11111]),
    ("Y", [0b10001, 0b01010, 0b00100, 0b00100, 0b00100]),
];

const EMPTY_GLYPH: [u8; 5] = [0; 5];

/// Render `text` (upper-case ASCII) into `buf` as 3x5 black pixels on the
/// existing background. `left` = true places the label at x=560..640 (GRANT
/// half); false places it at x=1936..2016 (DENY half).
fn paint_label(buf: &mut [u8], w: usize, h: usize, text: &[u8], left: bool) {
    let start_x = if left { 560 } else { 1936 };
    let start_y = 12;
    let mut glyph_idx = 0usize;
    for &raw in text.iter() {
        let ch = (raw as char).to_ascii_uppercase();
        let key: &str = match ch {
            'G' => "G",
            'R' => "R",
            'A' => "A",
            'N' => "N",
            'T' => "T",
            'D' => "D",
            'E' => "E",
            'Y' => "Y",
            _ => "",
        };
        let bits = GRANT_CONFIRM_GLYPHS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, b)| *b)
            .unwrap_or(EMPTY_GLYPH);
        for (row, row_bits) in bits.iter().enumerate() {
            for col in 0..5 {
                if row_bits & (1 << (4 - col)) != 0 {
                    let px = ((start_y + row) * w + start_x + glyph_idx * 8 + col) * 4;
                    if px + 3 < buf.len() {
                        buf[px] = 0;
                        buf[px + 1] = 0;
                        buf[px + 2] = 0;
                        buf[px + 3] = 255;
                    }
                }
            }
        }
        glyph_idx += 1;
    }
    let _ = h;
}

impl CompositorHandler for SimpleLayer {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) { self.draw(qh); }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for SimpleLayer {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for SimpleLayer {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {}
    fn configure(
        &mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface,
        configure: LayerSurfaceConfigure, _: u32,
    ) {
        let new_w = if configure.new_size.0 != 0 { configure.new_size.0 } else { self.width };
        let new_h = if configure.new_size.1 != 0 { configure.new_size.1 } else { self.height };
        if should_redraw_on_configure(self.width, self.height, new_w, new_h, self.first_configure) {
            // v40.5a — pin the redraw-on-size-change idents into the ELF so the
            // regression guard can grep for them.
            let _ = std::hint::black_box(OVERLAY_CLIENT_CONFIGURE_REDRAW_ON_SIZE_CHANGE);
            let _ = std::hint::black_box(_OVERLAY_CLIENT_CONFIGURE_REDRAW_ON_SIZE_CHANGE_BYTES);
            self.width = new_w;
            self.height = new_h;
            self.first_configure = false;
            self.draw(qh);
        }
    }
}

impl SeatHandler for SimpleLayer {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            if let Ok(keyboard) = self.seat_state.get_keyboard(qh, &seat, None) {
                self.keyboard = Some(keyboard);
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.pointer = Some(pointer);
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() { k.release(); }
        }
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() { p.release(); }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for SimpleLayer {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: RawModifiers, _: u32) {}
}

impl PointerHandler for SimpleLayer {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let _ = &event.surface != self.layer.wl_surface();
        }
    }
}

impl ShmHandler for SimpleLayer {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

delegate_compositor!(SimpleLayer);
delegate_output!(SimpleLayer);
delegate_shm!(SimpleLayer);
delegate_seat!(SimpleLayer);
delegate_keyboard!(SimpleLayer);
delegate_pointer!(SimpleLayer);
delegate_layer!(SimpleLayer);
delegate_registry!(SimpleLayer);

impl ProvidesRegistryState for SimpleLayer {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
pub(crate) use paint_grant_deny_canvas as paint_grant_deny_canvas_for_tests;
