//! Command-line entry point of the `shyhdl` binary.
//!
//! Two rules shape this file:
//!
//! * Every command either does its work or fails with a diagnostic and a
//!   non-zero exit code. Nothing is printed that was not produced, and a
//!   command whose backing milestone is not implemented reports exactly that
//!   (REQ-VER-007).
//! * Argument errors are caught by `clap` before any work starts, so mutually
//!   exclusive step modes and malformed parameters never reach the pipeline.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

use shyhdl::debugger::{DebugError, DebugOptions, DebugServer, RootMapping};
use shyhdl::error::{Error, Result};
use shyhdl::frontend::{self, CompiledDesign, PortDir};
use shyhdl::metadata::DesignMetadata;
use shyhdl::replay::{Edge, ReplayEngine, StepMode};
use shyhdl::simulation::{self, Outcome, SimOptions, Simulator, SourceFile, WaveOption};
use shyhdl::testbench::{self, DesignInfo, ModulePort, PortDirection};
use shyhdl::waveform::{Timescale, WaveError, Waveform};

/// Process exit codes. Stable enough to script against, and printed by
/// `--help` so they are not folklore.
///
/// Commands in this build can only reach a subset of them: the rest are used as
/// each milestone is wired in, which is why the whole table lives here.
#[allow(dead_code)]
mod exit {
    /// The command finished; a test run passed.
    pub const OK: u8 = 0;
    /// A test failed, or a general failure with no more specific code: an
    /// assertion failed, the run ended without a PASS marker (REQ-TEST-008),
    /// or a file could not be read.
    pub const FAILED: u8 = 1;
    /// Usage error: bad arguments, unknown command, mutually exclusive
    /// options. `clap` exits with this code on its own.
    pub const USAGE: u8 = 2;
    /// The source did not compile; diagnostics carry the source location.
    pub const COMPILE_ERROR: u8 = 3;
    /// The generated Verilog failed to build, or the simulator reported an
    /// error while running it.
    pub const SIMULATION_ERROR: u8 = 4;
    /// The simulation exceeded its timeout.
    pub const TIMEOUT: u8 = 5;
    /// A required external tool is missing or unusable (REQ-TEST-009).
    pub const ENVIRONMENT: u8 = 6;
    /// The requested capability is not implemented in this build.
    pub const NOT_IMPLEMENTED: u8 = 7;
    /// Refused: build id or metadata schema mismatch, or a write on a
    /// read-only replay (REQ-DBG-010, REQ-DBG-011).
    pub const REFUSED: u8 = 8;
}

const EXIT_CODE_HELP: &str = "\
Exit codes:
  0  success: the command finished; a test run passed
  1  a test failed, or a general failure with no more specific code
  2  usage error: bad arguments, unknown command, mutually exclusive options
  3  the source did not compile; diagnostics carry the source location
  4  the generated Verilog failed to build, or the simulation reported an error
  5  the simulation exceeded its timeout
  6  a required external tool is missing or unusable
  7  the requested capability is not implemented in this build
  8  refused: build id or metadata schema mismatch, or a write on a read-only replay";

/// Default artifact directory. Generated Verilog, metadata and waveforms are
/// build results, never sources (REQ-VER-008).
const DEFAULT_OUT_DIR: &str = "build";

/// Default simulation timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// ShyHDL compiler driver, testbench runner and recording debugger.
#[derive(Debug, Parser)]
#[command(
    name = "shyhdl",
    version,
    about = "Compile ShyHDL source, run generated testbenches and replay recordings",
    after_help = EXIT_CODE_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile a source file into Verilog and design metadata.
    Build(BuildArgs),
    /// Compile a source file and report diagnostics, writing no artifacts.
    Check(CheckArgs),
    /// Compile, generate a testbench for one test and run it.
    Test(TestArgs),
    /// List the tests declared in a source file.
    #[command(name = "list-tests")]
    ListTests(ListTestsArgs),
    /// Replay a recording under GDB.
    Debug(DebugArgs),
    /// Put a design on the NVBoard virtual board (needs the `nvboard` feature).
    Board(BoardArgs),
}

/// Arguments shared by every command that compiles a design.
#[derive(Debug, Args)]
struct CompileArgs {
    /// ShyHDL source file.
    #[arg(value_name = "SOURCE")]
    source: PathBuf,

    /// Name of the top-level module to compile, for example `Adder8` or the
    /// build-time specialization `Adder<8>`.
    ///
    /// A specialization is part of the name, so a parameterized top is built
    /// with `--top 'Adder<8>'`; there is no separate `--param` option, because
    /// the design's parameters come from the source and the frontend folds them
    /// into the Verilog name and the build id.
    #[arg(long, value_name = "NAME", value_parser = parse_name)]
    top: String,
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[command(flatten)]
    compile: CompileArgs,

