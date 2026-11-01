use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{
    self,
};
use std::os::unix::fs::PermissionsExt;
use std::process::Child;
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use waywire_protocol::Decoder;
use waywire_protocol::Record;
use waywire_protocol::pipe::Button;
use waywire_protocol::pipe::ButtonState;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::CursorShape;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::InputSequence;
use waywire_protocol::pipe::InputText;
use waywire_protocol::pipe::KeyCode;
use waywire_protocol::pipe::KeyState;
use waywire_protocol::pipe::KeyboardKey;
use waywire_protocol::pipe::KeyframeReadiness;
use waywire_protocol::pipe::KeyframeState;
use waywire_protocol::pipe::PointerAbsolute;
use waywire_protocol::pipe::PointerButton;
use waywire_protocol::pipe::PointerCoordinate;
use waywire_protocol::pipe::RequestId;
use waywire_protocol::pipe::Resize;
use waywire_protocol::pipe::ScaleV120;
use waywire_protocol::pipe::Text;
use waywire_protocol::pipe::TextAction;

struct Compositor(Child);

impl Drop for Compositor {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helper requires a live compositor pipe"
)]
fn events(child: &mut Child, decoder: &mut Decoder<Event>) -> Vec<Event> {
    let mut bytes = [0; 4096];
    let mut result = Vec::new();
    loop {
        match child.stdout.as_mut().expect("event pipe").read(&mut bytes) {
            Ok(0) => panic!("compositor event pipe closed"),
            Ok(n) => result.extend(decoder.push(&bytes[..n]).expect("decode events")),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return result,
            Err(error) => panic!("read events: {error}"),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helper requires Linux process status"
)]
fn switches(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("compositor status");
    status
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name == "voluntary_ctxt_switches" || name == "nonvoluntary_ctxt_switches")
                .then(|| value.trim().parse::<u64>().expect("context switches"))
        })
        .sum()
}

/// Wait for a file a test client writes once, and return its contents. Python's
/// `open(path, 'w').write(text)` creates the file before it writes the text, so
/// a reader that only waits for the file to exist can read it empty.
fn written_contents(path: &std::path::Path, deadline: Instant, what: &str) -> String {
    loop {
        match fs::read_to_string(path) {
            Ok(contents) if !contents.is_empty() => return contents,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("read {}: {error}", path.display()),
        }
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn idle_window_delay(now: Duration) -> Duration {
    let elapsed = Duration::new(now.as_secs() % 60, now.subsec_nanos());
    let remaining = Duration::from_mins(1).saturating_sub(elapsed);
    if remaining < Duration::from_secs(3) {
        remaining + Duration::from_secs(1)
    } else {
        Duration::ZERO
    }
}

#[test]
fn idle_window_waits_only_near_minute_boundary() {
    assert_eq!(idle_window_delay(Duration::from_secs(57)), Duration::ZERO);
    assert_eq!(
        idle_window_delay(Duration::from_millis(57_500)),
        Duration::from_millis(3500)
    );
    assert_eq!(
        idle_window_delay(Duration::from_millis(59_950)),
        Duration::from_millis(1050)
    );
}

#[test]
#[ignore = "requires FFmpeg and a working Xwayland on PATH"]
fn xwayland_starts_with_sigterm_unblocked() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600"])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let pid = compositor.0.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    let xwayland = loop {
        let children = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .expect("compositor children");
        if let Some(child) = children.split_whitespace().find(|child| {
            fs::read_to_string(format!("/proc/{child}/comm"))
                .is_ok_and(|name| name.trim() == "Xwayland")
        }) {
            break child.to_owned();
        }
        assert!(Instant::now() < deadline, "Xwayland did not start");
        thread::sleep(Duration::from_millis(10));
    };
    let status = fs::read_to_string(format!("/proc/{xwayland}/status")).expect("Xwayland status");
    let blocked = status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .expect("signal mask");
    let blocked = u64::from_str_radix(blocked.trim(), 16).expect("hex signal mask");
    assert_eq!(
        blocked & (1 << (nix::libc::SIGTERM - 1)),
        0,
        "SIGTERM is blocked"
    );
}

#[test]
#[ignore = "requires FFmpeg and a working Xwayland on PATH"]
fn idle_compositor_stops_rendering_and_emitting_frames() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args([
                "--encoder",
                "ffmpeg",
                "--rtp-port",
                &port,
                "--resolution",
                "1280x720",
                "--frame-rate",
                "30",
            ])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking events");
    let mut decoder = Decoder::<Event>::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(generation) = events(&mut compositor.0, &mut decoder)
            .into_iter()
            .find_map(|event| {
                if let Event::Frame(frame) = event {
                    Some(frame.generation)
                } else {
                    None
                }
            })
        {
            let ack = Command::KeyframeReadiness(KeyframeReadiness {
                generation,
                state: KeyframeState::Cached,
            });
            compositor
                .0
                .stdin
                .as_mut()
                .expect("command pipe")
                .write_all(&ack.encode())
                .expect("acknowledge keyframe");
            break;
        }
        assert!(Instant::now() < deadline, "first frame deadline");
        thread::sleep(Duration::from_millis(10));
    }
    // A screen that holds still is encoded again a few times to refine it, then not at all.
    thread::sleep(Duration::from_secs(2));
    let refreshes = events(&mut compositor.0, &mut decoder)
        .iter()
        .filter(|event| matches!(event, Event::Frame(_)))
        .count();
    assert!(
        refreshes >= 6,
        "still screen refreshed {refreshes} times, not at least 6"
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock");
    thread::sleep(idle_window_delay(now));
    events(&mut compositor.0, &mut decoder);
    let before = switches(compositor.0.id());
    thread::sleep(Duration::from_secs(2));
    let wakeups = switches(compositor.0.id()) - before;
    assert!(
        wakeups < 25,
        "idle compositor woke {wakeups} times in two seconds"
    );
    assert!(
        events(&mut compositor.0, &mut decoder)
            .iter()
            .all(|event| !matches!(event, Event::Frame(_))),
        "idle compositor emitted a frame"
    );
}

#[test]
#[ignore = "requires a working Xwayland on PATH"]
fn in_process_encoder_starts_with_a_key_frame_whose_stripes_tile_the_output() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args([
                "--rtp-port",
                "9",
                "--encoder",
                "openh264",
                "--stripes",
                "3",
                "--resolution",
                "800x600",
            ])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking events");
    let mut decoder = Decoder::<Event>::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let frame = loop {
        if let Some(frame) = events(&mut compositor.0, &mut decoder)
            .into_iter()
            .find_map(|event| {
                if let Event::EncodedFrame(frame) = event {
                    Some(frame)
                } else {
                    None
                }
            })
        {
            break frame;
        }
        assert!(Instant::now() < deadline, "first frame deadline");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(frame.key, "first frame is not a key frame");
    assert_eq!(
        (frame.metadata.width.get(), frame.metadata.height.get()),
        (800, 600)
    );
    // Stripe container: y and height (u16 LE), flags, length (u32 LE), access unit.
    let mut rows = Vec::new();
    let mut data = &frame.data[..];
    while !data.is_empty() {
        let y = u16::from_le_bytes([data[0], data[1]]);
        let height = u16::from_le_bytes([data[2], data[3]]);
        let length = u32::from_le_bytes([data[5], data[6], data[7], data[8]]) as usize;
        assert_eq!(data[4], 1, "stripe at {y} is not a key frame");
        assert!(
            data[9..9 + length].starts_with(&[0, 0, 0, 1]),
            "stripe at {y} is not Annex-B"
        );
        rows.push((y, height));
        data = &data[9 + length..];
    }
    assert_eq!(rows, [(0, 200), (200, 200), (400, 200)]);
}

