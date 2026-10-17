//! Contract tests for `compile_testbench`: what the generator accepts, what it
//! rejects with a source location, and what the emitted Verilog-2005 must look
//! like.
//!
//! These tests do not need the frontend: the DUT side is a hand-written Verilog
//! fixture plus the matching [`DesignInfo`], so a failure here is a generator
//! failure and never a compiler-frontend failure. The generated testbench is
//! really compiled and run by Icarus Verilog where the test needs runtime
//! evidence (a missing tool fails the test with a clear message instead of being
//! skipped).

use std::path::{Path, PathBuf};
use std::process::Command;

use shyhdl::metadata::ValueType;
use shyhdl::testbench::{compile_testbench, DesignInfo, ModulePort, PortDirection};

fn uint(width: u32) -> ValueType {
    ValueType::UInt { width }
}

/// A small DUT with the port shapes the generator has to handle: inputs,
/// outputs, a clock and a packed struct.
fn design() -> DesignInfo {
    DesignInfo::new(
        "Counter",
        "Counter",
        vec![
            ModulePort::new("clk", "clk", PortDirection::In, ValueType::Bool),
            ModulePort::new("rst_n", "rst_n", PortDirection::In, ValueType::Bool),
            ModulePort::new("enable", "enable", PortDirection::In, ValueType::Bool),
            ModulePort::new("d", "d", PortDirection::In, uint(8)),
            ModulePort::new("q", "q", PortDirection::Out, uint(8)),
        ],
    )
}

const DUT: &str = r#"
module Counter(input clk, input rst_n, input enable, input [7:0] d, output reg [7:0] q);
  always @(posedge clk or negedge rst_n)
    if (!rst_n) q <= 8'd0;
    else if (enable) q <= d;
endmodule
"#;

fn compile(source: &str) -> shyhdl::Result<shyhdl::testbench::TestbenchArtifact> {
    compile_testbench(source, "counter.shy", "smoke", &design())
}

/// Icarus Verilog from `PATH`; a missing tool is a hard failure (REQ-TEST-009).
fn iverilog() -> PathBuf {
    find_in_path("iverilog").unwrap_or_else(|| {
        panic!(
            "iverilog was not found on PATH; these tests need a real Verilog-2005 tool \
             (no result is faked and nothing is skipped)"
        )
    })
}

fn vvp() -> PathBuf {
    find_in_path("vvp").unwrap_or_else(|| panic!("vvp was not found on PATH"))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn artifact_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-artifacts")
        .join(name);
    std::fs::create_dir_all(&dir).expect("create artifact directory");
    dir
}

#[test]
fn generated_testbench_is_verilog_2005_and_runs() {
    // Exercises the wrappers, a fixed-count nested loop, an assertion, a
    // non-blocking assignment and both explicit-end and automatic end paths.
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    clock!(dut.clk, period = ns(10), initial = false);
    reset!(dut.rst_n, active_low, hold = ns(20));
    initial! {
        dut.enable = false;
        dut.d = 0x00;
        // Reset is held low across several clock edges; the output must stay
        // cleared during it (AC-MVP-006).
        wait(posedge(dut.clk));
        sample_after(posedge(dut.clk), ps(1));
        assert_eq!(dut.q, 0x00);
        wait_until(dut.rst_n == true);
        dut.enable = true;
        nb_assign!(dut.d, 0x2a);
        for i in 0..4 {
            wait(posedge(dut.clk));
            sample_after(posedge(dut.clk), ps(1));
            assert_eq!(dut.q, 0x2a);
            pass!();
        }
        assert!(!(dut.q == 0x00));
        assert_eq!(!0x55u8, 0xaau8);
        finish!();
    }
}
"##;
    let artifact = compile(source).expect("generate");
    let dir = artifact_dir("gen_smoke");
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("counter.v");
    std::fs::write(&tb, &artifact.verilog).unwrap();
    std::fs::write(&dut, DUT).unwrap();

    let vvp_file = dir.join("sim.vvp");
    let output = Command::new(iverilog())
        .arg("-g2005")
        .arg("-o")
        .arg(&vvp_file)
        .arg(&dut)
        .arg(&tb)
        .output()
        .expect("run iverilog");
    assert!(
        output.status.success(),
        "iverilog rejected the generated testbench:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = Command::new(vvp())
        .arg(&vvp_file)
        .output()
        .expect("run vvp");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "vvp failed: {}\n{}",
        String::from_utf8_lossy(&run.stderr),
        stdout
    );
    assert!(
        stdout.contains("SHYHDL_PASS test=smoke case=finish"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=smoke pass=5 fail=0 markers=4"),
        "{stdout}"
    );
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
}

#[test]
fn bitwise_not_and_bitwise_ops_follow_rust_semantics() {
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    let mut bits: uint<8> = 0x0f;
    initial! {
        assert_eq!(!0x55u8, 0xaau8);
        assert_eq!(!bits, 0xf0u8);
        assert_eq!(0x0fu8 & 0x3cu8, 0x0cu8);
        assert_eq!(0x0fu8 | 0x30u8, 0x3fu8);
        assert_eq!(0x0fu8 ^ 0xffu8, 0xf0u8);
        assert_eq!(0x01u8 << 3, 0x08u8);
        assert_eq!(0x80u8 >> 4, 0x08u8);
        finish!();
    }
}
"##;
    let artifact = compile(source).expect("generate");
    assert!(
        artifact.verilog.contains("(~(8'd85))"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("(~(bits))"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("(8'd15) & (8'd60)"),
        "{}",
        artifact.verilog
    );

    // Real runtime evidence: Icarus runs the same testbench and must not report
    // a failure.
    let dir = artifact_dir("gen_bitwise");
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("counter.v");
    std::fs::write(&tb, &artifact.verilog).unwrap();
    std::fs::write(&dut, DUT).unwrap();
    let vvp_file = dir.join("sim.vvp");
    let output = Command::new(iverilog())
        .arg("-g2005")
        .arg("-o")
        .arg(&vvp_file)
        .arg(&dut)
        .arg(&tb)
        .output()
        .expect("run iverilog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = Command::new(vvp())
        .arg(&vvp_file)
        .output()
        .expect("run vvp");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
    assert!(
        stdout.contains("SHYHDL_PASS test=smoke case=finish"),
        "{stdout}"
    );
}

#[test]
fn time_values_convert_to_exact_decimal_delays() {
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        delay(ns(10));
        delay(ps(1));
        delay(us(1));
        delay(ns(0));
        finish!();
    }
}
"##;
    let artifact = compile(source).expect("generate");
    assert!(artifact.verilog.contains("#10;"), "{}", artifact.verilog);
    assert!(artifact.verilog.contains("#0.001;"), "{}", artifact.verilog);
    assert!(
        artifact.verilog.contains("#1000.000;") || artifact.verilog.contains("#1000;"),
        "{}",
        artifact.verilog
    );
    assert!(artifact.verilog.contains("#0;"), "{}", artifact.verilog);

    // A finer timeunit converts the other way, still exactly.
    let fine = r#"
#[hardware_test(timeunit = "1ps", precision = "1ps")]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        delay(ns(10));
        delay(ps(1));
        finish!();
    }
}
"#;
    let artifact = compile(fine).expect("generate");
    assert!(
        artifact.verilog.contains("`timescale 1ps/1ps"),
        "{}",
        artifact.verilog
    );
    assert!(artifact.verilog.contains("#10000;"), "{}", artifact.verilog);
    assert!(artifact.verilog.contains("#1;"), "{}", artifact.verilog);

    // A delay below the precision is refused instead of being rounded silently.
    let too_fine = r#"
