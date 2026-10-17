//! Bit-level patterns: inclusive ranges and `mask!` bit strings.
//!
//! A range says "this field is between these constants"; a mask says "these bits
//! are this value, the rest do not matter". A `match` whose arms are all plain
//! bit patterns is one `case`, and a mask makes it `casez` with `?` in the arm
//! value; a range keeps the explicit comparison chain, because a `case` arm
//! compares one value.

use shyhdl::frontend::{codes, compile, CompiledDesign};

fn dut(body: &str) -> String {
    format!(
        "#[hardware_instance]\nstruct P {{\n    #[In] v: uint<8>,\n    #[Out] y: uint<4>,\n}}\n\nimpl Hardware for P {{\n    fn run(&mut self) {{\n{body}\n    }}\n}}\n"
    )
}

fn accept(body: &str) -> CompiledDesign {
    let source = dut(body);
    match compile(&source, "patterns.shy.rs", "P") {
        Ok(compiled) => compiled,
        Err(diagnostic) => panic!("expected this to compile, got {diagnostic}:\n{source}"),
    }
}

fn reject(body: &str, code: &str) {
    let source = dut(body);
    match compile(&source, "patterns.shy.rs", "P") {
        Ok(compiled) => panic!("expected {code}, got:\n{}", compiled.verilog),
        Err(diagnostic) => {
            assert_eq!(diagnostic.code, code, "unexpected diagnostic: {diagnostic}");
            assert!(diagnostic.span.is_some(), "the error must be located");
        }
    }
}

#[test]
fn ranges_are_total_without_a_wildcard() {
    // Two aligned ranges cover the whole eight-bit domain on their own, so no
    // `_` is needed and the coverage proof is an interval union.
    let compiled = accept(
        "match self.v {\n\
             0x00..=0x7F => { self.y = 1; }\n\
             0x80..=0xFF => { self.y = 2; }\n\
         }",
    );
    assert!(compiled.verilog.contains(">= 8'd0"));
    assert!(compiled.verilog.contains("<= 8'd127"));

    // A hole between the ranges is a real gap.
    reject(
        "match self.v {\n\
             0x00..=0x0F => { self.y = 1; }\n\
             0x20..=0xFF => { self.y = 2; }\n\
         }",
        codes::UNSUPPORTED,
    );
}

#[test]
fn ranges_keep_the_comparison_chain() {
    // A range is not one value, so a `case` arm cannot spell it: the arms stay an
    // `if` chain, and the matched signal is read directly rather than through a
    // snapshot temporary.
    let compiled = accept(
        "match self.v {\n\
             0x00..=0x7F => { self.y = 1; }\n\
             0x80..=0xFF => { self.y = 2; }\n\
         }",
    );
    assert!(!compiled.verilog.contains("case"), "{}", compiled.verilog);
    assert!(!compiled.verilog.contains("case"), "{}", compiled.verilog);
    assert!(compiled.verilog.contains("if ("), "{}", compiled.verilog);
    assert!(
        !compiled.verilog.contains("t_l"),
        "the matched signal needs no snapshot:\n{}",
        compiled.verilog
    );
}

#[test]
fn masks_compare_only_the_named_bits() {
    // A mask with a wildcard arm compiles to one `casez`: the bits the mask names
    // are compared, the others are `?`.
    let compiled = accept(
        "match self.v {\n\
             mask!(0b1111????) => { self.y = 1; }\n\
             mask!(\"0000????\") => { self.y = 2; }\n\
             _ => { self.y = 3; }\n\
         }",
    );
    assert!(
        compiled.verilog.contains("casez (v)"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("8'b1111????: begin"),
        "a mask must compare the bits it names:\n{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("8'b0000????: begin"),
        "{}",
        compiled.verilog
    );
    assert!(
        !compiled.verilog.contains("t_l"),
        "no temporary is needed for a plain bit `match`:\n{}",
        compiled.verilog
    );

    // Without a wildcard the match is not proven total: a bit-level cover is not
    // claimed here, so the error is a source-located rejection.
    reject(
        "match self.v {\n\
             mask!(0b1111????) => { self.y = 1; }\n\
         }",
        codes::UNSUPPORTED,
    );
}

#[test]
fn guards_keep_the_source_ordered_chain() {
    // A guard may reject an arm after its pattern matched, so the arms cannot be
    // `case` arms: the chain keeps the source order.
    let compiled = accept(
        "match self.v {\n\
             0 if self.v != 0 => { self.y = 1; }\n\
             0 => { self.y = 2; }\n\
             _ => { self.y = 3; }\n\
         }",
    );
    assert!(!compiled.verilog.contains("case"), "{}", compiled.verilog);
    assert!(compiled.verilog.contains("if ("), "{}", compiled.verilog);
}

#[test]
fn a_plain_bit_match_is_one_case() {
    // Constants, `or` alternatives and the `_` arm all fit one `case`/`default`.
    let compiled = accept(
        "match self.v {\n\
             1 | 2 => { self.y = 1; }\n\
             3 => { self.y = 2; }\n\
             _ => { self.y = 3; }\n\
         }",
    );
    assert!(
        compiled.verilog.contains("case (v)"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("8'd1, 8'd2: begin"),
        "`or` alternatives are one arm with two values:\n{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("default: begin"),
        "{}",
        compiled.verilog
    );
}

#[test]
fn range_and_mask_misuse_is_located() {
    // An open range is ambiguous.
    reject(
        "match self.v {\n0.. => { self.y = 1; }\n_ => { self.y = 2; }\n}",
        codes::UNSUPPORTED,
    );
    // A mask may only contain 0, 1 and ?.
    reject(
        "match self.v {\n\
             mask!(0b11xx1111) => { self.y = 1; }\n\
             _ => { self.y = 2; }\n\
         }",
        codes::UNSUPPORTED,
    );
    // A mask has to name every bit of the value.
    reject(
        "match self.v {\n\
             mask!(0b11??) => { self.y = 1; }\n\
             _ => { self.y = 2; }\n\
         }",
        codes::UNSUPPORTED,
    );
}
