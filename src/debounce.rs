// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Debounce state machine.
//!
//! # Contract
//!
//! - before feeding an input event timestamped `t`, call [`Debouncer::on_timeout`] with `t`;
//! - when idle, call [`Debouncer::on_timeout`] with the current time no later than
//!   [`Debouncer::next_deadline`];
//! - input timestamps must be weakly monotonic.
//!
//! # Guarantees
//!
//! Given a `window` of `w` and input obeying the contract, the following is guaranteed:
//!
//! - parity: output levels for a given key strictly alternate, starting with a press;
//! - monotonicity: output timestamps are weakly monotonic;
//! - consistency: every output event has a corresponding input event of the same key and level
//!   with a timestamp in the range `[t_out - w, t_out]` (as such, latency is bounded by `w`);
//! - convergence: once the input has been quiet for `w` and the timers have been drained, the
//!   downstream level matches the physical level on every key;
//! - suppression: two consecutive presses of the same key are at least `w` apart; under
//!   [`Mode::Eager`] and [`Mode::Defer`] this extends to all pairs of consecutive edges, but under
//!   [`Mode::Mixed`] a suppressed release can be emitted at the same instant as the press that ends
//!   the hold.
//! - cleanliness: traffic where every edge is at least `w` apart passes through unchanged; as
//!   such, [`Mode::Eager`] is idempotent.

use std::collections::HashMap;
use std::time::Duration;

/// Monotonic timestamp in ns measured from an unspecified starting point.
pub type Nanos = u64;

/// A Linux `KEY_*` / `BTN_*` code.
pub type KeyCode = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Eager on both edges. Reports a change, then ignores the key for one window and resamples.
    /// Not true eager ("emit the first seen release"), as it'd be functionally useless for
    /// preventing chatter and constantly drop modifiers mid-chord, mess up autorepeat, and make
    /// games unplayable.
    Eager,

    /// Defer on both edges. Reports a change once the level has been held for one window.
    /// Adds one window of latency to every keypress.
    Defer,

    /// Eager-press, defer-release. Typing latency is unchanged and the usual symptom, release
    /// chatter, gets mitigated. Most comfortable default.
    Mixed,
}

/// What the machine does with one edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Emit,
    Settle,
}

impl Mode {
    fn policy(self, down: bool) -> Policy {
        match (self, down) {
            (Mode::Eager, _) | (Mode::Mixed, true) => Policy::Emit,
            (Mode::Defer, _) | (Mode::Mixed, false) => Policy::Settle,
        }
    }

    fn lockout(self) -> bool {
        self == Mode::Eager
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub time: Nanos,
    pub key: KeyCode,
    pub down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
    /// Input is being ignored until the deadline, after which the key gets resampled.
    Lockout,

    /// The level isn't matching downstream and will be reported at the deadline unless it happens
    /// to go back to matching before the deadline.
    Settle,
}

#[derive(Debug, Clone, Copy)]
struct Timer {
    deadline: Nanos,
    kind: TimerKind,
}

#[derive(Debug, Clone, Copy, Default)]
struct KeyState {
    logical: bool,
    physical: bool,
    timer: Option<Timer>,
}

#[derive(Debug, Clone, Copy)]
struct Config {
    mode: Mode,
    window: Nanos,
}

#[derive(Debug, Clone)]
pub struct Debouncer {
    cfg: Config,
    keys: HashMap<KeyCode, KeyState>,
    last_emit: Nanos,
}

impl Debouncer {
    pub fn new(mode: Mode, window: Duration) -> Self {
        Self {
            cfg: Config {
                mode,
                window: u64::try_from(window.as_nanos()).unwrap_or(u64::MAX),
            },
            keys: HashMap::new(),
            last_emit: 0,
        }
    }

    /// What downstream should currently be seeing.
    pub fn is_down(&self, key: KeyCode) -> bool {
        self.keys.get(&key).is_some_and(|s| s.logical)
    }