#[hardware_test(timeunit = "1ns", precision = "1ps")]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        delay(fs(1));
        finish!();
    }
}
"#;
    let error = compile(too_fine).unwrap_err().to_string();
    assert!(error.contains("cannot be represented"), "{error}");
}

#[test]
fn generated_clock_frequency_is_exact_and_collected() {
    // `gen_clk` is an ordinary call; 100 MHz is an exact 10 ns period, and the
    // clock is identified by its full path in the recording.
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    gen_clk(dut.clk, mhz(100));
    initial! {
        dut.enable = false;
        dut.d = 0x00;
        wait(posedge(dut.clk));
        finish!();
    }
}
"##;
    let artifact = compile(source).expect("generate");
    assert!(
        artifact.verilog.contains("#5 dut_clk = 1'b1;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("#5 dut_clk = 1'b0;"),
        "{}",
        artifact.verilog
    );
    assert_eq!(artifact.clocks.len(), 1);
    assert_eq!(artifact.clocks[0].var, "dut");
    assert_eq!(
        artifact.clocks[0].signal,
        format!("TOP.{}.u_dut.clk", artifact.top)
    );

    // The capitalised aliases read like the unit they name.
    let alias = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    gen_clk(dut.clk, MHz(100));
    initial! {
        dut.d = 0x00;
        wait(posedge(dut.clk));
        finish!();
    }
}
"##;
    assert_eq!(compile(alias).expect("generate").clocks, artifact.clocks);

    // An asymmetric clock keeps the phase lengths the test asked for: starting
    // low, the low phase lasts `low` and the high phase lasts `high`.
    let asymmetric = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    clock!(dut.clk, high = ns(6), low = ns(4));
    initial! {
        dut.d = 0x00;
        wait(posedge(dut.clk));
        finish!();
    }
}
"##;
    let artifact = compile(asymmetric).expect("generate");
    let rising = artifact
        .verilog
        .find("#4 dut_clk = 1'b1;")
        .expect("low phase of 4ns");
    let falling = artifact
        .verilog
        .find("#6 dut_clk = 1'b0;")
        .expect("high phase of 6ns");
    assert!(rising < falling, "{}", artifact.verilog);

    let starts_high = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    clock!(dut.clk, high = ns(6), low = ns(4), initial = true);
    initial! {
        dut.d = 0x00;
        wait(negedge(dut.clk));
        finish!();
    }
}
"##;
    let artifact = compile(starts_high).expect("generate");
    let falling = artifact
        .verilog
        .find("#6 dut_clk = 1'b0;")
        .expect("high phase first");
    let rising = artifact
        .verilog
        .find("#4 dut_clk = 1'b1;")
        .expect("then the low phase");
    assert!(falling < rising, "{}", artifact.verilog);
}

#[test]
fn a_frequency_that_is_not_exact_is_rejected_with_a_location() {
    let cases: &[(&str, &str)] = &[
        ("not_divisible", "gen_clk(dut.clk, mhz(3))"),
        ("zero", "gen_clk(dut.clk, hz(0))"),
        ("no_unit", "gen_clk(dut.clk, 100)"),
        ("not_a_literal", "gen_clk(dut.clk, mhz(1 + 1))"),
        ("wrong_unit", "gen_clk(dut.clk, period = ns(10))"),
        ("too_many", "gen_clk(dut.clk, mhz(100), initial = false)"),
        // 200 THz is a 5 fs period: odd, which a 50% duty clock cannot express.
        ("odd_period", "gen_clk(dut.clk, hz(200000000000000))"),
    ];
    for (name, call) in cases {
        let source = format!(
            "#[hardware_test]\nfn smoke() {{\n    let mut dut = Counter::new();\n    {call};\n    initial! {{ dut.d = 0; finish!(); }}\n}}\n"
        );
        let error = compile(&source).unwrap_err();
        match &error {
            shyhdl::Error::Unsupported { span, .. } => {
                assert_eq!(span.file, "counter.shy", "{name}: {error}");
                assert!(span.line >= 4, "{name}: {error}");
            }
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
        let text = error.to_string();
        match *name {
            "not_divisible" => {
                assert!(text.contains("no exact whole-femtosecond period"), "{text}");
                assert!(text.contains("clock!("), "{text}");
                assert!(
                    !text.contains("finer precision"),
                    "precision cannot fix an inexact frequency: {text}"
                );
            }
            "odd_period" => {
                assert!(
                    text.contains("cannot be split into two equal phases"),
                    "{text}"
                );
                // The suggestion must be a call the language actually has.
                assert!(
                    text.contains("clock!(") && text.contains("high = fs(..)"),
                    "{text}"
                );
            }
            "zero" => assert!(text.contains("frequency of zero"), "{text}"),
            "no_unit" => assert!(text.contains("explicit unit"), "{text}"),
            "not_a_literal" => assert!(text.contains("integer literal"), "{text}"),
            "too_many" => assert!(
                text.contains("takes a DUT clock port and a frequency"),
                "{text}"
            ),
            _ => {}
        }
    }
}

#[test]
fn zero_clock_phases_and_overflowing_times_are_rejected() {
    // A wrapper is declared at the test level, a delay inside a process.
    let cases: &[(&str, &str, bool, &str)] = &[
        (
            "zero_period",
            "clock!(dut.clk, period = ns(0));",
            true,
            "period of zero",
        ),
        (
            "zero_high",
            "clock!(dut.clk, high = ns(0), low = ns(4));",
            true,
            "phase of zero",
        ),
        (
            "zero_low",
            "clock!(dut.clk, high = ns(4), low = ns(0));",
            true,
            "phase of zero",
        ),
        (
            "overflowing_period",
            "clock!(dut.clk, period = s(18446744073709551615));",
            true,
            "overflows 64 bits",
        ),
        (
            "overflowing_delay",
            "delay(s(18446744073709551615));",
            false,
            "overflows 64 bits",
        ),
    ];
    for (name, statement, at_test_level, expected) in cases {
        let source = if *at_test_level {
            format!(
                "#[hardware_test]\nfn smoke() {{\n    let mut dut = Counter::new();\n    {statement}\n    initial! {{ dut.d = 0x00; finish!(); }}\n}}\n"
            )
        } else {
            format!(
                "#[hardware_test]\nfn smoke() {{\n    let mut dut = Counter::new();\n    initial! {{\n        dut.d = 0x00;\n        {statement}\n        finish!();\n    }}\n}}\n"
            )
        };
        let error = compile(&source).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(expected), "{name}: {text}");
        match &error {
            shyhdl::Error::Unsupported { span, .. } => assert!(span.line >= 4, "{name}: {error}"),
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }
    // A zero delay is still the raw `#0` of the language; only a clock phase has
    // to advance time.
    let zero_delay = r#"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        dut.d = 0x00;
        delay(ns(0));
        finish!();
    }
}
"#;
    assert!(compile(zero_delay)
        .expect("generate")
        .verilog
        .contains("#0;"));
}

