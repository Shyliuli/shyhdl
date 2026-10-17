//! Contract tests for user enums, `Option`/`Result` and `match` over them: the
//! layout and the variant table are read from the typed metadata, and every
//! source that must not compile is pinned by its diagnostic family.
//!
//! What is *not* here: the packed values, tag numbering in bits and what an
//! out-of-range tag reads. Those are the HDL acceptance oracle's job
//! (`tests/fixtures/hdl_acceptance/{enums,options,wrapped}.shy.rs`); this file
//! never mirrors generated RTL and never runs a simulator.
//!
//! Two things are deliberately left unpinned because the interface is not
//! settled: the `name` string a builtin `Option`/`Result` is described with
//! (only its variant table and width are asserted), and the code of a
//! payload-arity misuse (rejection plus a source location is asserted instead of
//! a guessed code).

use std::fmt::Write as _;

use shyhdl::frontend::{codes, compile, CompiledDesign, Diagnostic};
use shyhdl::metadata::{EnumVariant, TypeField, ValueType};

/// The user enum shared by the cases: a unit variant, a positional payload and a
/// named payload whose fields differ in signedness.
const MSG: &str =
    "enum Msg {\n    Idle,\n    Data(uint<4>),\n    Pair { hi: int<4>, lo: uint<4> },\n}\n\n";

/// A struct that holds an enum beside an unsigned field.
const ENVELOPE: &str = "struct Envelope {\n    msg: Msg,\n    extra: uint<2>,\n}\n\n";

/// One hardware module named `Dut`: these port declarations and this `run` body.
fn dut(ports: &[&str], body: &str) -> String {
    let mut source = String::from("#[hardware_instance]\nstruct Dut {\n");
    for port in ports {
        let _ = writeln!(source, "    {port},");
    }
    source.push_str("}\n\nimpl Hardware for Dut {\n    fn run(&mut self) {\n");
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let _ = writeln!(source, "        {line}");
    }
    source.push_str("    }\n}\n");
    source
}

/// Compile a complete source that must be accepted.
fn accept(source: &str) -> CompiledDesign {
    match compile(source, "enums.shy.rs", "Dut") {
        Ok(compiled) => compiled,
        Err(diagnostic) => panic!("expected this to compile, got {diagnostic}:\n{source}"),
    }
}

/// Compile a complete source that must fail with `code`.
fn reject(source: &str, code: &str) -> Diagnostic {
    match compile(source, "enums.shy.rs", "Dut") {
        Ok(compiled) => panic!("expected {code}, but it compiled:\n{}", compiled.verilog),
        Err(diagnostic) => {
            assert_eq!(diagnostic.code, code, "unexpected diagnostic: {diagnostic}");
            assert!(
                diagnostic.span.is_some(),
                "every diagnostic must be locatable: {diagnostic}"
            );
            diagnostic
        }
    }
}

/// Compile a complete source that must fail, without pinning the code: used
/// where a diagnostic family is not settled, where rejection with a location is
/// the contract.
fn reject_any(source: &str) -> Diagnostic {
    match compile(source, "enums.shy.rs", "Dut") {
        Ok(compiled) => panic!("expected an error, but it compiled:\n{}", compiled.verilog),
        Err(diagnostic) => {
            assert!(
                diagnostic.span.is_some(),
                "every diagnostic must be locatable: {diagnostic}"
            );
            diagnostic
        }
    }
}

/// The type of one port, as the debug metadata declares it.
fn port_ty(compiled: &CompiledDesign, name: &str) -> ValueType {
    compiled
        .top()
        .port(name)
        .unwrap_or_else(|| panic!("no port named `{name}`"))
        .ty
        .clone()
}

/// The type of one module signal, from the debug metadata.
fn variable_ty(compiled: &CompiledDesign, source_name: &str) -> ValueType {
    compiled
        .metadata
        .variables
        .iter()
        .find(|variable| variable.source_name == source_name)
        .unwrap_or_else(|| panic!("no metadata variable named `{source_name}`"))
        .ty
        .clone()
}

/// One enum variant, as the debug metadata spells it.
fn variant(name: &str, tag: u64, payload: Option<ValueType>) -> EnumVariant {
    EnumVariant {
        name: name.to_string(),
        tag,
        payload: payload.map(Box::new),
    }
}

/// One struct field, as the debug metadata spells it.
fn field(name: &str, ty: ValueType) -> TypeField {
    TypeField {
        name: name.to_string(),
        ty,
    }
}

