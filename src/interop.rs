//! Rust-interop surface: run the same design flow from ordinary Rust code.
//!
//! ShyHDL stands for "shy hardware description": HDL that looks like Rust. It
//! works with source text and a CLI, but the same compiler is a library, and
//! this module is the door for Rust code that already *is* Rust — no separate
//! source file, no shelling out.
//!
//! Three pieces make that work:
//!
//! * The attributes [`hardware_instance`](crate::hardware_instance),
//!   [`hardware`](crate::hardware) and [`hardware_test`](crate::hardware_test)
//!   mark the items that belong to a design. The macros compile to nothing but a
//!   registration: the item itself stays a normal Rust item.
//! * [`source_for`] reads the marked items back out of the crate source, which is
//!   what the frontend compiles. That is why the same design works both as a
//!   `.shy.rs` file and as a Rust module.
//! * [`check`], [`build`], [`list_tests`] and [`test()`] drive the flow for one
//!   registered design type, and [`run`] hands the process to the ordinary CLI.
//!
//! Widths are Rust types with const parameters, so `U<8>` is an eight-bit
//! unsigned value and `I<4>` a four-bit two's-complement one; the frontend maps
//! them onto the `uint<8>` / `int<4>` it understands.

use std::any::type_name;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::frontend::{self, CompiledDesign, PortDir};
use crate::simulation::{Outcome, SimOptions, Simulator as CoreSimulator, SourceFile, WaveOption};
use crate::testbench::{self, DesignInfo, ModulePort, PortDirection};

/// An unsigned hardware value of a fixed bit width.
#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct U<const W: usize>;

/// A two's-complement signed hardware value of a fixed bit width.
#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct I<const W: usize>;

/// A memory resource: `N` elements of `T`, addressed by `read` / `write`.
///
/// This is not a value type and not a marker for one: a field of this type is a
/// separate resource of the module (an unpacked `reg [W-1:0] mem [0:N-1]` in the
/// generated Verilog), so it has no bit width to flatten into the packed value
/// types. Declaring it is the only place it may appear; `read(addr)` is an
/// expression and `write(addr, data)` is a statement that belongs in the clock
/// arm of `always!`. The methods exist so the macros and rust-analyzer accept
/// those calls; the frontend is what gives them hardware meaning.
pub struct Mem<T, const N: usize> {
    _element: std::marker::PhantomData<T>,
}

impl<T, const N: usize> Mem<T, N> {
    /// Read the element at `addr`.
    pub fn read<A>(&self, _addr: A) -> T {
        unreachable!("memories are compiled by ShyHDL, not executed by Rust")
    }

    /// Write `data` to `addr`. A write is only meaningful in the clock arm of
    /// `always!`, where it becomes a non-blocking assignment.
    pub fn write<A>(&mut self, _addr: A, _data: T) {
        unreachable!("memories are compiled by ShyHDL, not executed by Rust")
    }
}

/// The hardware behaviour of a design: what its combinational and clocked
/// logic does.
///
/// [`hardware_instance`](crate::hardware_instance) declares the ports of a
/// design; this trait carries the body. It is empty on purpose — the body is
/// read from the source by the frontend, not executed by Rust — so that an
/// `impl Hardware for T` is accepted by the compiler while saying nothing it
/// cannot check.
pub trait Hardware {
    /// Describe the behaviour of this design.
    ///
    /// The body is ShyHDL source: the frontend reads it from the file, so it
    /// runs during `check` / `build` / `test`, not when Rust calls this method.
    fn run(&mut self) {}
}

/// Which kind of design item a registration describes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PartKind {
    /// A design with ports, declared with `#[hardware_instance]`.
    Instance,
    /// A data type, for example an `enum` carrying a payload.
    Data,
    /// An `enum` declaration.
    Enum,
    /// A combinational function, declared with `#[hardware_instance]` on a
    /// `fn`.
    Function,
    /// The behaviour of a design, declared with `#[hardware]`.
    Impl,
    /// A test, declared with `#[hardware_test]`.
    Test,
}

