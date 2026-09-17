// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! E2E test done by making a virtual keyboard, running the binary on it, and reading back the
//! events it produces.
//!
//! Ignored by default, as all the tests need write access to `/dev/uinput` and read access to
//! `/dev/input`, and one of them also needs strace installed; run with:
//! ```sh
//! cargo build && sudo -E "$(which cargo)" test --test e2e -- --ignored --test-threads=1
//! ```
//!
//! It is recommended to either run this in a temporary container/VM in some way or to clean out all
//! of your cargo cache afterwards (both global and in this project), since otherwise it tends to
//! leave behind cache files owned by root that cargo then can't open or remove, causing it to fail.
//! `sudo rm -rf ~/.cargo/registry` + `sudo cargo clean` seems to have been sufficient for me, but
//! be careful with deleting things from your system as root.

#![cfg(target_os = "linux")]

use evdev::{
    AttributeSet, AutoRepeat, BusType, Device, EventSummary, EventType, InputEvent, InputId,
    KeyCode, LedCode, SynchronizationCode,
};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use ripples::app::device::use_monotonic_timestamps;
use ripples::app::virtdev::{Caps, Sink};

const SOURCE_NAME: &str = "ripples e2e test source";
const SINK_NAME: &str = "ripples e2e test sink";
const WINDOW: Duration = Duration::from_millis(20);

/// Kills the child on drop.
struct DropChild(Child);

impl Drop for DropChild {
    fn drop(&mut self) {
        _ = self.0.kill();
        _ = self.0.wait();
    }
}

fn signal(child: &DropChild, sig: Signal) {
    let pid = i32::try_from(child.0.id()).expect("a pid fits in an i32");
    let pid = Pid::from_raw(pid).expect("the child has a nonzero pid");
    kill_process(pid, sig).expect("send a signal to the child");
}

fn open_as_desktop() -> Device {
    let (_, desktop) = wait_for_device(SOURCE_NAME, Duration::from_secs(5));
    desktop.set_nonblocking(true).unwrap();
    desktop
}

fn source_id() -> InputId {
    InputId::new(BusType::BUS_USB, 0xcafe, 0x1225, 1)
}

fn make_source() -> Sink {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::KEY_A);
    keys.insert(KeyCode::KEY_B);
    let mut leds = AttributeSet::<LedCode>::new();
    leds.insert(LedCode::LED_CAPSL);
    leds.insert(LedCode::LED_NUML);
    leds.insert(LedCode::LED_MUTE);
    Sink::with_caps(
        &Caps {
            id: source_id(),
            keys: &keys,
            leds: Some(&leds),
            misc: None,
            // don't use the 250/33 defaults so the repeats can be distinguished
            repeat: Some(AutoRepeat {
                delay: 100,
                period: 20,
            }),
        },
        SOURCE_NAME,
    )
    .expect("create the synthetic keyboard (needs root and /dev/uinput)")
}

fn start_child(mode: &str) -> DropChild {
    start_child_with(mode, Duration::from_millis(100))
}

fn start_child_with(mode: &str, rescan: Duration) -> DropChild {
    let (source_path, _) = wait_for_device(SOURCE_NAME, Duration::from_secs(5));
    let child = Command::new(env!("CARGO_BIN_EXE_ripples"))
        .arg(&source_path)
        .args(["-m", mode])
        .args(["-w", "20"])
        .args(["--virtual-name", SINK_NAME])
        .args(["--device-rescan-interval", &rescan.as_millis().to_string()])
        .arg("--wait-for-device")
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn the child");
    DropChild(child)
}

/// Detaches strace from the child on drop.
struct Tracer(Child);

impl Drop for Tracer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Makes every `read(2)` syscall the child does on `path` return `delay` late by using strace's
/// fault injection, allowing arbitrarily widening the race window between reading the keyboard and
/// running the timers from a handful of microseconds to `delay`.
fn delay_reads(child: &DropChild, path: &Path, delay: Duration) -> Tracer {
    let spawned = Command::new("strace")
        .args(["-qq", "-o", "/dev/null", "-e", "trace=read"])
        .arg(format!("-einject=read:delay_exit={}", delay.as_micros()))
        .arg("-P")
        .arg(path)
        .args(["-p", &child.0.id().to_string()])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn strace, which this test needs installed");
    let tracer = Tracer(spawned);

    let status = format!("/proc/{}/status", child.0.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&status)
        .expect("read the child's status")
        .lines()
        .any(|line| line.starts_with("TracerPid:") && line.trim_end() != "TracerPid:\t0")
    {
        assert!(Instant::now() < deadline, "strace never attached");
        sleep(Duration::from_millis(10));
    }
    tracer
}

