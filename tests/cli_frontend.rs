//! End-to-end tests of the commands that really compile ShyHDL source:
//! `check` and `build`.
//!
//! These drive the real binary over the committed `examples/*.shy.rs` sources
//! and inspect only what a user sees: exit code, stdout, stderr and the files
//! that were written. The generated metadata is parsed back with the library's
//! own reader, so "it wrote a file" never stands in for "the file is usable".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use shyhdl::metadata::{DesignMetadata, SCHEMA_VERSION};

/// The binary under test, built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_shyhdl");

/// A source that cannot compile, and therefore must be a compile error.
const COMPILE_ERROR: i32 = 3;
/// A source that compiles: the command must finish successfully.
const SUCCESS: i32 = 0;

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

/// A committed example design.
fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

fn example_str(name: &str) -> String {
    example(name).to_string_lossy().into_owned()
}

/// A directory that is removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "shyhdl-cli-frontend-{}-{}-{}",
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
fn check_compiles_the_adder_example() {
    let source = example_str("adder.shy.rs");
    let output = run(&["check", &source, "--top", "Adder8"]);
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("Adder8") && text.contains("2 modules"),
        "check must report what it compiled:\n{text}"
    );
    assert!(
        stderr(&output).is_empty(),
        "a clean example must not warn:\n{}",
        stderr(&output)
    );
}

#[test]
fn build_writes_readable_verilog_and_metadata_the_debugger_can_read() {
    let dir = TempDir::new("build");
    let out = dir.path().join("artifacts");
    let source = example_str("adder.shy.rs");
    let output = run(&[
        "build",
        &source,
        "--top",
        "Adder8",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));

    // The result of `build` is exactly the two artifact paths.
    let listing = stdout(&output);
    let reported: Vec<&str> = listing.lines().collect();
    assert_eq!(reported.len(), 2, "unexpected build output:\n{listing}");
    assert!(reported[0].ends_with("Adder8.v"), "{}", reported[0]);
    assert!(reported[1].ends_with("Adder8.meta.json"), "{}", reported[1]);

    let verilog = fs::read_to_string(out.join("Adder8.v")).expect("read generated verilog");
    for expected in [
        "module Adder_N8 (",
        "module Adder8 (",
        "endmodule",
        // Readability (REQ-VER-005/006): the generated file keeps source
        // locations so a reader can map RTL back to ShyHDL.
        "examples/adder.shy.rs:",
    ] {
        assert!(
            verilog.contains(expected),
            "generated Verilog is missing `{expected}`:\n{verilog}"
        );
    }

    // The metadata is only useful if the library can load it again.
    let json = fs::read_to_string(out.join("Adder8.meta.json")).expect("read metadata");
    let metadata = DesignMetadata::from_json_str(&json).expect("metadata must round-trip");
    assert_eq!(metadata.schema_version, SCHEMA_VERSION);
    assert_eq!(metadata.top, "Adder8");
    assert!(!metadata.build_id.is_empty(), "build id must be recorded");
    assert!(
        metadata
            .variables
            .iter()
            .any(|variable| variable.rtl_name == "sum"),
        "top ports must be debug-visible variables"
    );
}

#[test]
fn build_output_directory_may_have_a_non_ascii_path() {
    // The CLI writes artifacts itself, so a non-ASCII path must work. (The
    // Verilator kit is a separate, documented upstream limitation.)
    let dir = TempDir::new("non-ascii");
    let out = dir.path().join("构建输出");
    let source = example_str("adder.shy.rs");
    let output = run(&[
        "build",
        &source,
        "--top",
        "Adder8",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));
    assert!(out.join("Adder8.v").is_file(), "verilog was not written");
    assert!(
        out.join("Adder8.meta.json").is_file(),
        "metadata was not written"
    );
}

#[test]
fn compile_errors_exit_with_3_and_point_at_the_source() {
    let dir = TempDir::new("compile-errors");

    // A field the module does not have: the diagnostic carries file:line:col.
    let broken = dir.path().join("broken.shy.rs");
    fs::write(
        &broken,
        "#[hardware_instance]\nstruct Broken {\n    #[In] a: uint<8>,\n    #[Out] sum: uint<8>,\n}\n\nimpl Hardware for Broken {\n    fn run(&mut self) {\n        self.sum = self.missing;\n    }\n}\n",
    )
    .expect("write fixture");
    let output = run(&["check", broken.to_str().unwrap(), "--top", "Broken"]);
    assert_eq!(code(&output), COMPILE_ERROR);
    let text = stderr(&output);
    assert!(
        text.contains("broken.shy.rs:9:20"),
        "the diagnostic must point at the offending token:\n{text}"
    );
    assert!(
        stdout(&output).is_empty(),
        "a failed check prints no result"
    );

    // A top that does not exist.
    let adder = example_str("adder.shy.rs");
    let output = run(&["check", &adder, "--top", "Nope"]);
    assert_eq!(code(&output), COMPILE_ERROR);
    assert!(
        stderr(&output).contains("Nope"),
        "the diagnostic must name the missing top:\n{}",
        stderr(&output)
    );

    // Source that is not even valid Rust syntax.
    let syntax = dir.path().join("syntax.shy.rs");
    fs::write(&syntax, "fn oops( {}\n").expect("write fixture");
    let output = run(&["check", syntax.to_str().unwrap(), "--top", "X"]);
    assert_eq!(code(&output), COMPILE_ERROR);
    assert!(
        stderr(&output).contains("syntax.shy.rs:1:"),
        "a syntax error must point at a location:\n{}",
        stderr(&output)
    );
}

