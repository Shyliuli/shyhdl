//! The smallest complete example: one adder, one test, exported and simulated
//! from an ordinary Rust `main`.
//!
//! Run it with `cargo run -p shyhdl-example-adder`.

use shyhdl::*;

/// A four-bit adder declared as a Rust struct. The attribute only records the
/// item; the fields never become real machine state.
#[hardware_instance]
struct Adder4 {
    #[In]
    a: U<4>,
    #[In]
    b: U<4>,
    #[Out]
    sum: U<4>,
    #[Out]
    carry: bool,
}

/// The behaviour. `run` is compiled from this text by the ShyHDL frontend, not
/// by rustc.
#[hardware]
impl Hardware for Adder4 {
    fn run(&mut self) {
        let wide: U<5> = self.a.zext::<5>() + self.b.zext::<5>();
        self.sum = wide.trunc::<4>();
        self.carry = wide.get(4);
    }
}

/// 0xF + 0x1 wraps to 0x0 and sets the carry out.
#[hardware_test]
fn wraps() {
    let mut dut = Adder4::new();
    initial! {
        dut.a = 0xF;
        dut.b = 0x1;
        delay(ns(1));
        assert_eq!(dut.sum, 0x0);
        assert!(dut.carry);
        pass!();
        finish!();
    }
}

fn main() -> shyhdl::Result<()> {
    let text = verilog::<Adder4>()?;
    println!("Adder4: {} lines of Verilog", text.lines().count());

    let mut sim = simulate::<Adder4>("build/adder")?;
    let result = sim.run(wraps)?;
    println!("wraps: {:?}", result.outcome);
    if result.outcome != Outcome::Pass {
        std::process::exit(1);
    }

    let dbg = Debugger::new(&result, 10)?;
    println!("debug start on port :{}", dbg.port);
    if std::env::var_os("SHYHDL_DEBUG").is_some() {
        // Blocks while one GDB session replays this recording.
        dbg.run()?;
    }
    Ok(())
}
