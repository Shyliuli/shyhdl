//! G1: the seam between the two halves of the record/replay chain.
//!
//! `shyhdl test` compiles ShyHDL source, generates the testbench, runs it under
//! Verilator and writes a recording with a run manifest. `shyhdl debug
//! --manifest` then serves that recording to a real GDB, which prints a DUT
//! register. `tests/simulation_e2e.rs` stops at the manifest and
//! `tests/debugger_gdb.rs` starts from a hand-written recording, so neither one
//! makes a value survive the whole chain.
//!
//! The testcase instantiates one register twice, at 100 MHz and 50 MHz, loaded
//! with different values. Each instance is debugged with `--instance` and with
//! neither `--clock` nor `--grid-interval`, so the clocks the testcase declared
//! with `gen_clk` are what the debugger defaults to: if a session stepped on the
//! other instance's clock, it would cross the other instance's number of
//! reference points.
//!
//! Verilator and a real `gdb` are hard requirements; a missing tool fails the
//! test rather than skipping it.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use shyhdl::simulation::{load_manifest, Simulator};

/// The binary under test, built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_shyhdl");

/// How long one `shyhdl debug` session plus its GDB may take before the test
/// kills it: a stuck handshake must fail the test, not hang the test binary.
const SESSION_DEADLINE: Duration = Duration::from_secs(60);

/// Wall-clock limit for each external process, applied with GNU `timeout`.
/// The deadline above only covers the wait *after* GDB returns; the handshake
/// read and GDB itself are blocking calls, so they need a limit of their own.
const PROCESS_TIMEOUT_SECS: &str = "60";

const TOP: &str = "Reg8";
const TEST: &str = "two_frequencies_two_values";

/// What the testcase loads last, and therefore what each session must end on.
const FAST_LAST: u64 = 0x33;
const SLOW_VALUE: u64 = 0x22;

/// The design and the testcase that produces the recording.
///
/// `reset!` drives the inactive level at time zero, asserts after one time unit
/// and releases after `hold`, so the stimulus waits for the assert and the
/// release of both instances before it samples anything. The first clock edge of
/// either instance therefore happens while the reset is asserted, which gives
/// the first observation point a defined zero instead of an unknown value.
/// `fast` changes its input once, so its last two reference points differ.
const SOURCE: &str = r#"
#[hardware_instance]
struct Reg8 {
    #[In] d: uint<8>,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: uint<8>,
}

impl Hardware for Reg8 {
    fn run(&mut self) {
        always! {
            if negedge(self.rst_n) {
                self.q = 0;
            } else if posedge(self.clk) {
                self.q = self.d;
            }
        }
    }
}

#[hardware_test]
fn two_frequencies_two_values() {
    let mut fast = Reg8::new();
    let mut slow = Reg8::new();

    gen_clk(fast.clk, mhz(100));
    gen_clk(slow.clk, mhz(50));
    reset!(fast.rst_n, active_low, hold = ns(20));
    reset!(slow.rst_n, active_low, hold = ns(20));

    initial! {
        fast.d = 0x11;
        slow.d = 0x22;

        wait_until(fast.rst_n == false);
        wait_until(slow.rst_n == false);
        wait_until(fast.rst_n == true);
        wait_until(slow.rst_n == true);

        sample_after(posedge(fast.clk), ps(1));
        assert_eq!(fast.q, 0x11);
        sample_after(posedge(slow.clk), ps(1));
        assert_eq!(slow.q, 0x22);

        fast.d = 0x33;
        sample_after(posedge(fast.clk), ps(1));
        assert_eq!(fast.q, 0x33);
        assert_eq!(slow.q, 0x22);

        pass!();
        finish!();
    }
}
"#;

/// A build directory under `target/`, unique per test process and wiped so no
/// stale artifact survives.
///
/// The process id keeps two concurrent `cargo test` invocations of this file
/// from deleting each other's recording: they share the source tree, not the
/// scratch directory.
fn build_dir(label: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/pipeline-debug")
        .join(format!("{}-{}", std::process::id(), label));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create the build directory");
    dir
}

