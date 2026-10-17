//! M3: run a generated testbench together with the DUT on a real Verilog
//! simulator.
//!
//! The backend is a released Verilator, invoked directly
//! (`--cc --exe --build --timing --trace`) to produce a model plus a thin C++
//! `main()` (`runner_main.cpp`); nothing is wrapped in an unavailable library and
//! no in-process embedding is claimed. Verilator owns time: the runner jumps from
//! one scheduled time slot to the next
//! (`VerilatedContext::time(nextTimeSlot())`), evaluates the model and dumps the
//! waveform, so Rust never implements a second scheduler (REQ-SIM-001,
//! REQ-SIM-002, REQ-SIM-003).
//!
//! # What a run reports
//!
//! The generated testbench prints structured lines; this module parses them
//! strictly, matching the test name and cross-checking the summary against the
//! lines it saw, and classifies the result into [`Outcome::Pass`],
//! [`Outcome::Fail`], [`Outcome::Timeout`], [`Outcome::Environment`],
//! [`Outcome::CompileError`] or [`Outcome::SimulationError`] (REQ-TEST-008,
//! REQ-TEST-009):
//!
//! * a `SHYHDL_FAIL` line wins over everything else;
//! * a non-zero exit code is a failure;
//! * no `SHYHDL_PASS` line is a failure - `$finish` with exit code 0 is not
//!   evidence of success by itself;
//! * a deadline that killed the process, or the runner's own step limit, is a
//!   timeout, and it is not confused with an unrelated `SIGKILL`;
//! * a missing Verilator or `timeout` is an environment error, reported with what
//!   was looked for, never a silently weaker path;
//! * a requested waveform that the run did not produce turns an otherwise
//!   successful run into a [`Outcome::SimulationError`], so a recording is never
//!   silently missing.
//!
//! # Artifacts
//!
//! Everything is written into the caller's build directory: the DUT and
//! testbench Verilog, the generated runner, Verilator's model and executable, the
//! build log, the waveform and [`RunManifest`]. Sources are never modified. The
//! directory may be relative, may contain non-ASCII characters and is
//! canonicalized once per run, because the tools are started with it as their
//! working directory. `reuse_build` only skips re-running Verilator when the
//! sources, the runner, the flags and the tool version are unchanged; a run and
//! its waveform are always produced fresh, and a waveform from an earlier run is
//! removed before the next one starts.
//!
//! # Known backend limitation
//!
//! Verilator refuses to build in a directory whose path contains a space. Its own
//! `verilated.mk` says so and stops the build:
//!
//! ```text
//! verilated.mk:76: *** Unsupported: GNU Make cannot build in directories
//! containing spaces, build elsewhere: '/tmp/.../with space/out'
//! ```
//!
//! The check is on the directory itself, so passing a relative `--Mdir .` with the
//! build directory as the working directory does not avoid it (`make -C .` still
//! fails at that line). Non-ASCII directory names work. This is a backend
//! limitation reported as [`Outcome::CompileError`] with Verilator's own message
//! in the build log; ShyHDL itself always passes an argument vector and never
//! builds a shell string.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::metadata::{compute_build_id, SourceUnit};
use crate::testbench::{DesignInfo, TestbenchArtifact};

mod manifest;
mod toolchain;

pub use manifest::{
    digest_file, load_manifest, write_manifest, ClockRef, DesignRef, ManifestPaths, RootMapping,
    RunManifest, TestRef, WaveRef, MANIFEST_FILE, MANIFEST_SCHEMA, WAVE_DIGEST_PREFIX,
};
pub use manifest::{BackendRef, Counts, ParameterRef};
use toolchain::{CommandOutcome, Toolchain};

/// Generated C++ `main()` written into the build directory.
const RUNNER_FILE: &str = "runner.cpp";
/// Name Verilator gives the built executable inside the build directory.
const EXE_NAME: &str = "sim";
/// Verilator build output kept for diagnosis.
const BUILD_LOG: &str = "build.log";
/// Marker file holding the build fingerprint of the last successful build.
const BUILD_STAMP: &str = ".shyhdl_build_stamp";
/// Safety net on the number of event steps a run may take; the wall-clock
/// deadline is the primary bound.
const MAX_STEPS: u64 = 100_000_000;

/// Result of a simulation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The testbench completed and reported success.
    Pass,
    /// An assertion failed, the exit status was non-zero, or no PASS was
    /// reported.
    Fail,
    /// A deadline killed the run, or the testbench's `timeout!()` fired.
    Timeout,
    /// A required tool is missing or unusable.
    Environment,
    /// Verilator could not build the model.
    CompileError,
    /// The simulation itself failed (a crash, or a run that ended without
    /// reaching a testbench end).
    SimulationError,
}

impl Outcome {
    /// Stable lower-case name, used in the manifest and by the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Timeout => "timeout",
            Outcome::Environment => "environment",
            Outcome::CompileError => "compile_error",
            Outcome::SimulationError => "simulation_error",
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One Verilog source handed to the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Plain file name inside the build directory (no separators, no `..`).
    pub name: String,
    /// Full text of the file.
    pub content: String,
}

impl SourceFile {
    /// Build a source file.
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: content.into(),
        }
    }
}

