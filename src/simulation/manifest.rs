//! The run manifest: what a recording belongs to, so replay can pair a waveform
//! with the design metadata it was produced from.
//!
//! Every path is relative to the directory that contains the manifest (the build
//! directory) and uses `/` as the separator. A consumer resolves
//! `manifest_dir.join(path)`, so a build directory can be moved or archived
//! without rewriting the manifest.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// File name of the manifest inside the build directory.
pub const MANIFEST_FILE: &str = "run.manifest.json";

/// How one DUT instance's design metadata maps onto the recorded waveform.
///
/// `source_root` is the metadata root of the design (the DUT top module as the
/// frontend named it), `wave_root` is the scope in the recording where that
/// instance was dumped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootMapping {
    /// Root of the design metadata this instance's signals hang below.
    pub source_root: String,
    /// Scope prefix in the waveform, for example `TOP.tb_reg8_hold.u_dut`.
    pub wave_root: String,
    /// Variable the test bound the instance to (`let mut <var> = Module::new()`).
    pub var: String,
    /// Verilog instance name inside the generated testbench (`u_<var>`).
    pub instance: String,
    /// Name of the instantiated module in the generated RTL.
    pub module: String,
    /// Source-level module name of the design.
    pub module_source_name: String,
}

/// A clock the testcase generated, identified inside the recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockRef {
    /// Variable the test bound the DUT to, which must match a `roots[].var`.
    pub var: String,
    /// Full waveform path of the clock, for example `TOP.tb_reg8_hold.u_dut.clk`.
    pub signal: String,
}

/// The design a recording belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignRef {
    /// Verilog module name of the design top.
    pub top: String,
    /// Source-level name of the design top.
    pub top_source_name: String,
}

/// The generated testbench of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRef {
    /// Name of the `#[hardware_test]` function.
    pub name: String,
    /// Verilog module name of the generated testbench.
    pub top: String,
    /// C++ model class the runner instantiated.
    pub top_type: String,
    /// Testbench time unit, for example `1ns`.
    pub timeunit: String,
    /// Testbench time precision, for example `1ps`.
    pub timeprecision: String,
}

/// Files of a run, relative to the manifest directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPaths {
    /// The manifest itself.
    pub manifest: String,
    /// DUT Verilog sources, in compile order.
    pub dut_verilog: Vec<String>,
    /// Generated testbench Verilog.
    pub tb_verilog: String,
    /// Frontend design metadata, when the caller supplied it.
    pub metadata: Option<String>,
    /// Verilator build output (stdout and stderr) of this run.
    pub build_log: String,
}

/// The recorded waveform of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaveRef {
    /// Waveform file, relative to the manifest directory.
    pub path: String,
    /// Format, `vcd` for this build.
    pub format: String,
    /// Time scale read from the recorded file's own header, for example `1ps`.
    pub timescale_in_file: Option<String>,
}

/// Version and flags of the simulation backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendRef {
    /// Backend name, `verilator` in this build.
    pub name: String,
    /// Backend version line.
    pub version: String,
    /// Flags the backend was invoked with, so a run can be repeated exactly.
    pub flags: Vec<String>,
}

/// One backend parameter of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterRef {
    /// Parameter name.
    pub name: String,
    /// Parameter value as text.
    pub value: String,
}

/// Observed result counts, taken from the protocol lines the testbench printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// `SHYHDL_PASS` lines (explicit markers plus the test-level completion).
    pub passes: u64,
    /// `SHYHDL_FAIL` lines.
    pub fails: u64,
    /// Explicit `pass!()` markers that completed.
    pub markers: u64,
    /// Markers the test declared as unconditionally reachable, when that is
    /// statically known.
    pub expected_markers: Option<u64>,
}

/// Everything a recording needs to be paired with a design and replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunManifest {
    /// Id of this run, unique per invocation.
    pub run_id: String,
    /// The design the recording belongs to.
    pub design: DesignRef,
    /// The testbench that was run.
    pub test: TestRef,
    /// Files of the run.
    pub paths: ManifestPaths,
    /// Waveform, when the run was asked for one and it exists.
    pub wave: Option<WaveRef>,
    /// How design metadata maps onto waveform scopes, one entry per DUT instance.
    pub roots: Vec<RootMapping>,
    /// Clocks the testcase generated, in declaration order; empty for a
    /// combinational test.
    pub clocks: Vec<ClockRef>,
    /// Simulation backend and its flags.
    pub backend: BackendRef,
    /// Random seed actually passed to the backend, or `None` when the backend
    /// default was used.
    pub seed: Option<u64>,
    /// Backend parameters of the run.
    pub parameters: Vec<ParameterRef>,
    /// Sampling strategy of the run, in words.
    pub sampling: String,
    /// Outcome: `pass`, `fail`, `timeout`, `environment`, `compile_error` or
    /// `simulation_error`.
    pub outcome: String,
    /// Exit code of the simulation process, when it exited normally.
    pub exit_code: Option<i32>,
    /// Result counts observed in the output.
    pub counts: Counts,
}

impl RunManifest {
    /// Serialize to the pretty JSON form stored next to a recording.
    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            action: "write",
            source,
        })
    }

    /// Parse a manifest written by [`RunManifest::to_json_string`].
    pub fn from_json_str(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|source| Error::Json {
            action: "read",
            source,
        })
    }
}

/// Read a manifest from disk.
pub fn load_manifest(path: &Path) -> Result<RunManifest> {
    let json = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    RunManifest::from_json_str(&json)
}

/// Write a manifest, returning the path it was written to.
pub fn write_manifest(manifest: &RunManifest, path: &Path) -> Result<PathBuf> {
    let json = manifest.to_json_string()?;
    std::fs::write(path, json).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(path.to_path_buf())
}
