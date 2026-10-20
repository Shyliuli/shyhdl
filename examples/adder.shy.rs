// ShyHDL source (Rust-style DSL). Compiled by `frontend::compile`, not by rustc.
//
// Combinational adder plus a registered variant that instantiates it. The
// generic parameter is a *build-time* constant: every width is resolved while
// compiling, and `Adder<8>` becomes the Verilog module `Adder_N8`.
//
// Const width expressions are written with braces (`{ N + 1 }`), the only
// spelling Rust (and therefore `syn`) accepts as a generic argument. The
// frontend evaluates them itself; it never asks rustc for generic const
// arithmetic.
//
// A sequential process lists its events as arms. `#[Pedge]`/`#[Nedge]` say
// which transition enters an arm; where the arm stands says what it is: the arms
// before the last one are the asynchronous resets, and the last arm is the
// regular edge branch (the clock). Nothing is inferred from a signal name or an
// edge polarity, so a clock is an ordinary `#[In]`. The generated block gives
// the resets priority in source order, the first arm highest.

#[hardware_instance]
struct Adder<const N: usize> {
    #[In] a: uint<N>,
    #[In] b: uint<N>,
    #[Out] sum: uint<N>,
    #[Out] carry: bool,
}

impl<const N: usize> Hardware for Adder<N> {
    fn run(&mut self) {
        // Widening is explicit: `zext` to N+1 bits, add, then take the low N
        // bits and bit N as the carry out.
        let wide: uint<{ N + 1 }> = self.a.zext::<{ N + 1 }>() + self.b.zext::<{ N + 1 }>();
        self.sum = wide[0..N];
        self.carry = wide.get(N);
    }
}

// A register wrapping the adder: the child is driven by assignments only, no
// `self.adder.run` anywhere.
#[hardware_instance]
struct Adder8 {
    #[In] a: uint<8>,
    #[In] b: uint<8>,
    #[In] enable: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] sum: uint<8>,
    #[Out] carry: bool,
    #[instance] adder: Adder<8>,
}

impl Hardware for Adder8 {
    fn run(&mut self) {
        // Wiring: connection order does not change the topology.
        self.adder.b = self.b;
        self.adder.a = self.a;

        always! {
            // The arms before the last one are the asynchronous resets, in
            // source order; the last arm is the clock.
            #[Nedge] self.rst_n => {
                self.sum = 0;
                self.carry = false;
            }
            #[Pedge] self.clk => {
                if self.enable {
                    self.sum = self.adder.sum;
                    self.carry = self.adder.carry;
                }
            }
        }
    }
}
