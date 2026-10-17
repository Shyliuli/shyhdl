//! Public behaviour of the replay engine: reference points, stepping, watch
//! hits and cursor semantics (REQ-DBG-020 to REQ-DBG-039, AC-MVP-027 to
//! AC-MVP-036).
//!
//! Fixtures are hand written VCD files under `tests/fixtures/replay/`; they
//! pin down the algorithms and are not presented as simulator output.

use std::path::PathBuf;

use shyhdl::replay::{Cursor, Direction, Edge, ReplayEngine, StepMode, StopReason};
use shyhdl::waveform::{BitVec, Value, WaveError, Waveform};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay")
        .join(name)
}

fn algo() -> Waveform {
    Waveform::load(fixture("algo.vcd")).expect("algo.vcd must load")
}

fn sameslot() -> Waveform {
    Waveform::load(fixture("sameslot.vcd")).expect("sameslot.vcd must load")
}

fn interval(origin: u64, period: u64) -> ReplayEngine {
    ReplayEngine::new(algo(), StepMode::Interval { origin, period })
        .expect("interval mode must be valid")
}

fn clock(path: &str, edge: Edge) -> ReplayEngine {
    clock_in(algo(), path, edge)
}

fn clock_in(wave: Waveform, path: &str, edge: Edge) -> ReplayEngine {
    ReplayEngine::new(
        wave,
        StepMode::Clock {
            signal: path.to_string(),
            edge,
        },
    )
    .expect("clock mode must be valid")
}

fn bits(msb_first: &str) -> Value {
    Value::Bits(BitVec::from_msb_string(msb_first))
}

/// Times reported by a watch stop, or a panic when it was not one.
fn watch_times(outcome: &shyhdl::replay::StepOutcome) -> Vec<u64> {
    match &outcome.reason {
        StopReason::Watch(hits) => hits.iter().map(|hit| hit.time).collect(),
        other => panic!("expected a watch hit, got {other:?}"),
    }
}

fn watch_paths(outcome: &shyhdl::replay::StepOutcome) -> Vec<String> {
    match &outcome.reason {
        StopReason::Watch(hits) => hits.iter().map(|hit| hit.path.clone()).collect(),
        other => panic!("expected a watch hit, got {other:?}"),
    }
}

#[test]
fn interval_mode_steps_on_the_grid_without_drift() {
    let mut engine = interval(0, 10);
    assert_eq!(
        engine.cursor(),
        Cursor {
            time: 0,
            index: Some(0)
        },
        "the cursor starts at the beginning of the record"
    );
    assert_eq!(engine.reference_bounds(), Some((0, 50)));

    // AC-MVP-030: from 13 step to 20, reverse-step to 10, no drift.
    assert_eq!(engine.seek(13).time, 13);
    let forward = engine.step(Direction::Forward);
    assert_eq!(forward.cursor.time, 20);
    assert_eq!(forward.reason, StopReason::ReferencePoint);

    let backward = engine.step(Direction::Backward);
    assert_eq!(backward.cursor.time, 10);
    assert_eq!(backward.reason, StopReason::ReferencePoint);

    assert_eq!(engine.step(Direction::Forward).cursor.time, 20);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 10);
}

#[test]
fn interval_grid_is_origin_plus_n_times_period() {
    let mut engine = interval(5, 7);
    // Grid: 5, 12, 19, 26, 33, 40, 47 inside [0, 50].
    assert_eq!(engine.reference_bounds(), Some((5, 47)));
    assert_eq!(engine.cursor().time, 0);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 5);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 12);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 19);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 12);

    // A grid point with no recorded sample still reads the held value, and
    // says which observation point it came from.
    assert_eq!(engine.cursor().time, 12);
    assert_eq!(engine.cursor().index, Some(2), "time 10 is observation 2");
}