/// One design item, registered at compile time by the attributes.
///
/// A user never writes this: the macros submit one entry per marked item, and
/// the entries are what [`source_for`] stitches back into compilable source.
#[derive(Debug)]
pub struct RegisteredPart {
    /// What kind of item this is.
    pub kind: PartKind,
    /// Item name as written in the source.
    pub name: &'static str,
    /// File the item came from, as the compiler saw it.
    pub source: &'static str,
    /// File the item came from, as written in the source tree.
    pub file: &'static str,
    /// Line the item starts on (1-based).
    pub line: u32,
}

inventory::collect!(RegisteredPart);

/// The registration macro, re-exported for the attribute macros.
///
/// The attributes expand to `::shyhdl::__inventory::submit! { … }`, so this
/// re-export is the stable path they can rely on regardless of how the user's
/// crate names its dependencies.
pub use inventory as __inventory;

/// The source text of every registered part, stitched back into one design.
///
/// Order matters: a type has to be defined before the `impl` that uses it, so
/// definitions (`Instance`, `Data`, `Enum`) come first and behaviours (`Impl`,
/// `Test`) after, each group ordered by `(file, line)`. Whole files are read
/// once and joined, which keeps line numbers meaningful inside each unit.
fn registered_source() -> Result<String> {
    let mut parts: Vec<&'static RegisteredPart> = inventory::iter::<RegisteredPart>().collect();
    if parts.is_empty() {
        return Err(Error::Unavailable {
            message: "no design registered: annotate an item with #[hardware_instance]",
        });
    }
    // Definitions come before behaviour, so an impl is always compiled with the
    // struct or enum it names already in scope. Within one group the source
    // order is kept, which is what makes diagnostics point where the user wrote.
    parts.sort_by_key(|part| (definition_rank(part.kind), part.file, part.line));

    let mut text = String::new();
    for part in parts {
        text.push_str(&format!("// {}:{}\n", part.file, part.line));
        text.push_str(part.source);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    Ok(text)
}

/// Ordering key that puts definitions before the items that use them.
fn definition_rank(kind: PartKind) -> u8 {
    match kind {
        PartKind::Instance | PartKind::Data | PartKind::Enum | PartKind::Function => 0,
        PartKind::Impl | PartKind::Test => 1,
    }
}

/// Where a registered part came from, as `file:line`.
///
/// Handy in reports and in test assertions; `T` is the design type, so
/// `source_for::<Adder4>()` names the module that declares it.
pub fn source_for<T: 'static>() -> String {
    let _ = type_name::<T>();
    registered_source().unwrap_or_default()
}

/// Span of the first registered part, so diagnostics point into the user's file
/// rather than at a joined copy.
fn source_name() -> String {
    match inventory::iter::<RegisteredPart>.into_iter().next() {
        Some(part) if !part.file.is_empty() => part.file.to_string(),
        Some(part) => part.source.to_string(),
        None => String::from("<registered design>"),
    }
}

/// The top module name to compile for a design type.
///
/// A design type's ports come from its `#[hardware_instance]` struct, so the
/// struct's name is the module the frontend should build.
fn top_name<T: 'static>() -> Result<String> {
    let design = type_name::<T>();
    let short = design
        .rsplit("::")
        .next()
        .unwrap_or(design)
        .split('<')
        .next()
        .unwrap_or(design);
    for part in inventory::iter::<RegisteredPart> {
        if part.kind == PartKind::Instance && part.name == short {
            return Ok(part.name.to_string());
        }
    }
    Err(Error::Unavailable {
        message: "no #[hardware_instance] registered for this type",
    })
}

/// Compile the registered design of type `T` and report the result.
///
/// The design is compiled exactly as `build` would compile it; nothing is
/// written and nothing is printed. Diagnostics go to stderr, so a failing check
/// looks the same in Rust and on the command line.
pub fn check<T: 'static>() -> Result<()> {
    let compiled = compile_design::<T>()?;
    eprintln!(
        "{}: ok, top {} ({} modules)",
        source_name(),
        compiled.top().verilog_name,
        compiled.design.modules().len()
    );
    Ok(())
}

