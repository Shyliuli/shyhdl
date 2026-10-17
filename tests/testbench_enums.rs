//! Contract tests for `enum` values in `compile_testbench`: what a variant
//! constructor packs to, which bits typed equality looks at, what a tag without a
//! variant means, and what `match` over an enum accepts and does at runtime.
//!
//! These tests do not need the frontend: the DUT is a hand-written Verilog module
//! whose ports are raw bits, and the matching [`DesignInfo`] declares those same
//! ports as `ValueType::Enum`. The metadata, not the Verilog, says what the bits
//! mean, so a failure here is a testbench-generator failure and never a frontend
//! failure. Every value scenario really runs through Icarus Verilog (a missing
//! tool fails the test with a clear message instead of being skipped); only the
//! diagnostic table stays generation-time.
//!
//! A user enum's variant is written qualified (`Msg::Idle`, `Msg::Data(..)`,
//! `Msg::Pair { .. }`) and a builtin's bare (`None`, `Some(..)`, `Ok(..)`), which
//! is what the frontend contract tests use; both spellings are accepted. An
//! enum's own `name` string in the metadata is not part of any contract here, so
//! a builtin whose description is not settled can still be asserted.
//!
//! What is deliberately not pinned: the wording of a rejection. A rejected source
//! must fail with a located error, which is what `located` checks.

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
/// bits, an eight bit widest payload (`Pair`), ten bits in total.
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

/// `struct Envelope { msg: Msg, extra: uint<2> }`.
fn envelope_type(msg: &ValueType) -> ValueType {
    ValueType::Struct {
        name: "Envelope".to_string(),
        fields: vec![field("msg", msg.clone()), field("extra", uint(2))],
    }
}

/// `Option<T>`: tag 0 `None`, tag 1 `Some(T)`. `name` is only a readable
/// spelling; no test here names a builtin in source.
fn option_type(payload: ValueType, name: &str) -> ValueType {
    ValueType::Enum {
        name: name.to_string(),
        tag_width: 1,
        variants: vec![variant("None", 0, None), variant("Some", 1, Some(payload))],
    }
}

/// `Result<T, E>`: tag 0 `Ok(T)`, tag 1 `Err(E)`.
fn result_type(ok: ValueType, err: ValueType, name: &str) -> ValueType {
    ValueType::Enum {
        name: name.to_string(),
        tag_width: 1,
        variants: vec![variant("Ok", 0, Some(ok)), variant("Err", 1, Some(err))],
    }
}

fn output_port(name: &str, ty: ValueType) -> ModulePort {
    ModulePort::new(name, name, PortDirection::Out, ty)
}

/// The DUT contract: raw-bit ports, plus what the metadata says those bits mean.
///
/// The five inputs are driven by the test (`msg`, `opt`, `ob`, `deep`, `unit`);
/// the `*_raw` outputs echo an input so a constructed value can be checked as
/// bits. Every other output is a constant raw value the DUT drives, so typed
/// equality and the tag rules are visible without the test having to build an
/// `x`/`z`.
fn enums_design() -> DesignInfo {
    let msg = msg_type();
    let opt = option_type(uint(4), "Option<uint<4>>");
    let ob = option_type(ValueType::Bool, "Option<bool>");
    let result = result_type(uint(4), sint(8), "Result<uint<4>, int<8>>");
    let deep = option_type(result, "Option<Result<uint<4>, int<8>>>");
    let unit = option_type(msg.clone(), "Option<Msg>");
    let envelope = envelope_type(&msg);
    let msg_pair = ValueType::Array {
        element: Box::new(msg.clone()),
        len: 2,
    };

    let mut ports = vec![
        ModulePort::new("msg", "msg", PortDirection::In, msg.clone()),
        ModulePort::new("opt", "opt", PortDirection::In, opt.clone()),
        ModulePort::new("ob", "ob", PortDirection::In, ob.clone()),
        ModulePort::new("deep", "deep", PortDirection::In, deep.clone()),
        ModulePort::new("unit", "unit", PortDirection::In, unit.clone()),
    ];
    for (name, ty) in [
        ("msg_raw", uint(10)),
        ("opt_raw", uint(5)),
        ("ob_raw", uint(2)),
        ("deep_raw", uint(10)),
        ("unit_raw", uint(11)),
        ("unit_padded", unit.clone()),
        ("idle_a", msg.clone()),
        ("idle_b", msg.clone()),
        ("data_a", msg.clone()),
        ("data_b", msg.clone()),
        ("data_c", msg.clone()),
        ("bad_a", msg.clone()),
        ("bad_b", msg.clone()),
        ("x_a", msg.clone()),
        ("x_b", msg.clone()),
        ("opt_none_a", opt.clone()),
        ("opt_none_b", opt.clone()),
        ("deep_ok_a", deep.clone()),
        ("deep_ok_b", deep.clone()),
        ("deep_err", deep.clone()),
        ("env_a", envelope.clone()),
        ("env_b", envelope.clone()),
        ("env_c", envelope.clone()),
        ("arr_a", msg_pair.clone()),
        ("arr_b", msg_pair.clone()),
        ("arr_c", msg_pair.clone()),
    ] {
        ports.push(output_port(name, ty));
    }
    DesignInfo::new("Enums", "Enums", ports)
}

