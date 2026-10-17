//! Contract tests for `match` used as an **expression** in a `#[hardware_test]`:
//! a value that feeds a `let`, a block tail, a parenthesized operand, an
//! assertion, a short-circuited operand and a sequential process.
//!
//! These tests do not need the frontend: the DUT is a hand-written Verilog module
//! whose ports are raw bits, and the matching [`DesignInfo`] declares those same
//! ports as typed metadata (`ValueType::Enum`, `Struct`, `Array`, `Bool`). The
//! metadata, not the Verilog, says what the bits mean, so a failure here is a
//! testbench-generator failure and never a frontend failure. Every value scenario
//! really runs through Icarus Verilog (a missing tool fails the test instead of
//! being skipped); the rejection table is the only generation-time part.
//!
//! The contract the rows pin, beyond "the value is right":
//!
//! * the result of a `match` expression is staged once in a hidden result net, so
//!   an arm body may have side effects and only the taken arm runs;
//! * the scrutinee is resolved **once**: rewriting the matched signal (or the
//!   index of a dynamic selection) inside an arm does not change the value the
//!   expression produced;
//! * a `match` inside `&&`/`||` is **not** evaluated when the other operand
//!   already decides the result, so its arm side effects do not run;
//! * an inferred `let` evaluates the expression exactly once (inference must not
//!   evaluate it a second time);
//! * a typed arm decides the common exact type, and an unsuffixed literal adapts
//!   to that type; an all-unsuffixed `match` with no context has no exact type and
//!   is rejected rather than guessed.
//!
//! Rejections are pinned by a located error only; the wording is the owner's.

use std::path::{Path, PathBuf};
use std::process::Command;

use shyhdl::metadata::{EnumVariant, SourceSpan, TypeField, ValueType};
use shyhdl::testbench::{
    compile_testbench, DesignInfo, ModulePort, PortDirection, TestbenchArtifact,
};
use shyhdl::Error;

fn uint(width: u32) -> ValueType {
    ValueType::UInt { width }
}

fn sint(width: u32) -> ValueType {
    ValueType::SInt { width }
}

fn variant(name: &str, tag: u64, payload: Option<ValueType>) -> EnumVariant {
    EnumVariant {
        name: name.to_string(),
        tag,
        payload: payload.map(Box::new),
    }
}

fn field(name: &str, ty: ValueType) -> TypeField {
    TypeField {
        name: name.to_string(),
        ty,
    }
}

/// `enum Msg { Idle, Data(uint<4>), Pair { hi: int<4>, lo: uint<4> } }`: two tag
/// bits, an eight bit widest payload, ten bits in total.
fn msg_type() -> ValueType {
    ValueType::Enum {
        name: "Msg".to_string(),
        tag_width: 2,
        variants: vec![
            variant("Idle", 0, None),
            variant("Data", 1, Some(uint(4))),
            variant(
                "Pair",
                2,
                Some(ValueType::Struct {
                    name: "Msg::Pair".to_string(),
                    fields: vec![field("hi", sint(4)), field("lo", uint(4))],
                }),
            ),
        ],
    }
}

/// `Option<bool>`: tag 0 `None`, tag 1 `Some(bool)`.
fn option_type(payload: ValueType, name: &str) -> ValueType {
    ValueType::Enum {
        name: name.to_string(),
        tag_width: 1,
        variants: vec![variant("None", 0, None), variant("Some", 1, Some(payload))],
    }
}

/// `struct Envelope { msg: Msg, extra: uint<2> }`.
fn envelope_type(msg: &ValueType) -> ValueType {
    ValueType::Struct {
        name: "Envelope".to_string(),
        fields: vec![field("msg", msg.clone()), field("extra", uint(2))],
    }
}

fn input_port(name: &str, ty: ValueType) -> ModulePort {
    ModulePort::new(name, name, PortDirection::In, ty)
}

fn output_port(name: &str, ty: ValueType) -> ModulePort {
    ModulePort::new(name, name, PortDirection::Out, ty)
}