/// Locate a required tool on `PATH`; a missing tool is a failure, not a skip.
fn tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "`{name}` is not on PATH, so this test cannot run; put the tool environment on \
         PATH (for example by sourcing the kit's env.sh)"
    );
}

/// Verilator, GDB and GNU `timeout` are hard requirements: an environment gap
/// is a failure.
fn require_tools() {
    Simulator::discover().unwrap_or_else(|error| {
        panic!(
            "a real Verilator is required for the full-chain test and none was usable: {error}\n\
             put the tool environment on PATH (for example by sourcing the kit's env.sh)"
        )
    });
    let gdb = Command::new(tool("gdb")).arg("-version").output();
    match gdb {
        Ok(output) if output.status.success() => {}
        Ok(output) => panic!("`gdb -version` failed; this test needs a real GDB: {output:?}"),
        Err(error) => panic!("cannot run gdb ({error}); this test needs a real GDB"),
    }
    let _ = tool("timeout");
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {BIN}: {error}"))
}

/// One line of the GDB script a session runs.
///
/// The script is data so one harness serves both a scalar read and a composite
/// value read field by field, while the `step` / `reverse-step` / `continue`
/// skeleton stays identical.
enum Script<'a> {
    /// Print `$pc` as `[label]`.
    Here(&'a str),
    /// Print one expression as `[label]`, for example `Maker__out.m.tag`.
    Read(&'a str, &'a str),
    /// Print one expression as `[label]` with signed formatting, for a value
    /// whose type is signed (`%u` would print a negative `int<8>` as 255).
    ReadSigned(&'a str, &'a str),
    /// Any other GDB command, for example `step`.
    Run(&'a str),
}

/// Read one scalar `symbol` across `step`, `reverse-step` and `continue`.
///
/// This is the original session, kept as a thin wrapper over
/// [`debug_with_gdb_script`] so existing callers and their expectations do not
/// change.
fn debug_with_gdb(manifest: &Path, instance: &str, symbol: &str) -> String {
    debug_with_gdb_script(
        manifest,
        instance,
        &[
            Script::Here("pc0"),
            Script::Read("q0", symbol),
            Script::Run("step"),
            Script::Here("pc1"),
            Script::Read("q1", symbol),
            Script::Run("reverse-step"),
            Script::Read("qb", symbol),
            Script::Run("continue"),
            Script::Read("qe", symbol),
        ],
    )
}

/// Start `shyhdl debug --manifest ... --instance ...`, run a real GDB against
/// it, and return what GDB printed.
///
/// Both processes run under GNU `timeout`: reading the handshake and running GDB
/// are blocking calls, so without a limit of their own a stuck handshake would
/// hang the test binary instead of failing. The debugger is also reaped after
/// GDB returns, so a session that outlives its GDB is killed.
fn debug_with_gdb_script(manifest: &Path, instance: &str, script: &[Script<'_>]) -> String {
    let mut child: Child = Command::new(tool("timeout"))
        .arg("-k")
        .arg("5")
        .arg(PROCESS_TIMEOUT_SECS)
        .arg(BIN)
        .arg("debug")
        .arg("--manifest")
        .arg(manifest)
        .arg("--instance")
        .arg(instance)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run {BIN}: {error}"));

    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut header = Vec::new();
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .expect("read the debugger handshake")
        > 0
    {
        let text = line.trim_end().to_string();
        header.push(text.clone());
        line.clear();
        if text.starts_with("port=") {
            break;
        }
    }
    let kill = |child: &mut Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let mut field = |name: &str| -> String {
        let found = header
            .iter()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_string);
        found.unwrap_or_else(|| {
            let code = child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code());
            kill(&mut child);
            let note = if code == Some(124) {
                format!(" (killed after {PROCESS_TIMEOUT_SECS}s)")
            } else {
                String::new()
            };
            panic!(
                "the debugger printed no `{name}`{note}, so no session started; it printed:\n{}",
                header.join("\n")
            )
        })
    };
    let elf = field("elf=");
    let port = field("port=");

    // The script decides the stops; the harness only turns it into `-ex`
    // arguments, after the connection facts it reads from the handshake.
    let mut commands = vec![
        "set confirm off".to_string(),
        "set pagination off".to_string(),
        format!("target remote 127.0.0.1:{port}"),
    ];
    for step in script {
        commands.push(match step {
            Script::Here(label) => format!("printf \"[{label}] %p\\n\", $pc"),
            Script::Read(label, expr) => format!("printf \"[{label}] %u\\n\", {expr}"),
            Script::ReadSigned(label, expr) => format!("printf \"[{label}] %d\\n\", {expr}"),
            Script::Run(command) => command.to_string(),
        });
    }
    let mut gdb = Command::new(tool("timeout"));
    gdb.args(["-k", "5", PROCESS_TIMEOUT_SECS])
        .arg("gdb")
        .args(["-q", "-batch", "-nx"])
        .arg(&elf);
    for command in &commands {
        gdb.arg("-ex").arg(command);
    }
    let gdb = match gdb.output() {
        Ok(output) => output,
        Err(error) => {
            kill(&mut child);
            panic!("cannot run gdb ({error}); this test needs a real GDB");
        }
    };
    let console =
        String::from_utf8_lossy(&gdb.stdout).into_owned() + &String::from_utf8_lossy(&gdb.stderr);
    if matches!(gdb.status.code(), Some(124) | Some(137)) {
        kill(&mut child);
        panic!("gdb was killed after {PROCESS_TIMEOUT_SECS}s; it printed:\n{console}");
    }
    assert!(
        gdb.status.success(),
        "gdb failed against `shyhdl debug`:\n{console}"
    );

    // Reap the debugger: it must exit once GDB disconnects, and it must not be
    // left behind if it does not.
    let deadline = Instant::now() + SESSION_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the debugger") {
            break status;
        }
        if Instant::now() > deadline {
            kill(&mut child);
            panic!(
                "the debugger did not exit after GDB disconnected:\n{}",
                header.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if matches!(status.code(), Some(124) | Some(137)) {
        panic!(
            "the debugger was killed after {PROCESS_TIMEOUT_SECS}s instead of exiting:\n{}",
            header.join("\n")
        );
    }
    assert!(
        status.success(),
        "the debugger exited with {status} after the session:\n{}",
        header.join("\n")
    );
    console
}

/// The text GDB printed after `[label] `, which is absent when a read did not
/// happen, so a missing marker fails loudly instead of comparing nothing.
fn marked<'a>(console: &'a str, label: &str) -> &'a str {
    let marker = format!("[{label}] ");
    console
        .lines()
        .find_map(|line| line.split(&marker).nth(1))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no `{label}` line in the GDB console:\n{console}"))
}

