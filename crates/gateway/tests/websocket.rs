//! WebSocket admission and input ownership through the gateway binary. A stub
//! compositor records the command pipe, so the tests observe what the gateway
//! hands the desktop without `FFmpeg` or Xwayland.
#![expect(clippy::expect_used, reason = "tests fail by panicking")]

use std::fs;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use waywire_protocol::Decoder;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::InputLease;

/// The gateway gives up on a compositor that sends no video for 15 seconds,
/// so each test finishes well inside that.
struct Gateway {
    child: Child,
    address: SocketAddr,
    pipe: PathBuf,
    _directory: tempfile::TempDir,
}

impl Gateway {
    fn start() -> Self {
        let directory = tempfile::tempdir().expect("test directory");
        let compositor = directory.path().join("compositor");
        let pipe = directory.path().join("commands");
        // The shell keeps the event pipe open while `cat` records commands.
        fs::write(
            &compositor,
            format!("#!/bin/sh\ncat > '{}'\n", pipe.display()),
        )
        .expect("stub compositor");
        fs::set_permissions(&compositor, fs::Permissions::from_mode(0o755))
            .expect("executable stub compositor");
        let address = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port");
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-gateway"))
            .arg("--listen")
            .arg(address.to_string())
            // A public URL with a path still admits only its origin.
            .args(["--public-url", "https://viewer.test/desktop/"])
            .arg("--compositor")
            .arg(&compositor)
            .env_remove("PUBLIC_URL")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start gateway");
        let gateway = Self {
            child,
            address,
            pipe,
            _directory: directory,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(address).is_err() {
            assert!(Instant::now() < deadline, "gateway did not listen");
            thread::sleep(Duration::from_millis(10));
        }
        gateway
    }

    /// Open a WebSocket handshake and return the stream with the response head.
    fn upgrade(&self, path: &str, origin: Option<&str>) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("read timeout");
        let origin = origin.map_or_else(String::new, |origin| format!("Origin: {origin}\r\n"));
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{origin}\r\n",
            self.address
        )
        .expect("handshake request");
        // One byte at a time, so frames sent right after the head stay unread.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).expect("handshake response");
            head.push(byte[0]);
        }
        (stream, String::from_utf8_lossy(&head).into_owned())
    }

    fn control(&self) -> TcpStream {
        let (stream, head) = self.upgrade("/control", Some("https://viewer.test"));
        assert!(head.starts_with("HTTP/1.1 101"), "{head:?}");
        stream
    }

    /// Wait until the compositor pipe holds exactly `expected`.
    fn assert_commands(&self, expected: &[Command]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let bytes = fs::read(&self.pipe).unwrap_or_default();
            let commands = Decoder::<Command>::new()
                .push(&bytes)
                .expect("gateway writes valid commands");
            if commands == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "compositor received {commands:?}, expected {expected:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn send_text(stream: &mut TcpStream, text: &str) {
    let length = u8::try_from(text.len()).expect("short test message");
    assert!(length < 126);
    // A masked text frame; the zero mask leaves the payload unchanged.
    let mut frame = vec![0x81, 0x80 | length, 0, 0, 0, 0];
    frame.extend_from_slice(text.as_bytes());
    stream.write_all(&frame).expect("send frame");
}

/// Read text frames until `expected` arrives.
fn expect_text(stream: &mut TcpStream, expected: &str) {
    loop {
        let mut header = [0; 2];
        stream.read_exact(&mut header).expect("frame header");
        let length = match header[1] & 0x7f {
            126 => {
                let mut length = [0; 2];
                stream.read_exact(&mut length).expect("frame length");
                usize::from(u16::from_be_bytes(length))
            }
            127 => panic!("unexpectedly long frame"),
            length => usize::from(length),
        };
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).expect("frame payload");
        if header[0] & 0x0f == 1 && payload == expected.as_bytes() {
            return;
        }
    }
}

const ACTIVE: &str = r#"{"type":"control-state","state":"active"}"#;
const BUSY: &str = r#"{"type":"control-state","state":"busy"}"#;
const READY: &str = r#"{"type":"control-state","state":"ready"}"#;