/// What the testbench generator produced, projected so this module does not
/// depend on the generator's internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBenchInfo {
    /// Name of the `#[hardware_test]` function.
    pub name: String,
    /// Verilog module name of the testbench.
    pub top: String,
    /// C++ model class of the testbench.
    pub top_type: String,
    /// Time unit, for example `1ns`.
    pub timeunit: String,
    /// Time precision, for example `1ps`.
    pub timeprecision: String,
    /// Design metadata to waveform scope mappings, one per DUT instance.
    pub roots: Vec<RootMapping>,
    /// Clocks the test generates, with their full path in the recording.
    pub clocks: Vec<ClockRef>,
}

/// Whether a waveform is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaveOption {
    /// No waveform.
    None,
    /// VCD waveform, written as a plain file name inside the build directory.
    Vcd {
        /// File name, for example `wave.vcd`.
        file_name: String,
    },
}

impl WaveOption {
    /// The default VCD waveform name.
    pub fn vcd() -> Self {
        WaveOption::Vcd {
            file_name: "wave.vcd".to_string(),
        }
    }

    fn file_name(&self) -> Option<&str> {
        match self {
            WaveOption::None => None,
            WaveOption::Vcd { file_name } => Some(file_name),
        }
    }
}

/// Everything a run needs.
#[derive(Debug, Clone)]
pub struct SimOptions {
    /// Directory for every artifact of the run; created if needed.
    pub build_dir: PathBuf,
    /// DUT Verilog, in compile order.
    pub dut_sources: Vec<SourceFile>,
    /// The generated testbench; compiled together with the DUT (REQ-VER-003).
    pub tb_source: SourceFile,
    /// Testbench metadata, including the waveform root mappings.
    pub testbench: TestBenchInfo,
    /// Verilog module name of the design top, used as `source_root`.
    pub design_top: String,
    /// Source-level name of the design top.
    pub design_top_source_name: String,
    /// Build id from the frontend metadata; recorded, never recomputed.
    pub design_build_id: Option<String>,
    /// Frontend metadata file (plain name inside `build_dir`) to pair with the
    /// recording.
    pub metadata_file: Option<String>,
    /// Waveform to write.
    pub wave: WaveOption,
    /// Deadline for the simulation process.
    pub timeout: Duration,
    /// Deadline for the Verilator build.
    pub build_timeout: Duration,
    /// Seed passed to the backend at run time, when the caller chose one.
    pub seed: Option<u64>,
    /// Backend parameters, recorded in the manifest.
    pub parameters: Vec<(String, String)>,
    /// Sampling strategy description, recorded in the manifest.
    pub sampling: String,
    /// Reuse the compiled model when sources, runner, flags and tool version are
    /// unchanged. Only the executable is reused: the simulation always runs again
    /// and its waveform and results are produced fresh.
    pub reuse_build: bool,
}

impl SimOptions {
    /// Options for running a generated testbench, filling the testbench and root
    /// information from the artifact so nothing has to be transcribed by hand.
    pub fn for_testbench(
        build_dir: PathBuf,
        artifact: &TestbenchArtifact,
        design: &DesignInfo,
        dut_sources: Vec<SourceFile>,
    ) -> Self {
        let roots = artifact
            .instances
            .iter()
            .map(|instance| RootMapping {
                source_root: design.top.clone(),
                wave_root: instance.scope.clone(),
                var: instance.var.clone(),
                instance: instance.instance.clone(),
                module: instance.module.clone(),
                module_source_name: instance.module_source_name.clone(),
            })
            .collect();
        Self {
            build_dir,
            dut_sources,
            tb_source: SourceFile::new(format!("{}.v", artifact.top), artifact.verilog.clone()),
            testbench: TestBenchInfo {
                name: artifact.name.clone(),
                top: artifact.top.clone(),
                top_type: artifact.top_type.clone(),
                timeunit: artifact.timeunit.as_verilog().to_string(),
                timeprecision: artifact.timeprecision.as_verilog().to_string(),
                roots,
                clocks: artifact
                    .clocks
                    .iter()
                    .map(|clock| ClockRef {
                        var: clock.var.clone(),
                        signal: clock.signal.clone(),
                    })
                    .collect(),
            },
            design_top: design.top.clone(),
            design_top_source_name: design.top_source_name.clone(),
            design_build_id: None,
            metadata_file: None,
            wave: WaveOption::None,
            timeout: Duration::from_secs(60),
            build_timeout: Duration::from_secs(300),
            seed: None,
            parameters: Vec::new(),
            sampling: "verilator --timing; the testbench samples after an edge with a positive \
                       settle offset (sample_after)"
                .to_string(),
            reuse_build: true,
        }
    }
}

/// What a run produced.
#[derive(Debug, Clone)]
pub struct SimReport {
    /// Classification of the run.
    pub outcome: Outcome,
    /// Why the run was classified that way, in one line.
    pub reason: String,
    /// Name of the test that was run.
    pub test_name: String,
    /// Exit code of the simulation process.
    pub exit_code: Option<i32>,
    /// `SHYHDL_PASS` lines.
    pub passes: u64,
    /// `SHYHDL_FAIL` lines.
    pub fails: u64,
    /// Explicit `pass!()` markers that completed.
    pub markers: u64,
    /// Statically reachable markers, as the summary reported them.
    pub expected_markers: Option<u64>,
    /// Raw `SHYHDL_FAIL` lines, ready to print.
    pub fail_lines: Vec<String>,
    /// Simulation output.
    pub stdout: String,
    /// Simulation error output (including Verilator's warnings).
    pub stderr: String,
    /// Waveform, when one was written.
    pub wave_path: Option<PathBuf>,
    /// Time scale read back from the waveform header, for example `1ps`.
    pub wave_timescale: Option<String>,
    /// Content digest of the waveform, the pairing identity recorded in the
    /// manifest (see [`manifest::digest_file`]).
    pub wave_digest: Option<String>,
    /// Manifest pairing this run with its design and waveform.
    pub manifest_path: PathBuf,
    /// Id of this run.
    pub run_id: String,
    /// Verilator build log.
    pub build_log_path: PathBuf,
    /// Verilator version used.
    pub verilator_version: Option<String>,
    /// Commands that were executed, for diagnosis.
    pub command_lines: Vec<String>,
}

