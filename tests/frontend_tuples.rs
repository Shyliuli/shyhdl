//! Tuple types and values, tuple field access, and `let` destructuring.
//!
//! A tuple is the struct layout with numeric field names, packed most
//! significant field first; `.0` is therefore the same `Component::Field` a
//! struct field is, and a destructured binding is a slice of the value.

use std::fmt::Write as _;

use shyhdl::frontend::{codes, compile, CompiledDesign, Diagnostic};
use shyhdl::metadata::{TypeField, ValueType};

/// A data struct declared before the module, for struct destructuring.
const CELL: &str = "struct Cell {\n    tag: int<4>,\n    val: uint<4>,\n}\n\n";

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
    match compile(source, "tuples.shy.rs", "Dut") {
        Ok(compiled) => compiled,
        Err(diagnostic) => panic!("expected this to compile, got {diagnostic}:\n{source}"),
    }
}

/// Compile a complete source that must fail with `code`.
fn reject(source: &str, code: &str) -> Diagnostic {
    match compile(source, "tuples.shy.rs", "Dut") {
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

/// The Verilog must contain `needle`.
fn contains(compiled: &CompiledDesign, needle: &str) {
    assert!(
        compiled.verilog.contains(needle),
        "expected `{needle}` in:\n{}",
        compiled.verilog
    );
}

/// The first generated line whose trimmed text starts with `prefix`.
fn line_starting_with<'a>(compiled: &'a CompiledDesign, prefix: &str) -> &'a str {
    compiled
        .verilog
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| {
            panic!(
                "expected a line starting with `{prefix}` in:\n{}",
                compiled.verilog
            )
        })
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

#[test]
fn a_tuple_type_is_a_struct_with_numeric_fields() {
    let compiled = accept(&dut(
        &["#[In] p: (uint<4>, bool)", "#[Out] y: uint<4>"],
        "y = p.0;",
    ));
    assert_eq!(
        port_ty(&compiled, "p"),
        ValueType::Struct {
            name: "(uint<4>, bool)".to_string(),
            fields: vec![
                TypeField {
                    name: "0".to_string(),
                    ty: ValueType::UInt { width: 4 },
                },
                TypeField {
                    name: "1".to_string(),
                    ty: ValueType::Bool,
                },
            ],
        }
    );
    assert_eq!(port_ty(&compiled, "p").bit_width().unwrap(), 5);
    contains(&compiled, "input wire [4:0] p");
}

#[test]
fn a_tuple_literal_packs_the_first_field_most_significant() {
    let compiled = accept(&dut(
        &[
            "#[In] a: uint<4>",
            "#[In] b: bool",
            "#[Out] t: (uint<4>, bool)",
        ],
        "t = (a, b);",
    ));
    contains(&compiled, "t = {a, b};");
}

#[test]
fn tuple_fields_can_be_read() {
    let compiled = accept(&dut(
        &[
            "#[In] p: (uint<4>, bool)",
            "#[Out] y: uint<4>",
            "#[Out] z: bool",
        ],
        "y = p.0;\nz = p.1;",
    ));
    contains(&compiled, "y = p[4:1];");
    contains(&compiled, "z = p[0];");
}

#[test]
fn tuple_fields_can_be_written() {
    let compiled = accept(&dut(
        &[
            "#[In] a: uint<4>",
            "#[In] f: bool",
            "#[Out] t: (uint<4>, bool)",
        ],
        "t.0 = a;\nt.1 = f;",
    ));
    contains(&compiled, "t[4:1] = a;");
    contains(&compiled, "t[0] = f;");
}

#[test]
fn a_nested_tuple_type_names_itself_and_reads_field_by_field() {
    let compiled = accept(&dut(
        &["#[In] p: ((uint<4>, bool), uint<8>)", "#[Out] y: uint<4>"],
        "y = p.0.0;",
    ));
    assert_eq!(
        port_ty(&compiled, "p"),
        ValueType::Struct {
            name: "((uint<4>, bool), uint<8>)".to_string(),
            fields: vec![
                TypeField {
                    name: "0".to_string(),
                    ty: ValueType::Struct {
                        name: "(uint<4>, bool)".to_string(),
                        fields: vec![
                            TypeField {
                                name: "0".to_string(),
                                ty: ValueType::UInt { width: 4 },
                            },
                            TypeField {
                                name: "1".to_string(),
                                ty: ValueType::Bool,
                            },
                        ],
                    },
                },
                TypeField {
                    name: "1".to_string(),
                    ty: ValueType::UInt { width: 8 },
                },
            ],
        }
    );
    // The nested tuple is the high five bits; its field 0 is the high four.
    contains(&compiled, "y = p[12:9];");
}

