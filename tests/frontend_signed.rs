//! Contract tests for signed values: `int<W>` is a `W`-bit two's-complement
//! value, the operations that have a signed reading keep it, and everything
//! that would silently read the same bits the other way is reported instead.
//!
//! What is pinned here is the frontend's public contract: which sources
//! compile, what type they produce (read from the typed metadata), and the
//! diagnostic code of the sources that must not compile. Bit-exact runtime
//! behaviour is the HDL acceptance oracle's job, not this file's.

use std::fmt::Write as _;

use shyhdl::frontend::{codes, compile, CompiledDesign, Diagnostic};
use shyhdl::metadata::ValueType;

/// A data struct declared before the module, for the cases that need one.
const PAIR: &str = "struct Pair {\n    lo: int<8>,\n    hi: uint<8>,\n}\n\n";

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
    match compile(source, "signed.shy.rs", "Dut") {
        Ok(compiled) => compiled,
        Err(diagnostic) => panic!("expected this to compile, got {diagnostic}:\n{source}"),
    }
}

/// Compile a complete source that must fail with `code`.
fn reject(source: &str, code: &str) -> Diagnostic {
    match compile(source, "signed.shy.rs", "Dut") {
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

#[test]
fn signed_arithmetic_and_bitwise_keep_the_signed_type() {
    // Same width and same sign, so the result is an `int<8>` again and assigns
    // to one. The type is read from the typed metadata, not from the RTL text.
    for operator in ["+", "-", "*", "&", "|", "^"] {
        let compiled = accept(&dut(
            &["#[In] a: int<8>", "#[In] b: int<8>", "#[Out] y: int<8>"],
            &format!("let result: int<8> = self.a {operator} self.b;\nself.y = result;"),
        ));
        assert_eq!(
            variable_ty(&compiled, "result"),
            ValueType::SInt { width: 8 },
            "`{operator}` on two int<8> values must stay signed"
        );
    }

    // Runtime negation is a signed operation at the operand's own width.
    let compiled = accept(&dut(
        &["#[In] a: int<8>", "#[Out] y: int<8>"],
        "let negated: int<8> = -self.a;\nself.y = negated;",
    ));
    assert_eq!(
        variable_ty(&compiled, "negated"),
        ValueType::SInt { width: 8 }
    );
}

#[test]
fn comparisons_are_signed_and_never_mix_signs() {
    // All six comparisons accept two `int<8>` operands and produce a `bool`.
    let compiled = accept(&dut(
        &[
            "#[In] a: int<8>",
            "#[In] b: int<8>",
            "#[Out] eq: bool",
            "#[Out] ne: bool",
            "#[Out] lt: bool",
            "#[Out] le: bool",
            "#[Out] gt: bool",
            "#[Out] ge: bool",
        ],
        "self.eq = self.a == self.b;\n\
         self.ne = self.a != self.b;\n\
         self.lt = self.a < self.b;\n\
         self.le = self.a <= self.b;\n\
         self.gt = self.a > self.b;\n\
         self.ge = self.a >= self.b;",
    ));
    for port in ["eq", "ne", "lt", "le", "gt", "ge"] {
        assert_eq!(port_ty(&compiled, port), ValueType::Bool, "{port}");
    }

    // A signed and an unsigned operand have no common reading: every operator
    // is reported instead of silently making the whole expression unsigned.
    for operator in ["==", "!=", "<", "<=", ">", ">="] {
        reject(
            &dut(
                &["#[In] a: int<8>", "#[In] b: uint<8>", "#[Out] y: bool"],
                &format!("self.y = self.a {operator} self.b;"),
            ),
            codes::WIDTH_MISMATCH,
        );
    }
    for operator in ["+", "-", "*", "&", "|", "^"] {
        reject(
            &dut(
                &["#[In] a: int<8>", "#[In] b: uint<8>", "#[Out] y: int<8>"],
                &format!("self.y = self.a {operator} self.b;"),
            ),
            codes::WIDTH_MISMATCH,
        );
    }

    // Assignment is not a reinterpretation either.
    reject(
        &dut(
            &["#[In] a: int<8>", "#[Out] y: uint<8>"],
            "self.y = self.a;",
        ),
        codes::WIDTH_MISMATCH,
    );
}

#[test]
fn a_literal_must_fit_the_target_signed_range() {
    for literal in ["0", "1", "127", "-128", "-1"] {
        accept(&dut(&["#[Out] y: int<8>"], &format!("self.y = {literal};")));
    }
    for literal in ["128", "255", "-129"] {
        reject(
            &dut(&["#[Out] y: int<8>"], &format!("self.y = {literal};")),
            codes::OUT_OF_RANGE,
        );
    }

    // A negative value has no unsigned reading, and the message names the type
    // that has one.
    let diagnostic = reject(
        &dut(&["#[Out] y: uint<8>"], "self.y = -1;"),
        codes::OUT_OF_RANGE,
    );
    assert!(diagnostic.to_string().contains("`int<"), "{diagnostic}");

    // A negative literal with nothing to infer a width from is not guessed.
    reject(
        &dut(
            &["#[Out] y: int<8>"],
            "let untyped = -1;\nself.y = untyped;",
        ),
        codes::WIDTH_UNKNOWN,
    );
}

#[test]
fn a_cast_reinterprets_bits_at_one_width_only() {
    // Equal width, either direction, and the same kind: the bits do not move.
    for (source_ty, target_ty, expected) in [
        ("int<8>", "uint<8>", ValueType::UInt { width: 8 }),
        ("uint<8>", "int<8>", ValueType::SInt { width: 8 }),
        ("int<8>", "int<8>", ValueType::SInt { width: 8 }),
        ("uint<8>", "uint<8>", ValueType::UInt { width: 8 }),
    ] {
        let ports = [
            format!("#[In] a: {source_ty}"),
            format!("#[Out] y: {target_ty}"),
        ];
        let ports: Vec<&str> = ports.iter().map(String::as_str).collect();
        let compiled = accept(&dut(&ports, &format!("self.y = self.a as {target_ty};")));
        assert_eq!(
            port_ty(&compiled, "y"),
            expected,
            "{source_ty} as {target_ty}"
        );
    }

    // The width never changes: that is an extension or a truncation, not a cast.
    for (source_ty, target_ty) in [("int<8>", "uint<16>"), ("uint<8>", "int<4>")] {
        let ports = [
            format!("#[In] a: {source_ty}"),
            format!("#[Out] y: {target_ty}"),
        ];
        let ports: Vec<&str> = ports.iter().map(String::as_str).collect();
        reject(
            &dut(&ports, &format!("self.y = self.a as {target_ty};")),
            codes::OUT_OF_RANGE,
        );
    }
    // The same rule for two unsigned widths, and the message says which
    // extension to write instead.
    let diagnostic = reject(
        &dut(
            &["#[In] a: uint<4>", "#[Out] y: uint<8>"],
            "self.y = self.a as uint<8>;",
        ),
        codes::OUT_OF_RANGE,
    );
    assert!(diagnostic.to_string().contains("zext"), "{diagnostic}");

    // Only integers have bits to reinterpret.
    reject(
        &dut(
            &["#[In] a: bool", "#[Out] y: int<8>"],
            "self.y = self.a as int<8>;",
        ),
        codes::BAD_TYPE,
    );
    reject(
        &dut(
            &["#[In] a: uint<8>", "#[Out] y: bool"],
            "self.y = self.a as bool;",
        ),
        codes::BAD_TYPE,
    );
    let source = format!(
        "{PAIR}{}",
        dut(
            &["#[In] p: Pair", "#[Out] y: int<8>"],
            "self.y = self.p as int<8>;"
        )
    );
    reject(&source, codes::BAD_TYPE);

    // A literal with no type of its own takes the cast's type, range check
    // included; a literal that has one is a bit pattern already.
    accept(&dut(&["#[Out] y: int<8>"], "self.y = 1 as int<8>;"));
    accept(&dut(&["#[Out] y: int<8>"], "self.y = -1 as int<8>;"));
    reject(
        &dut(&["#[Out] y: int<8>"], "self.y = 255 as int<8>;"),
        codes::OUT_OF_RANGE,
    );
    accept(&dut(&["#[Out] y: int<8>"], "self.y = 255u8 as int<8>;"));

    // A suffix that names an illegal width is a broken type, not a literal
    // without a type: it must not fall back to the width of the cast target.
    reject(
        &dut(&["#[Out] y: int<8>"], "self.y = 1u0 as int<8>;"),
        codes::BAD_TYPE,
    );
}

#[test]
fn extensions_keep_the_sign_and_a_raw_slice_drops_it() {
    let compiled = accept(&dut(
        &["#[In] a: int<8>", "#[Out] y: int<16>"],
        "let wide: int<16> = self.a.sext::<16>();\nself.y = wide;",
    ));
    assert_eq!(
        variable_ty(&compiled, "wide"),
        ValueType::SInt { width: 16 }
    );

    let compiled = accept(&dut(
        &["#[In] a: int<8>", "#[Out] y: int<4>"],
        "let narrow: int<4> = self.a.trunc::<4>();\nself.y = narrow;",
    ));
    assert_eq!(
        variable_ty(&compiled, "narrow"),
        ValueType::SInt { width: 4 }
    );

    // Zero extension would drop the sign, so it is refused and the message
    // points at the signed spelling.
    let diagnostic = reject(
        &dut(
            &["#[In] a: int<8>", "#[Out] y: int<16>"],
            "self.y = self.a.zext::<16>();",
        ),
        codes::BAD_TYPE,
    );
    assert!(diagnostic.to_string().contains("sext"), "{diagnostic}");
    // Widening with `trunc` or narrowing with `sext` is a range error.
    reject(
        &dut(
            &["#[In] a: int<8>", "#[Out] y: int<4>"],
            "self.y = self.a.sext::<4>();",
        ),
        codes::OUT_OF_RANGE,
    );
    reject(
        &dut(
            &["#[In] a: int<8>", "#[Out] y: int<16>"],
            "self.y = self.a.trunc::<16>();",
        ),
        codes::OUT_OF_RANGE,
    );

    // The unsigned extensions are unchanged.
    accept(&dut(
        &["#[In] a: uint<8>", "#[Out] y: uint<16>"],
        "self.y = self.a.zext::<16>();",
    ));
    accept(&dut(
        &["#[In] a: uint<8>", "#[Out] y: uint<16>"],
        "self.y = self.a.sext::<16>();",
    ));
    accept(&dut(
        &["#[In] a: uint<8>", "#[Out] y: uint<4>"],
        "self.y = self.a.trunc::<4>();",
    ));

    // A part-select is a raw bit pattern, so it is unsigned even out of a signed
    // value: it assigns to a `uint` target and not to an `int` one.
    accept(&dut(
        &["#[In] a: int<8>", "#[Out] y: uint<4>"],
        "self.y = self.a[0..4];",
    ));
    reject(
        &dut(
            &["#[In] a: int<8>", "#[Out] y: int<4>"],
            "self.y = self.a[0..4];",
        ),
        codes::WIDTH_MISMATCH,
    );
}

#[test]
fn an_arithmetic_shift_counts_unsigned() {
    // `>>` on a signed value keeps the signed type, which is what makes it an
    // arithmetic shift; the count is an unsigned value of its own.
    let compiled = accept(&dut(
        &["#[In] a: int<8>", "#[In] n: uint<3>", "#[Out] y: int<8>"],
        "let shifted: int<8> = self.a >> self.n;\nself.y = shifted;",
    ));
    assert_eq!(
        variable_ty(&compiled, "shifted"),
        ValueType::SInt { width: 8 }
    );
    assert!(
        compiled.verilog.contains("$signed("),
        "the signed operand must be shifted arithmetically:\n{}",
        compiled.verilog
    );

    // A constant count needs no type of its own.
    accept(&dut(
        &["#[In] a: int<8>", "#[Out] y: int<8>"],
        "self.y = self.a >> 1;",
    ));

    // A signed count has no shift meaning and is reported.
    reject(
        &dut(
            &["#[In] a: int<8>", "#[In] n: int<3>", "#[Out] y: int<8>"],
            "self.y = self.a >> self.n;",
        ),
        codes::BAD_TYPE,
    );
    // The same rule keeps a signed value out of an index.
    reject(
        &dut(
            &["#[In] a: uint<4>", "#[In] i: int<4>", "#[Out] y: bool"],
            "self.y = self.a[self.i];",
        ),
        codes::BAD_TYPE,
    );
}

#[test]
fn a_struct_field_keeps_the_sign_it_was_declared_with() {
    let source = format!(
        "{PAIR}{}",
        dut(
            &["#[In] p: Pair", "#[Out] y: int<8>", "#[Out] z: uint<8>"],
            "self.y = self.p.lo;\nself.z = self.p.hi;",
        )
    );
    let compiled = accept(&source);
    match port_ty(&compiled, "p") {
        ValueType::Struct { name, fields } => {
            assert_eq!(name, "Pair");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "lo");
            assert_eq!(fields[0].ty, ValueType::SInt { width: 8 });
            assert_eq!(fields[1].name, "hi");
            assert_eq!(fields[1].ty, ValueType::UInt { width: 8 });
        }
        other => panic!("`p` must be a struct value, got {other:?}"),
    }

    // Reading the signed field into an unsigned port is a signedness error,
    // which shows the field really is signed and not a raw 8-bit pattern.
    let source = format!(
        "{PAIR}{}",
        dut(
            &["#[In] p: Pair", "#[Out] z: uint<8>"],
            "self.z = self.p.lo;"
        )
    );
    reject(&source, codes::WIDTH_MISMATCH);
}

#[test]
fn signed_boundaries_stay_representable() {
    // `int<1>` holds -1 and 0, and comparing it against 0 is meaningful.
    accept(&dut(
        &["#[In] a: int<1>", "#[Out] y: int<1>", "#[Out] neg: bool"],
        "self.y = -1;\nself.neg = self.a < 0;",
    ));
    reject(
        &dut(&["#[Out] y: int<1>"], "self.y = 1;"),
        codes::OUT_OF_RANGE,
    );

    // A 127-bit zero and a 127-bit pattern must not be built with a signed shift
    // that overflows at bit 127.
    let compiled = accept(&dut(
        &["#[Out] y: uint<127>", "#[Out] z: int<127>"],
        "let zero: uint<127> = 0;\nself.y = zero;\nself.z = -1 as int<127>;",
    ));
    assert_eq!(
        variable_ty(&compiled, "zero"),
        ValueType::UInt { width: 127 }
    );
    accept(&dut(
        &["#[Out] y: uint<127>"],
        "self.y = (-1 as int<127>) as uint<127>;",
    ));

    // The same reinterpretation at 128 and 256 bits: a negative constant that
    // wide is beyond the compiler's own integer type, so it has to be built
    // rather than folded - and it is accepted, not reported as unimplemented.
    accept(&dut(
        &["#[Out] y: uint<128>"],
        "self.y = (-1 as int<128>) as uint<128>;",
    ));
    accept(&dut(
        &["#[Out] y: uint<256>"],
        "self.y = (-1 as int<256>) as uint<256>;",
    ));

    // A typed constant is a bit pattern at its own width: reinterpreting it is
    // equal-width, whatever value the bits spell.
    accept(&dut(&["#[Out] y: int<8>"], "self.y = 128u8 as int<8>;"));
    accept(&dut(&["#[Out] y: int<16>"], "self.y = 256u16 as int<16>;"));
}

#[test]
fn bits_another_process_drives_are_not_this_process_state() {
    let ports = [
        "#[In] a: bool",
        "#[In] b: bool",
        "#[In] i: uint<2>",
        "#[In] clk: bool",
        "#[Out] y: uint<2>",
    ];

    // The combinational process owns `y[1]` and reads the register's `y[0]`
    // through a dynamic index: feedback through a register, not a loop.
    accept(&dut(
        &ports,
        "self.y[1] = self.a;\n\
         self.y[1] = self.y[self.i];\n\
         always! {\n\
             #[Pedge] self.clk => {\n\
                 self.y[0] = self.b;\n\
             }\n\
         }",
    ));

    // The same read guarding a branch that writes only the combinational bit.
    accept(&dut(
        &ports,
        "self.y[1] = self.a;\n\
         if self.y[self.i] {\n\
             self.y[1] = self.a;\n\
         } else {\n\
             self.y[1] = self.b;\n\
         }\n\
         always! {\n\
             #[Pedge] self.clk => {\n\
                 self.y[0] = self.b;\n\
             }\n\
         }",
    ));

    // Reading the bit this process drives before assigning it is still the
    // process reading its own output, not another owner's state.
    reject(
        &dut(
            &ports,
            "if self.y[self.i] {\n\
                 self.y[1] = self.a;\n\
             } else {\n\
                 self.y[1] = self.b;\n\
             }\n\
             always! {\n\
                 #[Pedge] self.clk => {\n\
                     self.y[0] = self.b;\n\
                 }\n\
             }",
        ),
        codes::INCOMPLETE_PATH,
    );
}