fn number(console: &str, label: &str) -> u64 {
    marked(console, label)
        .parse()
        .unwrap_or_else(|_| panic!("`{label}` is not a number in:\n{console}"))
}

/// The signed number on a `[label] ` line, for a field whose type is signed.
fn signed_number(console: &str, label: &str) -> i64 {
    marked(console, label)
        .parse()
        .unwrap_or_else(|_| panic!("`{label}` is not a signed number in:\n{console}"))
}

fn address(console: &str, label: &str) -> u64 {
    let text = marked(console, label);
    let hex = text
        .strip_prefix("0x")
        .unwrap_or_else(|| panic!("`{label}` is not an address in:\n{console}"));
    u64::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("`{label}` is not hex: {text}"))
}

/// The composite debug test's design: primitive inputs in, one structured output
/// out. The testbench only ever drives scalars, because a struct-typed value has
/// to come from a DUT port (`src/testbench/lower.rs`).
///
/// `Payload` is the M4 shape, and its debug layout (REQ-DBG-009, REQ-LANG-040) is
/// what the test pins down:
///
/// | field | type | debug bytes |
/// | --- | --- | --- |
/// | `i` | `int<8>` | 1 |
/// | `xs` | `[uint<8>; 3]` | 3 |
/// | `m` | `Option<Result<uint<4>, int<8>>>` | 3 |
/// | `after` | `uint<4>` | 1 |
///
/// `after` starts at byte 7 whatever `m`'s active variant is: a payload union
/// that resized with the tag would move it, so the test reads `after` at every
/// stop and after a reverse-step.
const COMPOSITE_TOP: &str = "Maker";
const COMPOSITE_TEST: &str = "composite_values";
const COMPOSITE_AFTER: u64 = 5;