    pub fn next_deadline(&self) -> Option<Nanos> {
        self.keys
            .values()
            .filter_map(|s| s.timer.map(|t| t.deadline))
            .min()
    }

    pub fn on_key(&mut self, time: Nanos, key: KeyCode, down: bool, out: &mut Vec<KeyEvent>) {
        let start = out.len();
        let st = self.keys.entry(key).or_default();
        st.physical = down;

        match st.timer {
            Some(Timer {
                kind: TimerKind::Lockout,
                ..
            }) => {}
            Some(Timer {
                kind: TimerKind::Settle,
                ..
            }) => {
                if st.physical == st.logical {
                    // went back to matching downstream again before the deadline
                    st.timer = None;
                }
            }
            None => {
                if st.physical != st.logical {
                    Debouncer::begin_change(self.cfg, st, key, time, out);
                }
            }
        }

        self.settle_output(out, start);
    }

    pub fn on_timeout(&mut self, now: Nanos, out: &mut Vec<KeyEvent>) {
        let start = out.len();

        // if a lockout ends and the level is now changed, it emits and re-arms, so one pass isn't
        // always enough when the caller is late; the re-armed lockout can only expire onto an
        // unchanged level, since the physical level can't change without input, so two passes is
        let mut passes = 0;
        loop {
            let mut progress = false;
            for (&key, st) in &mut self.keys {
                let Some(timer) = st.timer else {
                    continue;
                };
                if timer.deadline > now {
                    continue;
                }
                st.timer = None;
                progress = true;
                match timer.kind {
                    TimerKind::Lockout => {
                        if st.physical != st.logical {
                            Debouncer::begin_change(self.cfg, st, key, timer.deadline, out);
                        }
                    }
                    TimerKind::Settle => {
                        debug_assert_ne!(st.physical, st.logical);
                        st.logical = st.physical;
                        out.push(KeyEvent {
                            time: timer.deadline,
                            key,
                            down: st.logical,
                        });
                    }
                }
            }
            if !progress {
                break;
            }
            passes += 1;
            debug_assert!(passes <= 2);
        }

        self.settle_output(out, start);
    }

    pub fn release_all(&mut self, now: Nanos, out: &mut Vec<KeyEvent>) {
        let start = out.len();

        for (&key, st) in &mut self.keys {
            st.timer = None;
            st.physical = false;
            if st.logical {
                st.logical = false;
                out.push(KeyEvent {
                    time: now,
                    key,
                    down: false,
                });
            }
        }

        self.settle_output(out, start);
    }

    fn settle_output(&mut self, out: &mut [KeyEvent], start: usize) {
        let tail = &mut out[start..];
        if tail.len() > 1 {
            tail.sort_by_key(|e| (e.time, e.key));
        }
        for ev in tail {
            ev.time = ev.time.max(self.last_emit);
            self.last_emit = ev.time;
        }
    }