fn find_device(name: &str) -> Option<(PathBuf, Device)> {
    evdev::enumerate().find(|(_, dev)| dev.name() == Some(name))
}

fn wait_for_device(name: &str, timeout: Duration) -> (PathBuf, Device) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = find_device(name) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "{name} should've appeared at some point"
        );
        sleep(Duration::from_millis(20));
    }
}

fn press(source: &mut Sink, key: KeyCode, down: bool) {
    source
        .emit(&[InputEvent::new(EventType::KEY.0, key.0, i32::from(down))])
        .unwrap();
}

fn syn() -> InputEvent {
    InputEvent::new(
        EventType::SYNCHRONIZATION.0,
        SynchronizationCode::SYN_REPORT.0,
        0,
    )
}

fn set_led(desktop: &mut Device, led: LedCode, on: bool) {
    desktop
        .send_events(&[
            InputEvent::new(EventType::LED.0, led.0, i32::from(on)),
            syn(),
        ])
        .expect("write the LED event");
}

fn drain_feedback(source: &Sink) {
    let mut events = Vec::new();
    source
        .feedback(&mut events)
        .expect("read the keyboard back");
}

fn led_reaches_keyboard(source: &Sink, led: LedCode, value: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
        source
            .feedback(&mut events)
            .expect("read the keyboard back");
        let arrived = events.iter().any(|event| {
            event.event_type() == EventType::LED && event.code() == led.0 && event.value() == value
        });
        if arrived || Instant::now() >= deadline {
            return arrived;
        }
        sleep(Duration::from_millis(10));
    }
}

fn collect(sink: &mut Device, settle: Duration) -> Vec<InputEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + settle;
    while Instant::now() < deadline {
        match sink.fetch_events() {
            Ok(events) => seen.extend(events),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(5));
            }
            Err(e) if Errno::from_io_error(&e) == Some(Errno::NODEV) => break,
            Err(e) => panic!("reading the sink: {e}"),
        }
    }
    seen
}

fn drain(sink: &mut Device, settle: Duration) -> Vec<(u16, bool)> {
    collect(sink, settle)
        .iter()
        .filter_map(|event| match event.destructure() {
            EventSummary::Key(_, code, value) if value != AUTOREPEAT => Some((code.0, value != 0)),
            _ => None,
        })
        .collect()
}

const AUTOREPEAT: i32 = 2;

