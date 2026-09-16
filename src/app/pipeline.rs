// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Feeds the evdev event stream into the debounce state machine.

use evdev::{EventSummary, EventType, InputEvent, MiscCode, SynchronizationCode};
use std::collections::HashMap;
use std::time::Duration;

use super::cli::KeyFilter;
use crate::debounce::{Debouncer, KeyCode, KeyEvent, Mode, Nanos};

/// `EV_KEY` value for a kernel-generated autorepeat.
const AUTOREPEAT: i32 = 2;

/// Type for the value of an `MSC_SCAN` event, i.e. what the hardware actually sent for the kernel
/// to convert into a keycode.
type Scancode = i32;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub edges_in: u64,
    pub edges_out: u64,
    pub repeats_dropped: u64,
}

impl Stats {
    pub fn suppressed(&self) -> u64 {
        self.edges_in.saturating_sub(self.edges_out)
    }
}

pub struct Pipeline {
    deb: Debouncer,
    filter: KeyFilter,
    /// Last `MSC_SCAN` seen for each key, so that an edge emitted a window late still has the
    /// scancode its packet had.
    scans: HashMap<KeyCode, Scancode>,
    pending_scan: Option<Scancode>,
    out: Vec<InputEvent>,
    edges: Vec<KeyEvent>,
    stats: Stats,
}

