// ShyHDL acceptance fixture for enums wrapped in a struct or an array, and for
// equality whose payload must be ignored.
// Compiled by `frontend::compile(source, name, "WrapTop")`; never compiled by
// rustc.
//
// The layout is the one `enums.shy.rs` establishes, applied inside other
// containers:
//
// * `Msg` has three variants, so its tag is two bits with one unused tag value,
//   and its widest payload is the eight-bit `Pair`, giving a ten-bit value,
// * a struct packs its fields most significant first, so a struct holding an
//   enum puts that enum in the position its declaration order gives it,
// * an array puts element 0 in the least significant bits, so the first `Msg` of
//   a two-element array is the low ten bits.
//
// Equality is the point of this fixture. Two values are equal when their tags are
// equal and, for a variant that carries a payload, the payload too. When the tag
// names a variant the value does not have, or names a variant whose payload is
// not part of the comparison, the payload bits are ignored entirely: the
// testbench drives raw bits so it can fill those with junk and require equality
// to survive it.

/// The enum from the other fixture, repeated here because a fixture is one
/// compilation unit.
enum Msg {
    Idle,
    Data(uint<4>),
    Pair { hi: int<4>, lo: uint<4> },
}

/// A struct that holds an enum beside an unsigned field.
struct Envelope {
    msg: Msg,
    extra: uint<2>,
}

/// Builds the wrapped values, and takes them apart again, so the layout inside a
/// struct and an array is checked as values.
#[hardware_instance]
struct WrapMake {
    #[In] v: uint<4>,
    #[In] s: int<4>,
    #[Out] env_idle: Envelope,
    #[Out] env_pair: Envelope,
    #[Out] arr: [Msg; 2],
    #[Out] arr0: Msg,
    #[Out] arr1: Msg,
    #[Out] env_field: Msg,
    #[Out] env_extra: uint<2>,
}

impl Hardware for WrapMake {
    fn run(&mut self) {
        self.env_idle = Envelope { msg: Msg::Idle, extra: 0x1 };
        self.env_pair = Envelope { msg: Msg::Pair { hi: self.s, lo: self.v }, extra: 0x2 };
        self.arr = [Msg::Data(self.v), Msg::Idle];

        self.arr0 = self.arr[0];
        self.arr1 = self.arr[1];
        // A field of a struct field: the enum inside `Envelope`, read out.
        self.env_field = self.env_pair.msg;
        self.env_extra = self.env_pair.extra;
    }
}

/// Equality over a wrapped enum, over an array of them, and over a struct that
/// contains one.
#[hardware_instance]
struct WrapEq {
    #[In] a: Envelope,
    #[In] b: Envelope,
    #[In] ea: Msg,
    #[In] eb: Msg,
    #[In] aa: [Msg; 2],
    #[In] ab: [Msg; 2],
    #[Out] env_same: bool,
    #[Out] env_differ: bool,
    #[Out] msg_same: bool,
    #[Out] msg_differ: bool,
    #[Out] arr_same: bool,
    #[Out] arr_differ: bool,
}

impl Hardware for WrapEq {
    fn run(&mut self) {
        self.env_same = self.a == self.b;
        self.env_differ = self.a != self.b;
        self.msg_same = self.ea == self.eb;
        self.msg_differ = self.ea != self.eb;
        self.arr_same = self.aa == self.ab;
        self.arr_differ = self.aa != self.ab;
    }
}

#[hardware_instance]
struct WrapTop {
    #[In] v: uint<4>,
    #[In] s: int<4>,
    #[In] a: Envelope,
    #[In] b: Envelope,
    #[In] ea: Msg,
    #[In] eb: Msg,
    #[In] aa: [Msg; 2],
    #[In] ab: [Msg; 2],
    #[Out] env_idle: Envelope,
    #[Out] env_pair: Envelope,
    #[Out] arr: [Msg; 2],
    #[Out] arr0: Msg,
    #[Out] arr1: Msg,
    #[Out] env_field: Msg,
    #[Out] env_extra: uint<2>,
    #[Out] env_same: bool,
    #[Out] env_differ: bool,
    #[Out] msg_same: bool,
    #[Out] msg_differ: bool,
    #[Out] arr_same: bool,
    #[Out] arr_differ: bool,
    #[instance] make: WrapMake,
    #[instance] eq: WrapEq,
}

impl Hardware for WrapTop {
    fn run(&mut self) {
        self.make.v = self.v;
        self.make.s = self.s;
        self.env_idle = self.make.env_idle;
        self.env_pair = self.make.env_pair;
        self.arr = self.make.arr;
        self.arr0 = self.make.arr0;
        self.arr1 = self.make.arr1;
        self.env_field = self.make.env_field;
        self.env_extra = self.make.env_extra;

        self.eq.a = self.a;
        self.eq.b = self.b;
        self.eq.ea = self.ea;
        self.eq.eb = self.eb;
        self.eq.aa = self.aa;
        self.eq.ab = self.ab;
        self.env_same = self.eq.env_same;
        self.env_differ = self.eq.env_differ;
        self.msg_same = self.eq.msg_same;
        self.msg_differ = self.eq.msg_differ;
        self.arr_same = self.eq.arr_same;
        self.arr_differ = self.eq.arr_differ;
    }
}
