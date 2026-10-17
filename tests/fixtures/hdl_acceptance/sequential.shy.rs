// ShyHDL acceptance fixture for AC-MVP-005, AC-MVP-006, AC-MVP-007 and
// AC-MVP-008. Compiled by `frontend::compile(source, name,
// "SeqTop")`; never compiled by rustc.
//
// `tb_sequential.v` instantiates Register_W8, Pipe8 and SeqTop and drives the
// clock, the asynchronous active-low reset and the enable itself. The child
// instance fields are named `store` and `pipe` because a field name that is a
// Verilog keyword would become an illegal instance label.
//
// The clock is a plain `#[In]` port: it is not marked, and no arm carries a
// `#[Reset]`. The arms of `always!` say what they are by position: every arm
// before the last is an asynchronous reset, in source order, and the last arm is
// the regular edge branch. `Register` below uses that form, so the existing
// reset-hold and clock regression in `tb_sequential.v` covers the new syntax as
// well; `Pipe2` keeps the older if/else chain, so the two forms stay compatible
// and produce the same behaviour.

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

// AC-MVP-008: `self.q` samples the previous `stage1`, because state assignments
// commit non-blocking (REQ-LANG-014, REQ-LANG-016).
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
        let mut stage1: uint<W>;

        always! {
            if negedge(self.rst_n) {
                stage1 = 0;
                self.q = 0;
            } else if posedge(self.clk) {
                if self.enable {
                    stage1 = self.d;
                    self.q = stage1;
                }
            }
        }

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

// One module that carries both the plain register and the two-stage pipeline,
// so the testbench can observe them under identical stimulus.
#[hardware_instance]
struct SeqTop {
    #[In] d: uint<8>,
    #[In] enable: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: uint<8>,
    #[Out] pq: uint<8>,
    #[Out] pstage: uint<8>,
    #[instance] store: Register<8>,
    #[instance] pipe: Pipe8,
}

impl Hardware for SeqTop {
    fn run(&mut self) {
        self.store.d = self.d;
        self.store.enable = self.enable;
        self.store.rst_n = self.rst_n;
        self.store.clk = self.clk;
        self.q = self.store.q;

        self.pipe.d = self.d;
        self.pipe.enable = self.enable;
        self.pipe.rst_n = self.rst_n;
        self.pipe.clk = self.clk;
        self.pq = self.pipe.q;
        self.pstage = self.pipe.stage1_out;
    }
}