impl Pipeline {
    pub fn new(mode: Mode, window: Duration, filter: KeyFilter) -> Pipeline {
        Pipeline {
            deb: Debouncer::new(mode, window),
            filter,
            scans: HashMap::new(),
            pending_scan: None,
            out: Vec::new(),
            edges: Vec::new(),
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn next_deadline(&self) -> Option<Nanos> {
        self.deb.next_deadline()
    }

    pub fn output(&self) -> &[InputEvent] {
        &self.out
    }

    pub fn clear_output(&mut self) {
        self.out.clear();
    }

    pub fn tick(&mut self, now: Nanos) {
        self.deb.on_timeout(now, &mut self.edges);
        self.drain_edges();
    }

    /// Returns true at a packet boundary, where the caller should flush.
    pub fn feed(&mut self, event: InputEvent, time: Nanos) -> bool {
        self.tick(time);

        match event.destructure() {
            EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                self.pending_scan = None;
                return true;
            }
            EventSummary::Synchronization(..) => {}
            EventSummary::Misc(_, MiscCode::MSC_SCAN, value) => {
                self.pending_scan = Some(value);
            }
            EventSummary::Key(_, code, value) => self.key(code.0, value, time),
            _ => self.out.push(event),
        }
        false
    }

    pub fn release_all(&mut self, now: Nanos) {
        self.deb.release_all(now, &mut self.edges);
        self.drain_edges();
        self.pending_scan = None;
    }

    fn key(&mut self, key: KeyCode, value: i32, time: Nanos) {
        let scan = self.pending_scan.take();
        if let Some(scan) = scan {
            self.scans.insert(key, scan);
        }

        // the source's repeat timer goes off the physical key state, and the virtual device has
        // its own repeat timer based on its state; forwarding these would double every repeat
        if value == AUTOREPEAT {
            self.stats.repeats_dropped += 1;
            return;
        }

        if !self.filter.should_debounce(key) {
            self.push_key(key, value, scan);
            return;
        }

        self.stats.edges_in += 1;
        self.deb.on_key(time, key, value != 0, &mut self.edges);
        self.drain_edges();
    }

    fn drain_edges(&mut self) {
        for edge in self.edges.drain(..) {
            if let Some(&scan) = self.scans.get(&edge.key) {
                self.out.push(InputEvent::new(
                    EventType::MISC.0,
                    MiscCode::MSC_SCAN.0,
                    scan,
                ));
            }
            self.out.push(InputEvent::new(
                EventType::KEY.0,
                edge.key,
                i32::from(edge.down),
            ));
            self.stats.edges_out += 1;
        }
    }

    fn push_key(&mut self, key: KeyCode, value: i32, scan: Option<Scancode>) {
        if let Some(scan) = scan {
            self.out.push(InputEvent::new(
                EventType::MISC.0,
                MiscCode::MSC_SCAN.0,
                scan,
            ));
        }
        self.out.push(InputEvent::new(EventType::KEY.0, key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::ms;
    use std::collections::HashSet;

    const A: KeyCode = 30; // KEY_A
    const SCAN: Scancode = 0x0007_0004; // HID usage a USB keyboard would report for KEY_A
    const WINDOW: Duration = Duration::from_millis(20);

    fn key_ev(key: KeyCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, key, value)
    }

    fn press(pipe: &mut Pipeline, key: KeyCode, scan: Scancode, value: i32, time: Nanos) -> bool {
        pipe.feed(
            InputEvent::new(EventType::MISC.0, MiscCode::MSC_SCAN.0, scan),
            time,
        );
        pipe.feed(key_ev(key, value), time);
        pipe.feed(
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ),
            time,
        )
    }

    /// Returns `(event_type, code, value)` triplets of the output.
    fn dump(pipe: &Pipeline) -> Vec<(u16, u16, i32)> {
        pipe.output()
            .iter()
            .map(|e| (e.event_type().0, e.code(), e.value()))
            .collect()
    }

    #[test]
    fn forwarded_key_keeps_its_scancode() {
        let mut pipe = Pipeline::new(Mode::Mixed, WINDOW, KeyFilter::All);
        let boundary = press(&mut pipe, A, SCAN, 1, ms(0));
        assert!(boundary, "SYN_REPORT must be reported as a packet boundary");
        assert_eq!(
            dump(&pipe),
            [
                (EventType::MISC.0, MiscCode::MSC_SCAN.0, SCAN),
                (EventType::KEY.0, A, 1),
            ]
        );
    }

    #[test]
    fn suppressed_key_includes_the_scancode() {
        let mut pipe = Pipeline::new(Mode::Defer, WINDOW, KeyFilter::All);
        press(&mut pipe, A, SCAN, 1, ms(0));
        assert_eq!(dump(&pipe), [], "the press should still be in settle");

        pipe.tick(ms(20));
        assert_eq!(
            dump(&pipe),
            [
                (EventType::MISC.0, MiscCode::MSC_SCAN.0, SCAN),
                (EventType::KEY.0, A, 1),
            ]
        );
    }

    #[test]
    fn autorepeat_from_the_source_is_dropped() {
        let mut pipe = Pipeline::new(Mode::Mixed, WINDOW, KeyFilter::All);
        press(&mut pipe, A, SCAN, 1, ms(0));
        pipe.clear_output();

        // the virtual device has its own autorepeat so the one from the source must be dropped
        pipe.feed(key_ev(A, AUTOREPEAT), ms(300));
        pipe.feed(key_ev(A, AUTOREPEAT), ms(333));
        assert_eq!(dump(&pipe), []);
        assert_eq!(pipe.stats().repeats_dropped, 2);
    }

    #[test]
    fn keys_not_covered_by_the_filter_are_just_forwarded() {
        let mut pipe = Pipeline::new(Mode::Defer, WINDOW, KeyFilter::Only(HashSet::new()));
        press(&mut pipe, A, SCAN, 1, ms(0));
        pipe.feed(key_ev(A, AUTOREPEAT), ms(300));
        pipe.feed(key_ev(A, 0), ms(1));
        assert_eq!(
            dump(&pipe),
            [
                (EventType::MISC.0, MiscCode::MSC_SCAN.0, SCAN),
                (EventType::KEY.0, A, 1),
                (EventType::KEY.0, A, 0),
            ],
            "an unfiltered key must be plainly forwarded but the autorepeat should still be dropped",
        );
        assert_eq!(pipe.stats().edges_in, 0);
    }

    #[test]
    fn losing_the_device_releases_what_downstream_holds() {
        let mut pipe = Pipeline::new(Mode::Mixed, WINDOW, KeyFilter::All);
        press(&mut pipe, A, SCAN, 1, ms(0));
        pipe.clear_output();
        pipe.release_all(ms(10));
        assert_eq!(
            dump(&pipe),
            [
                (EventType::MISC.0, MiscCode::MSC_SCAN.0, SCAN),
                (EventType::KEY.0, A, 0)
            ]
        );
    }
}
