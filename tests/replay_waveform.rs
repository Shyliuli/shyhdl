//! Public behaviour of the read-only waveform layer (REQ-MVP-009,
//! REQ-DBG-003, REQ-DBG-005, REQ-DBG-036, AC-MVP-033).
//!
//! The fixtures under `tests/fixtures/replay/` are hand written VCD files.
//! They are not Verilator output and do not claim to be: they exist to pin
//! down the replay algorithms, and are cross-checked against
//! tool-generated waveforms separately.

use std::path::PathBuf;

use shyhdl::waveform::{
    BitState, BitVec, ScopeKind, SignalKind, TimeUnit, Timescale, Value, WaveError, Waveform,
};

/// Recorded observation points of `algo.vcd`, in order.
const ALGO_TIMES: [u64; 12] = [0, 5, 10, 13, 14, 20, 25, 30, 35, 40, 45, 50];

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

fn bits(msb_first: &str) -> Value {
    Value::Bits(BitVec::from_msb_string(msb_first))
}

fn value_of(wave: &Waveform, path: &str, time: u64) -> Value {
    maybe_value_of(wave, path, time)
        .unwrap_or_else(|| panic!("{path} has no recorded value at {time}"))
}

fn maybe_value_of(wave: &Waveform, path: &str, time: u64) -> Option<Value> {
    let id = wave.signal(path).expect("signal must be recorded");
    wave.value_at(id, time).expect("value query must succeed")
}

fn exact_value_of(wave: &Waveform, path: &str, time: u64) -> Option<Value> {
    let id = wave.signal(path).expect("signal must be recorded");
    wave.exact_at(id, time).expect("exact query must succeed")
}

#[test]
fn hierarchy_timescale_and_width_are_reported() {
    let wave = algo();
    assert_eq!(wave.timescale(), Some(Timescale::new(1, TimeUnit::Nano)));
    assert_eq!(wave.timestamps(), ALGO_TIMES);
    assert_eq!(wave.observation_count(), ALGO_TIMES.len());
    assert_eq!(wave.first_time(), 0);
    assert_eq!(wave.last_time(), 50);
    assert_eq!(wave.observation_index(13), Some(3));
    assert_eq!(
        wave.observation_index(12),
        Some(2),
        "last point at or before"
    );
    assert_eq!(wave.observation_time(3), Some(13));

    let scopes: Vec<&str> = wave.scopes().iter().map(|s| s.path.as_str()).collect();
    assert!(scopes.contains(&"top"), "root scope: {scopes:?}");
    assert!(scopes.contains(&"top.cpu"), "nested scope: {scopes:?}");
    assert!(scopes.contains(&"top.cpu.alu"), "nested scope: {scopes:?}");

    let alu = wave
        .scopes()
        .iter()
        .find(|s| s.path == "top.cpu.alu")
        .expect("alu scope");
    assert_eq!(alu.name, "alu");
    assert_eq!(alu.parent.as_deref(), Some("top.cpu"));
    assert_eq!(alu.kind, ScopeKind::Module);

    let root = wave.scopes().iter().find(|s| s.path == "top").unwrap();
    assert_eq!(root.parent, None);

    // A signal inside a nested instance keeps its full path and its width.
    let carry = wave.signal("top.cpu.alu.carry").unwrap();
    assert_eq!(wave.path_of(carry).unwrap(), "top.cpu.alu.carry");
    assert_eq!(wave.width(carry).unwrap(), 1);
    assert_eq!(wave.signal_info(carry).unwrap().kind, SignalKind::Bit);

    let busy = wave.signal("top.cpu.busy").unwrap();
    assert_eq!(wave.width(busy).unwrap(), 1);

    let data = wave.signal("top.data").unwrap();
    assert_eq!(wave.width(data).unwrap(), 8);
    assert_eq!(wave.signal_info(data).unwrap().kind, SignalKind::Vector);

    let wide = wave.signal("top.wide").unwrap();
    assert_eq!(
        wave.width(wide).unwrap(),
        72,
        "widths are not limited to 64"
    );
}

#[test]
fn alias_names_resolve_to_the_same_recorded_signal() {
    let wave = algo();
    let data = wave.signal("top.data").unwrap();
    let alias = wave.signal("top.data_alias").unwrap();
    assert_eq!(data, alias, "a VCD alias is one signal with two names");

    // Both spellings stay visible, and the alias group is reported once.
    let declared: Vec<&str> = wave
        .signals()
        .iter()
        .map(|s| s.path.as_str())
        .filter(|path| path.starts_with("top.data"))
        .collect();
    assert_eq!(declared, vec!["top.data", "top.data_alias"]);
    assert_eq!(wave.path_of(alias).unwrap(), "top.data");
    assert_eq!(wave.find_signals("top.data_alias").len(), 1);
    assert_eq!(
        wave.find_signals("data").len(),
        1,
        "an alias group is not two candidates"
    );

    // Both names read the same recorded values.
    assert_eq!(
        wave.value_at(alias, 30).unwrap(),
        wave.value_at(data, 30).unwrap()
    );
    assert_eq!(value_of(&wave, "top.data", 30), bits("00000100"));
}