/// The DUT: `Enums` knows nothing about enums, it only drives and echoes raw
/// bits. The comments here and the metadata above are the two halves of one
/// layout statement.
const ENUMS_DUT: &str = r#"
module Enums(
    input  [9:0]  msg,
    input  [4:0]  opt,
    input  [1:0]  ob,
    input  [9:0]  deep,
    input  [10:0] unit,
    output [9:0]  msg_raw,
    output [4:0]  opt_raw,
    output [1:0]  ob_raw,
    output [9:0]  deep_raw,
    output [10:0] unit_raw,
    output [10:0] unit_padded,
    output [9:0]  idle_a,
    output [9:0]  idle_b,
    output [9:0]  data_a,
    output [9:0]  data_b,
    output [9:0]  data_c,
    output [9:0]  bad_a,
    output [9:0]  bad_b,
    output [9:0]  x_a,
    output [9:0]  x_b,
    output [4:0]  opt_none_a,
    output [4:0]  opt_none_b,
    output [9:0]  deep_ok_a,
    output [9:0]  deep_ok_b,
    output [9:0]  deep_err,
    output [11:0] env_a,
    output [11:0] env_b,
    output [11:0] env_c,
    output [19:0] arr_a,
    output [19:0] arr_b,
    output [19:0] arr_c
);
  // A raw view of each enum-typed input, so the test checks what a constructor
  // packs to instead of mirroring the generated Verilog.
  assign msg_raw  = msg;
  assign opt_raw  = opt;
  assign ob_raw   = ob;
  assign deep_raw = deep;
  assign unit_raw = unit;

  // `Some(Msg::Idle)`: bit 0 is the outer `Some`, the ten bit `Msg` sits above
  // it, and the 1s are all inside the inner `Idle`'s unused payload.
  assign unit_padded = 11'b11111111001;

  // `Idle` (tag 0) twice: no payload bit is active, so the 1s above the tag are
  // padding.
  assign idle_a = 10'b0000_0000_00;
  assign idle_b = 10'b1111_1111_00;

  // `Data(3)` (tag 1, active payload bits [5:2] = 0011) twice, with and without
  // 1s in the unused bits [9:6], and `Data(5)`, whose active payload differs.
  assign data_a = 10'b0000_0011_01;
  assign data_b = 10'b1111_0011_01;
  assign data_c = 10'b0000_0101_01;

  // Tag 3 names no variant, so no payload bit is active: two values that differ
  // everywhere above the tag are still the same value.
  assign bad_a = 10'b0000_0000_11;
  assign bad_b = 10'b1111_1111_11;

  // A four-state tag is not a value.
  assign x_a = 10'bx;
  assign x_b = 10'bz;

  // `None` (tag 0) twice: the unused payload bits are zero and one respectively.
  assign opt_none_a = 5'b0000_0;
  assign opt_none_b = 5'b1111_0;

  // `Some(Ok(9))`: bit 0 is `Some`, bit 1 is `Ok`, the active payload is bits
  // [5:2] = 1001 and bits [9:6] are the unused part of the eight bit slot.
  assign deep_ok_a = 10'b0000_1001_01;
  assign deep_ok_b = 10'b1111_1001_01;
  // `Some(Err(0x7f))`: bit 1 is `Err` and all eight payload bits are active.
  assign deep_err = 10'b0111_1111_11;

  // `Envelope { msg, extra }`: `msg` is declared first, so it keeps bits [11:2].
  assign env_a = 12'b0000000000_00;  // Idle, extra 0
  assign env_b = 12'b1111111100_00;  // Idle with 1s of padding, extra 0
  assign env_c = 12'b0000001101_00;  // Data(3), extra 0

  // `[Msg; 2]`: element 0 is the low ten bits.
  assign arr_a = {10'b0000001101, 10'b0000000000};  // [Idle, Data(3)]
  assign arr_b = {10'b1111001101, 10'b1111111100};  // [Idle + padding, Data(3) + unused bits]
  assign arr_c = {10'b0000001101, 10'b0000001101};  // [Data(3), Data(3)]
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
/// what the testbench printed.
fn run_testbench(dir_name: &str, artifact: &TestbenchArtifact) -> String {
    let dir = artifact_dir(dir_name);
    let tb = dir.join(format!("{}.v", artifact.top));
    let dut = dir.join("enums.v");
    std::fs::write(&tb, &artifact.verilog).expect("write the generated testbench");
    std::fs::write(&dut, ENUMS_DUT).expect("write the DUT");

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
    // The result is judged by the markers, never by the exit code alone
    // (REQ-TEST-008): a testbench that deliberately asserts a wrong value still
    // ends with `$finish` and a zero exit status.
    String::from_utf8_lossy(&run.stdout).into_owned()
}

