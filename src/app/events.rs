// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

use evdev::InputEvent;
use rustix::io::Errno;
use std::os::fd::AsFd;

pub fn read<F: AsFd>(fd: &F, buf: &mut [InputEvent]) -> std::io::Result<usize> {
    loop {
        match rustix::io::read(fd, as_bytes_mut(buf)) {
            Ok(bytes) => return Ok(bytes / size_of::<InputEvent>()),
            Err(Errno::AGAIN) => return Ok(0),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn write_all<F: AsFd>(fd: &F, events: &[InputEvent]) -> std::io::Result<()> {
    let mut bytes = as_bytes(events);
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
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