/// The DUT contract: raw-bit ports and what the metadata says they mean.
///
/// `arrout` and `miss` come from the DUT (a constant array and a four-state tag);
/// everything else is driven by the test.
fn match_expr_design() -> DesignInfo {
    let msg = msg_type();
    let ob = option_type(ValueType::Bool, "Option<bool>");
    let envelope = envelope_type(&msg);
    let pair = ValueType::Array {
        element: Box::new(uint(4)),
        len: 2,
    };
    let msg_pair = ValueType::Array {
        element: Box::new(msg.clone()),
        len: 2,
    };
    DesignInfo::new(
        "MatchExpr",
        "MatchExpr",
        vec![
            input_port("msg", msg.clone()),
            input_port("pair", pair),
            input_port("ob", ob),
            input_port("clk", ValueType::Bool),
            input_port("hold", uint(4)),
            input_port("env", envelope.clone()),
            input_port("env2", envelope),
            output_port("arrout", msg_pair),
            output_port("miss", msg),
        ],
    )
}

/// The DUT: `MatchExpr` knows nothing about enums or match arms, it only drives
/// and holds raw bits.
const MATCH_EXPR_DUT: &str = r#"
module MatchExpr(
    input  [9:0]  msg,
    input  [7:0]  pair,
    input  [1:0]  ob,
    input         clk,
    input  [3:0]  hold,
    input  [11:0] env,
    input  [11:0] env2,
    output [19:0] arrout,
    output [9:0]  miss
);
  // `[Msg; 2]`: element 0 is `Data(7)` and element 1 is `Idle`. Element 0 is the
  // low ten bits.
  assign arrout = {10'b0000000000, 10'b0000011101};
  // A four-state tag: no arm test can be true, so only a catch-all matches it.
  assign miss = 10'bx;
endmodule
"#;

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

/// Compile the generated testbench together with the DUT and run it, returning
/// what the testbench printed. The result is judged by the markers, never by the
/// exit code alone (REQ-TEST-008).
fn run_testbench(dir_name: &str, artifact: &TestbenchArtifact) -> String {
    let dir = artifact_dir(dir_name);
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("match_expr.v");
    std::fs::write(&tb, &artifact.verilog).expect("write the generated testbench");
    std::fs::write(&dut, MATCH_EXPR_DUT).expect("write the DUT");

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
    String::from_utf8_lossy(&run.stdout).into_owned()
}

/// The source location a rejection carries. Which error family reports a bad
/// `match` expression is the owner's interface, so the location is the contract
/// here, not the wording.
fn located(error: &Error) -> &SourceSpan {
    match error {
        Error::Unsupported { span, .. } => span,
        Error::TypeLayout {
            span: Some(span), ..
        } => span,
        other => panic!("expected a located rejection, got {other}"),
    }
}

