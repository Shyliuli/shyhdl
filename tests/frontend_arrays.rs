//! Contract tests for fixed-length arrays: `[T; N]` is a type of its own whose
//! *shape* - element type and length, at every nesting level - is what has to
//! match, and everything that cannot be represented is reported.
//!
//! The typed metadata carries the shape (`ValueType::Array`); the diagnostics
//! carry the rules. The packed values themselves (which element sits in which
//! bits, what an out-of-range runtime index reads) are the HDL acceptance
//! oracle's job, not this file's.

use std::fmt::Write as _;

use shyhdl::frontend::{codes, compile, CompiledDesign, Diagnostic};
use shyhdl::metadata::{TypeField, ValueType};

/// A data struct declared before the module, for the cases that need one.
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
    match compile(source, "arrays.shy.rs", "Dut") {
        Ok(compiled) => compiled,
        Err(diagnostic) => panic!("expected this to compile, got {diagnostic}:\n{source}"),
    }
}

/// Compile a complete source that must fail with `code`.
fn reject(source: &str, code: &str) -> Diagnostic {
    match compile(source, "arrays.shy.rs", "Dut") {
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

/// `[element; len]`, as the debug metadata spells an array type.
fn array_of(element: ValueType, len: usize) -> ValueType {
    ValueType::Array {
        element: Box::new(element),
        len,
    }
}

/// One struct field, as the debug metadata spells it.
fn field(name: &str, ty: ValueType) -> TypeField {
    TypeField {
        name: name.to_string(),
        ty,
    }
}

#[test]
fn an_array_keeps_its_exact_shape_in_the_metadata() {
    // Element 0 sits in the lowest bits and each next element above it, so the
    // shape is `[element; len]` all the way down, and an element keeps its own
    // signedness.
    let compiled = accept(&dut(
        &[
            "#[In] flat: [uint<4>; 3]",
            "#[In] nested: [[uint<4>; 2]; 2]",
            "#[In] signed_elements: [int<4>; 2]",
            "#[In] single: [bool; 1]",
            "#[In] signed_single: [int<1>; 1]",
        ],
        "",
    ));
    let element = ValueType::UInt { width: 4 };
    assert_eq!(port_ty(&compiled, "flat"), array_of(element.clone(), 3));
    assert_eq!(
        port_ty(&compiled, "nested"),
        array_of(array_of(element, 2), 2)
    );
    // `int<W>` elements stay signed values, not raw patterns.
    assert_eq!(
        port_ty(&compiled, "signed_elements"),
        array_of(ValueType::SInt { width: 4 }, 2)
    );
    // A one-element array is an ordinary array, for `bool` and for `int<1>`.
    assert_eq!(port_ty(&compiled, "single"), array_of(ValueType::Bool, 1));
    assert_eq!(
        port_ty(&compiled, "signed_single"),
        array_of(ValueType::SInt { width: 1 }, 1)
    );

    // The packed width is the element width times the length, at every level.
    let top = compiled.top();
    assert_eq!(top.port("flat").expect("flat").width, 12);
    assert_eq!(top.port("nested").expect("nested").width, 16);
    assert_eq!(top.port("signed_single").expect("signed_single").width, 1);
}

#[test]
fn arrays_compose_with_structs_by_shape() {
    // An array of structs and a struct holding an array are values of their own:
    // the metadata keeps the struct fields and the array shape inside them.
    let source = format!(
        "{CELL}struct Bundle {{\n    items: [uint<4>; 2],\n    flag: bool,\n}}\n\n{}",
        dut(
            &[
                "#[In] cells: [Cell; 2]",
                "#[In] b: Bundle",
                "#[Out] tags: [int<4>; 2]",
            ],
            "self.tags = [self.cells[0].tag, self.cells[1].tag];",
        )
    );
    let compiled = accept(&source);
    assert_eq!(
        port_ty(&compiled, "cells"),
        array_of(
            ValueType::Struct {
                name: "Cell".to_string(),
                fields: vec![
                    field("tag", ValueType::SInt { width: 4 }),
                    field("val", ValueType::UInt { width: 4 }),
                ],
            },
            2,
        )
    );
    assert_eq!(
        port_ty(&compiled, "b"),
        ValueType::Struct {
            name: "Bundle".to_string(),
            fields: vec![
                field("items", array_of(ValueType::UInt { width: 4 }, 2)),
                field("flag", ValueType::Bool),
            ],
        }
    );
    // Reading a signed field through an array index keeps the sign.
    assert_eq!(
        port_ty(&compiled, "tags"),
        array_of(ValueType::SInt { width: 4 }, 2)
    );
}

#[test]
fn list_repeat_and_copy_need_the_exact_shape() {
    // A repeat, a list and a whole-array copy of the same shape write an array.
    accept(&dut(
        &[
            "#[In] v: uint<4>",
            "#[Out] repeated: [uint<4>; 3]",
            "#[Out] listed: [uint<4>; 3]",
            "#[Out] copied: [uint<4>; 3]",
        ],
        "self.repeated = [self.v; 3];\n\
         self.listed = [self.v, 0x1, 0x2];\n\
         self.copied = self.listed;\n\
         self.listed[1] = 0x7;",
    ));

    // The same number of bits in a different shape is a different value: 16 bits
    // as four elements is not 16 bits as two arrays of two.
    reject(
        &dut(
            &[
                "#[In] flat: [uint<4>; 4]",
                "#[Out] nested: [[uint<4>; 2]; 2]",
            ],
            "self.nested = self.flat;",
        ),
        codes::WIDTH_MISMATCH,
    );
    reject(
        &dut(
            &["#[In] items: [uint<4>; 2]", "#[Out] y: uint<8>"],
            "self.y = self.items;",
        ),
        codes::WIDTH_MISMATCH,
    );

    // An aggregate holding an array is compared by the shape inside it, not by
    // its total width: a struct with an array field is not a struct with two
    // scalar fields that happen to pack to the same 8 bits.
    let shapes = "struct Wrap {\n    items: [uint<4>; 2],\n}\n\n\
                  struct Pair {\n    lo: uint<4>,\n    hi: uint<4>,\n}\n\n";
    let source = format!(
        "{shapes}{}",
        dut(
            &["#[In] pair: Pair", "#[Out] wrap: Wrap"],
            "self.wrap = self.pair;",
        )
    );
    reject(&source, codes::WIDTH_MISMATCH);
}

#[test]
fn a_constant_index_and_a_bare_range_read_bits_not_a_sub_array() {
    // A bare range is a raw bit pattern of the packed value, whatever the
    // receiver is; the sub-array is the `.slice(lo, hi)` method. A single index
    // is still the element.
    let compiled = accept(&dut(
        &[
            "#[In] src: [uint<4>; 3]",
            "#[Out] sub: [uint<4>; 2]",
            "#[Out] bits: uint<2>",
            "#[Out] elem: uint<4>",
        ],
        "let part: [uint<4>; 2] = self.src.slice(0, 2);\n\
         self.sub = part;\n\
         let raw = self.src[0..2];\n\
         self.bits = raw;\n\
         let first: uint<4> = self.src[0];\n\
         self.elem = first;",
    ));
    assert_eq!(
        variable_ty(&compiled, "part"),
        array_of(ValueType::UInt { width: 4 }, 2)
    );
    assert_eq!(variable_ty(&compiled, "raw"), ValueType::UInt { width: 2 });
    assert_eq!(
        variable_ty(&compiled, "first"),
        ValueType::UInt { width: 4 }
    );

    // The same at one element and one bit: `.slice(0, 1)` is an array of one
    // element (never a scalar), while `[0..1]` is one bit - and a one-bit range
    // is a `bool`, the length-based rule the language already had. The local is
    // deliberately unannotated so the assertion is about the range's own type.
    let compiled = accept(&dut(
        &[
            "#[In] src: [uint<4>; 3]",
            "#[Out] single: [uint<4>; 1]",
            "#[Out] bit_out: bool",
        ],
        "let one: [uint<4>; 1] = self.src.slice(0, 1);\n\
         self.single = one;\n\
         let low = self.src[0..1];\n\
         self.bit_out = low;",
    ));
    assert_eq!(
        variable_ty(&compiled, "one"),
        array_of(ValueType::UInt { width: 4 }, 1)
    );
    assert_eq!(
        port_ty(&compiled, "single"),
        array_of(ValueType::UInt { width: 4 }, 1)
    );
    assert_eq!(variable_ty(&compiled, "low"), ValueType::Bool);

    // Both spellings are places too, not only reads: a sub-array assigns whole
    // elements, a bare range assigns the packed bits of the array.
    accept(&dut(
        &["#[In] src: [uint<4>; 3]", "#[Out] dst: [uint<4>; 3]"],
        "self.dst = self.src;\n\
         self.dst.slice(0, 2) = self.src.slice(1, 3);\n\
         self.dst[8..12] = self.src[4..8];",
    ));

    // Constant selections outside what they select are reported: a sub-array is
    // bounded by the array's length, a bare range by the packed width.
    reject(
        &dut(
            &["#[In] src: [uint<4>; 3]", "#[Out] y: [uint<4>; 2]"],
            "self.y = self.src.slice(0, 4);",
        ),
        codes::OUT_OF_RANGE,
    );
    reject(
        &dut(
            &["#[In] src: [uint<4>; 3]", "#[Out] y: uint<13>"],
            "self.y = self.src[0..13];",
        ),
        codes::OUT_OF_RANGE,
    );
    reject(
        &dut(
            &["#[In] src: [uint<4>; 3]", "#[Out] y: uint<4>"],
            "self.y = self.src[3];",
        ),
        codes::OUT_OF_RANGE,
    );
    reject(
        &dut(
            &["#[In] src: [uint<4>; 3]", "#[Out] y: [uint<4>; 3]"],
            "self.y[3] = 0x1;",
        ),
        codes::OUT_OF_RANGE,
    );
    // The end of an inclusive range that overflows is reported, not wrapped.
    reject(
        &dut(
            &["#[In] src: [uint<4>; 3]", "#[Out] y: uint<2>"],
            "self.y = self.src[0..=18446744073709551615];",
        ),
        codes::OUT_OF_RANGE,
    );

    // `.slice` selects a sub-array, so an integer - which has no sub-array - is
    // rejected and pointed at the bracket form `[lo..hi]`.
    reject(
        &dut(
            &["#[In] u: uint<8>", "#[Out] y: uint<4>"],
            "self.y = self.u.slice(0, 4);",
        ),
        codes::UNSUPPORTED,
    );
}

#[test]
fn a_runtime_index_reads_one_element_of_any_unsigned_width() {
    // The index is an unsigned value, and it may be wider than the array needs:
    // index 3 has no element and is handled at runtime, not at build time.
    accept(&dut(
        &[
            "#[In] src: [uint<4>; 3]",
            "#[In] i: uint<2>",
            "#[Out] y: uint<4>",
        ],
        "self.y = self.src[self.i];",
    ));
    accept(&dut(
        &[
            "#[In] src: [uint<4>; 3]",
            "#[In] i: uint<8>",
            "#[Out] y: uint<4>",
        ],
        "self.y = self.src[self.i];",
    ));
    // A signed index is not an index.
    reject(
        &dut(
            &[
                "#[In] src: [uint<4>; 3]",
                "#[In] i: int<2>",
                "#[Out] y: uint<4>",
            ],
            "self.y = self.src[self.i];",
        ),
        codes::BAD_TYPE,
    );

    // A dynamic write on top of a whole-array default, with the element read
    // back in the same process: source order decides what that read sees.
    accept(&dut(
        &[
            "#[In] v: uint<4>",
            "#[In] i: uint<2>",
            "#[Out] out: [uint<4>; 3]",
            "#[Out] read_back: uint<4>",
        ],
        "self.out = [0x1, 0x2, 0x3];\n\
         self.out[self.i] = self.v;\n\
         self.read_back = self.out[self.i];",
    ));
}

#[test]
fn a_dynamic_write_drives_every_element_first() {
    let ports = &[
        "#[In] v: uint<4>",
        "#[In] i: uint<2>",
        "#[Out] out: [uint<4>; 3]",
    ];

    // The elements the index does not select would keep their old value, which
    // is exactly the latch the default combinational process must never have.
    let diagnostic = reject(
        &dut(ports, "self.out[self.i] = self.v;"),
        codes::INCOMPLETE_PATH,
    );
    assert!(diagnostic.to_string().contains("out"), "{diagnostic}");

    // Defaulted first, the same write is legal and is not misread as the process
    // feeding itself.
    accept(&dut(
        ports,
        "self.out = [0x1, 0x2, 0x3];\nself.out[self.i] = self.v;",
    ));

    // A runtime index may select a whole sub-array, and a second one may go
    // inside it: each level has its own index, and the whole array is still
    // defaulted first.
    accept(&dut(
        &[
            "#[In] v: uint<4>",
            "#[In] sub_v: [uint<4>; 2]",
            "#[In] i: uint<1>",
            "#[In] j: uint<1>",
            "#[Out] nested: [[uint<4>; 2]; 2]",
        ],
        "self.nested = [[0x1, 0x2], [0x3, 0x4]];\n\
         self.nested[self.i] = self.sub_v;\n\
         self.nested[self.i][self.j] = self.v;",
    ));

    // The same through a struct field: the array is defaulted as a whole, then
    // one dynamically selected element's fields are written.
    accept(&format!(
        "{CELL}{}",
        dut(
            &[
                "#[In] seed: [Cell; 2]",
                "#[In] i: uint<1>",
                "#[In] tag: int<4>",
                "#[In] val: uint<4>",
                "#[Out] out: [Cell; 2]",
            ],
            "self.out = self.seed;\n\
             self.out[self.i].tag = self.tag;\n\
             self.out[self.i].val = self.val;",
        )
    ));
}

#[test]
fn array_elements_may_have_different_owners() {
    // Statically disjoint elements: the default combinational process owns
    // `a[0]`, the register owns `a[1]`.
    accept(&dut(
        &[
            "#[In] v: uint<4>",
            "#[In] clk: bool",
            "#[Out] a: [uint<4>; 2]",
        ],
        "self.a[0] = self.v;\n\
         always! {\n\
             #[Pedge] self.clk => {\n\
                 self.a[1] = self.v;\n\
             }\n\
         }",
    ));

    // A dynamic index carries its own reach: with a one-bit index into three
    // elements it can only ever select `a[0]` or `a[1]`, so the register's
    // `a[2]` is not this process's element and there is no conflict. Treating
    // the write as reaching every element - or truncating the index - would
    // invent an owner for `a[2]`.
    accept(&dut(
        &[
            "#[In] v: uint<4>",
            "#[In] i: uint<1>",
            "#[In] clk: bool",
            "#[Out] a: [uint<4>; 3]",
        ],
        "self.a[0] = 0;\n\
         self.a[1] = 0;\n\
         self.a[self.i] = self.v;\n\
         always! {\n\
             #[Pedge] self.clk => {\n\
                 self.a[2] = self.v;\n\
             }\n\
         }",
    ));

    // The same element from two processes is the usual multi-driver error.
    reject(
        &dut(
            &[
                "#[In] v: uint<4>",
                "#[In] clk: bool",
                "#[Out] a: [uint<4>; 2]",
            ],
            "self.a[0] = self.v;\n\
             always! {\n\
                 #[Pedge] self.clk => {\n\
                     self.a[0] = self.v;\n\
                 }\n\
             }",
        ),
        codes::MULTI_DRIVER,
    );

    // Reading the array before this process assigns its elements is still the
    // process reading its own output, even though the index is dynamic.
    reject(
        &dut(
            &[
                "#[In] v: uint<4>",
                "#[In] i: uint<1>",
                "#[Out] y: uint<4>",
                "#[Out] a: [uint<4>; 2]",
            ],
            "self.y = self.a[self.i];\n\
             self.a[0] = self.v;\n\
             self.a[1] = self.v;",
        ),
        codes::INCOMPLETE_PATH,
    );
}

#[test]
fn array_limits_are_rejected_not_wrapped() {
    // An array is a type, so its limits are the type's limits: at least one
    // element, at most the supported width (65536 bits, the same cap `uint<W>`
    // has). The element count can never exceed that cap without the width
    // exceeding it too, so one shape covers both.
    for ty in [
        "[uint<4>; 0]",
        "[uint<65536>; 2]",
        "[uint<65536>; 65536]",
        "[bool; 65537]",
    ] {
        let ports = [format!("#[Out] y: {ty}")];
        let ports: Vec<&str> = ports.iter().map(String::as_str).collect();
        reject(&dut(&ports, ""), codes::BAD_TYPE);
    }
}

#[test]
fn an_array_is_not_an_integer_for_a_cast() {
    // `as` reinterprets integer bits at one width; an array is a shape, so
    // neither side of a cast may be one, however the total widths line up.
    reject(
        &dut(
            &["#[In] a: [uint<4>; 2]", "#[Out] y: uint<8>"],
            "self.y = self.a as uint<8>;",
        ),
        codes::BAD_TYPE,
    );
    reject(
        &dut(
            &["#[In] a: uint<8>", "#[Out] y: [uint<4>; 2]"],
            "self.y = self.a as [uint<4>; 2];",
        ),
        codes::BAD_TYPE,
    );
}

#[test]
fn a_runtime_index_may_select_a_sub_array() {
    // A runtime index on a nested array picks the whole sub-array, and a second
    // runtime index picks inside it. Every level has its own bounds, so the
    // lower level must not spill into its neighbour; which element an
    // out-of-range index reads is the HDL oracle's business, and what this file
    // pins is that the source type-checks and produces the sub-array's shape.
    let compiled = accept(&dut(
        &[
            "#[In] nested: [[int<4>; 2]; 2]",
            "#[In] i: uint<1>",
            "#[In] j: uint<1>",
            "#[Out] sub: [int<4>; 2]",
            "#[Out] elem: int<4>",
        ],
        "let whole: [int<4>; 2] = self.nested[self.i];\n\
         self.sub = whole;\n\
         let inner: int<4> = self.nested[self.i][self.j];\n\
         self.elem = inner;",
    ));
    assert_eq!(
        variable_ty(&compiled, "whole"),
        array_of(ValueType::SInt { width: 4 }, 2)
    );
    assert_eq!(
        variable_ty(&compiled, "inner"),
        ValueType::SInt { width: 4 }
    );

    // An index wider than the level needs is the same operation: the level's own
    // bounds decide the element, not the width of the index.
    accept(&dut(
        &[
            "#[In] nested: [[uint<4>; 2]; 2]",
            "#[In] i: uint<8>",
            "#[In] j: uint<8>",
            "#[Out] elem: uint<4>",
        ],
        "self.elem = self.nested[self.i][self.j];",
    ));

    // A constant level and a runtime level mix in either order.
    accept(&dut(
        &[
            "#[In] nested: [[uint<4>; 2]; 2]",
            "#[In] i: uint<1>",
            "#[Out] sub: [uint<4>; 2]",
            "#[Out] elem: uint<4>",
        ],
        "self.sub = self.nested[0];\n\
         self.elem = self.nested[0][self.i];",
    ));
}