const COMPOSITE_SOURCE: &str = r#"
struct Payload {
    i: int<8>,
    xs: [uint<8>; 3],
    m: Option<Result<uint<4>, int<8>>>,
    after: uint<4>,
}

#[hardware_instance]
struct Maker {
    #[In] a: int<8>,
    #[In] b: uint<8>,
    #[In] sel: uint<2>,
    #[In] clk: bool,
    #[Out] out: Payload,
}

impl Hardware for Maker {
    fn run(&mut self) {
        // One process drives the whole output, so no two processes ever write
        // different fields of the same packed port.
        always! {
            if posedge(self.clk) {
                self.out.i = self.a;
                self.out.xs = [self.b, self.b + 1, self.b + 2];
                self.out.after = 5;
                if self.sel == 0 {
                    self.out.m = None;
                } else if self.sel == 1 {
                    self.out.m = Some(Ok(9));
                } else {
                    self.out.m = Some(Err(-8));
                }
            }
        }
    }
}

#[hardware_test]
fn composite_values() {
    let mut dut = Maker::new();

    gen_clk(dut.clk, mhz(100));

    initial! {
        dut.a = -1;
        dut.b = 0x30;

        // Each variant is held for several clock edges before the next one, so
        // the end of the recording is unambiguous: the value the debugger reads
        // there has been stable for more than one edge, and does not depend on
        // whether an edge's own observation point is sampled before or after the
        // edge updates the port.
        dut.sel = 0;
        sample_after(posedge(dut.clk), ps(1));
        sample_after(posedge(dut.clk), ps(1));
        assert_eq!(dut.out.after, 5);

        dut.sel = 1;
        sample_after(posedge(dut.clk), ps(1));
        sample_after(posedge(dut.clk), ps(1));
        assert_eq!(dut.out.after, 5);

        dut.sel = 2;
        sample_after(posedge(dut.clk), ps(1));
        sample_after(posedge(dut.clk), ps(1));
        assert_eq!(dut.out.after, 5);

        pass!();
        finish!();
    }
}
"#;

