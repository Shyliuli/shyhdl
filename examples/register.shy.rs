// ShyHDL source (Rust-style DSL). Compiled by `frontend::compile`, not by rustc.
//
// A parameterised register with an asynchronous, active-low reset and a clock
// enable, plus a wrapper that instantiates it at two different widths so that
// build-time specialisation (`Register_W4`, `Register_W8`) is exercised.
//
// A clock is an ordinary `#[In]`: the language has no clock marking, and no
// attribute marks a reset either. The arms before the last one are the
// asynchronous resets, in source order, and the last arm is the regular edge
// branch. The debugger's reference clock is configured per testbench, not in the
// design.

#[hardware_instance]
struct Register<const W: usize> {
    #[In] d: uint<W>,
    #[In] enable: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: uint<W>,
}

impl<const W: usize> Hardware for Register<W> {
    fn run(&mut self) {
        always! {
            #[Nedge] self.rst_n => {
                self.q = 0;
            }
            #[Pedge] self.clk => {
                if self.enable {
                    self.q = self.d;
                }
            }
        }
    }
}

// Two instances of the same source struct at different widths.
#[hardware_instance]
struct RegisterPair {
    #[In] d: uint<8>,
    #[In] enable: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q_wide: uint<8>,
    #[Out] q_narrow: uint<4>,
    #[instance] wide: Register<8>,
    #[instance] narrow: Register<4>,
}

impl Hardware for RegisterPair {
    fn run(&mut self) {
        self.wide.d = self.d;
        self.wide.enable = self.enable;
        self.wide.rst_n = self.rst_n;
        self.wide.clk = self.clk;

        self.narrow.d = self.d[0..4];
        self.narrow.enable = self.enable;
        self.narrow.rst_n = self.rst_n;
        self.narrow.clk = self.clk;

        self.q_wide = self.wide.q;
        self.q_narrow = self.narrow.q;
    }
}