/// The source location a rejection carries. Which error family reports a
/// non-exhaustive `match` is the owner's interface, so the location is the
/// contract here, not the wording.
fn located(error: &Error) -> &SourceSpan {
    match error {
        Error::Unsupported { span, .. } => span,
        Error::TypeLayout {
            span: Some(span), ..
        } => span,
        other => panic!("expected a located rejection, got {other}"),
    }
}

/// `(case, setup, raw port, expected raw bits)`: what one constructor packs to.
/// The layout is the metadata's — tag in the lowest bits, the widest payload
/// above it, unused payload bits zero — and the DUT echoes the port back as
/// plain bits.
const CONSTRUCTION_ROWS: &[(&str, &str, &str, u64)] = &[
    ("idle", "dut.msg = Msg::Idle;", "dut.msg_raw", 0x000),
    ("data", "dut.msg = Msg::Data(0xf);", "dut.msg_raw", 0x03d),
    (
        "named_payload",
        "dut.msg = Msg::Pair { hi: -1, lo: 2 };",
        "dut.msg_raw",
        0x3ca,
    ),
    ("none", "dut.opt = None;", "dut.opt_raw", 0x00),
    ("some", "dut.opt = Some(3);", "dut.opt_raw", 0x07),
    ("some_false", "dut.ob = Some(false);", "dut.ob_raw", 0x1),
    ("some_true", "dut.ob = Some(true);", "dut.ob_raw", 0x3),
    (
        "nested_ok",
        "dut.deep = Some(Ok(9));",
        "dut.deep_raw",
        0x025,
    ),
    (
        "nested_err",
        "dut.deep = Some(Err(0x7f));",
        "dut.deep_raw",
        0x1ff,
    ),
    // A unit variant as a nested payload: the outer tag is bit 0 and the inner
    // `Msg::Idle` contributes only its own two tag bits.
    (
        "nested_unit",
        "dut.unit = Some(Msg::Idle);",
        "dut.unit_raw",
        0x001,
    ),
];