fn open_sink() -> Device {
    let (_, sink) = wait_for_device(SINK_NAME, Duration::from_secs(5));
    sink.set_nonblocking(true)
        .expect("set the sink to nonblocking");
    use_monotonic_timestamps(&sink).expect("set the sink to CLOCK_MONOTONIC");
    sink
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn only_chatter_is_filtered() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let _child = start_child("mixed");
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    // chatter
    press(&mut source, KeyCode::KEY_A, true);
    for _ in 0..3 {
        press(&mut source, KeyCode::KEY_A, false);
        press(&mut source, KeyCode::KEY_A, true);
    }
    press(&mut source, KeyCode::KEY_A, false);

    assert_eq!(
        drain(&mut sink, WINDOW * 4),
        [(KeyCode::KEY_A.0, true), (KeyCode::KEY_A.0, false)],
        "chatter burst should turn into one keystroke",
    );

    // clean
    for _ in 0..3 {
        press(&mut source, KeyCode::KEY_B, true);
        sleep(WINDOW * 5);
        press(&mut source, KeyCode::KEY_B, false);
        sleep(WINDOW * 5);
    }

    assert_eq!(
        drain(&mut sink, WINDOW * 4),
        [
            (KeyCode::KEY_B.0, true),
            (KeyCode::KEY_B.0, false),
            (KeyCode::KEY_B.0, true),
            (KeyCode::KEY_B.0, false),
            (KeyCode::KEY_B.0, true),
            (KeyCode::KEY_B.0, false),
        ],
        "clean keystrokes should stay unchanged",
    );
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn keyboard_being_unplugged_while_a_key_is_held_is_fine() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let _child = start_child("mixed");
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    press(&mut source, KeyCode::KEY_A, true);
    assert_eq!(drain(&mut sink, WINDOW * 2), [(KeyCode::KEY_A.0, true)]);

    // unplugged mid keystroke
    drop(source);
    assert_eq!(
        drain(&mut sink, Duration::from_millis(500)),
        [(KeyCode::KEY_A.0, false)],
        "a held key should be released if the device goes away",
    );

    // comes back on a new event node
    let mut source = make_source();
    sleep(Duration::from_millis(600));
    press(&mut source, KeyCode::KEY_B, true);
    sleep(WINDOW * 5);
    press(&mut source, KeyCode::KEY_B, false);

    assert_eq!(
        drain(&mut sink, WINDOW * 4),
        [(KeyCode::KEY_B.0, true), (KeyCode::KEY_B.0, false)],
        "child should've re-latched onto the reconnected keyboard",
    );
    assert!(
        find_device(SINK_NAME).is_some(),
        "the sink should've outlived the disconnect",
    );
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn shutting_down_releases_the_keyboard() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let mut child = start_child("eager");
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    press(&mut source, KeyCode::KEY_A, true);
    assert_eq!(drain(&mut sink, WINDOW * 2), [(KeyCode::KEY_A.0, true)]);

    signal(&child, Signal::TERM);
    assert_eq!(
        drain(&mut sink, Duration::from_millis(400)),
        [(KeyCode::KEY_A.0, false)],
        "SIGTERM should release held keys",
    );

    let status = child.0.wait().unwrap();
    assert!(
        status.success(),
        "child should've exited successfully, got {status}",
    );

    let (_, mut raw) = find_device(SOURCE_NAME).expect("the source outlives the child");
    raw.grab().expect("child should've ungrabbed on exit");
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn led_state_gets_passed_to_the_keyboard() {
    let source = make_source();
    sleep(Duration::from_millis(100));
    let _child = start_child("mixed");
    let (sink_path, sink) = wait_for_device(SINK_NAME, Duration::from_secs(5));

    assert!(
        sink.supported_leds()
            .is_some_and(|leds| leds.contains(LedCode::LED_CAPSL)),
        "the child's device should declare the lights its source has",
    );
    drop(sink);

    let mut desktop = Device::open(&sink_path).expect("open the child's device");

    // imitate unwanted numlock traffic
    set_led(&mut desktop, LedCode::LED_NUML, true);
    drain_feedback(&source);

    set_led(&mut desktop, LedCode::LED_CAPSL, true);
    assert!(
        led_reaches_keyboard(&source, LedCode::LED_CAPSL, 1, Duration::from_millis(500)),
        "caps lock should have been passed through to the keyboard",
    );

    drain_feedback(&source);
    set_led(&mut desktop, LedCode::LED_CAPSL, false);
    assert!(
        led_reaches_keyboard(&source, LedCode::LED_CAPSL, 0, Duration::from_millis(500)),
        "and so should turning it off again",
    );
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn led_state_is_restored_after_the_keyboard_is_replugged() {
    let source = make_source();
    sleep(Duration::from_millis(100));
    let _child = start_child("mixed");
    let (sink_path, _) = wait_for_device(SINK_NAME, Duration::from_secs(5));

    // use LED_MUTE rather than numlock because the kernel's console keyboard handler owns
    // capslock/numlock/scrolllock across every keyboard and resyncs them to the console's own state
    // whenever a keyboard appears, so replugging resets numlock here, which the child mirrors,
    // correctly, so the test doesn't actually measure anything, whereas mute is not a console light
    let mut desktop = Device::open(&sink_path).expect("open the child's device");
    set_led(&mut desktop, LedCode::LED_MUTE, true);
    assert!(
        led_reaches_keyboard(&source, LedCode::LED_MUTE, 1, Duration::from_millis(500)),
        "the led state should have been passed through before the keyboard is replugged",
    );

    drop(source);
    let source = make_source();

    assert!(
        led_reaches_keyboard(&source, LedCode::LED_MUTE, 1, Duration::from_secs(2)),
        "the led state should have been restored without the desktop being asked again",
    );
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn a_held_key_repeats_at_the_rate_the_keyboard_was_set_to() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let _child = start_child("mixed");
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    press(&mut source, KeyCode::KEY_A, true);
    let events = collect(&mut sink, Duration::from_millis(600));
    press(&mut source, KeyCode::KEY_A, false);

    let key_a = |event: &InputEvent| matches!(event.destructure(), EventSummary::Key(_, code, _) if code == KeyCode::KEY_A);
    let pressed = events
        .iter()
        .find(|event| key_a(event) && event.value() == 1)
        .expect("the press itself should've come through");
    let repeats: Vec<&InputEvent> = events
        .iter()
        .filter(|event| key_a(event) && event.value() == AUTOREPEAT)
        .collect();

    assert!(
        repeats.len() >= 3,
        "a key held for 600ms should've repeated 3+ times, got {} repeats",
        repeats.len(),
    );

    // the source was set to a 100ms delay whereas the default is 250ms, so this is only possible if
    // it carried over that setting to the virtual device
    let first = repeats[0]
        .timestamp()
        .duration_since(pressed.timestamp())
        .expect("the press comes before the repeats");
    assert!(
        first < Duration::from_millis(200),
        "the first repeat took {first:?}, so the source's repeat delay was not carried over",
    );

    for pair in repeats.windows(2) {
        let gap = pair[1]
            .timestamp()
            .duration_since(pair[0].timestamp())
            .expect("repeats are in order");
        assert!(
            gap >= Duration::from_millis(10),
            "repeats are {gap:?} apart, which means the source's are being forwarded as well",
        );
    }
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn the_grab_waits_for_a_held_key_to_be_released() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let mut desktop = open_as_desktop();

    press(&mut source, KeyCode::KEY_A, true);
    let _child = start_child("mixed");
    let mut sink = open_sink();
    sleep(Duration::from_millis(400));
    press(&mut source, KeyCode::KEY_A, false);

    sleep(Duration::from_millis(200));

    assert_eq!(
        drain(&mut desktop, WINDOW),
        [(KeyCode::KEY_A.0, true), (KeyCode::KEY_A.0, false)],
        "the desktop should have seen the release before the child grabbed the keyboard",
    );
    // that keystroke should have gone to whatever was reading the keyboard at the time
    assert_eq!(drain(&mut sink, WINDOW * 4), []);

    // the keyboard should belong to the child now
    press(&mut source, KeyCode::KEY_B, true);
    sleep(WINDOW * 5);
    press(&mut source, KeyCode::KEY_B, false);
    assert_eq!(
        drain(&mut sink, WINDOW * 4),
        [(KeyCode::KEY_B.0, true), (KeyCode::KEY_B.0, false)],
    );
    assert_eq!(drain(&mut desktop, WINDOW), []);
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn a_key_held_when_the_keyboard_replugs_does_not_get_stuck() {
    let source = make_source();
    sleep(Duration::from_millis(100));
    let _child = start_child_with("mixed", Duration::from_millis(300));
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    drop(source);
    let mut source = make_source();
    let mut desktop = open_as_desktop();
    // pressed before the first repoll from the child and held past it
    press(&mut source, KeyCode::KEY_A, true);
    sleep(Duration::from_millis(500));
    press(&mut source, KeyCode::KEY_A, false);

    sleep(Duration::from_millis(400));

    assert_eq!(
        drain(&mut desktop, WINDOW),
        [(KeyCode::KEY_A.0, true), (KeyCode::KEY_A.0, false)],
        "the desktop should have seen the release before the child regrabbed the keyboard",
    );

    press(&mut source, KeyCode::KEY_B, true);
    sleep(WINDOW * 5);
    press(&mut source, KeyCode::KEY_B, false);
    assert_eq!(
        drain(&mut sink, WINDOW * 4),
        [(KeyCode::KEY_B.0, true), (KeyCode::KEY_B.0, false)],
        "the child should have the keyboard grabbed",
    );
}

#[test]
#[ignore = "needs root and /dev/uinput"]
fn a_stall_long_enough_to_drop_events_does_not_leave_a_key_stuck() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let child = start_child("mixed");
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    press(&mut source, KeyCode::KEY_A, true);
    assert_eq!(drain(&mut sink, WINDOW * 2), [(KeyCode::KEY_A.0, true)]);

    signal(&child, Signal::STOP);
    sleep(Duration::from_millis(50));
    // force dropping events by artificially filling the queue
    for i in 0..4096 {
        source
            .emit(&[InputEvent::new(
                EventType::LED.0,
                LedCode::LED_MUTE.0,
                i32::from(i % 2 == 0),
            )])
            .unwrap();
    }
    press(&mut source, KeyCode::KEY_A, false);
    signal(&child, Signal::CONT);

    assert_eq!(
        drain(&mut sink, Duration::from_millis(500)),
        [(KeyCode::KEY_A.0, false)],
        "the release should have went through",
    );
}

#[test]
#[ignore = "needs root, /dev/uinput, and strace"]
fn a_bounce_read_late_still_cancels_the_pending_release() {
    let mut source = make_source();
    sleep(Duration::from_millis(100));
    let child = start_child("mixed");
    let mut sink = open_sink();
    drain(&mut sink, Duration::from_millis(100));

    let (source_path, _) = wait_for_device(SOURCE_NAME, Duration::from_secs(5));
    let _tracer = delay_reads(&child, &source_path, Duration::from_millis(50));

    // long enough for the delayed read of the press to return and the child to be polling again, so
    // the next read gets just the release
    press(&mut source, KeyCode::KEY_A, true);
    sleep(Duration::from_millis(75));
    press(&mut source, KeyCode::KEY_A, false);

    // within the 20ms window and the artificial 50ms read delay
    sleep(Duration::from_millis(5));

    press(&mut source, KeyCode::KEY_A, true);
    sleep(Duration::from_millis(10));
    press(&mut source, KeyCode::KEY_A, false);

    assert_eq!(
        drain(&mut sink, Duration::from_millis(500)),
        [(KeyCode::KEY_A.0, true), (KeyCode::KEY_A.0, false)],
        "the bounce should have cancelled the pending release"
    );
}
