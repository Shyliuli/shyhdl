//! Real-GDB acceptance tests for the replay debugger.
//!
//! These are behaviour tests, not self-tests: they start the RSP server, run the
//! real `gdb` binary against the generated debug ELF, and assert on what GDB
//! prints. The recording is a small VCD written by the test, with a grid
//! reference every 10 ns and a signal that changes at 13 ns and 14 ns, so the
//! "sampling at 10 ns and 20 ns shows the same value" case is exercised for real
//! (AC-MVP-029).
//!
//! Every test fails loudly when `gdb` is missing: an environment gap must not
//! look like a pass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shyhdl::debugger::{DebugOptions, DebugServer, RootMapping};
use shyhdl::metadata::{DesignMetadata, EnumVariant, SourceSpan, TypeField, ValueType, Variable};
use shyhdl::replay::{ReplayEngine, StepMode};
use shyhdl::waveform::Waveform;

/// Observation points: 0, 10, 13, 14, 20, 30 ns. Reference points (10 ns grid):
/// 0, 10, 20, 30. `sig_a` rises at 13 ns and falls at 14 ns, so the 10 ns and
/// 20 ns samples are both 0 (AC-MVP-029).
const VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module dut $end
$var wire 1 ! clk $end
$var wire 8 \" sig_a $end
$var wire 8 # sig_b $end
$var wire 9 $ pair $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
0!
b00000000 \"
b00000111 #
b000000000 $
#10
1!
#13
b00000001 \"
b000000011 $
#14
b00000000 \"
b000010011 $
#20
0!
b00000000 \"
#30
1!
b00001000 #
";

/// Two instances of one module whose signal has the same source name in both.
/// The values differ, so a symbol that resolved to the wrong instance shows.
const TWO_INSTANCE_VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module dut $end
$scope module u1 $end
$var wire 8 ! sig_a $end
$upscope $end
$scope module u2 $end
$var wire 8 \" sig_a $end
$upscope $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
b00000101 !
b00001001 \"
#10
b00000110 !
";

/// Signed fixture. `sneg` is -1 at 0 ns and -8 at 10 ns; `spair` packs its signed
/// field in the most significant bits (`neg` -1 -> 0b1111) above its unsigned
/// field (`pos` 7 -> 0b0111); `sx` holds x at 0 ns, which is not a value.
const SIGNED_VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module dut $end
$var wire 8 ! sneg $end
$var wire 8 \" spair $end
$var wire 8 # sx $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
b11111111 !
b11110111 \"
bxxxxxxxx #
#10
b11111000 !
b01110000 \"
b00000001 #
";

/// Array fixture. `xs` is `[xs_t; 3]` with `xs_t = { s: int<4>, u: uint<4> }`,
/// packed 8 bits per element and element 0 in the lowest bits of the recording.
/// At 0 ns: {(-1,2), (-8,9), (5,0)}; at 10 ns: {(2,1), (-3,7), (-8,4)}.
const ARRAY_VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module dut $end
$var wire 24 ! xs $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
b010100001000100111110010 !
#10
b100001001101011100100001 !
";

/// Payload enum fixture (REQ-LANG-040/041). The tag is in the lowest bits of a
/// recorded value and the widest payload sits above it, unused payload bits
/// zero. `holder` keeps a field after the enum, `queue` is an array of the same
/// enum, and `delta` carries a signed payload.
///
/// At 0 ns: holder{before 0x11, msg Data(42), after 0x22}, delta Move(-8),
/// queue [Data(1), Ping]. At 10 ns: holder{before 0x33, msg Pair{7,9},
/// after 0x44}, delta Idle, queue [Ping, Pair{7,9}].
const ENUM_VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module dut $end
$var wire 34 ! holder $end
$var wire 13 \" delta $end
$var wire 36 % queue $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
b0001000100000000001010100100100010 !
b1111111110001 \"
b000000000000000000000000000000000101 %
#10
b0011001100000111000010011001000100 !
b0000000000000 \"
b000001110000100110000000000000000000 %
";

fn temp_dir() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shyhdl-debugger-test-{stamp}"));
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

/// A variable of the design root instance.
fn variable(id: &str, name: &str, ty: ValueType) -> Variable {
    instance_variable(id, name, "top", ty)
}

/// A variable of a named instance, for fixtures with two same-named signals.
fn instance_variable(id: &str, name: &str, instance_path: &str, ty: ValueType) -> Variable {
    Variable {
        id: id.to_string(),
        source_name: name.to_string(),
        instance_path: instance_path.to_string(),
        rtl_name: name.to_string(),
        ty,
        source: SourceSpan::new("design.shy.rs", 1, 1),
    }
}