/// The equality half of the same test: both sides come from the DUT, so only the
/// comparison is under test.
const EQUALITY_TAIL: &str = r##"
        // Typed equality ignores the inactive payload: the DUT's two `Idle`
        // values differ above the tag and must still be equal, while a different
        // variant is not.
        assert!(dut.idle_a == dut.idle_b);
        assert_eq!(dut.idle_a, dut.idle_b);
        assert!(dut.idle_a != dut.data_a);
        pass!();

        // `Data(3)` with different unused bits is equal; `Data(5)` is not.
        assert_eq!(dut.data_a, dut.data_b);
        assert!(dut.data_a != dut.data_c);
        pass!();

        // A builtin enum is an enum: `None` ignores its unused payload.
        assert_eq!(dut.opt_none_a, dut.opt_none_b);
        pass!();

        // The recursion goes through a nested payload ...
        assert_eq!(dut.deep_ok_a, dut.deep_ok_b);
        assert!(dut.deep_ok_a != dut.deep_err);
        pass!();

        // A nested unit payload is padding too: what the test constructs is all
        // zero above the inner tag, while the DUT's raw value has 1s there.
        dut.unit = Some(Msg::Idle);
        delay(ps(1));
        assert_eq!(dut.unit, dut.unit_padded);
        pass!();

        // ... and through a struct and an array of enums.
        assert_eq!(dut.env_a, dut.env_b);
        assert!(dut.env_a != dut.env_c);
        assert_eq!(dut.arr_a, dut.arr_b);
        assert!(dut.arr_a != dut.arr_c);
        pass!();

        // The other operand can be a constructor: it carries zero unused bits
        // while the DUT's signal has 1s there, and the variant plus the active
        // payload still decide.
        assert!(dut.data_a == Msg::Data(3));
        assert!(dut.idle_a == Msg::Idle);
        assert!(dut.data_a != Msg::Data(5));
        assert!(dut.bad_a != Msg::Data(3));
        assert!(dut.deep_ok_a == Some(Ok(9)));
        pass!();
"##;

/// The construction testbench, built from [`CONSTRUCTION_ROWS`] so one row is one
/// independently reported case.
fn equality_test_source() -> String {
    use std::fmt::Write as _;
    let mut body = String::new();
    for (name, setup, port, expected) in CONSTRUCTION_ROWS {
        let _ = write!(
            body,
            "        // case: {name}\n        {setup}\n        delay(ps(1));\n        \
             assert_eq!({port}, {expected:#x});\n        pass!();\n"
        );
    }
    format!(
        "#[hardware_test]\n\
         fn enums_equality() {{\n    \
             let mut dut = Enums::new();\n    \
             initial! {{\n        \
                 dut.msg = Msg::Idle;\n        \
                 dut.opt = None;\n        \
                 dut.ob = None;\n        \
                 dut.deep = None;\n        \
                 dut.unit = None;\n        \
                 delay(ps(1));\n\
         {body}{tail}        \
             finish!();\n    \
         }}\n\
         }}\n",
        tail = EQUALITY_TAIL
    )
}

#[test]
fn enum_constructors_pack_and_typed_equality_ignores_the_inactive_payload() {
    let artifact = compile_testbench(
        &equality_test_source(),
        "enums.shy",
        "enums_equality",
        &enums_design(),
    )
    .expect("generate the enum testbench");
    let stdout = run_testbench("gen_enums_equality", &artifact);
    // Ten construction cases plus seven equality cases, all reached.
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=enums_equality pass=18 fail=0 markers=17"),
        "{stdout}"
    );
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");
}

const INVALID_TAG_TEST: &str = r##"
#[hardware_test]
fn enums_invalid_tag() {
    let mut dut = Enums::new();
    initial! {
        dut.msg = Msg::Idle;
        dut.opt = None;
        dut.ob = None;
        dut.deep = None;
        dut.unit = None;
        delay(ps(1));

        // Tag 3 names no variant, so no payload bit is active: two values that
        // differ everywhere above the tag are the same value.
        assert_eq!(dut.bad_a, dut.bad_b);

        // A declared tag and an undeclared one are never equal.
        assert!(dut.bad_a != dut.data_a);
        assert!(dut.data_a != dut.bad_a);
        pass!();
        finish!();
    }
}
"##;

