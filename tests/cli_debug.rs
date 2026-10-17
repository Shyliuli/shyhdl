//! End-to-end tests of `shyhdl debug`.
//!
//! The recording, metadata and run manifest here are written by the test, so
//! the pairing rules can be exercised without a Verilator run. The two cases
//! that need a real GDB drive the binary over the printed loopback port and
//! check what standard GDB commands see; a missing `gdb` fails the test with a
//! clear message rather than being skipped.

use std::fs;
use std::io::{BufRead, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use shyhdl::metadata::{DesignMetadata, SourceSpan, ValueType, Variable};
use shyhdl::simulation::{digest_file, Simulator};

const BIN: &str = env!("CARGO_BIN_EXE_shyhdl");

/// Refused: the symbols and the recording disagree, or a choice is ambiguous.
const REFUSED: i32 = 8;
/// Usage error: a mode was asked for without the input it needs.
const USAGE: i32 = 2;

/// A 40 ns recording of a 4-bit adder under a testbench wrapper. The design
/// root `Adder4` lives at `TOP.tb.u_dut`, and `clk` has a 10 ns grid.
const VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module u_dut $end
$var wire 4 ! a $end
$var wire 4 \" b $end
$var wire 1 # clk $end
$var wire 4 $ sum $end
$var wire 1 % carry $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
b0000 !
b0000 \"
0#
b0000 $
0%
#10
1#
#20
0#
#30
1#
#40
0#
";

/// Another run of the same design: the same signals, other stimulus, so the
/// file content — and therefore its digest — differs.
const OTHER_VCD: &str = "\
$timescale 1ns $end
$scope module TOP $end
$scope module tb $end
$scope module u_dut $end
$var wire 4 ! a $end
$var wire 4 \" b $end
$var wire 1 # clk $end
$var wire 4 $ sum $end
$var wire 1 % carry $end
$upscope $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
b0001 !
b0001 \"
0#
b0010 $
0%
#10
1#
#20
0#
";

fn variable(id: &str, name: &str, ty: ValueType) -> Variable {
    Variable {
        id: id.to_string(),
        source_name: name.to_string(),
        instance_path: "Adder4".to_string(),
        rtl_name: name.to_string(),
        ty,
        source: SourceSpan::new("alu.shy.rs", 1, 1),
    }
}

/// Metadata of the fixture design. `build_id` is a parameter so a test can
/// write metadata that belongs to another build.
///
/// The design metadata carries no clock marking: which signal is the reference
/// clock is a property of the testcase, and it is recorded in the run manifest.
fn metadata(build_id: &str) -> DesignMetadata {
    let uint4 = ValueType::UInt { width: 4 };
    DesignMetadata::new(
        build_id,
        "Adder4",
        vec![
            variable("Adder4/a", "a", uint4.clone()),
            variable("Adder4/b", "b", uint4.clone()),
            variable("Adder4/clk", "clk", ValueType::Bool),
            variable("Adder4/sum", "sum", uint4),
            variable("Adder4/carry", "carry", ValueType::Bool),
        ],
    )
}

/// The run manifest of the fixture design, as the simulation layer writes it.
///
/// Written as JSON text on purpose: `RunManifest`'s nested types are not
/// re-exported by `shyhdl::simulation`, so a consumer outside the crate cannot
/// build one by value. The CLI reads it through `simulation::load_manifest`, so
/// a malformed fixture fails the same way a malformed real manifest would.
fn manifest_json(
    build_id: &str,
    wave_path: &str,
    digest: &str,
    clocks: &[(&str, &str)],
    roots: &[(&str, &str)],
) -> String {
    let clocks = clocks
        .iter()
        .map(|(var, signal)| {
            format!(
                r#"    {{
      "var": "{var}",
      "signal": "{signal}"
    }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let roots = roots
        .iter()
        .map(|(var, wave_root)| {
            format!(
                r#"    {{
      "source_root": "Adder4",
      "wave_root": "{wave_root}",
      "var": "{var}",
      "instance": "u_{var}",
      "module": "Adder4",
      "module_source_name": "Adder4"
    }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        r#"{{
  "schema": 3,
  "run_id": "run-fixture-1",
  "design": {{
    "top": "Adder4",
    "top_source_name": "Adder4",
    "build_id": "{build_id}"
  }},
  "test": {{
    "name": "wraps_and_carries",
    "top": "tb_Adder4_wraps_and_carries",
    "top_type": "Vtb_Adder4_wraps_and_carries",
    "timeunit": "1ns",
    "timeprecision": "1ps"
  }},
  "paths": {{
    "manifest": "run.manifest.json",
    "dut_verilog": ["Adder4.v"],
    "tb_verilog": "tb_Adder4_wraps_and_carries.v",
    "metadata": "Adder4.meta.json",
    "build_log": "build.log"
  }},
  "wave": {{
    "path": "{wave_path}",
    "format": "vcd",
    "timescale_in_file": "1ns",
    "digest": "{digest}"
  }},
  "roots": [
{roots}
  ],
  "clocks": [
{clocks}
  ],
  "backend": {{
    "name": "verilator",
    "version": "fixture",
    "flags": ["--timing"]
  }},
  "seed": 0,
  "parameters": [],
  "sampling": "fixture",
  "outcome": "pass",
  "exit_code": 0,
  "counts": {{
    "passes": 2,
    "fails": 0,
    "markers": 1,
    "expected_markers": 1
  }}
}}
"#
    )
}

/// How the fixture's recording relates to the manifest written for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recording {
    /// The file the manifest was written for.
    Matching,
    /// The same path, rewritten after the manifest was written: a later run of
    /// the same design overwrote the waveform.
    Overwritten,
    /// Another run's file, named by this manifest.
    AnotherRun,
}

/// A directory that is removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "shyhdl-cli-debug-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            label
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// Write a recording, metadata and manifest into a fresh directory.
    ///
    /// `metadata_build_id` differs from `manifest_build_id` when a test wants
    /// symbols from another build.
    fn new(
        label: &str,
        metadata_build_id: &str,
        manifest_build_id: &str,
        clocks: &[(&str, &str)],
        recording: Recording,
        roots: &[(&str, &str)],
    ) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shyhdl-cli-debug-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            label
        ));
        fs::create_dir_all(&dir).expect("create fixture directory");

        // The manifest's identity is the content digest of the recording it was
        // written for, so the fixture computes it with the same public helper
        // the simulation layer uses: never a hard-coded value.
        let signed = dir.join("wave.vcd");
        fs::write(&signed, VCD).expect("write the recording");
        let digest = digest_file(&signed).expect("digest the recording");
        let wave_path = match recording {
            Recording::AnotherRun => "wave-b.vcd",
            Recording::Matching | Recording::Overwritten => "wave.vcd",
        };
        match recording {
            Recording::Matching => {}
            Recording::Overwritten => {
                fs::write(&signed, OTHER_VCD).expect("overwrite the recording")
            }
            Recording::AnotherRun => {
                fs::write(dir.join("wave-b.vcd"), OTHER_VCD).expect("write the other run")
            }
        }

        fs::write(
            dir.join("Adder4.meta.json"),
            metadata(metadata_build_id)
                .to_json_string()
                .expect("serialize metadata"),
        )
        .expect("write the metadata");
        fs::write(
            dir.join("run.manifest.json"),
            manifest_json(manifest_build_id, wave_path, &digest, clocks, roots),
        )
        .expect("write the manifest");
        Self { dir }
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("run.manifest.json")
    }

    fn metadata_path(&self) -> PathBuf {
        self.dir.join("Adder4.meta.json")
    }

    fn recording_path(&self) -> PathBuf {
        self.dir.join("wave.vcd")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {BIN}: {error}"))
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .unwrap_or_else(|| panic!("shyhdl was killed by a signal: {output:?}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Result of driving the debugger with a real GDB.
struct GdbRun {
    /// Exit status of the debugger process.
    status: ExitStatus,
    /// Lines the debugger printed before and during the session.
    session: String,
    /// What GDB printed.
    console: String,
}

/// Start the debugger and run the real `gdb` against it.
///
/// The binary prints its session header (read-only reason, `elf=`, `port=`,
/// `gdb=`) once the session is bound, which is what the test waits for; a
/// missing `gdb` is a hard failure, never a skip.
fn run_with_gdb(args: &[&str]) -> GdbRun {
    run_with_gdb_commands(args, &[])
}

/// The same session, with extra GDB commands after connecting.
///
/// The `read-only` gate needs to attempt a write and then read the state back,
/// which one test does; everything else uses [`run_with_gdb`].
fn run_with_gdb_commands(args: &[&str], extra: &[&str]) -> GdbRun {
    let mut child: Child = Command::new(BIN)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run {BIN}: {error}"));

    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut session = Vec::new();
    let mut port = None;
    let mut gdb_command = None;
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .expect("read the session header")
        > 0
    {
        let text = line.trim_end().to_string();
        if let Some(value) = text.strip_prefix("port=") {
            port = value.parse::<u16>().ok();
        }
        if let Some(value) = text.strip_prefix("gdb=") {
            gdb_command = Some(value.to_string());
        }
        let header_done = text.starts_with("gdb=");
        session.push(text);
        line.clear();
        if header_done {
            break;
        }
    }
    let port = port.unwrap_or_else(|| {
        let _ = child.kill();
        panic!("the debugger never reported a port; session so far: {session:?}")
    });

    // What a user sees before typing anything: the session says it is read-only,
    // and the command it prints turns GDB's own write permissions off, so a
    // `set var` fails with an explicit reason instead of the transport's generic
    // error.
    let header = session.join("\n");
    assert!(
        header.contains("replay=read-only: recorded values cannot be modified"),
        "the session must state that the replay is read-only:\n{header}"
    );
    let gdb_command =
        gdb_command.unwrap_or_else(|| panic!("no gdb= line in the session header:\n{header}"));
    for flag in ["set may-write-memory off", "set may-write-registers off"] {
        assert!(
            gdb_command.contains(flag),
            "the official gdb command must include `{flag}` so a copy-paste user gets an explicit \
             refusal:\n{gdb_command}"
        );
    }
    let remote = gdb_command
        .find("target remote")
        .unwrap_or_else(|| panic!("the official gdb command must connect:\n{gdb_command}"));
    for flag in ["set may-write-memory off", "set may-write-registers off"] {
        let at = gdb_command.find(flag).expect("checked above");
        assert!(
            at < remote,
            "`{flag}` must come before `target remote` in the official command:\n{gdb_command}"
        );
    }

    let elf = session
        .iter()
        .find_map(|line| line.strip_prefix("elf="))
        .unwrap_or_else(|| panic!("no elf= line in {session:?}"))
        .to_string();

    // The test drives GDB the way the session's own printed command tells a user
    // to: the two write switches are set before connecting. Checking the printed
    // text is static evidence; actually running with the switches is what proves
    // the configuration does not break `print` / `step` / `reverse-step` and
    // that a write is refused with an explicit reason.
    let mut commands: Vec<String> = vec![
        "-q".to_string(),
        "-batch".to_string(),
        "-nx".to_string(),
        "-ex".to_string(),
        "set confirm off".to_string(),
        "-ex".to_string(),
        "set pagination off".to_string(),
        "-ex".to_string(),
        "set may-write-memory off".to_string(),
        "-ex".to_string(),
        "set may-write-registers off".to_string(),
        elf.clone(),
        "-ex".to_string(),
        format!("target remote 127.0.0.1:{port}"),
        "-ex".to_string(),
        "printf \"[pc] %p\\n\", $pc".to_string(),
    ];
    for command in extra {
        commands.push("-ex".to_string());
        commands.push((*command).to_string());
    }

    let gdb = Command::new("gdb").args(&commands).output();
    let gdb = match gdb {
        Ok(output) => output,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cannot run gdb ({error}); this test needs a real GDB");
        }
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the debugger") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the debugger did not exit after GDB disconnected; session: {session:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    session.push(rest);

    let console =
        String::from_utf8_lossy(&gdb.stdout).into_owned() + &String::from_utf8_lossy(&gdb.stderr);
    assert!(
        gdb.status.success(),
        "gdb failed against the debugger:\n{console}"
    );
    assert!(
        console.contains("[pc] 0x"),
        "a standard GDB command must read the PC:\n{console}"
    );

    GdbRun {
        status,
        session: session.join("\n"),
        console,
    }
}

#[test]
fn a_manifest_with_a_grid_starts_a_real_gdb_session() {
    let fixture = Fixture::new(
        "grid",
        "build-1",
        "build-1",
        &[],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let manifest = manifest.to_str().unwrap();

    let run = run_with_gdb_commands(
        &["debug", "--manifest", manifest, "--grid-interval", "10ns"],
        &[
            "printf \"[ro0] pc=%p\\n\", $pc",
            // A write through the official configuration: GDB's own write
            // permission is off, so the refusal is explicit and the state must
            // not change.
            "set $pc = $pc + 8",
            "printf \"[ro1] pc=%p\\n\", $pc",
            // The same configuration must leave the standard commands working.
            "step",
            "printf \"[s1] pc=%p\\n\", $pc",
            "reverse-step",
            "printf \"[r1] pc=%p\\n\", $pc",
            "print 1 + 1",
        ],
    );
    assert!(
        run.status.success(),
        "the session must end cleanly after GDB:\n{}",
        run.console
    );
    let session = run.session;
    assert!(
        session.contains("elf="),
        "the ELF path is part of the result:\n{session}"
    );
    assert!(
        session.contains("gdb=gdb "),
        "the GDB command line must be printable:\n{session}"
    );
    assert!(
        session.contains("target remote 127.0.0.1:"),
        "the printed command must contain the connection:\n{session}"
    );

    // The read-only gate: an attempted write is refused with an explicit reason,
    // the recorded state is unchanged, and stepping still works.
    let console = &run.console;
    let refusal = console.to_lowercase();
    assert!(
        refusal.contains("not allowed"),
        "a write attempt must be refused with an explicit reason:\n{console}"
    );
    let pc = |label: &str| pc_from_console(console, label);
    assert_eq!(
        pc("ro0"),
        pc("ro1"),
        "the refused write must not change the state:\n{console}"
    );
    assert_ne!(
        pc("s1"),
        pc("ro0"),
        "`step` must still advance after the refused write:\n{console}"
    );
    assert_eq!(
        pc("r1"),
        pc("ro0"),
        "`reverse-step` must still return:\n{console}"
    );
    assert!(
        console.contains("$1 = 2"),
        "`print` must still work under the read-only configuration:\n{console}"
    );
}

/// Read the `pc=<hex>` a command printed after the `[label]` marker.
fn pc_from_console(console: &str, label: &str) -> u64 {
    let line = console
        .lines()
        .find(|line| line.contains(&format!("[{label}]")))
        .unwrap_or_else(|| panic!("no [{label}] line in GDB output:\n{console}"));
    let hex = line
        .split("pc=0x")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("cannot read the pc from {line:?}"));
    u64::from_str_radix(hex, 16).expect("pc is hexadecimal")
}

#[test]
fn a_single_recorded_clock_is_used_when_no_step_mode_is_given() {
    // No --clock and no --grid-interval: the testcase recorded exactly one clock
    // for this DUT instance, so it decides the reference points (REQ-DBG-021).
    let fixture = Fixture::new(
        "single-clock",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let manifest = manifest.to_str().unwrap();

    let run = run_with_gdb(&["debug", "--manifest", manifest]);
    assert!(
        run.status.success(),
        "the session must end cleanly:\n{}",
        run.console
    );
}

#[test]
fn two_recorded_clocks_for_one_instance_are_ambiguous() {
    // Two candidates for the selected instance: the command must refuse and list
    // them instead of taking the first (REQ-DBG-021).
    let fixture = Fixture::new(
        "two-clocks",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk"), ("dut", "TOP.tb.u_dut.clk2")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let output = run(&["debug", "--manifest", manifest.to_str().unwrap()]);
    assert_eq!(code(&output), REFUSED, "two candidates are ambiguous");
    let text = stderr(&output);
    assert!(
        text.contains("TOP.tb.u_dut.clk") && text.contains("TOP.tb.u_dut.clk2"),
        "both candidates must be listed:\n{text}"
    );
    assert!(stdout(&output).is_empty(), "no session was started");
}

#[test]
fn clock_candidates_of_another_instance_are_not_used() {
    // Two DUT instances, and only `dut2` has a recorded clock. Selecting `dut`
    // must not borrow `dut2`'s clock: that instance has no candidate of its own,
    // so an explicit choice is required.
    let fixture = Fixture::new(
        "other-instance-clock",
        "build-1",
        "build-1",
        &[("dut2", "TOP.tb.u_dut2.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut"), ("dut2", "TOP.tb.u_dut2")],
    );
    let manifest = fixture.manifest_path();
    let output = run(&[
        "debug",
        "--manifest",
        manifest.to_str().unwrap(),
        "--instance",
        "dut",
    ]);
    assert_eq!(code(&output), REFUSED);
    let text = stderr(&output);
    assert!(
        text.contains("--clock") && text.contains("--grid-interval"),
        "the diagnostic must ask for an explicit choice:\n{text}"
    );
    assert!(
        !text.contains("TOP.tb.u_dut2.clk"),
        "another instance's clock must not be offered as a default:\n{text}"
    );
    assert!(stdout(&output).is_empty(), "no session was started");
}

#[test]
fn an_explicit_edge_applies_to_the_recorded_clock() {
    // `--edge` does not need `--clock`: it refines the clock the manifest
    // recorded, so a single candidate plus `--edge negedge` starts a session.
    let fixture = Fixture::new(
        "edge-on-recorded-clock",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let manifest = manifest.to_str().unwrap();

    let run = run_with_gdb(&["debug", "--manifest", manifest, "--edge", "negedge"]);
    assert!(
        run.status.success(),
        "the recorded clock with an explicit edge must start a session:\n{}",
        run.console
    );
}

#[test]
fn an_explicit_clock_wins_over_the_recorded_candidates() {
    // Explicit configuration wins even when the manifest records candidates
    // (REQ-DBG-020).
    let fixture = Fixture::new(
        "explicit-clock",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let manifest = manifest.to_str().unwrap();

    let run = run_with_gdb(&[
        "debug",
        "--manifest",
        manifest,
        "--clock",
        "TOP.tb.u_dut.clk",
        "--edge",
        "negedge",
    ]);
    assert!(
        run.status.success(),
        "an explicit clock must reach the session:\n{}",
        run.console
    );
}

#[test]
fn a_run_without_a_recorded_clock_needs_an_explicit_step_mode() {
    let fixture = Fixture::new(
        "no-clock",
        "build-1",
        "build-1",
        &[],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let output = run(&["debug", "--manifest", manifest.to_str().unwrap()]);
    assert_eq!(code(&output), REFUSED);
    let text = stderr(&output);
    assert!(
        text.contains("--clock") && text.contains("--grid-interval"),
        "the diagnostic must ask for an explicit choice:\n{text}"
    );
    assert!(stdout(&output).is_empty(), "no session was started");
}

#[test]
fn a_build_id_mismatch_is_refused() {
    // The manifest and the metadata are of different builds: the recording and
    // the symbols cannot be paired (REQ-DBG-010).
    let fixture = Fixture::new(
        "mismatch",
        "other-build",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let output = run(&[
        "debug",
        "--manifest",
        manifest.to_str().unwrap(),
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(code(&output), REFUSED, "a mismatch must be refused");
    let text = stderr(&output);
    assert!(
        text.contains("build id mismatch") && text.contains("other-build"),
        "the diagnostic must name both builds:\n{text}"
    );
    assert!(stdout(&output).is_empty(), "no session was started");
}

#[test]
fn a_real_run_records_its_clock_and_debug_uses_it() {
    // The whole contract in one test: a testcase declares its clock with
    // `gen_clk(...)`, `shyhdl test` records it in the manifest, and `debug` with
    // no `--clock`/`--grid-interval` uses exactly that candidate. This needs a
    // real Verilator and a real GDB; a missing tool fails the test.
    Simulator::discover().unwrap_or_else(|error| {
        panic!(
            "a real Verilator is required for this test and none was usable: {error}\n\
             put the tool environment on PATH before running cargo test"
        )
    });

    let dir = TempDir::new("real-run");
    let out = dir.path().join("out");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli/alu.shy.rs");

    let output = run(&[
        "test",
        source.to_str().unwrap(),
        "--top",
        "Adder4",
        "--test",
        "clocked_smoke",
        "--out",
        out.to_str().unwrap(),
        "--wave",
        "vcd",
    ]);
    assert_eq!(
        code(&output),
        0,
        "the fixture test must pass:\n{}",
        stderr(&output)
    );

    let manifest = out.join("run.manifest.json");
    let listed = fs::read_to_string(&manifest).expect("the run must write a manifest");
    assert!(
        listed.contains("TOP.tb_Adder4_clocked_smoke.u_dut.clk"),
        "the testcase clock must be recorded:\n{listed}"
    );

    // No step mode is given: the recorded clock decides.
    let run = run_with_gdb(&["debug", "--manifest", manifest.to_str().unwrap()]);
    assert!(
        run.status.success(),
        "the recorded clock must start a usable session:\n{}",
        run.console
    );
}

#[test]
fn a_recording_overwritten_after_the_manifest_is_refused() {
    // The same path, rewritten by a later run of the same design: the manifest
    // still names the file, but the file is not the recording it was written
    // for, and the content digest is what proves it (REQ-DBG-010).
    let fixture = Fixture::new(
        "overwritten",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::Overwritten,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let output = run(&[
        "debug",
        "--manifest",
        manifest.to_str().unwrap(),
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(
        code(&output),
        REFUSED,
        "an overwritten recording must be refused"
    );
    let text = stderr(&output);
    assert!(
        text.contains("run-fixture-1")
            && (text.contains("content digest") || text.contains("does not match")),
        "the diagnostic must name the run and the identity check:\n{text}"
    );
    assert!(stdout(&output).is_empty(), "no session was started");
}

#[test]
fn a_manifest_pointed_at_another_runs_recording_is_refused() {
    // Another run of the same build and the same design, a different file:
    // pointing this manifest at that recording must not look like a verified
    // pair (AC-MVP-034).
    let fixture = Fixture::new(
        "another-run",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::AnotherRun,
        &[("dut", "TOP.tb.u_dut")],
    );
    let manifest = fixture.manifest_path();
    let output = run(&[
        "debug",
        "--manifest",
        manifest.to_str().unwrap(),
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(
        code(&output),
        REFUSED,
        "another run's recording must be refused"
    );
    let text = stderr(&output);
    assert!(
        text.contains("wave-b.vcd")
            && (text.contains("content digest") || text.contains("does not match")),
        "the diagnostic must name the file and the identity check:\n{text}"
    );
    assert!(stdout(&output).is_empty(), "no session was started");
}

#[test]
fn several_dut_instances_need_an_explicit_choice() {
    let fixture = Fixture::new(
        "two-roots",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk"), ("dut2", "TOP.tb.u_dut2.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut"), ("dut2", "TOP.tb.u_dut2")],
    );
    let manifest = fixture.manifest_path();

    let output = run(&[
        "debug",
        "--manifest",
        manifest.to_str().unwrap(),
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(code(&output), REFUSED);
    let text = stderr(&output);
    assert!(
        text.contains("--instance") && text.contains("dut") && text.contains("dut2"),
        "the diagnostic must list the candidates:\n{text}"
    );

    let output = run(&[
        "debug",
        "--manifest",
        manifest.to_str().unwrap(),
        "--grid-interval",
        "10ns",
        "--instance",
        "nope",
    ]);
    assert_eq!(code(&output), REFUSED);
    assert!(
        stderr(&output).contains("available: dut, dut2"),
        "an unknown instance must list the available ones:\n{}",
        stderr(&output)
    );
}

#[test]
fn external_metadata_mode_needs_an_asserted_build_id() {
    let fixture = Fixture::new(
        "external",
        "build-1",
        "build-1",
        &[("dut", "TOP.tb.u_dut.clk")],
        Recording::Matching,
        &[("dut", "TOP.tb.u_dut")],
    );
    let metadata = fixture.metadata_path();
    let recording = fixture.recording_path();

    // Without the assertion the pairing would be an assumption, so the argument
    // layer rejects it before anything is read.
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--metadata",
        metadata.to_str().unwrap(),
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(code(&output), USAGE);
    assert!(
        stderr(&output).contains("--assume-build-id"),
        "the missing assertion must be named:\n{}",
        stderr(&output)
    );

    // An assertion that does not match the metadata is refused.
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--metadata",
        metadata.to_str().unwrap(),
        "--assume-build-id",
        "some-other-build",
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(code(&output), REFUSED);
    assert!(
        stderr(&output).contains("build id mismatch"),
        "a wrong assertion must be refused:\n{}",
        stderr(&output)
    );
}