fn metadata() -> DesignMetadata {
    DesignMetadata::new(
        "test-build",
        "top",
        vec![
            variable("top.sig_a", "sig_a", ValueType::UInt { width: 8 }),
            variable("top.sig_b", "sig_b", ValueType::UInt { width: 8 }),
            variable(
                "top.pair",
                "pair",
                ValueType::Struct {
                    name: "pair_t".to_string(),
                    fields: vec![
                        TypeField {
                            name: "hi".to_string(),
                            ty: ValueType::UInt { width: 8 },
                        },
                        TypeField {
                            name: "lo".to_string(),
                            ty: ValueType::Bool,
                        },
                    ],
                },
            ),
        ],
    )
}

/// Start the debugger and run `gdb` with `commands`, returning its console.
fn run_gdb(dir: &Path, commands: &[&str]) -> String {
    let owned: Vec<String> = commands.iter().map(|c| c.to_string()).collect();
    run_gdb_named(dir, move |_| owned)
}

/// Run `gdb` against the shared fixture recording, with commands built from the
/// debug symbol names.
fn run_gdb_named(dir: &Path, build: impl FnOnce(&dyn Fn(&str) -> String) -> Vec<String>) -> String {
    run_gdb_with(dir, VCD, &metadata(), build)
}

/// Run `gdb` against a session built from `vcd` and `meta` on the 10 ns grid.
///
/// `build` receives the metadata-variable-id to debug-symbol-name lookup. Those
/// names come from the instance path, so they are not the bare source names.
fn run_gdb_with(
    dir: &Path,
    vcd: &str,
    meta: &DesignMetadata,
    build: impl FnOnce(&dyn Fn(&str) -> String) -> Vec<String>,
) -> String {
    let path = dir.join("recording.vcd");
    std::fs::write(&path, vcd).expect("write the fixture recording");
    let waveform = Waveform::load(&path).expect("load the fixture recording");
    let engine = ReplayEngine::new(
        waveform,
        StepMode::Interval {
            origin: 0,
            period: 10,
        },
    )
    .expect("the fixture has a usable reference grid");
    let elf = dir.join("debug.elf");
    let options = DebugOptions {
        port: 0,
        root: RootMapping::new("top", "TOP.tb.dut"),
        symbol_elf: Some(elf),
    };
    let server = DebugServer::build(engine, meta, &options).expect("build the debug session");
    let names = |id: &str| {
        server
            .symbol_name(id)
            .unwrap_or_else(|| panic!("{id} must have a debug symbol"))
            .to_string()
    };
    let commands = build(&names);
    let port = server.port();
    let elf = server.elf_path().to_path_buf();
    let serving = std::thread::spawn(move || server.serve_blocking());

    let mut gdb = Command::new("gdb");
    gdb.arg("-q")
        .arg("-batch")
        .arg("-nx")
        .arg("-ex")
        .arg("set confirm off")
        .arg("-ex")
        .arg("set pagination off")
        .arg(&elf)
        .arg("-ex")
        .arg(format!("target remote 127.0.0.1:{port}"));
    for command in &commands {
        gdb.arg("-ex").arg(command);
    }
    gdb.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = gdb.spawn().unwrap_or_else(|error| {
        panic!("cannot run gdb ({error}); these acceptance tests need a real GDB");
    });
    let (console, timed_out) = wait_with_deadline(child, GDB_DEADLINE);
    join_server(serving, port, timed_out);
    assert!(
        !timed_out,
        "gdb did not finish within {GDB_DEADLINE:?}; console so far:\n{console}"
    );
    console
}

/// How long one GDB session may take before it is killed. A stuck protocol
/// handshake or a condition-evaluation loop must fail this test, not hang the
/// whole test binary.
const GDB_DEADLINE: Duration = Duration::from_secs(30);

/// Wait for `child`, killing it when it overruns `deadline`.
///
/// Killing GDB closes its socket, which is what lets the serving thread stop
/// waiting for a packet.
fn wait_with_deadline(mut child: Child, deadline: Duration) -> (String, bool) {
    let start = Instant::now();
    loop {
        match child.try_wait().expect("wait for gdb") {
            Some(_) => break,
            None if start.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let output = child
                    .wait_with_output()
                    .expect("collect the killed gdb's output");
                let console = String::from_utf8_lossy(&output.stdout).into_owned()
                    + &String::from_utf8_lossy(&output.stderr);
                return (console, true);
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let output = child.wait_with_output().expect("collect gdb's output");
    let console = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    (console, false)
}

/// Join the serving thread without waiting forever.
///
/// If GDB died before connecting, `accept` is still blocked; a probe connection
/// releases it, and the closed probe ends the session.
fn join_server(
    handle: std::thread::JoinHandle<Result<(), shyhdl::debugger::DebugError>>,
    port: u16,
    timed_out: bool,
) {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() > Duration::from_secs(2) {
            if let Ok(probe) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                drop(probe);
            }
            std::thread::sleep(Duration::from_millis(100));
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if timed_out => {
            // The session ended because we killed GDB; the test asserts on the
            // console it produced, so the transport error is expected.
            eprintln!("debug session ended after the GDB deadline: {error}");
        }
        Ok(Err(error)) => panic!("the debug session failed: {error}"),
        Err(_) => panic!("the debug server thread panicked"),
    }
}

/// Read the `pc=<hex>` marker a test prints after a command.
fn pc_after(console: &str, label: &str) -> u64 {
    let line = console
        .lines()
        .find(|line| line.contains(&format!("[{label}]")))
        .unwrap_or_else(|| panic!("no [{label}] line in GDB output:\n{console}"));
    let hex = line
        .split("pc=0x")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("cannot read the pc from {line:?}"));
    u64::from_str_radix(hex, 16).expect("pc is hexadecimal")
}