#[test]
fn enum_tags_that_name_no_variant_compare_and_four_state_tags_fail() {
    // A tag that names no variant: covering every declared variant still leaves
    // tag 3 open, and two tag 3 values are the same value whatever their payload
    // bits hold, while a declared tag is never equal to an undeclared one.
    let artifact = compile_testbench(
        INVALID_TAG_TEST,
        "enums.shy",
        "enums_invalid_tag",
        &enums_design(),
    )
    .expect("generate the invalid tag testbench");
    let stdout = run_testbench("gen_enums_invalid_tag", &artifact);
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=enums_invalid_tag pass=2 fail=0 markers=1"),
        "{stdout}"
    );
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");

    // An `x`/`z` tag is not a value: the same tag twice cannot satisfy an
    // assertion either, so this run is wrong on purpose and must fail.
    let artifact = compile_testbench(
        UNKNOWN_TAG_TEST,
        "enums.shy",
        "enums_unknown_tag",
        &enums_design(),
    )
    .expect("generate the unknown tag testbench");
    let stdout = run_testbench("gen_enums_unknown_tag", &artifact);
    assert_eq!(stdout.matches("SHYHDL_FAIL").count(), 2, "{stdout}");
    assert_eq!(
        stdout.matches("SHYHDL_PASS").count(),
        0,
        "a failed assertion must suppress the completion marker: {stdout}"
    );
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=enums_unknown_tag pass=0 fail=2 markers=0"),
        "{stdout}"
    );
}

const UNKNOWN_TAG_TEST: &str = r##"
#[hardware_test]
fn enums_unknown_tag() {
    let mut dut = Enums::new();
    initial! {
        dut.msg = Msg::Idle;
        dut.opt = None;
        dut.ob = None;
        dut.deep = None;
        dut.unit = None;
        delay(ps(1));

        // A tag carrying `x`/`z` is not a value: even the same tag twice cannot
        // satisfy an assertion. Both assertions are wrong on purpose, so the
        // FAIL lines print.
        assert_eq!(dut.x_a, dut.x_a);
        assert_eq!(dut.x_a, dut.x_b);
        finish!();
    }
}
"##;

/// The `Msg` arms every runtime row runs, with `{scrutinee}` filled in by the
/// caller. The guarded arm is written first, so a source-order match takes it for
/// `v > 2`; the bare binding is the catch-all that covers tag 3, which has no
/// variant to name.
const MSG_ARMS: &str = r#"        match {scrutinee} {
            Msg::Data(v) if v > 2 => { y = 1; }
            Msg::Data(v) => { y = v; }
            Msg::Idle => { y = 3; }
            Msg::Pair { hi, lo } => { h = hi; y = lo; }
            other => { y = 15; }
        }
"#;

/// `Option<bool>` spelled out: the three arms cover the type, so no wildcard is
/// needed. `{scrutinee}` is filled in by the caller, so a local of the same type
/// runs the same arms.
const OPTION_BOOL_ARMS: &str = r#"        match {scrutinee} {
            None => { y = 0; }
            Some(false) => { y = 1; }
            Some(true) => { y = 2; }
        }
"#;

/// `Option<uint<4>>`: the same builtin root as `Option<bool>` with another
/// payload, so the written parameter list is what picks the type.
const OPTION_UINT_ARMS: &str = r#"        match {scrutinee} {
            None => { y = 0; }
            Some(v) => { y = v; }
        }
"#;

/// A cross-variant or-pattern: `Msg::Data(v)` and `Msg::Pair { lo: v, .. }` bind
/// one name of one type from two variants. The guarded arm covers nothing, so a
/// value whose alternative matches but whose guard is false stays open for the
/// arms after it.
const OR_ARMS: &str = r#"        match {scrutinee} {
            Msg::Data(v) | Msg::Pair { lo: v, .. } if v > 2 => { y = 7; }
            Msg::Data(v) | Msg::Pair { lo: v, .. } => { y = v; }
            Msg::Idle => { y = 3; }
            other => { y = 15; }
        }
"#;

/// An overlapping array or-pattern whose alternatives bind the same name at two
/// different offsets: `[x, 5]` takes the first element, `[0, x]` the second. A
/// false guard on the first alternative must let the same arm try its next
/// alternative before the match moves on.
const ARRAY_OR_ARMS: &str = r#"        match {scrutinee} {
            [x, 5] | [0, x] if x == 5 => { y = 1; }
            _ => { y = 15; }
        }
"#;

/// The arm body writes the matched signal itself; the binding was copied at the
/// arm's entry, so it must still hold the value the match saw.
const SNAPSHOT_ARMS: &str = r#"        match {scrutinee} {
            Msg::Data(v) => { y = v; {scrutinee} = Msg::Data(5); assert_eq!(v, 3); }
            Msg::Idle => { y = 3; }
            Msg::Pair { hi, lo } => { y = lo; }
            other => { y = 15; }
        }
