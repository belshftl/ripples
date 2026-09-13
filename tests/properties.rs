// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Property tests for the guarantees on the debounce state machine.

use proptest::prelude::*;
use ripples::debounce::{KeyCode, KeyEvent, Mode, Nanos};
use ripples::sim::{ms, run};

const KEYS: [KeyCode; 3] = [30, 48, 57]; // KEY_A, KEY_B, KEY_SPACE
const MODES: [Mode; 3] = [Mode::Eager, Mode::Defer, Mode::Mixed];

#[derive(Debug, Clone)]
struct Trace {
    window: Nanos,
    events: Vec<(Nanos, KeyCode, bool)>,
}

fn make_trace(window_ms: u64, steps: Vec<(usize, u64)>) -> Trace {
    let mut now = 0u64;
    let mut level = [false; KEYS.len()];
    let mut events = Vec::with_capacity(steps.len());
    for (idx, gap) in steps {
        now += gap;
        level[idx] = !level[idx];
        events.push((ms(now), KEYS[idx], level[idx]));
    }
    Trace {
        window: ms(window_ms),
        events,
    }
}

fn arbitrary() -> impl Strategy<Value = Trace> {
    (
        0u64..=40u64,
        prop::collection::vec((0usize..KEYS.len(), 0u64..=120u64), 0..40),
    )
        .prop_map(|(window_ms, steps)| make_trace(window_ms, steps))
}

/// Traffic where every edge is at least one window from the last, so no debouncing is needed.
fn clean() -> impl Strategy<Value = Trace> {
    (1u64..=40u64)
        .prop_flat_map(|window_ms| {
            let gap = window_ms..=(3 * window_ms);
            (
                Just(window_ms),
                prop::collection::vec((0usize..KEYS.len(), gap), 0..30),
            )
        })
        .prop_map(|(window_ms, steps)| make_trace(window_ms, steps))
}

fn level_after(events: &[(Nanos, KeyCode, bool)], key: KeyCode) -> bool {
    events
        .iter()
        .rev()
        .find(|&&(_, k, _)| k == key)
        .is_some_and(|&(_, _, down)| down)
}

fn out_level_after(events: &[KeyEvent], key: KeyCode) -> bool {
    events
        .iter()
        .rev()
        .find(|e| e.key == key)
        .is_some_and(|e| e.down)
}

proptest! {
    // guarantee 1: parity
    #[test]
    fn parity_is_consistent_per_key(trace in arbitrary()) {
        for mode in MODES {
            let out = run(mode, trace.window, &trace.events);
            for key in KEYS {
                let mut expect_down = true;
                for ev in out.iter().filter(|e| e.key == key) {
                    prop_assert_eq!(
                        ev.down, expect_down,
                        "{:?} key {}: parity violation at {:?} in {:?}", mode, key, ev, out
                    );
                    expect_down = !expect_down;
                }
            }
        }
    }

    // guarantee 2: (weak) monotonicity
    #[test]
    fn output_timestamps_are_monotonic(trace in arbitrary()) {
        for mode in MODES {
            let out = run(mode, trace.window, &trace.events);
            for pair in out.windows(2) {
                prop_assert!(
                    pair[0].time <= pair[1].time,
                    "{:?}: {:?} then {:?}", mode, pair[0], pair[1]
                );
            }
        }
    }

    // guarantee 3: consistency
    #[test]
    fn outputs_are_consistent_with_inputs(trace in arbitrary()) {
        for mode in MODES {
            let out = run(mode, trace.window, &trace.events);
            for ev in &out {
                let backed = trace.events.iter().any(|&(t, key, down)| {
                    key == ev.key && down == ev.down && ev.time - t <= trace.window && t <= ev.time
                });
                prop_assert!(
                    backed,
                    "{:?}: {:?} has no input within {}ns before it, in {:?}",
                    mode, ev, trace.window, trace.events
                );
            }
            prop_assert!(out.len() <= trace.events.len(), "{:?}: made up events without corresponding input", mode);
        }
    }

    // guarantee 4: convergence
    #[test]
    fn converges_with_downstream_state_when_quiescent(trace in arbitrary()) {
        for mode in MODES {
            let out = run(mode, trace.window, &trace.events);
            for key in KEYS {
                prop_assert_eq!(
                    out_level_after(&out, key),
                    level_after(&trace.events, key),
                    "{:?} key {}: downstream didn't converge with device state", mode, key
                );
            }
        }
    }

    // guarantee 5 pt 1: suppression
    #[test]
    fn presses_are_a_window_apart(trace in arbitrary()) {
        prop_assume!(trace.window > 0);
        for mode in MODES {
            let out = run(mode, trace.window, &trace.events);
            for key in KEYS {
                let times: Vec<Nanos> = out
                    .iter()
                    .filter(|e| e.key == key && e.down)
                    .map(|e| e.time)
                    .collect();
                for pair in times.windows(2) {
                    prop_assert!(
                        pair[1] - pair[0] >= trace.window,
                        "{:?} key {}: presses {}ns apart, window is {}ns",
                        mode, key, pair[1] - pair[0], trace.window
                    );
                }
            }
        }
    }

    // guarantee 5 pt 2: eager/defer must apply the guarantee to all edges
    #[test]
    fn all_edges_are_a_window_apart_if_both_edges_are_filtered(trace in arbitrary()) {
        prop_assume!(trace.window > 0);
        for mode in [Mode::Eager, Mode::Defer] {
            let out = run(mode, trace.window, &trace.events);
            for key in KEYS {
                let times: Vec<Nanos> = out.iter().filter(|e| e.key == key).map(|e| e.time).collect();
                for pair in times.windows(2) {
                    prop_assert!(
                        pair[1] - pair[0] >= trace.window,
                        "{:?} key {}: edges {}ns apart, window is {}ns",
                        mode, key, pair[1] - pair[0], trace.window
                    );
                }
            }
        }
    }

    // guarantee 6 pt 1: cleanliness
    #[test]
    fn clean_input_passes_through(trace in clean()) {
        for mode in MODES {
            let out = run(mode, trace.window, &trace.events);
            let expected: Vec<(Nanos, KeyCode, bool)> = trace
                .events
                .iter()
                .map(|&(t, key, down)| {
                    let delay = match (mode, down) {
                        (Mode::Eager, _) | (Mode::Mixed, true) => 0,
                        (Mode::Defer, _) | (Mode::Mixed, false) => trace.window,
                    };
                    (t + delay, key, down)
                })
                .collect();
            let got: Vec<(Nanos, KeyCode, bool)> =
                out.iter().map(|e| (e.time, e.key, e.down)).collect();
            prop_assert_eq!(got, expected, "{:?}", mode);
        }
    }

    // guarantee 6 pt 2: eager must be idempotent
    #[test]
    fn eager_is_idempotent(trace in arbitrary()) {
        let once = run(Mode::Eager, trace.window, &trace.events);
        let relayed: Vec<(Nanos, KeyCode, bool)> =
            once.iter().map(|e| (e.time, e.key, e.down)).collect();
        let twice = run(Mode::Eager, trace.window, &relayed);
        prop_assert_eq!(twice, once);
        // no point in running thrice; if twice == once, it'd by definition yield the same result
    }
}
