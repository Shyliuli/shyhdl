//! Cross-checks the replay layer against waveforms produced by real tools.
//!
//! The fixtures in `tests/fixtures/replay/` are hand written and only pin down
//! the algorithms. This file closes the other half: it elaborates a small
//! design with Icarus Verilog and Verilator, dumps the waveform, and asserts
//! that the same cursor semantics hold on tool output.
//!
//! The design deliberately contains what the hand written fixtures cannot
//! honestly claim: two instances of one module with different parameters,
//! 72-bit values (wider than `u64`), a tri-state net, and stimulus that moves
//! between clock edges.
//!
//! The waveform files are build products and are written to a temporary
//! directory, never into the repository (REQ-VER-008).
//!
//! These tests are `#[ignore]`d so that a plain `cargo test` stays offline and
//! reports them as ignored rather than passed. Run them with
//! `cargo test --test replay_tools -- --ignored`; run that way they *fail*
//! when a tool is missing, because an environment that cannot perform the
//! cross-check must never be reported as a pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use shyhdl::replay::{Direction, Edge, ReplayEngine, StepMode, StopReason};
use shyhdl::waveform::{BitVec, ScopeKind, TimeUnit, Timescale, Value, WaveError, Waveform};

/// Device under test: one module, instantiated twice with different increments.
const DESIGN_V: &str = r#"`timescale 1ns/1ps

module blk #(parameter INC = 1) (
  input  wire clk,
  input  wire rst_n,
  output reg  [71:0] wide,
  output reg  flag
);
  always @(posedge clk or negedge rst_n)
    if (!rst_n) begin
      wide <= 72'd0;
      flag <= 1'b0;
    end else begin
      wide <= wide + INC;
      flag <= ~flag;
    end
endmodule

module dut (
  input  wire clk,
  input  wire rst_n,
  input  wire [3:0] nib,
  input  wire d,
  output wire [71:0] wa,
  output wire [71:0] wb,
  output wire tristate,
  output wire xq
);
  blk #(.INC(1)) u_outer (.clk(clk), .rst_n(rst_n), .wide(wa), .flag());
  blk #(.INC(3)) u_inner (.clk(clk), .rst_n(rst_n), .wide(wb), .flag());
  assign tristate = 1'bz;
  assign xq = nib[3];
endmodule
"#;

/// Testbench: 10 ns clock, asynchronous reset released at 12 ns, and stimulus
/// at 15 ns / 18 ns / 19 ns / 25 ns so that changes fall between edges.
const TB_V: &str = r#"`timescale 1ns/1ps

module tb_dump;
  reg clk;
  reg rst_n;
  reg [3:0] nib;
  reg d;
  wire [71:0] wa;
  wire [71:0] wb;
  wire tristate;
  wire xq;

  dut u_dut (.clk(clk), .rst_n(rst_n), .nib(nib), .d(d),
             .wa(wa), .wb(wb), .tristate(tristate), .xq(xq));

  initial begin
    clk = 1'b0;
    rst_n = 1'b0;
    nib = 4'h0;
    d = 1'b0;
  end

  always #5 clk = ~clk;

  initial begin
    #12 rst_n = 1'b1;
    #3  d = 1'b1;
    #3  nib = 4'hf;
    #1  d = 1'b0;
    #6  nib = 4'h5;
    #20 $finish;
  end

  initial begin
    $dumpfile("DUMPFILE_PLACEHOLDER");
    $dumpvars(0, tb_dump);
  end
endmodule
"#;

/// Which simulator produced a dump, for the few places where their documented
/// behaviour differs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// Four-state simulator: keeps `x` and `z`.
    Icarus,
    /// Two-state simulator: does not keep `x` and `z` (REQ-SIM-007).
    Verilator,
}