fn lease(held: bool) -> Command {
    Command::InputLease(InputLease { held })
}

#[test]
fn upgrades_without_the_public_url_origin_are_forbidden() {
    let gateway = Gateway::start();
    for path in ["/control", "/stream"] {
        for origin in [
            None,
            Some("https://evil.test"),
            Some("http://viewer.test"),
            Some("https://viewer.test:8443"),
            Some("https://viewer.test.evil.test"),
            Some("https://viewer.test/desktop"),
            Some("null"),
        ] {
            let (_stream, head) = gateway.upgrade(path, origin);
            assert!(
                head.starts_with("HTTP/1.1 403"),
                "{path} from {origin:?} should be forbidden: {head:?}"
            );
        }
        let (_stream, head) = gateway.upgrade(path, Some("https://viewer.test"));
        assert!(head.starts_with("HTTP/1.1 101"), "{path}: {head:?}");
    }
}

#[test]
fn upgrades_past_the_connection_limit_are_unavailable_until_one_closes() {
    let gateway = Gateway::start();
    // Stream sockets have no read deadline, so they stay admitted while open.
    let mut open: Vec<TcpStream> = (0..32)
        .map(|_| {
            let (stream, head) = gateway.upgrade("/stream", Some("https://viewer.test"));
            assert!(head.starts_with("HTTP/1.1 101"), "{head:?}");
            stream
        })
        .collect();
    for path in ["/control", "/stream"] {
        let (_stream, head) = gateway.upgrade(path, Some("https://viewer.test"));
        assert!(
            head.starts_with("HTTP/1.1 503"),
            "a 33rd {path} upgrade should be unavailable: {head:?}"
        );
    }
    let (_stream, head) = gateway.upgrade("/control", Some("https://evil.test"));
    assert!(head.starts_with("HTTP/1.1 403"), "{head:?}");

    drop(open.pop());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut control = loop {
        let (stream, head) = gateway.upgrade("/control", Some("https://viewer.test"));
        if head.starts_with("HTTP/1.1 101") {
            break stream;
        }
        assert!(head.starts_with("HTTP/1.1 503"), "{head:?}");
        assert!(
            Instant::now() < deadline,
            "a closed socket should return its admission"
        );
        thread::sleep(Duration::from_millis(10));
    };
    send_text(&mut control, r#"{"type":"acquire"}"#);
    expect_text(&mut control, ACTIVE);
    let (_stream, head) = gateway.upgrade("/control", Some("https://viewer.test"));
    assert!(head.starts_with("HTTP/1.1 503"), "{head:?}");
}

#[test]
fn disconnecting_the_input_owner_hands_input_to_a_waiting_socket() {
    let gateway = Gateway::start();
    // The gateway starts by telling the compositor no browser holds input.
    let mut expected = vec![lease(false)];
    gateway.assert_commands(&expected);
    // An abrupt TCP close and a WebSocket close frame both end the session.
    for close_frame in [false, true] {
        let mut owner = gateway.control();
        send_text(&mut owner, r#"{"type":"acquire"}"#);
        expect_text(&mut owner, ACTIVE);
        expected.push(lease(true));
        gateway.assert_commands(&expected);

        let mut waiting = gateway.control();
        expect_text(&mut waiting, BUSY);
        send_text(&mut waiting, r#"{"type":"acquire"}"#);
        expect_text(&mut waiting, BUSY);

        // The close frame alone must end the session: its TCP connection
        // stays open until the handback has been checked.
        let open = if close_frame {
            owner
                .write_all(&[0x88, 0x80, 0, 0, 0, 0])
                .expect("close frame");
            Some(owner)
        } else {
            drop(owner);
            None
        };
        expected.push(lease(false));
        gateway.assert_commands(&expected);
        expect_text(&mut waiting, READY);
        drop(open);

        send_text(&mut waiting, r#"{"type":"acquire"}"#);
        expect_text(&mut waiting, ACTIVE);
        expected.push(lease(true));
        gateway.assert_commands(&expected);
        drop(waiting);
        expected.push(lease(false));
        gateway.assert_commands(&expected);
    }
}
