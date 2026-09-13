// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

#[cfg(not(target_os = "linux"))]
fn main() -> ! {
    panic!("ripples is Linux-only; this build can only run the tests");
}

#[cfg(target_os = "linux")]
fn main() {
    todo!()
}
