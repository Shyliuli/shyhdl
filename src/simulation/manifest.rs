//! The run manifest: what a recording belongs to, so replay can pair a waveform
//! with the design metadata it was produced from (REQ-SIM-009, REQ-DBG-010).
//!
//! # Path convention
//!
//! Every path in the manifest is **relative to the directory that contains the
//! manifest** (the build directory) and uses `/` as the separator. A consumer
//! resolves `manifest_dir.join(path)`. Nothing is absolute, so a build directory
//! can be moved or archived without rewriting the manifest.
//!
//! The manifest records the design build id produced by the frontend
//! ([`crate::metadata::DesignMetadata::build_id`]) and the run id of this run. It
//! never recomputes either: replay trusts the ids recorded here (REQ-DBG-010),
//! so recomputing them in a second place would let the two disagree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Wire format version of [`RunManifest`].
///
/// Version 2 binds a recording to its content through [`WaveRef::digest`]; a
/// version 1 manifest is refused instead of being read without that check.
/// Version 3 adds [`RunManifest::clocks`], the clocks the testcase generated,
/// which is what a debugger steps on now that a design port is not marked as a
/// clock any more (REQ-DBG-023).
pub const MANIFEST_SCHEMA: u32 = 3;

/// File name of the manifest inside the build directory.
pub const MANIFEST_FILE: &str = "run.manifest.json";

/// How one DUT instance's design metadata maps onto the recorded waveform.
///
/// `source_root` is the metadata root of the design (the DUT top module as the
/// frontend named it), `wave_root` is the scope in the recording where that
/// instance was dumped. A consumer replaces the metadata's root with
/// `wave_root`; it never guesses a suffix.
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
///
/// The clock is declared by the test, so the debugger never has to infer one from
/// a port marking or a name suffix (REQ-DBG-021, REQ-DBG-027). `signal` is the
/// full path of the clock in the waveform, below the `wave_root` of the instance
/// named by `var`.
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
    /// Build id from the frontend metadata, recorded as is; `None` when the
    /// caller did not supply metadata, which is reported rather than invented.
    pub build_id: Option<String>,
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
    /// Timestamps in the file are in this scale. `None` when the file could not
    /// be read back, which is reported as such instead of being assumed.
    pub timescale_in_file: Option<String>,
    /// Content digest of the recorded file, see [`digest_file`].
    ///
    /// This is the recording's own identity: a file name is reusable (two runs of
    /// the same design write the same `wave.vcd`), so a manifest must be paired
    /// with the exact recording it was written for. A consumer calls
    /// [`RunManifest::verify_wave`] before reading the waveform; that is the only
    /// check needed, and it is the same check for every consumer instead of one
    /// gate per consumer.
    pub digest: String,
}