impl SimReport {
    /// Whether the run is a pass.
    pub fn is_pass(&self) -> bool {
        self.outcome == Outcome::Pass
    }

    /// One line summary for a CLI.
    pub fn summary_line(&self) -> String {
        let expected = match self.expected_markers {
            Some(count) => count.to_string(),
            None => "unknown".to_string(),
        };
        format!(
            "{} test={} pass={} fail={} markers={}/{} exit={} run={} ({})",
            self.outcome,
            self.test_name,
            self.passes,
            self.fails,
            self.markers,
            expected,
            match self.exit_code {
                Some(code) => code.to_string(),
                None => "signal".to_string(),
            },
            self.run_id,
            self.reason
        )
    }
}

/// The Verilog simulator backend.
#[derive(Debug, Clone)]
pub struct Simulator {
    toolchain: Toolchain,
}

impl Simulator {
    /// Find the backend and the deadline tool, or report what is missing.
    pub fn discover() -> Result<Self> {
        Ok(Self {
            toolchain: Toolchain::discover()?,
        })
    }

    /// Path of the Verilator executable in use.
    pub fn verilator_path(&self) -> &Path {
        self.toolchain.verilator()
    }

    /// Version line of that Verilator.
    pub fn verilator_version(&self) -> &str {
        self.toolchain.verilator_version()
    }

    /// Path of the `timeout` executable in use.
    pub fn timeout_path(&self) -> &Path {
        self.toolchain.timeout()
    }

    /// Build the model if needed, run it, and report what happened.
    ///
    /// I/O problems (a build directory that cannot be written) are returned as
    /// `Err`; everything else - a failed build, a timeout, a failing test - is
    /// reported through [`SimReport::outcome`], and the manifest is written in
    /// every one of those cases so the evidence survives.
    pub fn run(&self, options: &SimOptions) -> Result<SimReport> {
        let context = self.prepare(options)?;
        match self.build(options, &context)? {
            BuildResult::Failed {
                outcome,
                reason,
                command,
            } => {
                let mut report = context.report(
                    &self.toolchain,
                    options,
                    outcome,
                    reason,
                    None,
                    Protocol::default(),
                    String::new(),
                    String::new(),
                    None,
                    vec![command],
                );
                context.write_manifest(options, &mut report)?;
                Ok(report)
            }
            BuildResult::Ready { commands } => self.execute(options, &context, commands),
        }
    }

    /// Write the sources and the runner, and validate every path.
    fn prepare(&self, options: &SimOptions) -> Result<RunContext> {
        // One absolute root for the whole run: the caller may pass a relative
        // build directory, and the tools are invoked with that directory as their
        // working directory, so a relative root would silently become
        // `build/build/...`.
        std::fs::create_dir_all(&options.build_dir).map_err(|source| Error::Io {
            path: options.build_dir.clone(),
            source,
        })?;
        let build_dir = &std::fs::canonicalize(&options.build_dir).map_err(|source| Error::Io {
            path: options.build_dir.clone(),
            source,
        })?;

        let mut names = Vec::new();
        for source in &options.dut_sources {
            validate_file_name(&source.name)?;
            names.push(source.name.clone());
        }
        validate_file_name(&options.tb_source.name)?;
        names.push(options.tb_source.name.clone());
        if let Some(metadata) = &options.metadata_file {
            validate_file_name(metadata)?;
            let path = build_dir.join(metadata);
            if !path.is_file() {
                return Err(Error::Io {
                    path,
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "the design metadata file was named but does not exist in the build \
                         directory; write it before the run so the manifest can pair it",
                    ),
                });
            }
        }
        if let Some(wave) = options.wave.file_name() {
            validate_file_name(wave)?;
        }
        for reserved in [RUNNER_FILE, BUILD_LOG, EXE_NAME, BUILD_STAMP, MANIFEST_FILE] {
            if names.iter().any(|name| name == reserved) {
                return Err(Error::TypeLayout {
                    message: format!(
                        "`{reserved}` is written by the runner and cannot be used as a source file \
                         name"
                    ),
                    span: None,
                });
            }
        }
        let duplicates = names
            .iter()
            .enumerate()
            .any(|(index, name)| names[..index].contains(name));
        if duplicates {
            return Err(Error::TypeLayout {
                message: "two sources have the same file name".to_string(),
                span: None,
            });
        }

        let mut dut_paths = Vec::new();
        for source in &options.dut_sources {
            let path = build_dir.join(&source.name);
            std::fs::write(&path, &source.content).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            dut_paths.push(path);
        }
        let tb_path = build_dir.join(&options.tb_source.name);
        std::fs::write(&tb_path, &options.tb_source.content).map_err(|source| Error::Io {
            path: tb_path.clone(),
            source,
        })?;

        let runner = runner_source(&options.testbench.top_type);
        let runner_path = build_dir.join(RUNNER_FILE);
        std::fs::write(&runner_path, &runner).map_err(|source| Error::Io {
            path: runner_path.clone(),
            source,
        })?;