#[test]
fn watch_sees_a_rise_between_two_equal_samples() {
    let mut engine = interval(0, 10);
    let d = engine.waveform().signal("top.d").unwrap();
    engine.watch(&[d]).unwrap();

    // AC-MVP-029: the samples at 10 and at 20 are both `0`.
    assert_eq!(engine.waveform().value_at(d, 10).unwrap(), Some(bits("0")));
    assert_eq!(engine.waveform().value_at(d, 20).unwrap(), Some(bits("0")));

    // AC-MVP-027: the change at 13 still hits.
    let first = engine.continue_run(Direction::Forward);
    assert_eq!(watch_times(&first), vec![13]);
    assert_eq!(first.cursor.time, 13);
    assert_eq!(first.cursor.index, Some(3));
    match &first.reason {
        StopReason::Watch(hits) => {
            assert_eq!(hits[0].previous, Some(bits("0")));
            assert_eq!(hits[0].value, bits("1"));
            assert_eq!(hits[0].path, "top.d");
        }
        other => panic!("expected a watch hit, got {other:?}"),
    }

    // AC-MVP-028: the fall at 14 hits too.
    let second = engine.continue_run(Direction::Forward);
    assert_eq!(watch_times(&second), vec![14]);
    match &second.reason {
        StopReason::Watch(hits) => {
            assert_eq!(hits[0].previous, Some(bits("1")));
            assert_eq!(hits[0].value, bits("0"));
        }
        other => panic!("expected a watch hit, got {other:?}"),
    }

    // Nothing left: the record ends, which is not an error (REQ-DBG-012).
    let third = engine.continue_run(Direction::Forward);
    assert_eq!(third.reason, StopReason::End);
    assert_eq!(third.cursor.time, 50);
    assert_eq!(third.cursor.index, Some(11));
}

#[test]
fn continue_never_reports_a_change_at_the_cursor_itself() {
    let mut engine = interval(0, 10);
    let d = engine.waveform().signal("top.d").unwrap();
    let data = engine.waveform().signal("top.data").unwrap();

    engine.watch(&[d]).unwrap();
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, 13);
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, 14);
    assert_eq!(engine.cursor().time, 14, "the cursor sits on the 14 change");

    // Sign up for a watch while sitting on 14: the change at 14 has already
    // happened and must not be reported again.
    engine.clear_watch();
    engine.watch(&[d, data]).unwrap();
    let next = engine.continue_run(Direction::Forward);
    assert_eq!(
        next.cursor.time, 20,
        "the search is strictly ahead of the cursor"
    );
    assert_eq!(watch_paths(&next), vec!["top.data"]);
}

#[test]
fn step_does_not_jump_over_an_earlier_watch_hit() {
    let mut engine = interval(0, 10);
    let d = engine.waveform().signal("top.d").unwrap();
    engine.watch(&[d]).unwrap();

    // REQ-DBG-034: the next reference point is 20, but d changes at 14.
    engine.seek(13);
    let interrupted = engine.step(Direction::Forward);
    assert_eq!(interrupted.cursor.time, 14);
    assert!(interrupted.is_watch_hit());

    // From there the step resumes reference stepping.
    let resumed = engine.step(Direction::Forward);
    assert_eq!(resumed.cursor.time, 20);
    assert_eq!(resumed.reason, StopReason::ReferencePoint);
}

#[test]
fn reverse_step_goes_to_the_previous_reference_point_only() {
    let mut engine = interval(0, 10);
    let d = engine.waveform().signal("top.d").unwrap();
    engine.watch(&[d]).unwrap();

    // REQ-DBG-031: reverse-step is the previous grid point, not the previous
    // value change, even with a watch registered.
    engine.seek(13);
    let backward = engine.step(Direction::Backward);
    assert_eq!(backward.cursor.time, 10);
    assert_eq!(backward.reason, StopReason::ReferencePoint);

    // At the start of the record there is no earlier reference point.
    engine.seek(0);
    let start = engine.step(Direction::Backward);
    assert_eq!(start.reason, StopReason::Start);
    assert_eq!(start.cursor.time, 0);
}

#[test]
fn clock_mode_uses_the_edges_that_were_actually_recorded() {
    let mut engine = clock("top.clk", Edge::Pos);
    assert_eq!(engine.clock(), Some(("top.clk", Edge::Pos)));
    assert_eq!(engine.reference_bounds(), Some((5, 50)));
    assert_eq!(engine.cursor().time, 0);

    // clk: 0@0 1@5 0@10 1@20 0@25 1@30 0@35 1@40 0@45 1@50
    assert_eq!(engine.step(Direction::Forward).cursor.time, 5);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 20);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 30);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 20);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 5);
    assert_eq!(engine.step(Direction::Backward).reason, StopReason::Start);

    let mut falling = clock("top.clk", Edge::Neg);
    assert_eq!(falling.reference_bounds(), Some((10, 45)));
    assert_eq!(falling.step(Direction::Forward).cursor.time, 10);
    assert_eq!(falling.step(Direction::Forward).cursor.time, 25);
    assert_eq!(falling.step(Direction::Forward).cursor.time, 35);
    assert_eq!(falling.step(Direction::Forward).cursor.time, 45);
    let done = falling.step(Direction::Forward);
    assert_eq!(done.reason, StopReason::End);

    let both = clock("top.clk", Edge::Both);
    assert_eq!(both.reference_bounds(), Some((5, 50)));
}