#[test]
fn a_signal_that_was_not_recorded_is_an_error_not_a_zero() {
    let wave = algo();
    match wave.signal("top.not_recorded") {
        Err(WaveError::SignalNotFound { path }) => assert_eq!(path, "top.not_recorded"),
        other => panic!("expected SignalNotFound, got {other:?}"),
    }
    // Suffix matching is available, but it is a separate query: the strict
    // lookup does not guess which `carry` was meant.
    assert!(wave.find_signals("carry").len() == 1);
    assert!(wave.find_signals("nosuchsignal").is_empty());
}

#[test]
fn values_keep_x_and_z_instead_of_zero() {
    let wave = algo();

    assert_eq!(value_of(&wave, "top.xbit", 0), bits("x"));
    assert_eq!(value_of(&wave, "top.zbit", 0), bits("z"));
    assert_eq!(value_of(&wave, "top.xbit", 30), bits("1"));

    let unknown = value_of(&wave, "top.xbit", 0);
    match unknown.as_u64("top.xbit") {
        Err(WaveError::NotTwoState { bits, .. }) => assert_eq!(bits, "x"),
        other => panic!("x must not become a number, got {other:?}"),
    }
    match unknown.as_bool("top.xbit") {
        Err(WaveError::NotTwoState { .. }) => {}
        other => panic!("x must not become a bool, got {other:?}"),
    }
    assert!(value_of(&wave, "top.xbit", 30).as_bool("x").unwrap());

    // A 72-bit value is readable in full even though it does not fit in u64.
    let wide = wave.signal("top.wide").unwrap();
    let high = wave.value_at(wide, 20).unwrap().unwrap();
    let text = high.as_bits().unwrap().to_msb_string();
    assert_eq!(text.len(), 72);
    assert_eq!(text, format!("1{}", "0".repeat(71)));
    match high.as_u64("top.wide") {
        Err(WaveError::ValueTooWide { target, .. }) => assert_eq!(target, "u64"),
        other => panic!("a 72-bit value must not be truncated to u64: {other:?}"),
    }
}

#[test]
fn a_greater_width_value_is_not_truncated() {
    let wave = algo();
    assert_eq!(value_of(&wave, "top.nib", 40), bits("1111"));
    assert_eq!(value_of(&wave, "top.nib", 40).as_u64("nib").unwrap(), 15);
}

#[test]
fn value_queries_never_invent_a_sample() {
    let wave = algo();
    // A time long after the recording holds the last recorded value.
    assert_eq!(value_of(&wave, "top.clk", u64::MAX), bits("1"));
    assert_eq!(wave.observation_index(0), Some(0));
    // A time that is not a recorded timestamp reports the held value, and
    // `exact_at` says whether that timestamp really contains a change.
    assert_eq!(value_of(&wave, "top.clk", 12), bits("0"));
    assert_eq!(exact_value_of(&wave, "top.clk", 12), None);
    assert_eq!(exact_value_of(&wave, "top.clk", 10), Some(bits("0")));
    assert_eq!(
        exact_value_of(&wave, "top.clk", 13),
        None,
        "clk does not change at 13"
    );
    assert_eq!(exact_value_of(&wave, "top.clk", 5), Some(bits("1")));

    // `late` is first recorded at 20: before that there is no value at all,
    // and the layer must say so instead of reporting a value or an event.
    assert_eq!(maybe_value_of(&wave, "top.late", 10), None);
    assert_eq!(exact_value_of(&wave, "top.late", 10), None);
    assert_eq!(maybe_value_of(&wave, "top.late", 20), Some(bits("1")));
    assert_eq!(
        wave.changes(wave.signal("top.late").unwrap()).unwrap(),
        vec![(20, bits("1"))]
    );
}

#[test]
fn change_records_are_merged_per_timestamp() {
    let wave = sameslot();
    assert_eq!(wave.timestamps(), [0, 10, 20, 30]);

    let d = wave.signal("top.d").unwrap();
    let bus = wave.signal("top.bus").unwrap();
    let tri = wave.signal("top.tri").unwrap();

    // `d` went 0 -> 1 -> 0 inside timestamp 10: the timestamp ends where it
    // started, so it is not a change at all.
    assert_eq!(
        wave.changes(d).unwrap(),
        vec![(0, bits("0")), (20, bits("1"))],
        "changes inside one timestamp merge into the value it ends with"
    );

    // `bus` was dumped twice inside timestamp 10 (15 then 1): one change, and
    // the value is the final one.
    assert_eq!(
        wave.changes(bus).unwrap(),
        vec![
            (0, bits("00000000")),
            (10, bits("00000001")),
            (20, bits("00000010"))
        ]
    );

    // Timestamp 30 re-dumps the same values; that is not a change.
    assert_eq!(
        wave.changes(tri).unwrap(),
        vec![(0, bits("0")), (10, bits("x")), (20, bits("1"))]
    );
    assert_eq!(value_of(&wave, "top.d", 30), bits("1"));
    assert_eq!(value_of(&wave, "top.bus", 30), bits("00000010"));

    // The unchanged re-dump is still visible as a recorded timestamp.
    assert!(wave.timestamps().contains(&30));
    assert_eq!(exact_value_of(&wave, "top.d", 30), None);
}