    /// Directory for the generated Verilog and metadata.
    #[arg(long, value_name = "DIR", default_value = DEFAULT_OUT_DIR)]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[command(flatten)]
    compile: CompileArgs,
}

#[derive(Debug, Args)]
struct TestArgs {
    #[command(flatten)]
    compile: CompileArgs,

    /// Name of the test function to generate and run.
    #[arg(long, value_name = "NAME", value_parser = parse_name)]
    test: String,

    /// Directory for the generated Verilog, testbench and run manifest.
    #[arg(long, value_name = "DIR", default_value = DEFAULT_OUT_DIR)]
    out: PathBuf,

    /// Random seed reported in the run manifest, so a run can be reproduced
    /// (REQ-SIM-009).
    #[arg(long, value_name = "SEED", default_value_t = 0)]
    seed: u64,

    /// Simulation timeout in seconds; exceeding it is reported as a timeout,
    /// not as a failure (REQ-TEST-024).
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = DEFAULT_TIMEOUT_SECS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    timeout: u64,

    /// Waveform to record, or `none` to record nothing (REQ-SIM-004).
    #[arg(long, value_name = "FORMAT", value_enum, default_value_t = WaveArg::Vcd)]
    wave: WaveArg,
}

#[derive(Debug, Args)]
struct BoardArgs {
    #[command(flatten)]
    compile: CompileArgs,

    /// Directory for the board model and its generated runner.
    #[arg(long, value_name = "DIR", default_value = DEFAULT_OUT_DIR)]
    out: PathBuf,

    /// Input port to toggle as the clock. Without it a width-1 input named
    /// `clk`, `clock`, `clk_i` or `clock_i` is used; a design with neither is
    /// simply combinational.
    #[arg(long, value_name = "NAME", value_parser = parse_name)]
    clock: Option<String>,
}

#[derive(Debug, Args)]
struct ListTestsArgs {
    /// ShyHDL source file.
    #[arg(value_name = "SOURCE")]
    source: PathBuf,
}

/// Step-mode options of `debug`.
///
/// Explicit configuration wins: `--clock` names the clock, `--grid-interval`
/// selects the fixed grid, and the two are mutually exclusive (REQ-DBG-020,
/// REQ-DBG-022). With neither given, the run manifest decides: the testcase that
/// produced the recording declares its clocks, and the one candidate belonging
/// to the selected DUT instance is used. No candidate, several candidates, or a
/// candidate of another instance are reported rather than guessed
/// (REQ-DBG-021). The design itself carries no clock marking, so nothing is
/// inferred from the source.
#[derive(Debug, Args)]
struct DebugArgs {
    /// Recorded waveform (VCD or FST) to replay. Optional when `--manifest`
    /// already names the recording of its run.
    #[arg(value_name = "RECORDING", required_unless_present = "manifest")]
    recording: Option<PathBuf>,

    /// Run manifest written by `shyhdl test`. This is the verified pairing:
    /// the manifest names the design build, the metadata file and the exact
    /// recording root of every DUT instance.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "metadata",
        required_unless_present = "metadata"
    )]
    manifest: Option<PathBuf>,

    /// Design metadata (`*.meta.json`) for an external fixture whose recording
    /// was not produced by `shyhdl test` (REQ-DBG-010).
    ///
    /// A recording carries no build identity of its own, so this mode also
    /// needs `--assume-build-id`: without it there is nothing to check the
    /// metadata against, and the pairing would be an assumption rather than a
    /// verification.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "manifest",
        requires = "assume_build_id",
        required_unless_present = "manifest"
    )]
    metadata: Option<PathBuf>,

    /// Build id this external recording is asserted to come from. Checked
    /// against the metadata's `build_id`; a different id is refused.
    #[arg(long, value_name = "ID", requires = "metadata")]
    assume_build_id: Option<String>,

    /// Recording path of the design root in an external recording, for example
    /// `TOP.tb_adder.dut`. Defaults to the metadata's own root (no testbench
    /// wrapper).
    #[arg(long, value_name = "PATH", requires = "metadata")]
    wave_root: Option<String>,

    /// Testbench variable of the design-under-test instance to debug, for
    /// example `dut` in `let mut dut = Adder8::new()`.
    ///
    /// A testbench may instantiate the DUT several times with distinct
    /// recording roots. One root is selected automatically; with several, this
    /// option picks one, and running without it is an error that lists the
    /// candidates instead of guessing by name suffix.
    #[arg(long, value_name = "TBVAR", requires = "manifest")]
    instance: Option<String>,

    /// Step on the edges of this clock signal, named by its path in the
    /// recording (REQ-DBG-023). Without it, the single clock the testcase
    /// recorded for the selected DUT instance is used; no candidate needs an
    /// explicit `--clock` or `--grid-interval`.
    #[arg(long, value_name = "SIGNAL", conflicts_with = "grid_interval")]
    clock: Option<String>,

    // Left optional rather than defaulted so that `--edge` alone neither counts
    // as a clock selection nor needs one: it refines whichever clock mode is in
    // effect, including a clock the manifest recorded.
    /// Which clock edge is a reference point; defaults to `posedge`
    /// (REQ-DBG-023). Only valid in clock mode, so it conflicts with the fixed
    /// grid.
    #[arg(
        long,
        value_name = "EDGE",
        value_enum,
        conflicts_with_all = ["grid_interval", "grid_origin"]
    )]
    edge: Option<EdgeArg>,

    /// Step on the fixed grid `origin + n * interval`, for example `--grid-interval 10ns`
    /// (REQ-DBG-025).
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    grid_interval: Option<Timescale>,

    /// First grid point; defaults to time zero of the recording.
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = parse_duration,
        requires = "grid_interval"
    )]
    grid_origin: Option<Timescale>,
}

