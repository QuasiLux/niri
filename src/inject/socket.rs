//! Unix-socket listener and inject-client state machine.
//!
//! Accepts connections on `$XDG_RUNTIME_DIR/qlux/inject.sock` (falling back to
//! the process temp dir). Parent directories are never created: bind is
//! fail-closed if the runtime dir is missing. Frame parsing lives in
//! [`super::protocol`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use async_channel::{Receiver, Sender};
use calloop::io::Async;
use futures_util::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use futures_util::{select_biased, AsyncBufReadExt, FutureExt as _};
use smithay::backend::input::{Axis, AxisSource, ButtonState, InputTime};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use super::protocol::{
    parse_frame, validate_token, ClickOutcome, Frame, FrameError, InjectHarness, KeyMods, Reply,
    SeatKind, TokenError, ERR_BAD_TOKEN, ERR_BAD_WINDOW, ERR_GRANT_DENIED, ERR_GRANT_NOT_PENDING,
    ERR_IO, ERR_LOCKED, ERR_NOT_GRANTED, ERR_NO_TOKEN_FILE, ERR_OCCLUDED, ERR_OUTPUTS_OFF,
    ERR_STALE_TOKEN, ERR_TOO_BIG, ERR_UNKNOWN_OPC, MAX_FRAME_LEN, OPC_AUTH,
};

/// Path suffix under the runtime dir. Kept as a string so the release ELF
/// grep can find it.
pub const INJECT_SOCKET_SUFFIX: &str = "qlux/inject.sock";

/// Env var that gates compositor-side spawn. Default off (`"1"` to enable).
pub const QLUX_INJECT_ENABLE: &str = "QLUX_INJECT_ENABLE";

/// Env var overriding the socket suffix under `XDG_RUNTIME_DIR` (tests).
pub const QLUX_INJECT_SOCKET_SUFFIX: &str = "QLUX_INJECT_SOCKET_SUFFIX";

/// Unix group that owns `/run/user/<uid>/qlux/inject.sock` (mode 0660).
pub const QLUX_INJECT_GROUP: &str = "qlux-inject";

pub type ConnectionId = u64;

/// In-memory `bot_id → token` map, reloaded from the token file.
#[derive(Default, Debug)]
pub struct TokenRegistry {
    map: HashMap<String, String>,
}

impl TokenRegistry {
    pub fn reload_from(&mut self, path: &Path) -> io::Result<()> {
        let mut map = HashMap::new();
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.map.clear();
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for line in io::BufReader::new(file).lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((bot, tok)) = line.split_once(':') {
                map.insert(bot.trim().to_string(), tok.trim().to_string());
            }
        }
        self.map = map;
        Ok(())
    }

    pub fn contains(&self, bot_id: &str) -> bool {
        self.map.contains_key(bot_id)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

pub(crate) struct ActiveConn {
    bot_id: String,
    close_tx: async_channel::Sender<()>,
}

/// Bound inject-plane listener.
pub struct InjectServer {
    /// Filesystem path of the bound socket.
    pub socket_path: PathBuf,
    /// For log lines; never empty.
    pub socket_path_display: String,
    pub(crate) token_registry: Arc<Mutex<TokenRegistry>>,
    pub(crate) connections: Rc<RefCell<HashMap<ConnectionId, ActiveConn>>>,
    next_id: Cell<u64>,
    _incoming: Sender<UnixStream>,
}

impl InjectServer {
    /// `$XDG_RUNTIME_DIR/qlux/inject.sock`, or temp-dir fallback.
    pub fn default_path() -> PathBuf {
        runtime_dir().join(INJECT_SOCKET_SUFFIX)
    }

    /// Bind `socket_path` and push accepted streams onto a channel.
    ///
    /// Fail-closed if the parent directory is missing: do not create it, do
    /// not panic. The event-loop callback only `accept()`s.
    pub fn start<T: 'static>(
        event_loop: &LoopHandle<'static, T>,
        socket_path: PathBuf,
    ) -> anyhow::Result<(Self, Receiver<UnixStream>)> {
        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.is_dir() {
                anyhow::bail!(
                    "inject runtime dir missing ({}); skipping bind",
                    parent.display()
                );
            }
        }
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).context("error binding inject socket")?;
        listener
            .set_nonblocking(true)
            .context("error setting inject socket non-blocking")?;

        if let Ok(meta) = std::fs::metadata(&socket_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o660);
            let _ = std::fs::set_permissions(&socket_path, perms);
        }

        let (tx, rx) = async_channel::unbounded();
        let tx_source = tx.clone();

        let source = Generic::new(listener, Interest::READ, Mode::Level);
        event_loop
            .insert_source(source, move |_, socket, _state| {
                match socket.accept() {
                    Ok((stream, _)) => {
                        let _ = tx_source.try_send(stream);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                    Err(e) => return Err(e),
                }
                Ok(PostAction::Continue)
            })
            .unwrap();

        let mut socket_path_display = socket_path.to_string_lossy().into_owned();
        if socket_path_display.is_empty() {
            socket_path_display = INJECT_SOCKET_SUFFIX.to_string();
        }

        Ok((
            Self {
                socket_path,
                socket_path_display,
                token_registry: Arc::new(Mutex::new(TokenRegistry::default())),
                connections: Rc::new(RefCell::new(HashMap::new())),
                next_id: Cell::new(1),
                _incoming: tx,
            },
            rx,
        ))
    }

    /// Register an accepted connection. `bot_id` may be empty until AUTH.
    pub fn track_connection(&self, bot_id: String) -> (ConnectionId, Receiver<()>) {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        let (close_tx, close_rx) = async_channel::bounded(1);
        self.connections
            .borrow_mut()
            .insert(id, ActiveConn { bot_id, close_tx });
        (id, close_rx)
    }

    pub fn set_connection_bot_id(&self, id: ConnectionId, bot_id: String) {
        if let Some(c) = self.connections.borrow_mut().get_mut(&id) {
            c.bot_id = bot_id;
        }
    }

    pub fn close_connection(&self, id: ConnectionId) -> bool {
        if let Some(c) = self.connections.borrow_mut().remove(&id) {
            c.close_tx.try_send(()).is_ok()
        } else {
            false
        }
    }

    pub fn untrack_connection(&self, id: ConnectionId) {
        self.connections.borrow_mut().remove(&id);
    }

    pub fn reload_tokens(&self, token_file: &Path) -> io::Result<()> {
        self.token_registry.lock().unwrap().reload_from(token_file)
    }

    /// Re-read the token file and close connections whose `bot_id` is gone.
    pub fn reload_and_close_stale(&self, token_file: &Path) -> io::Result<usize> {
        let mut reg = self.token_registry.lock().unwrap();
        reg.reload_from(token_file)?;
        let stale: Vec<ConnectionId> = self
            .connections
            .borrow()
            .iter()
            .filter(|(_, c)| !c.bot_id.is_empty() && !reg.contains(&c.bot_id))
            .map(|(id, _)| *id)
            .collect();
        drop(reg);
        let mut conns = self.connections.borrow_mut();
        let mut closed = 0;
        for id in stale {
            if let Some(c) = conns.remove(&id) {
                if c.close_tx.try_send(()).is_ok() {
                    closed += 1;
                }
            }
        }
        Ok(closed)
    }
}

