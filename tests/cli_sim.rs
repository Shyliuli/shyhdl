//! End-to-end tests of `shyhdl test`: real ShyHDL source, real generated
//! testbench, real Verilator, real manifest.
//!
//! These tests need Verilator on `PATH` (or `$SHYHDL_VERILATOR`). A missing
//! tool fails the test with a clear message; nothing is skipped and no result
//! is faked. The case that *must* work without a tool is the environment
//! diagnostic, and it is tested by removing the tools from `PATH`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use shyhdl::metadata::DesignMetadata;
use shyhdl::simulation::{load_manifest, Simulator};

/// The binary under test, built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_shyhdl");

/// Exit code for a test that failed, and for an unusable environment.
const TEST_FAILED: i32 = 1;
const ENVIRONMENT: i32 = 6;
/// Exit code for a run that exceeded its deadline.
const TIMEOUT: i32 = 5;

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

/// Verilator is a hard requirement for the runs in this file.
fn require_verilator() {
    Simulator::discover().unwrap_or_else(|error| {
        panic!(
            "a real Verilator is required for these tests and none was usable: {error}\n\
             put the tool environment on PATH (for example by sourcing the kit's env.sh) \
             before running cargo test"
        )
    });
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli")
        .join(name)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "shyhdl-cli-sim-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            label
        );
        let path = std::env::temp_dir().join(unique);
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

#[test]
fn a_passing_test_exits_zero_and_pairs_the_recording_with_its_design() {
    require_verilator();
    let dir = TempDir::new("pass");
    let out = dir.path().join("out");
    let source = fixture("alu.shy.rs");
    let output = run(&[
        "test",
        source.to_str().unwrap(),
        "--top",
        "Adder4",
        "--test",
        "wraps_and_carries",
        "--out",
        out.to_str().unwrap(),
        "--wave",
        "vcd",
    ]);
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));

    let text = stdout(&output);
    assert!(
        text.starts_with("pass test=wraps_and_carries"),
        "the summary must report the outcome first:\n{text}"
    );
    assert!(text.contains("fail=0"), "no assertion may fail:\n{text}");
    assert!(
        text.contains("manifest="),
        "the manifest path is part of the result:\n{text}"
    );

    // The manifest is the pairing evidence: design build id, metadata file and
    // the exact recording root of the DUT instance.
    let manifest = load_manifest(&out.join("run.manifest.json")).expect("manifest must load");
    assert_eq!(manifest.outcome, "pass");
    assert_eq!(manifest.design.top, "Adder4");
    assert_eq!(manifest.test.name, "wraps_and_carries");
    assert_eq!(manifest.counts.fails, 0);

    let json = fs::read_to_string(out.join("Adder4.meta.json")).expect("metadata file");
    let metadata = DesignMetadata::from_json_str(&json).expect("metadata must round-trip");
    assert_eq!(
        manifest.design.build_id.as_deref(),
        Some(metadata.build_id.as_str()),
        "the manifest must carry the frontend's build id unchanged"
    );
    assert_eq!(
        manifest.paths.metadata.as_deref(),
        Some("Adder4.meta.json"),
        "the manifest must name the metadata it was paired with"
    );

    assert_eq!(manifest.roots.len(), 1, "one DUT instance was instantiated");
    let root = &manifest.roots[0];
    assert_eq!(root.source_root, "Adder4");
    assert_eq!(root.var, "dut");
    assert_eq!(root.instance, "u_dut");
    assert!(
        root.wave_root.starts_with("TOP.") && root.wave_root.ends_with(".u_dut"),
        "the recording root must be the exact scope: {}",
        root.wave_root
    );

    let wave = manifest.wave.as_ref().expect("a VCD was requested");
    assert_eq!(wave.format, "vcd");
    assert!(
        out.join(&wave.path).is_file(),
        "the recorded waveform must exist"
    );
    assert!(
        wave.timescale_in_file.is_some(),
        "the recorded timescale must be read back, not assumed"
    );
}

#[test]
fn a_failing_assertion_exits_one_and_is_not_a_pass() {
    require_verilator();
    let dir = TempDir::new("fail");
    let out = dir.path().join("out");
    let source = fixture("alu.shy.rs");
    let output = run(&[
        "test",
        source.to_str().unwrap(),
        "--top",
        "Adder4",
        "--test",
        "fails_on_purpose",
        "--out",
        out.to_str().unwrap(),
        "--wave",
        "none",
    ]);
    assert_eq!(
        code(&output),
        TEST_FAILED,
        "an assertion failure is exit 1 even though the simulator exited 0:\n{}",
        stdout(&output)
    );

    let text = stdout(&output);
    assert!(
        text.starts_with("fail test=fails_on_purpose"),
        "the summary must say the test failed:\n{text}"
    );
    assert!(
        stderr(&output).contains("SHYHDL_FAIL"),
        "the failing check must be shown:\n{}",
        stderr(&output)
    );

    let manifest = load_manifest(&out.join("run.manifest.json")).expect("manifest must load");
    assert_eq!(manifest.outcome, "fail");
    assert_eq!(manifest.counts.fails, 1);
    assert!(
        manifest.wave.is_none(),
        "`--wave none` must not claim a recording"
    );
}

#[test]
fn a_test_that_never_finishes_is_a_timeout_not_a_failure() {
    require_verilator();
    let dir = TempDir::new("timeout");
    let out = dir.path().join("out");
    let source = fixture("alu.shy.rs");
    let output = run(&[
        "test",
        source.to_str().unwrap(),
        "--top",
        "Adder4",
        "--test",
        "never_finishes",
        "--out",
        out.to_str().unwrap(),
        "--timeout",
        "3",
    ]);
    assert_eq!(
        code(&output),
        TIMEOUT,
        "a deadline is a timeout, not a test failure:\n{}{}",
        stdout(&output),
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.starts_with("timeout test=never_finishes"),
        "the summary must report the timeout:\n{text}"
    );

    let manifest = load_manifest(&out.join("run.manifest.json")).expect("manifest must load");
    assert_eq!(manifest.outcome, "timeout");
    assert_eq!(
        manifest.counts.passes, 0,
        "a run that timed out must not report a pass"
    );
}

#[test]
fn a_missing_verilator_is_an_environment_error_not_a_test_failure() {
    let dir = TempDir::new("no-tools");
    // The right shape of `PATH`, with nothing in it: discovery must fail, and
    // the failure must be classified as the environment's, not the test's.
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).expect("create empty path dir");
    let out = dir.path().join("out");
    let source = fixture("alu.shy.rs");

    let output = Command::new(BIN)
        .args([
            "test",
            source.to_str().unwrap(),
            "--top",
            "Adder4",
            "--test",
            "wraps_and_carries",
            "--out",
            out.to_str().unwrap(),
        ])
        .env("PATH", &empty)
        .env_remove("SHYHDL_VERILATOR")
        .env_remove("SHYHDL_TIMEOUT")
        .output()
        .expect("run shyhdl");

    assert_eq!(
        code(&output),
        ENVIRONMENT,
        "a missing tool must not look like a failing test:\n{}{}",
        stdout(&output),
        stderr(&output)
    );
    let text = stderr(&output);
    assert!(
        text.contains("verilator"),
        "the diagnostic must name the missing tool:\n{text}"
    );
    assert!(
        stdout(&output).is_empty(),
        "no result may be printed for a run that never happened:\n{}",
        stdout(&output)
    );
}