fn tool_available(tool: &str, version_arg: &str) -> bool {
    Command::new(tool)
        .arg(version_arg)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Fails when an explicitly requested cross-check cannot be performed.
///
/// These tests only run when asked for (`--ignored`), so a missing tool is a
/// failed check, not a skip: reporting "passed" for a comparison that never
/// happened is exactly the kind of evidence this project refuses.
fn require_tool(tool: &str, version_arg: &str) {
    assert!(
        tool_available(tool, version_arg),
        "{tool} is not on PATH; put the simulator toolchain on PATH and rerun \
         `cargo test --test replay_tools -- --ignored`"
    );
}

/// A private temporary directory for one test's build products.
fn workdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shyhdl_replay_tools_{}_{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary work directory must be creatable");
    dir
}

fn run(command: &mut Command) -> String {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {command:?}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "{command:?} failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    stdout
}

/// Writes the design and a testbench that dumps to `dump`.
fn write_sources(dir: &Path, dump: &Path) -> (PathBuf, PathBuf) {
    let design = dir.join("design.v");
    let testbench = dir.join("tb.v");
    std::fs::write(&design, DESIGN_V).expect("design must be writable");
    std::fs::write(
        &testbench,
        TB_V.replace("DUMPFILE_PLACEHOLDER", &dump.to_string_lossy()),
    )
    .expect("testbench must be writable");
    (design, testbench)
}

/// Converts nanoseconds into a dump's own time unit once, so assertions do not
/// have to borrow the engine they move.
struct Ticks {
    per_ns: u64,
}

impl Ticks {
    fn new(wave: &Waveform) -> Ticks {
        Ticks {
            per_ns: wave
                .ticks(Timescale::parse("1ns").unwrap())
                .expect("both tools dump at a precision that divides 1ns"),
        }
    }

    fn ns(&self, ns: u64) -> u64 {
        ns * self.per_ns
    }
}

fn wide72(value: u64) -> Value {
    Value::Bits(BitVec::from_msb_string(&format!("{value:072b}")))
}

fn bits(msb_first: &str) -> Value {
    Value::Bits(BitVec::from_msb_string(msb_first))
}

fn value_of(wave: &Waveform, path: &str, time: u64) -> Value {
    let id = wave
        .signal(path)
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    wave.value_at(id, time)
        .unwrap_or_else(|error| panic!("{path}: {error}"))
        .unwrap_or_else(|| panic!("{path} has no recorded value at {time}"))
}

/// Everything that must hold for either simulator's dump.
///
/// Times are expressed through [`Waveform::ticks`] so the assertions are about
/// the design's timing, not about the dump's time unit.
fn assert_common(dump: &Path, flavor: Flavor) {
    let wave = Waveform::load(dump).expect("the tool dump must load");

    // Both tools dump with the module's 1 ps precision, so 1 ns is 1000 ticks.
    // That is itself part of the check: a physical duration survives the trip
    // from the dump's timescale to an integer tick.
    let timescale = wave.timescale().expect("tool dumps declare a timescale");
    assert_eq!(
        (timescale.factor(), timescale.unit()),
        (1, TimeUnit::Pico),
        "the dump time unit comes from the design's precision"
    );
    let ticks = Ticks::new(&wave);
    let ns = |ns: u64| ticks.ns(ns);
    assert_eq!(ns(1), 1000);
    assert_eq!(wave.first_time(), 0);
    assert_eq!(wave.last_time(), ns(45));

    // Hierarchy as elaborated by the tool, with two instances of one module.
    let scopes: Vec<&str> = wave.scopes().iter().map(|s| s.path.as_str()).collect();
    for expected in [
        "tb_dump",
        "tb_dump.u_dut",
        "tb_dump.u_dut.u_outer",
        "tb_dump.u_dut.u_inner",
    ] {
        assert!(
            scopes.contains(&expected),
            "missing scope {expected} in {scopes:?}"
        );
    }
    for scope in wave.scopes() {
        assert_eq!(scope.kind, ScopeKind::Module, "{scope:?}");
    }
    let inner_scope = wave
        .scopes()
        .iter()
        .find(|scope| scope.path == "tb_dump.u_dut.u_inner")
        .unwrap();
    assert_eq!(inner_scope.parent.as_deref(), Some("tb_dump.u_dut"));

    // Values wider than u64, from a real elaborator.
    for path in [
        "tb_dump.wa",
        "tb_dump.wb",
        "tb_dump.u_dut.u_outer.wide",
        "tb_dump.u_dut.u_inner.wide",
    ] {
        let id = wave
            .signal(path)
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(wave.width(id).unwrap(), 72, "{path}");
    }
    assert_eq!(wave.width(wave.signal("tb_dump.clk").unwrap()).unwrap(), 1);

    // Two instances of one module are two signals with their own values:
    // u_outer increments by 1, u_inner by 3.
    let outer = wave.signal("tb_dump.u_dut.u_outer.wide").unwrap();
    let inner = wave.signal("tb_dump.u_dut.u_inner.wide").unwrap();
    assert_ne!(outer, inner, "instances must not be conflated");
    assert_eq!(wave.value_at(outer, ns(25)).unwrap(), Some(wide72(2)));
    assert_eq!(wave.value_at(inner, ns(25)).unwrap(), Some(wide72(6)));
    assert_eq!(wave.value_at(inner, ns(45)).unwrap(), Some(wide72(12)));
    assert_eq!(
        wave.changes(outer).unwrap(),
        vec![
            (ns(0), wide72(0)),
            (ns(15), wide72(1)),
            (ns(25), wide72(2)),
            (ns(35), wide72(3)),
            (ns(45), wide72(4)),
        ]
    );
    // The output port and the instance register carry the same activity.
    assert_eq!(
        wave.changes(wave.signal("tb_dump.wa").unwrap()).unwrap(),
        wave.changes(outer).unwrap()
    );
    if flavor == Flavor::Verilator {
        assert_eq!(
            wave.signal("tb_dump.wa").unwrap(),
            wave.signal("tb_dump.u_dut.wa").unwrap(),
            "Verilator reuses one identifier for a wire connected through a port"
        );
    }

    // The clock net is declared in every scope and reaches the instances
    // through ports: the tools reuse one identifier, so all those names are one
    // signal and a watch on it is a watch on the real net.
    let clocks = wave.find_signals("clk");
    assert_eq!(clocks.len(), 1, "one connected net: {clocks:?}");
    assert_eq!(clocks[0].id, wave.signal("tb_dump.clk").unwrap());
    for path in [
        "tb_dump.u_dut.clk",
        "tb_dump.u_dut.u_inner.clk",
        "tb_dump.u_dut.u_outer.clk",
    ] {
        assert_eq!(
            wave.signal(path).unwrap(),
            clocks[0].id,
            "{path} is the same net as tb_dump.clk"
        );
    }

    // `flag` is internal to each instance, so there really are two of them.
    let found = wave.find_signals("flag");
    let mut flags: Vec<&str> = found.iter().map(|info| info.path.as_str()).collect();
    flags.sort_unstable();
    assert_eq!(
        flags,
        vec!["tb_dump.u_dut.u_inner.flag", "tb_dump.u_dut.u_outer.flag"],
        "each instance keeps its own flag"
    );
    // And a partial name is never resolved by guessing (REQ-DBG-021).
    match wave.signal("flag") {
        Err(WaveError::SignalNotFound { path }) => assert_eq!(path, "flag"),
        other => panic!("a partial path must not be resolved, got {other:?}"),
    }

    // Module parameters are recorded once, at time zero.
    for (path, expected) in [
        ("tb_dump.u_dut.u_inner.INC", 3u64),
        ("tb_dump.u_dut.u_outer.INC", 1u64),
    ] {
        let id = wave
            .signal(path)
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(value_of(&wave, path, ns(0)).as_u64(path).unwrap(), expected);
        assert_eq!(
            wave.changes(id).unwrap().len(),
            1,
            "{path} is recorded once and never changes"
        );
    }
    assert_eq!(wave.find_signals("INC").len(), 2);

    // Stimulus that moves between clock edges.
    let d = wave.signal("tb_dump.d").unwrap();
    assert_eq!(
        wave.changes(d).unwrap(),
        vec![(ns(0), bits("0")), (ns(15), bits("1")), (ns(19), bits("0"))]
    );
    assert_eq!(value_of(&wave, "tb_dump.nib", ns(18)), bits("1111"));
    assert_eq!(value_of(&wave, "tb_dump.nib", ns(25)), bits("0101"));
    assert_eq!(
        wave.changes(wave.signal("tb_dump.xq").unwrap()).unwrap()[1].0,
        ns(18)
    );

    // `z` is only meaningful for the four-state simulator.
    let tristate = value_of(&wave, "tb_dump.tristate", ns(0));
    match flavor {
        Flavor::Icarus => {
            assert_eq!(tristate, bits("z"), "Icarus keeps the high-impedance z");
            match tristate.as_u64("tristate") {
                Err(WaveError::NotTwoState { bits, .. }) => assert_eq!(bits, "z"),
                other => panic!("z must not become a number, got {other:?}"),
            }
        }
        Flavor::Verilator => {
            assert!(
                tristate.as_u64("tristate").is_ok(),
                "Verilator is two-state and cannot report z (REQ-SIM-007), got {tristate}"
            );
        }
    }
}

/// Cursor semantics on tool output: real edges, a real grid, real watch hits.
fn assert_replay(dump: &Path) {
    let mut engine = ReplayEngine::new(
        Waveform::load(dump).expect("the tool dump must load"),
        StepMode::Clock {
            signal: "tb_dump.clk".to_string(),
            edge: Edge::Pos,
        },
    )
    .expect("clock mode must be valid");
    let ticks = Ticks::new(engine.waveform());
    let ns = |ns: u64| ticks.ns(ns);

    // clk is 0@0, 1@5, 0@10, 1@15, 0@20, 1@25, 0@30, 1@35, 0@40, 1@45 (ns).
    assert_eq!(engine.reference_bounds(), Some((ns(5), ns(45))));
    assert_eq!(engine.cursor().time, 0);
    assert_eq!(engine.step(Direction::Forward).cursor.time, ns(5));
    assert_eq!(engine.step(Direction::Forward).cursor.time, ns(15));
    assert_eq!(engine.step(Direction::Forward).cursor.time, ns(25));
    assert_eq!(engine.step(Direction::Backward).cursor.time, ns(15));

    let mut grid = ReplayEngine::new(
        Waveform::load(dump).expect("the tool dump must load"),
        StepMode::Interval {
            origin: 0,
            period: ns(10),
        },
    )
    .expect("interval mode must be valid");
    // AC-MVP-030 on a real dump: 13 -> 20 -> 10, no drift.
    assert_eq!(grid.seek(ns(13)).time, ns(13));
    assert_eq!(grid.step(Direction::Forward).cursor.time, ns(20));
    assert_eq!(grid.step(Direction::Backward).cursor.time, ns(10));

    // A 72-bit signal can be watched too, and its value survives in full.
    let inner = engine
        .waveform()
        .signal("tb_dump.u_dut.u_inner.wide")
        .unwrap();
    engine.reset();
    engine.watch(&[inner]).unwrap();
    let first = engine.continue_run(Direction::Forward);
    assert_eq!(first.cursor.time, ns(15));
    match &first.reason {
        StopReason::Watch(hits) => {
            assert_eq!(hits[0].previous, Some(wide72(0)));
            assert_eq!(hits[0].value, wide72(3));
            assert_eq!(hits[0].value.as_u64(&hits[0].path).unwrap(), 3);
        }
        other => panic!("expected a watch hit, got {other:?}"),
    }
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, ns(25));
    assert_eq!(engine.continue_run(Direction::Forward).cursor.time, ns(35));

    // `d` changes between edges, where a clock-sampled watch would miss it.
    let mut edge_watch = ReplayEngine::new(
        Waveform::load(dump).expect("the tool dump must load"),
        StepMode::Clock {
            signal: "tb_dump.clk".to_string(),
            edge: Edge::Pos,
        },
    )
    .expect("clock mode must be valid");
    let d = edge_watch.waveform().signal("tb_dump.d").unwrap();
    edge_watch.watch(&[d]).unwrap();
    assert_eq!(
        edge_watch.continue_run(Direction::Forward).cursor.time,
        ns(15)
    );
    // The next grid edge is 25, but d falls at 19: forward stepping must not
    // jump over it (REQ-DBG-034).
    let fallen = edge_watch.step(Direction::Forward);
    assert_eq!(fallen.cursor.time, ns(19));
    assert!(fallen.is_watch_hit());
}