#[test]
fn a_wrapper_and_the_test_cannot_drive_the_same_signal_in_either_order() {
    // The clock wrapper first: the assignment is rejected where it is written.
    let wrapper_first = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    gen_clk(dut.clk, mhz(100));
    initial! {
        dut.clk = false;
        dut.d = 0x00;
        finish!();
    }
}
"##;
    let error = compile(wrapper_first).unwrap_err().to_string();
    assert!(error.contains("clock"), "{error}");

    // The assignment first: the conflict must still be found, not silently
    // accepted because the wrapper came later.
    let assignment_first = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        dut.clk = false;
        dut.d = 0x00;
        finish!();
    }
    clock!(dut.clk, period = ns(10));
}
"##;
    let error = compile(assignment_first).unwrap_err().to_string();
    assert!(error.contains("already driven"), "{error}");
    assert!(error.contains("dut_clk"), "{error}");

    // The same for a reset wrapper.
    let reset_with_assignment = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        dut.rst_n = false;
        dut.d = 0x00;
        finish!();
    }
    reset!(dut.rst_n, active_low, hold = ns(20));
}
"##;
    let error = compile(reset_with_assignment).unwrap_err().to_string();
    assert!(
        error.contains("already driven") && error.contains("reset wrapper"),
        "{error}"
    );
}

/// A DUT with signed and unsigned ports, a narrower unsigned port and a packed
/// struct whose `tag` field is signed.
fn signed_design() -> DesignInfo {
    DesignInfo::new(
        "Signed",
        "Signed",
        vec![
            ModulePort::new("s", "s", PortDirection::In, ValueType::SInt { width: 8 }),
            ModulePort::new(
                "count",
                "count",
                PortDirection::In,
                ValueType::SInt { width: 8 },
            ),
            ModulePort::new("u", "u", PortDirection::In, uint(8)),
            ModulePort::new("narrow", "narrow", PortDirection::In, uint(4)),
            ModulePort::new(
                "w",
                "w",
                PortDirection::In,
                ValueType::Struct {
                    name: "Word".into(),
                    fields: vec![
                        shyhdl::metadata::TypeField {
                            name: "op".into(),
                            ty: uint(4),
                        },
                        shyhdl::metadata::TypeField {
                            name: "addr".into(),
                            ty: uint(32),
                        },
                        shyhdl::metadata::TypeField {
                            name: "flag".into(),
                            ty: ValueType::Bool,
                        },
                        shyhdl::metadata::TypeField {
                            name: "tag".into(),
                            ty: ValueType::SInt { width: 7 },
                        },
                    ],
                },
            ),
            ModulePort::new(
                "s_out",
                "s_out",
                PortDirection::Out,
                ValueType::SInt { width: 8 },
            ),
            ModulePort::new("u_out", "u_out", PortDirection::Out, uint(8)),
        ],
    )
}

const SIGNED_DUT: &str = r#"
module Signed(
    input signed [7:0] s,
    input signed [7:0] count,
    input [7:0] u,
    input [3:0] narrow,
    input [43:0] w,
    output signed [7:0] s_out,
    output [7:0] u_out
);
  assign s_out = s + count;
  assign u_out = u >> 1;
endmodule
"#;

const SIGNED_TEST: &str = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut x: int<8> = -1;
    let mut u: uint<8> = 0xff;
    initial! {
        dut.s = -8;
        dut.count = 1;
        dut.u = 0xff;
        dut.narrow = 0;
        dut.w = 0;
        dut.w.tag = -1;
        // Combinational DUT outputs settle after the drive, one precision tick
        // later; reading them in the same time step would be a race (and would
        // read X under a four-state tool).
        delay(ps(1));
        assert_eq!(dut.s, -8);
        assert!(dut.s < 0);
        assert_eq!(dut.w.tag, -1);
        assert_eq!(x >> 1, -1);
        assert_eq!(u >> 1, 0x7f);
        assert_eq!(dut.u_out, 0x7f);
        assert_eq!(dut.s_out, -7);
        x = x + 1;
        assert_eq!(x, 0);
        pass!();
        finish!();
    }
}
"##;

#[test]
fn signed_values_are_declared_and_lowered_as_two_complement() {
    let artifact = compile_testbench(SIGNED_TEST, "signed.shy", "smoke", &signed_design())
        .expect("generate the signed testbench");
    let verilog = &artifact.verilog;
    // A `int<N>` signal is a signed Verilog signal, for locals and for the nets
    // the testbench declares for the DUT ports.
    assert!(verilog.contains("reg signed [7:0] x;"), "{verilog}");
    assert!(verilog.contains("reg [7:0] u;"), "{verilog}");
    assert!(verilog.contains("reg signed [7:0] dut_s;"), "{verilog}");
    assert!(verilog.contains("reg signed [7:0] dut_count;"), "{verilog}");
    assert!(verilog.contains("reg [7:0] dut_u;"), "{verilog}");
    assert!(
        verilog.contains("wire signed [7:0] dut_s_out;"),
        "{verilog}"
    );
    assert!(verilog.contains("wire [7:0] dut_u_out;"), "{verilog}");
    // Negative literals are written the way Verilog accepts them.
    assert!(verilog.contains("dut_s = -8'sd8;"), "{verilog}");
    assert!(verilog.contains("dut_count = 8'sd1;"), "{verilog}");
    // A signed struct field is assigned through the plain slice and read back with
    // `$signed(...)`, because a Verilog part-select is unsigned.
    assert!(verilog.contains("dut_w[6:0] = -7'sd1;"), "{verilog}");
    assert!(verilog.contains("($signed(dut_w[6:0]))"), "{verilog}");
    // Comparisons and shifts keep two's complement semantics.
    assert!(verilog.contains("(dut_s) !== (-8'sd8)"), "{verilog}");
    assert!(verilog.contains("(dut_s) < (8'sd0)"), "{verilog}");
    assert!(verilog.contains("((x) >>> (1'd1))"), "{verilog}");
    assert!(verilog.contains("((u) >> (1'd1))"), "{verilog}");
    assert!(verilog.contains("((x) + (8'sd1))"), "{verilog}");

    // The same testbench is accepted by a real Verilog-2005 tool and its runtime
    // results match two's complement wrap-around.
    let dir = artifact_dir("gen_signed");
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("signed.v");
    std::fs::write(&tb, verilog).unwrap();
    std::fs::write(&dut, SIGNED_DUT).unwrap();
    let vvp_file = dir.join("sim.vvp");
    let output = Command::new(iverilog())
        .arg("-g2005")
        .arg("-o")
        .arg(&vvp_file)
        .arg(&dut)
        .arg(&tb)
        .output()
        .expect("run iverilog");
    assert!(
        output.status.success(),
        "iverilog rejected the signed testbench:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = Command::new(vvp())
        .arg(&vvp_file)
        .output()
        .expect("run vvp");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
    assert!(
        stdout.contains("SHYHDL_PASS test=smoke case=finish"),
        "{stdout}"
    );
}

#[test]
fn signed_contexts_are_exact_width_while_unsigned_widening_is_unchanged() {
    // The unsigned path keeps its documented rule: a narrower unsigned value may
    // be assigned to a wider one (Verilog zero-extends it).
    let unsigned = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut u16: uint<16> = 0;
    initial! {
        dut.s = 0;
        dut.count = 0;
        dut.u = 0xff;
        dut.narrow = 0;
        dut.w = 0;
        u16 = dut.u;
        finish!();
    }
}
"##;
    let artifact =
        compile_testbench(unsigned, "signed.shy", "smoke", &signed_design()).expect("generate");
    assert!(
        artifact.verilog.contains("u16 = dut_u;"),
        "{}",
        artifact.verilog
    );

    // The signed path does not: `int<N>` arithmetic wraps at N bits, so an
    // implicit extension would let Verilog size the expression at the assignment's
    // width and produce a value the type says cannot exist. A literal is exempt
    // because it takes the target's width.
    let accepted = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut wide: int<16> = 0;
    let mut same: int<8> = 0;
    initial! {
        dut.s = 127;
        dut.count = 127;
        dut.u = 0;
        dut.narrow = 0;
        dut.w = 0;
        same = dut.s + dut.count;
        wide = 254;
        assert_eq!(wide, 254);
        finish!();
    }
}
"##;
    let artifact =
        compile_testbench(accepted, "signed.shy", "smoke", &signed_design()).expect("generate");
    assert!(
        artifact.verilog.contains("same = ((dut_s) + (dut_count));"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("wide = 16'sd254;"),
        "{}",
        artifact.verilog
    );

    // Every way a signed width could grow silently is refused with a location.
    let rejected: &[(&str, &str)] = &[
        ("operand", "wide = dut.s + dut.count;"),
        ("value", "wide = dut.s;"),
        ("negation", "wide = -dut.s;"),
        ("cast", "wide = dut.s as int<8>;"),
    ];
    for (name, statement) in rejected {
        let source = format!(
            "#[hardware_test]\nfn smoke() {{\n    let mut dut = Signed::new();\n    let mut wide: int<16> = 0;\n    initial! {{\n        dut.s = 0;\n        dut.count = 0;\n        dut.u = 0;\n        dut.narrow = 0;\n        dut.w = 0;\n        {statement}\n        finish!();\n    }}\n}}\n"
        );
        let error =
            compile_testbench(&source, "signed.shy", "smoke", &signed_design()).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("signed value keeps its own width")
                || text.contains("signedness is never converted implicitly"),
            "{name}: {text}"
        );
        match &error {
            shyhdl::Error::Unsupported { span, .. } => assert!(span.line >= 6, "{name}: {error}"),
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }
}

