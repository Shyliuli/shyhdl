// ShyHDL acceptance fixture for AC-MVP-001, AC-MVP-002, AC-MVP-003 and
// AC-MVP-004. Compiled by `frontend::compile(source, name,
// "AdderTop")`; never compiled by rustc.
//
// `tb_adder.v` instantiates Add_N1, Add_N8, Adder8 and AdderTop and compares
// every output against an independent bit-level ripple-carry reference model.

#[hardware_instance]
struct Add<const N: usize> {
    #[In] a: uint<N>,
    #[In] b: uint<N>,
    #[Out] sum: uint<N>,
    #[Out] carry: bool,
}

impl<const N: usize> Hardware for Add<N> {
    fn run(&mut self) {
        let wide: uint<{ N + 1 }> = self.a.zext::<{ N + 1 }>() + self.b.zext::<{ N + 1 }>();
        self.sum = wide[0..N];
        self.carry = wide.get(N);
    }
}

// AC-MVP-003: the parent works by assignment only. There is no `self.add.run()`
// anywhere, and the two input wires are written in the opposite order to the
// declaration of `Add` on purpose (REQ-LANG-012, REQ-LANG-013).
#[hardware_instance]
struct Adder8 {
    #[In] a: uint<8>,
    #[In] b: uint<8>,
    #[Out] sum: uint<8>,
    #[Out] carry: bool,
    #[instance] add: Add<8>,
}

impl Hardware for Adder8 {
    fn run(&mut self) {
        self.add.b = self.b;
        self.add.a = self.a;
        self.sum = self.add.sum;
        self.carry = self.add.carry;
    }
}

// AC-MVP-004: the same source struct instantiated at N = 1 and N = 8, plus the
// two-level chain AdderTop -> Adder8 -> Add_N8. `eight` and `other8` are two
// instances of the *same* specialisation `Add_N8`, so the debug metadata has to
// keep two independent instance paths for them instead of collapsing them onto
// the module name (which would also duplicate the variable ids).
#[hardware_instance]
struct AdderTop {
    #[In] a8: uint<8>,
    #[In] b8: uint<8>,
    #[In] a1: uint<1>,
    #[In] b1: uint<1>,
    #[Out] sum8: uint<8>,
    #[Out] carry8: bool,
    #[Out] osum8: uint<8>,
    #[Out] ocarry8: bool,
    #[Out] sum1: uint<1>,
    #[Out] carry1: bool,
    #[Out] wsum: uint<8>,
    #[Out] wcarry: bool,
    #[instance] one: Add<1>,
    #[instance] eight: Add<8>,
    #[instance] other8: Add<8>,
    #[instance] wrapper: Adder8,
}

impl Hardware for AdderTop {
    fn run(&mut self) {
        self.one.a = self.a1;
        self.one.b = self.b1;
        self.sum1 = self.one.sum;
        self.carry1 = self.one.carry;

        self.eight.a = self.a8;
        self.eight.b = self.b8;
        self.sum8 = self.eight.sum;
        self.carry8 = self.eight.carry;

        self.other8.a = self.a8;
        self.other8.b = self.b8;
        self.osum8 = self.other8.sum;
        self.ocarry8 = self.other8.carry;

        self.wrapper.a = self.a8;
        self.wrapper.b = self.b8;
        self.wsum = self.wrapper.sum;
        self.wcarry = self.wrapper.carry;
    }
}