#[test]
fn step_and_next_advance_one_reference_point_and_reverse_step_returns() {
    let dir = temp_dir();
    let console = run_gdb(
        &dir,
        &[
            "printf \"[s0] pc=%p\\n\", $pc",
            "step",
            "printf \"[s1] pc=%p\\n\", $pc",
            "step",
            "printf \"[s2] pc=%p\\n\", $pc",
            "next",
            "printf \"[s3] pc=%p\\n\", $pc",
            "reverse-step",
            "printf \"[r1] pc=%p\\n\", $pc",
            "reverse-step",
            "printf \"[r2] pc=%p\\n\", $pc",
            "step",
            "printf \"[s4] pc=%p\\n\", $pc",
        ],
    );

    // Point 0 is the first observation; points 1, 4, 5 are the 10, 20 and 30 ns
    // reference points, and 2 and 3 are the 13 and 14 ns changes in between.
    let p0 = pc_after(&console, "s0");
    let p1 = pc_after(&console, "s1");
    let p2 = pc_after(&console, "s2");
    let p3 = pc_after(&console, "s3");
    assert_eq!(
        p1 - p0,
        8,
        "one step advances one observation point (10 ns)"
    );
    assert_eq!(
        p2 - p0,
        32,
        "the next step reaches the 20 ns reference point, skipping 13/14 ns"
    );
    assert_eq!(p3 - p2, 8, "next reaches the 30 ns reference point");
    assert_eq!(
        pc_after(&console, "r1"),
        p2,
        "reverse-step returns to the previous reference point"
    );
    assert_eq!(
        pc_after(&console, "r2"),
        p1,
        "reverse-step again returns to the one before it"
    );
    assert_eq!(
        pc_after(&console, "s4"),
        p2,
        "moving forward again is stable"
    );
}

#[test]
fn watch_reports_changes_between_reference_points() {
    let dir = temp_dir();
    let console = run_gdb_named(&dir, |name| {
        let sig_a = name("top.sig_a");
        vec![
            format!("watch {sig_a}"),
            "continue".to_string(),
            format!("printf \"[w1] {sig_a}=%d\\n\", {sig_a}"),
            "continue".to_string(),
            format!("printf \"[w2] {sig_a}=%d\\n\", {sig_a}"),
        ]
    });

    let hits: Vec<&str> = console
        .lines()
        .filter(|line| line.contains("Hardware watchpoint 1"))
        .collect();
    assert!(
        hits.len() >= 2,
        "the 13 ns rise and the 14 ns fall are both reported:\n{console}"
    );
    assert!(
        console.contains("New value = 1"),
        "the 13 ns rise is reported even though 10 ns and 20 ns both read 0:\n{console}"
    );
    assert!(
        console.contains("New value = 0"),
        "the 14 ns fall is reported:\n{console}"
    );
}

#[test]
fn print_shows_struct_fields_of_the_current_observation_point() {
    let dir = temp_dir();
    let console = run_gdb_named(&dir, |name| {
        let pair = name("top.pair");
        vec![
            "continue".to_string(),
            "printf \"[h1] pc=%p\\n\", $pc".to_string(),
            format!("print {pair}"),
        ]
    });
    assert!(
        console.contains("hi = "),
        "a struct prints its fields:\n{console}"
    );
}

#[test]
fn writes_are_refused_and_the_address_stays_readable() {
    // AC-MVP-035, REQ-DBG-011: the write is refused with a reason, and refusing
    // it must leave the read path for that same address intact.
    let dir = temp_dir();
    let console = run_gdb_named(&dir, |name| {
        let sig_a = name("top.sig_a");
        vec![
            format!("printf \"[before] %d\\n\", {sig_a}"),
            format!("set var {sig_a} = 5"),
            format!("printf \"[after] %d\\n\", {sig_a}"),
        ]
    });
    assert!(
        console.contains("[before] 0"),
        "the recorded value reads before the write:\n{console}"
    );
    assert!(
        console.contains("Cannot access memory") || console.contains("cannot be modified"),
        "a write to a replayed signal is refused with a reason:\n{console}"
    );
    assert!(
        console.contains("[after] 0"),
        "the same address still reads its recorded value, not the refused write:\n{console}"
    );
}