/// Waveform recording choice of `test`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WaveArg {
    /// Record a VCD waveform.
    Vcd,
    /// Record nothing.
    None,
}

/// Clock edge choice of `debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EdgeArg {
    /// Rising edge.
    Posedge,
    /// Falling edge.
    Negedge,
    /// Both edges.
    Both,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("shyhdl: {error}");
            ExitCode::from(exit_code_for(&error))
        }
    }
}

/// Dispatch one command. `Ok` carries the exit code of a command that ran to
/// completion, `Err` a failure that still needs mapping to a code.
fn run(cli: &Cli) -> Result<u8> {
    match &cli.command {
        Command::Build(args) => run_build(args),
        Command::Check(args) => run_check(args),
        Command::Test(args) => run_test(args),
        Command::ListTests(args) => run_list_tests(args),
        Command::Debug(args) => run_debug(args),
        Command::Board(args) => run_board(args),
    }
}

/// Exit code of a failure. An unclassified failure is `1`, never success.
fn exit_code_for(error: &Error) -> u8 {
    match error {
        Error::NotImplemented { .. } => exit::NOT_IMPLEMENTED,
        Error::BuildIdMismatch { .. }
        | Error::SchemaVersion { .. }
        | Error::InvalidMetadata { .. } => exit::REFUSED,
        Error::Parse { .. }
        | Error::Unsupported { .. }
        | Error::TypeLayout { .. }
        | Error::MissingSourceLocation { .. } => exit::COMPILE_ERROR,
        Error::Io { .. } | Error::Json { .. } => exit::FAILED,
        // The board backend's own failures: a missing SDL, a failed compile.
        Error::Board { .. } => exit::ENVIRONMENT,
    }
}

// ---------------------------------------------------------------------------
// Commands
//
// Each command reads its source file first, so an unreadable path is an I/O
// failure with that path rather than a milestone gap. The pipeline behind the
// read is wired in as its module lands; until then the command fails with the
// milestone that owns it instead of pretending to have compiled anything.
// ---------------------------------------------------------------------------

fn run_build(args: &BuildArgs) -> Result<u8> {
    let source = read_text(&args.compile.source)?;
    let source_name = source_name(&args.compile.source);
    let compiled = match compile_source(&source, &source_name, &args.compile.top) {
        Ok(compiled) => compiled,
        Err(code) => return Ok(code),
    };

    let out = &args.out;
    fs::create_dir_all(out).map_err(|source| Error::Io {
        path: out.clone(),
        source,
    })?;
    let top = compiled.top().verilog_name.clone();
    let verilog_path = out.join(format!("{top}.v"));
    let metadata_path = out.join(format!("{top}.meta.json"));
    write_file(&verilog_path, &compiled.verilog)?;
    write_file(&metadata_path, &compiled.metadata.to_json_string()?)?;

    // The artifact paths are the command's result, so they go to stdout and
    // nothing else does.
    println!("{}", verilog_path.display());
    println!("{}", metadata_path.display());
    Ok(exit::OK)
}

fn run_check(args: &CheckArgs) -> Result<u8> {
    let source = read_text(&args.compile.source)?;
    let source_name = source_name(&args.compile.source);
    let compiled = match compile_source(&source, &source_name, &args.compile.top) {
        Ok(compiled) => compiled,
        Err(code) => return Ok(code),
    };
    println!(
        "{source_name}: ok, top {} ({} modules)",
        compiled.top().verilog_name,
        compiled.design.modules().len()
    );
    Ok(exit::OK)
}