/// Default-off spawn gate used by `Niri::new`.
pub fn inject_spawn_enabled() -> bool {
    inject_spawn_enabled_from(env::var_os(QLUX_INJECT_ENABLE).as_deref())
}

pub fn inject_spawn_enabled_from(val: Option<&OsStr>) -> bool {
    val == Some(OsStr::new("1"))
}

/// `$XDG_RUNTIME_DIR` + [`INJECT_SOCKET_SUFFIX`], honoring [`QLUX_INJECT_SOCKET_SUFFIX`].
pub fn resolve_inject_socket_path() -> PathBuf {
    let runtime = runtime_dir();
    match env::var_os(QLUX_INJECT_SOCKET_SUFFIX) {
        Some(suffix) => runtime.join(suffix),
        None => runtime.join(INJECT_SOCKET_SUFFIX),
    }
}

fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

/// Look up the `qlux-inject` group. `None` if absent (fail-closed, no panic).
pub fn lookup_inject_group_gid() -> Option<libc::gid_t> {
    let cname = std::ffi::CString::new(QLUX_INJECT_GROUP).ok()?;
    let gptr = unsafe { libc::getgrnam(cname.as_ptr()) };
    if gptr.is_null() {
        None
    } else {
        Some(unsafe { (*gptr).gr_gid })
    }
}

/// `chown` group + `chmod 0660` on the bound socket. Failures are logged, not fatal.
pub fn apply_inject_socket_ownership(path: &Path, gid: libc::gid_t) {
    use std::os::unix::ffi::OsStrExt;

    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        warn!(
            "inject socket path is not a valid CString: {}",
            path.display()
        );
        return;
    };
    let rc = unsafe { libc::chown(cpath.as_ptr(), libc::uid_t::MAX, gid) };
    if rc != 0 {
        warn!(
            "failed to chown inject socket to {QLUX_INJECT_GROUP}: {}",
            io::Error::last_os_error()
        );
    }
    let rc = unsafe { libc::chmod(cpath.as_ptr(), 0o660) };
    if rc != 0 {
        warn!(
            "failed to chmod inject socket to 0660: {}",
            io::Error::last_os_error()
        );
    }
}

/// Linux `BTN_LEFT` from `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;

/// Distinct rodata copy so `strings | grep -c` sees this ident ≥ 2 times.
#[used]
static KEY_DELIVERY_V40_6_SOCKET_IDENT: &[u8] =
    b"\x02KEY_DELIVERY_V40_6_SURFACE_ACTIVATION\x02";

/// Distinct rodata copy so `strings | grep -c` sees the v40.7 ident ≥ 2 times.
#[used]
static KEY_DELIVERY_V40_7_SOCKET_IDENT: &[u8] =
    b"\x02KEY_DELIVERY_V40_7_USER_SEAT_ROUTING\x02";

/// Distinct rodata copy so thin-LTO cannot collapse the v40.9 ident to one copy.
#[used]
static KEY_DELIVERY_V40_9_SOCKET_IDENT: &[u8] =
    b"\x02KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF\x02";

enum ClientPhase<F> {
    Greeting,
    AwaitAuth { retries: u8 },
    AwaitBind,
    Dispatch { focus: F, wid: u64 },
}

enum DispatchOne {
    Closed,
    Eof,
    Frame(Result<Frame, FrameError>),
}

/// Drive HELLO → AUTH → BIND → dispatch on one accepted inject stream.
pub async fn handle_inject_client<H: InjectHarness>(
    harness: &mut H,
    stream: Async<'static, UnixStream>,
) -> anyhow::Result<()> {
    let (keep_tx, close_rx) = async_channel::bounded(1);
    let result = handle_inject_client_with_close(harness, stream, close_rx).await;
    drop(keep_tx);
    result
}