#[test]
fn tuple_destructuring_with_an_annotation_at_the_top_level() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] f: bool",
            "#[Out] y: uint<4>",
            "#[Out] z: bool",
        ],
        "let (a, b): (uint<4>, bool) = (x, f);\ny = a;\nz = b;",
    ));
    // A run-level `let` declares module signals, so the bindings are wires.
    contains(&compiled, "y = a;");
    contains(&compiled, "z = b;");
    assert!(
        line_starting_with(&compiled, "a = ").ends_with("[4:1];"),
        "the first tuple field is the high bits:\n{}",
        compiled.verilog
    );
    assert!(
        line_starting_with(&compiled, "b = ").ends_with("[0];"),
        "the second tuple field is the low bit:\n{}",
        compiled.verilog
    );
}

#[test]
fn tuple_destructuring_without_an_annotation() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] f: bool",
            "#[Out] y: uint<4>",
            "#[Out] z: bool",
        ],
        "let (a, b) = (x, f);\ny = a;\nz = b;",
    ));
    contains(&compiled, "y = a;");
    contains(&compiled, "z = b;");
    assert!(line_starting_with(&compiled, "a = ").ends_with("[4:1];"));
    assert!(line_starting_with(&compiled, "b = ").ends_with("[0];"));
}

#[test]
fn nested_tuple_destructuring() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] f: bool",
            "#[In] w: uint<8>",
            "#[Out] y: uint<4>",
            "#[Out] q: bool",
            "#[Out] z: uint<8>",
        ],
        "let ((a, b), c): ((uint<4>, bool), uint<8>) = ((x, f), w);\ny = a;\nq = b;\nz = c;",
    ));
    contains(&compiled, "y = a;");
    contains(&compiled, "q = b;");
    contains(&compiled, "z = c;");
    // The nested tuple is the high five bits; its own field 0 is the high four.
    assert!(line_starting_with(&compiled, "a = ").ends_with("[12:9];"));
    assert!(line_starting_with(&compiled, "b = ").ends_with("[8];"));
    assert!(line_starting_with(&compiled, "c = ").ends_with("[7:0];"));
}

#[test]
fn tuple_destructuring_inside_a_block_declares_locals() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] f: bool",
            "#[Out] y: uint<4>",
            "#[Out] z: bool",
        ],
        "if f {\n    let (a, b): (uint<4>, bool) = (x, f);\n    y = a;\n    z = b;\n} else {\n    y = 0;\n    z = f;\n}",
    ));
    contains(&compiled, "y = a;");
    contains(&compiled, "z = b;");
}

#[test]
fn tuple_destructuring_keeps_mut_bindings() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] f: bool",
            "#[Out] y: uint<4>",
            "#[Out] z: bool",
        ],
        "if f {\n    let (mut a, mut b): (uint<4>, bool) = (x, f);\n    a = a + 1;\n    b = !b;\n    y = a;\n    z = b;\n} else {\n    y = x;\n    z = f;\n}",
    ));
    contains(&compiled, "y = a;");
    contains(&compiled, "z = b;");
}

#[test]
fn struct_destructuring_binds_each_field() {
    let source = format!(
        "{CELL}{}",
        dut(
            &["#[In] c: Cell", "#[Out] y: uint<4>", "#[Out] t: int<4>"],
            "let Cell { tag, val } = c;\ny = val;\nt = tag;",
        )
    );
    let compiled = accept(&source);
    contains(&compiled, "y = val;");
    contains(&compiled, "t = tag;");
    assert!(line_starting_with(&compiled, "val = ").ends_with("[3:0];"));
    assert!(line_starting_with(&compiled, "tag = ").ends_with("[7:4];"));
}

#[test]
fn struct_destructuring_accepts_a_rest_pattern() {
    let source = format!(
        "{CELL}{}",
        dut(
            &["#[In] c: Cell", "#[Out] y: uint<4>"],
            "let Cell { val, .. } = c;\ny = val;",
        )
    );
    let compiled = accept(&source);
    contains(&compiled, "y = val;");
}

#[test]
fn a_tuple_pattern_must_match_the_tuple_arity() {
    let diagnostic = reject(
        &dut(
            &["#[In] x: uint<4>", "#[In] f: bool", "#[Out] y: uint<4>"],
            "let (a, b, c): (uint<4>, bool) = (x, f);\ny = a;",
        ),
        codes::WIDTH_MISMATCH,
    );
    assert!(
        diagnostic.message.contains("tuple pattern"),
        "the diagnostic should name the pattern: {diagnostic}"
    );
}

#[test]
fn a_tuple_literal_element_is_width_checked() {
    reject(
        &dut(
            &["#[In] x: uint<4>", "#[In] f: bool", "#[Out] y: uint<4>"],
            "let t: (uint<4>, uint<8>) = (x, f);\ny = x;",
        ),
        codes::WIDTH_MISMATCH,
    );
}

