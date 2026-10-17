//! The Rust interop surface: a design written as annotated Rust items, driven by
//! the library instead of by the CLI.
//!
//! The design below is deliberately small and non-generic: the point is that the
//! attributes register the items, `check` / `build` read them back out of this
//! file and run the ordinary pipeline, and `list_tests` sees the test.

use std::fs;

use shyhdl::*;

/// A two-bit adder declared as a Rust struct.
#[hardware_instance]
struct TinyAdder {
    #[In]
    a: U<2>,
    #[In]
    b: U<2>,
    #[Out]
    sum: U<2>,
    #[Out]
    carry: bool,
}

/// Its behaviour, in an ordinary `impl` block.
#[hardware]
impl Hardware for TinyAdder {
    fn run(&mut self) {
        let wide: U<3> = self.a.zext::<3>() + self.b.zext::<3>();
        self.sum = wide.trunc::<2>();
        self.carry = wide.get(2);
    }
}

/// A test over the same design.
#[hardware_test]
fn adds_up() {
    let mut dut = TinyAdder::new();
    initial! {
        dut.a = 0x3;
        dut.b = 0x1;
        delay(ns(1));
        assert_eq!(dut.sum, 0x0);
        assert!(dut.carry);
        pass!();
        finish!();
    }
}

/// The `TestId` of `adds_up`, so `Simulator::run` names the test through a value
/// rather than a string literal.
fn adds_up_id() -> TestId {
    TestId("adds_up")
}

#[test]
fn check_accepts_a_design_written_as_rust() {
    check::<TinyAdder>().expect("the registered design must compile");
}

#[test]
fn build_writes_verilog_and_readable_metadata() {
    let dir = std::env::temp_dir().join(format!("shyhdl-interop-{}", std::process::id()));
    let paths = build::<TinyAdder>(&dir).expect("the design must build");

    assert_eq!(
        paths.len(),
        2,
        "build writes Verilog and metadata: {paths:?}"
    );
    for path in &paths {
        assert!(path.is_file(), "{} must exist", path.display());
    }
    let verilog = fs::read_to_string(&paths[0]).expect("read the Verilog");
    assert!(
        verilog.contains("module "),
        "the generated Verilog must declare a module:\n{verilog}"
    );

    let json = fs::read_to_string(&paths[1]).expect("read the metadata");
    let metadata =
        shyhdl::DesignMetadata::from_json_str(&json).expect("the metadata must be readable again");
    assert!(
        !metadata.build_id.is_empty(),
        "the metadata must carry a build id"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn list_tests_sees_the_registered_test() {
    let tests = list_tests::<TinyAdder>();
    assert!(
        tests.iter().any(|name| name == "adds_up"),
        "the #[hardware_test] must be registered: {tests:?}"
    );
}

#[test]
fn simulate_runs_the_registered_test() {
    let dir = std::env::temp_dir().join(format!("shyhdl-interop-run-{}", std::process::id()));
    let mut sim = simulate::<TinyAdder>(&dir).expect("the design must prepare for simulation");
    assert_eq!(sim.top(), "TinyAdder");

    let result = sim.run(adds_up_id).expect("the test must run");
    assert_eq!(
        result.outcome,
        shyhdl::Outcome::Pass,
        "the test must pass: {}",
        result.reason
    );
    assert!(
        result.manifest_path.is_file(),
        "the run must write its manifest: {}",
        result.manifest_path.display()
    );
    assert!(
        result.wave_path.as_ref().is_some_and(|path| path.is_file()),
        "the run must record a waveform: {:?}",
        result.wave_path
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn source_for_points_at_the_declaring_file() {
    let origin = source_for::<TinyAdder>();
    assert!(
        origin.contains("macro_interop.rs"),
        "the origin must name this file: {origin}"
    );
}