#[test]
#[ignore = "requires FFmpeg and a working Xwayland on PATH"]
fn compositor_exits_promptly_when_the_session_program_exits() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let marker = runtime.path().join("exited");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "640x360", "--"])
            .args(["sh", "-c", "sleep 0.5; touch \"$0\""])
            .arg(&marker)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "session program did not run");
        thread::sleep(Duration::from_millis(5));
    }
    let exited = Instant::now();
    while compositor
        .0
        .try_wait()
        .expect("compositor status")
        .is_none()
    {
        assert!(
            exited.elapsed() < Duration::from_millis(500),
            "compositor still running {:?} after its session program exited",
            exited.elapsed()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, xterm and xdotool"]
fn x11_requested_position_is_honored_and_constrained() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let marker = runtime.path().join("xterm-window");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let _compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .args([
                "sh",
                "-c",
                r#"
                xterm -title waywire-position -geometry 40x10+120+100 &
                app=$!
                trap 'kill "$app" 2>/dev/null' EXIT
                while :; do
                    id=$(xdotool search --pid "$app" --onlyvisible 2>/dev/null | head -n 1)
                    [ -n "$id" ] && break
                    sleep 0.05
                done
                sleep 0.5
                printf '%s %s %s' "$DISPLAY" "$WAYLAND_DISPLAY" "$id" > "$0"
                wait "$app"
                "#,
            ])
            .arg(&marker)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let target = written_contents(&marker, deadline, "xterm did not map");
    let mut target = target.split_whitespace();
    let display = target.next().expect("display");
    let wayland = target.next().expect("Wayland display");
    let id = target.next().expect("window id");
    let geometry = || {
        let output = ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args(["getwindowgeometry", "--shell", id])
            .output()
            .expect("query geometry");
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).expect("geometry UTF-8");
        let dimension = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .expect("geometry dimension")
                .parse::<i32>()
                .expect("integer dimension")
        };
        (
            dimension("X="),
            dimension("Y="),
            dimension("WIDTH="),
            dimension("HEIGHT="),
        )
    };
    let (x, y, _, _) = geometry();
    assert_eq!((x, y), (120, 100), "initial requested position");
    for (x, y) in [(2000, 2000), (150, 180)] {
        let status = ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args(["windowmove", id, &x.to_string(), &y.to_string()])
            .status()
            .expect("move window");
        assert!(status.success());
        thread::sleep(Duration::from_millis(300));
        let (actual_x, actual_y, width, height) = geometry();
        if x == 2000 {
            assert!(actual_x >= 0 && actual_x + width <= 800);
            assert!(actual_y >= 24 && actual_y + height <= 568);
        } else {
            assert_eq!((actual_x, actual_y), (x, y), "requested position");
        }
    }
    if let Some(path) = std::env::var_os("WAYWIRE_WM_SCREENSHOT") {
        let status = ProcessCommand::new("grim")
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("WAYLAND_DISPLAY", wayland)
            .arg(path)
            .status()
            .expect("capture screenshot");
        assert!(status.success());
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, GTK3 Python bindings and xdotool"]
fn x11_pre_map_maximize_fills_work_area() {
    x11_pre_map_expanded_fills_work_area("maximize", 540);
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk, xdotool, xprop and grim"]
#[expect(
    clippy::too_many_lines,
    reason = "the X11 session and minimize checks form one integration scenario"
)]
fn x11_minimize_requests_update_wm_state() {
    use smithay::reexports::x11rb::connection::Connection as _;
    use smithay::reexports::x11rb::protocol::xproto::ClientMessageEvent;
    use smithay::reexports::x11rb::protocol::xproto::ConnectionExt as _;
    use smithay::reexports::x11rb::protocol::xproto::EventMask;
    let runtime = tempfile::tempdir().expect("runtime");
    let script = runtime.path().join("minimize.py");
    let ready = runtime.path().join("ready");
    fs::write(&script, format!("#!/usr/bin/python3\nimport os, tkinter as tk\nr=tk.Tk()\nr.title('waywire-minimize-test')\nr.geometry('400x300+100+100')\nr.update()\nopen({:?}, 'w').write(str(os.getpid()))\nr.mainloop()\n", ready.to_string_lossy())).expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let _compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .arg(&script)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = written_contents(&ready, deadline, "client did not map");
    let environ = fs::read(format!("/proc/{pid}/environ")).expect("environment");
    let env = |name: &[u8]| {
        environ
            .split(|b| *b == 0)
            .find_map(|e| e.strip_prefix(name))
            .and_then(|v| std::str::from_utf8(v).ok())
            .expect("environment value")
    };
    let display = env(b"DISPLAY=");
    let output = ProcessCommand::new("xdotool")
        .env("DISPLAY", display)
        .args(["search", "--name", "waywire-minimize-test"])
        .output()
        .expect("xdotool");
    let id = String::from_utf8(output.stdout)
        .expect("id")
        .lines()
        .next()
        .expect("window")
        .parse::<u32>()
        .expect("number");
    let (conn, screen) = smithay::reexports::x11rb::connect(Some(display)).expect("X11 connection");
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| {
        conn.intern_atom(false, name)
            .expect("intern")
            .reply()
            .expect("atom")
            .atom
    };
    let change = atom(b"WM_CHANGE_STATE");
    let net_state = atom(b"_NET_WM_STATE");
    let hidden = atom(b"_NET_WM_STATE_HIDDEN");
    let send = |ty, data: [u32; 5]| {
        let event = ClientMessageEvent::new(32, id, ty, data);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .expect("send");
        conn.flush().expect("flush");
    };
    let check = |minimized: bool, phase: &str| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = ProcessCommand::new("xprop")
                .env("DISPLAY", display)
                .args(["-id", &id.to_string(), "WM_STATE", "_NET_WM_STATE"])
                .output()
                .expect("xprop");
            let props = String::from_utf8(output.stdout).expect("properties");
            if props.contains(if minimized { "Iconic" } else { "Normal" })
                && props.contains("_NET_WM_STATE_HIDDEN") == minimized
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unexpected X11 state after {phase}: {props}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    send(change, [3, 0, 0, 0, 0]);
    check(true, "WM_CHANGE_STATE minimize");
    if let Some(path) = std::env::var_os("WAYWIRE_WM_SCREENSHOT") {
        thread::sleep(Duration::from_millis(300));
        assert!(
            ProcessCommand::new("grim")
                .env("XDG_RUNTIME_DIR", runtime.path())
                .env("WAYLAND_DISPLAY", env(b"WAYLAND_DISPLAY="))
                .arg(path)
                .status()
                .expect("grim")
                .success()
        );
    }
    send(change, [1, 0, 0, 0, 0]);
    check(false, "WM_CHANGE_STATE restore");
    send(net_state, [1, hidden, 0, 1, 0]);
    check(true, "hidden add");
    send(net_state, [0, hidden, 0, 1, 0]);
    check(false, "hidden remove");
    send(net_state, [2, hidden, 0, 1, 0]);
    check(true, "hidden toggle on");
    send(net_state, [2, hidden, 0, 1, 0]);
    check(false, "hidden toggle off");
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, GTK3 Python bindings and xdotool"]
fn x11_pre_map_fullscreen_fills_work_area() {
    x11_pre_map_expanded_fills_work_area("fullscreen", 568);
}

#[expect(
    clippy::expect_used,
    reason = "integration scenario requires a live GTK client"
)]
fn x11_pre_map_expanded_fills_work_area(mode: &str, height: i32) {
    let runtime = tempfile::tempdir().expect("runtime");
    let script = runtime.path().join("maximized.py");
    let ready = runtime.path().join("ready");
    fs::write(
        &script,
        format!(
            r"#!/usr/bin/python3
import os
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib
window = Gtk.Window(title='waywire-premap-maximized')
window.set_default_size(300, 200)
window.{}()
window.show_all()
GLib.timeout_add(500, lambda: (open({:?}, 'w').write(str(os.getpid())), False)[1])
Gtk.main()
",
            mode,
            ready.to_string_lossy()
        ),
    )
    .expect("write GTK script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let _compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .arg(&script)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("GDK_BACKEND", "x11")
            .env("GTK_CSD", "0")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = written_contents(&ready, deadline, "GTK window did not map");
    let environ = fs::read(format!("/proc/{pid}/environ")).expect("environment");
    let environment = |name: &[u8]| {
        environ
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(name))
            .and_then(|value| std::str::from_utf8(value).ok())
            .expect("display environment")
    };
    let display = environment(b"DISPLAY=");
    // GTK reports the map before the window manager has made it viewable.
    let deadline = Instant::now() + Duration::from_secs(5);
    let id = loop {
        let id = ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args([
                "search",
                "--onlyvisible",
                "--name",
                "waywire-premap-maximized",
            ])
            .output()
            .expect("find window");
        if id.status.success() || Instant::now() >= deadline {
            break id;
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(id.status.success(), "window never became viewable");
    let id = String::from_utf8(id.stdout).expect("window id");
    let geometry = ProcessCommand::new("xdotool")
        .env("DISPLAY", display)
        .args(["getwindowgeometry", "--shell", id.trim()])
        .output()
        .expect("window geometry");
    assert!(geometry.status.success());
    let geometry = String::from_utf8(geometry.stdout).expect("geometry");
    assert!(geometry.contains("WIDTH=800\n"), "{geometry}");
    assert!(
        geometry.contains(&format!("HEIGHT={height}\n")),
        "{geometry}"
    );
    if let Some(path) = std::env::var_os("WAYWIRE_WM_SCREENSHOT") {
        let status = ProcessCommand::new("grim")
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("WAYLAND_DISPLAY", environment(b"WAYLAND_DISPLAY="))
            .arg(path)
            .status()
            .expect("capture screenshot");
        assert!(status.success());
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, distro gtk3-demo and xdotool"]
fn gtk3_x11_window_keeps_its_size_after_output_resizes() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let marker = runtime.path().join("gtk-window");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "1280x720", "--"])
            .args([
                "sh",
                "-c",
                r#"
                GDK_BACKEND=x11 GTK_CSD=1 gtk3-demo &
                app=$!
                trap 'kill "$app" 2>/dev/null' EXIT
                while :; do
                    id=$(xdotool search --pid "$app" --onlyvisible 2>/dev/null | head -n 1)
                    [ -n "$id" ] && break
                    sleep 0.05
                done
                sleep 0.3
                xdotool windowsize "$id" 1100 680
                sleep 0.3
                printf '%s %s' "$DISPLAY" "$id" > "$0"
                wait "$app"
            "#,
            ])
            .arg(&marker)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let target = written_contents(&marker, deadline, "GTK3 window did not map");
    let (display, id) = target.split_once(' ').expect("display and window id");
    let geometry = || {
        let output = ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args(["getwindowgeometry", "--shell", id])
            .output()
            .expect("query GTK3 geometry");
        assert!(output.status.success(), "xdotool geometry failed");
        let text = String::from_utf8(output.stdout).expect("geometry UTF-8");
        let dimension = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .expect("geometry dimension")
                .parse::<i32>()
                .expect("integer dimension")
        };
        (
            dimension("X="),
            dimension("Y="),
            dimension("WIDTH="),
            dimension("HEIGHT="),
        )
    };
    let original = geometry();
    assert_eq!((original.2, original.3), (1100, 680));
    for i in 0..6 {
        let resize = Command::Resize(Resize {
            size: FrameSize::new(if i % 2 == 0 { 1282 } else { 1280 }, 720).expect("size"),
            scale_v120: ScaleV120::new(120).expect("scale"),
            request_id: RequestId::new(i + 1).expect("request id"),
        });
        compositor
            .0
            .stdin
            .as_mut()
            .expect("command pipe")
            .write_all(&resize.encode())
            .expect("resize output");
        thread::sleep(Duration::from_millis(250));
        assert_eq!(
            geometry(),
            original,
            "GTK3 buffer moved or shrank after output resize {i}"
        );
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, GTK3 Python bindings, xdotool and grim"]
#[expect(
    clippy::too_many_lines,
    reason = "the GTK dialog and menu form one integration scenario"
)]
fn gtk3_x11_dialog_menu_does_not_minimize() {
    let runtime = tempfile::tempdir().expect("runtime");
    let script = runtime.path().join("dialog.py");
    let ready = runtime.path().join("ready");
    fs::write(
        &script,
        format!(
            r"#!/usr/bin/python3
import os
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib
parent = Gtk.Window(title='waywire-dialog-parent')
parent.set_default_size(500, 300)
parent.show_all()
dialog = Gtk.Dialog(title='waywire-modal-dialog', transient_for=parent, modal=True)
dialog.set_default_size(320, 150)
dialog.show_all()
GLib.timeout_add(500, lambda: (open({:?}, 'w').write(str(os.getpid())), False)[1])
Gtk.main()
",
            ready.to_string_lossy()
        ),
    )
    .expect("write GTK script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .arg(&script)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("GDK_BACKEND", "x11")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = written_contents(&ready, deadline, "GTK dialog did not map");
    let environ = fs::read(format!("/proc/{pid}/environ")).expect("environment");
    let display = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"DISPLAY="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .expect("DISPLAY");
    let wayland = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"WAYLAND_DISPLAY="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .expect("WAYLAND_DISPLAY");
    let xdotool = |args: &[&str]| {
        ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args(args)
            .output()
            .expect("xdotool")
    };
    let search = xdotool(&["search", "--name", "waywire-modal-dialog"]);
    let id = String::from_utf8(search.stdout).expect("dialog id");
    let id = id.lines().next().expect("dialog window");
    let geometry = xdotool(&["getwindowgeometry", "--shell", id]);
    let geometry = String::from_utf8(geometry.stdout).expect("geometry");
    let value = |key: &str| {
        geometry
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .expect("coordinate")
            .parse::<u32>()
            .expect("number")
    };
    let (x, y) = (value("X=") + 60, value("Y=") - 14);
    let stdin = compositor.0.stdin.as_mut().expect("stdin");
    let mut seq = 0;
    let mut send = |command: Command| stdin.write_all(&command.encode()).expect("input");
    seq += 1;
    send(Command::PointerAbsolute(PointerAbsolute {
        x: PointerCoordinate::new(x * 65535 / 800).expect("x"),
        y: PointerCoordinate::new(y * 65535 / 600).expect("y"),
        sequence: InputSequence::new(seq).expect("sequence"),
    }));
    for state in [ButtonState::Pressed, ButtonState::Released] {
        seq += 1;
        send(Command::PointerButton(PointerButton {
            button: Button::Right,
            state,
            sequence: InputSequence::new(seq).expect("sequence"),
        }));
    }
    seq += 1;
    send(Command::PointerAbsolute(PointerAbsolute {
        x: PointerCoordinate::new((x + 10) * 65535 / 800).expect("x"),
        y: PointerCoordinate::new((y + 13) * 65535 / 600).expect("y"),
        sequence: InputSequence::new(seq).expect("sequence"),
    }));
    let pixel = || {
        // grim can fail transiently on a loaded host; retry the capture only.
        let deadline = Instant::now() + Duration::from_secs(5);
        let output = loop {
            let output = ProcessCommand::new("grim")
                .env("WAYLAND_DISPLAY", wayland)
                .env("XDG_RUNTIME_DIR", runtime.path())
                .args([
                    "-t",
                    "ppm",
                    "-g",
                    &format!("{},{} 1x1", x + 10, y + 40),
                    "-",
                ])
                .output()
                .expect("capture menu");
            if output.status.success() || Instant::now() >= deadline {
                break output;
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert!(
            output.status.success(),
            "grim failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.ends_with(&[32, 37, 45]),
            "menu not visible: {:?}",
            output.stdout
        );
    };
    thread::sleep(Duration::from_millis(300));
    pixel();
    for state in [ButtonState::Pressed, ButtonState::Released] {
        seq += 1;
        send(Command::PointerButton(PointerButton {
            button: Button::Left,
            state,
            sequence: InputSequence::new(seq).expect("sequence"),
        }));
    }
    thread::sleep(Duration::from_millis(300));
    pixel();
    let focused = xdotool(&["getwindowfocus"]);
    assert_eq!(
        String::from_utf8(focused.stdout).expect("focus").trim(),
        id,
        "dialog lost focus after clicking disabled Minimize"
    );
}

fn text_client_script(wayland: bool, output: &std::path::Path) -> String {
    if wayland {
        format!(
            r"#!/usr/bin/python3
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk
window = Gtk.Window()
entry = Gtk.Entry()
window.add(entry)
entry.connect('changed', lambda entry: open('{0}', 'w').write(entry.get_text()))
window.connect('destroy', Gtk.main_quit)
window.show_all()
entry.grab_focus()
open('{0}', 'w').close()
Gtk.main()
",
            output.display()
        )
    } else {
        format!(
            "#!/bin/sh\nstty raw -echo; exec cat > {}\n",
            output.display()
        )
    }
}

#[expect(
    clippy::expect_used,
    reason = "integration test requires a running client"
)]
fn text_reaches_terminal(wayland: bool) {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let output = runtime.path().join("received");
    let shell = runtime.path().join("shell");
    let script = text_client_script(wayland, &output);
    fs::write(&shell, script).expect("write client script");
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).expect("make shell executable");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let mut cmd = ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"));
    cmd.args(["--rtp-port", &port, "--resolution", "800x600", "--"]);
    if wayland {
        cmd.env("GDK_BACKEND", "wayland")
            .env("GTK_IM_MODULE", "xim")
            .arg(&shell);
    } else {
        cmd.args(["xterm", "-u8", "-e"]).arg(&shell);
    }
    let mut compositor = Compositor(
        cmd.env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !output.exists() {
        assert!(Instant::now() < deadline, "terminal did not start");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_secs(1));
    let stdin = compositor.0.stdin.as_mut().expect("command pipe");
    for (state, sequence) in [(KeyState::Pressed, 1), (KeyState::Released, 2)] {
        stdin
            .write_all(
                &Command::KeyboardKey(KeyboardKey {
                    key: KeyCode::new(30).expect("key"),
                    state,
                    sequence: InputSequence::new(sequence).expect("sequence"),
                })
                .encode(),
            )
            .expect("send control key");
    }
    let mut expected = String::from("a");
    let texts = std::iter::once(format!("😀é{}", "😀".repeat(20)))
        .chain("αβγδεζηθικλμνξοπρστυφχψω😀".chars().map(|c| c.to_string()))
        .chain(wayland.then(|| (0x4e00..0x4e28).filter_map(char::from_u32).collect()));
    for text in texts {
        stdin
            .write_all(
                &Command::Text(Text {
                    action: TextAction::Commit,
                    sequence: InputSequence::new(3).expect("sequence"),
                    text: InputText::new(text.clone()).expect("text"),
                })
                .encode(),
            )
            .expect("send text");
        expected.push_str(&text);
        let deadline = Instant::now() + Duration::from_secs(3);
        while fs::read(&output).expect("read client input") != expected.as_bytes() {
            assert!(
                Instant::now() < deadline,
                "client did not receive {expected}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    for (state, sequence) in [(KeyState::Pressed, 4), (KeyState::Released, 5)] {
        stdin
            .write_all(
                &Command::KeyboardKey(KeyboardKey {
                    key: KeyCode::new(48).expect("key"),
                    state,
                    sequence: InputSequence::new(sequence).expect("sequence"),
                })
                .encode(),
            )
            .expect("send trailing control key");
    }
    expected.push('b');
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let received = fs::read(&output).expect("read terminal input");
        if received == expected.as_bytes() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal received {:?}, expected {expected}",
            String::from_utf8_lossy(&received)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland and GTK3 Python bindings"]
fn composed_text_reaches_wayland_client_without_text_input() {
    text_reaches_terminal(true);
}

#[test]
#[ignore = "requires FFmpeg, Xwayland and xterm"]
fn composed_text_reaches_x11_client() {
    text_reaches_terminal(false);
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk and xdotool"]
fn xwayland_relative_warp_moves_the_compositor_pointer() {
    xwayland_pointer_at_scale(120);
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk and xdotool"]
fn xwayland_click_and_relative_warp_match_at_double_scale() {
    xwayland_pointer_at_scale(240);
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk and xdotool"]
fn x11_top_and_left_resize_keep_opposite_edges_fixed() {
    x11_resize("none");
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk and xdotool"]
fn x11_resize_respects_minimum_size_hints() {
    x11_resize("minimum");
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk and xdotool"]
fn x11_resize_respects_increment_hints() {
    x11_resize("increment");
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, python3-tk and xdotool"]
fn x11_resize_respects_aspect_hints() {
    x11_resize("aspect");
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, xterm, xprop and xdotool"]
fn xterm_resize_respects_character_width() {
    x11_resize("xterm");
}

#[expect(
    clippy::too_many_lines,
    reason = "the X11 client and resize assertions form one integration scenario"
)]
#[expect(clippy::expect_used, reason = "integration test requires live clients")]
fn x11_resize(mode: &str) {
    let runtime = tempfile::tempdir().expect("runtime");
    let script = runtime.path().join("client.py");
    let ready = runtime.path().join("ready");
    let hints = match mode {
        "minimum" => "r.minsize(395, 290)",
        "increment" => "r.wm_grid(400, 300, 20, 20)",
        "aspect" => "r.aspect(4, 3, 4, 3)",
        _ => "",
    };
    let program = if mode == "xterm" {
        format!(
            "#!/usr/bin/python3\nimport os, subprocess\nchild = subprocess.Popen(['xterm', '-title', 'waywire-resize-test', '-geometry', '80x24+100+100'])\nopen({:?}, 'w').write(str(os.getpid()))\nchild.wait()\n",
            ready.to_string_lossy()
        )
    } else {
        format!(
            "#!/usr/bin/python3\nimport os, tkinter as tk\nr=tk.Tk()\nr.title('waywire-resize-test')\nr.geometry('400x300')\nr.update()\n{hints}\nr.update()\nopen({:?}, 'w').write(str(os.getpid()))\nr.mainloop()\n",
            ready.to_string_lossy()
        )
    };
    fs::write(&script, program).expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .arg(&script)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking events");
    let mut decoder = Decoder::<Event>::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = written_contents(&ready, deadline, "client did not start");
    let environ = fs::read(format!("/proc/{pid}/environ")).expect("environment");
    let display = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"DISPLAY="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .expect("DISPLAY");
    let xdotool = |args: &[&str]| {
        ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args(args)
            .output()
            .expect("xdotool")
    };
    let ids = loop {
        let search = xdotool(&["search", "--name", "waywire-resize-test"]);
        let ids = String::from_utf8(search.stdout).expect("window id");
        if !ids.is_empty() {
            break ids;
        }
        assert!(Instant::now() < deadline, "window did not appear");
        thread::sleep(Duration::from_millis(20));
    };
    let id = ids.lines().next().expect("window");
    let geometry = || {
        let output = xdotool(&["getwindowgeometry", "--shell", id]);
        let text = String::from_utf8(output.stdout).expect("geometry");
        let value = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .expect("field")
                .parse::<u32>()
                .expect("number")
        };
        (value("X="), value("Y="), value("WIDTH="), value("HEIGHT="))
    };
    let mut stdin = compositor.0.stdin.take().expect("stdin");
    let mut sequence = 0;
    let mut send = |command: Command| stdin.write_all(&command.encode()).expect("send input");
    if mode == "xterm" {
        while geometry().2 < 300 {
            assert!(
                Instant::now() < deadline,
                "xterm did not map at its final size"
            );
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(300));
    }
    let xterm_hints = if mode == "xterm" {
        let output = ProcessCommand::new("xprop")
            .env("DISPLAY", display)
            .args(["-id", id, "WM_NORMAL_HINTS"])
            .output()
            .expect("xprop");
        let text = String::from_utf8(output.stdout).expect("hints");
        let width = |label: &str| -> u32 {
            text.lines()
                .find_map(|line| line.trim().strip_prefix(label))
                .expect("hint")
                .split_whitespace()
                .next()
                .expect("width")
                .parse()
                .expect("number")
        };
        Some((
            width("program specified base size: "),
            width("program specified resize increment: "),
        ))
    } else {
        None
    };
    // The compositor maps an X11 window, and starts a resize from its frame,
    // only once Xwayland commits the window's first buffer, which can trail
    // the X11 map under load. Hover the left edge until the compositor shows
    // the resize cursor, as a user would, before pressing.
    let (x, y, _, height) = geometry();
    loop {
        sequence += 1;
        send(Command::PointerAbsolute(PointerAbsolute {
            x: PointerCoordinate::new(x * 65535 / 800).expect("x"),
            y: PointerCoordinate::new((y + height / 2) * 65535 / 600).expect("y"),
            sequence: InputSequence::new(sequence).expect("sequence"),
        }));
        if events(&mut compositor.0, &mut decoder)
            .contains(&Event::CursorShape(CursorShape::WResize))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "left edge never showed a resize cursor"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let (mut x, mut y, mut width, mut height) = geometry();
    let first_dx = if mode == "xterm" {
        31
    } else if mode == "increment" || mode == "aspect" {
        30
    } else {
        10
    };
    for (dx, dy) in [(first_dx, 0), (0, 20), (10, 20)] {
        let (px, py) = (
            if dx == 0 { x + width / 2 } else { x },
            if dy == 0 { y + height / 2 } else { y - 28 },
        );
        sequence += 1;
        send(Command::PointerAbsolute(PointerAbsolute {
            x: PointerCoordinate::new(px * 65535 / 800).expect("x"),
            y: PointerCoordinate::new(py * 65535 / 600).expect("y"),
            sequence: InputSequence::new(sequence).expect("sequence"),
        }));
        sequence += 1;
        send(Command::PointerButton(PointerButton {
            button: Button::Left,
            state: ButtonState::Pressed,
            sequence: InputSequence::new(sequence).expect("sequence"),
        }));
        sequence += 1;
        send(Command::PointerAbsolute(PointerAbsolute {
            x: PointerCoordinate::new((px + dx) * 65535 / 800).expect("x"),
            y: PointerCoordinate::new((py + dy) * 65535 / 600).expect("y"),
            sequence: InputSequence::new(sequence).expect("sequence"),
        }));
        sequence += 1;
        send(Command::PointerButton(PointerButton {
            button: Button::Left,
            state: ButtonState::Released,
            sequence: InputSequence::new(sequence).expect("sequence"),
        }));
        if mode == "increment" || mode == "aspect" || mode == "xterm" {
            thread::sleep(Duration::from_millis(300));
            let actual = geometry();
            let (_, _, actual_width, actual_height) = actual;
            assert!(
                actual_width < width,
                "resize must change window size: initial {:?}, actual {actual:?}",
                (x, y, width, height)
            );
            if mode == "increment" {
                assert_eq!(actual_width % 20, 0, "width increment");
                assert_eq!(actual_height % 20, 0, "height increment");
            } else if let Some((base, increment)) = xterm_hints {
                assert_eq!(
                    (actual_width - base) % increment,
                    0,
                    "xterm character width"
                );
            } else {
                assert!(
                    actual_width * 3 >= actual_height * 4 - 2
                        && actual_width * 3 <= actual_height * 4 + 2,
                    "aspect ratio"
                );
            }
            return;
        }
        let new_width = if mode == "minimum" {
            (width - dx).max(395)
        } else {
            width - dx
        };
        let new_height = if mode == "minimum" {
            (height - dy).max(290)
        } else {
            height - dy
        };
        let expected = (
            x + width - new_width,
            y + height - new_height,
            new_width,
            new_height,
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let actual = geometry();
            if actual == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected:?}, got {actual:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        (x, y, width, height) = expected;
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the X11 session and pointer assertions form one integration scenario"
)]
#[expect(
    clippy::expect_used,
    reason = "integration test helper requires live clients"
)]
fn xwayland_pointer_at_scale(scale: u16) {
    let runtime = tempfile::tempdir().expect("runtime");
    let script = runtime.path().join("client.py");
    let ready = runtime.path().join("ready");
    let click = runtime.path().join("click");
    fs::write(
        &script,
        format!(
            r"#!/usr/bin/python3
import ctypes, tkinter as tk
root = tk.Tk()
root.title('waywire-warp-test')
root.geometry('400x300')
root.bind('<Button-1>', lambda e: open('{}', 'w').write(str(e.x) + ' ' + str(e.y)))
root.update()
x11 = ctypes.CDLL('libX11.so.6')
fixes = ctypes.CDLL('libXfixes.so.3')
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XFlush.argtypes = [ctypes.c_void_p]
fixes.XFixesHideCursor.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
d = x11.XOpenDisplay(None)
fixes.XFixesHideCursor(d, root.winfo_id())
x11.XFlush(d)
open('{}', 'w').write(str(__import__('os').getpid()))
root.mainloop()
",
            click.display(),
            ready.display()
        ),
    )
    .expect("write client");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable client");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .arg(&script)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = written_contents(&ready, deadline, "client did not start");
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking");
    let mut decoder = Decoder::<Event>::new();
    let resize = Command::Resize(Resize {
        size: FrameSize::new(800, 600).expect("size"),
        scale_v120: ScaleV120::new(scale).expect("scale"),
        request_id: RequestId::new(1).expect("request"),
    });
    compositor
        .0
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&resize.encode())
        .expect("resize");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events(&mut compositor.0, &mut decoder)
        .iter()
        .any(|event| matches!(event, Event::ResizeApplied(_)))
    {
        assert!(Instant::now() < deadline, "resize not applied");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(300));
    let environ = fs::read(format!("/proc/{pid}/environ")).expect("client environment");
    let display = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"DISPLAY="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .expect("DISPLAY");
    let xdotool = |args: &[&str]| {
        ProcessCommand::new("xdotool")
            .env("DISPLAY", display)
            .args(args)
            .output()
            .expect("xdotool")
    };
    let search = xdotool(&["search", "--name", "waywire-warp-test"]);
    let window = String::from_utf8(search.stdout).expect("window id");
    let window = window.lines().next().expect("window");
    let geometry = xdotool(&["getwindowgeometry", "--shell", window]);
    let geometry = String::from_utf8(geometry.stdout).expect("geometry");
    let value = |name: &str| {
        geometry
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .expect("coordinate")
            .parse::<u32>()
            .expect("number")
    };
    let (x, y) = (value("X=") + 100, value("Y=") + 100);
    let command = Command::PointerAbsolute(PointerAbsolute {
        x: PointerCoordinate::new(x * 65535 / 800).expect("x"),
        y: PointerCoordinate::new(y * 65535 / 600).expect("y"),
        sequence: InputSequence::new(1).expect("sequence"),
    });
    compositor
        .0
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&command.encode())
        .expect("move pointer");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if events(&mut compositor.0, &mut decoder)
            .iter()
            .any(|event| matches!(event, Event::CursorPosition(_)))
        {
            break;
        }
        assert!(Instant::now() < deadline, "no initial cursor position");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(300));
    for state in [ButtonState::Pressed, ButtonState::Released] {
        let command = Command::PointerButton(PointerButton {
            button: Button::Left,
            state,
            sequence: InputSequence::new(2).expect("sequence"),
        });
        compositor
            .0
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(&command.encode())
            .expect("click");
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let coordinates = written_contents(&click, deadline, "click did not reach X11 client");
    let coordinates: Vec<i32> = coordinates
        .split_whitespace()
        .map(|v| v.parse().expect("coordinate"))
        .collect();
    assert_eq!(coordinates.len(), 2);
    assert!(
        (95..=105).contains(&coordinates[0]) && (95..=105).contains(&coordinates[1]),
        "wrong click coordinates: {coordinates:?}"
    );
    events(&mut compositor.0, &mut decoder);
    let moved = xdotool(&["mousemove_relative", "--", "20", "0"]);
    assert!(moved.status.success(), "xdotool failed");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let updates = events(&mut compositor.0, &mut decoder);
        if updates.iter().any(|event| matches!(event, Event::CursorPosition(pos) if pos.x.get() > x * 65535 / 800 + 1000)) { break }
        assert!(
            Instant::now() < deadline,
            "relative warp did not reach compositor"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland and xprop"]
fn xwayland_resource_dpi_tracks_output_scale() {
    let runtime = tempfile::tempdir().expect("runtime");
    let ready = runtime.path().join("display");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .args([
                "sh",
                "-c",
                "printf '%s' \"$DISPLAY\" > \"$0\"; exec sleep 30",
            ])
            .arg(&ready)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let display = written_contents(&ready, deadline, "session did not start");
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking");
    let mut decoder = Decoder::<Event>::new();
    let resize = Command::Resize(Resize {
        size: FrameSize::new(800, 600).expect("size"),
        scale_v120: ScaleV120::new(240).expect("scale"),
        request_id: RequestId::new(1).expect("request"),
    });
    compositor
        .0
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&resize.encode())
        .expect("resize");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if events(&mut compositor.0, &mut decoder)
            .iter()
            .any(|event| matches!(event, Event::ResizeApplied(_)))
        {
            break;
        }
        assert!(Instant::now() < deadline, "resize not applied");
        thread::sleep(Duration::from_millis(10));
    }
    let output = ProcessCommand::new("xprop")
        .env("DISPLAY", display)
        .args(["-root", "RESOURCE_MANAGER"])
        .output()
        .expect("xprop");
    let resources = String::from_utf8(output.stdout).expect("resource text");
    assert!(
        resources.contains("Xft.dpi:\\t192"),
        "missing 2x DPI: {resources}"
    );
}

/// A compositor whose session only waits, for driving the clipboard with
/// wl-clipboard and xclip.
struct Desktop {
    compositor: Compositor,
    runtime: tempfile::TempDir,
    display: String,
    wayland: String,
}

#[expect(clippy::expect_used, reason = "test helper requires a live desktop")]
fn desktop() -> Desktop {
    let runtime = tempfile::tempdir().expect("runtime");
    let ready = runtime.path().join("displays");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("address").port().to_string();
    let compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .args([
                "sh",
                "-c",
                "printf '%s\\n%s' \"$DISPLAY\" \"$WAYLAND_DISPLAY\" > \"$0.tmp\"; mv \"$0.tmp\" \"$0\"; exec sleep 60",
            ])
            .arg(&ready)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let displays = written_contents(&ready, deadline, "session did not start");
    let (display, wayland) = displays.split_once('\n').expect("two displays");
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking");
    Desktop {
        display: display.to_owned(),
        wayland: wayland.to_owned(),
        compositor,
        runtime,
    }
}

impl Desktop {
    fn command(&self, args: &[&str]) -> ProcessCommand {
        let mut command = ProcessCommand::new(args[0]);
        command
            .args(&args[1..])
            .env("DISPLAY", &self.display)
            .env("WAYLAND_DISPLAY", &self.wayland)
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// Starts a tool that owns a selection, writing `input` to it.
    #[expect(clippy::expect_used, reason = "test helper requires the tool")]
    fn copy(&self, args: &[&str], input: &[u8]) -> Child {
        let mut child = self
            .command(args)
            .stdout(Stdio::null())
            .spawn()
            .expect("start copy tool");
        child
            .stdin
            .take()
            .expect("tool stdin")
            .write_all(input)
            .expect("write copy input");
        child
    }

    /// Pastes with `args` until the output is `expected` or five seconds pass.
    #[expect(clippy::expect_used, reason = "test helper requires the tool")]
    fn paste(&self, args: &[&str], expected: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // A hung transfer fails the test instead of hanging it.
            let output = self
                .command(&[&["timeout", "5"], args].concat())
                .stdin(Stdio::null())
                .output()
                .expect("run paste tool")
                .stdout;
            if output == expected || Instant::now() >= deadline {
                return output;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, wl-clipboard and xclip"]
fn primary_selection_crosses_wayland_and_x11() {
    let mut desktop = desktop();
    let mut wayland = desktop.copy(
        &["wl-copy", "--primary", "--foreground"],
        "wayland α".as_bytes(),
    );
    assert_eq!(
        desktop.paste(
            &["xclip", "-o", "-selection", "primary"],
            "wayland α".as_bytes()
        ),
        "wayland α".as_bytes()
    );
    let mut x11 = desktop.copy(
        &["xclip", "-quiet", "-i", "-selection", "primary"],
        "x11 β".as_bytes(),
    );
    assert_eq!(
        desktop.paste(
            &["wl-paste", "--primary", "--no-newline"],
            "x11 β".as_bytes()
        ),
        "x11 β".as_bytes()
    );
    // The primary selection stays on the desktop.
    let mut decoder = Decoder::<Event>::new();
    let synced = events(&mut desktop.compositor.0, &mut decoder);
    assert!(
        !synced
            .iter()
            .any(|event| matches!(event, Event::Clipboard(_))),
        "the primary selection reached the viewer: {synced:?}"
    );
    for child in [&mut wayland, &mut x11] {
        drop(child.kill());
        drop(child.wait());
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, wl-clipboard and xclip"]
fn x11_string_clipboard_is_iso_8859_1_both_ways() {
    let mut desktop = desktop();
    let mut wayland = desktop.copy(
        &[
            "wl-copy",
            "--foreground",
            "--type",
            "text/plain;charset=utf-8",
        ],
        "café €".as_bytes(),
    );
    let string = ["xclip", "-o", "-selection", "clipboard", "-t", "STRING"];
    assert_eq!(desktop.paste(&string, b"caf\xe9 ?"), b"caf\xe9 ?");
    let mut x11 = desktop.copy(
        &[
            "xclip",
            "-quiet",
            "-i",
            "-selection",
            "clipboard",
            "-t",
            "STRING",
        ],
        b"na\xefve",
    );
    assert_eq!(
        desktop.paste(&["wl-paste", "--no-newline"], "naïve".as_bytes()),
        "naïve".as_bytes()
    );
    let mut decoder = Decoder::<Event>::new();
    let synced = events(&mut desktop.compositor.0, &mut decoder);
    assert!(
        synced
            .iter()
            .any(|event| matches!(event, Event::Clipboard(text) if text.as_str() == "naïve")),
        "the viewer did not receive the X11 STRING copy: {synced:?}"
    );
    for child in [&mut wayland, &mut x11] {
        drop(child.kill());
        drop(child.wait());
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, wl-clipboard and xclip"]
fn clipboard_outlives_its_wayland_and_x11_owners() {
    let desktop = desktop();
    let mut wayland = desktop.copy(&["wl-copy", "--foreground"], b"from wayland");
    assert_eq!(
        desktop.paste(&["wl-paste", "--no-newline"], b"from wayland"),
        b"from wayland"
    );
    drop(wayland.kill());
    drop(wayland.wait());
    let x11_paste = ["xclip", "-o", "-selection", "clipboard"];
    assert_eq!(desktop.paste(&x11_paste, b"from wayland"), b"from wayland");
    assert_eq!(
        desktop.paste(&["wl-paste", "--no-newline"], b"from wayland"),
        b"from wayland"
    );

    let mut x11 = desktop.copy(
        &["xclip", "-quiet", "-i", "-selection", "clipboard"],
        b"from x11",
    );
    assert_eq!(
        desktop.paste(&["wl-paste", "--no-newline"], b"from x11"),
        b"from x11"
    );
    drop(x11.kill());
    drop(x11.wait());
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        desktop.paste(&["wl-paste", "--no-newline"], b"from x11"),
        b"from x11"
    );
    assert_eq!(desktop.paste(&x11_paste, b"from x11"), b"from x11");
}

#[test]
#[ignore = "requires FFmpeg, Xwayland and GTK3 Python bindings"]
#[expect(
    clippy::too_many_lines,
    reason = "the drag scenario keeps both clients, the input and the checks together"
)]
fn drags_cross_between_wayland_and_x11_clients() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let script = runtime.path().join("dnd.py");
    fs::write(
        &script,
        r"#!/usr/bin/python3
import os, sys
import gi
gi.require_version('Gdk', '3.0')
gi.require_version('Gtk', '3.0')
from gi.repository import Gdk, GLib, Gtk
backend, directory = sys.argv[1], sys.argv[2]
window = Gtk.Window(title='waywire-dnd-' + backend)
window.set_default_size(300, 300)
if backend == 'x11':
    window.move(450, 150)
box = Gtk.EventBox()
box.add(Gtk.Label(label=backend))
box.drag_source_set(Gdk.ModifierType.BUTTON1_MASK, [], Gdk.DragAction.COPY)
box.drag_source_add_text_targets()
box.connect('drag-data-get', lambda *args: args[2].set_text('from-' + backend, -1))
box.drag_dest_set(Gtk.DestDefaults.ALL, [], Gdk.DragAction.COPY)
box.drag_dest_add_text_targets()
def received(widget, context, x, y, data, info, time):
    with open(os.path.join(directory, backend + '-received'), 'w') as output:
        output.write(data.get_text())
box.connect('drag-data-received', received)
window.add(box)
window.connect('destroy', Gtk.main_quit)
window.show_all()
GLib.timeout_add(500, lambda: open(os.path.join(directory, backend + '-ready'), 'w').close())
Gtk.main()
",
    )
    .expect("write GTK script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    // The Wayland window is placed at (32, 32); the X11 window asks for
    // (450, 150). Each is a drag source and a drop target for text.
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args(["--rtp-port", &port, "--resolution", "800x600", "--"])
            .args([
                "sh",
                "-c",
                r#"
                GDK_BACKEND=wayland "$0" wayland "$1" &
                while [ ! -e "$1/wayland-ready" ]; do sleep 0.05; done
                GDK_BACKEND=x11 "$0" x11 "$1" &
                wait
                "#,
            ])
            .arg(&script)
            .arg(runtime.path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !runtime.path().join("x11-ready").exists() {
        assert!(Instant::now() < deadline, "GTK windows did not map");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(500));
    let stdin = compositor.0.stdin.as_mut().expect("command pipe");
    let mut sequence = 0;
    let mut send = |input: &dyn Fn(InputSequence) -> Command| {
        sequence += 1;
        stdin
            .write_all(&input(InputSequence::new(sequence).expect("sequence")).encode())
            .expect("send input");
    };
    let point = |x: u32, y: u32| {
        move |sequence| {
            Command::PointerAbsolute(PointerAbsolute {
                x: PointerCoordinate::new(x * 65535 / 800).expect("x"),
                y: PointerCoordinate::new(y * 65535 / 600).expect("y"),
                sequence,
            })
        }
    };
    let left = |state| {
        move |sequence| {
            Command::PointerButton(PointerButton {
                button: Button::Left,
                state,
                sequence,
            })
        }
    };
    for ((from_x, to_x), received, expected) in [
        ((180, 600), "x11-received", "from-wayland"),
        ((600, 180), "wayland-received", "from-x11"),
    ] {
        send(&point(from_x, 250));
        thread::sleep(Duration::from_millis(200));
        send(&left(ButtonState::Pressed));
        // Small steps pass GTK's drag threshold, then cross to the target.
        for step in 1..=30 {
            let x = if to_x > from_x {
                from_x + (to_x - from_x) * step / 30
            } else {
                from_x - (from_x - to_x) * step / 30
            };
            send(&point(x, 250));
            thread::sleep(Duration::from_millis(30));
        }
        // XDND answers positions asynchronously; let the target accept.
        for wiggle in [251, 250, 251, 250] {
            thread::sleep(Duration::from_millis(150));
            send(&point(to_x, wiggle));
        }
        thread::sleep(Duration::from_millis(300));
        send(&left(ButtonState::Released));
        let path = runtime.path().join(received);
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&path).ok().as_deref() != Some(expected) {
            assert!(
                Instant::now() < deadline,
                "{received} is {:?}, expected {expected}",
                fs::read_to_string(&path).ok()
            );
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(300));
    }
}

/// Every foreign toplevel a session showed, and each title that lost the
/// activated state, as `zwlr_foreign_toplevel_manager_v1` reports them.
#[derive(Default)]
struct Toplevels {
    pending: std::collections::HashMap<wayland_client::backend::ObjectId, (String, bool)>,
    activated: std::collections::HashMap<wayland_client::backend::ObjectId, bool>,
    titles: Vec<String>,
    lost_focus: Vec<String>,
}

type ToplevelLog = std::sync::Arc<std::sync::Mutex<Toplevels>>;

mod toplevel_watch {
    use wayland_client::Connection;
    use wayland_client::Dispatch;
    use wayland_client::Proxy;
    use wayland_client::QueueHandle;
    use wayland_client::protocol::wl_registry;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

    use super::ToplevelLog;

    pub(super) struct Watch(pub(super) ToplevelLog);

    impl Dispatch<wl_registry::WlRegistry, ()> for Watch {
        fn event(
            _: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name, interface, ..
            } = event
                && interface == ZwlrForeignToplevelManagerV1::interface().name
            {
                registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(name, 3, qh, ());
            }
        }
    }

    impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Watch {
        fn event(
            _: &mut Self,
            _: &ZwlrForeignToplevelManagerV1,
            _: zwlr_foreign_toplevel_manager_v1::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }

        wayland_client::event_created_child!(Watch, ZwlrForeignToplevelManagerV1, [
            zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
        ]);
    }

    impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Watch {
        #[expect(clippy::unwrap_used, reason = "the log mutex is never poisoned")]
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "other events change neither the title nor the activated state"
        )]
        fn event(
            watch: &mut Self,
            handle: &ZwlrForeignToplevelHandleV1,
            event: zwlr_foreign_toplevel_handle_v1::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let mut log = watch.0.lock().unwrap();
            let id = handle.id();
            match event {
                zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                    log.pending.entry(id).or_default().0 = title;
                }
                zwlr_foreign_toplevel_handle_v1::Event::State { state } => {
                    let activated = state
                        .chunks_exact(4)
                        .any(|value| value == 2u32.to_ne_bytes());
                    log.pending.entry(id).or_default().1 = activated;
                }
                zwlr_foreign_toplevel_handle_v1::Event::Done => {
                    let (title, activated) = log.pending.get(&id).cloned().unwrap_or_default();
                    if !log.titles.contains(&title) {
                        log.titles.push(title.clone());
                    }
                    if log.activated.insert(id, activated) == Some(true) && !activated {
                        log.lost_focus.push(title);
                    }
                }
                zwlr_foreign_toplevel_handle_v1::Event::Closed
                    if log.activated.remove(&id) == Some(true) =>
                {
                    let title = log.pending.get(&id).cloned().unwrap_or_default().0;
                    log.lost_focus.push(title);
                }
                _ => {}
            }
        }
    }
}

impl Desktop {
    /// Records the session's windows and focus changes until the compositor exits.
    #[expect(clippy::expect_used, reason = "test helper requires the socket")]
    fn watch_toplevels(&self) -> ToplevelLog {
        let socket =
            std::os::unix::net::UnixStream::connect(self.runtime.path().join(&self.wayland))
                .expect("connect to the compositor");
        let connection = wayland_client::Connection::from_socket(socket).expect("connection");
        let mut queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let log = ToplevelLog::default();
        let mut watch = toplevel_watch::Watch(std::sync::Arc::clone(&log));
        thread::spawn(move || while queue.blocking_dispatch(&mut watch).is_ok() {});
        log
    }
}

#[test]
#[ignore = "requires FFmpeg, Xwayland, GTK3 Python bindings, wl-clipboard and xclip"]
#[expect(clippy::unwrap_used, reason = "the log mutex is never poisoned")]
fn data_control_copies_reach_x11_and_the_viewer_without_taking_focus() {
    let mut desktop = desktop();
    let log = desktop.watch_toplevels();
    let mut focused = desktop
        .command(&[
            "/usr/bin/python3",
            "-c",
            "import gi; gi.require_version('Gtk', '3.0'); from gi.repository import Gtk; \
             w = Gtk.Window(title='waywire-focus'); w.connect('destroy', Gtk.main_quit); \
             w.show_all(); Gtk.main()",
        ])
        .env("GDK_BACKEND", "wayland")
        .stdout(Stdio::null())
        .spawn()
        .expect("start the focused window");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log
        .lock()
        .unwrap()
        .activated
        .values()
        .any(|&activated| activated)
    {
        assert!(Instant::now() < deadline, "the GTK window was not focused");
        thread::sleep(Duration::from_millis(20));
    }
    let mut decoder = Decoder::<Event>::new();
    events(&mut desktop.compositor.0, &mut decoder);

    let mut clipboard = desktop.copy(&["wl-copy", "--foreground"], "data control α".as_bytes());
    let mut primary = desktop.copy(
        &["wl-copy", "--primary", "--foreground"],
        "primary β".as_bytes(),
    );
    let x11 = ["xclip", "-o", "-selection", "clipboard"];
    assert_eq!(
        desktop.paste(&x11, "data control α".as_bytes()),
        "data control α".as_bytes()
    );
    assert_eq!(
        desktop.paste(&["wl-paste", "--no-newline"], "data control α".as_bytes()),
        "data control α".as_bytes()
    );
    assert_eq!(
        desktop.paste(
            &["xclip", "-o", "-selection", "primary"],
            "primary β".as_bytes()
        ),
        "primary β".as_bytes()
    );
    assert_eq!(
        desktop.paste(
            &["wl-paste", "--primary", "--no-newline"],
            "primary β".as_bytes()
        ),
        "primary β".as_bytes()
    );
    let synced: Vec<_> = events(&mut desktop.compositor.0, &mut decoder)
        .into_iter()
        .filter_map(|event| match event {
            Event::Clipboard(text) => Some(text.as_str().to_owned()),
            Event::Frame(_)
            | Event::EncodedFrame(_)
            | Event::ResizeApplied(_)
            | Event::CursorShape(_)
            | Event::CursorImage(_)
            | Event::CursorVisibility(_)
            | Event::CursorPosition(_)
            | Event::CursorClick(_)
            | Event::ResetVideoRefused(_)
            | Event::AgentInput(_) => None,
        })
        .collect();
    assert_eq!(synced, ["data control α"], "viewer clipboard events");

    // The copy outlives wl-copy, as clipboards from focused clients do.
    drop(clipboard.kill());
    drop(clipboard.wait());
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        desktop.paste(&x11, "data control α".as_bytes()),
        "data control α".as_bytes()
    );
    {
        let log = log.lock().unwrap();
        assert_eq!(log.titles, ["waywire-focus"], "windows the session showed");
        assert!(
            log.lost_focus.is_empty(),
            "lost focus: {:?}",
            log.lost_focus
        );
    }
    for child in [&mut primary, &mut focused] {
        drop(child.kill());
        drop(child.wait());
    }
}
