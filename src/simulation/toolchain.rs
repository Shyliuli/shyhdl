//! Locating the external Verilog tools and running them safely.
//!
//! Two tools are needed and both are discovered explicitly instead of being
//! guessed:
//!
//! * `verilator` (the simulation backend, REQ-SIM-001) - from `$SHYHDL_VERILATOR`,
//!   then `$VERILATOR`, then `PATH`. No repository-relative or machine-specific
//!   path is baked in: where the tools live is a deployment decision.
//! * GNU coreutils `timeout` - used to bound both the build and the run. It runs
//!   the child in its own process group and signals the whole group, so a killed
//!   Verilator build leaves no orphaned `make`/`g++` behind, and its `--verbose`
//!   report is the evidence that a deadline (and not something else) ended the
//!   process. A missing or non-GNU `timeout` is an environment error: this build
//!   has no weaker fallback semantics.
//!
//! Every command is spawned with an argument vector, never a shell string, so a
//! build directory containing spaces or non-ASCII characters cannot change what
//! is executed (it can still break a tool that mishandles such paths; that
//! failure is reported with the tool's own output).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// The external tools a run needs, resolved once and reused.
#[derive(Debug, Clone)]
pub struct Toolchain {
    verilator: PathBuf,
    version: String,
    timeout: PathBuf,
    kill_after: Duration,
}

/// Result of one bounded external command.
#[derive(Debug, Clone)]
pub(crate) struct CommandOutcome {
    /// Exit code, or `None` when the process was killed by a signal.
    pub status: Option<i32>,
    /// Whether the process ended because our own deadline expired. This is taken
    /// from the deadline tool's own report of the signal it sent, so an unrelated
    /// SIGKILL (an out-of-memory kill, for instance) is never reported as a test
    /// timeout.
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    /// Human-readable command line, for diagnostics only.
    pub command_line: String,
}

impl CommandOutcome {
    pub(crate) fn success(&self) -> bool {
        self.status == Some(0)
    }
}

impl Toolchain {
    /// Find the tools, or report exactly what is missing and where it was looked
    /// for (an environment problem is reported as one, REQ-TEST-009).
    pub fn discover() -> Result<Self> {
        let verilator = discover_verilator()?;
        let output = Command::new(&verilator)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map_err(|source| Error::Io {
                path: verilator.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(Error::Io {
                path: verilator.clone(),
                source: std::io::Error::other(format!(
                    "`verilator --version` failed with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
            });
        }
        let version = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let timeout = discover_timeout()?;
        Ok(Self {
            verilator,
            version,
            timeout,
            kill_after: Duration::from_secs(5),
        })
    }

    /// Path of the Verilator executable that will be used.
    pub fn verilator(&self) -> &Path {
        &self.verilator
    }

    /// Version line of that Verilator (recorded so a run can be reproduced,
    /// REQ-SIM-009).
    pub fn verilator_version(&self) -> &str {
        &self.version
    }

    /// Path of the `timeout` executable used to bound builds and runs.
    pub fn timeout(&self) -> &Path {
        &self.timeout
    }

    /// Run a command under a deadline.
    ///
    /// The command is wrapped in `timeout --kill-after`, which puts the child in
    /// its own process group and signals the whole group, so a killed build
    /// takes its compiler children with it.
    pub(crate) fn run_bounded(
        &self,
        program: &Path,
        args: &[OsString],
        cwd: &Path,
        deadline: Duration,
    ) -> Result<CommandOutcome> {
        let timeout_seconds = deadline.as_secs().max(1);
        let kill_after = self.kill_after.as_secs().max(1);
        let mut command = Command::new(&self.timeout);
        command
            .arg(format!("--kill-after={kill_after}s"))
            .arg(format!("{timeout_seconds}s"))
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let command_line = render_command(program, args);
        let started = Instant::now();
        let output = command.output().map_err(|source| Error::Io {
            path: program.to_path_buf(),
            source,
        })?;
        let elapsed = started.elapsed();
        let status = output.status.code();
        // 124: `timeout` sent the signal after the deadline. 137: the child had
        // to be SIGKILLed, which with `--kill-after` also means the deadline was
        // reached. Any other signal death is left as a signal death so an
        // external kill (for example the OOM killer) is not mistaken for a
        // timeout.
        let timed_out = status == Some(124)
            || (status == Some(137) && elapsed + Duration::from_secs(1) >= deadline);
        Ok(CommandOutcome {
            status,
            timed_out,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            elapsed,
            command_line,
        })
    }
}

/// `$SHYHDL_VERILATOR`, then `$VERILATOR`, then `PATH`.
fn discover_verilator() -> Result<PathBuf> {
    for variable in ["SHYHDL_VERILATOR", "VERILATOR"] {
        if let Some(value) = std::env::var_os(variable) {
            let candidate = PathBuf::from(value);
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
            return Err(Error::Io {
                path: candidate,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("${variable} does not name an executable verilator"),
                ),
            });
        }
    }
    if let Some(found) = find_in_path("verilator") {
        return Ok(found);
    }
    Err(Error::Io {
        path: PathBuf::from("verilator"),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "verilator was not found; set $SHYHDL_VERILATOR to its path or put it on PATH \
             (a development kit usually exports this from its env.sh)",
        ),
    })
}

/// `$SHYHDL_TIMEOUT`, then `PATH`.
///
/// The tool must be GNU coreutils `timeout`: this build relies on `--kill-after`
/// and on the `--verbose` report of the signal it sent, which is the evidence
/// that separates "our deadline expired" from a program that exited by itself.
/// A different `timeout` is rejected instead of being used with weaker
/// semantics.
fn discover_timeout() -> Result<PathBuf> {
    let candidate = match std::env::var_os("SHYHDL_TIMEOUT") {
        Some(value) => PathBuf::from(value),
        None => find_in_path("timeout").unwrap_or_else(|| PathBuf::from("timeout")),
    };
    if !is_executable_file(&candidate) {
        return Err(Error::Io {
            path: candidate,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the GNU coreutils `timeout` command was not found (looked at $SHYHDL_TIMEOUT and \
                 PATH); it is required to bound builds and runs and to kill a timed out process \
                 group",
            ),
        });
    }
    let version = Command::new(&candidate)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Io {
            path: candidate.clone(),
            source,
        })?;
    let text = String::from_utf8_lossy(&version.stdout);
    if !version.status.success() || !text.contains("GNU coreutils") {
        return Err(Error::Io {
            path: candidate,
            source: std::io::Error::other(
                "this build needs GNU coreutils `timeout` (for `--kill-after` and `--verbose`); \
                 another implementation is not accepted because its timeout evidence would be \
                 weaker",
            ),
        });
    }
    Ok(candidate)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

fn render_command(program: &Path, args: &[OsString]) -> String {
    let mut rendered = program.display().to_string();
    for arg in args {
        rendered.push(' ');
        rendered.push_str(&arg.to_string_lossy());
    }
    rendered
}
