// ShyHDL source (Rust-style DSL). Compiled by `frontend::compile`, not by rustc.
//
// Two-stage pipeline. `stage1` is a module-scope value written only by the
// sequential process, so it is a register; `self.q` samples the *old* `stage1`
// because state assignments commit non-blocking.
// Reading `stage1` from wiring observes the committed register value.

#[hardware_instance]
struct Pipe2<const W: usize> {
    #[In] d: uint<W>,
    #[In] enable: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: uint<W>,
    #[Out] stage1_out: uint<W>,
}

impl<const W: usize> Hardware for Pipe2<W> {
    fn run(&mut self) {
        // Declared, not initialised: it becomes a register because a sequential
        // process drives it.
        let mut stage1: uint<W>;

        always! {
            // The arms before the last one are the asynchronous resets, in
            // source order; the last arm is the clock.
            #[Nedge] self.rst_n => {
                stage1 = 0;
                self.q = 0;
            }
            #[Pedge] self.clk => {
                if self.enable {
                    stage1 = self.d;
                    // Non-blocking commit: this reads the previous stage1.
                    self.q = stage1;
                }
            }
        }

        // Observation port: continuous wiring may read a register.
        self.stage1_out = stage1;
    }
}

#[hardware_instance]
struct Pipe8 {
    #[In] d: uint<8>,
    #[In] enable: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: uint<8>,
    #[Out] stage1_out: uint<8>,
    #[instance] body: Pipe2<8>,
}

impl Hardware for Pipe8 {
    fn run(&mut self) {
        self.body.d = self.d;
        self.body.enable = self.enable;
        self.body.rst_n = self.rst_n;
        self.body.clk = self.clk;
        self.q = self.body.q;
        self.stage1_out = self.body.stage1_out;
    }
}