fn run_test(args: &TestArgs) -> Result<u8> {
    let source = read_text(&args.compile.source)?;
    let source_name = source_name(&args.compile.source);
    let compiled = match compile_source(&source, &source_name, &args.compile.top) {
        Ok(compiled) => compiled,
        Err(code) => return Ok(code),
    };
    let design = design_info(&compiled);

    let artifact = match testbench::compile_testbench(&source, &source_name, &args.test, &design) {
        Ok(artifact) => artifact,
        Err(error) => {
            // A test that does not fit the supported subset is a source-level
            // failure, reported with its location.
            eprintln!("shyhdl: {error}");
            return Ok(exit_code_for(&error));
        }
    };

    let simulator = match Simulator::discover() {
        Ok(simulator) => simulator,
        Err(error) => {
            eprintln!("shyhdl: {error}");
            return Ok(match error {
                Error::NotImplemented { .. } => exit::NOT_IMPLEMENTED,
                _ => exit::ENVIRONMENT,
            });
        }
    };

    // The run manifest pairs the recording with the design build and its
    // metadata file, so both have to exist before the run starts.
    let top = compiled.design.top().verilog_name.clone();
    let metadata_name = format!("{top}.meta.json");
    let out = &args.out;
    fs::create_dir_all(out).map_err(|source| Error::Io {
        path: out.clone(),
        source,
    })?;
    write_file(
        &out.join(&metadata_name),
        &compiled.metadata.to_json_string()?,
    )?;

    let mut options = SimOptions::for_testbench(
        out.clone(),
        &artifact,
        &design,
        vec![SourceFile::new(
            format!("{top}.v"),
            compiled.verilog.clone(),
        )],
    );
    options.design_build_id = Some(compiled.metadata.build_id.clone());
    options.metadata_file = Some(metadata_name);
    options.timeout = Duration::from_secs(args.timeout);
    options.seed = Some(args.seed);
    options.wave = match args.wave {
        WaveArg::Vcd => WaveOption::vcd(),
        WaveArg::None => WaveOption::None,
    };

    let report = simulator.run(&options)?;

    println!("{}", report.summary_line());
    if let Some(wave) = &report.wave_path {
        println!("wave={}", wave.display());
    }
    println!("manifest={}", report.manifest_path.display());
    if !report.is_pass() {
        // Failing lines and the simulator's own output only when something
        // went wrong: a passing run stays one summary line plus its artifacts.
        for line in &report.fail_lines {
            eprintln!("{line}");
        }
        eprint!("{}", report.stdout);
        eprint!("{}", report.stderr);
    }
    Ok(outcome_exit_code(report.outcome))
}

