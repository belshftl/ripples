// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

#[cfg(not(target_os = "linux"))]
fn main() -> ! {
    panic!("ripples is Linux-only; this build can only run the tests");
}

#[cfg(target_os = "linux")]
fn main() {
    // use a temporary binding as to not `temporary value dropped while borrowed`
    let a0_binding = std::env::args_os().next();
    let argv0 = a0_binding.as_deref().map_or(
        std::borrow::Cow::Borrowed("ripples"),
        std::ffi::OsStr::to_string_lossy,
    );
    match ripples::app::main::run(argv0.as_ref()) {
        Ok(rv) => std::process::exit(rv),
        Err(e) => {
            eprintln!("{argv0}: {e:#}");
            std::process::exit(1);
        }
    }
}
