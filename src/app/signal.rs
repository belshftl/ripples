// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Signal management.
//!
//! This needs them delivered on a readable fd, and Linux has signalfd for that exact purpose, but
//! rustix doesn't implement it and it'd need `sigprocmask` which is buried in rustix internals, and
//! `signal_hook` is the usual idiomatic solution anyways.

use anyhow::Context;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with}; // if this doesn't resolve for you, compile on linux
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

const CAUGHT: [i32; 3] = [SIGINT, SIGTERM, SIGHUP];

pub struct Signals {
    rx: OwnedFd,
}

impl Signals {
    pub fn install() -> anyhow::Result<Signals> {
        let (rx, tx) = pipe_with(PipeFlags::CLOEXEC).context("creating the signal pipe")?;
        for sig in CAUGHT {
            let tx = tx.try_clone().context("dup-ing the signal pipe tx")?;
            signal_hook::low_level::pipe::register(sig, tx)
                .with_context(|| format!("installing the handler for signal {sig}"))?;
        }
        Ok(Signals { rx })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.rx.as_fd()
    }

    pub fn is_signaled(revents: PollFlags) -> bool {
        revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
    }

    /// Waits for a signal or the timeout, whichever comes first; returns whether the signal is what
    /// woke. `None` waits indefinitely.
    pub fn wait(&self, timeout: Option<&Timespec>) -> anyhow::Result<bool> {
        let mut fds = [PollFd::new(&self.rx, PollFlags::IN)];
        match poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(Errno::INTR) => return Ok(false),
            Err(e) => return Err(e).context("waiting for a signal"),
        }
        Ok(Signals::is_signaled(fds[0].revents()))
    }
}