#[test]
fn a_gated_clock_advances_to_the_end_of_the_record() {
    // gclk: 0@0 1@5 0@10 1@20 0@25, then it never rises again.
    let mut engine = clock("top.gclk", Edge::Pos);
    assert_eq!(engine.reference_bounds(), Some((5, 20)));
    assert_eq!(engine.step(Direction::Forward).cursor.time, 5);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 20);

    let done = engine.step(Direction::Forward);
    assert_eq!(done.reason, StopReason::End, "REQ-DBG-024");
    assert_eq!(done.cursor.time, 50);
}

#[test]
fn four_state_edges_are_not_silently_dropped() {
    // tri is 0@0, x@10, 1@20: both transitions are rising edges in Verilog.
    let rising = clock_in(sameslot(), "top.tri", Edge::Pos);
    assert_eq!(rising.reference_bounds(), Some((10, 20)));

    // ... and the second one is not a falling edge either.
    let mut falling = clock_in(sameslot(), "top.tri", Edge::Neg);
    assert_eq!(falling.reference_bounds(), None);
    assert_eq!(falling.step(Direction::Forward).reason, StopReason::End);

    let both = clock_in(sameslot(), "top.tri", Edge::Both);
    assert_eq!(both.reference_bounds(), Some((10, 20)));
}

#[test]
fn a_first_recorded_value_is_a_level_not_an_edge() {
    // xbit is x@0 then 1@30. The initial x is the starting level.
    let engine = clock("top.xbit", Edge::Pos);
    assert_eq!(engine.reference_bounds(), Some((30, 30)));
}

#[test]
fn step_modes_are_never_guessed() {
    match ReplayEngine::new(
        algo(),
        StepMode::Clock {
            signal: "top.missing".to_string(),
            edge: Edge::Pos,
        },
    ) {
        Err(WaveError::SignalNotFound { path }) => assert_eq!(path, "top.missing"),
        other => panic!("expected SignalNotFound, got {other:?}"),
    }

    match ReplayEngine::new(
        algo(),
        StepMode::Clock {
            signal: "top.data".to_string(),
            edge: Edge::Pos,
        },
    ) {
        Err(WaveError::WidthMismatch { width, .. }) => assert_eq!(width, 8),
        other => panic!("expected WidthMismatch, got {other:?}"),
    }

    match ReplayEngine::new(
        algo(),
        StepMode::Interval {
            origin: 0,
            period: 0,
        },
    ) {
        Err(WaveError::InvalidMode { .. }) => {}
        other => panic!("expected InvalidMode, got {other:?}"),
    }
}

#[test]
fn values_are_read_from_the_cursor_observation_point() {
    let mut engine = interval(0, 10);
    let clk = engine.waveform().signal("top.clk").unwrap();
    let data = engine.waveform().signal("top.data").unwrap();

    engine.seek_index(3).expect("index 3 is recorded");
    assert_eq!(
        engine.cursor(),
        Cursor {
            time: 13,
            index: Some(3)
        }
    );
    assert_eq!(engine.value(clk).unwrap(), Some(bits("0")));
    assert_eq!(
        engine.snapshot(&[clk, data]).unwrap(),
        vec![(clk, Some(bits("0"))), (data, Some(bits("00000010")))]
    );

    // AC-MVP-023: after a move the values come from the new observation point.
    engine.seek(20);
    assert_eq!(engine.value(data).unwrap(), Some(bits("00000011")));
    engine.step(Direction::Backward);
    assert_eq!(engine.cursor().time, 10);
    assert_eq!(engine.value(data).unwrap(), Some(bits("00000001")));
    engine.step(Direction::Forward);
    assert_eq!(engine.cursor().time, 20);
    assert_eq!(engine.value(data).unwrap(), Some(bits("00000011")));
}