#[test]
fn literal_casts_take_the_target_width_and_are_range_checked() {
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut s: int<8> = 0;
    let mut u: uint<4> = 0;
    initial! {
        dut.s = 0;
        dut.count = 0;
        dut.u = 0xff;
        dut.narrow = 0;
        dut.w = 0;
        s = 1 as int<8>;
        s = (1) as int<8>;
        s = ((1)) as int<8>;
        s = -1 as int<8>;
        s = (-1) as int<8>;
        s = 255u8 as int<8>;
        u = 1 as uint<4>;
        finish!();
    }
}
"##;
    let artifact =
        compile_testbench(source, "signed.shy", "smoke", &signed_design()).expect("generate");
    let verilog = &artifact.verilog;
    // An unsuffixed literal takes the target's width and signedness, parentheses
    // and a negation included.
    for expected in ["s = 8'sd1;", "s = -8'sd1;", "u = 4'd1;"] {
        assert!(
            verilog.contains(expected),
            "missing `{expected}` in:\n{verilog}"
        );
    }
    // A suffixed literal is a value of its own type, so it keeps the source width
    // rule and the cast only reinterprets the sign.
    assert!(verilog.contains("s = $signed(8'd255);"), "{verilog}");
    // A literal outside the target's range is refused, never wrapped.
    for (name, statement, expected) in [
        (
            "signed_too_large",
            "s = 255 as int<8>;",
            "does not fit a signed 8 bit value",
        ),
        (
            "signed_too_small",
            "s = -129 as int<8>;",
            "does not fit a signed 8 bit value",
        ),
        (
            "unsigned_too_large",
            "u = 16 as uint<4>;",
            "does not fit in 4 bits",
        ),
    ] {
        let source = format!(
            "#[hardware_test]\nfn smoke() {{\n    let mut dut = Signed::new();\n    let mut s: int<8> = 0;\n    let mut u: uint<4> = 0;\n    initial! {{\n        dut.s = 0;\n        dut.count = 0;\n        dut.u = 0;\n        dut.narrow = 0;\n        dut.w = 0;\n        {statement}\n        finish!();\n    }}\n}}\n"
        );
        let error =
            compile_testbench(&source, "signed.shy", "smoke", &signed_design()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(expected), "{name}: {text}");
        match &error {
            shyhdl::Error::Unsupported { span, .. } => assert!(span.line >= 6, "{name}: {error}"),
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }
}

#[test]
fn a_suffixed_literal_is_a_known_type_not_a_flexible_literal() {
    // `u8` names a type (`uint<8>`), so the literal is a typed value: it is range
    // checked at its own width and then adapted like any other unsigned value.
    // The documented unsigned widening still applies; narrowing and a sign change
    // do not, and negating it stays an unsigned negation (an error).
    let accepted = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut u16: uint<16> = 0;
    let mut s8: int<8> = 0;
    initial! {
        dut.s = 0;
        dut.count = 0;
        dut.u = 0;
        dut.narrow = 0;
        dut.w = 0;
        u16 = 0xffu8;
        // A legacy `iN` literal keeps fitting the context exactly as before.
        s8 = 1i8;
        dut.s = -1i8;
        finish!();
    }
}
"##;
    let artifact = compile_testbench(accepted, "signed.shy", "smoke", &signed_design())
        .expect("a `u8` literal may widen into a `uint<16>` target");
    // The value keeps its own width and Verilog zero-extends it on assignment,
    // exactly as it does for a `uint<8>` value widened into `uint<16>`.
    for expected in ["u16 = 8'd255;", "s8 = 8'sd1;", "dut_s = -8'sd1;"] {
        assert!(
            artifact.verilog.contains(expected),
            "missing `{expected}` in:\n{}",
            artifact.verilog
        );
    }

    let cases: &[(&str, &str, &str)] = &[
        (
            "unsigned_narrow",
            "dut.narrow = 1u8;",
            "truncation must be explicit",
        ),
        (
            "signed_mismatch",
            "dut.s = 1u8;",
            "signedness is never converted implicitly",
        ),
        (
            "source_width_range",
            "u16 = 256u8;",
            "does not fit its declared width of 8 bits",
        ),
        // A negated `u8` is still an unsigned value, so it never slides into a
        // signed target, with or without parentheses.
        (
            "negative_suffixed",
            "dut.s = -1u8;",
            "signedness is never converted implicitly",
        ),
        (
            "negative_suffixed_paren",
            "dut.s = -(1u8);",
            "signedness is never converted implicitly",
        ),
        (
            "negative_suffixed_group",
            "dut.s = -((1u8));",
            "signedness is never converted implicitly",
        ),
        // Where no signed target asks for it first, negating the unsigned value
        // itself is what is refused.
        (
            "negative_suffixed_unsigned_target",
            "u16 = -1u8;",
            "unary minus on an unsigned value is not supported",
        ),
        // The legacy `iN` spelling keeps its own-width range check with no
        // context, so it is not silently re-fitted either.
        (
            "legacy_iN_range",
            "assert_eq!(256i8, 0);",
            "does not fit its declared width of 8 bits",
        ),
    ];
    for (name, statement, expected) in cases {
        let source = format!(
            "#[hardware_test]\nfn smoke() {{\n    let mut dut = Signed::new();\n    let mut u16: uint<16> = 0;\n    initial! {{\n        dut.s = 0;\n        dut.count = 0;\n        dut.u = 0;\n        dut.narrow = 0;\n        dut.w = 0;\n        {statement}\n        finish!();\n    }}\n}}\n"
        );
        let error =
            compile_testbench(&source, "signed.shy", "smoke", &signed_design()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(expected), "{name}: {text}");
        match &error {
            shyhdl::Error::Unsupported { span, .. } => assert!(span.line >= 6, "{name}: {error}"),
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }
}

