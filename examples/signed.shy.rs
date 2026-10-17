//! A signed datapath, the two's-complement counterpart of `examples/adder.shy.rs`.
//!
//! Build it with `shyhdl build --top Signed8 examples/signed.shy.rs`.
//!
//! `int<W>` is a `W`-bit two's-complement value (REQ-LATER-003): same-width
//! arithmetic wraps exactly as an ALU wraps, `>>` shifts the sign bit in, and
//! the ordering comparisons read the values as signed. Signed and unsigned
//! values are never combined silently: `as int<W>` / `as uint<W>` reinterprets
//! the same bits when the other reading is meant.

/// A signed value next to its unsigned tag, so the two readings are visible in
/// one layout: `value` is signed, `tag` is not.
struct Tagged {
    value: int<8>,
    tag: uint<4>,
}

#[hardware_instance]
struct Signed8 {
    #[In] a: int<8>,
    #[In] b: int<8>,
    #[In] req: Tagged,
    #[Out] sum: int<8>,
    #[Out] halved: int<8>,
    #[Out] negated: int<8>,
    #[Out] wider: int<16>,
    #[Out] negative: bool,
    #[Out] unsigned_below: bool,
    #[Out] value: int<8>,
    #[Out] tag: uint<4>,
}

impl Hardware for Signed8 {
    fn run(&mut self) {
        // Same width, so the result wraps and the carry out is dropped.
        self.sum = self.a + self.b;
        // Arithmetic: the sign bit shifts in, so -4 >> 1 is -2 and not 126.
        self.halved = self.a >> 1;
        // The two's-complement negation, at the value's own width.
        self.negated = -self.a;
        // Sign extension keeps the value signed, so the 16-bit result still
        // compares as a signed value.
        self.wider = self.a.sext::<16>();
        self.negative = self.a < 0;
        // The same bits as the comparison above, read the other way: the bits do
        // not change, the reading does.
        self.unsigned_below = (self.a as uint<8>) < (self.b as uint<8>);
        // A struct field keeps the type it was declared with: reading the
        // signed one is signed, and the unsigned one next to it stays unsigned.
        self.value = self.req.value;
        self.tag = self.req.tag;
    }
}