"#;

/// A nested payload pattern: `Some(Ok(v))` binds what the inner variant carries,
/// and the three arms still cover the whole type.
const NESTED_ARMS: &str = r#"        match dut.deep {
            Some(Ok(v)) => { y = v; }
            Some(Err(e)) => { y = 15; }
            None => { y = 0; }
        }
"#;

/// `(case, setup, matched expression, expected y, expected h)`.
const MSG_ROWS: &[(&str, &str, &str, u64, i64)] = &[
    ("idle", "dut.msg = Msg::Idle;", "dut.msg", 3, 0),
    (
        "data_below_the_guard",
        "dut.msg = Msg::Data(2);",
        "dut.msg",
        2,
        0,
    ),
    (
        "data_above_the_guard",
        "dut.msg = Msg::Data(3);",
        "dut.msg",
        1,
        0,
    ),
    (
        "data_at_the_top",
        "dut.msg = Msg::Data(15);",
        "dut.msg",
        1,
        0,
    ),
    (
        "named_payload",
        "dut.msg = Msg::Pair { hi: -1, lo: 9 };",
        "dut.msg",
        9,
        -1,
    ),
    // The tag with no variant is only reachable as raw bits, so the DUT drives it.
    (
        "tag_with_no_variant",
        "dut.msg = Msg::Idle;",
        "dut.bad_a",
        15,
        0,
    ),
    // A dynamic selection: the DUT's `[Msg; 2]` is indexed by a testbench local,
    // and an index outside the array reads `x`, which only the catch-all can
    // match. Element 0 is `Idle`, element 1 is `Data(3)`.
    ("array_element_idle", "i = 0;", "dut.arr_a[i]", 3, 0),
    ("array_element_data", "i = 1;", "dut.arr_a[i]", 1, 0),
    (
        "array_element_out_of_range_is_x",
        "i = 2;",
        "dut.arr_a[i]",
        15,
        0,
    ),
];

/// `(case, setup, expected y)`.
const OPTION_ROWS: &[(&str, &str, u64)] = &[
    ("none", "dut.ob = None;", 0),
    ("some_false", "dut.ob = Some(false);", 1),
    ("some_true", "dut.ob = Some(true);", 2),
];

/// `(case, setup, expected y)`.
const NESTED_ROWS: &[(&str, &str, u64)] = &[
    ("none", "dut.deep = None;", 0),
    ("some_ok", "dut.deep = Some(Ok(9));", 9),
    ("some_err", "dut.deep = Some(Err(0x7f));", 15),
];

/// `(case, setup, matched expression, arms, expected y, expected h)`: the rows
/// that need their own arm list instead of one of the shared ones.
const ARMS_ROWS: &[(&str, &str, &str, &str, u64, i64)] = &[
    // A local whose type the source writes out: `Option<bool>` and
    // `Option<uint<4>>` are two enums of this design, so the parameter list is
    // what resolves each one.
    (
        "local_option_bool",
        "ob_local = Some(true);",
        "ob_local",
        OPTION_BOOL_ARMS,
        2,
        0,
    ),
    (
        "local_option_uint",
        "opt_local = Some(3);",
        "opt_local",
        OPTION_UINT_ARMS,
        3,
        0,
    ),
    // One name bound from two variants; the guard holds on the first
    // alternative ...
    (
        "or_pattern_guard_holds",
        "dut.msg = Msg::Data(3);",
        "dut.msg",
        OR_ARMS,
        7,
        0,
    ),
    // ... and is false on the second variant, so the arm written after it takes
    // the value.
    (
        "or_pattern_guard_false_takes_the_later_arm",
        "dut.msg = Msg::Pair { hi: -1, lo: 2 };",
        "dut.msg",
        OR_ARMS,
        2,
        0,
    ),
    // One value matches the first alternative, where the guard is false, and the
    // next alternative binds the other element and holds the guard: the arm has
    // to try it instead of giving up on the whole arm.
    (
        "array_or_second_alternative_after_a_false_guard",
        "pair = [0, 5];",
        "pair",
        ARRAY_OR_ARMS,
        1,
        0,
    ),
    // The first alternative alone holds the guard.
    (
        "array_or_first_alternative",
        "pair = [5, 5];",
        "pair",
        ARRAY_OR_ARMS,
        1,
        0,
    ),
    // Neither alternative holds the guard, so the catch-all takes the value.
    (
        "array_or_neither_guard",
        "pair = [0, 0];",
        "pair",
        ARRAY_OR_ARMS,
        15,
        0,
    ),
    // The arm body writes the matched signal itself; the binding must keep the
    // value the match copied at the arm's entry.
    (
        "binding_survives_a_body_write",
        "dut.msg = Msg::Data(3);",
        "dut.msg",
        SNAPSHOT_ARMS,
        3,
        0,
    ),
];