#[test]
fn an_unknown_tuple_field_is_reported() {
    let diagnostic = reject(
        &dut(
            &["#[In] p: (uint<4>, bool)", "#[Out] y: uint<4>"],
            "y = p.2;",
        ),
        codes::UNKNOWN_NAME,
    );
    assert!(
        diagnostic.message.contains("has no field `2`"),
        "the diagnostic should name the missing field: {diagnostic}"
    );
}

#[test]
fn the_unit_type_and_value_are_rejected() {
    reject(
        &dut(
            &["#[In] x: uint<4>", "#[Out] y: uint<4>"],
            "let t: () = ();\ny = x;",
        ),
        codes::BAD_TYPE,
    );
    reject(
        &dut(
            &["#[In] x: uint<4>", "#[Out] y: uint<4>"],
            "let t = ();\ny = x;",
        ),
        codes::BAD_TYPE,
    );
}

#[test]
fn an_enum_pattern_in_a_let_is_a_documented_gap() {
    let diagnostic = reject(
        &dut(
            &["#[In] x: uint<4>", "#[Out] y: uint<4>"],
            "let o: Option<uint<4>> = Some(x);\nlet Some(v) = o;\ny = v;",
        ),
        codes::UNSUPPORTED,
    );
    assert!(
        diagnostic.message.contains("match"),
        "the diagnostic should point at `match`: {diagnostic}"
    );
}

#[test]
fn a_named_enum_payload_pattern_in_a_let_is_a_documented_gap() {
    let diagnostic = reject(
        &dut(
            &["#[In] x: uint<4>", "#[Out] y: uint<4>"],
            "let o: Option<uint<4>> = Some(x);\nlet Option::Some { .. } = o;\ny = x;",
        ),
        codes::UNSUPPORTED,
    );
    assert!(
        diagnostic.message.contains("match"),
        "the diagnostic should point at `match`: {diagnostic}"
    );
}

#[test]
fn a_struct_pattern_must_bind_every_field_or_use_rest() {
    let source = format!(
        "{CELL}{}",
        dut(
            &["#[In] c: Cell", "#[Out] y: uint<4>"],
            "let Cell { tag } = c;\ny = c.val;",
        )
    );
    reject(&source, codes::DECLARATION);
}

#[test]
fn an_unknown_struct_pattern_field_is_reported() {
    let source = format!(
        "{CELL}{}",
        dut(
            &["#[In] c: Cell", "#[Out] y: uint<4>"],
            "let Cell { nope, .. } = c;\ny = c.val;",
        )
    );
    let diagnostic = reject(&source, codes::UNKNOWN_NAME);
    assert!(
        diagnostic.message.contains("has no field `nope`"),
        "the diagnostic should name the missing field: {diagnostic}"
    );
}

#[test]
fn a_struct_pattern_can_rename_and_nest() {
    let source = format!(
        "struct Packet {{\n    cell: Cell,\n    flag: bool,\n}}\n\n{CELL}{}",
        dut(
            &["#[In] p: Packet", "#[Out] y: uint<4>", "#[Out] q: bool"],
            "let Packet { cell: Cell { val, .. }, flag } = p;\ny = val;\nq = flag;",
        )
    );
    let compiled = accept(&source);
    contains(&compiled, "y = val;");
    contains(&compiled, "q = flag;");
    // `val` is the low four bits of the cell, which sits one bit above `flag`.
    assert!(line_starting_with(&compiled, "val = ").ends_with("[4:1];"));
}

#[test]
fn a_wildcard_inside_a_tuple_pattern_ignores_that_field() {
    let compiled = accept(&dut(
        &["#[In] x: uint<4>", "#[In] f: bool", "#[Out] y: uint<4>"],
        "let (a, _): (uint<4>, bool) = (x, f);\ny = a;",
    ));
    contains(&compiled, "y = a;");
}

#[test]
fn a_destructuring_let_without_an_initialiser_declares_its_bindings() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] f: bool",
            "#[Out] y: uint<4>",
            "#[Out] q: bool",
        ],
        "let (a, b): (uint<4>, bool);\na = x;\nb = f;\ny = a;\nq = b;",
    ));
    contains(&compiled, "y = a;");
    contains(&compiled, "q = b;");
}

#[test]
fn a_run_level_binding_may_not_redeclare_a_module_signal() {
    let diagnostic = reject(
        &dut(
            &["#[In] x: uint<4>", "#[In] f: bool", "#[Out] y: uint<4>"],
            "let (y, b): (uint<4>, bool) = (x, f);\ny = y;",
        ),
        codes::NAME_COLLISION,
    );
    assert!(
        diagnostic.message.contains("`y`"),
        "the diagnostic should name the colliding binding: {diagnostic}"
    );
}