/// The Verilog text of the registered design of type `T`.
///
/// Nothing is written: the caller owns the string, so it can print it, embed it
/// or hand it to another tool.
pub fn verilog<T: 'static>() -> Result<String> {
    Ok(compile_design::<T>()?.verilog)
}

/// The Verilog text of one module of the registered design, by module name.
///
/// The design is emitted as one file with every module in it; this returns the
/// `module`.. `endmodule` block for `module`, so a caller can export exactly the
/// instance it is interested in.
pub fn module_verilog<T: 'static>(module: &str) -> Result<String> {
    let text = verilog::<T>()?;
    extract_module(&text, module).ok_or(Error::Unavailable {
        message: "no module with that name is part of this design",
    })
}

/// The Verilog text of one generated testbench.
///
/// `test` names a `#[hardware_test]` function; the string is the standalone
/// testbench that instantiates the DUT, exactly what `test` would run.
pub fn test_verilog<T: 'static>(test: &str) -> Result<String> {
    let source = registered_source()?;
    let compiled = compile_design::<T>()?;
    let design = design_info(&compiled);
    let artifact = testbench::compile_testbench(&source, &source_name(), test, &design)?;
    Ok(artifact.verilog)
}

/// One `module`.. `endmodule` block, or `None` when the name is not there.
fn extract_module(text: &str, module: &str) -> Option<String> {
    let header = format!("module {module}");
    let start = text.find(&header)?;
    let rest = &text[start..];
    let end = rest.find("endmodule")? + "endmodule".len();
    Some(rest[..end].to_string())
}

/// Compile the registered design of type `T` and write its two artifacts.
///
/// Returns the paths written, Verilog first, so a caller can keep them: for
/// `build::<Adder4>(out)` in `out`, `<top>.v` and `<top>.meta.json`.
pub fn build<T: 'static>(out: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let compiled = compile_design::<T>()?;
    let out = out.as_ref();
    fs::create_dir_all(out).map_err(|source| Error::Io {
        path: out.to_path_buf(),
        source,
    })?;
    let top = compiled.top().verilog_name.clone();
    let verilog_path = out.join(format!("{top}.v"));
    let metadata_path = out.join(format!("{top}.meta.json"));
    write_file(&verilog_path, &compiled.verilog)?;
    write_file(&metadata_path, &compiled.metadata.to_json_string()?)?;
    Ok(vec![verilog_path, metadata_path])
}

/// Names of the tests registered for the design of type `T`.
///
/// Only the tests declared with `#[hardware_test]` in this crate are listed;
/// their time units and locations are what `list-tests` reports too.
pub fn list_tests<T: 'static>() -> Vec<String> {
    let source = match registered_source() {
        Ok(source) => source,
        Err(_) => return Vec::new(),
    };
    match testbench::list_tests(&source, &source_name()) {
        Ok(tests) => tests.into_iter().map(|test| test.name).collect(),
        Err(_) => Vec::new(),
    }
}

/// Generate, build and run one registered test.
///
/// `T` names the design, `name` the test. The run writes Verilog, the testbench,
/// the metadata, the waveform and the run manifest into `out`, exactly like
/// `test` on the command line, and returns the classified outcome.
pub fn test<T: 'static>(name: &str, out: impl AsRef<Path>) -> Result<Outcome> {
    let source = registered_source()?;
    let source_name = source_name();
    let compiled = compile_design::<T>()?;
    let design = design_info(&compiled);

    let artifact = testbench::compile_testbench(&source, &source_name, name, &design)?;

    let simulator = CoreSimulator::discover()?;
    let out = out.as_ref();
    fs::create_dir_all(out).map_err(|source| Error::Io {
        path: out.to_path_buf(),
        source,
    })?;
    let top = compiled.design.top().verilog_name.clone();
    let metadata_name = format!("{top}.meta.json");
    write_file(
        &out.join(&metadata_name),
        &compiled.metadata.to_json_string()?,
    )?;

    let mut options = SimOptions::for_testbench(
        out.to_path_buf(),
        &artifact,
        &design,
        vec![SourceFile::new(
            format!("{top}.v"),
            compiled.verilog.clone(),
        )],
    );
    options.metadata_file = Some(metadata_name);
    options.timeout = Duration::from_secs(60);
    options.seed = Some(0);
    options.wave = WaveOption::vcd();

    let report = simulator.run(&options)?;
    eprintln!("{}", report.summary_line());
    Ok(report.outcome)
}