/// The `Msg` enum as the debug metadata declares it.
fn msg_type() -> ValueType {
    ValueType::Enum {
        name: "Msg".to_string(),
        tag_width: 2,
        variants: vec![
            variant("Idle", 0, None),
            variant("Data", 1, Some(ValueType::UInt { width: 4 })),
            variant(
                "Pair",
                2,
                Some(ValueType::Struct {
                    name: "Msg::Pair".to_string(),
                    fields: vec![
                        field("hi", ValueType::SInt { width: 4 }),
                        field("lo", ValueType::UInt { width: 4 }),
                    ],
                }),
            ),
        ],
    }
}

/// An enum's tag width and its variants as `(name, tag, payload width)`. The
/// variant table is the contract; an enum's own `name` string is not part of it,
/// so a builtin whose description is not settled can still be asserted.
fn enum_shape(ty: &ValueType) -> (u32, Vec<(String, u64, Option<u32>)>) {
    match ty {
        ValueType::Enum {
            tag_width,
            variants,
            ..
        } => (
            *tag_width,
            variants
                .iter()
                .map(|variant| {
                    (
                        variant.name.clone(),
                        variant.tag,
                        variant
                            .payload
                            .as_ref()
                            .map(|payload| payload.bit_width().expect("a payload has a width")),
                    )
                })
                .collect(),
        ),
        other => panic!("expected an enum type, got {other:?}"),
    }
}

#[test]
fn an_enum_layout_is_the_metadata_contract() {
    // A user enum is one value: tag in the lowest bits, in declaration order from
    // zero, with the payload above it and unused payload bits zero. The metadata
    // carries exactly that.
    let compiled = accept(&format!("{MSG}{}", dut(&["#[In] msg: Msg"], "")));
    assert_eq!(port_ty(&compiled, "msg"), msg_type());
    // Tag 2 bits wide, widest payload (`Pair`) 8 bits.
    assert_eq!(compiled.top().port("msg").expect("msg").width, 10);
}

#[test]
fn option_and_result_layouts() {
    // The builtins are enums with the same layout rules; only the variant table
    // and the width are pinned (the description string of a builtin is not a
    // settled contract yet).
    let compiled = accept(&dut(
        &[
            "#[In] opt: Option<uint<4>>",
            "#[In] res: Result<uint<4>, int<8>>",
            "#[In] deep: Option<Result<uint<4>, int<8>>>",
        ],
        "",
    ));

    assert_eq!(
        enum_shape(&port_ty(&compiled, "opt")),
        (
            1,
            vec![
                ("None".to_string(), 0, None),
                ("Some".to_string(), 1, Some(4)),
            ]
        )
    );
    assert_eq!(
        enum_shape(&port_ty(&compiled, "res")),
        (
            1,
            vec![
                ("Ok".to_string(), 0, Some(4)),
                ("Err".to_string(), 1, Some(8)),
            ]
        )
    );
    assert_eq!(
        enum_shape(&port_ty(&compiled, "deep")),
        (
            1,
            vec![
                ("None".to_string(), 0, None),
                ("Some".to_string(), 1, Some(9)),
            ]
        )
    );

    // One tag bit over the widest case: 1 + 4, 1 + 8, and 1 + (1 + 8) nested.
    assert_eq!(port_ty(&compiled, "opt").bit_width().unwrap(), 5);
    assert_eq!(port_ty(&compiled, "res").bit_width().unwrap(), 9);
    assert_eq!(port_ty(&compiled, "deep").bit_width().unwrap(), 10);

    // The nested payload is itself the inner enum.
    let ValueType::Enum { variants, .. } = port_ty(&compiled, "deep") else {
        panic!("`deep` must be an enum");
    };
    let inner = variants[1]
        .payload
        .as_ref()
        .expect("`Some` carries a payload");
    assert_eq!(
        enum_shape(inner),
        (
            1,
            vec![
                ("Ok".to_string(), 0, Some(4)),
                ("Err".to_string(), 1, Some(8)),
            ]
        )
    );
}

