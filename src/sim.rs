// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Drives a [`Debouncer`] like a real event loop. Purely for tests.

use crate::debounce::{Debouncer, KeyCode, KeyEvent, Mode, Nanos};
use std::time::Duration;

pub const fn ms(v: u64) -> Nanos {
    v * 1_000_000
}

pub struct Sim {
    deb: Debouncer,
    out: Vec<KeyEvent>,
}

impl Sim {
    pub fn new(mode: Mode, window: Nanos) -> Self {
        Self {
            deb: Debouncer::new(mode, Duration::from_nanos(window)),
            out: Vec::new(),
        }
    }

    pub fn feed(&mut self, time: Nanos, key: KeyCode, down: bool) -> &mut Self {
        self.deb.on_timeout(time, &mut self.out);
        self.deb.on_key(time, key, down, &mut self.out);
        self
    }

    pub fn idle_until(&mut self, time: Nanos) -> &mut Self {
        while let Some(deadline) = self.deb.next_deadline() {
            if deadline > time {
                break;
            }
            self.deb.on_timeout(deadline, &mut self.out);
        }
        self
    }

    pub fn drain(&mut self) -> &mut Self {
        while let Some(deadline) = self.deb.next_deadline() {
            self.deb.on_timeout(deadline, &mut self.out);
        }
        self
    }

    pub fn release_all(&mut self, time: Nanos) -> &mut Self {
        self.deb.release_all(time, &mut self.out);
        self
    }

    pub fn debouncer(&self) -> &Debouncer {
        &self.deb
    }

    pub fn events(&self) -> &[KeyEvent] {
        &self.out
    }
}

pub fn run(mode: Mode, window: Nanos, input: &[(Nanos, KeyCode, bool)]) -> Vec<KeyEvent> {
    let mut sim = Sim::new(mode, window);
    for &(time, key, down) in input {
        sim.feed(time, key, down);
    }
    sim.drain();
    sim.out
}