/// The composite chain through a real GDB: one typed field of every kind, and
/// the field after the payload enum.
///
/// The value assertions are deliberately of three kinds that do not depend on
/// how many observation points Verilator happens to record: the layout invariant
/// (`after` is 5 at every stop), same-value restoration (every field reads at
/// `reverse-step` exactly what it read before `step`), and the end of the
/// recording, where the last value the testcase loaded is known.
#[test]
fn shyhdl_test_to_debug_to_gdb_composite_values() {
    require_tools();
    let dir = build_dir("composite");
    let source = dir.join("composite.shy.rs");
    fs::write(&source, COMPOSITE_SOURCE).expect("write the source");
    let out = dir.join("run");

    let test_run = run(&[
        "test",
        source.to_str().unwrap(),
        "--top",
        COMPOSITE_TOP,
        "--test",
        COMPOSITE_TEST,
        "--out",
        out.to_str().unwrap(),
        "--wave",
        "vcd",
    ]);
    let text = String::from_utf8_lossy(&test_run.stdout).into_owned();
    assert!(
        test_run.status.success() && text.starts_with(&format!("pass test={COMPOSITE_TEST}")),
        "`shyhdl test` must pass:\nstdout:\n{text}\nstderr:\n{}",
        String::from_utf8_lossy(&test_run.stderr)
    );

    let manifest_path = out.join("run.manifest.json");
    let manifest = load_manifest(&manifest_path).expect("the run must write a manifest");
    assert_eq!(manifest.design.top, COMPOSITE_TOP);
    assert_eq!(
        manifest.roots.len(),
        1,
        "the composite testcase instantiates the DUT once"
    );

    let root = "Maker__out";
    // Address and size facts, asked of GDB itself: they are what proves the
    // payload union did not move the field after it, which a value that merely
    // fits four bits would not.
    let ptype_union = format!("ptype {root}.m.payload.Some");
    let ptype_array = format!("ptype {root}.xs");
    let offset_command =
        format!("printf \"[offset] %ld\\n\", (long)((char *)&{root}.after - (char *)&{root}.i)");
    let size_command = format!("printf \"[size] %d\\n\", (int)sizeof({root})");

    let console = debug_with_gdb_script(
        &manifest_path,
        "dut",
        &[
            Script::Here("pc0"),
            Script::ReadSigned("i0", &format!("{root}.i")),
            Script::Read("x0", &format!("{root}.xs[0]")),
            Script::Read("after0", &format!("{root}.after")),
            Script::Run(ptype_union.as_str()),
            Script::Run(ptype_array.as_str()),
            Script::Run(offset_command.as_str()),
            Script::Run(size_command.as_str()),
            Script::Run("step"),
            Script::Here("pc1"),
            Script::ReadSigned("i1", &format!("{root}.i")),
            Script::Read("x0b", &format!("{root}.xs[0]")),
            Script::Read("x2b", &format!("{root}.xs[2]")),
            Script::Read("after1", &format!("{root}.after")),
            Script::Run("reverse-step"),
            Script::Here("pcb"),
            Script::ReadSigned("ib", &format!("{root}.i")),
            Script::Read("x0c", &format!("{root}.xs[0]")),
            Script::Read("afterb", &format!("{root}.after")),
            Script::Run("continue"),
            Script::Here("pce"),
            Script::ReadSigned("ie", &format!("{root}.i")),
            Script::Read("x0e", &format!("{root}.xs[0]")),
            Script::Read("x2e", &format!("{root}.xs[2]")),
            Script::Read("tage", &format!("{root}.m.tag")),
            Script::Read("oktag_e", &format!("{root}.m.payload.Some.tag")),
            Script::ReadSigned("erre", &format!("{root}.m.payload.Some.payload.Err")),
            Script::Read("aftere", &format!("{root}.after")),
        ],
    );

    // Every typed path resolved: a missing DIE, a wrong union member name or a
    // payload that overlapped a neighbour would fail a read instead of printing
    // a number.
    assert!(
        !console.contains("Cannot access memory") && !console.contains("No symbol"),
        "every typed field of the composite output must be readable:\n{console}"
    );

    // The offset of the field after the payload enum, and the size of the whole
    // struct, as GDB itself computes them. This is the layout assertion: the
    // enum is one tag byte plus the widest payload, so `after` starts at byte 7
    // and the struct is 8 bytes, whatever the active variant is.
    assert_eq!(
        number(&console, "offset"),
        7,
        "the field after the payload enum starts at byte 7:\n{console}"
    );
    assert_eq!(
        number(&console, "size"),
        8,
        "the composite output is eight debug bytes:\n{console}"
    );

    // GDB sees the payload as a union of the variant payloads and the array as a
    // three-element array, not as one wide scalar.
    assert!(
        console.contains("union"),
        "the payload is a union of the variant payloads:\n{console}"
    );
    assert!(
        console.contains("uint<8> [3]"),
        "the array keeps its three uint<8> elements:\n{console}"
    );

    // The recording's first stop is before any clock edge, so the output has not
    // been driven yet: Verilator's model is two-state, so it reads zero.
    assert_eq!(
        signed_number(&console, "i0"),
        0,
        "the output is undriven at the first stop:\n{console}"
    );
    assert_eq!(number(&console, "x0"), 0, "and so is the array:\n{console}");
    assert_eq!(
        number(&console, "after0"),
        0,
        "and the field after the enum too:\n{console}"
    );

    // One `step` reaches the first clock edge, where every field of the output
    // has the value the testcase drove.
    assert_eq!(
        address(&console, "pc1"),
        address(&console, "pc0") + 8,
        "step advances one observation point:\n{console}"
    );
    assert_eq!(
        signed_number(&console, "i1"),
        -1,
        "a signed output field is negative after the first edge:\n{console}"
    );
    assert_eq!(
        number(&console, "x0b"),
        0x30,
        "array element 0 after the first edge:\n{console}"
    );
    assert_eq!(
        number(&console, "x2b"),
        0x32,
        "element 2 is the third element, at its own debug stride:\n{console}"
    );
    assert_eq!(
        number(&console, "after1"),
        COMPOSITE_AFTER,
        "the field after the payload enum holds its driven value:\n{console}"
    );

    // `reverse-step` returns to the first stop and reads those values again.
    assert_eq!(
        address(&console, "pcb"),
        address(&console, "pc0"),
        "reverse-step returns to the point step left:\n{console}"
    );
    assert_eq!(
        signed_number(&console, "ib"),
        0,
        "the signed field is restored:\n{console}"
    );
    assert_eq!(
        number(&console, "x0c"),
        0,
        "an array element is restored:\n{console}"
    );
    assert_eq!(
        number(&console, "afterb"),
        0,
        "the field after the enum is restored:\n{console}"
    );

    // The end of the recording holds the last value the testcase loaded:
    // `sel = 2`, so the variant is `Some(Err(-8))`, and a signed field prints
    // negative rather than as its unsigned bit pattern.
    assert_eq!(
        signed_number(&console, "ie"),
        -1,
        "an int<8> of -1 prints as -1, not 255:\n{console}"
    );
    assert_eq!(
        number(&console, "x0e"),
        0x30,
        "array element 0 at the end:\n{console}"
    );
    // Element 2 is the third element, not the first one's bits again: the array
    // stride in the debug layout is one whole byte per element.
    assert_eq!(
        number(&console, "x2e"),
        0x32,
        "array element 2 is sliced at the last element's stride:\n{console}"
    );
    assert_eq!(
        number(&console, "tage"),
        1,
        "the outer Option tag is Some at the end:\n{console}"
    );
    assert_eq!(
        number(&console, "oktag_e"),
        1,
        "the inner Result tag is Err at the end:\n{console}"
    );
    assert_eq!(
        signed_number(&console, "erre"),
        -8,
        "a signed payload inside the union prints negative:\n{console}"
    );
    assert_eq!(
        number(&console, "aftere"),
        COMPOSITE_AFTER,
        "and the field after the enum still holds its value:\n{console}"
    );
    assert!(
        address(&console, "pce") > address(&console, "pc1"),
        "continue runs on past the stop it resumed from:\n{console}"
    );
}