#[test]
fn construction_is_typed_by_the_expected_type() {
    // A variant is built from the expected type: a bare `None`, a positional
    // payload, and a nested construction whose inner variant comes from the outer
    // expected type rather than from an annotation.
    let option = &["#[In] v: uint<4>", "#[Out] o: Option<uint<4>>"];
    accept(&dut(option, "self.o = None;"));
    accept(&dut(option, "self.o = Some(self.v);"));

    let result = &[
        "#[In] v: uint<4>",
        "#[In] s: int<8>",
        "#[Out] r: Result<uint<4>, int<8>>",
    ];
    accept(&dut(result, "self.r = Ok(self.v);"));
    accept(&dut(result, "self.r = Err(self.s);"));

    let deep = &[
        "#[In] v: uint<4>",
        "#[In] s: int<8>",
        "#[Out] d: Option<Result<uint<4>, int<8>>>",
    ];
    accept(&dut(deep, "self.d = Some(Ok(self.v));"));
    accept(&dut(deep, "self.d = Some(Err(self.s));"));
}

#[test]
fn a_match_binds_each_payload_with_its_own_type() {
    // Every payload spelling binds: nothing (unit), one positional value, and a
    // named payload whose fields keep their own signedness. The bindings are
    // proven by assigning them to exact-width ports.
    accept(&format!(
        "{MSG}{}",
        dut(
            &[
                "#[In] msg: Msg",
                "#[Out] y: uint<4>",
                "#[Out] hi: int<4>",
                "#[Out] seen: uint<4>",
            ],
            "match self.msg {\n\
                 Msg::Idle => { self.y = 0; self.hi = 0; self.seen = 0; }\n\
                 Msg::Data(v) => { self.y = v; self.hi = 0; self.seen = v; }\n\
                 Msg::Pair { hi, lo } => { self.y = lo; self.hi = hi; self.seen = lo; }\n\
                 _ => { self.y = 0xF; self.hi = 0; self.seen = 0xF; }\n\
             }",
        )
    ));
}

#[test]
fn a_guard_takes_the_first_true_arm() {
    let ports = &["#[In] msg: Msg", "#[Out] y: uint<4>", "#[Out] t: uint<2>"];

    // The guarded arm is written first and handles the `Data` values above two;
    // the unguarded arm below takes the rest. A guard that can never be true is
    // allowed - a redundant arm is not an error by itself - and its pattern still
    // binds (`v` is used in the body).
    accept(&format!(
        "{MSG}{}",
        dut(
            ports,
            "match self.msg {\n\
                 Msg::Data(v) if v > 2 => { self.y = v; self.t = 1; }\n\
                 Msg::Data(v) => { self.y = 1; self.t = 1; }\n\
                 Msg::Idle => { self.y = 0; self.t = 0; }\n\
                 Msg::Pair { hi, lo } => { self.y = lo; self.t = 2; }\n\
                 Msg::Data(v) if false => { self.y = v; self.t = 1; }\n\
                 _ => { self.y = 0xF; self.t = 3; }\n\
             }",
        )
    ));

    // A guard does not cover its variant: the values it rejects would have no arm
    // and the tag hole is still open, so this is not exhaustive.
    reject(
        &format!(
            "{MSG}{}",
            dut(
                ports,
                "match self.msg {\n\
                     Msg::Data(v) if v > 2 => { self.y = v; self.t = 1; }\n\
                     Msg::Idle => { self.y = 0; self.t = 0; }\n\
                     Msg::Pair { hi, lo } => { self.y = lo; self.t = 2; }\n\
                 }",
            )
        ),
        codes::UNSUPPORTED,
    );

    // The same rule without a tag hole to hide behind: `Option` has two variants
    // and one tag bit, so both variants are named and the guard alone is what
    // keeps the match from being total.
    reject(
        &dut(
            &["#[In] opt: Option<uint<4>>", "#[Out] y: uint<4>"],
            "match self.opt {\n\
                 None => { self.y = 0; }\n\
                 Some(v) if v > 2 => { self.y = v; }\n\
             }",
        ),
        codes::UNSUPPORTED,
    );
}