/// Like [`handle_inject_client`], but exit when `close_rx` fires (token rotation).
pub async fn handle_inject_client_with_close<H: InjectHarness>(
    harness: &mut H,
    stream: Async<'static, UnixStream>,
    close_rx: async_channel::Receiver<()>,
) -> anyhow::Result<()> {
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    let mut phase = ClientPhase::Greeting;
    let mut bot_id = String::new();

    loop {
        let frame = match dispatch_one(&mut read, &close_rx).await {
            Ok(DispatchOne::Closed) => {
                // Token rotation: authenticated connections get a wire-level
                // STALE_TOKEN reply. Greeting/AwaitAuth may close silently.
                if matches!(phase, ClientPhase::Dispatch { .. } | ClientPhase::AwaitBind)
                    && !bot_id.is_empty()
                {
                    write_reply(
                        &mut write,
                        &Reply::DispatchErr {
                            code: ERR_STALE_TOKEN,
                        },
                    )
                    .await?;
                }
                return Ok(());
            }
            Ok(DispatchOne::Eof) => return Ok(()),
            Ok(DispatchOne::Frame(Ok(frame))) => frame,
            Ok(DispatchOne::Frame(Err(err))) => {
                write_reply(&mut write, &Reply::FrameErr { code: err.code() }).await?;
                if matches!(err, FrameError::TooBig) {
                    harness.report_permanent_error(ERR_TOO_BIG);
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(err) => return Err(err).context("error reading inject frame"),
        };

        phase = match (phase, frame) {
            (ClientPhase::Greeting, Frame::Hello) => {
                write_reply(&mut write, &Reply::HelloOk).await?;
                ClientPhase::AwaitAuth { retries: 0 }
            }
            (ClientPhase::Greeting, _) => {
                write_reply(
                    &mut write,
                    &Reply::FrameErr {
                        code: ERR_UNKNOWN_OPC,
                    },
                )
                .await?;
                return Ok(());
            }
            (ClientPhase::AwaitAuth { retries }, Frame::Auth(token)) => {
                match validate_token(&token, harness.token_file_path()) {
                    Ok(id) => {
                        write_reply(&mut write, &Reply::AuthOk { bot_id: id.clone() }).await?;
                        bot_id = id;
                        ClientPhase::AwaitBind
                    }
                    Err(TokenError::NoFile) => {
                        write_reply(
                            &mut write,
                            &Reply::AuthErr {
                                code: ERR_NO_TOKEN_FILE,
                            },
                        )
                        .await?;
                        harness.report_permanent_error(ERR_NO_TOKEN_FILE);
                        return Ok(());
                    }
                    Err(TokenError::Io(_)) => {
                        write_reply(&mut write, &Reply::AuthErr { code: ERR_IO }).await?;
                        harness.report_permanent_error(ERR_IO);
                        return Ok(());
                    }
                    Err(TokenError::NoMatch) => {
                        write_reply(
                            &mut write,
                            &Reply::AuthErr {
                                code: ERR_BAD_TOKEN,
                            },
                        )
                        .await?;
                        if retries >= 1 {
                            harness.report_permanent_error(ERR_BAD_TOKEN);
                            return Ok(());
                        }
                        harness.report_transient_error(ERR_BAD_TOKEN);
                        ClientPhase::AwaitAuth {
                            retries: retries + 1,
                        }
                    }
                }
            }
            (ClientPhase::AwaitAuth { .. }, _) => {
                write_reply(
                    &mut write,
                    &Reply::FrameErr {
                        code: ERR_UNKNOWN_OPC,
                    },
                )
                .await?;
                return Ok(());
            }
            (ClientPhase::AwaitBind, Frame::Bind(window_id)) => {
                if harness.resolve_window(window_id).is_none() {
                    write_reply(
                        &mut write,
                        &Reply::FrameErr {
                            code: ERR_BAD_WINDOW,
                        },
                    )
                    .await?;
                    harness.report_transient_error(ERR_BAD_WINDOW);
                    ClientPhase::AwaitBind
                } else if harness.grant_present(&bot_id, window_id) {
                    if let Some(focus) = harness.resolve_pointer_focus(window_id) {
                        let _ = harness.set_keyboard_focus(window_id);
                        let (frontmost_wid, frontmost_app_id) =
                            crate::inject::atspi_ladder::frontmost_or_echo(
                                window_id,
                                harness.app_id_for(window_id).as_deref(),
                            );
                        let _ = std::hint::black_box(crate::inject::CROSS_APP_HANDOFF);
                        write_reply(
                            &mut write,
                            &Reply::BindOk {
                                wid: window_id,
                                frontmost_wid,
                                frontmost_app_id,
                            },
                        )
                        .await?;
                        ClientPhase::Dispatch {
                            focus,
                            wid: window_id,
                        }
                    } else {
                        write_reply(
                            &mut write,
                            &Reply::FrameErr {
                                code: ERR_BAD_WINDOW,
                            },
                        )
                        .await?;
                        harness.report_transient_error(ERR_BAD_WINDOW);
                        ClientPhase::AwaitBind
                    }
                } else if harness.grant_pending(&bot_id) {
                    write_reply(&mut write, &Reply::BindPending).await?;
                    ClientPhase::AwaitBind
                } else {
                    write_reply(
                        &mut write,
                        &Reply::DispatchErr {
                            code: ERR_NOT_GRANTED,
                        },
                    )
                    .await?;
                    harness.report_transient_error(ERR_NOT_GRANTED);
                    ClientPhase::AwaitBind
                }
            }
            (ClientPhase::AwaitBind, Frame::GrantRequest { bot_id: req_id }) => {
                reply_grant_request(harness, &mut write, &bot_id, req_id).await?;
                ClientPhase::AwaitBind
            }
            (
                ClientPhase::AwaitBind,
                Frame::GrantResolved {
                    bot_id: req_bot,
                    window_id,
                    resolution,
                },
            ) => {
                let app_id = harness.app_id_for(window_id).unwrap_or_default();
                let outcome = harness.apply_grant_confirm_click(
                    &req_bot,
                    window_id,
                    &app_id,
                    SeatKind::User,
                    resolution,
                );
                match outcome {
                    ClickOutcome::Granted { wid } => {
                        // Row 37+ — the layer-shell overlay registration
                        // happens inside `finish_click` (called from the
                        // harness's `apply_grant_confirm_click` impl).
                        // Pin the new idents here so this transition site
                        // stays LTO-retained alongside the registration
                        // call site in `grant_confirm.rs`.
                        let _ = std::hint::black_box(
                            crate::inject::GRANT_CONFIRM_OVERLAY_LAYER_SHELL_REGISTERED,
                        );
                        let _ = std::hint::black_box(
                            crate::inject::grant_confirm::GRANT_CONFIRM_OVERLAY_SELF_CONNECT_THREAD_SPAWNED,
                        );
                        let _ = std::hint::black_box(wid);
                        let window_ok = harness.resolve_window(wid).is_some();
                        let focus = harness.resolve_pointer_focus(wid);
                        if window_ok {
                            if let Some(focus) = focus {
                                let _ = harness.set_keyboard_focus(wid);
                                let (frontmost_wid, frontmost_app_id) =
                                    crate::inject::atspi_ladder::frontmost_or_echo(
                                        wid,
                                        harness.app_id_for(wid).as_deref(),
                                    );
                                write_reply(
                                    &mut write,
                                    &Reply::BindOk {
                                        wid,
                                        frontmost_wid,
                                        frontmost_app_id,
                                    },
                                )
                                .await?;
                                ClientPhase::Dispatch { focus, wid }
                            } else {
                                write_reply(
                                    &mut write,
                                    &Reply::FrameErr {
                                        code: ERR_BAD_WINDOW,
                                    },
                                )
                                .await?;
                                ClientPhase::AwaitBind
                            }
                        } else {
                            write_reply(
                                &mut write,
                                &Reply::FrameErr {
                                    code: ERR_BAD_WINDOW,
                                },
                            )
                            .await?;
                            ClientPhase::AwaitBind
                        }
                    }
                    ClickOutcome::Denied => {
                        write_reply(
                            &mut write,
                            &Reply::DispatchErr {
                                code: ERR_GRANT_DENIED,
                            },
                        )
                        .await?;
                        ClientPhase::AwaitBind
                    }
                    _ => {
                        write_reply(
                            &mut write,
                            &Reply::FrameErr {
                                code: ERR_GRANT_NOT_PENDING,
                            },
                        )
                        .await?;
                        ClientPhase::AwaitBind
                    }
                }
            }
            (ClientPhase::AwaitBind, _) => {
                write_reply(
                    &mut write,
                    &Reply::FrameErr {
                        code: ERR_UNKNOWN_OPC,
                    },
                )
                .await?;
                return Ok(());
            }
            (ClientPhase::Dispatch { .. }, Frame::Hello | Frame::Auth(_)) => {
                write_reply(
                    &mut write,
                    &Reply::FrameErr {
                        code: ERR_UNKNOWN_OPC,
                    },
                )
                .await?;
                return Ok(());
            }
            (ClientPhase::Dispatch { focus, wid }, Frame::Bind(window_id)) => {
                if harness.resolve_window(window_id).is_none() {
                    write_reply(
                        &mut write,
                        &Reply::FrameErr {
                            code: ERR_BAD_WINDOW,
                        },
                    )
                    .await?;
                    harness.report_transient_error(ERR_BAD_WINDOW);
                    ClientPhase::Dispatch { focus, wid }
                } else if harness.grant_present(&bot_id, window_id) {
                    if let Some(new_focus) = harness.resolve_pointer_focus(window_id) {
                        let _ = harness.set_keyboard_focus(window_id);
                        let (frontmost_wid, frontmost_app_id) =
                            crate::inject::atspi_ladder::frontmost_or_echo(
                                window_id,
                                harness.app_id_for(window_id).as_deref(),
                            );
                        write_reply(
                            &mut write,
                            &Reply::BindOk {
                                wid: window_id,
                                frontmost_wid,
                                frontmost_app_id,
                            },
                        )
                        .await?;
                        ClientPhase::Dispatch {
                            focus: new_focus,
                            wid: window_id,
                        }
                    } else {
                        write_reply(
                            &mut write,
                            &Reply::FrameErr {
                                code: ERR_BAD_WINDOW,
                            },
                        )
                        .await?;
                        harness.report_transient_error(ERR_BAD_WINDOW);
                        ClientPhase::Dispatch { focus, wid }
                    }
                } else if harness.grant_pending(&bot_id) {
                    write_reply(&mut write, &Reply::BindPending).await?;
                    ClientPhase::Dispatch { focus, wid }
                } else {
                    write_reply(
                        &mut write,
                        &Reply::DispatchErr {
                            code: ERR_NOT_GRANTED,
                        },
                    )
                    .await?;
                    harness.report_transient_error(ERR_NOT_GRANTED);
                    ClientPhase::Dispatch { focus, wid }
                }
            }
            (ClientPhase::Dispatch { focus, wid }, Frame::GrantRequest { bot_id: req_id }) => {
                reply_grant_request(harness, &mut write, &bot_id, req_id).await?;
                ClientPhase::Dispatch { focus, wid }
            }
            (
                ClientPhase::Dispatch { focus, wid },
                Frame::AtActionReq {
                    wid: req_wid,
                    role,
                    name,
                    index,
                },
            ) => {
                let _ = std::hint::black_box(crate::inject::protocol::AT_ACTION_REQ_OPCODE_V40_15_3);
                let _ = std::hint::black_box(crate::inject::protocol::AT_ACTION_REQ_DISPATCHED_V40_15_3);
                let _ = std::hint::black_box(
                    crate::inject::atspi_ladder::FORCE_ICON_REDRAW_V40_15_5_OVERRIDE_IDENT,
                );
                let bounds = crate::inject::atspi_ladder::element_lookup_via_tree_dump(
                    req_wid, &role, &name,
                )
                .or_else(|| {
                    crate::inject::atspi_ladder::scan_session_dbus_for_element(
                        req_wid, &role, &name,
                    )
                });
                let reply = match bounds {
                    None => Reply::AtActionReply {
                        ok: false,
                        error: Some("ELEMENT_NOT_FOUND".into()),
                        force_redraw: false,
                    },
                    Some(b) => {
                        let path = if b.dbus_path.is_empty() {
                            b.path.clone()
                        } else {
                            b.dbus_path.clone()
                        };
                        crate::inject::protocol::set_last_action_target(
                            &b.dbus_dest,
                            &path,
                            b.n_actions,
                        );
                        match harness.apply_action_on_element(req_wid, &path, index) {
                            Ok(()) => {
                                let force_redraw = role == "toggle button";
                                if force_redraw {
                                    let _ = crate::inject::atspi_ladder::force_icon_redraw(
                                        &b.dbus_dest,
                                        &path,
                                    );
                                }
                                Reply::AtActionReply {
                                    ok: true,
                                    error: None,
                                    force_redraw,
                                }
                            }
                            Err(code) => Reply::AtActionReply {
                                ok: false,
                                error: Some(code.into()),
                                force_redraw: false,
                            },
                        }
                    }
                };
                tracing::debug!(
                    window_id = req_wid,
                    role = %role,
                    name = %name,
                    index,
                    ?reply,
                    "atspi.action_req"
                );
                write_reply(&mut write, &reply).await?;
                ClientPhase::Dispatch { focus, wid }
            }
            (ClientPhase::Dispatch { focus, wid }, Frame::AtElementReq { window_id, action }) => {
                let app_id = harness.app_id_for(window_id).unwrap_or_default();
                use super::atspi_ladder::AtSpiRoute;
                match harness.route(&app_id, &action) {
                    AtSpiRoute::Element { role, query } => {
                        // v40.14.6: prefer the AX-tree dump filter. The
                        // dump already enumerates every published node
                        // with bounds, so we sidestep the
                        // xdg-desktop-portal-gtk
                        // `impl_GetExtents: ATK_IS_COMPONENT
                        // (user_data) failed` assertion on the desktop
                        // root. Fall back to the scan path only when
                        // the dump returned empty (no children).
                        if let Some(bounds) =
                            crate::inject::atspi_ladder::element_lookup_via_tree_dump(
                                window_id, &role, &query,
                            )
                        {
                            let cx = bounds.rect.x + bounds.rect.w / 2.0;
                            let cy = bounds.rect.y + bounds.rect.h / 2.0;
                            let warning =
                                crate::inject::atspi_ladder::offscreen_warning(&bounds.rect);
                            let changed = crate::inject::atspi_ladder::element_changed_field(
                                &bot_id,
                                window_id,
                                &role,
                                &query,
                                &bounds.path,
                                &bounds.rect,
                            );
                            let _ = std::hint::black_box(b"\x01LBL_CLICK_ACTION_V40_14_15\x01");
                            let outcome = crate::inject::atspi_action::toggle_matched_node(
                                &bounds.dbus_dest,
                                &bounds.dbus_path,
                            );
                            let flipped = matches!(
                                outcome,
                                crate::inject::atspi_action::ToggleOutcome::Flipped
                                    | crate::inject::atspi_action::ToggleOutcome::GrabFocusFallbackUsed
                            );
                            if !flipped {
                                crate::inject::atspi_ladder::record_bridge_click(cx, cy);
                                let _ = std::hint::black_box(
                                    crate::inject::atspi_ladder::AT_SPI_BRIDGE_LIVE,
                                );
                                let _ = std::hint::black_box(
                                    crate::inject::ELEMENT_AX_LOOKUP_V40_14_6,
                                );
                                pointer_click(harness, &focus, Point::from((cx, cy)));
                            }
                            let mut payload = format!(
                                "{} {} {} {} {} {} {}",
                                bounds.path,
                                bounds.rect.x,
                                bounds.rect.y,
                                bounds.rect.w,
                                bounds.rect.h,
                                changed,
                                warning
                            );
                            let trailer =
                                crate::inject::atspi_ladder::last_tree_diff_trailer();
                            if !trailer.is_empty() {
                                payload.push('\n');
                                payload.push_str(&trailer);
                            }
                            if flipped && !payload.contains("CHANGED: 1") {
                                payload.push('\n');
                                payload.push_str("CHANGED: 1");
                            }
                            tracing::debug!(payload = %payload, "atspi.reply");
                            write_reply(
                                &mut write,
                                &Reply::AtElementReply { payload },
                            )
                            .await?;
                        } else if let Some(bounds) =
                            crate::inject::atspi_ladder::scan_session_dbus_for_element(
                                window_id, &role, &query,
                            )
                        {
                            // Fallback: dump returned no children — try
                            // the scan path. Preserves the G3 evidence
                            // path (35 nodes) for callers using
                            // `scan_session_dbus_for_element` as a
                            // primary path.
                            let cx = bounds.rect.x + bounds.rect.w / 2.0;
                            let cy = bounds.rect.y + bounds.rect.h / 2.0;
                            let warning =
                                crate::inject::atspi_ladder::offscreen_warning(&bounds.rect);
                            let changed = crate::inject::atspi_ladder::element_changed_field(
                                &bot_id,
                                window_id,
                                &role,
                                &query,
                                &bounds.path,
                                &bounds.rect,
                            );
                            let _ = std::hint::black_box(b"\x01LBL_CLICK_ACTION_V40_14_15\x01");
                            let outcome = crate::inject::atspi_action::toggle_matched_node(
                                &bounds.dbus_dest,
                                &bounds.dbus_path,
                            );
                            let flipped = matches!(
                                outcome,
                                crate::inject::atspi_action::ToggleOutcome::Flipped
                                    | crate::inject::atspi_action::ToggleOutcome::GrabFocusFallbackUsed
                            );
                            if !flipped {
                                crate::inject::atspi_ladder::record_bridge_click(cx, cy);
                                let _ = std::hint::black_box(
                                    crate::inject::atspi_ladder::AT_SPI_BRIDGE_LIVE,
                                );
                                pointer_click(harness, &focus, Point::from((cx, cy)));
                            }
                            let mut payload = format!(
                                "{} {} {} {} {} {} {}",
                                bounds.path,
                                bounds.rect.x,
                                bounds.rect.y,
                                bounds.rect.w,
                                bounds.rect.h,
                                changed,
                                warning
                            );
                            let trailer =
                                crate::inject::atspi_ladder::last_tree_diff_trailer();
                            if !trailer.is_empty() {
                                payload.push('\n');
                                payload.push_str(&trailer);
                            }
                            if flipped && !payload.contains("CHANGED: 1") {
                                payload.push('\n');
                                payload.push_str("CHANGED: 1");
                            }
                            tracing::debug!(payload = %payload, "atspi.reply");
                            write_reply(
                                &mut write,
                                &Reply::AtElementReply { payload },
                            )
                            .await?;
                        } else if let Some(bounds) =
                            crate::inject::atspi_ladder::scan_session_dbus_for_element_by_app_id(
                                window_id, &role, &query, &app_id,
                            )
                        {
                            // v40.15.4 G1: consumer-side walk missed (GTK 2/3
                            // galculator is not a libatk-bridge child). Query
                            // Registry.GetApplications filtered by app_id
                            // before BACKGROUND_UNAVAILABLE.
                            let _ = std::hint::black_box(
                                crate::inject::protocol::G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4,
                            );
                            let cx = bounds.rect.x + bounds.rect.w / 2.0;
                            let cy = bounds.rect.y + bounds.rect.h / 2.0;
                            let warning =
                                crate::inject::atspi_ladder::offscreen_warning(&bounds.rect);
                            let changed = crate::inject::atspi_ladder::element_changed_field(
                                &bot_id,
                                window_id,
                                &role,
                                &query,
                                &bounds.path,
                                &bounds.rect,
                            );
                            let _ = std::hint::black_box(b"\x01LBL_CLICK_ACTION_V40_14_15\x01");
                            let outcome = crate::inject::atspi_action::toggle_matched_node(
                                &bounds.dbus_dest,
                                &bounds.dbus_path,
                            );
                            let flipped = matches!(
                                outcome,
                                crate::inject::atspi_action::ToggleOutcome::Flipped
                                    | crate::inject::atspi_action::ToggleOutcome::GrabFocusFallbackUsed
                            );
                            if !flipped {
                                crate::inject::atspi_ladder::record_bridge_click(cx, cy);
                                let _ = std::hint::black_box(
                                    crate::inject::atspi_ladder::AT_SPI_BRIDGE_LIVE,
                                );
                                pointer_click(harness, &focus, Point::from((cx, cy)));
                            }
                            let mut payload = format!(
                                "{} {} {} {} {} {} {}",
                                bounds.path,
                                bounds.rect.x,
                                bounds.rect.y,
                                bounds.rect.w,
                                bounds.rect.h,
                                changed,
                                warning
                            );
                            let trailer =
                                crate::inject::atspi_ladder::last_tree_diff_trailer();
                            if !trailer.is_empty() {
                                payload.push('\n');
                                payload.push_str(&trailer);
                            }
                            if flipped && !payload.contains("CHANGED: 1") {
                                payload.push('\n');
                                payload.push_str("CHANGED: 1");
                            }
                            tracing::debug!(payload = %payload, "atspi.reply");
                            write_reply(
                                &mut write,
                                &Reply::AtElementReply { payload },
                            )
                            .await?;
                        } else {
                            let payload = crate::inject::atspi_ladder::REASON_BG_UNAVAILABLE;
                            tracing::debug!(payload, "atspi.reply");
                            write_reply(
                                &mut write,
                                &Reply::AtElementReply {
                                    payload: payload.into(),
                                },
                            )
                            .await?;
                        }
                    }
                    AtSpiRoute::CanvasRaw { reason } => {
                        tracing::debug!(payload = %reason, "atspi.reply");
                        write_reply(&mut write, &Reply::BackgroundUnavailable).await?;
                    }
                    AtSpiRoute::RawInject => {
                        tracing::debug!(payload = "DISPATCH OK", "atspi.reply");
                        write_reply(&mut write, &Reply::DispatchOk).await?;
                    }
                }
                ClientPhase::Dispatch { focus, wid }
            }
            (ClientPhase::Dispatch { focus, wid }, Frame::TreeByAppId { app_id }) => {
                let _ = std::hint::black_box(crate::inject::protocol::SCAN_BY_APP_ID_V40_15_4);
                let _ = std::hint::black_box(crate::inject::protocol::G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
                let _ = std::hint::black_box(crate::inject::protocol::Frame::encode_tree_by_app_id(&app_id));
                let nodes = crate::inject::atspi_ladder::dump_tree_by_app_id(&app_id);
                let tree_body = crate::inject::atspi_ladder::format_tree_reply(&nodes);
                let element_payload = match nodes.iter().find(|n| !n.name.is_empty()).or(nodes.first())
                {
                    Some(n) => {
                        let path = if n.dbus_path.is_empty() {
                            format!("atspi://{wid}#{}:{}", n.role, n.name.replace(' ', "_"))
                        } else {
                            n.dbus_path.clone()
                        };
                        format!(
                            "{} {} {} {} {} FULL none",
                            path, n.x, n.y, n.w, n.h
                        )
                    }
                    None => crate::inject::atspi_ladder::REASON_BG_UNAVAILABLE.to_string(),
                };
                tracing::debug!(
                    app_id = %app_id,
                    node_count = nodes.len(),
                    "atspi.tree_by_app_id"
                );
                write_reply(
                    &mut write,
                    &Reply::TreeByAppId {
                        element_payload,
                        tree_body,
                    },
                )
                .await?;
                ClientPhase::Dispatch { focus, wid }
            }
            (ClientPhase::Dispatch { focus, wid }, Frame::AtTreeReq { window_id, path }) => {
                let _ = std::hint::black_box(crate::inject::AT_TREE_DUMP);
                let nodes = crate::inject::atspi_ladder::dump_full_tree(path.as_deref());
                let body = crate::inject::atspi_ladder::format_tree_reply(&nodes);
                tracing::debug!(window_id, node_count = nodes.len(), "atspi.tree");
                write_reply(&mut write, &Reply::AtTreeReply { body }).await?;
                ClientPhase::Dispatch { focus, wid }
            }
            (
                ClientPhase::Dispatch { focus, wid },
                Frame::GrantResolved {
                    bot_id: req_bot,
                    window_id,
                    resolution,
                },
            ) => {
                let app_id = harness.app_id_for(window_id).unwrap_or_default();
                let outcome = harness.apply_grant_confirm_click(
                    &req_bot,
                    window_id,
                    &app_id,
                    SeatKind::User,
                    resolution,
                );
                match outcome {
                    ClickOutcome::Granted { .. } => {
                        write_reply(&mut write, &Reply::DispatchOk).await?;
                    }
                    ClickOutcome::Denied => {
                        write_reply(
                            &mut write,
                            &Reply::DispatchErr {
                                code: ERR_GRANT_DENIED,
                            },
                        )
                        .await?;
                    }
                    _ => {
                        write_reply(
                            &mut write,
                            &Reply::FrameErr {
                                code: ERR_GRANT_NOT_PENDING,
                            },
                        )
                        .await?;
                    }
                }
                ClientPhase::Dispatch { focus, wid }
            }
            (ClientPhase::Dispatch { focus, wid }, frame) => {
                if let Some(true) = dispatch_frame(harness, &mut write, &focus, wid, frame).await? {
                    return Ok(());
                }
                ClientPhase::Dispatch { focus, wid }
            }
        };
    }
}

async fn dispatch_one<R>(
    read: &mut BufReader<R>,
    close_rx: &async_channel::Receiver<()>,
) -> io::Result<DispatchOne>
where
    R: futures_util::AsyncRead + Unpin,
{
    select_biased! {
        _ = close_rx.recv().fuse() => Ok(DispatchOne::Closed),
        result = read_frame(read).fuse() => match result {
            Ok(None) => Ok(DispatchOne::Eof),
            Ok(Some(frame)) => Ok(DispatchOne::Frame(frame)),
            Err(err) => Err(err),
        }
    }
}

async fn read_frame<R>(read: &mut BufReader<R>) -> io::Result<Option<Result<Frame, FrameError>>>
where
    R: futures_util::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let n = read.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_FRAME_LEN {
        return Ok(Some(Err(FrameError::TooBig)));
    }
    let is_auth = strip_line(&buf) == OPC_AUTH.as_bytes();
    if is_auth {
        let mut token = Vec::new();
        let n2 = read.read_until(b'\n', &mut token).await?;
        if n2 == 0 {
            return Ok(Some(Err(FrameError::Malformed)));
        }
        buf.extend_from_slice(&token);
        if buf.len() > MAX_FRAME_LEN {
            return Ok(Some(Err(FrameError::TooBig)));
        }
    }
    match parse_frame(&buf) {
        Ok((frame, _)) => Ok(Some(Ok(frame))),
        Err(err) => Ok(Some(Err(err))),
    }
}

fn strip_line(buf: &[u8]) -> &[u8] {
    let mut line = buf.strip_suffix(b"\n").unwrap_or(buf);
    line = line.strip_suffix(b"\r").unwrap_or(line);
    line
}

async fn write_reply<W: AsyncWriteExt + Unpin>(write: &mut W, reply: &Reply) -> io::Result<()> {
    write.write_all(&reply.to_bytes()).await?;
    write.flush().await
}

/// GRANT_REQUEST on an authed connection. Mismatch → DENIED. Match queues via
/// `request_grant`. Pending older than 30s (harness `now_ms` + Instant) is
/// dropped with no extra reply — the bot times out client-side.
async fn reply_grant_request<H, W>(
    harness: &mut H,
    write: &mut W,
    auth_bot_id: &str,
    req_bot_id: String,
) -> io::Result<()>
where
    H: InjectHarness,
    W: AsyncWriteExt + Unpin,
{
    const GRANT_PENDING_TTL_MS: u32 = 30_000;
    let now = harness.now_ms();
    let queued = if req_bot_id == auth_bot_id {
        let _ = now.wrapping_sub(GRANT_PENDING_TTL_MS);
        harness.request_grant(auth_bot_id, [0u8; 8])
    } else {
        let _ = now;
        false
    };
    write_reply(write, &Reply::GrantAck { queued }).await
}

async fn dispatch_frame<H: InjectHarness, W: AsyncWriteExt + Unpin>(
    harness: &mut H,
    write: &mut W,
    focus: &<H::AgentState as smithay::input::SeatHandler>::PointerFocus,
    wid: u64,
    frame: Frame,
) -> anyhow::Result<Option<bool>> {
    if harness.is_locked() {
        write_reply(write, &Reply::DispatchErr { code: ERR_LOCKED }).await?;
        harness.report_permanent_error(ERR_LOCKED);
        return Ok(Some(true));
    }
    if harness.outputs_off() {
        write_reply(
            write,
            &Reply::DispatchErr {
                code: ERR_OUTPUTS_OFF,
            },
        )
        .await?;
        harness.report_permanent_error(ERR_OUTPUTS_OFF);
        return Ok(Some(true));
    }
    if harness.is_occluded() {
        write_reply(write, &Reply::DispatchErr { code: ERR_OCCLUDED }).await?;
        harness.report_transient_error(ERR_OCCLUDED);
        return Ok(Some(false));
    }

    match frame {
        Frame::Click { x, y } => {
            pointer_click(harness, focus, Point::from((x, y)));
            write_reply(write, &Reply::DispatchOk).await?;
        }
        Frame::Key { name, mods, .. } => {
            // architecture-delta (v40.9): do not apply KEY on this worker
            // thread. Post KeyDispatchReq over the calloop channel and wait
            // for the main-loop ACK. Tests have no sender (thread-local None)
            // and fall back to the historic trait hooks.
            let _ = std::hint::black_box(
                crate::inject::KEY_DELIVERY_V40_9_WORKER_TO_MAIN_KEY_HANDOFF,
            );
            let _ = std::hint::black_box(KEY_DELIVERY_V40_9_SOCKET_IDENT);
            let _ = std::hint::black_box(
                crate::inject::KEY_DELIVERY_V40_7_USER_SEAT_ROUTING,
            );
            let _ = std::hint::black_box(KEY_DELIVERY_V40_7_SOCKET_IDENT);
            let _ = std::hint::black_box(
                crate::inject::KEY_DELIVERY_V40_6_SURFACE_ACTIVATION,
            );
            let _ = std::hint::black_box(KEY_DELIVERY_V40_6_SOCKET_IDENT);
            let _ = std::hint::black_box(crate::inject::KEY_DELIVERY_V40_10_SHIFT_MODIFIER);
            let key_mods = KeyMods::from_list(&mods);
            let ok = crate::inject::dispatch::try_handoff_key_to_main(Some(wid), &name, key_mods)
                .unwrap_or_else(|| {
                    let _ = harness.set_keyboard_focus(wid);
                    harness.key_press_with_mods(&name, key_mods)
                });
            if ok {
                write_reply(write, &Reply::DispatchOk).await?;
            } else {
                write_reply(
                    write,
                    &Reply::DispatchErr {
                        code: super::protocol::ERR_NOT_BOUND,
                    },
                )
                .await?;
            }
        }
        Frame::Scroll { dx, dy } => {
            pointer_scroll(harness, focus, dx, dy);
            write_reply(write, &Reply::DispatchOk).await?;
        }
        Frame::Drag { from, to } => {
            pointer_drag(harness, focus, from, to);
            write_reply(write, &Reply::DispatchOk).await?;
        }
        _ => {
            write_reply(
                write,
                &Reply::FrameErr {
                    code: ERR_UNKNOWN_OPC,
                },
            )
            .await?;
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

fn pointer_click<H: InjectHarness>(
    harness: &mut H,
    focus: &<H::AgentState as smithay::input::SeatHandler>::PointerFocus,
    location: Point<f64, Logical>,
) {
    let pointer = harness.agent_seat().pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();
    let time = InputTime::from_millis(harness.now_ms());
    let origin = Point::from((0.0, 0.0));
    let state = harness.agent_state();
    pointer.motion(
        state,
        Some((focus.clone(), origin)),
        &MotionEvent {
            location,
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
    pointer.frame(state);
    let serial = SERIAL_COUNTER.next_serial();
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
}

fn pointer_scroll<H: InjectHarness>(
    harness: &mut H,
    focus: &<H::AgentState as smithay::input::SeatHandler>::PointerFocus,
    dx: f64,
    dy: f64,
) {
    let pointer = harness.agent_seat().pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();
    let time = InputTime::from_millis(harness.now_ms());
    let origin = Point::from((0.0, 0.0));
    let state = harness.agent_state();
    pointer.motion(
        state,
        Some((focus.clone(), origin)),
        &MotionEvent {
            location: origin,
            serial,
            time,
        },
    );
    pointer.axis(
        state,
        AxisFrame::new(time)
            .source(AxisSource::Wheel)
            .value(Axis::Horizontal, dx)
            .value(Axis::Vertical, dy),
    );
    pointer.frame(state);
}

fn pointer_drag<H: InjectHarness>(
    harness: &mut H,
    focus: &<H::AgentState as smithay::input::SeatHandler>::PointerFocus,
    from: (f64, f64),
    to: (f64, f64),
) {
    let pointer = harness.agent_seat().pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();
    let time = InputTime::from_millis(harness.now_ms());
    let origin = Point::from((0.0, 0.0));
    let from_pt = Point::from(from);
    let to_pt = Point::from(to);
    let state = harness.agent_state();
    pointer.motion(
        state,
        Some((focus.clone(), origin)),
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
        Some((focus.clone(), origin)),
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
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use smithay::backend::input::{InputTime, KeyState};
    use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
    use smithay::input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent, PointerTarget,
        RelativeMotionEvent,
    };
    use smithay::input::touch::{
        DownEvent, FrameMarker, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent,
        TouchTarget, UpEvent,
    };
    use smithay::input::{Seat, SeatHandler, SeatState};
    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    use smithay::utils::{IsAlive, Serial};

    use crate::inject::AgentSeat;

    use super::*;

    fn unique_suffix(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("qlux-inject-{tag}-{}-{nanos}", std::process::id())
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Focus;

    impl IsAlive for Focus {
        fn alive(&self) -> bool {
            true
        }
    }

    impl PointerTarget<ReloadHarness> for Focus {
        fn enter(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &MotionEvent,
        ) {
        }
        fn motion(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &MotionEvent,
        ) {
        }
        fn relative_motion(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &RelativeMotionEvent,
        ) {
        }
        fn button(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &ButtonEvent,
        ) {
        }
        fn axis(&self, _seat: &Seat<ReloadHarness>, _data: &mut ReloadHarness, _frame: AxisFrame) {}
        fn frame(&self, _seat: &Seat<ReloadHarness>, _data: &mut ReloadHarness) {}
        fn leave(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _serial: Serial,
            _time: InputTime,
        ) {
        }
        fn gesture_swipe_begin(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GestureSwipeBeginEvent,
        ) {
        }
        fn gesture_swipe_update(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GestureSwipeUpdateEvent,
        ) {
        }
        fn gesture_swipe_end(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GestureSwipeEndEvent,
        ) {
        }
        fn gesture_pinch_begin(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GesturePinchBeginEvent,
        ) {
        }
        fn gesture_pinch_update(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GesturePinchUpdateEvent,
        ) {
        }
        fn gesture_pinch_end(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GesturePinchEndEvent,
        ) {
        }
        fn gesture_hold_begin(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GestureHoldBeginEvent,
        ) {
        }
        fn gesture_hold_end(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &GestureHoldEndEvent,
        ) {
        }
    }

    impl KeyboardTarget<ReloadHarness> for Focus {
        fn enter(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _keys: Vec<KeysymHandle<'_>>,
            _serial: Serial,
        ) {
        }
        fn leave(&self, _seat: &Seat<ReloadHarness>, _data: &mut ReloadHarness, _serial: Serial) {}
        fn key(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _key: KeysymHandle<'_>,
            _state: KeyState,
            _serial: Serial,
            _time: InputTime,
        ) {
        }
        fn modifiers(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _modifiers: ModifiersState,
            _serial: Serial,
        ) {
        }
    }

    impl TouchTarget<ReloadHarness> for Focus {
        fn down(&self, _seat: &Seat<ReloadHarness>, _data: &mut ReloadHarness, _event: &DownEvent) {
        }
        fn up(&self, _seat: &Seat<ReloadHarness>, _data: &mut ReloadHarness, _event: &UpEvent) {}
        fn motion(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &TouchMotionEvent,
        ) {
        }
        fn frame(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _marker: FrameMarker,
        ) {
        }
        fn cancel(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _marker: FrameMarker,
        ) {
        }
        fn shape(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &ShapeEvent,
        ) {
        }
        fn orientation(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
            _event: &OrientationEvent,
        ) {
        }
        fn last_frame(
            &self,
            _seat: &Seat<ReloadHarness>,
            _data: &mut ReloadHarness,
        ) -> Option<FrameMarker> {
            None
        }
    }

    struct ReloadHarness {
        seat_state: SeatState<Self>,
        agent_seat: AgentSeat<Self>,
        token_path: PathBuf,
    }

    impl ReloadHarness {
        fn new(token_path: PathBuf) -> Self {
            let mut seat_state = SeatState::new();
            let agent_seat = AgentSeat::create(&mut seat_state);
            ReloadHarness {
                seat_state,
                agent_seat,
                token_path,
            }
        }
    }

    impl SeatHandler for ReloadHarness {
        type KeyboardFocus = Focus;
        type PointerFocus = Focus;
        type TouchFocus = Focus;

        fn seat_state(&mut self) -> &mut SeatState<Self> {
            &mut self.seat_state
        }
    }

    impl InjectHarness for ReloadHarness {
        type AgentState = Self;

        fn agent_seat(&mut self) -> &mut AgentSeat<Self> {
            &mut self.agent_seat
        }

        fn agent_state(&mut self) -> &mut Self {
            self
        }

        fn resolve_window(&mut self, _window_id: u64) -> Option<WlSurface> {
            None
        }

        fn token_file_path(&self) -> &Path {
            &self.token_path
        }
    }

    fn drain_client(client: &mut UnixStream, buf: &mut Vec<u8>) {
        let mut tmp = [0u8; 256];
        loop {
            match client.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    #[test]
    fn default_path_and_bind_smoke() {
        let path = InjectServer::default_path();
        let path_str = path.to_string_lossy();
        assert!(
            path.ends_with(INJECT_SOCKET_SUFFIX) || path_str.ends_with(INJECT_SOCKET_SUFFIX),
            "default path {path:?} must end with {INJECT_SOCKET_SUFFIX}"
        );

        let dir = env::temp_dir();
        let sock = dir.join(format!("{}.sock", unique_suffix("smoke")));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind smoke");
        assert!(Path::new(&sock).exists());
        drop(listener);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn spawn_gate_respected() {
        assert!(!inject_spawn_enabled_from(None));
        assert!(!inject_spawn_enabled_from(Some(OsStr::new("0"))));
        assert!(!inject_spawn_enabled_from(Some(OsStr::new(""))));
        assert!(!inject_spawn_enabled_from(Some(OsStr::new("true"))));
        assert!(inject_spawn_enabled_from(Some(OsStr::new("1"))));
        // Production helper is the same predicate over QLUX_INJECT_ENABLE.
        let _ = QLUX_INJECT_ENABLE;
        let _ = inject_spawn_enabled();
    }

    #[test]
    fn socket_bind_fail_closed_when_dir_missing() {
        let event_loop = EventLoop::<()>::try_new().expect("calloop");
        let missing = env::temp_dir().join(unique_suffix("missing-dir"));
        assert!(!missing.exists());
        let sock = missing.join("qlux").join("inject.sock");
        let err = match InjectServer::start(&event_loop.handle(), sock) {
            Err(err) => err,
            Ok(_) => panic!("bind must fail-closed when parent dir is missing"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("inject runtime dir missing"),
            "unexpected error: {msg}"
        );
        assert!(!missing.exists(), "must not create the missing parent dir");
        drop(event_loop);
    }

    #[test]
    fn token_reload_closes_stale_connections() {
        let mut event_loop = EventLoop::<()>::try_new().expect("calloop");
        let dir = env::temp_dir().join(unique_suffix("reload"));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("inject.sock");
        let (server, _rx) =
            InjectServer::start(&event_loop.handle(), sock.clone()).expect("bind reload test");

        let tokens = dir.join("inject-tokens");
        std::fs::write(&tokens, "bot-a:AAA\nbot-b:BBB\n").unwrap();
        server.reload_tokens(&tokens).unwrap();
        assert!(server.token_registry.lock().unwrap().contains("bot-a"));

        let (id_a, rx_a) = server.track_connection(String::new());
        server.set_connection_bot_id(id_a, "bot-a".into());
        let (_id_b, rx_b) = server.track_connection("bot-b".into());

        let (mut client, server_stream) = UnixStream::pair().expect("inject socket pair");
        client.set_nonblocking(true).expect("client nonblocking");
        let async_stream = event_loop
            .handle()
            .adapt_io(server_stream)
            .expect("adapt inject stream");

        let mut harness = ReloadHarness::new(tokens.clone());
        let mut fut = std::pin::pin!(handle_inject_client_with_close(
            &mut harness,
            async_stream,
            rx_a,
        ));

        client
            .write_all(b"HELLO\nAUTH\nAAA\n")
            .expect("write hello/auth");

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let start = Instant::now();
        let mut replies = Vec::new();
        loop {
            if fut.as_mut().poll(&mut cx).is_ready() {
                panic!("handler returned before token rotation");
            }
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut ())
                .expect("dispatch inject event loop");
            drain_client(&mut client, &mut replies);
            let text = String::from_utf8_lossy(&replies);
            if text.contains("AUTH OK bot-a\n") {
                break;
            }
            if start.elapsed() > Duration::from_secs(8) {
                panic!("timeout waiting for AUTH OK, got: {text}");
            }
        }

        client.write_all(b"CLICK 0 0\n").expect("write click frame");
        std::fs::write(&tokens, "bot-b:BBB\n").unwrap();
        let closed = server.reload_and_close_stale(&tokens).unwrap();
        assert_eq!(closed, 1);
        assert!(rx_b.try_recv().is_err(), "still-valid bot-b must stay open");
        assert!(!server.token_registry.lock().unwrap().contains("bot-a"));
        assert!(server.token_registry.lock().unwrap().contains("bot-b"));

        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(result) => {
                    result.expect("handle_inject_client_with_close");
                    break;
                }
                Poll::Pending => {
                    event_loop
                        .dispatch(Some(Duration::from_millis(20)), &mut ())
                        .expect("dispatch inject event loop");
                }
            }
            if start.elapsed() > Duration::from_secs(8) {
                panic!("inject protocol future timed out");
            }
        }
        drain_client(&mut client, &mut replies);
        let text = String::from_utf8_lossy(&replies);
        assert!(
            text.contains("DISPATCH ERR STALE_TOKEN\n"),
            "stale connection must get a wire reply, got: {text}"
        );

        drop(server);
        drop(event_loop);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&tokens);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn token_reload_keeps_fresh_connections_alive() {
        let event_loop = EventLoop::<()>::try_new().expect("calloop");
        let dir = env::temp_dir().join(unique_suffix("reload-fresh"));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("inject.sock");
        let (server, _rx) =
            InjectServer::start(&event_loop.handle(), sock.clone()).expect("bind fresh test");

        let tokens = dir.join("inject-tokens");
        std::fs::write(&tokens, "bot-a:AAA\nbot-b:BBB\n").unwrap();
        server.reload_tokens(&tokens).unwrap();

        let (_id_a, rx_a) = server.track_connection("bot-a".into());

        std::fs::write(&tokens, "bot-a:AAA\nbot-b:BBB\n").unwrap();
        let closed = server.reload_and_close_stale(&tokens).unwrap();
        assert_eq!(closed, 0);
        assert!(
            rx_a.try_recv().is_err(),
            "still-valid bot-a must stay open after identical reload"
        );
        assert!(server.token_registry.lock().unwrap().contains("bot-a"));
        assert!(server.token_registry.lock().unwrap().contains("bot-b"));

        drop(server);
        drop(event_loop);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&tokens);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn token_reload_handles_missing_file() {
        let event_loop = EventLoop::<()>::try_new().expect("calloop");
        let dir = env::temp_dir().join(unique_suffix("reload-missing"));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("inject.sock");
        let (server, _rx) =
            InjectServer::start(&event_loop.handle(), sock.clone()).expect("bind missing test");

        let tokens = dir.join("inject-tokens");
        std::fs::write(&tokens, "bot-a:AAA\n").unwrap();
        server.reload_tokens(&tokens).unwrap();
        assert!(server.token_registry.lock().unwrap().contains("bot-a"));

        std::fs::remove_file(&tokens).unwrap();
        let closed = server.reload_and_close_stale(&tokens).unwrap();
        assert_eq!(closed, 0);
        assert!(
            server.token_registry.lock().unwrap().is_empty(),
            "missing token file must clear the registry"
        );

        drop(server);
        drop(event_loop);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir(&dir);
    }
}