/// Put the design on the NVBoard virtual board and run it until the window is
/// closed.
///
/// Switches drive the design's inputs, LEDs show its outputs, and a
/// clock-looking input (`clk`, `clock`, `clk_i`, `clock_i`) is toggled by the
/// frame loop. Only available with the `nvboard` feature; the board sources are
/// vendored, so nothing else has to be fetched.
#[cfg(feature = "nvboard")]
pub fn board<T: 'static>(out: impl AsRef<Path>) -> Result<crate::nvboard::BoardRun> {
    board_impl::<T>(out.as_ref(), None)
}

/// Put the design on the board with an explicit port-to-pin table.
///
/// `bindings` maps each top-level port to the first pin of its group; a
/// multi-bit port takes consecutive pins from there, and `SEG0`..`SEG7` name
/// the eight 7-seg digits. The clock (`clk`/`clock`/`clk_i`/`clock_i`) is still
/// toggled by the frame loop and must not be listed. Every other port has to be
/// bound; the array is the whole plan, nothing is inferred from names or order.
#[cfg(feature = "nvboard")]
pub fn board_with<T: 'static>(
    out: impl AsRef<Path>,
    bindings: &[(&str, &str)],
) -> Result<crate::nvboard::BoardRun> {
    let bindings = bindings
        .iter()
        .map(|(port, pin)| ((*port).to_string(), (*pin).to_string()))
        .collect();
    board_impl::<T>(out.as_ref(), Some(bindings))
}

#[cfg(feature = "nvboard")]
fn board_impl<T: 'static>(
    out: &Path,
    bindings: Option<Vec<(String, String)>>,
) -> Result<crate::nvboard::BoardRun> {
    let compiled = compile_design::<T>()?;
    let top = compiled.design.top();
    let ports = top
        .ports
        .iter()
        .map(|port| {
            Ok(crate::nvboard::BoardPort::new(
                port.verilog_name.clone(),
                port.ty.bit_width()?,
                matches!(port.dir, PortDir::In),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let build_dir = out.join("board");
    crate::nvboard::run(&crate::nvboard::BoardOptions {
        build_dir,
        top: top.verilog_name.clone(),
        verilog: compiled.verilog,
        ports,
        clock: None,
        bindings,
        announce: true,
    })
}

/// Hand the process to the ordinary command-line interface.
///
/// `main` can be `shyhdl::run()`: the same subcommands and the same exit codes as
/// the `shyhdl` binary, so a program that also wants the CLI keeps one entry
/// point.
///
/// The binary owns the clap definition, so the library cannot reach it without a
/// second copy of the whole command surface. Instead of duplicating it, `run`
/// executes the installed binary in place of this process when the current
/// executable is not the binary itself, which keeps both entry points behaving
/// identically.
pub fn run() -> ! {
    let status = std::process::Command::new(shyhdl_binary())
        .args(std::env::args_os().skip(1))
        .status();
    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("shyhdl: cannot start the `shyhdl` binary: {error}");
            std::process::exit(1)
        }
    }
}

/// Path of the `shyhdl` binary that ships beside this library.
///
/// Falls back to the plain name so a `PATH` install keeps working.
fn shyhdl_binary() -> std::ffi::OsString {
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.with_file_name(format!("shyhdl{}", std::env::consts::EXE_SUFFIX));
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    std::ffi::OsString::from("shyhdl")
}

/// Compile the registered design, sharing one code path with `check`.
fn compile_design<T: 'static>() -> Result<CompiledDesign> {
    let source = registered_source()?;
    let top = top_name::<T>()?;
    frontend::compile(&source, &source_name(), &top).map_err(|diagnostic| Error::Unsupported {
        span: diagnostic
            .span
            .clone()
            .unwrap_or_else(|| crate::metadata::SourceSpan::new(source_name(), 1, 1)),
        message: diagnostic.message,
    })
}

/// Ports of the compiled design, in the shape the testbench generator wants.
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
                        PortDir::In => PortDirection::In,
                        PortDir::Out => PortDirection::Out,
                    },
                    port.ty.clone(),
                )
            })
            .collect(),
    )
}