#[test]
fn reverse_step_prints_the_previous_observation_value() {
    // AC-MVP-023: what `print` shows right after `reverse-step` is the previous
    // reference point's value, not the one the cursor was on. `sig_b` is 7 until
    // 30 ns, where it becomes 8, so only the reversed cursor can read 7.
    let dir = temp_dir();
    let console = run_gdb_named(&dir, |name| {
        let sig_b = name("top.sig_b");
        vec![
            "step".to_string(),
            "step".to_string(),
            "step".to_string(),
            format!("printf \"[fwd] %d\\n\", {sig_b}"),
            "reverse-step".to_string(),
            format!("printf \"[rev] %d\\n\", {sig_b}"),
        ]
    });
    assert!(
        console.contains("[fwd] 8"),
        "three steps reach the 30 ns point, where sig_b is 8:\n{console}"
    );
    assert!(
        console.contains("[rev] 7"),
        "the value printed after reverse-step is the 20 ns one:\n{console}"
    );
}

#[test]
fn two_instances_with_the_same_signal_name_read_independently() {
    // AC-MVP-026: two instances of one module expose a signal with the same
    // source name. The debug symbol names are per instance, so reading one must
    // never return the other's value.
    let dir = temp_dir();
    let meta = DesignMetadata::new(
        "test-build",
        "top",
        vec![
            instance_variable(
                "top.u1.sig_a",
                "sig_a",
                "top.u1",
                ValueType::UInt { width: 8 },
            ),
            instance_variable(
                "top.u2.sig_a",
                "sig_a",
                "top.u2",
                ValueType::UInt { width: 8 },
            ),
        ],
    );
    let console = run_gdb_with(&dir, TWO_INSTANCE_VCD, &meta, |name| {
        let u1 = name("top.u1.sig_a");
        let u2 = name("top.u2.sig_a");
        vec![
            format!("printf \"[p0] %d %d\\n\", {u1}, {u2}"),
            "step".to_string(),
            format!("printf \"[p1] %d %d\\n\", {u1}, {u2}"),
        ]
    });
    assert!(
        console.contains("[p0] 5 9"),
        "each instance reads its own signal:\n{console}"
    );
    assert!(
        console.contains("[p1] 6 9"),
        "and still reads it after the cursor moves:\n{console}"
    );
}

#[test]
fn signed_values_print_negatively_and_survive_reverse_step() {
    // REQ-DBG-004, REQ-DBG-005: stored signed bytes plus a signed DWARF type
    // must show -1, not 255. The signed field of a struct is packed above its
    // unsigned neighbour in the recording, and a value holding x is not a value
    // at all: it is reported unreadable rather than invented as zero.
    let dir = temp_dir();
    let meta = DesignMetadata::new(
        "test-build",
        "top",
        vec![
            variable("top.sneg", "sneg", ValueType::SInt { width: 8 }),
            variable(
                "top.spair",
                "spair",
                ValueType::Struct {
                    name: "spair_t".to_string(),
                    fields: vec![
                        TypeField {
                            name: "neg".to_string(),
                            ty: ValueType::SInt { width: 4 },
                        },
                        TypeField {
                            name: "pos".to_string(),
                            ty: ValueType::UInt { width: 4 },
                        },
                    ],
                },
            ),
            variable("top.sx", "sx", ValueType::SInt { width: 8 }),
        ],
    );
    let console = run_gdb_with(&dir, SIGNED_VCD, &meta, |name| {
        let sneg = name("top.sneg");
        let spair = name("top.spair");
        let sx = name("top.sx");
        vec![
            format!("ptype {spair}"),
            format!("printf \"[p0] sneg=%d\\n\", {sneg}"),
            format!("printf \"[p0] spair.neg=%d spair.pos=%d\\n\", {spair}.neg, {spair}.pos"),
            format!("printf \"[p0] sx=%d\\n\", {sx}"),
            format!("printf \"[p0b] sneg=%d\\n\", {sneg}"),
            "step".to_string(),
            format!("printf \"[p1] sneg=%d\\n\", {sneg}"),
            format!("printf \"[p1] spair.neg=%d spair.pos=%d\\n\", {spair}.neg, {spair}.pos"),
            format!("printf \"[p1] sx=%d\\n\", {sx}"),
            "reverse-step".to_string(),
            format!("printf \"[rev] sneg=%d\\n\", {sneg}"),
            format!("printf \"[rev] spair.neg=%d spair.pos=%d\\n\", {spair}.neg, {spair}.pos"),
        ]
    });
    assert!(
        console.contains("int<4> neg"),
        "the signed field keeps a signed DWARF type:\n{console}"
    );
    assert!(
        console.contains("[p0] sneg=-1"),
        "-1 is printed signed, not as 255:\n{console}"
    );
    assert!(
        console.contains("[p0] spair.neg=-1 spair.pos=7"),
        "the signed field is negative beside a positive unsigned one:\n{console}"
    );
    // GDB prints the format prefix before it fetches the value, so the marker is
    // followed by the error, never by a number.
    let after_sx_marker = console.split("[p0] sx=").nth(1).unwrap_or("");
    assert!(
        !after_sx_marker.starts_with(|c: char| c.is_ascii_digit()),
        "a value holding x prints no number:\n{console}"
    );
    assert!(
        console.contains("Cannot access memory"),
        "the unreadable value is reported instead of zeroed:\n{console}"
    );
    assert!(
        console.contains("[p0b] sneg=-1"),
        "the session keeps serving reads after the unreadable one:\n{console}"
    );
    assert!(
        console.contains("[p1] sneg=-8"),
        "a negative int<8> value is signed too:\n{console}"
    );
    assert!(
        console.contains("[p1] spair.neg=7 spair.pos=0"),
        "the positive boundary of int<4> stays positive:\n{console}"
    );
    assert!(
        console.contains("[p1] sx=1"),
        "the same signal reads at the next point, so only 0 ns was unreadable:\n{console}"
    );
    assert!(
        console.contains("[rev] sneg=-1") && console.contains("[rev] spair.neg=-1 spair.pos=7"),
        "after reverse-step the signed values are the previous point's:\n{console}"
    );
}

