// CLI fixture: a combinational DUT plus three tests. `tests/cli_sim.rs` runs
// them through `shyhdl test` end to end, so every construct here has to pass
// both the frontend and the testbench generator.
//
// The DUT is deliberately combinational: no clock syntax is involved, which
// keeps this fixture about the CLI pipeline rather than about clock wrappers.

#[hardware_instance]
struct Adder4 {
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    // A clock is an ordinary input: the language has no clock marking, and the
    // reference clock of a debug session is configured by the testcase below.
    #[In] clk: bool,
    #[Out] sum: uint<4>,
    #[Out] carry: bool,
}

impl Hardware for Adder4 {
    fn run(&mut self) {
        let wide: uint<{ 4 + 1 }> = self.a.zext::<{ 4 + 1 }>() + self.b.zext::<{ 4 + 1 }>();
        self.sum = wide[0..4];
        self.carry = wide.get(4);
    }
}

// Passes: 0xf + 0x1 wraps to 0x0 with carry set.
#[hardware_test]
fn wraps_and_carries() {
    let mut dut = Adder4::new();
    initial! {
        dut.a = 0xf;
        dut.b = 0x1;
        delay(ns(1));
        assert_eq!(dut.sum, 0x0);
        assert_eq!(dut.carry, true);
        pass!();
        finish!();
    }
}

// Fails: the real sum is 0x2, and this asks for 0x3.
#[hardware_test]
fn fails_on_purpose() {
    let mut dut = Adder4::new();
    initial! {
        dut.a = 0x1;
        dut.b = 0x1;
        delay(ns(1));
        assert_eq!(dut.sum, 0x3);
        finish!();
    }
}

// Passes, and declares the reference clock the debugger can default to
// (`gen_clk`): the DUT is combinational, so the clock only has to tick.
#[hardware_test]
fn clocked_smoke() {
    let mut dut = Adder4::new();
    gen_clk(dut.clk, mhz(100));
    initial! {
        dut.a = 0x3;
        dut.b = 0x4;
        wait(posedge(dut.clk));
        sample_after(posedge(dut.clk), ps(1));
        assert_eq!(dut.sum, 0x7);
        pass!();
        finish!();
    }
}

// Never finishes: a free-running clock keeps the simulator busy while the
// condition can never become true, so the run has to be reported as a timeout
// rather than as a failure or a pass. The clock is a testcase property, declared
// here with `gen_clk`.
#[hardware_test]
fn never_finishes() {
    let mut dut = Adder4::new();
    gen_clk(dut.clk, mhz(100));
    initial! {
        dut.a = 0x1;
        dut.b = 0x1;
        wait_until(dut.sum == 0x9);
        finish!();
    }
}