/// `(case, statements)`: one scalar row of the runtime test. Each row runs inside
/// `initial!` and ends with its own `pass!()`, so a FAIL line names the case.
const SCALAR_ROWS: &[(&str, &str)] = &[
    (
        "typed_let",
        r#"        dut.msg = Msg::Data(3);
        let mut a: uint<4> = match dut.msg {
            Msg::Data(v) => v,
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
        assert_eq!(a, 3);
        pass!();
"#,
    ),
    (
        "inferred_let_from_a_typed_arm",
        r#"        dut.msg = Msg::Pair { hi: -2, lo: 0 };
        let mut b = match dut.msg {
            Msg::Pair { hi, .. } => hi,
            other => -1,
        };
        assert_eq!(b, -2);
        let mut bits: uint<4> = b as uint<4>;
        assert_eq!(bits, 0xE);
        pass!();
"#,
    ),
    (
        "parenthesized_arithmetic_operand",
        r#"        dut.msg = Msg::Data(3);
        let mut c: uint<4> = 4u4 * (match dut.msg {
            Msg::Data(v) => v,
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        });
        assert_eq!(c, 12);
        pass!();
"#,
    ),
    (
        "block_tail_with_side_effects",
        r#"        dut.msg = Msg::Data(3);
        hit = 0;
        let mut t: uint<4> = {
            let mut base: uint<4> = 2u4;
            match dut.msg {
                Msg::Data(v) => { hit = 1; base + v }
                Msg::Idle => { hit = 2; base }
                Msg::Pair { lo, .. } => { hit = 3; base + lo }
                other => { hit = 0; base }
            }
        };
        assert_eq!(t, 5);
        assert_eq!(hit, 1);
        pass!();
"#,
    ),
    (
        "asserted_value_match",
        r#"        dut.msg = Msg::Data(3);
        assert!(match dut.msg {
            Msg::Data(v) => v == 3,
            Msg::Idle => false,
            Msg::Pair { lo, .. } => lo == 9,
            other => false,
        });
        pass!();
"#,
    ),
    (
        "overlapping_or_with_a_false_guard",
        r#"        dut.pair = [0, 5];
        let mut o: uint<4> = match dut.pair {
            [x, 5] | [0, x] if x == 5 => x,
            _ => 15,
        };
        assert_eq!(o, 5);
        pass!();
"#,
    ),
    (
        "scrutinee_snapshot_of_a_signal",
        r#"        dut.msg = Msg::Data(3);
        let mut s: uint<4> = match dut.msg {
            Msg::Data(v) => { dut.msg = Msg::Data(5); v }
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
        assert_eq!(s, 3);
        pass!();
"#,
    ),
    (
        "scrutinee_snapshot_of_a_dynamic_selection",
        r#"        idx = 0;
        let mut k: uint<4> = match dut.arrout[idx] {
            Msg::Data(v) => { idx = 1; v }
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => { idx = 1; lo }
            other => { idx = 1; 15 }
        };
        assert_eq!(k, 7);
        pass!();
"#,
    ),
    (
        "four_state_scrutinee_takes_the_catch_all",
        r#"        let mut xv: uint<4> = match dut.miss {
            Msg::Data(v) => v,
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 9,
        };
        assert_eq!(xv, 9);
        pass!();
"#,
    ),
    (
        "short_circuited_and_does_not_run_the_arms",
        r#"        dut.msg = Msg::Data(3);
        hit = 0;
        let mut short_and: bool = false && (match dut.msg {
            Msg::Data(v) => { hit = hit + 1; v == 3 }
            Msg::Idle => { hit = hit + 1; false }
            Msg::Pair { lo, .. } => { hit = hit + 1; lo == 2 }
            other => { hit = hit + 1; false }
        });
        assert!(!short_and);
        assert_eq!(hit, 0);
        pass!();
"#,
    ),
    (
        "short_circuited_or_does_not_run_the_arms",
        r#"        dut.msg = Msg::Data(3);
        hit = 0;
        let mut short_or: bool = true || (match dut.msg {
            Msg::Data(v) => { hit = hit + 1; v == 3 }
            Msg::Idle => { hit = hit + 1; false }
            Msg::Pair { lo, .. } => { hit = hit + 1; lo == 2 }
            other => { hit = hit + 1; false }
        });
        assert!(short_or);
        assert_eq!(hit, 0);
        pass!();
"#,
    ),
    (
        "inferred_let_is_evaluated_once",
        r#"        dut.ob = None;
        dut.pair = [3, 0];
        hit = 0;
        let mut once = match dut.ob {
            None => { hit = hit + 1; dut.pair[0] }
            Some(v) => { hit = hit + 1; dut.pair[0] }
        };
        assert_eq!(once, 3);
        assert_eq!(hit, 1);
        pass!();
"#,
    ),
];

/// The scalar/nested testbench: one top level `let` (a constructor scrutinee, so
/// it does not depend on drive order), then every row of [`SCALAR_ROWS`].
fn scalar_test_source() -> String {
    let mut body = String::new();
    for (name, statements) in SCALAR_ROWS {
        body.push_str(&format!("        // case: {name}\n{statements}"));
    }
    format!(
        "#[hardware_test]\n\
         fn enums_match_expr_scalar() {{\n    \
             let mut dut = MatchExpr::new();\n    \
             let mut hit: uint<2> = 0;\n    \
             let mut idx: uint<2> = 0;\n    \
             let mut top: uint<4> = match Msg::Data(3) {{\n        \
                 Msg::Data(v) => v,\n        \
                 Msg::Idle => 0,\n        \
                 Msg::Pair {{ lo, .. }} => lo,\n        \
                 other => 15,\n    \
             }};\n    \
             initial! {{\n        \
                 dut.msg = Msg::Idle;\n        \
                 dut.ob = None;\n        \
                 dut.pair = [0; 2];\n        \
                 delay(ps(1));\n        \
                 assert_eq!(top, 3);\n        \
                 pass!();\n\
         {body}        \
                 finish!();\n    \
         }}\n\
         }}\n"
    )
}

#[test]
fn match_expression_scalar_nested_and_short_circuit() {
    let artifact = compile_testbench(
        &scalar_test_source(),
        "match_expr.shy",
        "enums_match_expr_scalar",
        &match_expr_design(),
    )
    .expect("generate the scalar match expression testbench");
    let stdout = run_testbench("gen_match_expr_scalar", &artifact);
    // One top level `let` plus every row of the table, all reached.
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=enums_match_expr_scalar pass=14 fail=0 markers=13"),
        "{stdout}"
    );
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
}

