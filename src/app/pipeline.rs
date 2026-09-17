// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Feeds the evdev event stream into the debounce state machine.

use evdev::{EventSummary, EventType, InputEvent, MiscCode, SynchronizationCode};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::cli::KeyFilter;
use crate::debounce::{Debouncer, KeyCode, KeyEvent, Mode, Nanos};

/// `EV_KEY` value for a kernel-generated autorepeat.
const AUTOREPEAT: i32 = 2;

/// Type for the value of an `MSC_SCAN` event, i.e. what the hardware actually sent for the kernel
/// to convert into a keycode.
type Scancode = i32;

/// What the caller must do after feeding an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fed {
    Nothing,
    /// A packet ended, flush the output.
    Report,
    /// The kernel dropped events for this reader, call [`Pipeline::resync`] with the keyboard's
    /// current key state.
    Dropped,
}

#[derive(Debug, Clone, Copy, Default)]
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
    /// Keys down according to the events seen so far.
    down: HashSet<KeyCode>,
    /// Set from a `SYN_DROPPED` until the report that ends the partial packet after it.
    discarding: bool,
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
            down: HashSet::new(),
            discarding: false,
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

    pub fn feed(&mut self, event: InputEvent, time: Nanos) -> Fed {
        self.tick(time);

        let summary = event.destructure();
        if self.discarding {
            if matches!(
                summary,
                EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _)
            ) {
                self.discarding = false;
            }
            return Fed::Nothing;
        }

        match summary {
            EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                self.pending_scan = None;
                return Fed::Report;
            }
            EventSummary::Synchronization(_, SynchronizationCode::SYN_DROPPED, _) => {
                self.pending_scan = None;
                self.discarding = true;
                return Fed::Dropped;
            }
            EventSummary::Misc(_, MiscCode::MSC_SCAN, value) => {
                self.pending_scan = Some(value);
            }
            EventSummary::Key(_, code, value) => self.key(code.0, value, time),
            EventSummary::Misc(..) => self.out.push(event),
            // everything else is either
            // - a sync code other than `SYN_REPORT`
            // - an event type the virtual device doesn't declare
            // - an echo of something we sent the keyboard (leds, repeat settings, ...), which the
            //   kernel delivers as input
            _ => {}
        }
        Fed::Nothing
    }

    pub fn resync(&mut self, held: &[KeyCode], time: Nanos) {
        self.tick(time);
        let held: HashSet<KeyCode> = held.iter().copied().collect();
        let mut missed: Vec<(KeyCode, i32)> = self
            .down
            .difference(&held)
            .map(|&key| (key, 0))
            .chain(held.difference(&self.down).map(|&key| (key, 1)))
            .collect();
        missed.sort_unstable();
        for (key, value) in missed {
            self.key(key, value, time);
        }
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
        if value == 0 {
            self.down.remove(&key);
        } else {
            self.down.insert(key);
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
    use evdev::{LedCode, RepeatCode};
    use std::collections::HashSet;

    const A: KeyCode = 30; // KEY_A
    const B: KeyCode = 48; // KEY_B
    const SCAN: Scancode = 0x0007_0004; // HID usage a USB keyboard would report for KEY_A
    const WINDOW: Duration = Duration::from_millis(20);

    fn key_ev(key: KeyCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, key, value)
    }

    fn syn_ev() -> InputEvent {
        InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        )
    }

    fn press(pipe: &mut Pipeline, key: KeyCode, scan: Scancode, value: i32, time: Nanos) -> Fed {
        pipe.feed(
            InputEvent::new(EventType::MISC.0, MiscCode::MSC_SCAN.0, scan),
            time,
        );
        pipe.feed(key_ev(key, value), time);
        pipe.feed(syn_ev(), time)
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
        assert_eq!(
            boundary,
            Fed::Report,
            "SYN_REPORT must be reported as a packet boundary"
        );
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
    fn a_drop_skips_the_partial_packet_and_resync_makes_up_the_missed_edges() {
        let mut pipe = Pipeline::new(Mode::Eager, WINDOW, KeyFilter::All);
        press(&mut pipe, A, SCAN, 1, ms(0));
        pipe.clear_output();

        let dropped = InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_DROPPED.0,
            0,
        );
        assert_eq!(pipe.feed(dropped, ms(100)), Fed::Dropped);
        pipe.feed(key_ev(B, 1), ms(100));
        assert_eq!(
            pipe.feed(syn_ev(), ms(100)),
            Fed::Nothing,
            "the report that ends the partial packet is not a real one"
        );
        assert_eq!(dump(&pipe), []);

        pipe.resync(&[B], ms(100));
        assert_eq!(
            dump(&pipe),
            [
                (EventType::MISC.0, MiscCode::MSC_SCAN.0, SCAN),
                (EventType::KEY.0, A, 0),
                (EventType::KEY.0, B, 1),
            ]
        );
    }

    #[test]
    fn echoes_of_outgoing_events_are_not_forwarded() {
        let mut pipe = Pipeline::new(Mode::Mixed, WINDOW, KeyFilter::All);
        pipe.feed(
            InputEvent::new(EventType::LED.0, LedCode::LED_CAPSL.0, 1),
            ms(0),
        );
        pipe.feed(
            InputEvent::new(EventType::REPEAT.0, RepeatCode::REP_DELAY.0, 250),
            ms(0),
        );
        pipe.feed(syn_ev(), ms(0));
        assert_eq!(dump(&pipe), []);
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
