// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Finds, identifies, and prepares the source device.

use anyhow::Context;
use evdev::{Device, EventType, InputEvent};
use rustix::event::{Secs, Timespec};
use rustix::io::Errno;
use rustix::ioctl::{Setter, opcode};
use rustix::time::{ClockId, clock_gettime};
use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tabular::{Row, Table, row};

use crate::debounce::Nanos;

/// Identity used to recognize a device on replug.
///
/// Two identical keyboards that don't report a serial in `uniq` end up having the same fingerprint,
/// and reconnecting to "the same" one is a coin flip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub name: Option<String>,
    pub uniq: Option<String>,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub bus: u16,
}

impl Fingerprint {
    pub fn of(dev: &Device) -> Fingerprint {
        let id = dev.input_id();
        Fingerprint {
            name: dev.name().map(str::to_owned),
            uniq: dev.unique_name().map(str::to_owned),
            vendor: id.vendor(),
            product: id.product(),
            version: id.version(),
            bus: id.bus_type().0,
        }
    }

    pub fn matches(&self, dev: &Device) -> bool {
        *self == Fingerprint::of(dev)
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("<unnamed device>")
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} [{:04x}:{:04x}]",
            self.display_name(),
            self.vendor,
            self.product
        )
    }
}

/// How high a device should be listed on `--list`; the lower, the more certain it's a keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DeviceListingRank {
    Keyboard,
    MaybeKeyboard,
    ProbablyNotKeyboard,
    NotKeyboard,
}

impl DeviceListingRank {
    fn rank(dev: &Device) -> Self {
        // this is pretty inefficient but it doesn't have a need to be efficient

        if !dev.supported_events().contains(EventType::KEY) {
            return Self::NotKeyboard;
        }
        let Some(keys) = dev.supported_keys() else {
            return Self::NotKeyboard;
        };
        if keys
            .iter()
            .any(|k| k == evdev::KeyCode::KEY_A || k == evdev::KeyCode::KEY_ENTER)
        {
            return Self::Keyboard;
        }
        if keys.iter().next().is_none() {
            return Self::NotKeyboard;
        }
        let Some(name) = dev.name() else {
            return Self::NotKeyboard;
        };
        let name = name.to_ascii_lowercase();
        // "video bus" may seem absurd but for some reason it comes up as "looks like a keyboard" on
        // the linux box i tested this on
        if name.contains("touchpad")
            || name.contains("trackpad")
            || name.contains("mouse")
            || name.contains("speaker")
            || name.contains("headphone")
            || name.contains("headset")
            || name.contains("power button")
            || name.contains("video bus")
        {
            return Self::ProbablyNotKeyboard;
        }
        Self::MaybeKeyboard
    }

    fn describe(self) -> &'static str {
        match self {
            Self::NotKeyboard => "not a keyboard",
            Self::MaybeKeyboard => "looks like a keyboard?",
            Self::ProbablyNotKeyboard => "probably not a keyboard",
            Self::Keyboard => "keyboard",
        }
    }
}

/// Whether the device supports any keys at all.
pub fn has_keys(dev: &Device) -> bool {
    dev.supported_events().contains(EventType::KEY)
        && dev
            .supported_keys()
            .is_some_and(|k| k.iter().next().is_some())
}

/// Opens the keyboard at `path`. Returns `Ok(None)` if it's not there (ENOENT), since that's an
/// ordinary state (not plugged in yet) rather than an error.
pub fn find(path: &Path) -> anyhow::Result<Option<(PathBuf, Device)>> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match Device::open(&real) {
        Ok(dev) => Ok(Some((real, dev))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(e).with_context(|| {
            format!(
                "opening {} (which needs root or the user being in the `input` group)",
                real.display()
            )
        }),
        Err(e) => Err(e).with_context(|| format!("opening {}", real.display())),
    }
}

pub fn find_matching(fingerprint: &Fingerprint, exclude: &str) -> Option<(PathBuf, Device)> {
    evdev::enumerate().find(|(_, dev)| dev.name() != Some(exclude) && fingerprint.matches(dev))
}

pub fn list() {
    let mut rows: Vec<(Row, DeviceListingRank)> = Vec::new();
    for (path, dev) in evdev::enumerate() {
        let id = dev.input_id();
        let rank = DeviceListingRank::rank(&dev);
        rows.push((
            row!(
                path.display(),
                format!("{:04x}", id.vendor()),
                format!("{:04x}", id.product()),
                id.bus_type().to_string(),
                dev.name().unwrap_or("<unnamed device>"),
                rank.describe(),
            ),
            rank,
        ));
    }
    if rows.is_empty() {
        eprintln!("no input devices could be opened; are you in the `input` group?");
        return;
    }
    rows.sort_unstable_by_key(|(_, rank)| *rank);

    // tabular has no `Table::from_rows` / `Table::with_rows`, for some reason
    let mut table = Table::new("{:<} | {:<}:{:<} | {:<} | {:<} | {:<}");
    for (row, _) in rows {
        table.add_row(row);
    }
    print!("{table}");
}

/// `EVIOCSCLOCKID`, i.e. `_IOW('E', 0xa0, int)`.
type SetClockId = Setter<{ opcode::write::<c_int>(b'E', 0xa0) }, c_int>;

/// `CLOCK_MONOTONIC`, which is 1 on every Linux arch. Manually defined because rustix's `ClockId`
/// is an enum requiring an `as` cast to `c_int`, and this codebase is set to disallow `as` casts.
const CLOCK_MONOTONIC: c_int = 1;

/// Asks the kernel to use `CLOCK_MONOTONIC` for this device's events. Otherwise, the timestamps
/// come from `CLOCK_REALTIME`, so they're functionally useless.
pub fn use_monotonic_timestamps(dev: &Device) -> anyhow::Result<()> {
    // SAFETY: the opcode is the one the kernel documents for this ioctl, and its operand is an
    // `int` passed by pointer, which is what the `Setter` writes
    let request = unsafe { SetClockId::new(CLOCK_MONOTONIC) };

    // SAFETY: the fd is a live evdev handle, and the request above matches the opcode
    unsafe { rustix::ioctl::ioctl(dev, request) }
        .context("switching the device to CLOCK_MONOTONIC via EVIOCSCLOCKID")
}

/// # Panics
///
/// Panics if `CLOCK_MONOTONIC` reports a negative time, which would mean the kernel is broken.
pub fn now() -> Nanos {
    let ts = clock_gettime(ClockId::Monotonic);
    let secs = Nanos::try_from(ts.tv_sec).expect("CLOCK_MONOTONIC is never negative");
    let nanos = Nanos::try_from(ts.tv_nsec).expect("timespec's tv_nsec is never negative");
    secs * 1_000_000_000 + nanos
}

pub fn timespec(duration: Duration) -> Timespec {
    Timespec::try_from(duration).unwrap_or(Timespec {
        tv_sec: Secs::MAX,
        tv_nsec: 0,
    })
}

pub fn stamp(event: &InputEvent) -> Nanos {
    event
        .timestamp()
        .duration_since(SystemTime::UNIX_EPOCH) // epoch isn't actually relevant, anything works here
        .map_or(0, |dur| {
            Nanos::try_from(dur.as_nanos()).unwrap_or(Nanos::MAX)
        })
}

pub fn is_disconnect(err: &std::io::Error) -> bool {
    Errno::from_io_error(err)
        .is_some_and(|errno| [Errno::NODEV, Errno::NOENT, Errno::IO, Errno::NXIO].contains(&errno))
}

pub fn describe_path(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}