#[test]
fn an_out_of_range_observation_point_is_reported() {
    let mut engine = interval(0, 10);
    engine.seek(13);
    match engine.seek_index(99) {
        Err(WaveError::ObservationOutOfRange { index, count }) => {
            assert_eq!(index, 99);
            assert_eq!(count, 12);
        }
        other => panic!("expected ObservationOutOfRange, got {other:?}"),
    }
    assert_eq!(
        engine.cursor().time,
        13,
        "a rejected seek leaves the cursor alone"
    );
}

#[test]
fn repeated_moves_round_trip_exactly() {
    let mut engine = interval(0, 10);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 10);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 20);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 10);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 0);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 10);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 20);
    assert_eq!(engine.step(Direction::Backward).cursor.time, 10);
    assert_eq!(engine.reset().time, 0);
    assert_eq!(engine.step(Direction::Forward).cursor.time, 10);
}

#[test]
fn changes_at_one_timestamp_are_reported_as_one_snapshot() {
    let mut engine = ReplayEngine::new(
        sameslot(),
        StepMode::Interval {
            origin: 0,
            period: 10,
        },
    )
    .unwrap();
    let d = engine.waveform().signal("top.d").unwrap();
    let bus = engine.waveform().signal("top.bus").unwrap();
    let tri = engine.waveform().signal("top.tri").unwrap();
    engine.watch(&[d, bus, tri]).unwrap();

    // At 10, bus changes and tri goes 0 -> x; `d` goes 0 -> 1 -> 0 and is not
    // reported, because the timestamp ends where it started.
    let first = engine.continue_run(Direction::Forward);
    assert_eq!(watch_times(&first), vec![10, 10]);
    assert_eq!(watch_paths(&first), vec!["top.bus", "top.tri"]);
    match &first.reason {
        StopReason::Watch(hits) => {
            assert_eq!(hits[1].value, bits("x"), "x is a value, not a zero");
            assert_eq!(hits[1].previous, Some(bits("0")));
        }
        other => panic!("expected a watch hit, got {other:?}"),
    }

    // At 20 all three change.
    let second = engine.continue_run(Direction::Forward);
    assert_eq!(watch_times(&second), vec![20, 20, 20]);
    assert_eq!(watch_paths(&second), vec!["top.d", "top.bus", "top.tri"]);

    // At 30 every signal is re-dumped with its current value: no change.
    let third = engine.continue_run(Direction::Forward);
    assert_eq!(third.reason, StopReason::End);
    assert_eq!(third.cursor.time, 30);
}

#[test]
fn reverse_continue_walks_back_through_recorded_hits() {
    let mut engine = interval(0, 10);
    let d = engine.waveform().signal("top.d").unwrap();
    engine.watch(&[d]).unwrap();

    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, 13);
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, 14);
    assert_eq!(
        engine.continue_run(Direction::Forward).reason,
        StopReason::End
    );

    let back = engine.continue_run(Direction::Backward);
    assert_eq!(back.cursor.time, 14);
    assert_eq!(watch_times(&back), vec![14]);
    let back = engine.continue_run(Direction::Backward);
    assert_eq!(back.cursor.time, 13);
    // The value recorded first has no predecessor, and says so.
    let back = engine.continue_run(Direction::Backward);
    assert_eq!(back.cursor.time, 0);
    match &back.reason {
        StopReason::Watch(hits) => {
            assert_eq!(
                hits[0].previous, None,
                "the first recorded value has no before"
            );
            assert_eq!(hits[0].value, bits("0"));
        }
        other => panic!("expected a watch hit, got {other:?}"),
    }
    let back = engine.continue_run(Direction::Backward);
    assert_eq!(back.reason, StopReason::Start);
    assert_eq!(back.cursor.time, 0);

    // Forward again: the same two hits, in the same order.
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, 13);
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, 14);
}

#[test]
fn watching_several_signals_reports_the_earliest_timestamp_first() {
    let mut engine = interval(0, 10);
    let clk = engine.waveform().signal("top.clk").unwrap();
    let carry = engine.waveform().signal("top.cpu.alu.carry").unwrap();
    engine.watch(&[clk, carry]).unwrap();

    // clk changes at 5, carry at 5 as well: one timestamp, two signals.
    let first = engine.continue_run(Direction::Forward);
    assert_eq!(first.cursor.time, 5);
    assert_eq!(watch_paths(&first), vec!["top.clk", "top.cpu.alu.carry"]);
}