#[test]
fn unguarded_coverage_and_a_bare_binding() {
    let ports = &["#[In] ob: Option<bool>", "#[Out] y: uint<4>"];

    // Both payload values are spelled out, so the three arms are complete without
    // a `_`.
    accept(&dut(
        ports,
        "match self.ob {\n\
             None => { self.y = 0; }\n\
             Some(false) => { self.y = 1; }\n\
             Some(true) => { self.y = 2; }\n\
         }",
    ));
    // A bare binding is an irrefutable catch-all, like `_`.
    accept(&dut(
        ports,
        "match self.ob {\n\
             Some(true) => { self.y = 2; }\n\
             other => { self.y = 0; }\n\
         }",
    ));
    // `Some(true)` is missing, so the value it carries has no arm.
    reject(
        &dut(
            ports,
            "match self.ob {\n\
                 None => { self.y = 0; }\n\
                 Some(false) => { self.y = 1; }\n\
             }",
        ),
        codes::UNSUPPORTED,
    );

    // The same coverage rule applies inside a sequential process: the missing
    // variant is reported, and spelling every arm out is accepted.
    let seq = &[
        "#[In] ob: Option<bool>",
        "#[In] clk: bool",
        "#[Out] y: uint<4>",
    ];
    reject(
        &dut(
            seq,
            "always! {\n\
                 #[Pedge] self.clk => {\n\
                     match self.ob {\n\
                         None => { self.y = 0; }\n\
                     }\n\
                 }\n\
             }",
        ),
        codes::UNSUPPORTED,
    );
    accept(&dut(
        seq,
        "always! {\n\
             #[Pedge] self.clk => {\n\
                 match self.ob {\n\
                     None => { self.y = 0; }\n\
                     Some(false) => { self.y = 1; }\n\
                     Some(true) => { self.y = 2; }\n\
                 }\n\
             }\n\
         }",
    ));
}

#[test]
fn an_unused_tag_is_a_hole_that_needs_a_wildcard() {
    // Three variants need two tag bits, so one tag value has no variant: covering
    // every real variant is still not exhaustive.
    let ports = &["#[In] msg: Msg", "#[Out] y: uint<4>"];
    reject(
        &format!(
            "{MSG}{}",
            dut(
                ports,
                "match self.msg {\n\
                     Msg::Idle => { self.y = 0; }\n\
                     Msg::Data(v) => { self.y = v; }\n\
                     Msg::Pair { hi, lo } => { self.y = lo; }\n\
                 }",
            )
        ),
        codes::UNSUPPORTED,
    );
    // The `_` arm closes it.
    accept(&format!(
        "{MSG}{}",
        dut(
            ports,
            "match self.msg {\n\
                 Msg::Idle => { self.y = 0; }\n\
                 Msg::Data(v) => { self.y = v; }\n\
                 Msg::Pair { hi, lo } => { self.y = lo; }\n\
                 _ => { self.y = 0xF; }\n\
             }",
        )
    ));
}

#[test]
fn typed_equality_ignores_the_inactive_payload() {
    // Enum equality is typed, not a raw `==` over the packed bits: the variants
    // that are active decide. Which bits an inactive payload holds is the HDL
    // oracle's business.
    accept(&dut(
        &[
            "#[In] a: Option<uint<4>>",
            "#[In] b: Option<uint<4>>",
            "#[Out] same: bool",
            "#[Out] differ: bool",
        ],
        "self.same = self.a == self.b;\nself.differ = self.a != self.b;",
    ));

    // Wrapped in a struct and in an array, both sides typed.
    accept(&format!(
        "{MSG}{ENVELOPE}{}",
        dut(
            &[
                "#[In] ea: Envelope",
                "#[In] eb: Envelope",
                "#[In] ma: Msg",
                "#[In] mb: Msg",
                "#[In] aa: [Msg; 2]",
                "#[In] ab: [Msg; 2]",
                "#[Out] es: bool",
                "#[Out] ms: bool",
                "#[Out] arr_same: bool",
            ],
            "self.es = self.ea == self.eb;\n\
             self.ms = self.ma == self.mb;\n\
             self.arr_same = self.aa == self.ab;",
        )
    ));

    // Two different enum types never compare, even at the same packed width, and
    // an enum is not its raw bits.
    reject(
        &dut(
            &[
                "#[In] a: Option<uint<4>>",
                "#[In] b: Option<uint<8>>",
                "#[Out] same: bool",
            ],
            "self.same = self.a == self.b;",
        ),
        codes::WIDTH_MISMATCH,
    );
    reject(
        &dut(
            &[
                "#[In] a: Option<uint<4>>",
                "#[In] b: uint<5>",
                "#[Out] same: bool",
            ],
            "self.same = self.a == self.b;",
        ),
        codes::WIDTH_MISMATCH,
    );
}