/// Version and flags of the simulation backend (REQ-SIM-009).
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
    /// Wire format version, see [`MANIFEST_SCHEMA`].
    pub schema: u32,
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
    /// combinational test. A debugger may use the unique entry as its clock step
    /// mode (REQ-DBG-023), and must ask for one explicitly otherwise.
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

    /// Parse a manifest and refuse a different schema version.
    ///
    /// This only reads the wire format; [`RunManifest::validate`] additionally
    /// checks the fields a consumer relies on. Use [`load_manifest`] to do both.
    pub fn from_json_str(json: &str) -> Result<Self> {
        // The version is read from the header on its own, so a recording written by
        // an older schema is refused as a version problem. Deserializing the whole
        // manifest first would report a field that only the newer format has (a
        // missing `clocks`, for example) and hide the real reason.
        #[derive(Deserialize)]
        struct Header {
            schema: u32,
        }
        let header: Header = serde_json::from_str(json).map_err(|source| Error::Json {
            action: "read",
            source,
        })?;
        if header.schema != MANIFEST_SCHEMA {
            return Err(Error::SchemaVersion {
                expected: MANIFEST_SCHEMA,
                found: header.schema,
            });
        }
        // A version this build understands must carry every field it declares: a
        // `clocks` key missing from a schema 3 manifest is an error, not an empty
        // list.
        let manifest: Self = serde_json::from_str(json).map_err(|source| Error::Json {
            action: "read",
            source,
        })?;
        Ok(manifest)
    }

    /// Check the fields a consumer needs and the path convention.
    ///
    /// `design.build_id`, `paths.metadata` and `wave` may legitimately be absent
    /// (a hand-written Verilog fixture has no frontend metadata and no waveform),
    /// so their absence is not an error here: a consumer that needs a pair reports
    /// that itself. What is checked is what must always hold - a run id, a
    /// non-empty test and design identity, and every path relative to the
    /// manifest directory.
    pub fn validate(&self) -> Result<()> {
        let invalid = |message: String| Error::TypeLayout {
            message: format!("invalid run manifest: {message}"),
            span: None,
        };
        if self.run_id.trim().is_empty() {
            return Err(invalid("`run_id` is empty".to_string()));
        }
        if self.design.top.trim().is_empty() {
            return Err(invalid("`design.top` is empty".to_string()));
        }
        if self.test.name.trim().is_empty() || self.test.top.trim().is_empty() {
            return Err(invalid("`test.name` or `test.top` is empty".to_string()));
        }
        for (field, value) in [
            ("paths.manifest", &self.paths.manifest),
            ("paths.tb_verilog", &self.paths.tb_verilog),
            ("paths.build_log", &self.paths.build_log),
        ] {
            validate_relative_path(field, value).map_err(&invalid)?;
        }
        for (index, path) in self.paths.dut_verilog.iter().enumerate() {
            validate_relative_path(&format!("paths.dut_verilog[{index}]"), path)
                .map_err(&invalid)?;
        }
        if let Some(metadata) = &self.paths.metadata {
            validate_relative_path("paths.metadata", metadata).map_err(&invalid)?;
        }
        if let Some(wave) = &self.wave {
            validate_relative_path("wave.path", &wave.path).map_err(&invalid)?;
            if wave.format.trim().is_empty() {
                return Err(invalid("`wave.format` is empty".to_string()));
            }
        }
        for (index, root) in self.roots.iter().enumerate() {
            if root.var.trim().is_empty() {
                return Err(invalid(format!("roots[{index}].var is empty")));
            }
            // One entry per instance: a clock and a metadata mapping select a root
            // by `var`, so two entries with the same `var` would make that choice
            // silently ambiguous.
            if self.roots[..index]
                .iter()
                .any(|other| other.var == root.var)
            {
                return Err(invalid(format!(
                    "roots[{index}].var = `{}` is repeated; each instance has exactly one root",
                    root.var
                )));
            }
            if root.source_root.trim().is_empty() || root.wave_root.trim().is_empty() {
                return Err(invalid(format!(
                    "roots[{index}] has an empty `source_root` or `wave_root`"
                )));
            }
        }
        if let Some(wave) = &self.wave {
            if !wave.digest.starts_with(WAVE_DIGEST_PREFIX)
                || wave.digest.len() <= WAVE_DIGEST_PREFIX.len()
            {
                return Err(invalid(format!(
                    "`wave.digest` = `{}` is not a `{WAVE_DIGEST_PREFIX}` value",
                    wave.digest
                )));
            }
        }
        for (index, clock) in self.clocks.iter().enumerate() {
            if clock.var.trim().is_empty() || clock.signal.trim().is_empty() {
                return Err(invalid(format!(
                    "clocks[{index}] has an empty `var` or `signal`"
                )));
            }
            // The clock must belong to a recorded instance ...
            let Some(root) = self.roots.iter().find(|root| root.var == clock.var) else {
                return Err(invalid(format!(
                    "clocks[{index}].var = `{}` is not one of the roots ({}); a clock has to name \
                     the instance it was generated for",
                    clock.var,
                    if self.roots.is_empty() {
                        "none".to_string()
                    } else {
                        self.roots
                            .iter()
                            .map(|root| root.var.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            };
            // ... and be exactly one signal directly below that instance's scope.
            let prefix = format!("{}.", root.wave_root);
            let leaf = clock
                .signal
                .strip_prefix(&prefix)
                .filter(|leaf| !leaf.is_empty() && !leaf.contains('.') && !leaf.contains(' '));
            if leaf.is_none() {
                return Err(invalid(format!(
                    "clocks[{index}].signal = `{}` is not exactly `<signal>` below \
                     `{}` (the wave root of `{}`)",
                    clock.signal, root.wave_root, clock.var
                )));
            }
            // The same clock must not be recorded twice.
            if self.clocks[..index]
                .iter()
                .any(|other| other.var == clock.var && other.signal == clock.signal)
            {
                return Err(invalid(format!(
                    "clocks[{index}] repeats `{}` / `{}`",
                    clock.var, clock.signal
                )));
            }
        }
        Ok(())
    }

    /// Verify that the recording this manifest names really is the recording it
    /// was written for, and return its path.
    ///
    /// `manifest_dir` is the directory the manifest lives in; the waveform path is
    /// resolved inside it. A missing file, a path that would escape the directory,
    /// or a file whose [`digest_file`] differs from the recorded one is an error
    /// ([`Error::InvalidMetadata`], reported by the CLI as a refusal): a run of the
    /// same design writes the same file name again, so a recording can be replaced
    /// while an older manifest still looks valid by build id and path alone
    /// (REQ-DBG-010).
    pub fn verify_wave(&self, manifest_dir: &Path) -> Result<PathBuf> {
        let Some(wave) = &self.wave else {
            return Err(Error::TypeLayout {
                message: "this recording has no waveform, so there is nothing to replay"
                    .to_string(),
                span: None,
            });
        };
        validate_relative_path("wave.path", &wave.path).map_err(|message| Error::TypeLayout {
            message: format!("invalid run manifest: {message}"),
            span: None,
        })?;
        let path = manifest_dir.join(&wave.path);
        let found = digest_file(&path)?;
        if found != wave.digest {
            return Err(Error::BuildIdMismatch {
                expected: wave.digest.clone(),
                found,
            });
        }
        Ok(path)
    }
}

/// Prefix of a [`digest_file`] value, so a reader can tell which algorithm
/// produced it.
pub const WAVE_DIGEST_PREFIX: &str = "fnv1a64-";

/// FNV-1a 64-bit constants, the same pairing digest the build id uses.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Digest of a file's bytes: FNV-1a (64 bit), read in fixed-size chunks so a
/// large recording is never loaded into memory whole.
///
/// The value is `fnv1a64-` plus 16 lower-case hex digits over the raw file bytes,
/// in order. Like [`crate::metadata::compute_build_id`] this only pairs an
/// artifact with the run that produced it: it is not a security primitive and
/// makes no integrity promise, and a collision would at worst accept a mismatched
/// recording. It is deliberately simple and fully specified so any consumer
/// (including one that never links this crate) can recompute it.
pub fn digest_file(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hash = FNV_OFFSET_BASIS;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    Ok(format!("{WAVE_DIGEST_PREFIX}{hash:016x}"))
}

/// Every path in a manifest is relative to the manifest directory, so an
/// absolute path or a `..` component would escape the recording.
fn validate_relative_path(field: &str, value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("`{field}` is empty"));
    }
    if value.starts_with('/') || value.starts_with('\\') || value.contains(':') {
        return Err(format!(
            "`{field}` = `{value}` must be relative to the manifest directory"
        ));
    }
    if value.split(['/', '\\']).any(|component| component == "..") {
        return Err(format!("`{field}` = `{value}` must not contain `..`"));
    }
    Ok(())
}

/// Read a manifest from disk and validate it.
///
/// A consumer calls this instead of deserializing by hand so the schema version
/// and the required fields are checked in exactly one place.
pub fn load_manifest(path: &Path) -> Result<RunManifest> {
    let json = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest = RunManifest::from_json_str(&json)?;
    manifest.validate()?;
    Ok(manifest)
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