/// One runtime case: drive the DUT, clear the result locals, run the arms, and
/// check what the taken arm wrote.
fn case(source: &mut String, name: &str, setup: &str, arms: &str, y: u64, h: i64) {
    use std::fmt::Write as _;
    let _ = write!(
        source,
        "        // case: {name}\n        {setup}\n        delay(ps(1));\n        y = 0;\n        \
         h = 0;\n{arms}        assert_eq!(y, {y});\n        assert_eq!(h, {h});\n        pass!();\n"
    );
}

/// The runtime `match` testbench. Every row runs the same arms for its scrutinee
/// type, so a FAIL line names the case in the comment above it.
fn match_test_source() -> String {
    let mut body = String::new();
    for (name, setup, scrutinee, y, h) in MSG_ROWS {
        let arms = MSG_ARMS.replace("{scrutinee}", scrutinee);
        case(&mut body, name, setup, &arms, *y, *h);
    }
    for (name, setup, y) in OPTION_ROWS {
        let arms = OPTION_BOOL_ARMS.replace("{scrutinee}", "dut.ob");
        case(&mut body, name, setup, &arms, *y, 0);
    }
    for (name, setup, y) in NESTED_ROWS {
        case(&mut body, name, setup, NESTED_ARMS, *y, 0);
    }
    for (name, setup, scrutinee, arms, y, h) in ARMS_ROWS {
        let arms = arms.replace("{scrutinee}", scrutinee);
        case(&mut body, name, setup, &arms, *y, *h);
    }
    body.push_str("        finish!();\n");
    format!(
        "#[hardware_test]\n\
         fn enums_match() {{\n    \
             let mut dut = Enums::new();\n    \
             let mut y: uint<4> = 0;\n    \
             let mut h: int<4> = 0;\n    \
             let mut i: uint<2> = 0;\n    \
             let mut ob_local: Option<bool> = None;\n    \
             let mut opt_local: Option<uint<4>> = None;\n    \
             let mut pair: [uint<4>; 2] = [0; 2];\n    \
             initial! {{\n        \
                 dut.msg = Msg::Idle;\n        \
                 dut.opt = None;\n        \
                 dut.ob = None;\n        \
                 dut.deep = None;\n        \
                 dut.unit = None;\n        \
                 delay(ps(1));\n\
         {body}    \
         }}\n\
         }}\n"
    )
}

/// Whether a diagnostic source must generate or must be rejected.
enum Expected {
    /// Inside the supported subset: `compile_testbench` must produce a testbench.
    Accepted,
    /// Must be rejected with a located error.
    Rejected,
}

/// `(case, statement body, expected)`. Every accepted row proves the shared
/// preamble, so a rejection can only come from the row's own difference: a
/// missing arm, a guard that covers nothing, a binding outside its arm, or a
/// comparison between two shapes that only happen to be the same width.
const DIAGNOSTIC_CASES: &[(&str, &str, Expected)] = &[
    (
        "msg_with_a_wildcard",
        r#"        match dut.msg {
            Msg::Idle => { y = 0; }
            Msg::Data(v) => { y = v; }
            Msg::Pair { hi, lo } => { y = lo; }
            _ => { y = 15; }
        }
"#,
        Expected::Accepted,
    ),
    (
        "msg_with_a_bare_binding",
        r#"        match dut.msg {
            Msg::Idle => { y = 0; }
            Msg::Data(v) => { y = v; }
            Msg::Pair { hi, lo } => { y = lo; }
            other => { y = 15; }
        }
