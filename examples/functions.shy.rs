// ShyHDL source (Rust-style DSL). Compiled by `frontend::compile`, not by rustc.
//
// A combinational function: `#[hardware_instance] fn` declares one, its
// parameters are the inputs and its return type lists the outputs. Every call
// site instantiates it, so calling it twice builds two independent adders.
//
// The function body is ordinary combinational code and must end with the value
// it returns. It may not contain `always!`: sequential logic belongs to a
// module's `run`.

#[hardware_instance]
fn Adder4(a: uint<4>, b: uint<4>) -> (uint<4>, bool) {
    let wide: uint<5> = a.zext::<5>() + b.zext::<5>();
    (wide[0..4], wide.get(4))
}

#[hardware_instance]
struct TwoAdders {
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    #[In] c: uint<4>,
    #[Out] first: uint<4>,
    #[Out] second: uint<4>,
    #[Out] carry_out: bool,
}

impl Hardware for TwoAdders {
    fn run(&mut self) {
        // A run-level `let` declares module signals, and destructuring binds one
        // signal per name.
        let (sum, carry) = Adder4(a, b);
        first = sum;
        // The second call is a second instance, wired from the first result.
        let (sum2, _) = Adder4(sum, c);
        second = sum2;
        carry_out = carry;
    }
}

#[hardware_test]
fn two_adders_add() {
    let mut dut = TwoAdders::new();
    initial! {
        dut.a = 0xF;
        dut.b = 0x1;
        dut.c = 0x2;
        delay(ns(1));
        assert_eq!(dut.first, 0x0);
        assert_eq!(dut.second, 0x2);
        assert!(dut.carry_out);
        pass!();
        finish!();
    }
}