        let run_id = make_run_id(options, &runner, &dut_paths, &tb_path);
        Ok(RunContext {
            build_dir: build_dir.clone(),
            tb_name: options.tb_source.name.clone(),
            dut_names: options
                .dut_sources
                .iter()
                .map(|source| source.name.clone())
                .collect(),
            metadata_file: options.metadata_file.clone(),
            runner,
            dut_paths,
            tb_path,
            runner_path,
            run_id,
            commands: Vec::new(),
        })
    }

    /// Build the model unless a previous identical build can be reused.
    fn build(&self, options: &SimOptions, context: &RunContext) -> Result<BuildResult> {
        let flags = self.verilator_flags(options, context);
        let fingerprint = build_fingerprint(
            &self.toolchain,
            &flags,
            &context.runner,
            &context.dut_paths,
            &context.tb_path,
        );
        let exe = context.build_dir.join(EXE_NAME);
        let stamp = context.build_dir.join(BUILD_STAMP);
        let up_to_date = options.reuse_build
            && exe.is_file()
            && std::fs::read_to_string(&stamp)
                .map(|text| text.trim() == fingerprint)
                .unwrap_or(false);
        if up_to_date {
            return Ok(BuildResult::Ready {
                commands: Vec::new(),
            });
        }

        let args: Vec<OsString> = flags.iter().map(OsString::from).collect();
        let outcome = self.toolchain.run_bounded(
            self.toolchain.verilator(),
            &args,
            &context.build_dir,
            options.build_timeout,
        )?;
        let log = format!(
            "$ {}\n[exit {:?}, {:.1}s]\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
            outcome.command_line,
            outcome.status,
            outcome.elapsed.as_secs_f64(),
            outcome.stdout,
            outcome.stderr
        );
        let build_log_path = context.build_dir.join(BUILD_LOG);
        std::fs::write(&build_log_path, &log).map_err(|source| Error::Io {
            path: build_log_path.clone(),
            source,
        })?;

        if !outcome.success() {
            let (kind, reason) = if outcome.timed_out {
                (
                    Outcome::Timeout,
                    format!(
                        "the Verilator build did not finish within {}s and its process group was \
                         killed; see {}",
                        options.build_timeout.as_secs(),
                        build_log_path.display()
                    ),
                )
            } else {
                (
                    Outcome::CompileError,
                    format!(
                        "the Verilator build failed ({}); see {}",
                        describe_status(&outcome),
                        build_log_path.display()
                    ),
                )
            };
            return Ok(BuildResult::Failed {
                outcome: kind,
                reason,
                command: outcome.command_line,
            });
        }
        if !exe.is_file() {
            return Ok(BuildResult::Failed {
                outcome: Outcome::CompileError,
                reason: format!(
                    "the Verilator build reported success but {} does not exist; see {}",
                    exe.display(),
                    build_log_path.display()
                ),
                command: outcome.command_line,
            });
        }
        std::fs::write(&stamp, &fingerprint).map_err(|source| Error::Io {
            path: stamp.clone(),
            source,
        })?;
        Ok(BuildResult::Ready {
            commands: vec![outcome.command_line],
        })
    }

    fn verilator_flags(&self, options: &SimOptions, context: &RunContext) -> Vec<String> {
        let mut flags: Vec<String> = vec![
            "--cc".into(),
            "--exe".into(),
            "--build".into(),
            "--timing".into(),
            "--trace".into(),
            "-Wno-fatal".into(),
            "--Mdir".into(),
            context.build_dir.display().to_string(),
            "--top-module".into(),
            options.testbench.top.clone(),
            "-o".into(),
            EXE_NAME.into(),
        ];
        if let Ok(jobs) = std::thread::available_parallelism() {
            let jobs = jobs.get().min(8);
            if jobs > 1 {
                flags.push("-j".into());
                flags.push(jobs.to_string());
            }
        }
        // Absolute paths as separate arguments: the build runs with the build
        // directory as its working directory, and no shell ever sees these, so a
        // Chinese or spaced path cannot change what is executed.
        for path in &context.dut_paths {
            flags.push(path.display().to_string());
        }
        flags.push(context.tb_path.display().to_string());
        flags.push(context.runner_path.display().to_string());
        flags
    }

    fn execute(
        &self,
        options: &SimOptions,
        context: &RunContext,
        mut commands: Vec<String>,
    ) -> Result<SimReport> {
        let exe = context.build_dir.join(EXE_NAME);
        let wave_path = options
            .wave
            .file_name()
            .map(|name| context.build_dir.join(name));
        // Drop any waveform from an earlier run first: a stale file must never be
        // paired with this run's id in the manifest.
        if let Some(path) = &wave_path {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: path.clone(),
                        source,
                    })
                }
            }
        }
        let mut args: Vec<OsString> = vec![
            wave_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
                .into(),
            MAX_STEPS.to_string().into(),
        ];
        // A caller-chosen seed is really handed to the backend, which reads
        // `+verilator+seed+<n>` in VerilatedContext::commandArgs.
        if let Some(seed) = options.seed {
            args.push(format!("+verilator+seed+{seed}").into());
        }
        let outcome =
            self.toolchain
                .run_bounded(&exe, &args, &context.build_dir, options.timeout)?;
        commands.push(outcome.command_line.clone());

        let protocol = parse_protocol(&outcome.stdout, &options.testbench.name);
        let mut wave_written = wave_path.filter(|path| path.is_file());
        let mut wave_timescale = None;
        let mut wave_digest = None;
        let mut wave_problem = None;
        if let Some(path) = wave_written.clone() {
            match manifest::digest_file(&path).and_then(|digest| {
                wave_digest = Some(digest);
                read_vcd_timescale(&path)
            }) {
                Ok(scale) => wave_timescale = scale,
                Err(error) => {
                    // Keep the cause: the file exists but cannot be read back, so
                    // the recording cannot be paired with this run.
                    wave_problem = Some(format!("the waveform cannot be read back: {error}"));
                }
            }
        } else if let Some(requested) = options.wave.file_name() {
            wave_problem = Some(format!(
                "the requested waveform `{requested}` was not written by the run"
            ));
        }
        let (mut kind, mut reason) = classify(options, &outcome, &protocol);
        if let Some(problem) = wave_problem {
            // A failure or a timeout of the test itself stays the primary result;
            // an otherwise successful run that lost a requested artifact is not a
            // pass (REQ-TEST-008).
            if kind == Outcome::Pass {
                kind = Outcome::SimulationError;
                reason = format!("{problem}; the run is not reported as a pass without it");
            } else {
                reason = format!("{reason}; {problem}");
            }
            wave_written = None;
        }
        let mut report = context.report(
            &self.toolchain,
            options,
            kind,
            reason,
            outcome.status,
            protocol,
            outcome.stdout.clone(),
            outcome.stderr.clone(),
            wave_written,
            commands,
        );
        report.wave_timescale = wave_timescale;
        report.wave_digest = wave_digest;
        context.write_manifest(options, &mut report)?;
        Ok(report)
    }
}

