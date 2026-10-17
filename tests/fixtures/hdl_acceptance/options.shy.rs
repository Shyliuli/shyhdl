// ShyHDL acceptance fixture for `Option<T>` and `Result<T, E>`, including the
// nested patterns the contract allows.
// Compiled by `frontend::compile(source, name, "OptTop")`; never compiled by
// rustc.
//
// Layout: `Option<T>` is `None` = tag 0 and `Some` = tag 1, `Result<T, E>` is
// `Ok` = tag 0 and `Err` = tag 1, with the tag in the lowest bits and the payload
// above it, exactly like a user enum. The payload is as wide as the widest case,
// so `Option<uint<4>>` is one tag bit plus four payload bits, and
// `Option<Result<uint<4>, int<8>>>` is one outer tag bit plus an inner one-bit
// tag plus the widest inner payload, the signed eight-bit `Err`.
//
// No user enum is declared for these: the value a nested construction produces is
// taken from the expected type, so `Some(Ok(x))` builds an
// `Option<Result<uint<4>, int<8>>>` directly.
//
// Equality compares the variants that are active, so the payload bits an inactive
// variant does not use do not take part. To pin that down, the equality inputs are
// driven from the testbench as raw bits, which lets it put junk in exactly those
// inactive bits and require the comparison to ignore it.

#[hardware_instance]
struct OptMake {
    #[In] v: uint<4>,
    #[In] s: int<8>,
    #[Out] none: Option<uint<4>>,
    #[Out] some: Option<uint<4>>,
    #[Out] ok: Result<uint<4>, int<8>>,
    #[Out] err: Result<uint<4>, int<8>>,
    #[Out] deep_ok: Option<Result<uint<4>, int<8>>>,
    #[Out] deep_err: Option<Result<uint<4>, int<8>>>,
}

impl Hardware for OptMake {
    fn run(&mut self) {
        self.none = None;
        self.some = Some(self.v);
        self.ok = Ok(self.v);
        self.err = Err(self.s);
        self.deep_ok = Some(Ok(self.v));
        self.deep_err = Some(Err(self.s));
    }
}

/// `match` over an `Option`, then over a `Result`, with the payload bound in each
/// arm. The inactive payload of a `Result` arm has to read as zero.
#[hardware_instance]
struct OptMatch {
    #[In] opt: Option<uint<4>>,
    #[In] res: Result<uint<4>, int<8>>,
    #[Out] opt_y: uint<4>,
    #[Out] res_y: uint<4>,
    #[Out] res_err: int<8>,
}

impl Hardware for OptMatch {
    fn run(&mut self) {
        match self.opt {
            None => {
                self.opt_y = 0xF;
            }
            Some(v) => {
                self.opt_y = v;
            }
        }

        match self.res {
            Ok(v) => {
                self.res_y = v;
                self.res_err = 0;
            }
            Err(e) => {
                self.res_y = 0;
                self.res_err = e;
            }
        }
    }
}

/// The exhaustive nested case: `Option<bool>` in three arms and no `_`, which is
/// complete because both payload values are spelled out.
#[hardware_instance]
struct NestedMatch {
    #[In] ob: Option<bool>,
    #[Out] y: uint<4>,
}

impl Hardware for NestedMatch {
    fn run(&mut self) {
        match self.ob {
            None => {
                self.y = 0;
            }
            Some(false) => {
                self.y = 1;
            }
            Some(true) => {
                self.y = 2;
            }
        }
    }
}

/// A nested construction destructured two levels deep, through both of its
/// payload arms.
#[hardware_instance]
struct DeepNest {
    #[In] deep_ok: Option<Result<uint<4>, int<8>>>,
    #[In] deep_err: Option<Result<uint<4>, int<8>>>,
    #[Out] ok_y: uint<4>,
    #[Out] ok_e: int<8>,
    #[Out] err_y: uint<4>,
    #[Out] err_e: int<8>,
}