"#,
        Expected::Accepted,
    ),
    (
        "msg_without_the_tag_hole_arm",
        r#"        match dut.msg {
            Msg::Idle => { y = 0; }
            Msg::Data(v) => { y = v; }
            Msg::Pair { hi, lo } => { y = lo; }
        }
"#,
        Expected::Rejected,
    ),
    (
        "option_bool_covering_every_variant",
        r#"        match dut.ob {
            None => { y = 0; }
            Some(false) => { y = 1; }
            Some(true) => { y = 2; }
        }
"#,
        Expected::Accepted,
    ),
    (
        "option_bool_missing_a_payload_value",
        r#"        match dut.ob {
            None => { y = 0; }
            Some(false) => { y = 1; }
        }
"#,
        Expected::Rejected,
    ),
    (
        "guarded_payload_without_a_covering_arm",
        r#"        match dut.opt {
            Some(v) if v > 0 => { y = v; }
            None => { y = 0; }
        }
"#,
        Expected::Rejected,
    ),
    (
        "guarded_payload_with_a_covering_arm",
        r#"        match dut.opt {
            Some(v) if v > 0 => { y = v; }
            Some(v) => { y = 15; }
            None => { y = 0; }
        }
"#,
        Expected::Accepted,
    ),
    (
        "nested_payload_patterns",
        r#"        match dut.deep {
            Some(Ok(v)) => { y = v; }
            Some(Err(e)) => { y = 15; }
            None => { y = 0; }
        }
"#,
        Expected::Accepted,
    ),
    (
        "arm_binding_out_of_its_arm",
        r#"        match dut.msg {
            Msg::Data(v) => { y = v; }
            Msg::Idle => { y = 0; }
            Msg::Pair { hi, lo } => { y = lo; }
            _ => { y = v; }
        }
"#,
        Expected::Rejected,
    ),
    // Two enum types of the same ten bits are not comparable, and neither is an
    // enum and the raw bits of the same width.
    (
        "different_enum_types_are_not_comparable",
        r#"        assert!(dut.deep == dut.msg);
"#,
        Expected::Rejected,
    ),
    (
        "an_enum_is_not_its_raw_bits",
        r#"        assert_eq!(dut.msg, dut.msg_raw);
"#,
        Expected::Rejected,
    ),
];

/// The diagnostic sources share everything but the body under test.
fn match_source(body: &str) -> String {
    format!(
        "#[hardware_test]\n\
         fn smoke() {{\n    \
             let mut dut = Enums::new();\n    \
             let mut y: uint<4> = 0;\n    \
             let mut h: int<4> = 0;\n    \
             initial! {{\n        \
                 dut.msg = Msg::Idle;\n        \
                 dut.opt = None;\n        \
                 dut.ob = None;\n        \
                 dut.deep = None;\n        \
                 dut.unit = None;\n        \
                 delay(ps(1));\n\
         {body}        \
             finish!();\n    \
         }}\n\
         }}\n"
    )
}

#[test]
fn enum_match_at_runtime_and_located_rejections() {
    // Runtime: every row drives the DUT, runs the same arms and checks the value
    // the taken arm wrote.
    let artifact = compile_testbench(
        &match_test_source(),
        "enums.shy",
        "enums_match",
        &enums_design(),
    )
    .expect("generate the match testbench");
    let stdout = run_testbench("gen_enums_match", &artifact);
    // 9 `Msg` rows (6 values plus 3 dynamic selections), 3 `dut.ob`, 3 `dut.deep`
    // and 8 rows with their own arms, all reached.
    assert!(
        stdout.contains("SHYHDL_SUMMARY test=enums_match pass=24 fail=0 markers=23"),
        "{stdout}"
    );
    assert!(!stdout.contains("SHYHDL_FAIL"), "{stdout}");

    // Diagnostics: a guard never contributes coverage, a tag that names no
    // variant is a hole a wildcard or a bare binding has to close, and a
    // comparison of two different shapes of one width is refused.
    for (name, body, expected) in DIAGNOSTIC_CASES {
        let source = match_source(body);
        match (
            expected,
            compile_testbench(&source, "enums.shy", "smoke", &enums_design()),
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
                assert_eq!(span.file, "enums.shy", "{name}: {error}");
                assert!(
                    span.line >= 2,
                    "{name}: the location must point inside the test: {error}"
                );
            }
        }
    }
}