#[test]
fn a_bool_producer_never_slides_into_a_signed_target() {
    // A `bool` literal, a comparison and a logical operator all produce a one bit
    // `bool` whose text is legal Verilog, so without a check at the single
    // expression exit they would be stored into a `int<N>` as if they were an
    // integer. Each case must be a located error instead.
    let cases: &[(&str, &str, bool, &str)] = &[
        (
            "bool_literal_init",
            "let mut y: int<8> = true;",
            true,
            "is a `bool`",
        ),
        ("comparison", "x = dut.u < 2;", false, "is a `bool`"),
        (
            "equality_nba",
            "nb_assign!(x, dut.u == 0);",
            false,
            "is a `bool`",
        ),
        ("logical_not", "x = !dut.u[0];", false, "is a `bool`"),
        (
            "bitwise_bools",
            "x = dut.u[0] & dut.u[1];",
            false,
            "is a `bool`",
        ),
        (
            "signed_comparison",
            "x = dut.s == dut.count;",
            false,
            "is a `bool`",
        ),
        ("packed_struct", "x = dut.w;", false, "packed struct"),
    ];
    for (name, statement, at_test_level, expected) in cases {
        let body = if *at_test_level {
            format!(
                "    let mut dut = Signed::new();\n    {statement}\n    initial! {{ dut.s = 0; dut.count = 0; dut.u = 0; dut.narrow = 0; dut.w = 0; finish!(); }}\n"
            )
        } else {
            format!(
                "    let mut dut = Signed::new();\n    let mut x: int<8> = 0;\n    initial! {{\n        dut.s = 0;\n        dut.count = 0;\n        dut.u = 0;\n        dut.narrow = 0;\n        dut.w = 0;\n        {statement}\n        finish!();\n    }}\n"
            )
        };
        let source = format!("#[hardware_test]\nfn smoke() {{\n{body}}}\n");
        let error =
            compile_testbench(&source, "signed.shy", "smoke", &signed_design()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(expected), "{name}: {text}");
        assert!(
            text.contains("a signed 8 bit"),
            "{name}: the diagnostic must name the signed target: {text}"
        );
        match &error {
            shyhdl::Error::Unsupported { span, .. } => {
                assert_eq!(span.file, "signed.shy", "{name}: {error}");
                assert!(span.line >= 3, "{name}: {error}");
            }
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }

    // A `bool` stored into an unsigned or `bool` target keeps its old behavior:
    // only the signed path gained the exact type rule.
    let unchanged = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut flag: bool = false;
    let mut bits: uint<8> = 0;
    initial! {
        dut.s = 0;
        dut.count = 0;
        dut.u = 0;
        dut.narrow = 0;
        dut.w = 0;
        flag = dut.u < 2;
        bits = true;
        finish!();
    }
}
"##;
    let artifact = compile_testbench(unchanged, "signed.shy", "smoke", &signed_design())
        .expect("the unsigned and bool paths are unchanged");
    // The comparison result stays a `bool`, and the literal takes the operand's
    // width; only a signed target would refuse the store.
    assert!(
        artifact.verilog.contains("flag = ((dut_u) < (8'd2));"),
        "{}",
        artifact.verilog
    );
}

#[test]
fn signed_misuse_is_rejected_with_a_location() {
    // `at_test_level` statements are written at the top of the test; the others
    // go inside `initial!`.
    let cases: &[(&str, &str, bool, &str)] = &[
        (
            "negative_to_unsigned",
            "dut.u = -1;",
            false,
            "needs a signed target",
        ),
        (
            "mixed_comparison",
            "assert!(dut.s < dut.u);",
            false,
            "signedness is never converted implicitly",
        ),
        (
            "mixed_equality",
            "assert_eq!(dut.s, dut.u);",
            false,
            "signedness is never converted implicitly",
        ),
        (
            "signed_into_unsigned",
            "dut.u = x;",
            false,
            "signedness is never converted implicitly",
        ),
        (
            "signed_shift_count",
            "assert_eq!(dut.s >> dut.count, 0);",
            false,
            "shift count must be unsigned",
        ),
        (
            "cast_changes_width",
            "assert_eq!(dut.s as uint<4>, 0);",
            false,
            "cast must keep the width",
        ),
        (
            "cast_to_bool",
            "assert!(dut.s as bool);",
            false,
            "cast to `bool`",
        ),
        (
            "signed_range_high",
            "let mut y: int<4> = 8;",
            true,
            "does not fit a signed 4 bit value",
        ),
        (
            "signed_range_low",
            "let mut y: int<4> = -9;",
            true,
            "does not fit a signed 4 bit value",
        ),
        (
            "negate_unsigned",
            "let mut y: uint<8> = 0; y = -y;",
            false,
            "unary minus on an unsigned value",
        ),
        (
            "different_widths",
            "assert_eq!(dut.s, dut.narrow);",
            false,
            "two operands of the same width",
        ),
    ];
    for (name, statement, at_test_level, expected) in cases {
        let body = if *at_test_level {
            format!(
                "    let mut dut = Signed::new();\n    let mut x: int<8> = 0;\n    {statement}\n    initial! {{ dut.s = 0; dut.count = 0; dut.u = 0; dut.narrow = 0; dut.w = 0; finish!(); }}\n"
            )
        } else {
            format!(
                "    let mut dut = Signed::new();\n    let mut x: int<8> = 0;\n    initial! {{\n        dut.s = 0;\n        dut.count = 0;\n        dut.u = 0;\n        dut.narrow = 0;\n        dut.w = 0;\n        {statement}\n        finish!();\n    }}\n"
            )
        };
        let source = format!("#[hardware_test]\nfn smoke() {{\n{body}}}\n");
        let error =
            compile_testbench(&source, "signed.shy", "smoke", &signed_design()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(expected), "{name}: {text}");
        match &error {
            shyhdl::Error::Unsupported { span, .. } => {
                assert_eq!(span.file, "signed.shy", "{name}: {error}");
                assert!(span.line >= 4, "{name}: {error}");
            }
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }
}

#[test]
fn bare_slices_stay_unsigned_and_casts_are_equal_width() {
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    let mut view: uint<8> = 0;
    initial! {
        dut.s = -1;
        dut.u = 0xff;
        dut.narrow = 0;
        dut.w = 0;
        // A bare slice of a signed value is an unsigned bit vector.
        assert_eq!(dut.s[0..4], dut.narrow);
        // An equal-width cast reinterprets the same bits on purpose.
        view = dut.s as uint<8>;
        assert_eq!(view, 0xff);
        assert_eq!(dut.u as int<8>, -1);
        pass!();
        finish!();
    }
}
"##;
    let artifact =
        compile_testbench(source, "signed.shy", "smoke", &signed_design()).expect("generate");
    let verilog = &artifact.verilog;
    assert!(
        verilog.contains("((dut_s[3:0]) !== (dut_narrow))"),
        "{verilog}"
    );
    assert!(verilog.contains("$unsigned(dut_s)"), "{verilog}");
    assert!(verilog.contains("$signed(dut_u)"), "{verilog}");
    // A selection inside a selection folds into one part-select instead of nesting
    // (Verilog has no `x[..][..]`): `dut.w.tag[0..2]` is bits 1:0 of the packed
    // struct, read back as an unsigned slice.
    let nested = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Signed::new();
    initial! {
        dut.s = 0;
        dut.u = 0;
        dut.narrow = 0;
        dut.w = 0;
        assert_eq!(dut.w.tag[0..2], 0);
        finish!();
    }
}
"##;
    let artifact = compile_testbench(nested, "signed.shy", "smoke", &signed_design())
        .expect("a nested selection folds instead of nesting");
    assert!(
        artifact.verilog.contains("dut_w[1:0]"),
        "{}",
        artifact.verilog
    );
}