#[test]
fn array_of_signed_structs_slices_each_element_independently() {
    // REQ-DBG-009: two different strides are involved and must not be confused.
    // In the recording an element is sliced out of the packed value at its own
    // `bit_width` (element 0 in the lowest bits, fields most significant first);
    // in the debug memory GDB reads, the same elements are laid out at each
    // element's `byte_size`. Here one element is 8 raw bits but 2 debug bytes,
    // which is exactly where a mix-up would show. Every element therefore has to
    // read its own value: a stride or bit order mistake shows up as one element's
    // fields appearing in another's.
    let dir = temp_dir();
    let element = ValueType::Struct {
        name: "xs_t".to_string(),
        fields: vec![
            TypeField {
                name: "s".to_string(),
                ty: ValueType::SInt { width: 4 },
            },
            TypeField {
                name: "u".to_string(),
                ty: ValueType::UInt { width: 4 },
            },
        ],
    };
    let meta = DesignMetadata::new(
        "test-build",
        "top",
        vec![variable(
            "top.xs",
            "xs",
            ValueType::Array {
                element: Box::new(element),
                len: 3,
            },
        )],
    );
    let console = run_gdb_with(&dir, ARRAY_VCD, &meta, |name| {
        let xs = name("top.xs");
        vec![
            format!("ptype {xs}"),
            format!("printf \"[p0] x0.s=%d x0.u=%d\\n\", {xs}[0].s, {xs}[0].u"),
            format!("printf \"[p0] x1.s=%d x1.u=%d\\n\", {xs}[1].s, {xs}[1].u"),
            format!("printf \"[p0] x2.s=%d x2.u=%d\\n\", {xs}[2].s, {xs}[2].u"),
            format!("print {xs}[2]"),
            "step".to_string(),
            format!("printf \"[p1] x0.s=%d x0.u=%d\\n\", {xs}[0].s, {xs}[0].u"),
            format!("printf \"[p1] x1.s=%d x1.u=%d\\n\", {xs}[1].s, {xs}[1].u"),
            format!("printf \"[p1] x2.s=%d x2.u=%d\\n\", {xs}[2].s, {xs}[2].u"),
            "reverse-step".to_string(),
            format!("printf \"[rev] x1.s=%d x1.u=%d\\n\", {xs}[1].s, {xs}[1].u"),
        ]
    });
    assert!(
        console.contains("} [3]") && console.contains("int<4> s"),
        "GDB sees an array of three signed structs:\n{console}"
    );
    // Element 0 is in the lowest bits, and each element's signed field is above
    // its own unsigned one.
    assert!(
        console.contains("[p0] x0.s=-1 x0.u=2"),
        "element 0 reads its own signed and unsigned fields:\n{console}"
    );
    assert!(
        console.contains("[p0] x1.s=-8 x1.u=9"),
        "element 1 is not element 0's or element 2's value:\n{console}"
    );
    assert!(
        console.contains("[p0] x2.s=5 x2.u=0"),
        "element 2 is sliced at the last element's stride:\n{console}"
    );
    assert!(
        console.contains("$1 = {s = 5 ") && console.contains("u = 0 "),
        "one whole element prints as a struct:\n{console}"
    );

    // A condition on one element's signed field is evaluated at every point, so
    // it stops where that field is negative. The walk advances past the point the
    // debugger is already stopped on, so the hit is 10 ns, where element 1 is -3
    // with 7 in its unsigned field. This fixture has no other observation point.
    let dir2 = temp_dir();
    let console2 = run_gdb_with(&dir2, ARRAY_VCD, &meta, |name| {
        let xs = name("top.xs");
        vec![
            format!("break tick if {xs}[1].s < 0"),
            "continue".to_string(),
            format!("printf \"[hit] x1.s=%d x1.u=%d\\n\", {xs}[1].s, {xs}[1].u"),
        ]
    });
    assert!(
        console2.contains("[hit] x1.s=-3 x1.u=7"),
        "the condition hits the point where element 1 is negative:\n{console2}"
    );
    assert!(
        console.contains("[p1] x0.s=2 x0.u=1"),
        "the same element reads the next point's value:\n{console}"
    );
    assert!(
        console.contains("[p1] x1.s=-3 x1.u=7"),
        "a signed field stays signed when it turns negative at the next point:\n{console}"
    );
    assert!(
        console.contains("[p1] x2.s=-8 x2.u=4"),
        "element 2 is negative now and still isolated:\n{console}"
    );
    assert!(
        console.contains("[rev] x1.s=-8 x1.u=9"),
        "after reverse-step element 1 is the previous point's value again:\n{console}"
    );
}

