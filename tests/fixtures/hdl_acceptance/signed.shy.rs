// ShyHDL acceptance fixture for signed (`int<W>`) semantics. Compiled by
// `frontend::compile(source, name, "SignedTop")`; never compiled by rustc.
//
// The oracle (`tb_signed.v`) computes every expected value in the integer domain
// — Verilog `integer` arithmetic — and then truncates the result to the width
// under test itself, so the comparison is made against a bit pattern it produced
// rather than against a second run of the same expression. The bit shifts are
// defined explicitly: a copied sign bit for arithmetic, a zero for logical.
// `Signed8` covers the -128 / 127 boundaries, `Signed4` is swept
// exhaustively, and the struct, the cast and the constant-only process each pin
// down that signedness survives a context: a field, a part-select, an alias, a
// branch and a literal.
//
// Signed and unsigned behaviour has to be told apart, so each group also has a
// witness the testbench insists on: a signed comparison that disagrees with the
// unsigned one, and an arithmetic shift that disagrees with the logical one.

/// A struct whose fields differ in signedness, so the layout alone cannot hide a
/// field read as the wrong kind.
struct Pair {
    s: int<4>,
    u: uint<4>,
}

/// Same-width signed `+ - *`, bitwise operators, shifts, negation and the six
/// comparisons, plus the same comparison after reinterpreting as `uint<4>`.
#[hardware_instance]
struct Signed4 {
    #[In] a: int<4>,
    #[In] b: int<4>,
    #[Out] sum: int<4>,
    #[Out] diff: int<4>,
    #[Out] prod: int<4>,
    #[Out] band: int<4>,
    #[Out] bor: int<4>,
    #[Out] bxor: int<4>,
    #[Out] shl: int<4>,
    #[Out] shr: int<4>,
    #[Out] neg: int<4>,
    #[Out] lt: bool,
    #[Out] le: bool,
    #[Out] gt: bool,
    #[Out] ge: bool,
    #[Out] eq: bool,
    #[Out] ne: bool,
    #[Out] ult: bool,
}

impl Hardware for Signed4 {
    fn run(&mut self) {
        self.sum = self.a + self.b;
        self.diff = self.a - self.b;
        self.prod = self.a * self.b;
        self.band = self.a & self.b;
        self.bor = self.a | self.b;
        self.bxor = self.a ^ self.b;
        self.shl = self.a << 1;
        self.shr = self.a >> 1;
        self.neg = -self.a;
        self.lt = self.a < self.b;
        self.le = self.a <= self.b;
        self.gt = self.a > self.b;
        self.ge = self.a >= self.b;
        self.eq = self.a == self.b;
        self.ne = self.a != self.b;
        // An equal-width reinterpretation, so the comparison is unsigned.
        self.ult = (self.a as uint<4>) < (self.b as uint<4>);
    }
}

/// The same semantics at the width whose boundaries matter: -128 and 127.
#[hardware_instance]
struct Signed8 {
    #[In] a: int<8>,
    #[In] b: int<8>,
    #[Out] sum: int<8>,
    #[Out] shr: int<8>,
    #[Out] neg: int<8>,
    #[Out] lt: bool,
    #[Out] ult: bool,
    #[Out] ext: int<16>,
}

impl Hardware for Signed8 {
    fn run(&mut self) {
        self.sum = self.a + self.b;
        self.shr = self.a >> 1;
        self.neg = -self.a;
        self.lt = self.a < self.b;
        self.ult = (self.a as uint<8>) < (self.b as uint<8>);
        self.ext = self.a.sext::<16>();
    }
}

/// A signed struct field read whole, sign-extended, truncated back, shifted
/// through an alias, compared as a signed value, and part-selected into raw
/// bits that compare unsigned.
#[hardware_instance]
struct PairExt {
    #[In] p: Pair,
    #[In] t: int<4>,
    #[Out] s: int<4>,
    #[Out] u: uint<4>,
    #[Out] ext: int<8>,
    #[Out] back: int<4>,
    #[Out] shr: int<4>,
    #[Out] lt: bool,
    #[Out] low: uint<2>,
    #[Out] low_lt: bool,
}

impl Hardware for PairExt {
    fn run(&mut self) {
        self.s = self.p.s;
        self.u = self.p.u;
        self.ext = self.p.s.sext::<8>();
        self.back = self.p.s.sext::<8>().trunc::<4>();

        // A run-local alias of a signed field keeps its signedness, so this
        // comparison is signed.
        let sv: int<4> = self.p.s;
        self.lt = sv < self.t;

        // Reading the field expression directly has to recover the signed type
        // as well: an unsigned read would turn this shift logical.
        self.shr = self.p.s >> 1;

        // A part-select of a signed field is raw bits, so this comparison is
        // unsigned even though the field it came from is signed.
        self.low = self.p.s[0..2];
        self.low_lt = self.p.s[0..2] < self.p.u[0..2];
    }
}

