//! Unix-socket listener skeleton and inject-client state machine.
//!
//! Accepts connections on `$XDG_RUNTIME_DIR/qlux/inject.sock` (falling back to
//! the process temp dir). Frame parsing lives in [`super::protocol`].

use std::env;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::Context;
use async_channel::{Receiver, Sender};
use calloop::io::Async;
use futures_util::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use futures_util::AsyncBufReadExt;
use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use super::protocol::{
    parse_frame, validate_token, Frame, FrameError, InjectHarness, Reply, TokenError, ERR_BAD_TOKEN,
    ERR_BAD_WINDOW, ERR_IO, ERR_LOCKED, ERR_NO_TOKEN_FILE, ERR_OCCLUDED, ERR_OUTPUTS_OFF,
    ERR_TOO_BIG, ERR_UNKNOWN_OPC, MAX_FRAME_LEN, OPC_AUTH,
};

/// Path suffix under the runtime dir. Kept as a string so the release ELF
/// grep can find it.
pub const INJECT_SOCKET_SUFFIX: &str = "qlux/inject.sock";

/// Bound inject-plane listener. Protocol handling is deferred to v3.
pub struct InjectServer {
    /// Filesystem path of the bound socket.
    pub socket_path: PathBuf,
    _incoming: Sender<UnixStream>,
}

impl InjectServer {
    /// `$XDG_RUNTIME_DIR/qlux/inject.sock`, or temp-dir fallback.
    pub fn default_path() -> PathBuf {
        runtime_dir().join(INJECT_SOCKET_SUFFIX)
    }

    /// Bind `socket_path` and push accepted streams onto a channel.
    ///
    /// The event-loop callback only `accept()`s. It does not parse frames.
    #[allow(dead_code)]
    pub fn start<T: 'static>(
        event_loop: &LoopHandle<'static, T>,
        socket_path: PathBuf,
    ) -> anyhow::Result<(Self, Receiver<UnixStream>)> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).context("error binding inject socket")?;
        listener
            .set_nonblocking(true)
            .context("error setting inject socket non-blocking")?;

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

        Ok((
            Self {
                socket_path,
                _incoming: tx,
            },
            rx,
        ))
    }
}

fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

/// Linux `BTN_LEFT` from `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;

enum ClientPhase<F> {
    Greeting,
    AwaitAuth { retries: u8 },
    AwaitBind,
    Dispatch { focus: F },
}

/// Drive HELLO → AUTH → BIND → dispatch on one accepted inject stream.
pub async fn handle_inject_client<H: InjectHarness>(
    harness: &mut H,
    stream: Async<'static, UnixStream>,
) -> anyhow::Result<()> {
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    let mut phase = ClientPhase::Greeting;

    loop {
        let frame = match read_frame(&mut read).await {
            Ok(None) => return Ok(()),
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(err))) => {
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
                    Ok(bot_id) => {
                        write_reply(&mut write, &Reply::AuthOk { bot_id }).await?;
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
                } else if let Some(focus) = harness.resolve_pointer_focus(window_id) {
                    write_reply(&mut write, &Reply::BindOk).await?;
                    ClientPhase::Dispatch { focus }
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
            (ClientPhase::Dispatch { .. }, Frame::Hello | Frame::Auth(_) | Frame::Bind(_)) => {
                write_reply(
                    &mut write,
                    &Reply::FrameErr {
                        code: ERR_UNKNOWN_OPC,
                    },
                )
                .await?;
                return Ok(());
            }
            (ClientPhase::Dispatch { focus }, frame) => {
                if let Some(true) = dispatch_frame(harness, &mut write, &focus, frame).await? {
                    return Ok(());
                }
                ClientPhase::Dispatch { focus }
            }
        };
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

async fn dispatch_frame<H: InjectHarness, W: AsyncWriteExt + Unpin>(
    harness: &mut H,
    write: &mut W,
    focus: &<H::AgentState as smithay::input::SeatHandler>::PointerFocus,
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
        Frame::Key(name) => {
            if harness.key_press(&name) {
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
    let time = harness.now_ms();
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
    let time = harness.now_ms();
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
    let time = harness.now_ms();
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
    use std::path::Path;

    use super::*;

    #[test]
    fn default_path_and_bind_smoke() {
        let path = InjectServer::default_path();
        let path_str = path.to_string_lossy();
        assert!(
            path.ends_with(INJECT_SOCKET_SUFFIX) || path_str.ends_with(INJECT_SOCKET_SUFFIX),
            "default path {path:?} must end with {INJECT_SOCKET_SUFFIX}"
        );

        let dir = env::temp_dir();
        let sock = dir.join(format!(
            "qlux-inject-smoke-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind smoke");
        assert!(Path::new(&sock).exists());
        drop(listener);
        let _ = std::fs::remove_file(&sock);
    }
}