impl Hardware for DeepNest {
    fn run(&mut self) {
        match self.deep_ok {
            Some(Ok(x)) => {
                self.ok_y = x;
                self.ok_e = 0;
            }
            Some(Err(e)) => {
                self.ok_y = 0;
                self.ok_e = e;
            }
            None => {
                self.ok_y = 0xF;
                self.ok_e = 0;
            }
        }

        match self.deep_err {
            Some(Ok(x)) => {
                self.err_y = x;
                self.err_e = 0;
            }
            Some(Err(e)) => {
                self.err_y = 0;
                self.err_e = e;
            }
            None => {
                self.err_y = 0xF;
                self.err_e = 0;
            }
        }
    }
}

/// Equality and inequality on enum-typed values, with both sides driven as raw
/// bits so the testbench can vary the parts that must not matter.
#[hardware_instance]
struct EnumEq {
    #[In] a: Option<uint<4>>,
    #[In] b: Option<uint<4>>,
    #[In] ra: Result<uint<4>, int<8>>,
    #[In] rb: Result<uint<4>, int<8>>,
    #[In] na: Option<Result<uint<4>, int<8>>>,
    #[In] nb: Option<Result<uint<4>, int<8>>>,
    #[Out] same: bool,
    #[Out] differ: bool,
    #[Out] rsame: bool,
    #[Out] rdiffer: bool,
    #[Out] nsame: bool,
    #[Out] ndiffer: bool,
}

impl Hardware for EnumEq {
    fn run(&mut self) {
        self.same = self.a == self.b;
        self.differ = self.a != self.b;
        self.rsame = self.ra == self.rb;
        self.rdiffer = self.ra != self.rb;
        self.nsame = self.na == self.nb;
        self.ndiffer = self.na != self.nb;
    }
}

/// Passes every case through one level of hierarchy.
#[hardware_instance]
struct OptTop {
    #[In] v: uint<4>,
    #[In] s: int<8>,
    #[In] ob: Option<bool>,
    #[In] a: Option<uint<4>>,
    #[In] b: Option<uint<4>>,
    #[In] ra: Result<uint<4>, int<8>>,
    #[In] rb: Result<uint<4>, int<8>>,
    #[In] na: Option<Result<uint<4>, int<8>>>,
    #[In] nb: Option<Result<uint<4>, int<8>>>,
    #[Out] none: Option<uint<4>>,
    #[Out] some: Option<uint<4>>,
    #[Out] ok: Result<uint<4>, int<8>>,
    #[Out] err: Result<uint<4>, int<8>>,
    #[Out] deep_ok: Option<Result<uint<4>, int<8>>>,
    #[Out] deep_err: Option<Result<uint<4>, int<8>>>,
    #[Out] n_y: uint<4>,
    #[Out] same: bool,
    #[Out] differ: bool,
    #[Out] rsame: bool,
    #[Out] rdiffer: bool,
    #[Out] nsame: bool,
    #[Out] ndiffer: bool,
    #[Out] ok_y: uint<4>,
    #[Out] ok_e: int<8>,
    #[Out] err_y: uint<4>,
    #[Out] err_e: int<8>,
    #[instance] make: OptMake,
    #[instance] nested: NestedMatch,
    #[instance] eq: EnumEq,
    #[instance] deep: DeepNest,
}

impl Hardware for OptTop {
    fn run(&mut self) {
        self.make.v = self.v;
        self.make.s = self.s;
        self.none = self.make.none;
        self.some = self.make.some;
        self.ok = self.make.ok;
        self.err = self.make.err;
        self.deep_ok = self.make.deep_ok;
        self.deep_err = self.make.deep_err;

        self.nested.ob = self.ob;
        self.n_y = self.nested.y;

        self.eq.a = self.a;
        self.eq.b = self.b;
        self.eq.ra = self.ra;
        self.eq.rb = self.rb;
        self.eq.na = self.na;
        self.eq.nb = self.nb;
        self.same = self.eq.same;
        self.differ = self.eq.differ;
        self.rsame = self.eq.rsame;
        self.rdiffer = self.eq.rdiffer;
        self.nsame = self.eq.nsame;
        self.ndiffer = self.eq.ndiffer;

        self.deep.deep_ok = self.make.deep_ok;
        self.deep.deep_err = self.make.deep_err;
        self.ok_y = self.deep.ok_y;
        self.ok_e = self.deep.ok_e;
        self.err_y = self.deep.err_y;
        self.err_e = self.deep.err_e;
    }
}