/// No input at all: the constant, the constant condition and the local alias all
/// have to keep the signedness of a negative value.
#[hardware_instance]
struct SignedConst {
    #[Out] v: int<4>,
    #[Out] v_shr: int<4>,
    #[Out] u: uint<4>,
}

impl Hardware for SignedConst {
    fn run(&mut self) {
        let mut chosen: int<4>;
        let flag: bool = false;
        if flag {
            chosen = 5;
        } else {
            // A negative literal inside the signed range, in the taken arm.
            chosen = -3;
        }
        self.v = chosen;
        // -3 >> 1 is -2 arithmetically; a logical shift would give 6.
        self.v_shr = chosen >> 1;
        // The same four bits as a positive unsigned value, for contrast.
        self.u = 0xF;
    }
}

/// Wires every leaf through one level of hierarchy, so a signed port also has to
/// survive a module boundary.
#[hardware_instance]
struct SignedTop {
    #[In] a4: int<4>,
    #[In] b4: int<4>,
    #[In] a8: int<8>,
    #[In] b8: int<8>,
    #[In] p: Pair,
    #[In] t: int<4>,
    #[Out] sum4: int<4>,
    #[Out] diff4: int<4>,
    #[Out] prod4: int<4>,
    #[Out] band4: int<4>,
    #[Out] bor4: int<4>,
    #[Out] bxor4: int<4>,
    #[Out] shl4: int<4>,
    #[Out] shr4: int<4>,
    #[Out] neg4: int<4>,
    #[Out] lt4: bool,
    #[Out] le4: bool,
    #[Out] gt4: bool,
    #[Out] ge4: bool,
    #[Out] eq4: bool,
    #[Out] ne4: bool,
    #[Out] ult4: bool,
    #[Out] sum8: int<8>,
    #[Out] shr8: int<8>,
    #[Out] neg8: int<8>,
    #[Out] lt8: bool,
    #[Out] ult8: bool,
    #[Out] ext8: int<16>,
    #[Out] ps: int<4>,
    #[Out] pu: uint<4>,
    #[Out] pext: int<8>,
    #[Out] pback: int<4>,
    #[Out] pshr: int<4>,
    #[Out] plt: bool,
    #[Out] plow: uint<2>,
    #[Out] plowlt: bool,
    #[Out] cv: int<4>,
    #[Out] cvshr: int<4>,
    #[Out] cu: uint<4>,
    #[instance] four: Signed4,
    #[instance] eight: Signed8,
    #[instance] pair: PairExt,
    #[instance] konst: SignedConst,
}

impl Hardware for SignedTop {
    fn run(&mut self) {
        self.four.a = self.a4;
        self.four.b = self.b4;
        self.sum4 = self.four.sum;
        self.diff4 = self.four.diff;
        self.prod4 = self.four.prod;
        self.band4 = self.four.band;
        self.bor4 = self.four.bor;
        self.bxor4 = self.four.bxor;
        self.shl4 = self.four.shl;
        self.shr4 = self.four.shr;
        self.neg4 = self.four.neg;
        self.lt4 = self.four.lt;
        self.le4 = self.four.le;
        self.gt4 = self.four.gt;
        self.ge4 = self.four.ge;
        self.eq4 = self.four.eq;
        self.ne4 = self.four.ne;
        self.ult4 = self.four.ult;

        self.eight.a = self.a8;
        self.eight.b = self.b8;
        self.sum8 = self.eight.sum;
        self.shr8 = self.eight.shr;
        self.neg8 = self.eight.neg;
        self.lt8 = self.eight.lt;
        self.ult8 = self.eight.ult;
        self.ext8 = self.eight.ext;

        self.pair.p = self.p;
        self.pair.t = self.t;
        self.ps = self.pair.s;
        self.pu = self.pair.u;
        self.pext = self.pair.ext;
        self.pback = self.pair.back;
        self.pshr = self.pair.shr;
        self.plt = self.pair.lt;
        self.plow = self.pair.low;
        self.plowlt = self.pair.low_lt;

        self.cv = self.konst.v;
        self.cvshr = self.konst.v_shr;
        self.cu = self.konst.u;
    }
}