#[test]
fn physical_durations_convert_to_ticks_without_floats() {
    let wave = algo();
    let timescale = wave.timescale().expect("fixture declares a timescale");
    assert_eq!(timescale, Timescale::new(1, TimeUnit::Nano));

    assert_eq!(wave.ticks(Timescale::parse("10ns").unwrap()).unwrap(), 10);
    assert_eq!(wave.ticks(Timescale::parse("1ns").unwrap()).unwrap(), 1);
    assert_eq!(wave.ticks(Timescale::parse("1us").unwrap()).unwrap(), 1000);

    // A duration finer than the recording precision is refused rather than
    // rounded, so a step grid can never drift (REQ-DBG-025).
    match wave.ticks(Timescale::parse("1ps").unwrap()) {
        Err(WaveError::IndivisibleDuration { remainder_zs, .. }) => {
            assert_eq!(remainder_zs, 10u128.pow(6) * 1000)
        }
        other => panic!("expected IndivisibleDuration, got {other:?}"),
    }

    assert!(Timescale::parse("1.5ns").is_err());
    assert!(Timescale::parse("10").is_err());
    assert!(Timescale::parse("0ns").is_err());
}

#[test]
fn a_missing_file_is_reported_with_its_path() {
    match Waveform::load(fixture("does_not_exist.vcd")) {
        Err(WaveError::Io { path, .. }) => {
            assert!(path.ends_with("does_not_exist.vcd"), "{path:?}")
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn a_truncated_recording_is_refused_rather_than_half_read() {
    let path = std::env::temp_dir().join("shyhdl_replay_truncated_fixture.vcd");
    std::fs::write(&path, "$timescale 1ns $end\n$scope module top $end\n").unwrap();
    let outcome = Waveform::load(&path);
    let _ = std::fs::remove_file(&path);
    match outcome {
        Err(WaveError::Parse { .. }) | Err(WaveError::NoObservationPoints { .. }) => {}
        other => panic!("a truncated recording must be refused, got {other:?}"),
    }
}

#[test]
fn bit_vectors_expose_bits_least_significant_first() {
    let value = BitVec::from_msb_string("10000001");
    assert_eq!(value.width(), 8);
    assert_eq!(value.bits()[0], BitState::One);
    assert_eq!(value.bits()[7], BitState::One);
    assert_eq!(value.bits()[1], BitState::Zero);
    assert_eq!(value.to_msb_string(), "10000001");
    assert_eq!(value.as_u64("d").unwrap(), 0b1000_0001);
    // Width alone is not a reason to refuse: the value must not fit to fail.
    assert_eq!(
        BitVec::from_msb_string(&format!("{:072b}", 3u64))
            .as_u64("wide")
            .unwrap(),
        3
    );
}

/// Writes a throwaway file next to the other temporary test artifacts.
fn temp_file(name: &str, contents: &str) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, contents).expect("temporary file must be writable");
    path
}

#[test]
fn a_file_that_is_not_a_waveform_is_refused() {
    let path = temp_file(
        "shyhdl_replay_not_a_waveform.bin",
        "hello, not a waveform\n",
    );
    let outcome = Waveform::load(&path);
    let _ = std::fs::remove_file(&path);
    match outcome {
        Err(WaveError::Parse { path: reported, .. }) => assert_eq!(reported, path),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn a_recording_without_a_timescale_still_reads_but_refuses_durations() {
    let path = temp_file(
        "shyhdl_replay_no_timescale.vcd",
        "$scope module top $end\n\
         $var wire 1 ! clk $end\n\
         $upscope $end\n\
         $enddefinitions $end\n\
         #0\n\
         $dumpvars\n\
         0!\n\
         $end\n\
         #5\n\
         1!\n",
    );
    let wave = Waveform::load(&path).expect("a waveform without a timescale is still a waveform");
    let _ = std::fs::remove_file(&path);

    assert_eq!(wave.timescale(), None);
    assert_eq!(wave.timestamps(), [0, 5]);
    assert_eq!(
        value_of(&wave, "top.clk", 5),
        bits("1"),
        "values are readable without a timescale"
    );
    match wave.ticks(Timescale::parse("10ns").unwrap()) {
        Err(WaveError::MissingTimescale { path: reported }) => assert_eq!(reported, path),
        other => panic!("expected MissingTimescale, got {other:?}"),
    }
}