#[test]
#[ignore = "requires iverilog and vvp on PATH; run: cargo test --test replay_tools -- --ignored"]
fn icarus_dump_cross_checks_four_state_wide_values_and_replay() {
    require_tool("iverilog", "-V");
    require_tool("vvp", "-V");

    let dir = workdir("icarus");
    let dump = dir.join("icarus.vcd");
    let (design, testbench) = write_sources(&dir, &dump);
    let image = dir.join("sim.vvp");
    run(Command::new("iverilog")
        .arg("-g2005")
        .arg("-o")
        .arg(&image)
        .arg(&testbench)
        .arg(&design));
    run(Command::new("vvp").arg(&image).current_dir(&dir));
    assert!(dump.exists(), "iverilog did not produce {dump:?}");

    assert_common(&dump, Flavor::Icarus);
    assert_replay(&dump);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires verilator on PATH; run: cargo test --test replay_tools -- --ignored"]
fn verilator_vcd_dump_cross_checks_wide_values_and_replay() {
    require_tool("verilator", "--version");

    let dir = workdir("verilator_vcd");
    let dump = dir.join("verilator.vcd");
    let (design, testbench) = write_sources(&dir, &dump);
    let obj = dir.join("obj_vcd");
    run(Command::new("verilator")
        .args([
            "--timing",
            "--binary",
            "--top-module",
            "tb_dump",
            "--trace-vcd",
            "-Wno-fatal",
            "-o",
            "sim_vcd",
            "--Mdir",
        ])
        .arg(&obj)
        .arg(&testbench)
        .arg(&design));
    run(Command::new(obj.join("sim_vcd")).current_dir(&dir));
    assert!(dump.exists(), "verilator did not produce {dump:?}");

    assert_common(&dump, Flavor::Verilator);
    assert_replay(&dump);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires verilator on PATH; run: cargo test --test replay_tools -- --ignored"]
fn verilator_fst_dump_cross_checks_the_same_recording() {
    require_tool("verilator", "--version");

    let dir = workdir("verilator_fst");
    let dump = dir.join("verilator.fst");
    let (design, testbench) = write_sources(&dir, &dump);
    let obj = dir.join("obj_fst");
    run(Command::new("verilator")
        .args([
            "--timing",
            "--binary",
            "--top-module",
            "tb_dump",
            "--trace-fst",
            "-Wno-fatal",
            "-o",
            "sim_fst",
            "--Mdir",
        ])
        .arg(&obj)
        .arg(&testbench)
        .arg(&design));
    run(Command::new(obj.join("sim_fst")).current_dir(&dir));
    assert!(dump.exists(), "verilator did not produce {dump:?}");

    assert_common(&dump, Flavor::Verilator);
    assert_replay(&dump);
    let _ = std::fs::remove_dir_all(&dir);
}