#[test]
fn conditional_breakpoint_is_checked_at_every_observation_point() {
    let dir = temp_dir();
    // `pair` changes at 13 ns while `sig_a` is 1, and again at 14 ns. The
    // condition is only true at 13 ns, which is not a reference point: a
    // debugger that only checked clock samples would miss it. `tick` is the
    // synthetic anchor the symbol ELF exposes for exactly this purpose
    // (REQ-DBG-056).
    let console = run_gdb_named(&dir, |name| {
        let pair = name("top.pair");
        vec![
            format!("break tick if {pair}.hi == 1 && {pair}.lo == 1"),
            "continue".to_string(),
            format!("printf \"[c1] {pair}.hi=%d\\n\", {pair}.hi"),
        ]
    });
    assert!(
        console.contains("Breakpoint 1"),
        "the breakpoint is accepted:\n{console}"
    );
    assert!(
        console.contains("hi=1") || console.contains("hi = 1"),
        "the condition is satisfied at the 13 ns observation point:\n{console}"
    );
}

#[test]
fn tick_breakpoint_state_does_not_leak_into_later_commands() {
    // The anchor walk is armed by GDB inserting the breakpoint and disarmed
    // when it removes it. A hit must not leave the walk active: `step` has to
    // reach the next reference point again, and after `delete` a `continue`
    // must run to the end of the recording instead of walking observation
    // points (REQ-DBG-039).
    let dir = temp_dir();
    let console = run_gdb_named(&dir, |name| {
        let pair = name("top.pair");
        vec![
            "printf \"[p0] pc=%p\\n\", $pc".to_string(),
            format!("break tick if {pair}.hi == 1"),
            "continue".to_string(),
            format!("printf \"[hit] {pair}.hi=%d pc=%p\\n\", {pair}.hi, $pc"),
            "step".to_string(),
            "printf \"[stepped] pc=%p\\n\", $pc".to_string(),
            "reverse-step".to_string(),
            "printf \"[reversed] pc=%p\\n\", $pc".to_string(),
            "delete".to_string(),
            "continue".to_string(),
            "printf \"[deleted] pc=%p\\n\", $pc".to_string(),
        ]
    });

    let base = pc_after(&console, "p0");
    let hit = pc_after(&console, "hit");
    assert!(
        console.contains("hi=1"),
        "the condition hit the 13 ns point:\n{console}"
    );
    assert_ne!(
        hit, base,
        "the hit is reported at the tick anchor, not at the first point:\n{console}"
    );
    // Point 2 (13 ns) is where the hit happened; the next reference point is
    // point 4 (20 ns), which is four slots further.
    assert_eq!(
        pc_after(&console, "stepped") - base,
        32,
        "after a hit, step reaches the next reference point again:\n{console}"
    );
    assert!(
        pc_after(&console, "reversed") < pc_after(&console, "stepped"),
        "reverse-step after a hit moves back:\n{console}"
    );
    // With the breakpoint deleted, the walk is over: the cursor runs to the end
    // of the recording.
    assert!(
        console.contains("Reached end of recorded history")
            || pc_after(&console, "deleted") >= base + 40,
        "a deleted breakpoint leaves no walk behind:\n{console}"
    );
}