/// Write one artifact, reporting the path on failure.
fn write_file(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------
// Object-shaped API: the same flow as a value you can hold.
// ---------------------------------------------------------------------------

/// The test a caller wants to run, named by the placeholder function itself.
///
/// A `#[hardware_test]` function is registered as a [`TestId`]-valued function,
/// so `sim.run(adds)` names the test without a string that could drift from the
/// source.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TestId(pub &'static str);

/// What one run produced, in structured form.
///
/// The CLI prints this; a Rust caller decides what to do with it.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// How the run was classified.
    pub outcome: Outcome,
    /// One-line reason for that classification.
    pub reason: String,
    /// Name of the test that ran.
    pub test_name: String,
    /// Waveform written by the run, when one was recorded.
    pub wave_path: Option<PathBuf>,
    /// Manifest pairing the recording with this build.
    pub manifest_path: PathBuf,
    /// Directory the artifacts went into.
    pub out: PathBuf,
    /// Path of the design metadata written for this run.
    pub metadata_path: PathBuf,
}

/// A design type prepared for simulation.
///
/// Built by [`simulate`]; holds the compiled design and where artifacts go, so
/// several tests can run against one compilation.
pub struct Simulator {
    top: String,
    out: PathBuf,
    design: DesignInfo,
    source: String,
    source_name: String,
    verilog: String,
    metadata: crate::metadata::DesignMetadata,
}

/// Compile the registered design of type `T` and prepare to run its tests.
///
/// The compilation happens here, once: [`Simulator::run`] can then be called for
/// each test without recompiling the design.
pub fn simulate<T: 'static>(out: impl AsRef<Path>) -> Result<Simulator> {
    let compiled = compile_design::<T>()?;
    let design = design_info(&compiled);
    Ok(Simulator {
        top: compiled.design.top().verilog_name.clone(),
        out: out.as_ref().to_path_buf(),
        design,
        source: registered_source()?,
        source_name: source_name(),
        verilog: compiled.verilog,
        metadata: compiled.metadata,
    })
}

impl Simulator {
    /// Generate, build and run one test, returning the structured result.
    ///
    /// `test` is the placeholder function of the test to run, so
    /// `sim.run(adds)` cannot name a test that does not exist without the call
    /// failing to compile.
    pub fn run<F: FnOnce() -> TestId>(&mut self, test: F) -> Result<RunResult> {
        let TestId(name) = test();
        let artifact =
            testbench::compile_testbench(&self.source, &self.source_name, name, &self.design)?;

        let simulator = CoreSimulator::discover()?;
        fs::create_dir_all(&self.out).map_err(|source| Error::Io {
            path: self.out.clone(),
            source,
        })?;
        let metadata_name = format!("{}.meta.json", self.top);
        let metadata_path = self.out.join(&metadata_name);
        write_file(&metadata_path, &self.metadata.to_json_string()?)?;

        let mut options = SimOptions::for_testbench(
            self.out.clone(),
            &artifact,
            &self.design,
            vec![SourceFile::new(
                format!("{}.v", self.top),
                self.verilog.clone(),
            )],
        );
        options.metadata_file = Some(metadata_name);
        options.timeout = Duration::from_secs(60);
        options.seed = Some(0);
        options.wave = WaveOption::vcd();

        let report = simulator.run(&options)?;
        Ok(RunResult {
            outcome: report.outcome,
            reason: report.reason,
            test_name: report.test_name,
            wave_path: report.wave_path,
            manifest_path: report.manifest_path,
            out: self.out.clone(),
            metadata_path,
        })
    }

    /// Directory the artifacts of this simulator go into.
    pub fn out(&self) -> &Path {
        &self.out
    }

    /// Verilog module name of the design being simulated.
    pub fn top(&self) -> &str {
        &self.top
    }
}