    // not a method because it's problematic with the borrow checker, as `st` is usually a mutable
    // reference into this same struct
    fn begin_change(
        cfg: Config,
        st: &mut KeyState,
        key: KeyCode,
        time: Nanos,
        out: &mut Vec<KeyEvent>,
    ) {
        debug_assert_ne!(st.physical, st.logical);
        match cfg.mode.policy(st.physical) {
            Policy::Emit => {
                st.logical = st.physical;
                out.push(KeyEvent {
                    time,
                    key,
                    down: st.logical,
                });
                st.timer = cfg.mode.lockout().then(|| Timer {
                    deadline: time + cfg.window,
                    kind: TimerKind::Lockout,
                });
            }
            Policy::Settle => {
                st.timer = Some(Timer {
                    deadline: time + cfg.window,
                    kind: TimerKind::Settle,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{Sim, ms, run};

    use std::time::Duration;

    fn trace(events: &[KeyEvent]) -> Vec<(u64, KeyCode, bool)> {
        events
            .iter()
            .map(|e| {
                assert_eq!(e.time % ms(1), 0, "test times are all whole milliseconds");
                (e.time / ms(1), e.key, e.down)
            })
            .collect()
    }

    fn input(spec: &[(u64, KeyCode, bool)]) -> Vec<(Nanos, KeyCode, bool)> {
        spec.iter().map(|&(t, k, d)| (ms(t), k, d)).collect()
    }

    const A: KeyCode = 30; // KEY_A
    const B: KeyCode = 48; // KEY_B
    const W: Nanos = ms(20);
    const MODES: [Mode; 3] = [Mode::Eager, Mode::Defer, Mode::Mixed];

    #[test]
    fn eager_passes_clean_input_through() {
        let src = input(&[
            (0, A, true),
            (100, A, false),
            (200, A, true),
            (300, A, false),
        ]);
        let out = run(Mode::Eager, W, &src);
        assert_eq!(
            trace(&out),
            [
                (0, A, true),
                (100, A, false),
                (200, A, true),
                (300, A, false)
            ]
        );
    }

    #[test]
    fn eager_reports_the_press_once_and_swallows_tail_chatter() {
        let src = input(&[
            (0, A, true),
            (2, A, false),
            (4, A, true),
            (6, A, false),
            (8, A, true),
        ]);
        let out = run(Mode::Eager, W, &src);
        assert_eq!(trace(&out), [(0, A, true)]);
    }

    #[test]
    fn eager_resamples_when_lockout_expires() {
        let src = input(&[(0, A, true), (2, A, false)]);
        let out = run(Mode::Eager, W, &src);
        assert_eq!(trace(&out), [(0, A, true), (20, A, false)]);
    }

    #[test]
    fn eager_rearms_the_lockout_after_a_resampled_edge() {
        let src = input(&[(0, A, true), (10, A, false), (25, A, true)]);
        let out = run(Mode::Eager, W, &src);
        assert_eq!(trace(&out), [(0, A, true), (20, A, false), (40, A, true)]);
    }

    #[test]
    fn defer_delays_every_edge_by_one_window() {
        let src = input(&[(0, A, true), (50, A, false)]);
        let out = run(Mode::Defer, W, &src);
        assert_eq!(trace(&out), [(20, A, true), (70, A, false)]);
    }

    #[test]
    fn defer_swallows_a_press_shorter_than_the_window() {
        let src = input(&[(0, A, true), (5, A, false)]);
        let out = run(Mode::Defer, W, &src);
        assert_eq!(trace(&out), []);
    }

    #[test]
    fn defer_withdraws_a_pending_edge_on_a_bounce() {
        let src = input(&[(0, A, true), (5, A, false), (10, A, true), (60, A, false)]);
        let out = run(Mode::Defer, W, &src);
        assert_eq!(trace(&out), [(30, A, true), (80, A, false)]);
    }

    #[test]
    fn mixed_reports_the_press_and_defers_the_release() {
        let src = input(&[(0, A, true), (50, A, false)]);
        let out = run(Mode::Mixed, W, &src);
        assert_eq!(trace(&out), [(0, A, true), (70, A, false)]);
    }

    #[test]
    fn mixed_swallows_release_chatter_into_the_pending_release() {
        let src = input(&[(0, A, true), (50, A, false), (53, A, true), (56, A, false)]);
        let out = run(Mode::Mixed, W, &src);
        assert_eq!(trace(&out), [(0, A, true), (76, A, false)]);
    }

    #[test]
    fn mixed_swallows_press_chatter_without_a_lockout() {
        let src = input(&[(0, A, true), (2, A, false), (4, A, true), (6, A, false)]);
        let out = run(Mode::Mixed, W, &src);
        assert_eq!(trace(&out), [(0, A, true), (26, A, false)]);
    }

    #[test]
    fn a_deferred_release_can_be_emitted_same_instant_as_the_next_press_under_mixed() {
        let src = input(&[(0, A, true), (0, A, false), (20, A, true)]);
        let out = run(Mode::Mixed, W, &src);
        assert_eq!(trace(&out), [(0, A, true), (20, A, false), (20, A, true)]);
    }

    #[test]
    fn keys_are_independent() {
        let src = input(&[
            (0, A, true),
            (1, B, true),
            (2, A, false),
            (3, A, true),
            (60, B, false),
            (61, A, false),
        ]);
        for mode in MODES {
            let out = run(mode, W, &src);
            for key in [A, B] {
                let only: Vec<&KeyEvent> = out.iter().filter(|e| e.key == key).collect();
                assert!(
                    only.windows(2).all(|w| w[0].down != w[1].down),
                    "{mode:?} key {key}: levels must alternate, got {only:?}"
                );
                assert_eq!(
                    only.last().map(|e| e.down),
                    Some(false),
                    "{mode:?} key {key}"
                );
            }
        }
    }

    #[test]
    fn a_zero_window_is_a_passthrough() {
        let src = input(&[(0, A, true), (0, B, true), (1, A, false), (100, B, false)]);
        for mode in MODES {
            let out = run(mode, 0, &src);
            assert_eq!(
                trace(&out),
                [(0, A, true), (0, B, true), (1, A, false), (100, B, false)],
                "{mode:?}"
            );
        }
    }

    #[test]
    fn release_all_releases_only_what_downstream_is_holding() {
        for mode in MODES {
            let mut sim = Sim::new(mode, W);
            // B goes up and its release is fully flushed; A is still held on device disconnect
            sim.feed(ms(0), A, true)
                .feed(ms(1), B, true)
                .idle_until(ms(50))
                .feed(ms(51), B, false)
                .idle_until(ms(89))
                .release_all(ms(90));

            let held_at_disconnect: Vec<(u16, bool)> = sim
                .events()
                .iter()
                .filter(|e| e.time == ms(90))
                .map(|e| (e.key, e.down))
                .collect();
            assert_eq!(held_at_disconnect, [(A, false)], "{mode:?}");
            assert!(sim.debouncer().next_deadline().is_none(), "{mode:?}");
            assert!(!sim.debouncer().is_down(A), "{mode:?}");
        }
    }

    #[test]
    fn is_down_tracks_downstream() {
        let mut sim = Sim::new(Mode::Defer, W);
        sim.feed(ms(0), A, true);
        assert!(!sim.debouncer().is_down(A));
        sim.idle_until(ms(20));
        assert!(sim.debouncer().is_down(A));
    }

    #[test]
    fn a_late_caller_still_converges_in_one_call() {
        let mut deb = Debouncer::new(Mode::Eager, Duration::from_nanos(W));
        let mut out = Vec::new();
        deb.on_key(ms(0), A, true, &mut out);
        deb.on_key(ms(10), A, false, &mut out);
        deb.on_timeout(ms(1000), &mut out);
        assert_eq!(trace(&out), [(0, A, true), (20, A, false)]);
        assert!(deb.next_deadline().is_none());
    }

    #[test]
    fn output_timestamps_are_monotonic() {
        // out of order input isn't supposed to ever even happen, but a timer wakeup that wins
        // against a wakeup for an older input would produce it, and it must not leak into the
        // output
        let mut deb = Debouncer::new(Mode::Eager, Duration::from_nanos(W));
        let mut out = Vec::new();
        deb.on_key(ms(100), A, true, &mut out);
        deb.on_key(ms(50), B, true, &mut out);
        assert_eq!(trace(&out), [(100, A, true), (100, B, true)]);
    }

    #[test]
    fn simultaneous_deadlines_come_out_in_key_order() {
        let src = input(&[(0, B, true), (0, A, true), (5, B, false), (5, A, false)]);
        let out = run(Mode::Defer, W, &src);
        assert_eq!(trace(&out), []);

        let src = input(&[(0, B, true), (0, A, true)]);
        let out = run(Mode::Defer, W, &src);
        assert_eq!(trace(&out), [(20, A, true), (20, B, true)]);
    }
}