#[test]
fn test_rejects_the_design_before_running_anything() {
    // Both cases fail before any simulator is needed: a design that does not
    // compile, and a test name that the source does not declare.
    let dir = TempDir::new("test-rejects");
    let broken = dir.path().join("broken.shy.rs");
    fs::write(&broken, "fn oops( {}\n").expect("write fixture");
    let out = dir.path().join("artifacts");

    let output = run(&[
        "test",
        broken.to_str().unwrap(),
        "--top",
        "Broken",
        "--test",
        "anything",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), COMPILE_ERROR);
    assert!(stdout(&output).is_empty(), "nothing may be reported as run");

    let adder = example_str("adder.shy.rs");
    let output = run(&[
        "test",
        &adder,
        "--top",
        "Adder8",
        "--test",
        "not_a_test",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(
        code(&output),
        COMPILE_ERROR,
        "an unknown test is a source-level error:\n{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("not_a_test"),
        "the diagnostic must name the unknown test:\n{}",
        stderr(&output)
    );
}

#[test]
fn build_writes_nothing_when_the_source_does_not_compile() {
    let dir = TempDir::new("no-artifacts");
    let source = dir.path().join("broken.shy.rs");
    fs::write(&source, "fn oops( {}\n").expect("write fixture");
    let out = dir.path().join("artifacts");

    let output = run(&[
        "build",
        source.to_str().unwrap(),
        "--top",
        "Broken",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), COMPILE_ERROR);
    assert!(
        stdout(&output).is_empty(),
        "a failed build lists no artifacts"
    );
    let written: Vec<_> = fs::read_dir(&out)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(
        written.is_empty(),
        "a failed build must not leave artifacts behind: {written:?}"
    );
}

/// A source that uses `match` as a value. The expression form is a supported
/// construct now, so `build` has to compile it and write both artifacts; the
/// behaviour of the generated logic is covered by the frontend/testbench tests,
/// not by this build-level check.
const VALUE_MATCH: &str = r#"
#[hardware_instance]
struct MatchValue {
    #[In] sel: uint<2>,
    #[Out] y: uint<8>,
}

impl Hardware for MatchValue {
    fn run(&mut self) {
        self.y = match self.sel { 0 => 1u8, _ => 2u8 };
    }
}
"#;

/// The same supported construct next to a genuine error (a field this module
/// does not have), which must stay a compile error and must not be reported as
/// a missing capability.
const VALUE_MATCH_WITH_REAL_ERROR: &str = r#"
#[hardware_instance]
struct MatchValue {
    #[In] sel: uint<2>,
    #[Out] y: uint<8>,
}

impl Hardware for MatchValue {
    fn run(&mut self) {
        self.y = match self.sel { 0 => 1u8, _ => 2u8 };
        self.nope = 0;
    }
}
"#;

#[test]
fn a_value_position_match_compiles_and_writes_artifacts() {
    let dir = TempDir::new("value-match");
    let source = dir.path().join("value_match.shy.rs");
    fs::write(&source, VALUE_MATCH).expect("write fixture");
    let out = dir.path().join("artifacts");

    let output = run(&[
        "build",
        source.to_str().unwrap(),
        "--top",
        "MatchValue",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(
        code(&output),
        SUCCESS,
        "a supported value-position match must build:\n{}",
        stderr(&output)
    );
    let printed = stdout(&output);
    let listed: Vec<&str> = printed.lines().collect();
    assert_eq!(
        listed.len(),
        2,
        "a build prints its two artifacts:\n{listed:?}"
    );
    let names: Vec<String> = fs::read_dir(&out)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        names.iter().any(|name| name.ends_with(".v")),
        "the build must write the Verilog: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.ends_with(".meta.json")),
        "the build must write the metadata: {names:?}"
    );
    assert!(
        stderr(&output).is_empty(),
        "a clean build must not report diagnostics:\n{}",
        stderr(&output)
    );
}

#[test]
fn a_value_position_match_does_not_hide_a_real_error() {
    let dir = TempDir::new("value-match-plus-error");
    let source = dir.path().join("mixed.shy.rs");
    fs::write(&source, VALUE_MATCH_WITH_REAL_ERROR).expect("write fixture");

    let output = run(&["check", source.to_str().unwrap(), "--top", "MatchValue"]);
    assert_eq!(
        code(&output),
        COMPILE_ERROR,
        "a real error must stay a compile error:\n{}",
        stderr(&output)
    );
    let text = stderr(&output);
    assert!(
        text.contains("SH-F003"),
        "the real error must be reported:\n{text}"
    );
    assert!(
        !text.contains("SH-F024"),
        "a supported construct must not be reported as a missing capability:\n{text}"
    );
    assert!(
        stdout(&output).is_empty(),
        "a failed check prints no result"
    );
}
