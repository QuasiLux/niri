//! Unix-socket listener skeleton for the inject plane.
//!
//! Accepts connections on `$XDG_RUNTIME_DIR/qlux/inject.sock` (falling back to
//! the process temp dir). AUTH / HELLO / the input wire format are v3.

use std::env;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::Context;
use async_channel::{Receiver, Sender};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

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
