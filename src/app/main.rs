// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Main event loop. [`App::wait`] is the only place in the entire program that blocks.

use anyhow::{Context, bail};
use evdev::{Device, EventType, InputEvent, KeyCode, SynchronizationCode};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cli::{self, CliResult};
use super::device::{self, Fingerprint};
use super::events;
use super::pipeline::{Fed, Pipeline};
use super::signal::Signals;
use super::virtdev::Sink;
use crate::debounce::Nanos;

/// Emitting a key event and immediately destroying the uinput device can lose the event.
const DRAIN_GRACE: Duration = Duration::from_millis(20);

/// How long to wait for the keyboard to go idle before grabbing it.
const GRAB_WAIT: Duration = Duration::from_secs(2);

/// How often to recheck while waiting for the keyboard to go idle.
const GRAB_IDLE_POLL: Duration = Duration::from_millis(20);

struct Source {
    path: PathBuf,
    dev: Device,
}

/// Result of a successful read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pumped {
    Read,
    Empty,
    Disconnected,
}

struct App {
    fingerprint: Fingerprint,
    virtual_name: String,
    rescan: Nanos,
    pipe: Pipeline,
    sink: Sink,
    source: Option<Source>,
    next_rescan: Option<Nanos>,
    leds: BTreeMap<u16, i32>, // btreemap for deterministic iter order
    feedback: Vec<InputEvent>,
    last_stamp: Nanos,
}

struct Ready {
    signaled: bool,
    sink: PollFlags,
    device: Option<PollFlags>,
}

