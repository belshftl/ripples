// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Virtual uinput device for the filtered events.
//!
//! `evdev::uinput` is not viable because the builder doesn't have `UI_SET_LEDBIT` and keeps its
//! descriptor private, so it's not possible to declare LEDs with it, which in turns is problematic
//! because not passing LED events through means the lights on the real keyboard stop meaning anything.

use anyhow::{Context, bail};
use evdev::{
    AttributeSet, AttributeSetRef, AutoRepeat, Device, EventType, InputEvent, InputId, KeyCode,
    LedCode, MiscCode, RepeatCode, SynchronizationCode,
};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::ioctl::{IntegerSetter, NoArg, Opcode, Setter, ioctl, opcode};
use std::ffi::c_int;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

const UINPUT_PATH: &str = "/dev/uinput";
const UINPUT: u8 = b'U';

/// Max size of `uinput_setup.name`; the name + a null terminator has to fit into this.
const UINPUT_MAX_NAME_SIZE: usize = 80;
const MAX_NAME: usize = UINPUT_MAX_NAME_SIZE - 2;

// the `UI_SET_*BIT` opcodes encode `int` but argument is read by value and not through a pointer
const UI_SET_EVBIT: Opcode = opcode::write::<c_int>(UINPUT, 100);
const UI_SET_KEYBIT: Opcode = opcode::write::<c_int>(UINPUT, 101);
const UI_SET_MSCBIT: Opcode = opcode::write::<c_int>(UINPUT, 104);
const UI_SET_LEDBIT: Opcode = opcode::write::<c_int>(UINPUT, 105);
type DevSetup = Setter<{ opcode::write::<UinputSetup>(UINPUT, 3) }, UinputSetup>;
type DevCreate = NoArg<{ opcode::none(UINPUT, 1) }>;
type DevDestroy = NoArg<{ opcode::none(UINPUT, 2) }>;

#[repr(C)]
#[derive(Clone, Copy)]
struct RawInputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

// ffi match of `struct uinput_setup` from `linux/uinput.h`; there aren't any fields that change
// size by platform/arch so having one central definition is safe
#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id: RawInputId,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

pub struct Caps<'a> {
    pub id: InputId,
    pub keys: &'a AttributeSetRef<KeyCode>,
    pub leds: Option<&'a AttributeSetRef<LedCode>>,
    pub misc: Option<&'a AttributeSetRef<MiscCode>>,
    pub repeat: Option<AutoRepeat>,
}

pub struct Sink {
    fd: OwnedFd,
    keys: AttributeSet<KeyCode>,
    leds: AttributeSet<LedCode>,
    repeats: bool,
    batch: Vec<InputEvent>,
}

impl Sink {
    pub fn create(source: &Device, name: &str) -> anyhow::Result<Sink> {
        let keys = source
            .supported_keys()
            .context("the source device reports no keys")?;
        Sink::with_caps(
            &Caps {
                // copy vendor/product so libinput workarounds and keymaps and other things keyed on
                // them still apply
                id: source.input_id(),
                keys,
                leds: source.supported_leds(),
                misc: source.misc_properties(),
                repeat: source.get_auto_repeat(),
            },
            name,
        )
    }

    pub fn with_caps(caps: &Caps<'_>, name: &str) -> anyhow::Result<Sink> {
        let fd = open(
            UINPUT_PATH,
            OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(open_uinput_error)?;

        // SAFETY: all these opcodes take the bit to set by value; the `fd` is a just-opened
        // /dev/uinput with no device created on it yet
        unsafe {
            set_bit::<UI_SET_EVBIT>(&fd, EventType::KEY.0)?;
            for key in caps.keys {
                set_bit::<UI_SET_KEYBIT>(&fd, key.0)?;
            }
            if let Some(misc) = caps.misc {
                set_bit::<UI_SET_EVBIT>(&fd, EventType::MISC.0)?;
                for code in misc {
                    set_bit::<UI_SET_MSCBIT>(&fd, code.0)?;
                }
            }
            if let Some(leds) = caps.leds {
                set_bit::<UI_SET_EVBIT>(&fd, EventType::LED.0)?;
                for led in leds {
                    set_bit::<UI_SET_LEDBIT>(&fd, led.0)?;
                }
            }
            // there's no UI_SET_REPBIT, declaring EV_REP is all it takes
            if caps.repeat.is_some() {
                set_bit::<UI_SET_EVBIT>(&fd, EventType::REPEAT.0)?;
            }
        }

        let setup = UinputSetup {
            id: RawInputId {
                bustype: caps.id.bus_type().0,
                vendor: caps.id.vendor(),
                product: caps.id.product(),
                version: caps.id.version(),
            },
            name: encode_name(name)?,
            ff_effects_max: 0,
        };

        // SAFETY: `UI_DEV_SETUP` takes `struct uinput_setup *`, which is what the `Setter` passes,
        // and `UI_DEV_CREATE` takes no argument
        unsafe {
            ioctl(&fd, DevSetup::new(setup)).context("UI_DEV_SETUP")?;
            ioctl(&fd, DevCreate::new()).context("UI_DEV_CREATE")?;
        }

        let mut sink = Sink {
            fd,
            keys: owned_keys(caps.keys),
            leds: caps.leds.map(owned_leds).unwrap_or_default(),
            repeats: caps.repeat.is_some(),
            batch: Vec::new(),
        };

        // these need to be carried over manually
        if let Some(repeat) = &caps.repeat {
            sink.emit(&[
                InputEvent::new(
                    EventType::REPEAT.0,
                    RepeatCode::REP_DELAY.0,
                    i32::try_from(repeat.delay).unwrap_or(i32::MAX),
                ),
                InputEvent::new(
                    EventType::REPEAT.0,
                    RepeatCode::REP_PERIOD.0,
                    i32::try_from(repeat.period).unwrap_or(i32::MAX),
                ),
            ])
            .context("setting the repeat rate of the virtual device")?;
        }

        Ok(sink)
    }

    /// Writes one source packet and the terminating `SYN_REPORT`.
    pub fn emit(&mut self, events: &[InputEvent]) -> std::io::Result<()> {
        self.batch.clear();
        self.batch.extend_from_slice(events);
        self.batch.push(InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        ));
        write_all(&self.fd, as_bytes(&self.batch))
    }