/// Build the board model and hand the window to the user.
///
/// The board keeps the process until its window is closed, so this command is
/// interactive by nature: switches drive the design's inputs, LEDs show its
/// outputs, and the pin assignment is printed before the window opens.
#[cfg(feature = "nvboard")]
fn run_board(args: &BoardArgs) -> Result<u8> {
    let source = read_text(&args.compile.source)?;
    let source_name = source_name(&args.compile.source);
    let compiled = match compile_source(&source, &source_name, &args.compile.top) {
        Ok(compiled) => compiled,
        Err(code) => return Ok(code),
    };

    let top = compiled.design.top();
    let ports = top
        .ports
        .iter()
        .map(|port| {
            Ok(shyhdl::nvboard::BoardPort::new(
                port.verilog_name.clone(),
                port.ty.bit_width()?,
                matches!(port.dir, PortDir::In),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let run = shyhdl::nvboard::run(&shyhdl::nvboard::BoardOptions {
        build_dir: args.out.clone(),
        top: top.verilog_name.clone(),
        verilog: compiled.verilog.clone(),
        ports,
        clock: args.clock.clone(),
        announce: true,
    })?;
    for binding in &run.bindings {
        println!("{binding}");
    }
    Ok(exit::OK)
}

/// The board is compiled out: say how to get it instead of pretending.
#[cfg(not(feature = "nvboard"))]
fn run_board(_args: &BoardArgs) -> Result<u8> {
    eprintln!(
        "shyhdl: this build has no `nvboard` feature; rebuild with `--features nvboard` to use \
         `board`"
    );
    Ok(exit::NOT_IMPLEMENTED)
}

/// Exit code of a finished simulation run. A run that did not pass never exits
/// zero, whatever the simulator's own exit status was (REQ-TEST-008).
fn outcome_exit_code(outcome: Outcome) -> u8 {
    match outcome {
        Outcome::Pass => exit::OK,
        Outcome::Fail => exit::FAILED,
        Outcome::Timeout => exit::TIMEOUT,
        Outcome::Environment => exit::ENVIRONMENT,
        Outcome::CompileError | Outcome::SimulationError => exit::SIMULATION_ERROR,
    }
}

/// The testbench generator's view of the DUT top module.
///
/// This is the whole adapter between the frontend and the testbench layer: the
/// port list and its directions, and nothing else is re-derived from the
/// source.
fn design_info(compiled: &CompiledDesign) -> DesignInfo {
    let top = compiled.design.top();
    DesignInfo::new(
        top.verilog_name.clone(),
        top.source_name.clone(),
        top.ports
            .iter()
            .map(|port| {
                ModulePort::new(
                    port.source_name.clone(),
                    port.verilog_name.clone(),
                    match port.dir {
                        // A clock is an ordinary input: which signal is used as
                        // the reference clock is a testbench property, not a
                        // property of the design.
                        PortDir::In => PortDirection::In,
                        PortDir::Out => PortDirection::Out,
                    },
                    port.ty.clone(),
                )
            })
            .collect(),
    )
}

fn run_list_tests(args: &ListTestsArgs) -> Result<u8> {
    let source = read_text(&args.source)?;
    let source_name = source_name(&args.source);
    let tests = testbench::list_tests(&source, &source_name)?;
    if tests.is_empty() {
        // Nothing to list is not a failure, but the note goes to stderr so
        // stdout stays a list of tests and nothing else.
        eprintln!("shyhdl: {source_name}: no #[hardware_test] functions");
        return Ok(exit::OK);
    }
    for test in tests {
        println!(
            "{}:{}:{} {} timeunit={} precision={}",
            test.source.file,
            test.source.line,
            test.source.column,
            test.name,
            test.timeunit,
            test.timeprecision
        );
    }
    Ok(exit::OK)
}

fn run_debug(args: &DebugArgs) -> Result<u8> {
    match serve_one_session(args) {
        Ok(()) => Ok(exit::OK),
        Err(failure) => {
            eprintln!("shyhdl: {}", failure.message);
            Ok(failure.code)
        }
    }
}

/// Serve one GDB session. Returns when GDB disconnects.
fn serve_one_session(args: &DebugArgs) -> std::result::Result<(), DebugFailure> {
    let pairing = debug_pairing(args)?;
    let recording = pairing.recording.clone();
    let waveform = Waveform::load(&recording).map_err(|error| wave_failure(&recording, error))?;
    let mode = debug_step_mode(args, &pairing, &waveform)?;
    let engine =
        ReplayEngine::new(waveform, mode).map_err(|error| wave_failure(&recording, error))?;

    let server = DebugServer::build(engine, &pairing.metadata, &DebugOptions::new(pairing.root))?;
    // The session is only usable if the caller knows where to point GDB, so the
    // connection facts go to stdout and are flushed before the server blocks.
    // The read-only line comes first: it is what a user must know before typing
    // anything, and it leaves the `elf=` / `port=` / `gdb=` lines a script
    // parses untouched.
    println!("replay=read-only: recorded values cannot be modified");
    println!("elf={}", server.elf_path().display());
    println!("port={}", server.port());
    println!(
        "gdb={}",
        server
            .gdb_argv()
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let _ = std::io::stdout().flush();

    server.serve_blocking()?;
    Ok(())
}

/// Everything one debug session needs: the design metadata, the explicit
/// metadata-to-recording root mapping, and the recording itself.
struct DebugPairing {
    metadata: DesignMetadata,
    root: RootMapping,
    /// `roots[].var` of the selected DUT instance; the key clock candidates are
    /// matched by. Empty in external-fixture mode, which has no testcase record.
    root_var: String,
    /// Clocks the testcase declared, from the run manifest.
    clocks: Vec<simulation::ClockRef>,
    recording: PathBuf,
}

/// Resolve how the recording and the design symbols are paired.
fn debug_pairing(args: &DebugArgs) -> std::result::Result<DebugPairing, DebugFailure> {
    match &args.manifest {
        Some(path) => pairing_from_manifest(args, path),
        None => pairing_from_metadata(args),
    }
}

/// Verified pairing: the run manifest recorded by `shyhdl test`.
fn pairing_from_manifest(
    args: &DebugArgs,
    manifest_path: &Path,
) -> std::result::Result<DebugPairing, DebugFailure> {
    // A manifest that is malformed or describes an unusable run is a refused
    // pairing, not a source-level compile error, so its validation failure is
    // re-classified instead of surfacing as exit code 3.
    let manifest = simulation::load_manifest(manifest_path).map_err(classify_manifest_error)?;
    let dir = manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // The metadata file is the only source of source-level variables.
    let metadata_name =
        manifest
            .paths
            .metadata
            .as_deref()
            .ok_or_else(|| invalid_metadata(format!(
                "{} records no design metadata, so no source variable can be read; re-run `shyhdl test` with this build",
                manifest_path.display()
            )))?;
    let metadata_path = dir.join(metadata_name);
    let metadata = read_metadata(&metadata_path)?;

    // Build id: recorded by the run, repeated by the metadata. Both come from
    // the frontend, and a disagreement means the symbols are of another build.
    let recorded_build_id = manifest.design.build_id.clone().ok_or_else(|| {
        invalid_metadata(format!(
            "{} records no design build id, so the recording cannot be paired with symbols",
            manifest_path.display()
        ))
    })?;
    if recorded_build_id != metadata.build_id {
        return Err(DebugFailure::from(Error::BuildIdMismatch {
            expected: metadata.build_id.clone(),
            found: recorded_build_id,
        }));
    }
    if manifest.design.top != metadata.top {
        return Err(DebugFailure::from(invalid_metadata(format!(
            "{} names top `{}` but {} names `{}`",
            manifest_path.display(),
            manifest.design.top,
            metadata_path.display(),
            metadata.top
        ))));
    }

    let selected = select_root(&manifest.roots, args.instance.as_deref(), manifest_path)?;
    let root = RootMapping::new(metadata.top.clone(), selected.wave_root.clone());

    // The recording carries its own identity: two runs of the same design write
    // the same file name, so the content digest recorded in the manifest is what
    // proves that this file is the one the manifest was written for
    // (REQ-DBG-010). The manifest verifies it itself, so there is exactly one
    // implementation of that check.
    let verified = match &manifest.wave {
        Some(_) => manifest.verify_wave(dir).map_err(|error| match error {
            // The library compares the recorded digest with the file it finds;
            // name the run and the two digests in the CLI's own words, because
            // the shared message talks about symbols and this is about the
            // recording's identity.
            Error::BuildIdMismatch { expected, found } => {
                recording_mismatch(&manifest, &expected, &found)
            }
            other => classify_manifest_error(other),
        })?,
        None => {
            return Err(DebugFailure::from(invalid_metadata(format!(
                "{} records no waveform, so its recording cannot be identified; \
                 use the manifest of a run that recorded one, or pair an external \
                 fixture with --metadata and --assume-build-id",
                manifest_path.display()
            ))))
        }
    };

    // An explicit recording must be that same file, so two runs can never be
    // mixed by pointing one manifest at another run's path.
    let recording = match &args.recording {
        Some(explicit) => {
            if !same_recording(explicit, &verified) {
                return Err(DebugFailure::from(invalid_metadata(format!(
                    "RECORDING {} is not the recording of {} ({}); pass one of them",
                    explicit.display(),
                    manifest_path.display(),
                    verified.display()
                ))));
            }
            explicit.clone()
        }
        None => verified,
    };

    Ok(DebugPairing {
        metadata,
        root,
        root_var: selected.var.clone(),
        clocks: manifest.clocks.clone(),
        recording,
    })
}

/// External fixture mode: metadata plus a recording that was not produced by
/// `shyhdl test`, paired by an explicit assertion.
fn pairing_from_metadata(args: &DebugArgs) -> std::result::Result<DebugPairing, DebugFailure> {
    // The argument layer guarantees these three together.
    let metadata_path = args.metadata.as_deref().ok_or_else(|| {
        invalid_metadata("either --manifest or --metadata is required".to_string())
    })?;
    let recording = args
        .recording
        .clone()
        .ok_or_else(|| invalid_metadata("RECORDING is required with --metadata".to_string()))?;
    let metadata = read_metadata(metadata_path)?;

    let assumed = args.assume_build_id.as_deref().ok_or_else(|| {
        invalid_metadata(
            "a recording carries no build identity of its own: pass --assume-build-id <ID> to state              which build it came from, or use --manifest from `shyhdl test`"
                .to_string(),
        )
    })?;
    if assumed != metadata.build_id {
        return Err(DebugFailure::from(Error::BuildIdMismatch {
            expected: metadata.build_id.clone(),
            found: assumed.to_string(),
        }));
    }

    let wave_root = args
        .wave_root
        .clone()
        .unwrap_or_else(|| metadata.top.clone());
    Ok(DebugPairing {
        root: RootMapping::new(metadata.top.clone(), wave_root),
        metadata,
        // A recording that no `shyhdl test` run produced has no testcase record,
        // so there are no clock candidates to default to.
        root_var: String::new(),
        clocks: Vec::new(),
        recording,
    })
}

/// The DUT instance of a manifest whose recording root is used, or an error
/// listing the candidates. Never a suffix guess (REQ-DBG-026).
fn select_root<'a>(
    roots: &'a [simulation::RootMapping],
    instance: Option<&str>,
    manifest_path: &Path,
) -> std::result::Result<&'a simulation::RootMapping, DebugFailure> {
    let vars = || {
        roots
            .iter()
            .map(|root| root.var.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (roots, instance) {
        ([], _) => Err(DebugFailure::from(invalid_metadata(format!(
            "{} records no DUT instance, so no source variable can be placed in the recording",
            manifest_path.display()
        )))),
        ([only], None) => Ok(only),
        (many, None) => Err(DebugFailure::from(invalid_metadata(format!(
            "{} records {} DUT instances; select one with --instance <TBVAR>: {}",
            manifest_path.display(),
            many.len(),
            vars()
        )))),
        (_many, Some(var)) => roots.iter().find(|root| root.var == var).ok_or_else(|| {
            DebugFailure::from(invalid_metadata(format!(
                "no DUT instance named `{var}` in {}; available: {}",
                manifest_path.display(),
                vars()
            )))
        }),
    }
}

/// Reference points of the replay.
///
/// An explicit choice always wins: `--clock` names a signal path in the
/// recording, `--grid-interval` selects the fixed grid, and the two are
/// mutually exclusive (REQ-DBG-020, REQ-DBG-022).
///
/// With neither given, the reference clock comes from the testcase that
/// produced the recording: the manifest lists the clocks it generated
/// (`gen_clk(...)` / `clock!`), and only the candidates whose `var` is the
/// selected DUT instance count. Exactly one is used; none, several, or a
/// candidate that is not in the recording are reported rather than guessed
/// (REQ-DBG-021). Nothing is inferred from the source: a clock is an ordinary
/// input, so the design carries no clock marking at all.
fn debug_step_mode(
    args: &DebugArgs,
    pairing: &DebugPairing,
    waveform: &Waveform,
) -> std::result::Result<StepMode, DebugFailure> {
    let edge = match args.edge {
        Some(EdgeArg::Negedge) => Edge::Neg,
        Some(EdgeArg::Both) => Edge::Both,
        _ => Edge::Pos,
    };
    if let Some(signal) = &args.clock {
        return Ok(StepMode::Clock {
            signal: require_recorded_clock(signal, waveform)?,
            edge,
        });
    }
    if let Some(interval) = args.grid_interval {
        let period = waveform
            .ticks(interval)
            .map_err(|error| wave_failure(&pairing.recording, error))?;
        let origin = match args.grid_origin {
            Some(origin) => waveform
                .ticks(origin)
                .map_err(|error| wave_failure(&pairing.recording, error))?,
            None => 0,
        };
        return Ok(StepMode::Interval { origin, period });
    }

    let candidates: Vec<&simulation::ClockRef> = pairing
        .clocks
        .iter()
        .filter(|clock| clock.var == pairing.root_var)
        .collect();
    match candidates.as_slice() {
        [clock] => Ok(StepMode::Clock {
            signal: require_recorded_clock(&clock.signal, waveform)?,
            edge,
        }),
        [] => Err(DebugFailure::from(invalid_metadata(format!(
            "no step mode was given and no testcase clock was recorded for the DUT instance `{}`: \
             pass --clock <SIGNAL> (a signal path in the recording) or --grid-interval <DURATION>",
            pairing.root_var
        )))),
        candidates => Err(DebugFailure::from(invalid_metadata(format!(
            "no step mode was given and {} testcase clocks were recorded for the DUT instance `{}`: {}; \
             choose one with --clock <SIGNAL> or use --grid-interval <DURATION>",
            candidates.len(),
            pairing.root_var,
            candidates
                .iter()
                .map(|clock| clock.signal.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))),
    }
}

/// The recording path of a clock that is actually in the waveform. A candidate
/// or an explicit `--clock` that the recording does not contain is refused
/// instead of producing a session that cannot step.
fn require_recorded_clock(
    signal: &str,
    waveform: &Waveform,
) -> std::result::Result<String, DebugFailure> {
    match waveform.signal(signal) {
        Ok(_) => Ok(signal.to_string()),
        Err(error) => Err(DebugFailure::from(invalid_metadata(format!(
            "clock `{signal}` is not in the recording: {error}"
        )))),
    }
}

/// A failure of `debug`, carrying the exit code it maps to. The debug layer has
/// its own error type, and this keeps one command from collapsing every kind of
/// failure into the same code.
struct DebugFailure {
    code: u8,
    message: String,
}

impl From<Error> for DebugFailure {
    fn from(error: Error) -> Self {
        Self {
            code: exit_code_for(&error),
            message: error.to_string(),
        }
    }
}

impl From<DebugError> for DebugFailure {
    fn from(error: DebugError) -> Self {
        let code = match &error {
            DebugError::UnsupportedType { .. } => exit::NOT_IMPLEMENTED,
            // The symbols and the recording disagree about the design: refuse
            // rather than read a value from the wrong signal (REQ-DBG-010).
            DebugError::RootMapping { .. } | DebugError::WidthMismatch { .. } => exit::REFUSED,
            DebugError::Symbols(error) => exit_code_for(error),
            DebugError::Io { .. } | DebugError::Waveform(_) | DebugError::Session(_) => {
                exit::FAILED
            }
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

/// A waveform failure, reported against the recording it happened on. The
/// crate error type has no waveform variant, so the message is carried as the
/// I/O cause rather than dropped.
fn wave_failure(path: &Path, error: WaveError) -> DebugFailure {
    DebugFailure::from(Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
    })
}

fn invalid_metadata(message: String) -> Error {
    Error::InvalidMetadata { message }
}

/// Read a design metadata file and validate it.
fn read_metadata(path: &Path) -> Result<DesignMetadata> {
    let json = read_text(path)?;
    DesignMetadata::from_json_str(&json)
}

/// The recording is not the one the manifest was written for: name the run and
/// both content digests instead of reading a value from a stray file.
fn recording_mismatch(
    manifest: &simulation::RunManifest,
    expected: &str,
    found: &str,
) -> DebugFailure {
    let named = manifest
        .wave
        .as_ref()
        .map(|wave| wave.path.as_str())
        .unwrap_or("<no recording>");
    DebugFailure::from(invalid_metadata(format!(
        "the recording {named} is not the one run {} wrote (recorded content digest {expected}, found {found}); re-run `shyhdl test` to pair them",
        manifest.run_id
    )))
}

/// A manifest whose content is unusable is a refused pairing rather than a
/// source-level compile error, so its validation failure is reported as invalid
/// metadata.
fn classify_manifest_error(error: Error) -> DebugFailure {
    match error {
        Error::TypeLayout { message, .. } => DebugFailure::from(Error::InvalidMetadata { message }),
        other => DebugFailure::from(other),
    }
}

/// Whether two paths name the same file. Paths that cannot be canonicalized
/// (a file that does not exist yet) are compared as written.
fn same_recording(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Quote one GDB argument so the printed command line can be pasted into a
/// shell unchanged.
fn shell_quote(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/._-:=+,".contains(character));
    if plain {
        argument.to_string()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

/// Read a text file, reporting the path on failure.
fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Name a source file is known by, exactly as the user spelled it. The
/// frontend, the testbench generator and the metadata all report locations
/// against this string, so the CLI must not rewrite it.
fn source_name(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Write a generated artifact, reporting the path on failure.
fn write_file(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Compile one source file, printing its diagnostics.
///
/// A source that does not compile is a normal outcome with its own exit code,
/// not a crate error: `Err` carries the code to exit with. Everything the
/// frontend reported goes to stderr, with its source location.
fn compile_source(
    source: &str,
    source_name: &str,
    top: &str,
) -> std::result::Result<CompiledDesign, u8> {
    match frontend::compile_all(source, source_name, top) {
        Ok(compiled) => {
            for warning in compiled.warnings() {
                eprintln!("{}", warning.to_report_string());
            }
            Ok(compiled)
        }
        Err(diagnostics) => {
            for diagnostic in diagnostics.as_slice() {
                eprintln!("{}", diagnostic.to_report_string());
            }
            let errors: Vec<&frontend::Diagnostic> = diagnostics.errors().collect();
            // Only a source whose every error is "this build does not have that
            // capability yet" is the build's fault; a genuine error in the
            // source stays a compile error.
            let unimplemented = !errors.is_empty()
                && errors
                    .iter()
                    .all(|error| error.code == frontend::codes::NOT_IMPLEMENTED);
            Err(if unimplemented {
                exit::NOT_IMPLEMENTED
            } else {
                exit::COMPILE_ERROR
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

/// A module reference or a test function name.
///
/// The frontend owns the grammar of a module reference: `--top` also accepts a
/// build-time specialization such as `Adder<8>`, so the CLI must not be
/// stricter than the compiler that reads it. Only values that can never be a
/// name are rejected here, and the compiler diagnoses the rest with a source
/// location.
fn parse_name(text: &str) -> std::result::Result<String, String> {
    if text.trim().is_empty() {
        return Err("the name is empty".to_string());
    }
    if text.trim() != text {
        return Err(format!(
            "`{text}` has leading or trailing whitespace, which is never part of a name"
        ));
    }
    Ok(text.to_string())
}

/// Integer duration with an explicit unit, for example `10ns`. Fractional
/// values and unitless numbers are rejected rather than rounded (REQ-TEST-004).
fn parse_duration(text: &str) -> std::result::Result<Timescale, String> {
    Timescale::parse(text).map_err(|error| error.to_string())
}
