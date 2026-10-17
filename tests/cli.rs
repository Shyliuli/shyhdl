//! Behavioural tests of the `shyhdl` command line: help, argument validation,
//! exit codes and the promise that a command which cannot do its work says so
//! instead of printing a result.
//!
//! These tests drive the real binary and only look at what a user can observe
//! (stdout, stderr, exit status), so they stay valid as the pipeline behind the
//! CLI is filled in. The checks that assert a milestone is *not implemented*
//! are current-state checks: they must be replaced by end-to-end checks for the
//! same command when that milestone lands, never deleted to make a run green.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The binary under test, built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_shyhdl");

/// Not-implemented paths exit with this code (see `--help`).
const NOT_IMPLEMENTED: i32 = 7;
/// Usage errors exit with this code, which is also what clap itself uses.
const USAGE: i32 = 2;

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

/// A file inside a directory that is removed when the test ends.
struct TempSource {
    path: PathBuf,
    _dir: TempDir,
}

impl TempSource {
    /// A readable source file. Its content is intentionally not ShyHDL: until
    /// the frontend is implemented the CLI must not depend on the syntax, and
    /// tests of the frontend belong next to the frontend.
    fn new(name: &str) -> Self {
        let dir = TempDir::new(name);
        let path = dir.path().join("input.shy.rs");
        fs::write(
            &path,
            b"// test fixture; the frontend milestone owns the syntax\n",
        )
        .expect("write fixture");
        Self { path, _dir: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn as_str(&self) -> &str {
        self.path.to_str().expect("fixture path is UTF-8")
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "shyhdl-cli-{}-{}-{}",
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
fn help_lists_every_command_and_the_exit_codes() {
    for flag in ["-h", "--help"] {
        let output = run(&[flag]);
        assert_eq!(code(&output), 0, "{flag} must succeed");
        let text = stdout(&output);
        for command in ["build", "check", "test", "list-tests", "debug"] {
            assert!(
                text.contains(command),
                "{flag} output does not mention `{command}`:\n{text}"
            );
        }
        assert!(
            text.contains("Exit codes:"),
            "{flag} output does not document exit codes:\n{text}"
        );
        assert!(stderr(&output).is_empty(), "{flag} wrote to stderr");
    }
}

#[test]
fn version_reports_the_package_version() {
    let output = run(&["--version"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "unexpected version output: {text}"
    );
}

#[test]
fn help_of_a_subcommand_describes_its_arguments() {
    let output = run(&["test", "--help"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    for expected in ["--top", "--test", "--seed", "--timeout", "--wave"] {
        assert!(
            text.contains(expected),
            "`test --help` does not mention {expected}:\n{text}"
        );
    }
}

#[test]
fn missing_subcommand_is_a_usage_error() {
    let output = run(&[]);
    assert_eq!(code(&output), USAGE);
    assert!(
        stderr(&output).contains("Usage:"),
        "usage error must print usage:\n{}",
        stderr(&output)
    );
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let output = run(&["frobnicate"]);
    assert_eq!(code(&output), USAGE);
    assert!(
        stderr(&output).contains("frobnicate"),
        "the error must name the rejected subcommand:\n{}",
        stderr(&output)
    );
}

#[test]
fn build_requires_source_and_top() {
    let output = run(&["build"]);
    assert_eq!(code(&output), USAGE);
    let text = stderr(&output);
    assert!(
        text.contains("--top"),
        "missing --top not reported:\n{text}"
    );

    let source = TempSource::new("build-no-top");
    let output = run(&["build", source.as_str()]);
    assert_eq!(code(&output), USAGE);
    assert!(
        stderr(&output).contains("--top"),
        "missing --top not reported:\n{}",
        stderr(&output)
    );
}

#[test]
fn top_must_be_a_non_empty_name_without_surrounding_whitespace() {
    let source = TempSource::new("bad-top");
    for bad in ["", " ", " Adder8", "Adder8 "] {
        let output = run(&["check", source.as_str(), "--top", bad]);
        assert_eq!(code(&output), USAGE, "`--top {bad:?}` must be rejected");
        assert!(
            !stderr(&output).is_empty(),
            "rejection of `--top {bad:?}` is not explained"
        );
    }

    // A build-time specialization is a valid module reference and must reach
    // the compiler instead of being rejected by the argument layer.
    for good in ["Adder_8", "Adder<8>"] {
        let output = run(&["check", source.as_str(), "--top", good]);
        assert_ne!(
            code(&output),
            USAGE,
            "`--top {good}` must not be a usage error:\n{}",
            stderr(&output)
        );
    }
}

#[test]
fn a_specialization_is_spelled_in_the_top_name_not_as_a_parameter() {
    let source = TempSource::new("specialized-top");
    // There is deliberately no `--param`: the design's parameters come from the
    // source, and the top is named `Adder<8>` when it is generic.
    let output = run(&["check", source.as_str(), "--top", "top", "--param", "N=8"]);
    assert_eq!(code(&output), USAGE, "`--param` must not exist");
    assert!(
        stderr(&output).contains("--param"),
        "the rejection must name the unknown option:\n{}",
        stderr(&output)
    );
}

#[test]
fn missing_source_file_is_an_io_failure_naming_the_path() {
    let dir = TempDir::new("missing-source");
    let missing = dir.path().join("absent.shy.rs");
    let output = run(&["check", missing.to_str().unwrap(), "--top", "top"]);
    assert_eq!(
        code(&output),
        1,
        "an unreadable path is a failure, not usage"
    );
    let text = stderr(&output);
    assert!(
        text.contains("absent.shy.rs"),
        "error must name the path it failed on:\n{text}"
    );
}

/// Path of a committed fixture under `tests/fixtures/cli/`.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli")
        .join(name)
}

#[test]
fn list_tests_reports_each_test_with_its_source_location() {
    let path = fixture("tests.shy.rs");
    let output = run(&["list-tests", path.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));

    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected output:\n{text}");
    assert!(
        lines[0].contains("tests.shy.rs:7:4")
            && lines[0].ends_with("counts_up timeunit=1ns precision=1ps"),
        "first test line is wrong: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("tests.shy.rs:10:4")
            && lines[1].ends_with("waits_for_reset timeunit=10ns precision=100ps"),
        "second test line is wrong: {}",
        lines[1]
    );
}

#[test]
fn list_tests_without_tests_succeeds_and_keeps_stdout_empty() {
    let source = TempSource::new("no-tests");
    let output = run(&["list-tests", source.as_str()]);
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));
    assert!(
        stdout(&output).is_empty(),
        "a source without tests must print no test line:\n{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("no #[hardware_test] functions"),
        "the empty result must still be explained:\n{}",
        stderr(&output)
    );
}

#[test]
fn list_tests_reports_a_syntax_error_as_a_compile_error() {
    let dir = TempDir::new("broken-syntax");
    let path = dir.path().join("broken.shy.rs");
    fs::write(&path, "fn broken( {}\n").expect("write fixture");

    let output = run(&["list-tests", path.to_str().unwrap()]);
    assert_eq!(
        code(&output),
        3,
        "a syntax error is a compile error, not a generic failure:\n{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("broken.shy.rs"),
        "the diagnostic must name the source:\n{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "nothing was listed");
}

#[test]
fn every_command_reaches_its_own_work_instead_of_a_placeholder() {
    // No command may end in a generic "not implemented" placeholder any more:
    // each one either does its work or fails with a specific diagnostic. The
    // real behaviour of every command is covered by its own end-to-end file
    // (`cli_frontend.rs`, `cli_sim.rs`, `cli_debug.rs`); this check only pins
    // down that the placeholder is gone.
    let source = TempSource::new("no-placeholder");
    let recording = source.path().with_extension("vcd");
    let invocations: Vec<Vec<&str>> = vec![
        vec!["check", source.as_str(), "--top", "top"],
        vec!["build", source.as_str(), "--top", "top"],
        vec!["list-tests", source.as_str()],
        vec!["test", source.as_str(), "--top", "top", "--test", "only"],
        vec!["debug", recording.to_str().unwrap(), "--clock", "clk"],
    ];

    for args in invocations {
        let output = run(&args);
        let text = stderr(&output);
        assert!(
            !text.contains("is not implemented in this build"),
            "`{}` still reports a placeholder:\n{text}",
            args.join(" ")
        );
        assert_ne!(
            code(&output),
            NOT_IMPLEMENTED,
            "`{}` must not fall back to the placeholder:\n{text}",
            args.join(" ")
        );
    }
}

#[test]
fn clock_and_fixed_grid_are_mutually_exclusive() {
    let dir = TempDir::new("step-mode");
    let recording = dir.path().join("recording.vcd");
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--clock",
        "clk",
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(code(&output), USAGE, "both step modes must be rejected");
    assert!(
        stderr(&output).contains("cannot be used with"),
        "the conflict must be explained:\n{}",
        stderr(&output)
    );
}

#[test]
fn instance_selection_requires_the_run_manifest() {
    let dir = TempDir::new("instance-without-manifest");
    let recording = dir.path().join("recording.vcd");
    let output = run(&["debug", recording.to_str().unwrap(), "--instance", "dut"]);
    assert_eq!(
        code(&output),
        USAGE,
        "the instance selector only means something with a manifest"
    );
    assert!(
        stderr(&output).contains("--manifest"),
        "the missing requirement must be named:\n{}",
        stderr(&output)
    );
}

#[test]
fn edge_conflicts_with_the_fixed_grid() {
    let dir = TempDir::new("edge-with-grid");
    let recording = dir.path().join("recording.vcd");
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--edge",
        "negedge",
        "--grid-interval",
        "10ns",
    ]);
    assert_eq!(code(&output), USAGE, "an edge has no meaning on a grid");
    assert!(
        stderr(&output).contains("cannot be used with"),
        "the conflict must be explained:\n{}",
        stderr(&output)
    );

    // `--edge` is not tied to `--clock`: with no explicit clock the step mode
    // comes from the design metadata (REQ-DBG-021), so this only has to pass
    // argument checking. Pairing is given here so the command gets that far.
    let metadata = recording.with_extension("meta.json");
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--metadata",
        metadata.to_str().unwrap(),
        "--assume-build-id",
        "some-build",
        "--edge",
        "negedge",
    ]);
    assert_ne!(
        code(&output),
        USAGE,
        "--edge alone must reach the command:\n{}",
        stderr(&output)
    );
}

#[test]
fn grid_origin_requires_a_grid_interval() {
    let dir = TempDir::new("origin-without-interval");
    let recording = dir.path().join("recording.vcd");
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--grid-origin",
        "10ns",
    ]);
    assert_eq!(code(&output), USAGE);
    assert!(
        stderr(&output).contains("--grid-interval"),
        "the missing requirement must be named:\n{}",
        stderr(&output)
    );
}