    /// Events written to the virtual device from elsewhere, i.e. LED state.
    pub fn feedback(&self, out: &mut Vec<InputEvent>) -> std::io::Result<()> {
        let mut buf = [InputEvent::new(0, 0, 0); 16];
        loop {
            match rustix::io::read(&self.fd, as_bytes_mut(&mut buf)) {
                Ok(0) | Err(Errno::AGAIN) => return Ok(()),
                Ok(r) => out.extend_from_slice(&buf[..r / size_of::<InputEvent>()]),
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Whether this sink covers everything `source` is capable of producing. If a device gets
    /// replugged and gains a new key or LED, we need a new sink.
    pub fn covers(&self, source: &Device) -> bool {
        let keys = source
            .supported_keys()
            .is_none_or(|keys| keys.iter().all(|key| self.keys.contains(key)));
        let leds = source
            .supported_leds()
            .is_none_or(|leds| leds.iter().all(|led| self.leds.contains(led)));
        let repeats = source.get_auto_repeat().is_none() || self.repeats;
        keys && leds && repeats
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // closing the fd would destroy the device too, explicitly doing it just means it happens
        // here rather than somewhere later in the drop order

        // SAFETY: `UI_DEV_DESTROY` takes no argument, and the fd is our own uinput handle with a
        // device created on it
        _ = unsafe { ioctl(&self.fd, DevDestroy::new()) };
    }
}

/// # Safety
///
/// `OPCODE` must be one of the `UI_SET_*BIT` opcodes, which take the bit to set by value, and
/// `bit` must be a capability index of the kind that opcode expects.
unsafe fn set_bit<const OPCODE: Opcode>(fd: &OwnedFd, bit: u16) -> anyhow::Result<()> {
    // SAFETY: guaranteed by the caller
    unsafe { ioctl(fd, IntegerSetter::<OPCODE>::new_usize(usize::from(bit))) }
        .with_context(|| format!("declaring capability {bit:#x} on the virtual device"))
}

fn open_uinput_error(err: Errno) -> anyhow::Error {
    let hint = match err {
        Errno::NOENT => " (the uinput module may not be loaded: `modprobe uinput`)",
        Errno::ACCESS | Errno::PERM => {
            " (needs root, or a udev rule giving the `input` group access to it)"
        }
        _ => "",
    };
    anyhow::Error::new(err).context(format!("opening {UINPUT_PATH}{hint}"))
}

fn encode_name(name: &str) -> anyhow::Result<[u8; UINPUT_MAX_NAME_SIZE]> {
    if name.is_empty() {
        bail!("the virtual device must have a name");
    }
    let mut end = name.len().min(MAX_NAME);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let mut encoded = [0u8; UINPUT_MAX_NAME_SIZE];
    encoded[..end].copy_from_slice(&name.as_bytes()[..end]);
    Ok(encoded)
}

fn owned_keys(keys: &AttributeSetRef<KeyCode>) -> AttributeSet<KeyCode> {
    let mut set = AttributeSet::new();
    for key in keys {
        set.insert(key);
    }
    set
}

fn owned_leds(leds: &AttributeSetRef<LedCode>) -> AttributeSet<LedCode> {
    let mut set = AttributeSet::new();
    for led in leds {
        set.insert(led);
    }
    set
}

fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(w) => bytes = &bytes[w..],
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn as_bytes(events: &[InputEvent]) -> &[u8] {
    // SAFETY: `InputEvent` is ffi-compatible with `struct input_event`, whose fields are all plain
    // integers, so every bit pattern is a valid value, and there is no padding. therefore, the cast
    // is sound in both directions
    unsafe { std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), size_of_val(events)) }
}

fn as_bytes_mut(events: &mut [InputEvent]) -> &mut [u8] {
    let len = size_of_val(events);
    // SAFETY: see above in `as_bytes`
    unsafe { std::slice::from_raw_parts_mut(events.as_mut_ptr().cast::<u8>(), len) }
}
