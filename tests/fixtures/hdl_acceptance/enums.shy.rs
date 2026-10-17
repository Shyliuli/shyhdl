// ShyHDL acceptance fixture for user enums with payloads, and for `match` over
// them. Compiled by `frontend::compile(source, name, "EnumTop")`; never compiled
// by rustc.
//
// Layout, which every expectation in `tb_enums.v` is built from by hand:
//
// * the tag lives in the lowest bits of the value, `max(1, ceil(log2 n))` wide,
//   and variants are numbered in declaration order from 0,
// * a payload sits above the tag and is as wide as the widest variant's payload,
// * payload bits a variant does not use are zero,
// * a struct payload packs its fields most significant first.
//
// `Msg` has three variants, so the tag is two bits wide and one tag value has no
// variant at all. That value is why the `match` below needs a final `_`: every
// real tag is covered explicitly, but an undefined tag is still representable.

/// A user enum: a unit variant, one with a positional payload, and one with a
/// named payload whose fields differ in signedness.
enum Msg {
    Idle,
    Data(uint<4>),
    Pair { hi: int<4>, lo: uint<4> },
}

/// Constructs each variant, so the tag numbering and the payload packing are
/// visible as values rather than only as a layout claim.
#[hardware_instance]
struct EnumMake {
    #[In] v: uint<4>,
    #[In] s: int<4>,
    #[Out] idle: Msg,
    #[Out] data: Msg,
    #[Out] pair: Msg,
}

impl Hardware for EnumMake {
    fn run(&mut self) {
        self.idle = Msg::Idle;
        self.data = Msg::Data(self.v);
        self.pair = Msg::Pair { hi: self.s, lo: self.v };
    }
}

/// The same value read back through `match`: a unit variant, a payload arm
/// guarded so that a second arm for the same variant handles the rest (source
/// priority), a named-payload arm that binds both fields, and a final `_` for
/// the tag that has no variant.
///
/// `tag_seen` is recovered from the arms rather than cast, so nothing here
/// depends on a conversion that is not part of the contract.
#[hardware_instance]
struct EnumMatch {
    #[In] msg: Msg,
    #[Out] y: uint<4>,
    #[Out] tag_seen: uint<2>,
    #[Out] bound: uint<8>,
}

impl Hardware for EnumMatch {
    fn run(&mut self) {
        match self.msg {
            Msg::Idle => {
                self.y = 0;
                self.tag_seen = 0;
                self.bound = 0;
            }
            // The guard is evaluated first, so a `Data` above two takes this arm
            // and every other `Data` falls through to the arm below.
            Msg::Data(v) if v > 2 => {
                self.y = v;
                self.tag_seen = 1;
                // `v` is an unsigned four-bit value, so it is zero-extended; the
                // high half of `bound` is what `Data` does not use.
                self.bound = v.zext::<8>();
            }
            Msg::Data(v) => {
                self.y = 1;
                self.tag_seen = 1;
                self.bound = v.zext::<8>();
            }
            // The named payload binds both fields, and the signed one is
            // sign-extended by the binding itself.
            Msg::Pair { hi, lo } => {
                self.y = lo;
                self.tag_seen = 2;
                // `hi` is signed, so sign-extension keeps it signed and the bits
                // are reinterpreted to drive the unsigned port. The bits do not
                // change, so an all-ones `hi` still reads as 0xF8.
                self.bound = hi.sext::<8>() as uint<8>;
            }
            // Every real tag is handled above; this arm exists because the tag
            // width admits one more value than there are variants.
            _ => {
                self.y = 0xF;
                self.tag_seen = 3;
                self.bound = 0;
            }
        }
    }
}

/// An `or` pattern with a guard over an array, which is where the general
/// matching path shows up: two alternatives bind the same name, so each has to
/// be tried with its own binding before the guard decides. A first alternative
/// that matches but whose guard fails gives way to the second one, and the
/// binding stays inside the arm, so the `let` above the `match` is only shadowed.
#[hardware_instance]
struct OrMatch {
    #[In] pair: [uint<4>; 2],
    #[Out] y: uint<4>,
    #[Out] hit: bool,
    #[Out] after: uint<4>,
}

impl Hardware for OrMatch {
    fn run(&mut self) {
        let x: uint<4> = self.pair[0];
        self.hit = false;
        self.y = 0;

        match self.pair {
            // `[x, 5]` matches when the second element is five and binds `x` to
            // the first; `[0, x]` matches when the first is zero and binds `x` to
            // the second. The guard sees the binding of whichever alternative
            // was taken.
            [x, 5] | [0, x] if x == 5 => {
                self.y = x;
                self.hit = true;
            }
            _ => {
                self.y = 0;
            }
        }

        // The pattern binding did not escape the arm, so this reads the `let`.
        self.after = x;
    }
}

/// Passes the enum construction and the `match` through one level of hierarchy,
/// so a user-enum port and an enum-typed value also survive a module boundary.
/// The constructed value is matched by a second instance, so the per-input
/// checks and the construction checks never share one path.
#[hardware_instance]
struct EnumTop {
    #[In] v: uint<4>,
    #[In] s: int<4>,
    #[In] msg: Msg,
    #[In] or_pair: [uint<4>; 2],
    #[Out] idle: Msg,
    #[Out] data: Msg,
    #[Out] pair: Msg,
    #[Out] y: uint<4>,
    #[Out] tag_seen: uint<2>,
    #[Out] bound: uint<8>,
    #[Out] made_y: uint<4>,
    #[Out] made_tag: uint<2>,
    #[Out] or_y: uint<4>,
    #[Out] or_hit: bool,
    #[Out] or_after: uint<4>,
    #[instance] make: EnumMake,
    #[instance] matched: EnumMatch,
    #[instance] matched_made: EnumMatch,
    #[instance] ormatch: OrMatch,
}

impl Hardware for EnumTop {
    fn run(&mut self) {
        self.make.v = self.v;
        self.make.s = self.s;
        self.idle = self.make.idle;
        self.data = self.make.data;
        self.pair = self.make.pair;

        self.matched.msg = self.msg;
        self.y = self.matched.y;
        self.tag_seen = self.matched.tag_seen;
        self.bound = self.matched.bound;

        // The freshly constructed `Data` value, matched by its own instance, so
        // construction and destructuring are checked on one path as well.
        self.matched_made.msg = self.make.data;
        self.made_y = self.matched_made.y;
        self.made_tag = self.matched_made.tag_seen;

        self.ormatch.pair = self.or_pair;
        self.or_y = self.ormatch.y;
        self.or_hit = self.ormatch.hit;
        self.or_after = self.ormatch.after;
    }
}