#[test]
fn wrapped_enums_keep_their_shape() {
    // An enum inside a struct and inside an array keeps its variant table in the
    // metadata; the field order and the element order are the layout rules the
    // oracle checks.
    let compiled = accept(&format!(
        "{MSG}{ENVELOPE}{}",
        dut(&["#[In] env: Envelope", "#[In] arr: [Msg; 2]"], "")
    ));
    assert_eq!(
        port_ty(&compiled, "env"),
        ValueType::Struct {
            name: "Envelope".to_string(),
            fields: vec![
                field("msg", msg_type()),
                field("extra", ValueType::UInt { width: 2 }),
            ],
        }
    );
    assert_eq!(
        port_ty(&compiled, "arr"),
        ValueType::Array {
            element: Box::new(msg_type()),
            len: 2,
        }
    );
}

#[test]
fn enum_misuse_is_reported_with_a_location() {
    let ports = &["#[In] v: uint<4>", "#[In] s: int<8>", "#[Out] m: Msg"];

    // A variant that does not exist, and a field a named payload does not have.
    for (name, expression, mentioned) in [
        ("unknown_variant", "Msg::Nope", "Nope"),
        (
            "unknown_payload_field",
            "Msg::Pair { hi: self.s.trunc::<4>(), nope: self.v }",
            "nope",
        ),
    ] {
        let diagnostic = reject(
            &format!("{MSG}{}", dut(ports, &format!("self.m = {expression};"))),
            codes::UNKNOWN_NAME,
        );
        assert!(
            diagnostic.to_string().contains(mentioned),
            "{name}: {diagnostic}"
        );
    }

    // The payload of `Data` is a `uint<4>`; a signed eight-bit value does not fit.
    reject(
        &format!("{MSG}{}", dut(ports, "self.m = Msg::Data(self.s);")),
        codes::WIDTH_MISMATCH,
    );

    // A payload that is simply missing is rejected with a location; which family
    // reports it is not settled yet, so no code is pinned.
    reject_any(&format!("{MSG}{}", dut(ports, "self.m = Msg::Data;")));

    // The same in a pattern position: an unknown variant is named, not treated as
    // a catch-all.
    reject(
        &format!(
            "{MSG}{}",
            dut(
                &[
                    "#[In] src: Msg",
                    "#[In] v: uint<4>",
                    "#[In] s: int<8>",
                    "#[Out] m: Msg",
                ],
                "match self.src {\n\
                     Msg::Nope => { self.m = Msg::Idle; }\n\
                     _ => { self.m = Msg::Idle; }\n\
                 }",
            )
        ),
        codes::UNKNOWN_NAME,
    );
}

#[test]
fn an_expression_match_is_a_value() {
    // A `match` in value position: on the right of an assignment, as a `let`
    // initialiser (inferred from same-typed arms, and annotated), as an arm body
    // with statements and a tail, and as an operand of arithmetic. The expected
    // type reaches the arms, which is what gives a literal arm its width.
    let compiled = accept(&dut(
        &[
            "#[In] sel: uint<2>",
            "#[In] a: uint<4>",
            "#[In] b: uint<4>",
            "#[Out] rhs: uint<4>",
            "#[Out] inf_out: uint<4>",
            "#[Out] sized_out: uint<8>",
            "#[Out] block_tail: uint<4>",
            "#[Out] nested: uint<4>",
        ],
        "self.rhs = match self.sel { 0 => 10, _ => 15 };\n\
         let inf = match self.sel { 0 => self.a, _ => self.b };\n\
         self.inf_out = inf;\n\
         let sized: uint<8> = match self.sel { 0 => self.a.zext::<8>(), _ => self.b.zext::<8>() };\n\
         self.sized_out = sized;\n\
         self.block_tail = match self.sel { 0 => { let t: uint<4> = self.a; t + 1 }, _ => { let t: uint<4> = self.b; t + 2 } };\n\
         self.nested = (match self.sel { 0 => 1, _ => 2 }) + (match self.a { 0 => 3, _ => 4 });",
    ));
    // The arms give each `let` its type: two same-typed arms infer it, an
    // annotation imposes it.
    assert_eq!(variable_ty(&compiled, "inf"), ValueType::UInt { width: 4 });
    assert_eq!(
        variable_ty(&compiled, "sized"),
        ValueType::UInt { width: 8 }
    );
}