/// A fixed-length array of `element`.
fn array_of(element: ValueType, len: usize) -> ValueType {
    ValueType::Array {
        element: Box::new(element),
        len,
    }
}

/// `struct Point { x: uint<8>, vals: [uint<4>; 2] }`.
fn point_type() -> ValueType {
    ValueType::Struct {
        name: "Point".into(),
        fields: vec![
            shyhdl::metadata::TypeField {
                name: "x".into(),
                ty: uint(8),
            },
            shyhdl::metadata::TypeField {
                name: "vals".into(),
                ty: array_of(uint(4), 2),
            },
        ],
    }
}

/// A DUT whose ports cover the array shapes: a plain array, an `int` array, a
/// nested array, a struct containing an array, an array of structs and a `bool`
/// array.
fn arrays_design() -> DesignInfo {
    DesignInfo::new(
        "Arrays",
        "Arrays",
        vec![
            ModulePort::new("u4x3", "u4x3", PortDirection::In, array_of(uint(4), 3)),
            ModulePort::new(
                "i8x2",
                "i8x2",
                PortDirection::In,
                array_of(ValueType::SInt { width: 8 }, 2),
            ),
            ModulePort::new(
                "nested",
                "nested",
                PortDirection::In,
                array_of(array_of(uint(2), 2), 2),
            ),
            ModulePort::new("word", "word", PortDirection::In, point_type()),
            ModulePort::new(
                "points",
                "points",
                PortDirection::In,
                array_of(point_type(), 2),
            ),
            ModulePort::new(
                "flags",
                "flags",
                PortDirection::In,
                array_of(ValueType::Bool, 4),
            ),
            ModulePort::new("sum", "sum", PortDirection::Out, uint(12)),
        ],
    )
}

const ARRAYS_DUT: &str = r#"
module Arrays(
    input [11:0] u4x3,
    input [15:0] i8x2,
    input [7:0] nested,
    input [15:0] word,
    input [31:0] points,
    input [3:0] flags,
    output [11:0] sum
);
  assign sum = u4x3;
endmodule
"#;

const ARRAYS_TEST: &str = r##"
#[hardware_test]
fn arrays() {
    let mut dut = Arrays::new();
    let mut a: [uint<4>; 3] = [1, 2, 3];
    let mut b: [uint<4>; 3] = [0; 3];
    let mut sub: [uint<4>; 2] = [0; 2];
    let mut sub1: [uint<4>; 1] = [0; 1];
    let mut i: uint<2> = 1;
    let mut inner: uint<2> = 1;
    initial! {
        dut.u4x3 = a;
        dut.i8x2 = [0; 2];
        // An array-shaped value takes a shaped literal: `[0; 2]` here would be a
        // scalar standing in for `[[uint<2>; 2]; 2]`.
        dut.nested = [[0; 2]; 2];
        dut.word.x = 0;
        dut.word.vals = [0; 2];
        dut.points[0].x = 0;
        dut.points[0].vals = [0; 2];
        dut.points[1].x = 0;
        dut.points[1].vals = [0; 2];
        dut.flags = [false; 4];
        delay(ps(1));
        assert_eq!(a[0], 1);
        assert_eq!(a[2], 3);
        assert_eq!(a[i], 2);
        b = a;
        assert_eq!(b[1], 2);
        b[0] = 9;
        assert_eq!(b[0], 9);
        // Writing one element must not disturb its neighbours.
        assert_eq!(b[1], 2);
        assert_eq!(b[2], 3);
        for k in 0..3 {
            b[k] = 5;
            assert_eq!(b[k], 5);
        }
        sub = a.slice(1, 3);
        assert_eq!(sub[0], 2);
        assert_eq!(sub[1], 3);
        // A one element sub-array is still an array.
        sub1 = a.slice(1, 2);
        assert_eq!(sub1[0], 2);
        // A sub-array and a bare range are places as well as reads: the first
        // writes those elements, the second the packed bits of the array.
        a.slice(1, 3) = [7, 8];
        assert_eq!(a[2], 8);
        a[0..4] = 6;
        assert_eq!(a[0], 6);
        dut.i8x2[0] = -1;
        assert_eq!(dut.i8x2[0], -1);
        // A dynamic index into a signed element reads back signed.
        dut.i8x2[i] = -1;
        assert_eq!(dut.i8x2[i], -1);
        dut.nested[0][1] = 3;
        assert_eq!(dut.nested[0][1], 3);
        assert_eq!(dut.nested[1][inner], 0);
        // Successful writes through both dynamic layers ...
        i = 1;
        inner = 1;
        dut.nested[i][inner] = 1;
        assert_eq!(dut.nested[i][inner], 1);
        // The neighbour the fold could have reached is untouched.
        assert_eq!(dut.nested[0][0], 0);
        // ... and through a dynamic element index plus a static field.
        dut.points[i].x = 0xcdu8;
        assert_eq!(dut.points[i].x, 0xcdu8);
        assert_eq!(dut.points[0].x, 0);
        dut.word.vals[1] = 7;
        assert_eq!(dut.word.vals[1], 7);
        dut.points[1].x = 0xabu8;
        assert_eq!(dut.points[1].x, 0xabu8);
        dut.flags[2] = true;
        assert!(dut.flags[2] == true);
        pass!();
        finish!();
    }
}
"##;