/// The whole chain, once: source, CLI run, manifest, debug sessions, GDB values.
#[test]
fn shyhdl_test_to_debug_to_gdb_print_over_two_clock_frequencies() {
    require_tools();
    let dir = build_dir("full-chain");
    let source = dir.join("full_chain.shy.rs");
    fs::write(&source, SOURCE).expect("write the source");
    let out = dir.join("run");

    // 1. The real CLI run: compile, generate the testbench, simulate, record.
    let test_run = run(&[
        "test",
        source.to_str().unwrap(),
        "--top",
        TOP,
        "--test",
        TEST,
        "--out",
        out.to_str().unwrap(),
        "--wave",
        "vcd",
    ]);
    let text = String::from_utf8_lossy(&test_run.stdout).into_owned();
    assert!(
        test_run.status.success() && text.starts_with(&format!("pass test={TEST}")),
        "`shyhdl test` must pass:\nstdout:\n{text}\nstderr:\n{}",
        String::from_utf8_lossy(&test_run.stderr)
    );

    // 2. The manifest pairs the recording with the design and carries the
    //    testcase clocks, one per DUT instance, which is what `debug` defaults
    //    to when no step mode is given.
    let manifest_path = out.join("run.manifest.json");
    let manifest = load_manifest(&manifest_path).expect("the run must write a manifest");
    assert_eq!(manifest.design.top, TOP);
    assert!(
        manifest.design.build_id.is_some(),
        "the manifest must record its build id"
    );
    assert!(
        manifest.paths.metadata.is_some(),
        "the manifest must name its metadata"
    );
    assert_eq!(
        manifest.roots.len(),
        2,
        "the testcase instantiates the DUT twice"
    );

    let clock_of = |var: &str| -> String {
        let clocks: Vec<&str> = manifest
            .clocks
            .iter()
            .filter(|clock| clock.var == var)
            .map(|clock| clock.signal.as_str())
            .collect();
        assert_eq!(
            clocks.len(),
            1,
            "`{var}` must have exactly one testcase clock: {clocks:?}"
        );
        clocks[0].to_string()
    };
    let fast_clock = clock_of("fast");
    let slow_clock = clock_of("slow");
    assert!(
        fast_clock != slow_clock && fast_clock.contains("u_fast") && slow_clock.contains("u_slow"),
        "each instance needs its own clock: {fast_clock} / {slow_clock}"
    );

    // 3. Debug each instance on its own, with no explicit step mode: the clock
    //    the testcase declared for that instance has to be selected. The symbol
    //    of the top's own `q` is `<top>__<source name>`
    //    (`src/debugger/symbols.rs`); a wrong name makes GDB print no `[q..]`
    //    line, which fails below rather than comparing nothing.
    let symbol = format!("{TOP}__q");
    let fast = debug_with_gdb(&manifest_path, "fast", &symbol);
    let slow = debug_with_gdb(&manifest_path, "slow", &symbol);

    // The cursor starts at the recording's first observation point; Verilator's
    // model is two-state, so an unreset register reads zero there. The first
    // `step` moves to the first reference point, which is a clock edge that
    // happens after the reset was asserted, so it also reads a defined zero.
    // `reverse-step` then returns to the point `step` left and reads its value
    // (AC-MVP-022, AC-MVP-023).
    assert_eq!(number(&fast, "q0"), 0, "fast at its first point:\n{fast}");
    assert_eq!(number(&fast, "q1"), 0, "fast at its first edge:\n{fast}");
    assert_eq!(
        number(&fast, "qb"),
        number(&fast, "q0"),
        "fast after reverse-step:\n{fast}"
    );
    assert_eq!(number(&slow, "q0"), 0, "slow at its first point:\n{slow}");
    assert_eq!(number(&slow, "q1"), 0, "slow at its first edge:\n{slow}");
    assert_eq!(
        number(&slow, "qb"),
        number(&slow, "q0"),
        "slow after reverse-step:\n{slow}"
    );

    // The end of the recording holds what the testcase loaded last, and the two
    // instances share neither a scope nor a value.
    assert_eq!(
        number(&fast, "qe"),
        FAST_LAST,
        "fast must end on its loaded value:\n{fast}"
    );
    assert_eq!(
        number(&slow, "qe"),
        SLOW_VALUE,
        "slow must end on its loaded value:\n{slow}"
    );
    assert_ne!(
        number(&fast, "qe"),
        number(&fast, "qb"),
        "fast must change value:\n{fast}"
    );

    // One `step` advances to the next edge of that session's clock, and `$pc`
    // counts recorded observation points, which every other signal also adds
    // to. A 50 MHz step therefore spans more of them than a 100 MHz step over
    // the same recording. Equal spans would mean both sessions stepped on one
    // clock, which is exactly what the per-instance default has to prevent.
    let step_span = |console: &str| address(console, "pc1") - address(console, "pc0");
    assert!(
        step_span(&slow) > step_span(&fast),
        "fast={} slow={}\nfast console:\n{fast}\nslow console:\n{slow}",
        step_span(&fast),
        step_span(&slow)
    );
}