#[test]
fn unavailable_signals_do_not_print_a_zero() {
    let dir = temp_dir();
    // `missing` is in the metadata but not in the recording: it must have no
    // symbol at all, so GDB says so instead of printing 0 (AC-MVP-033).
    let mut meta = metadata();
    meta.variables.push(variable(
        "top.missing",
        "missing",
        ValueType::UInt { width: 8 },
    ));
    let console = run_gdb_with(&dir, VCD, &meta, |_| vec!["print missing".to_string()]);

    assert!(
        console.contains("No symbol") || console.contains("no symbol"),
        "an unrecorded signal is reported as unavailable:\n{console}"
    );
    assert!(
        !console.contains("= 0"),
        "an unrecorded signal is never printed as zero:\n{console}"
    );
}

#[test]
fn bind_signals_reports_what_the_recording_does_not_have() {
    let dir = temp_dir();
    let mut meta = metadata();
    meta.variables.push(variable(
        "top.missing",
        "missing",
        ValueType::UInt { width: 8 },
    ));
    let path = dir.join("recording.vcd");
    std::fs::write(&path, VCD).expect("write recording");
    let waveform = Waveform::load(&path).expect("load recording");
    let binding = shyhdl::debugger::session::bind_signals(
        &meta,
        &waveform,
        &RootMapping::new("top", "TOP.tb.dut"),
    )
    .expect("the declared root mapping applies to every variable");

    let mut found: Vec<&String> = binding.signal_of_var.keys().collect();
    found.sort();
    assert_eq!(
        found,
        vec!["top.pair", "top.sig_a", "top.sig_b"],
        "recorded signals are bound"
    );
    assert_eq!(binding.unavailable, vec!["top.missing".to_string()]);
}

#[test]
fn a_root_mapping_that_cannot_place_an_instance_path_is_an_error() {
    let mapping = RootMapping::new("top", "TOP.tb.dut");
    assert_eq!(
        mapping
            .waveform_path("top.u_adder")
            .expect("under the root"),
        "TOP.tb.dut.u_adder"
    );
    assert!(
        mapping.waveform_path("other.u_adder").is_err(),
        "an instance path outside the declared root is refused, not guessed"
    );
}

/// Keeping the fixture honest: the metadata names must match the recording.
#[test]
fn fixture_paths_exist_in_the_recording() {
    let dir = temp_dir();
    let path = dir.join("recording.vcd");
    std::fs::write(&path, VCD).expect("write recording");
    let waveform = Waveform::load(&path).expect("load recording");
    for name in ["TOP.tb.dut.clk", "TOP.tb.dut.sig_a", "TOP.tb.dut.sig_b"] {
        assert!(
            waveform.signal(name).is_ok(),
            "{name} must exist in the fixture recording"
        );
    }
    let observations: Vec<u64> = waveform.timestamps().to_vec();
    assert_eq!(
        observations,
        vec![0, 10, 13, 14, 20, 30],
        "the fixture records exactly the observation points the tests assume"
    );
    let _: HashMap<(), ()> = HashMap::new();
}