#[test]
fn array_selection_folds_and_matches_the_element_layout() {
    let artifact = compile_testbench(ARRAYS_TEST, "arrays.shy", "arrays", &arrays_design())
        .expect("generate the array testbench");
    let verilog = &artifact.verilog;
    // Element 0 is the least significant, so the list is concatenated in reverse;
    // everything else is checked by running the testbench rather than by mirroring
    // its text.
    assert!(
        verilog.contains("{4'd3, 4'd2, 4'd1}"),
        "element 0 must be the low bits: {verilog}"
    );
    // A dynamic selection is one indexed part-select, with its own in-range guard,
    // and a guarded read substitutes `x`.
    assert!(verilog.contains("+:"), "{verilog}");
    assert!(verilog.contains("'bx)"), "{verilog}");
    // A signed element is read back signed even though a part-select is unsigned.
    assert!(verilog.contains("$signed("), "{verilog}");

    // The same testbench really runs: layout, static and dynamic reads and writes,
    // nested arrays, struct-of-array and array-of-struct.
    let dir = artifact_dir("gen_arrays");
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("arrays.v");
    std::fs::write(&tb, verilog).unwrap();
    std::fs::write(&dut, ARRAYS_DUT).unwrap();
    let vvp_file = dir.join("sim.vvp");
    let output = Command::new(iverilog())
        .arg("-g2005")
        .arg("-o")
        .arg(&vvp_file)
        .arg(&dut)
        .arg(&tb)
        .output()
        .expect("run iverilog");
    assert!(
        output.status.success(),
        "iverilog rejected the array testbench:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = Command::new(vvp())
        .arg(&vvp_file)
        .output()
        .expect("run vvp");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
    assert!(
        stdout.contains("SHYHDL_PASS test=arrays case=finish"),
        "{stdout}"
    );
}

#[test]
fn dynamic_out_of_range_is_x_on_read_and_a_no_op_on_write() {
    // A dynamic index that is out of range must stay inside its own layer: the
    // write is skipped and the read is `x` under a four-state tool. The read is
    // asserted against a wrong value on purpose, so the FAIL line prints it.
    let source = r##"
#[hardware_test]
fn arrays_oob() {
    let mut dut = Arrays::new();
    let mut inner: uint<2> = 2;
    let mut bit4: uint<3> = 4;
    let mut bit0: uint<1> = 0;
    let mut one: [bool; 1] = [false];
    initial! {
        dut.u4x3 = [0; 3];
        dut.i8x2 = [0; 2];
        dut.nested = [[0; 2]; 2];
        dut.word.x = 0;
        dut.word.vals = [0; 2];
        dut.points[0].x = 0;
        dut.points[0].vals = [0; 2];
        dut.points[1].x = 0;
        dut.points[1].vals = [0; 2];
        dut.flags = [false; 4];
        delay(ps(1));
        dut.nested[0][1] = 3;
        dut.nested[0][inner] = 1;
        assert_eq!(dut.nested[0][1], 3);
        // A dynamic bit index inside a field is guarded too: bit 4 of a 4 bit
        // element is out of range, so it must leave the *neighbouring* element of
        // `vals` exactly as it was.
        dut.word.vals[1] = 5;
        dut.word.vals[0][bit4] = 1;
        assert_eq!(dut.word.vals[1], 5);
        assert_eq!(dut.word.vals[0], 0);
        bit4 = 3;
        dut.word.vals[0][bit4] = 1;
        assert_eq!(dut.word.vals[0], 8);
        assert_eq!(dut.word.vals[1], 5);
        // A one bit root is a scalar: index 0 is legal, index 1 is a no-op, and its
        // read is `x` under a four-state tool.
        one[bit0] = true;
        assert!(one[0] == true);
        bit0 = 1;
        one[bit0] = false;
        assert!(one[0] == true);
        assert_eq!(one[bit0], false);
        assert_eq!(dut.nested[0][inner], 0);
        finish!();
    }
}
"##;
    let artifact =
        compile_testbench(source, "arrays.shy", "arrays_oob", &arrays_design()).expect("generate");
    let dir = artifact_dir("gen_arrays_oob");
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("arrays.v");
    std::fs::write(&tb, &artifact.verilog).unwrap();
    std::fs::write(&dut, ARRAYS_DUT).unwrap();
    let vvp_file = dir.join("sim.vvp");
    let output = Command::new(iverilog())
        .arg("-g2005")
        .arg("-o")
        .arg(&vvp_file)
        .arg(&dut)
        .arg(&tb)
        .output()
        .expect("run iverilog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = Command::new(vvp())
        .arg(&vvp_file)
        .output()
        .expect("run vvp");
    let stdout = String::from_utf8_lossy(&run.stdout);
    // Two deliberate failures, both out-of-range reads; every in-range assertion,
    // including the ones on the neighbouring values, passed.
    assert_eq!(stdout.matches("SHYHDL_FAIL").count(), 2, "{stdout}");
    assert_eq!(
        stdout.matches("actual=x").count(),
        2,
        "a four-state read of an out-of-range element must be x: {stdout}"
    );
}

#[test]
fn array_misuse_is_rejected_with_a_location() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "static_element_out_of_range",
            "assert_eq!(a[3], 0);",
            "outside an array of 3",
        ),
        (
            "nested_static_out_of_range",
            "dut.nested[0][2] = 0;",
            "outside an array of 2",
        ),
        (
            "subarray_out_of_range",
            "sub = a.slice(1, 4);",
            "outside an array of 3",
        ),
        (
            "list_length",
            "let mut c: [uint<4>; 2] = [1, 2, 3];",
            "list has 3 element(s) but the target holds 2",
        ),
        (
            "repeat_count",
            "let mut c: [uint<4>; 2] = [0; 3];",
            "repeats 3 element(s) but the target holds 2",
        ),
        (
            "copy_shape",
            "let mut c: [uint<8>; 2] = [0; 2]; c = a;",
            "different shape",
        ),
        (
            "aggregate_into_scalar",
            "let mut w: uint<12> = 0; w = a;",
            "different shape",
        ),
        (
            "signed_index",
            "let mut si: int<8> = 0; assert_eq!(a[si], 0);",
            "index must be unsigned",
        ),
        (
            "struct_indexed_by_element",
            "assert_eq!(dut.word[0], 0);",
            "has no elements to index",
        ),
        (
            "struct_element_local",
            "let mut pts: [Point; 2] = [0; 2];",
            "unsupported testbench type `Point`",
        ),
        (
            "literal_without_array_target",
            "let mut w: uint<8> = 0; w = [1, 2];",
            "array literal needs an array target",
        ),
    ];
    for (name, statement, expected) in cases {
        let source = format!(
            "#[hardware_test]\nfn smoke() {{\n    let mut dut = Arrays::new();\n    let mut a: [uint<4>; 3] = [0; 3];\n    let mut sub: [uint<4>; 2] = [0; 2];\n    let mut sub1: [uint<4>; 1] = [0; 1];\n    let mut x: uint<4> = 0;\n    let mut inner: uint<2> = 0;\n    let mut i: uint<2> = 0;\n    initial! {{\n        dut.u4x3 = a;\n        dut.i8x2 = [0; 2];\n        dut.nested = [[0; 2]; 2];\n        dut.word.x = 0;\n        dut.word.vals = [0; 2];\n        dut.points[0].x = 0;\n        dut.points[0].vals = [0; 2];\n        dut.points[1].x = 0;\n        dut.points[1].vals = [0; 2];\n        dut.flags = [false; 4];\n        delay(ps(1));\n        {statement}\n        finish!();\n    }}\n}}\n"
        );
        let error =
            compile_testbench(&source, "arrays.shy", "smoke", &arrays_design()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(expected), "{name}: {text}");
        match &error {
            shyhdl::Error::Unsupported { span, .. } => {
                assert_eq!(span.file, "arrays.shy", "{name}: {error}");
                assert!(span.line >= 4, "{name}: {error}");
            }
            other => panic!("{name}: expected a located error, got {other:?}"),
        }
    }
}

#[test]
fn systemverilog_reserved_names_are_rejected() {
    // `expect` is legal Verilog-2005 but reserved in SystemVerilog, which
    // Verilator parses: a generated testbench that the backend cannot compile is
    // a generator bug.
    for name in ["expect", "bit", "logic"] {
        let source = format!(
            r#"
#[hardware_test]
fn smoke() {{
    let mut dut = Counter::new();
    let mut {name}: uint<8> = 0;
    initial! {{
        dut.q = 0;
        finish!();
    }}
}}
"#
        );
        let error = compile(&source).unwrap_err().to_string();
        assert!(error.contains("reserved word"), "{name}: {error}");
    }
}

