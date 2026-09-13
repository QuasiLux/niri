//! Inject-plane e2e: real fork + tmp socket + HELLO→AUTH→BIND→CLICK/SCROLL/KEY/DRAG.
//!
//! The child process binds via [`niri::inject::resolve_inject_socket_path`] (same
//! path resolution as `Niri::new`) and drives a Smithay `AgentSeat` through
//! `InjectServer::start`. The parent is the tagged-line client.

use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use niri::inject::AGENT_SEAT_NAME;

const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KNOWN_WINDOW: u64 = 42;

fn unique_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "qlux-inject-e2e-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("qlux")).expect("e2e runtime dir");
    dir
}

fn read_line(stream: &mut UnixStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(err) => panic!("e2e read: {err}"),
        }
    }
    String::from_utf8(buf).expect("utf8 reply")
}

fn send(stream: &mut UnixStream, bytes: &[u8]) {
    stream.write_all(bytes).expect("e2e write");
    stream.flush().expect("e2e flush");
}

fn connect_with_retry(path: &Path) -> UnixStream {
    let start = Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return s,
            Err(_) => {
                if start.elapsed() > Duration::from_secs(8) {
                    panic!("timeout connecting to inject socket {path:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn child_run_harness() {
    if std::env::var_os("QLUX_INJECT_E2E_HARNESS").is_none() {
        return;
    }
    let socket = niri::inject::resolve_inject_socket_path();
    let tokens = PathBuf::from(std::env::var("QLUX_INJECT_E2E_TOKENS").unwrap());
    let output = PathBuf::from(std::env::var("QLUX_INJECT_E2E_OUTPUT").unwrap());
    niri::inject_e2e::run_harness(socket, tokens, output);
}

#[test]
fn e2e_hello_auth_bind_click_scroll_key_drag() {
    if std::env::var_os("QLUX_INJECT_E2E_HARNESS").is_some() {
        return;
    }

    let runtime = unique_dir();
    let qlux = runtime.join("qlux");
    let socket = qlux.join("inject.sock");
    let tokens = runtime.join("inject-tokens");
    let output = runtime.join("harness.out");
    fs::write(&tokens, format!("bot-a:{TOKEN_A}\n")).unwrap();

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(&exe)
        .env("QLUX_INJECT_E2E_HARNESS", "1")
        .env("QLUX_INJECT_E2E_TOKENS", &tokens)
        .env("QLUX_INJECT_E2E_OUTPUT", &output)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("QLUX_INJECT_SOCKET_SUFFIX", "qlux/inject.sock")
        .env("QLUX_INJECT_ENABLE", "1")
        .args(["--exact", "child_run_harness", "--nocapture"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn e2e harness child");

    let mut stream = connect_with_retry(&socket);

    send(&mut stream, b"HELLO\n");
    let hello = read_line(&mut stream);
    assert_eq!(hello, "HELLO OK\n", "{hello:?}");

    send(&mut stream, format!("AUTH\n{TOKEN_A}\n").as_bytes());
    let auth = read_line(&mut stream);
    assert_eq!(auth, "AUTH OK bot-a\n", "{auth:?}");

    send(&mut stream, format!("BIND {KNOWN_WINDOW}\n").as_bytes());
    let bind = read_line(&mut stream);
    assert_eq!(bind, "BIND OK\n", "{bind:?}");

    send(&mut stream, b"CLICK 100 200\n");
    let click = read_line(&mut stream);
    assert_eq!(click, "DISPATCH OK\n", "{click:?}");

    send(&mut stream, b"SCROLL -3 7\n");
    let scroll = read_line(&mut stream);
    assert_eq!(scroll, "DISPATCH OK\n", "{scroll:?}");

    send(&mut stream, b"KEY XF86XK_AudioMute\n");
    let key = read_line(&mut stream);
    assert_eq!(key, "DISPATCH OK\n", "{key:?}");

    send(&mut stream, b"DRAG 10 20 30 40\n");
    let drag = read_line(&mut stream);
    assert_eq!(drag, "DISPATCH OK\n", "{drag:?}");

    stream.shutdown(Shutdown::Write).ok();
    let status = child.wait().expect("wait e2e child");
    assert!(status.success(), "harness child failed: {status}");

    let body = fs::read_to_string(&output).expect("read harness output");
    assert!(
        body.contains(&format!("agent_seat={AGENT_SEAT_NAME}")),
        "motion must land on the AGENT seat, got:\n{body}"
    );
    assert!(
        body.contains("motion_count=4"),
        "CLICK+SCROLL+DRAG should produce 4 pointer motions, got:\n{body}"
    );
    assert!(
        body.contains("last_motion=30,40"),
        "last motion should be the DRAG endpoint, got:\n{body}"
    );
    assert!(
        body.contains("axis_count=1"),
        "SCROLL must call pointer axis on the agent seat, got:\n{body}"
    );
    assert!(
        body.contains("key_count=1"),
        "KEY must be accepted on the agent seat, got:\n{body}"
    );
    assert!(
        body.contains("user_focused_window=99"),
        "user FocusedWindow must be unchanged, got:\n{body}"
    );
    assert!(
        body.contains("user_scroll_x=1.5"),
        "user-seat scroll_offset must not be consumed, got:\n{body}"
    );
    assert!(
        body.contains("user_scroll_y=2.5"),
        "user-seat scroll_offset must not be consumed, got:\n{body}"
    );

    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(&tokens);
    let _ = fs::remove_file(&output);
    let _ = fs::remove_dir_all(&runtime);
}