/// The aggregate and sequential testbench.
///
/// A typed result keeps its own shape: a signed value, an enum, an array and a
/// struct (inferred from the two port values). The sequential part drives an
/// input with `nb_assign!` from inside `always!`, so the same expression form is
/// shown in a clocked process as well.
const AGGREGATE_TEST: &str = r##"
#[hardware_test]
fn enums_match_expr_aggregates() {
    let mut dut = MatchExpr::new();
    clock!(dut.clk, period = ns(10), initial = false);
    always! {
        wait(posedge(dut.clk));
        let mut held: uint<4> = match dut.msg {
            Msg::Data(v) => v,
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
        nb_assign!(dut.hold, held);
    }
    initial! {
        dut.msg = Msg::Idle;
        dut.ob = None;
        dut.pair = [0; 2];
        dut.env.msg = Msg::Idle;
        dut.env.extra = 0;
        dut.env2.msg = Msg::Data(2);
        dut.env2.extra = 1;
        delay(ps(1));

        // A signed result: the annotation is the context, and a negative value
        // has to survive as a negative value, not just as its four bits.
        dut.msg = Msg::Pair { hi: -2, lo: 0 };
        let mut s4: int<4> = match dut.msg {
            Msg::Pair { hi, .. } => hi,
            other => -1,
        };
        assert_eq!(s4, -2);
        let mut s4_bits: uint<4> = s4 as uint<4>;
        assert_eq!(s4_bits, 0xE);
        pass!();

        // An enum result: the arms build variants of the result type.
        dut.ob = Some(true);
        let mut m: Msg = match dut.ob {
            None => Msg::Idle,
            Some(true) => Msg::Data(3),
            Some(false) => Msg::Idle,
        };
        assert_eq!(m, Msg::Data(3));
        pass!();

        // An array result: element 0 is the low bits, so both elements are
        // checked and a reversed order cannot pass.
        let mut arr2: [uint<4>; 2] = match dut.ob {
            None => [0; 2],
            Some(true) => [1, 2],
            Some(false) => [3, 4],
        };
        assert_eq!(arr2[0], 1);
        assert_eq!(arr2[1], 2);
        pass!();

        // A struct result, inferred from the two enum-shaped port values.
        let mut e = match dut.ob {
            None => dut.env,
            Some(true) => dut.env2,
            Some(false) => dut.env,
        };
        assert_eq!(e.extra, 1);
        assert_eq!(e.msg, Msg::Data(2));
        pass!();

        // The same expression form in a clocked process: the value the edge sees
        // is clocked into an input with `nb_assign!`.
        dut.msg = Msg::Data(3);
        wait(posedge(dut.clk));
        wait(posedge(dut.clk));
        sample_after(posedge(dut.clk), ps(1));
        assert_eq!(dut.hold, 3);
        pass!();
        finish!();
    }
}
"##;

#[test]
fn match_expression_typed_aggregates_and_sequential_process() {
    let artifact = compile_testbench(
        AGGREGATE_TEST,
        "match_expr.shy",
        "enums_match_expr_aggregates",
        &match_expr_design(),
    )
    .expect("generate the aggregate match expression testbench");
    let stdout = run_testbench("gen_match_expr_aggregates", &artifact);
    // Four aggregate rows plus the clocked process, all reached.
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=enums_match_expr_aggregates pass=6 fail=0 markers=5"),
        "{stdout}"
    );
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
}

