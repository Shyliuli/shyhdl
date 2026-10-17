//! A design written as ordinary Rust, compiled by the `shyhdl` library.
//!
//! This is the Rust-side entry point: the design items are annotated Rust items
//! and the flow is driven from `main` instead of from the command line. It
//! demonstrates the two things that make the interop possible:
//!
//! * the attributes only register the items, so the module below is still plain
//!   Rust and this file builds with `cargo build`;
//! * `simulate` / `Simulator::run` / `Debugger` take the design type, read the
//!   marked items back out of the source, and run the same pipeline as the CLI.
//!
//! Running it:
//!
//! ```console
//! $ cargo run --bin rust_main
//! ```
//!
//! Passing no arguments compiles the design, runs its test and starts a GDB
//! session on the recording; anything else is handed to the ordinary CLI, so this
//! binary is also a usable `shyhdl` command.

use shyhdl::*;

/// A four-bit adder, declared as a Rust struct.
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

/// The behaviour, in an ordinary `impl` block.
#[hardware]
impl Hardware for Adder4 {
    fn run(&mut self) {
        let wide: U<5> = self.a.zext::<5>() + self.b.zext::<5>();
        self.sum = wide.trunc::<4>();
        self.carry = wide.get(4);
    }
}

/// A test over the same design.
#[hardware_test]
fn adds() {
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

fn main() {
    // Arguments belong to the CLI; a bare run drives the library flow instead.
    if std::env::args_os().len() > 1 {
        shyhdl::run();
    }

    // The Verilog text is available without touching the filesystem: the whole
    // design, one module of it, or the generated testbench.
    let dut_text = verilog::<Adder4>().expect("design verilog");
    let module_text = module_verilog::<Adder4>("Adder4").expect("Adder4 module");
    let tb_text = test_verilog::<Adder4>("adds").expect("testbench verilog");
    println!(
        "verilog: {} lines, Adder4 module {} lines, testbench {} lines",
        dut_text.lines().count(),
        module_text.lines().count(),
        tb_text.lines().count()
    );

    // Compile the design once, then run tests against that compilation.
    let mut sim = match simulate::<Adder4>("build/rust-interop") {
        Ok(sim) => sim,
        Err(error) => {
            eprintln!("cannot prepare the design: {error}");
            std::process::exit(1);
        }
    };

    // `adds_id()` names the test through a value, so the name lives in one place.
    let result = match sim.run(adds) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("the run failed to start: {error}");
            std::process::exit(1);
        }
    };
    println!("outcome: {:?}", result.outcome);
    println!(
        "wave={}",
        result
            .wave_path
            .as_ref()
            .map_or_else(|| String::from("<none>"), |path| path.display().to_string(),)
    );
    println!("manifest={}", result.manifest_path.display());

    // Serve the recording of this run to a real GDB over loopback.
    let dbg = match Debugger::new(&result, 10) {
        Ok(dbg) => dbg,
        Err(error) => {
            eprintln!("cannot start the debugger: {error}");
            std::process::exit(1);
        }
    };
    println!("debug start on port :{}", dbg.port);
    if let Err(error) = dbg.run(result) {
        eprintln!("the debug session failed: {error}");
        std::process::exit(1);
    }
}