pub fn run(argv0: &str) -> anyhow::Result<i32> {
    let cli = match cli::parse()? {
        CliResult::Usage => {
            eprint!(
                "\
usage: {argv0} [options] device
try '--help' for more info
"
            );
            return Ok(2);
        }
        CliResult::List => {
            device::list();
            return Ok(0);
        }
        CliResult::Help => {
            eprint!("\
usage: {argv0} [options] device
debounce a /dev/input keyboard device

options:
  -m, --mode <eager|defer|mixed>  debounce mode; mixed means eager press + defer release (default: mixed)
  -w, --window <MS>               debounce window, in ms (default: 20)
  --keys <A,B,...>                debounce only these keys (format: KEY_A or A or 0x1e or30, case insensitive), conflicts with --exclude-keys
  --exclude-keys <A,B,...>        debounce all keys except these (format: KEY_A or A or 0x1e or 30, case insensitive), conflicts with --keys
  --virtual-name <NAME>           name to use for the virtual device (default: the original name + \" (debounced)\")
  --device-rescan-interval <MS>   how often to rescan for the device while unplugged, in ms (default: 500)
  --wait-for-device               wait for the device to appear at startup if it's missing rather than error
  -l, --list                      list available devices and exit
  -h, --help                      display this help and exit
  -V, --version                   output version information and exit

devices like touchpads and mice use the same protocol for sending events as keyboards use for keys,
so it's difficult to distinguish them programmatically; as such, -l/--list just lists every single
device alongside a description of how much it thinks it's a keyboard. \"debouncing\" a touchpad or
mouse doesn't error out and just makes the device unusable.
");
            return Ok(0);
        }
        CliResult::Version => {
            eprintln!("ripples v0.1.0");
            return Ok(0);
        }
        CliResult::Cli(c) => c,
    };

    let filter = cli.key_filter()?;
    let signals = Signals::install()?;

    let (path, mut dev) = open_initial(
        &cli.device,
        cli.wait_for_device,
        cli.device_rescan_interval,
        &signals,
    )?;
    if !device::has_keys(&dev) {
        bail!(
            "{} reports no keys; this device is not a keyboard",
            path.display()
        );
    }
    let fingerprint = Fingerprint::of(&dev);
    warn_of_dropped_events(&dev);

    let virtual_name = cli
        .virtual_name
        .unwrap_or_else(|| format!("{} (debounced)", fingerprint.display_name()));
    if Some(virtual_name.as_str()) == fingerprint.name.as_deref() {
        // the sink copies the source's vendor and product ids so an identical name would be
        // completely indistinguishable from the original and a reconnect scan could falsely attach
        // to it
        bail!("--virtual-name must differ from the name of the source device");
    }

    let sink = Sink::create(&dev, &virtual_name)?;
    wait_until_idle(&mut dev, &signals).context("waiting for the keyboard to go idle")?;
    attach(&mut dev).with_context(|| format!("attaching to {fingerprint}"))?;

    eprintln!(
        "{} on {} -> {:?} ({:?}, {}ms)",
        fingerprint,
        device::describe_path(&path),
        virtual_name,
        cli.mode,
        cli.window.as_millis(),
    );

    let mut app = App {
        fingerprint,
        virtual_name,
        rescan: Nanos::try_from(cli.device_rescan_interval.as_nanos()).unwrap_or(Nanos::MAX),
        pipe: Pipeline::new(cli.mode, cli.window, filter),
        sink,
        source: Some(Source { path, dev }),
        next_rescan: None,
        leds: BTreeMap::new(),
        feedback: Vec::new(),
        last_stamp: 0,
    };

    app.run(&signals)?;
    app.shutdown();
    Ok(0)
}

impl App {
    fn run(&mut self, signals: &Signals) -> anyhow::Result<()> {
        loop {
            let now = device::now();
            self.drain_source()?;
            self.pipe.tick(now);
            self.flush()?;
            self.try_reconnect()?;

            let ready = self.wait(signals)?;
            if ready.signaled {
                return Ok(());
            }
            if ready.sink.contains(PollFlags::IN) {
                self.on_sink_feedback()?;
            }
            if ready.device.is_some_and(|revents| {
                revents.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
            }) {
                self.on_disconnect()?;
            }
        }
    }

    fn wait(&self, signals: &Signals) -> anyhow::Result<Ready> {
        let timeout = self.next_deadline().map(|deadline| {
            device::timespec(Duration::from_nanos(deadline.saturating_sub(device::now())))
        });

        let signal_fd = signals.as_fd();
        let sink_fd = self.sink.as_fd();
        let device_fd = self.source.as_ref().map(|src| src.dev.as_fd());
        let fds: &mut [PollFd] = if let Some(device_fd) = &device_fd {
            &mut [
                PollFd::new(&signal_fd, PollFlags::IN),
                PollFd::new(&sink_fd, PollFlags::IN),
                PollFd::new(device_fd, PollFlags::IN),
            ]
        } else {
            &mut [
                PollFd::new(&signal_fd, PollFlags::IN),
                PollFd::new(&sink_fd, PollFlags::IN),
            ]
        };

        match poll(fds, timeout.as_ref()) {
            Ok(_) => {}
            Err(Errno::INTR) => {
                return Ok(Ready {
                    signaled: false,
                    sink: PollFlags::empty(),
                    device: None,
                });
            }
            Err(e) => return Err(e).context("waiting for input"),
        }

        Ok(Ready {
            signaled: Signals::is_signaled(fds[0].revents()),
            sink: fds[1].revents(),
            device: device_fd.is_some().then(|| fds[2].revents()),
        })
    }

    fn on_sink_feedback(&mut self) -> anyhow::Result<()> {
        self.feedback.clear();
        self.sink
            .feedback(&mut self.feedback)
            .context("reading LED state back from the virtual device")?;

        let mut changed = false;
        for event in &self.feedback {
            if event.event_type() == EventType::LED {
                self.leds.insert(event.code(), event.value());
                changed = true;
            }
        }
        if changed {
            self.sync_leds();
        }
        Ok(())
    }

    fn sync_leds(&mut self) {
        if self.leds.is_empty() {
            return;
        }
        let Some(src) = self.source.as_mut() else {
            return;
        };
        let mut batch: Vec<InputEvent> = self
            .leds
            .iter()
            .map(|(&code, &value)| InputEvent::new(EventType::LED.0, code, value))
            .collect();
        batch.push(InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        ));
        if let Err(e) = src.dev.send_events(&batch) {
            eprintln!("warning: failed to set keyboard LED state: {e}");
        }
    }

    fn next_deadline(&self) -> Option<Nanos> {
        match (self.pipe.next_deadline(), self.next_rescan) {
            (Some(timer), Some(rescan)) => Some(timer.min(rescan)),
            (timer, rescan) => timer.or(rescan),
        }
    }

    fn drain_source(&mut self) -> anyhow::Result<()> {
        loop {
            match self.pump()? {
                Pumped::Read => {}
                Pumped::Empty => return Ok(()),
                Pumped::Disconnected => return self.on_disconnect(),
            }
        }
    }

    fn pump(&mut self) -> anyhow::Result<Pumped> {
        let mut buf = [InputEvent::new(0, 0, 0); 64];
        let count = {
            let Some(src) = self.source.as_ref() else {
                return Ok(Pumped::Empty);
            };
            match events::read(&src.dev, &mut buf) {
                Ok(0) => return Ok(Pumped::Empty),
                Ok(count) => count,
                Err(e) if device::is_disconnect(&e) => return Ok(Pumped::Disconnected),
                Err(e) => return Err(e).context("reading from the keyboard"),
            }
        };

        for &event in &buf[..count] {
            let time = device::stamp(&event);
            self.last_stamp = time;
            match self.pipe.feed(event, time) {
                Fed::Nothing => {}
                Fed::Report => self.flush()?,
                Fed::Dropped => self.resync()?,
            }
        }

        self.flush()?;
        Ok(Pumped::Read)
    }

    fn resync(&mut self) -> anyhow::Result<()> {
        let Some(src) = self.source.as_ref() else {
            return Ok(());
        };
        let held: Vec<u16> = match src.dev.get_key_state() {
            Ok(held) => held.iter().map(|key| key.0).collect(),
            // the next read should report the disconnect
            Err(e) if device::is_disconnect(&e) => return Ok(()),
            Err(e) => return Err(e).context("reading which keys the keyboard has down"),
        };
        self.pipe.resync(&held, self.last_stamp);
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        if self.pipe.output().is_empty() {
            return Ok(());
        }
        let result = self.sink.emit(self.pipe.output());
        self.pipe.clear_output();
        result.with_context(|| format!("writing to {:?}", self.virtual_name))
    }

    fn on_disconnect(&mut self) -> anyhow::Result<()> {
        if let Some(src) = self.source.take() {
            eprintln!(
                "device {} is gone (probably unplugged); waiting for it to come back",
                device::describe_path(&src.path)
            );
        }
        let now = device::now();
        self.pipe.release_all(now);
        self.next_rescan = Some(now.saturating_add(self.rescan));
        self.flush()
    }

    fn try_reconnect(&mut self) -> anyhow::Result<()> {
        // intentionally swallow some errors here; maybe the node appeared before udev set its
        // permissions or the compositor grabbed it for a moment

        let now = device::now();
        match self.next_rescan {
            Some(due) if due <= now => {}
            _ => return Ok(()),
        }
        self.next_rescan = Some(now.saturating_add(self.rescan));

        let Some((path, mut dev)) = device::find_matching(&self.fingerprint, &self.virtual_name)
        else {
            return Ok(());
        };

        // the desktop opened the keyboard we're polling for as soon as it appeared, so a key that
        // was pressed before the grab and released only after it would leave it stuck
        match held_keys(&dev) {
            Ok(held) if held.is_empty() => {}
            Ok(held) => {
                eprintln!(
                    "{held:?} is held on device {}, so can't regrab yet; trying again later",
                    device::describe_path(&path)
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!(
                    "device {} is not usable yet after the replug ({e:#}); trying again later",
                    device::describe_path(&path)
                );
            }
        }

        match attach(&mut dev) {
            Ok(()) => {
                if !self.sink.covers(&dev) {
                    eprintln!(
                        "capabilities changed after the replug; rebuilding the virtual device"
                    );
                    self.sink = Sink::create(&dev, &self.virtual_name)?;
                }
                eprintln!(
                    "device {} is back at {}",
                    self.fingerprint,
                    device::describe_path(&path)
                );
                self.source = Some(Source { path, dev });
                self.next_rescan = None;
                self.sync_leds();
            }
            Err(e) => {
                eprintln!(
                    "device {} is not usable yet after the replug ({e:#}); trying again later",
                    device::describe_path(&path)
                );
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.pipe.release_all(device::now());
        if let Err(e) = self.flush() {
            eprintln!("error: could not release held keys on the way out: {e:#}");
        }

        if let Some(src) = self.source.as_mut()
            && let Err(e) = src.dev.ungrab()
        {
            eprintln!("error: could not ungrab the keyboard: {e}");
        }

        let stats = self.pipe.stats();
        if stats.suppressed() > 0 {
            eprintln!(
                "shutdown: {} of {} edges suppressed, {} autorepeats from the source device dropped",
                stats.suppressed(),
                stats.edges_in,
                stats.repeats_dropped,
            );
        }
        std::thread::sleep(DRAIN_GRACE);
    }
}

fn wait_until_idle(dev: &mut Device, signals: &Signals) -> anyhow::Result<()> {
    dev.set_nonblocking(true)
        .context("setting the device to nonblocking")?;

    let deadline = Instant::now() + GRAB_WAIT;
    let poll = device::timespec(GRAB_IDLE_POLL);
    let mut announced = false;

    loop {
        let held = held_keys(dev)?;
        if held.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "\
warning: {held:?} still held after {GRAB_WAIT:?}, just grabbing anyway; whatever was seeing the \
key as down may keep thinking it's down until a new key event / refocus / etc. happens"
            );
            break;
        }
        if !announced {
            eprintln!(
                "waiting for {held:?} to be released for up to {GRAB_WAIT:?} before grabbing"
            );
            announced = true;
        }
        discard_pending(dev)?;
        if signals.wait(Some(&poll))? {
            bail!("interrupted while waiting for the keyboard to go idle");
        }
    }

    Ok(())
}

fn held_keys(dev: &Device) -> anyhow::Result<Vec<KeyCode>> {
    Ok(dev
        .get_key_state()
        .context("reading which keys the keyboard has down")?
        .iter()
        .collect())
}

fn discard_pending(dev: &Device) -> anyhow::Result<()> {
    let mut buf = [InputEvent::new(0, 0, 0); 64];
    while events::read(&dev, &mut buf).context("reading from the keyboard")? > 0 {}
    Ok(())
}

fn attach(dev: &mut Device) -> anyhow::Result<()> {
    dev.set_nonblocking(true)
        .context("setting the device to nonblocking")?;
    discard_pending(dev)?;
    device::use_monotonic_timestamps(dev)?;
    dev.grab()
        .context("grabbing the keyboard for exclusive use")?;
    Ok(())
}

fn open_initial(
    selected: &Path,
    wait: bool,
    rescan: Duration,
    signals: &Signals,
) -> anyhow::Result<(PathBuf, Device)> {
    let timeout = device::timespec(rescan);
    loop {
        if let Some(found) = device::find(selected)? {
            return Ok(found);
        }
        if !wait {
            bail!(
                "device {} is not present; use -l/--list to list available devices, or --wait-for-device to stay idle until it appears",
                selected.display(),
            );
        }
        if signals.wait(Some(&timeout))? {
            bail!(
                "interrupted while waiting for device {}",
                selected.display()
            );
        }
    }
}

const CARRIED: [EventType; 3] = [EventType::SYNCHRONIZATION, EventType::KEY, EventType::MISC];
const TO_DEVICE: [EventType; 4] = [
    EventType::LED,
    EventType::SOUND,
    EventType::REPEAT,
    EventType::FORCEFEEDBACK,
];

fn warn_of_dropped_events(dev: &Device) {
    let dropped: Vec<EventType> = dev
        .supported_events()
        .iter()
        .filter(|ty| !CARRIED.contains(ty) && !TO_DEVICE.contains(ty))
        .collect();
    if !dropped.is_empty() {
        eprintln!(
            "warning: {dropped:?} events coming from the source device are not (yet) supported and will be dropped"
        );
    }
}