/// Whether a diagnostic source must generate or must be rejected.
enum Expected {
    /// Inside the supported subset: `compile_testbench` must produce a testbench.
    Accepted,
    /// Must be rejected with a located error.
    Rejected,
}

/// `(case, statements, expected)`. The accepted rows share the preamble and the
/// arm shapes of the rejected ones, so a rejection can come only from the row's
/// own difference: an arm of another shape, a missing catch-all, an arm block
/// with no value, or a `match` with no exact type.
const DIAGNOSTIC_CASES: &[(&str, &str, Expected)] = &[
    (
        "typed_arms_are_accepted",
        r#"        let mut ok: uint<4> = match dut.msg {
            Msg::Data(v) => v,
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
"#,
        Expected::Accepted,
    ),
    (
        "an_arm_of_another_shape_is_rejected",
        r#"        let mut bad: uint<4> = match dut.msg {
            Msg::Data(v) => v,
            Msg::Idle => [0, 0],
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
"#,
        Expected::Rejected,
    ),
    (
        "a_missing_catch_all_is_rejected",
        r#"        let mut bad: uint<4> = match dut.msg {
            Msg::Data(v) => v,
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
        };
"#,
        Expected::Rejected,
    ),
    (
        "an_arm_block_with_a_value_is_accepted",
        r#"        let mut ok: uint<4> = match dut.msg {
            Msg::Data(v) => { hit = 1; v }
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
"#,
        Expected::Accepted,
    ),
    (
        "an_arm_block_without_a_value_is_rejected",
        r#"        let mut bad: uint<4> = match dut.msg {
            Msg::Data(v) => { hit = 1; }
            Msg::Idle => 0,
            Msg::Pair { lo, .. } => lo,
            other => 15,
        };
"#,
        Expected::Rejected,
    ),
    (
        "a_suffixed_match_has_an_exact_type",
        r#"        let mut ok = match dut.ob {
            None => 0u4,
            Some(v) => 1u4,
        };
"#,
        Expected::Accepted,
    ),
    (
        "an_all_unsuffixed_match_has_no_exact_type",
        r#"        let mut bad = match dut.ob {
            None => 0,
            Some(v) => 1,
        };
"#,
        Expected::Rejected,
    ),
];

/// The diagnostic sources share everything but the statements under test.
fn diagnostic_source(statements: &str) -> String {
    format!(
        "#[hardware_test]\n\
         fn smoke() {{\n    \
             let mut dut = MatchExpr::new();\n    \
             let mut hit: uint<2> = 0;\n    \
             initial! {{\n        \
                 dut.msg = Msg::Idle;\n        \
                 dut.ob = None;\n        \
                 dut.pair = [0; 2];\n        \
                 delay(ps(1));\n\
         {statements}        \
                 finish!();\n    \
         }}\n\
         }}\n"
    )
}

#[test]
fn match_expression_rejections_are_located() {
    for (name, statements, expected) in DIAGNOSTIC_CASES {
        let source = diagnostic_source(statements);
        match (
            expected,
            compile_testbench(&source, "match_expr.shy", "smoke", &match_expr_design()),
        ) {
            (Expected::Accepted, Ok(_)) => {}
            (Expected::Accepted, Err(error)) => {
                panic!("{name}: expected this to generate, got {error}:\n{source}")
            }
            (Expected::Rejected, Ok(_)) => {
                panic!("{name}: expected a rejection, but it generated:\n{source}")
            }
            (Expected::Rejected, Err(error)) => {
                let span = located(&error);
                assert_eq!(span.file, "match_expr.shy", "{name}: {error}");
                assert!(
                    span.line >= 2,
                    "{name}: the location must point inside the test: {error}"
                );
            }
        }
    }
}