#[test]
fn unsupported_constructs_are_reported_with_a_location() {
    let cases: &[(&str, &str)] = &[
        ("while_loop", "fn smoke() { let mut dut = Counter::new(); while true { } }"),
        ("unknown_call", "fn smoke() { let mut dut = Counter::new(); initial! { dut.q = 0; println(); } }"),
        ("unknown_macro", "fn smoke() { let mut dut = Counter::new(); initial! { dut.q = 0; todo!(); } }"),
        ("assert_needs_bool", "fn smoke() { let mut dut = Counter::new(); initial! { assert!(dut.q); } }"),
        ("if_needs_bool", "fn smoke() { let mut dut = Counter::new(); initial! { if dut.q { } } }"),
        ("implicit_truncation", "fn smoke() { let mut dut = Counter::new(); let mut wide: uint<16> = 0; initial! { dut.q = wide; } }"),
        ("unary_minus", "fn smoke() { let mut dut = Counter::new(); initial! { dut.q = -1; } }"),
        ("slice_out_of_range", "fn smoke() { let mut dut = Counter::new(); initial! { dut.d = 0; dut.d[0..9] = 0; } }"),
        ("missing_declaration", "fn smoke() { let mut dut = Counter::new(); initial! { dut.q = nope; } }"),
        ("unknown_port", "fn smoke() { let mut dut = Counter::new(); initial! { dut.missing = 0; } }"),
        ("wrong_module", "fn smoke() { let mut dut = Other::new(); initial! { } }"),
        ("sample_after_zero", "fn smoke() { let mut dut = Counter::new(); initial! { sample_after(posedge(dut.clk), ps(0)); } }"),
        ("assign_to_output", "fn smoke() { let mut dut = Counter::new(); initial! { dut.q = 1; } }"),
        ("local_without_type", "fn smoke() { let mut dut = Counter::new(); let mut x = 1; initial! { dut.q = 0; } }"),
        ("time_without_unit", "fn smoke() { let mut dut = Counter::new(); initial! { delay(10); } }"),
    ];
    for (name, body) in cases {
        let source = format!("#[hardware_test]\n{body}\n");
        let error = compile(&source).unwrap_err();
        match &error {
            shyhdl::Error::Unsupported { span, message } => {
                assert_eq!(span.file, "counter.shy", "{name}: {error}");
                assert!(
                    span.line >= 2,
                    "{name}: location should point inside the test: {error}"
                );
                assert!(!message.is_empty(), "{name}: empty message");
            }
            other => panic!("{name}: expected a located Unsupported error, got {other:?}"),
        }
    }
    // A missing test function is still an error, just not a located one.
    let error = compile_testbench(
        "#[hardware_test]\nfn other() {}\n",
        "counter.shy",
        "smoke",
        &design(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("available tests: other"), "{error}");
}

#[test]
fn finish_is_optional_and_a_test_that_never_ends_is_not_a_pass() {
    let auto = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        dut.enable = false;
        dut.d = 0x11;
        delay(ns(1));
        assert_eq!(dut.q, 0x00);
    }
}
"##;
    let artifact = compile(auto).expect("generate");
    // Automatic completion: PASS is printed only after the block returns.
    assert!(
        artifact
            .verilog
            .contains("wait (__shyhdl_blocks_done == 1)"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("case=complete"),
        "{}",
        artifact.verilog
    );
    assert_eq!(artifact.expected_passes, Some(0));

    // With an explicit `finish!()` the testbench ends itself.
    let explicit = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    initial! {
        dut.d = 0;
        pass!();
        finish!();
    }
}
"##;
    let artifact = compile(explicit).expect("generate");
    assert!(
        artifact.verilog.contains("case=finish"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("$finish;"),
        "{}",
        artifact.verilog
    );
    assert_eq!(
        artifact.expected_passes, None,
        "`finish!()` can end a run early"
    );
}

#[test]
fn instances_carry_their_waveform_scope() {
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    clock!(dut.clk, high = ns(6), low = ns(4));
    initial! {
        dut.d = 0;
        finish!();
    }
}
"##;
    let artifact = compile(source).expect("generate");
    assert_eq!(artifact.instances.len(), 1);
    let instance = &artifact.instances[0];
    assert_eq!(instance.var, "dut");
    assert_eq!(instance.instance, "u_dut");
    assert_eq!(instance.module, "Counter");
    assert_eq!(instance.scope, format!("TOP.{}.u_dut", artifact.top));
    assert_eq!(instance.module_source_name, "Counter");
    let names: Vec<&str> = instance
        .ports
        .iter()
        .map(|port| port.source_name.as_str())
        .collect();
    assert_eq!(names, ["clk", "rst_n", "enable", "d", "q"]);
    assert_eq!(instance.ports[3].net, "dut_d");
    assert_eq!(instance.ports[3].width, 8);
    assert_eq!(artifact.clocks.len(), 1);
    assert_eq!(
        artifact.clocks[0].signal,
        format!("TOP.{}.u_dut.clk", artifact.top)
    );
    // The clock wrapper keeps the explicit high and low phases (REQ-TEST-003):
    // starting low, the low phase lasts `low` and the high phase lasts `high`.
    assert!(
        artifact.verilog.contains("#4 dut_clk = 1'b1;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("#6 dut_clk = 1'b0;"),
        "{}",
        artifact.verilog
    );
    // The reset wrapper drives the inactive level first so the assert is a real
    // 1 -> 0 transition (REQ-TEST-011).
    let reset = r#"
#[hardware_test]
fn smoke() {
    let mut dut = Counter::new();
    reset!(dut.rst_n, active_low, hold = ns(20));
    initial! { dut.d = 0; finish!(); }
}
"#;
    let artifact = compile(reset).expect("generate");
    assert!(
        artifact.verilog.contains("dut_rst_n = 1'b1;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("#1 dut_rst_n = 1'b0;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("#20 dut_rst_n = 1'b1;"),
        "{}",
        artifact.verilog
    );
}

#[test]
fn nested_fixed_loops_and_matrix_ports_are_supported() {
    let matrix = DesignInfo::new(
        "Word",
        "Word",
        vec![
            ModulePort::new("clk", "clk", PortDirection::In, ValueType::Bool),
            ModulePort::new(
                "w",
                "w",
                PortDirection::In,
                ValueType::Struct {
                    name: "Word".into(),
                    fields: vec![
                        shyhdl::metadata::TypeField {
                            name: "op".into(),
                            ty: uint(4),
                        },
                        shyhdl::metadata::TypeField {
                            name: "addr".into(),
                            ty: uint(32),
                        },
                        shyhdl::metadata::TypeField {
                            name: "flag".into(),
                            ty: ValueType::Bool,
                        },
                        shyhdl::metadata::TypeField {
                            name: "tag".into(),
                            ty: uint(7),
                        },
                    ],
                },
            ),
            ModulePort::new(
                "w_out",
                "w_out",
                PortDirection::Out,
                ValueType::UInt { width: 44 },
            ),
        ],
    );
    let source = r##"
#[hardware_test]
fn smoke() {
    let mut dut = Word::new();
    clock!(dut.clk, period = ns(10));
    initial! {
        dut.w = 0;
        dut.w.op = 0x5;
        dut.w.addr = 0xdeadbeef;
        dut.w.flag = true;
        assert_eq!(dut.w.op, 0x5);
        for i in 0..2 {
            for j in 0..3 {
                nb_assign!(dut.w.tag, 0x3f);
            }
        }
        finish!();
    }
}
"##;
    let artifact = compile_testbench(source, "word.shy", "smoke", &matrix).expect("generate");
    // The 44-bit struct is packed most significant field first: op in [43:40],
    // flag in [7:7] and tag in [6:0].
    assert!(
        artifact.verilog.contains("dut_w[43:40] = 4'd5;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("dut_w[7] = 1'b1;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact.verilog.contains("dut_w[6:0] <= 7'd63;"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact
            .verilog
            .contains("for (i = 0; i < 2; i = i + 1) begin"),
        "{}",
        artifact.verilog
    );
    assert!(
        artifact
            .verilog
            .contains("for (j = 0; j < 3; j = j + 1) begin"),
        "{}",
        artifact.verilog
    );
    // `finish!()` can end the run before any later marker, so the static marker
    // count is unknown rather than a number the runner would enforce.
    assert_eq!(artifact.expected_passes, None);
}