enum BuildResult {
    Ready {
        commands: Vec<String>,
    },
    Failed {
        outcome: Outcome,
        reason: String,
        command: String,
    },
}

/// Files and ids of one run, shared by every exit path.
struct RunContext {
    build_dir: PathBuf,
    tb_name: String,
    dut_names: Vec<String>,
    metadata_file: Option<String>,
    runner: String,
    dut_paths: Vec<PathBuf>,
    tb_path: PathBuf,
    runner_path: PathBuf,
    run_id: String,
    commands: Vec<String>,
}

impl RunContext {
    #[allow(clippy::too_many_arguments)]
    fn report(
        &self,
        toolchain: &Toolchain,
        options: &SimOptions,
        outcome: Outcome,
        reason: String,
        exit_code: Option<i32>,
        protocol: Protocol,
        stdout: String,
        stderr: String,
        wave_path: Option<PathBuf>,
        commands: Vec<String>,
    ) -> SimReport {
        let mut all_commands = self.commands.clone();
        all_commands.extend(commands);
        SimReport {
            outcome,
            reason,
            test_name: options.testbench.name.clone(),
            exit_code,
            passes: protocol.passes,
            fails: protocol.fails,
            markers: protocol.markers,
            expected_markers: protocol.expected_markers,
            fail_lines: protocol.fail_lines,
            stdout,
            stderr,
            wave_path,
            wave_timescale: None,
            wave_digest: None,
            manifest_path: self.build_dir.join(MANIFEST_FILE),
            run_id: self.run_id.clone(),
            build_log_path: self.build_dir.join(BUILD_LOG),
            verilator_version: Some(toolchain.verilator_version().to_string()),
            command_lines: all_commands,
        }
    }