/// A GDB server for the recording of one run, listening on loopback.
///
/// [`Debugger::new`] takes the step of the replay timeline — how far one `step`
/// moves, in nanoseconds — and binds a free loopback port,
/// so both are known before the session starts and can be printed or handed to a
/// tool. [`Debugger::run`] then serves GDB sessions until the process ends,
/// restarting every session at the first observation point.
pub struct Debugger {
    /// Loopback port the server listens on.
    pub port: u16,
    server: crate::debugger::DebugServer,
}

impl Debugger {
    /// Prepare a session for the recording a run wrote, stepping `step_ns`
    /// nanoseconds at a time (`10` is a 10 ns grid).
    ///
    /// There is no default step: a step is a property of the replay a caller
    /// asks for — one clock period, one time step, whatever the recording is
    /// being read for — and a constant here would silently decide it.
    ///
    /// The manifest the run wrote is the pairing: it names the metadata file and
    /// the recording root of every DUT instance, which is what the server needs
    /// to bind symbols to signals.
    pub fn new(result: &RunResult, step_ns: u32) -> Result<Debugger> {
        let manifest = crate::simulation::load_manifest(&result.manifest_path)?;
        let wave = manifest.wave.as_ref().ok_or(Error::Unavailable {
            message: "this run recorded no waveform, so there is nothing to replay",
        })?;
        let wave_path = PathBuf::from(&wave.path);
        let recording = if wave_path.is_absolute() {
            wave_path
        } else {
            result.out.join(&wave.path)
        };

        let metadata_file = manifest
            .paths
            .metadata
            .as_ref()
            .map(|name| result.out.join(name))
            .unwrap_or_else(|| result.metadata_path.clone());
        let metadata = crate::metadata::DesignMetadata::from_json_str(
            &fs::read_to_string(&metadata_file).map_err(|source| Error::Io {
                path: metadata_file.clone(),
                source,
            })?,
        )?;

        let root = manifest
            .roots
            .first()
            .map(|root| {
                crate::debugger::RootMapping::new(root.source_root.clone(), root.wave_root.clone())
            })
            .ok_or(Error::Unavailable {
                message: "the run manifest records no DUT root, so there is nothing to bind",
            })?;

        let waveform = crate::waveform::Waveform::load(&recording).map_err(|error| Error::Io {
            path: recording.clone(),
            source: std::io::Error::other(error.to_string()),
        })?;
        // The step is the caller's, converted into the recording's own ticks. A
        // step the timescale cannot express is reported, not rounded away: the
        // grid a caller asks for is the grid they get.
        let step =
            crate::waveform::Timescale::new(u64::from(step_ns), crate::waveform::TimeUnit::Nano);
        let period = waveform.ticks(step).map_err(|error| Error::Io {
            path: recording.clone(),
            source: std::io::Error::other(error.to_string()),
        })?;
        let step = crate::replay::StepMode::Interval { origin: 0, period };
        let engine =
            crate::replay::ReplayEngine::new(waveform, step).map_err(|error| Error::Io {
                path: recording.clone(),
                source: std::io::Error::other(error.to_string()),
            })?;
        let options = crate::debugger::DebugOptions::new(root);
        let server =
            crate::debugger::DebugServer::build(engine, &metadata, &options).map_err(|error| {
                Error::Io {
                    path: metadata_file.clone(),
                    source: std::io::Error::other(error.to_string()),
                }
            })?;
        Ok(Debugger {
            port: server.port(),
            server,
        })
    }

    /// Serve GDB sessions one at a time until the process ends, each starting
    /// from the first observation point.
    ///
    /// Reconnecting is how a front end restarts a replay, so the listener stays
    /// up; the recording itself is only read.
    pub fn run(self) -> Result<()> {
        let elf = self.server.elf_path().to_path_buf();
        let server = self.server;
        server.serve_sessions().map_err(|error| Error::Io {
            path: elf,
            source: std::io::Error::other(error.to_string()),
        })?;
        Ok(())
    }

    /// Path of the symbol ELF the session exposes to GDB.
    pub fn elf_path(&self) -> &Path {
        self.server.elf_path()
    }

    /// The command line that reproduces this session in a terminal.
    pub fn gdb_argv(&self) -> Vec<String> {
        self.server.gdb_argv()
    }
}