#[test]
fn a_plain_let_mut_still_declares_a_local() {
    let compiled = accept(&dut(
        &["#[In] x: uint<4>", "#[Out] y: uint<4>"],
        "if x == 0 {\n    let mut t: uint<4> = 1;\n    t = t + 1;\n    y = t;\n} else {\n    y = x;\n}",
    ));
    contains(&compiled, "t = 4'd1;");
    contains(&compiled, "t = (t + 4'd1);");
    contains(&compiled, "y = t;");
}

/// A declared data struct that holds a tuple: the tuple is one more packed field.
const HOLDER: &str = "struct Holder {\n    pair: (uint<4>, bool),\n}\n\n";

#[test]
fn a_declared_struct_can_hold_a_tuple() {
    let source = format!(
        "{HOLDER}{}",
        dut(
            &[
                "#[In] a: uint<4>",
                "#[In] f: bool",
                "#[Out] h: Holder",
                "#[Out] y: uint<4>",
                "#[Out] q: bool",
            ],
            "h = Holder { pair: (a, f) };\ny = h.pair.0;\nq = h.pair.1;",
        )
    );
    let compiled = accept(&source);
    contains(&compiled, "h = {a, f};");
    contains(&compiled, "y = h[4:1];");
    contains(&compiled, "q = h[0];");
}

#[test]
fn tuples_work_as_array_elements() {
    let compiled = accept(&dut(
        &[
            "#[In] sel: uint<1>",
            "#[In] a: uint<4>",
            "#[In] f: bool",
            "#[In] b: uint<4>",
            "#[In] g: bool",
            "#[Out] y: uint<4>",
        ],
        "let xs: [(uint<4>, bool); 2] = [(a, f), (b, g)];\ny = xs[sel].0;",
    ));
    // Element 0 is the low five bits, element 1 the high five, and a field read
    // through the run-time index still selects the field's own four bits.
    contains(&compiled, "xs = {{b, g}, {a, f}};");
    assert!(
        compiled.verilog.contains("[4:1]"),
        "the field read must slice the selected element:\n{}",
        compiled.verilog
    );
}

#[test]
fn the_documented_tuple_destructuring_annotation_works() {
    let compiled = accept(&dut(
        &[
            "#[In] x: uint<4>",
            "#[In] w: uint<8>",
            "#[Out] y: uint<4>",
            "#[Out] z: uint<8>",
        ],
        "let (a, b): (uint<4>, uint<8>) = (x, w);\ny = a;\nz = b;",
    ));
    contains(&compiled, "y = a;");
    contains(&compiled, "z = b;");
    // Twelve bits, field 0 most significant: `a` is the high four, `b` the low eight.
    assert!(line_starting_with(&compiled, "a = ").ends_with("[11:8];"));
    assert!(line_starting_with(&compiled, "b = ").ends_with("[7:0];"));
}

#[test]
fn tuple_literal_elements_are_typed_by_the_annotation() {
    let compiled = accept(&dut(
        &["#[Out] y: uint<4>", "#[Out] z: uint<8>"],
        "let (a, b): (uint<4>, uint<8>) = (1, 2);\ny = a;\nz = b;",
    ));
    contains(&compiled, "a = ");
    contains(&compiled, "b = ");
    assert!(
        compiled.verilog.contains("4'd1") && compiled.verilog.contains("8'd2"),
        "each literal must be typed by its field:\n{}",
        compiled.verilog
    );
}

#[test]
fn a_tuple_rest_pattern_is_reported_clearly() {
    let diagnostic = reject(
        &dut(
            &["#[In] x: uint<4>", "#[In] f: bool", "#[Out] y: uint<4>"],
            "let (a, ..): (uint<4>, bool) = (x, f);\ny = a;",
        ),
        codes::UNSUPPORTED,
    );
    assert!(
        diagnostic.message.contains("rest pattern"),
        "the diagnostic should name the rest pattern: {diagnostic}"
    );
}

#[test]
fn a_destructuring_initialiser_with_control_flow_runs_once() {
    let compiled = accept(&dut(
        &[
            "#[In] f: bool",
            "#[In] x: uint<4>",
            "#[In] w: uint<4>",
            "#[Out] y: uint<4>",
            "#[Out] q: bool",
        ],
        "let (a, b): (uint<4>, bool) = if f { (x, f) } else { (w, f) };\ny = a;\nq = b;",
    ));
    assert_eq!(
        compiled.verilog.matches(" ? ").count(),
        1,
        "the initialiser must be lowered once, not once per binding:\n{}",
        compiled.verilog
    );
    contains(&compiled, "y = a;");
    contains(&compiled, "q = b;");
}