    /// Write the manifest that pairs this run with its design and waveform.
    fn write_manifest(&self, options: &SimOptions, report: &mut SimReport) -> Result<()> {
        let manifest = RunManifest {
            schema: MANIFEST_SCHEMA,
            run_id: self.run_id.clone(),
            design: DesignRef {
                top: options.design_top.clone(),
                top_source_name: options.design_top_source_name.clone(),
                build_id: options.design_build_id.clone(),
            },
            test: TestRef {
                name: options.testbench.name.clone(),
                top: options.testbench.top.clone(),
                top_type: options.testbench.top_type.clone(),
                timeunit: options.testbench.timeunit.clone(),
                timeprecision: options.testbench.timeprecision.clone(),
            },
            paths: ManifestPaths {
                manifest: MANIFEST_FILE.to_string(),
                dut_verilog: self.dut_names.clone(),
                tb_verilog: self.tb_name.clone(),
                metadata: self.metadata_file.clone(),
                build_log: BUILD_LOG.to_string(),
            },
            wave: report.wave_path.as_ref().map(|path| WaveRef {
                path: file_name_of(path),
                format: "vcd".to_string(),
                timescale_in_file: report.wave_timescale.clone(),
                // The recorded file's own identity, so a stale manifest cannot be
                // paired with a later recording that reused the same file name.
                digest: report.wave_digest.clone().unwrap_or_default(),
            }),
            roots: options.testbench.roots.clone(),
            clocks: options.testbench.clocks.clone(),
            backend: BackendRef {
                name: "verilator".to_string(),
                version: report.verilator_version.clone().unwrap_or_default(),
                flags: self.verilator_flags_for_manifest(options),
            },
            seed: options.seed,
            parameters: options
                .parameters
                .iter()
                .map(|(name, value)| ParameterRef {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            sampling: options.sampling.clone(),
            outcome: report.outcome.as_str().to_string(),
            exit_code: report.exit_code,
            counts: Counts {
                passes: report.passes,
                fails: report.fails,
                markers: report.markers,
                expected_markers: report.expected_markers,
            },
        };
        let path = self.build_dir.join(MANIFEST_FILE);
        write_manifest(&manifest, &path)?;
        report.manifest_path = path;
        Ok(())
    }

    /// Backend flags for the record: the fixed Verilator options, without the
    /// absolute paths of this particular build directory.
    fn verilator_flags_for_manifest(&self, options: &SimOptions) -> Vec<String> {
        let mut flags: Vec<String> = vec![
            "--cc".into(),
            "--exe".into(),
            "--build".into(),
            "--timing".into(),
            "--trace".into(),
            "-Wno-fatal".into(),
            "--Mdir".into(),
            "<build_dir>".into(),
            "--top-module".into(),
            options.testbench.top.clone(),
            "-o".into(),
            EXE_NAME.into(),
        ];
        for name in &self.dut_names {
            flags.push(name.clone());
        }
        flags.push(self.tb_name.clone());
        flags.push(RUNNER_FILE.to_string());
        flags
    }
}

/// Parse the structured lines the testbench printed.
#[derive(Debug, Default, Clone)]
struct Protocol {
    passes: u64,
    markers: u64,
    fails: u64,
    fail_lines: Vec<String>,
    timeout_lines: Vec<String>,
    summary: Option<Summary>,
    runner_timeout: bool,
    runner_error: Option<String>,
    runner_done: bool,
    other_test_lines: u64,
    expected_markers: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Summary {
    pass: u64,
    fail: u64,
    markers: u64,
    expected: Option<u64>,
}

impl Protocol {
    fn has_pass(&self) -> bool {
        self.passes > 0
    }
}

/// Read the protocol lines for exactly `test_name`.
///
/// A line only counts when it starts with a protocol token *and* names this test,
/// so a stray `$display` cannot be mistaken for a result; the summary is
/// cross-checked against the counted lines afterwards.
fn parse_protocol(stdout: &str, test_name: &str) -> Protocol {
    let mut protocol = Protocol::default();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("SHYHDL_PASS ") {
            match field(line, "test") {
                Some(name) if name == test_name => {
                    protocol.passes += 1;
                    if let Some(case) = field(line, "case") {
                        if case.chars().all(|character| character.is_ascii_digit()) {
                            protocol.markers += 1;
                        }
                    }
                }
                _ => protocol.other_test_lines += 1,
            }
        } else if line.starts_with("SHYHDL_FAIL ") {
            match field(line, "test") {
                Some(name) if name == test_name => {
                    protocol.fails += 1;
                    protocol.fail_lines.push(line.to_string());
                }
                _ => protocol.other_test_lines += 1,
            }
        } else if line.starts_with("SHYHDL_TIMEOUT ") {
            match field(line, "test") {
                Some(name) if name == test_name => protocol.timeout_lines.push(line.to_string()),
                _ => protocol.other_test_lines += 1,
            }
        } else if line.starts_with("SHYHDL_SUMMARY ") {
            if field(line, "test") == Some(test_name) {
                let expected = match field(line, "expected_markers") {
                    Some("unknown") | None => None,
                    Some(text) => text.parse().ok(),
                };
                protocol.summary = Some(Summary {
                    pass: field(line, "pass")
                        .and_then(|text| text.parse().ok())
                        .unwrap_or(0),
                    fail: field(line, "fail")
                        .and_then(|text| text.parse().ok())
                        .unwrap_or(0),
                    markers: field(line, "markers")
                        .and_then(|text| text.parse().ok())
                        .unwrap_or(0),
                    expected,
                });
            } else {
                protocol.other_test_lines += 1;
            }
        } else if line.starts_with("SHYHDL_RUNNER_TIMEOUT") {
            protocol.runner_timeout = true;
        } else if line.starts_with("SHYHDL_RUNNER_ERROR") {
            protocol.runner_error = Some(line.to_string());
        } else if line.starts_with("SHYHDL_RUNNER_DONE") {
            protocol.runner_done = true;
        }
    }
    if let Some(summary) = protocol.summary {
        protocol.expected_markers = summary.expected;
    }
    protocol
}

/// Value of ` key=value` in a whole protocol line.
///
/// The caller passes the full line, so the first field (right after the protocol
/// token) is found too - a parser that only looked at the text after the token
/// would silently count every line as "another test's line".
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Turn the process result and the parsed lines into one outcome.
fn classify(
    options: &SimOptions,
    outcome: &CommandOutcome,
    protocol: &Protocol,
) -> (Outcome, String) {
    if let Some(error) = &protocol.runner_error {
        return (
            Outcome::SimulationError,
            format!("the runner reported: {error}"),
        );
    }
    // A failed assertion is the strongest evidence: it is reported even when the
    // run also hit a deadline (REQ-TEST-008).
    if protocol.fails > 0 {
        return (
            Outcome::Fail,
            format!(
                "{} assertion failure(s), first: {}",
                protocol.fails, protocol.fail_lines[0]
            ),
        );
    }
    if !protocol.timeout_lines.is_empty() {
        return (
            Outcome::Timeout,
            format!("the testbench timeout fired: {}", protocol.timeout_lines[0]),
        );
    }
    if protocol.runner_timeout {
        return (
            Outcome::Timeout,
            "the run reached the event step limit without ending; the testbench never completed"
                .to_string(),
        );
    }
    if outcome.timed_out {
        return (
            Outcome::Timeout,
            format!(
                "the simulation did not finish within {}s and its process group was killed",
                options.timeout.as_secs()
            ),
        );
    }
    match outcome.status {
        Some(0) => {}
        Some(code) if code > 128 => {
            return (
                Outcome::SimulationError,
                format!(
                    "the simulation was killed by a signal (exit {code}) without a timeout marker, \
                     which can mean an external kill such as the out-of-memory killer; stderr: {}",
                    first_line(&outcome.stderr)
                ),
            )
        }
        None => {
            return (
                Outcome::SimulationError,
                format!(
                    "the simulation process died from a signal; stderr: {}",
                    first_line(&outcome.stderr)
                ),
            )
        }
        Some(code) => {
            return (
                Outcome::Fail,
                format!(
                    "the simulation exited with status {code}; stderr: {}",
                    first_line(&outcome.stderr)
                ),
            )
        }
    }
    if !protocol.has_pass() {
        return (
            Outcome::Fail,
            "the run ended without a single SHYHDL_PASS line, so it is not evidence of success \
             (REQ-TEST-008)"
                .to_string(),
        );
    }
    let Some(summary) = protocol.summary else {
        return (
            Outcome::Fail,
            "the run ended without a SHYHDL_SUMMARY line, so its result is incomplete".to_string(),
        );
    };
    if summary.pass != protocol.passes
        || summary.fail != protocol.fails
        || summary.markers != protocol.markers
    {
        return (
            Outcome::Fail,
            format!(
                "the summary disagrees with the observed lines (summary pass={} fail={} \
                 markers={}, observed pass={} fail={} markers={})",
                summary.pass,
                summary.fail,
                summary.markers,
                protocol.passes,
                protocol.fails,
                protocol.markers
            ),
        );
    }
    if let Some(expected) = protocol.expected_markers {
        if protocol.markers < expected {
            return (
                Outcome::Fail,
                format!(
                    "only {} of the {expected} explicitly marked cases completed; a case that never \
                     completed is not a pass",
                    protocol.markers
                ),
            );
        }
    }
    if !protocol.runner_done {
        return (
            Outcome::Fail,
            "the runner never reported that the simulation ended".to_string(),
        );
    }
    (
        Outcome::Pass,
        format!(
            "{} pass line(s), no failures, simulation ended normally",
            protocol.passes
        ),
    )
}

fn describe_status(outcome: &CommandOutcome) -> String {
    match outcome.status {
        Some(code) => format!("exit status {code}"),
        None => "killed by a signal".to_string(),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("(no output)")
        .trim()
        .to_string()
}

/// Reject anything that would escape the build directory.
///
/// Only plain file names are accepted: no separators, no `.`/`..`, nothing that
/// starts with `-` (which a tool could read as an option), no control characters.
fn validate_file_name(name: &str) -> Result<()> {
    let invalid = |reason: &str| {
        Err(Error::TypeLayout {
            message: format!(
                "`{name}` cannot be used as a file name in the build directory: {reason}"
            ),
            span: None,
        })
    };
    if name.is_empty() {
        return invalid("it is empty");
    }
    if name == "." || name == ".." {
        return invalid("it names a directory");
    }
    if name.contains('/') || name.contains('\\') {
        return invalid("path separators are not allowed, only a plain file name");
    }
    if name.starts_with('-') {
        return invalid("a leading `-` would be read as a command line option");
    }
    if name.chars().any(|character| character.is_control()) {
        return invalid("it contains a control character");
    }
    Ok(())
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The C++ runner with the model class substituted in.
fn runner_source(top_type: &str) -> String {
    include_str!("runner_main.cpp").replace("@@TOP_TYPE@@", top_type)
}

/// Build fingerprint: sources, runner, flags and the tool version. A run result
/// and a waveform are never reused, only the compiled model.
fn build_fingerprint(
    toolchain: &Toolchain,
    flags: &[String],
    runner: &str,
    dut_paths: &[PathBuf],
    tb_path: &Path,
) -> String {
    let mut units = Vec::new();
    for path in dut_paths
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(tb_path))
    {
        let name = file_name_of(path);
        let content = std::fs::read_to_string(path).unwrap_or_default();
        units.push(SourceUnit {
            path: format!("source/{name}"),
            content,
        });
    }
    units.push(SourceUnit {
        path: "runner.cpp".to_string(),
        content: runner.to_string(),
    });
    units.push(SourceUnit {
        path: "build-flags".to_string(),
        content: format!("{}\n{}", toolchain.verilator_version(), flags.join(" ")),
    });
    compute_build_id(&units)
}

/// A run id that is unique per invocation but stable in the manifest.
fn make_run_id(
    options: &SimOptions,
    runner: &str,
    dut_paths: &[PathBuf],
    tb_path: &Path,
) -> String {
    let mut units = Vec::new();
    for path in dut_paths
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(tb_path))
    {
        units.push(SourceUnit {
            path: format!("source/{}", file_name_of(path)),
            content: std::fs::read_to_string(path).unwrap_or_default(),
        });
    }
    units.push(SourceUnit {
        path: "runner.cpp".to_string(),
        content: runner.to_string(),
    });
    units.push(SourceUnit {
        path: "seed".to_string(),
        content: format!("{:?}", options.seed),
    });
    let digest = compute_build_id(&units);
    let tail = digest.rsplit('-').next().unwrap_or(&digest).to_string();
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("run-{seconds}-{tail}")
}

/// Time scale recorded in a VCD header, for example `1ps`.
fn read_vcd_timescale(path: &Path) -> Result<Option<String>> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let Some(start) = head.find("$timescale") else {
        return Ok(None);
    };
    let rest = &head[start + "$timescale".len()..];
    let Some(end) = rest.find("$end") else {
        return Ok(None);
    };
    let scale = rest[..end].trim();
    if scale.is_empty() {
        return Ok(None);
    }
    Ok(Some(scale.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_options() -> SimOptions {
        SimOptions {
            build_dir: PathBuf::from("."),
            dut_sources: vec![],
            tb_source: SourceFile::new("tb.v", ""),
            testbench: TestBenchInfo {
                name: "mine".into(),
                top: "tb".into(),
                top_type: "Vtb".into(),
                timeunit: "1ns".into(),
                timeprecision: "1ps".into(),
                roots: vec![],
                clocks: vec![],
            },
            design_top: "d".into(),
            design_top_source_name: "d".into(),
            design_build_id: None,
            metadata_file: None,
            wave: WaveOption::None,
            timeout: Duration::from_secs(1),
            build_timeout: Duration::from_secs(1),
            seed: None,
            parameters: vec![],
            sampling: String::new(),
            reuse_build: false,
        }
    }

    fn command_outcome(status: Option<i32>, timed_out: bool) -> CommandOutcome {
        CommandOutcome {
            status,
            timed_out,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
            command_line: "sim".into(),
        }
    }

    #[test]
    fn file_names_cannot_escape_the_build_directory() {
        assert!(validate_file_name("dut.v").is_ok());
        assert!(validate_file_name("../dut.v").is_err());
        assert!(validate_file_name("/tmp/dut.v").is_err());
        assert!(validate_file_name("sub/dut.v").is_err());
        assert!(validate_file_name("dut.v\nSHYHDL_PASS").is_err());
        assert!(validate_file_name("-o").is_err());
        assert!(validate_file_name("..").is_err());
    }

    #[test]
    fn protocol_lines_are_matched_to_the_test() {
        let stdout = "\
SHYHDL_PASS test=other case=1
SHYHDL_PASS test=mine case=1
SHYHDL_PASS test=mine case=2
SHYHDL_FAIL test=mine case=3 check=assert_eq file=a.shy line=4 expected=0 actual=1
SHYHDL_SUMMARY test=mine pass=3 fail=1 markers=2 expected_markers=2
";
        let protocol = parse_protocol(stdout, "mine");
        assert_eq!(protocol.passes, 2);
        assert_eq!(protocol.markers, 2);
        assert_eq!(protocol.fails, 1);
        assert_eq!(protocol.other_test_lines, 1);
        assert_eq!(protocol.expected_markers, Some(2));
    }

    #[test]
    fn summary_disagreement_is_not_a_pass() {
        let options = test_options();
        let outcome = command_outcome(Some(0), false);
        let stdout = "SHYHDL_PASS test=mine case=finish\n\
                      SHYHDL_SUMMARY test=mine pass=2 fail=0 markers=0 expected_markers=unknown\n\
                      SHYHDL_RUNNER_DONE sim_time=1 steps=1 finish=1\n";
        let (kind, reason) = classify(&options, &outcome, &parse_protocol(stdout, "mine"));
        assert_eq!(kind, Outcome::Fail, "{reason}");
        assert!(reason.contains("disagrees"), "{reason}");

        let stdout = "SHYHDL_PASS test=mine case=finish\n\
                      SHYHDL_SUMMARY test=mine pass=1 fail=0 markers=0 expected_markers=unknown\n\
                      SHYHDL_RUNNER_DONE sim_time=1 steps=1 finish=1\n";
        let (kind, reason) = classify(&options, &outcome, &parse_protocol(stdout, "mine"));
        assert_eq!(kind, Outcome::Pass, "{reason}");
    }

    #[test]
    fn missing_pass_marker_is_not_success() {
        let options = test_options();
        let outcome = command_outcome(Some(0), false);
        let stdout = "SHYHDL_SUMMARY test=mine pass=0 fail=0 markers=0 expected_markers=unknown\n\
                      SHYHDL_RUNNER_DONE sim_time=1 steps=1 finish=1\n";
        let (kind, reason) = classify(&options, &outcome, &parse_protocol(stdout, "mine"));
        assert_eq!(kind, Outcome::Fail, "{reason}");
        assert!(reason.contains("without a single SHYHDL_PASS"), "{reason}");
    }

    #[test]
    fn incomplete_cases_are_not_a_pass() {
        let options = test_options();
        let outcome = command_outcome(Some(0), false);
        let stdout = "SHYHDL_PASS test=mine case=1\n\
                      SHYHDL_SUMMARY test=mine pass=1 fail=0 markers=1 expected_markers=4\n\
                      SHYHDL_RUNNER_DONE sim_time=1 steps=1 finish=1\n";
        let (kind, reason) = classify(&options, &outcome, &parse_protocol(stdout, "mine"));
        assert_eq!(kind, Outcome::Fail, "{reason}");
        assert!(reason.contains("explicitly marked cases"), "{reason}");
    }

    #[test]
    fn fail_wins_over_a_timeout() {
        let options = test_options();
        let timed_out = command_outcome(Some(124), true);
        let stdout = "SHYHDL_FAIL test=mine case=1 check=assert file=a.shy line=3\n";
        let (kind, _) = classify(&options, &timed_out, &parse_protocol(stdout, "mine"));
        assert_eq!(kind, Outcome::Fail);

        let (kind, reason) = classify(&options, &timed_out, &parse_protocol("", "mine"));
        assert_eq!(kind, Outcome::Timeout, "{reason}");

        // A plain SIGKILL is not a timeout.
        let killed = command_outcome(Some(137), false);
        let (kind, reason) = classify(&options, &killed, &parse_protocol("", "mine"));
        assert_eq!(kind, Outcome::SimulationError, "{reason}");
        assert!(reason.contains("out-of-memory"), "{reason}");
    }
}