#[test]
fn payload_enums_read_the_active_variant_and_keep_the_layout_fixed() {
    // REQ-LANG-040/041, REQ-DBG-009: the tag is the lowest bits of the recorded
    // value and the active payload sits above it. In the debug memory GDB reads,
    // the enum is a tag plus a union of every payload, so the field after the
    // enum and an array of enums keep their offsets whatever the tag is. Only
    // the active payload's bytes are written: the other interpretation of those
    // same bytes is shown as it is, never padded into invented zeros.
    let dir = temp_dir();
    let message = || ValueType::Enum {
        name: "Msg".to_string(),
        tag_width: 2,
        variants: vec![
            EnumVariant {
                name: "ping".to_string(),
                tag: 0,
                payload: None,
            },
            EnumVariant {
                name: "data".to_string(),
                tag: 1,
                payload: Some(Box::new(ValueType::UInt { width: 8 })),
            },
            EnumVariant {
                name: "pair".to_string(),
                tag: 2,
                payload: Some(Box::new(ValueType::Struct {
                    name: "Pair8".to_string(),
                    fields: vec![
                        TypeField {
                            name: "hi".to_string(),
                            ty: ValueType::UInt { width: 8 },
                        },
                        TypeField {
                            name: "lo".to_string(),
                            ty: ValueType::UInt { width: 8 },
                        },
                    ],
                })),
            },
        ],
    };
    let meta = DesignMetadata::new(
        "test-build",
        "top",
        vec![
            variable(
                "top.holder",
                "holder",
                ValueType::Struct {
                    name: "Holder".to_string(),
                    fields: vec![
                        TypeField {
                            name: "before".to_string(),
                            ty: ValueType::UInt { width: 8 },
                        },
                        TypeField {
                            name: "msg".to_string(),
                            ty: message(),
                        },
                        TypeField {
                            name: "after".to_string(),
                            ty: ValueType::UInt { width: 8 },
                        },
                    ],
                },
            ),
            variable(
                "top.delta",
                "delta",
                ValueType::Enum {
                    name: "Delta".to_string(),
                    tag_width: 1,
                    variants: vec![
                        EnumVariant {
                            name: "idle".to_string(),
                            tag: 0,
                            payload: None,
                        },
                        EnumVariant {
                            name: "move".to_string(),
                            tag: 1,
                            payload: Some(Box::new(ValueType::SInt { width: 12 })),
                        },
                    ],
                },
            ),
            variable(
                "top.queue",
                "queue",
                ValueType::Array {
                    element: Box::new(message()),
                    len: 2,
                },
            ),
        ],
    );
    let console = run_gdb_with(&dir, ENUM_VCD, &meta, |name| {
        let holder = name("top.holder");
        let delta = name("top.delta");
        let queue = name("top.queue");
        vec![
            format!("ptype {holder}"),
            format!("ptype {holder}.msg"),
            format!("printf \"[size] msg=%d holder=%d\\n\", sizeof({holder}.msg), sizeof({holder})"),
            format!("printf \"[p0] before=%d after=%d\\n\", {holder}.before, {holder}.after"),
            format!(
                "printf \"[p0] tag=%d data=%d\\n\", {holder}.msg.tag, {holder}.msg.payload.data"
            ),
            format!(
                "printf \"[p0] alias_hi=%d alias_lo=%d\\n\", {holder}.msg.payload.pair.hi, {holder}.msg.payload.pair.lo"
            ),
            format!("printf \"[p0] move=%d\\n\", {delta}.payload.move"),
            format!("printf \"[p0] q0=%d q1=%d\\n\", {queue}[0].tag, {queue}[1].tag"),
            "step".to_string(),
            format!("printf \"[p1] before=%d after=%d\\n\", {holder}.before, {holder}.after"),
            format!(
                "printf \"[p1] tag=%d pair=%d,%d\\n\", {holder}.msg.tag, {holder}.msg.payload.pair.hi, {holder}.msg.payload.pair.lo"
            ),
            format!("printf \"[p1] delta=%d\\n\", {delta}.tag"),
            format!("printf \"[p1] q0=%d q1=%d\\n\", {queue}[0].tag, {queue}[1].tag"),
            "reverse-step".to_string(),
            format!(
                "printf \"[rev] tag=%d data=%d\\n\", {holder}.msg.tag, {holder}.msg.payload.data"
            ),
            format!("break tick if {delta}.tag == 0"),
            "continue".to_string(),
            format!(
                "printf \"[hit] delta=%d holder_tag=%d\\n\", {delta}.tag, {holder}.msg.tag"
            ),
        ]
    });
    // `ptype <struct>` names a nested member but does not expand it, so the enum
    // itself is asked for: the tag type and the payload union both have to show.
    assert!(
        console.contains("enum Msg_tag tag") && console.contains("union payload payload"),
        "a payload enum is a tag plus a union of the variant payloads:\n{console}"
    );
    assert!(
        console.contains("[size] msg=3 holder=5"),
        "the enum is the tag plus the widest payload, and the struct keeps its size:\n{console}"
    );
    assert!(
        console.contains("[p0] before=17 after=34"),
        "the field after the payload enum keeps its place:\n{console}"
    );
    assert!(
        console.contains("[p0] tag=1 data=42"),
        "the active payload reads at the first observation point:\n{console}"
    );
    assert!(
        console.contains("[p0] alias_hi=42 alias_lo=0"),
        "the union shows the same bytes instead of invented zeros:\n{console}"
    );
    assert!(
        console.contains("[p0] move=-8"),
        "a signed payload is sign extended:\n{console}"
    );
    assert!(
        console.contains("[p0] q0=1 q1=0"),
        "element 0 of the enum array is the lowest element:\n{console}"
    );
    assert!(
        console.contains("[p1] before=51 after=68"),
        "the field after the enum survives a step:\n{console}"
    );
    assert!(
        console.contains("[p1] tag=2 pair=7,9"),
        "the next point reads its own active payload:\n{console}"
    );
    assert!(
        console.contains("[p1] delta=0"),
        "a payload-free variant prints its tag:\n{console}"
    );
    assert!(
        console.contains("[p1] q0=0 q1=2"),
        "the array stride does not move with the tag:\n{console}"
    );
    assert!(
        console.contains("[rev] tag=1 data=42"),
        "reverse-step reads the previous point again:\n{console}"
    );
    assert!(
        console.contains("[hit] delta=0 holder_tag=2"),
        "the condition is checked at every observation point:\n{console}"
    );
}