#[test]
fn an_assignment_match_writes_its_target_directly() {
    // A `match` that is the whole right-hand side writes its target from inside
    // each arm: no result temporary and no copy back.
    let compiled = accept(&dut(
        &[
            "#[In] sel: uint<2>",
            "#[In] a: uint<4>",
            "#[In] b: uint<4>",
            "#[Out] rhs: uint<4>",
        ],
        "self.rhs = match self.sel { 0 => self.a, _ => self.b };",
    ));
    assert!(
        compiled.verilog.contains("case (sel)"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("rhs = a;"),
        "{}",
        compiled.verilog
    );
    assert!(
        !compiled.verilog.contains("t_l"),
        "no temporary is needed:\n{}",
        compiled.verilog
    );

    // The same for a `let`: the initialiser's temporary becomes the binding.
    let compiled = accept(&dut(
        &[
            "#[In] sel: uint<2>",
            "#[In] a: uint<4>",
            "#[In] b: uint<4>",
            "#[Out] y: uint<4>",
        ],
        "let picked = match self.sel { 0 => self.a, _ => self.b };\nself.y = picked;",
    ));
    assert!(
        compiled.verilog.contains("reg [3:0] picked;"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("picked = a;"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("y = picked;"),
        "{}",
        compiled.verilog
    );
    assert!(
        !compiled.verilog.contains("t_l"),
        "the `let` must not copy a temporary:\n{}",
        compiled.verilog
    );

    // A `match` in the tail of another arm writes the same target, so a nested
    // value still needs no temporary of its own.
    let compiled = accept(&dut(
        &[
            "#[In] sel: uint<2>",
            "#[In] a: uint<4>",
            "#[Out] y: uint<4>",
        ],
        "self.y = match self.sel { 0 => match self.a { 0 => 1u4, _ => 2u4 }, _ => 3u4 };",
    ));
    assert!(
        compiled.verilog.contains("y = 4'd1;"),
        "{}",
        compiled.verilog
    );
    assert!(
        !compiled.verilog.contains("t_l"),
        "a nested arm value needs no temporary:\n{}",
        compiled.verilog
    );
}

#[test]
fn expression_match_patterns_take_guards_bindings_and_or() {
    // Value position uses the same patterns as statement position: a guard that
    // reads its binding, an or-alternative, and a typed enum scrutinee with all
    // three payload spellings.
    accept(&format!(
        "{MSG}{}",
        dut(
            &[
                "#[In] a: uint<4>",
                "#[In] b: uint<4>",
                "#[In] msg: Msg",
                "#[Out] guarded: uint<4>",
                "#[Out] enum_expr: uint<4>",
            ],
            "self.guarded = match self.a { x if x > 2 => x, 0 | 1 => 3, _ => self.b };\n\
             self.enum_expr = match self.msg { Msg::Idle => 0, Msg::Data(v) => v, Msg::Pair { hi, lo } => lo, _ => 0xF };",
        )
    ));
}

#[test]
fn expression_match_misuse_is_reported() {
    let ports = &[
        "#[In] sel: uint<2>",
        "#[In] a: uint<4>",
        "#[Out] y: uint<4>",
    ];

    // The two arms must agree on a type, and the existing width check says so.
    reject(
        &dut(
            ports,
            "self.y = match self.sel { 0 => self.a, _ => self.a.zext::<8>() };",
        ),
        codes::WIDTH_MISMATCH,
    );
    // Coverage is the statement rule: every value, or a wildcard/binding, and a
    // guard does not cover its variant.
    reject(
        &dut(ports, "self.y = match self.sel { 0 => 10, 1 => 15 };"),
        codes::UNSUPPORTED,
    );
    reject(
        &dut(ports, "self.y = match self.a { x if x > 2 => x };"),
        codes::UNSUPPORTED,
    );

    // An arm body that ends with a statement yields no value. Whether the tail is
    // missing or a semicolon closes the block, it is rejected with a location;
    // which family reports it is not pinned.
    reject_any(&dut(
        ports,
        "self.y = match self.sel { 0 => { let t: uint<4> = self.a; }, _ => 10 };",
    ));
    reject_any(&dut(
        ports,
        "self.y = match self.sel { 0 => { self.y = self.a; }, _ => 10 };",
    ));

    // With no expected type, unsized literals are not Rust `i32`: every hardware
    // width is explicit, so an all-literal value stays unknown.
    reject(
        &dut(
            ports,
            "let t = match self.sel { 0 => 1, _ => 2 };\nself.y = t;",
        ),
        codes::WIDTH_UNKNOWN,
    );
}