#[test]
fn durations_need_an_explicit_integer_unit() {
    let dir = TempDir::new("durations");
    let recording = dir.path().join("recording.vcd");
    for bad in ["10", "1.5ns", "0ns", "-10ns", "10fortnights"] {
        let output = run(&["debug", recording.to_str().unwrap(), "--grid-interval", bad]);
        assert_eq!(code(&output), USAGE, "`{bad}` must be rejected");
    }

    let metadata = recording.with_extension("meta.json");
    let output = run(&[
        "debug",
        recording.to_str().unwrap(),
        "--metadata",
        metadata.to_str().unwrap(),
        "--assume-build-id",
        "some-build",
        "--grid-interval",
        "10ns",
    ]);
    assert_ne!(
        code(&output),
        USAGE,
        "a well-formed duration must reach the command:\n{}",
        stderr(&output)
    );
}

#[test]
fn timeout_must_be_positive() {
    let source = TempSource::new("timeout");
    let output = run(&[
        "test",
        source.as_str(),
        "--top",
        "top",
        "--test",
        "only",
        "--timeout",
        "0",
    ]);
    assert_eq!(code(&output), USAGE, "a zero timeout must be rejected");
}

#[test]
fn unknown_wave_format_is_a_usage_error() {
    let source = TempSource::new("wave-format");
    let output = run(&[
        "test",
        source.as_str(),
        "--top",
        "top",
        "--test",
        "only",
        "--wave",
        "fst",
    ]);
    assert_eq!(
        code(&output),
        USAGE,
        "only the documented waveform choices are accepted"
    );
}
