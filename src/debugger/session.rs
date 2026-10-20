//! Serving one recording to one real GDB, and naming the file GDB should open.
//!
//! The server owns the engine and the symbol layout, binds a loopback socket,
//! and answers RSP from [`crate::debugger::target::ReplayTarget`]. Only one GDB
//! is served at a time: the cursor is a single global position.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use gdbstub::conn::{Connection, ConnectionExt};
use gdbstub::stub::{run_blocking, DisconnectReason, GdbStub, SingleThreadStopReason};

use crate::debugger::symbols::{build_symbol_elf, SymbolLayout};
use crate::debugger::target::ReplayTarget;
use crate::debugger::DebugError;
use crate::metadata::DesignMetadata;
use crate::replay::ReplayEngine;
use crate::waveform::{SignalId, WaveError, Waveform};

/// Counter that makes each server's temporary symbol directory unique inside
/// one process.
fn next_session_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Which recording signal carries each debug-visible variable, and which
/// variables the recording does not contain.
#[derive(Debug, Clone, Default)]
pub struct SignalBinding {
    /// Metadata variable id -> recording signal.
    pub signal_of_var: HashMap<String, SignalId>,
    /// Variables with no signal in this recording. They get no symbol at all,
    /// so GDB reports them as unavailable instead of printing a zero.
    pub unavailable: Vec<String>,
}

/// Resolve every metadata variable against the recording, using the explicit
/// root mapping. A path is never guessed: only the declared mapping is applied,
/// and a strict full-path lookup decides availability.
pub fn bind_signals(
    meta: &DesignMetadata,
    waveform: &Waveform,
    root: &RootMapping,
) -> Result<SignalBinding, DebugError> {
    let mut binding = SignalBinding::default();
    for variable in &meta.variables {
        let path = root.waveform_path(&variable.instance_path)?;
        let full = if variable.rtl_name.is_empty() {
            path
        } else {
            format!("{path}.{}", variable.rtl_name)
        };
        match waveform.signal(&full) {
            Ok(signal) => {
                // A width mismatch means the metadata describes a different
                // design than the recording: padding or truncating would hand
                // GDB a value that was never simulated.
                let declared = variable.ty.bit_width()?;
                let recorded = waveform
                    .width(signal)
                    .map_err(|error| DebugError::Waveform(error.to_string()))?;
                if declared != recorded {
                    return Err(DebugError::WidthMismatch {
                        variable: variable.id.clone(),
                        path: full,
                        declared,
                        recorded,
                    });
                }
                binding.signal_of_var.insert(variable.id.clone(), signal);
            }
            Err(WaveError::SignalNotFound { .. }) => {
                binding.unavailable.push(variable.id.clone());
            }
            // An ambiguous path, a broken recording or anything else is a real
            // failure, not a signal that happens to be missing.
            Err(error) => return Err(DebugError::Waveform(error.to_string())),
        }
    }
    Ok(binding)
}

/// Where the design's names live inside the recorded RTL hierarchy.
///
/// The generated testbench wraps the design (`TOP.tb_foo.dut`), while the
/// metadata root is the design itself (`top`). The mapping is explicit, because
/// guessing a waveform path from a metadata path by suffix matching would
/// silently pick the wrong instance when two instances share a name
/// An instance path the mapping cannot place is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootMapping {
    /// Instance-path root used by the metadata, for example `top`.
    pub design_root: String,
    /// Signal-path root used by the recording, for example `TOP.tb_adder.dut`.
    pub waveform_root: String,
}

impl RootMapping {
    /// Build a mapping between two explicit roots.
    pub fn new(design_root: impl Into<String>, waveform_root: impl Into<String>) -> Self {
        Self {
            design_root: design_root.into(),
            waveform_root: waveform_root.into(),
        }
    }

    /// Identity mapping, for recordings whose hierarchy already matches the
    /// metadata (no testbench wrapper).
    pub fn identity(root: impl Into<String>) -> Self {
        let root = root.into();
        Self {
            design_root: root.clone(),
            waveform_root: root,
        }
    }

    /// Recording path for a metadata instance path.
    ///
    /// `top.u_adder.res` becomes `TOP.tb_adder.dut.u_adder.res` when the roots
    /// are `top` and `TOP.tb_adder.dut`.
    ///
    /// Only the declared root itself or a `.`-separated descendant of it is
    /// accepted: a bare prefix match would place `topology.u_x` under `top` and
    /// silently read the wrong signal.
    pub fn waveform_path(&self, instance_path: &str) -> Result<String, DebugError> {
        let reject = |reason: &str| DebugError::RootMapping {
            instance_path: instance_path.to_string(),
            design_root: self.design_root.clone(),
            reason: reason.to_string(),
        };
        if self.design_root.is_empty() || self.waveform_root.is_empty() {
            return Err(reject("the root mapping has an empty root"));
        }
        let rest = if instance_path == self.design_root {
            ""
        } else if let Some(rest) = instance_path.strip_prefix(&self.design_root) {
            rest.strip_prefix('.')
                .ok_or_else(|| reject("the instance path is not under the declared root"))?
        } else {
            return Err(reject("the instance path is not under the declared root"));
        };
        Ok(match rest {
            "" => self.waveform_root.clone(),
            rest => format!("{}.{}", self.waveform_root, rest),
        })
    }
}

/// Everything the caller may choose about a debugging session.
#[derive(Debug, Clone)]
pub struct DebugOptions {
    /// Loopback port; `0` (the default) lets the operating system choose one.
    pub port: u16,
    /// Mapping from metadata paths to recording paths.
    pub root: RootMapping,
    /// Where to write the debug ELF. Defaults to a file under the system
    /// temporary directory.
    pub symbol_elf: Option<PathBuf>,
}

impl DebugOptions {
    /// Defaults for a metadata root and a recording root.
    pub fn new(root: RootMapping) -> Self {
        Self {
            port: 0,
            root,
            symbol_elf: None,
        }
    }
}

/// A bound, not yet connected, GDB server for one recording.
pub struct DebugServer {
    listener: TcpListener,
    port: u16,
    engine: ReplayEngine,
    layout: SymbolLayout,
    signal_of_var: HashMap<String, SignalId>,
}

impl DebugServer {
    /// Bind a loopback socket for `engine`.
    ///
    /// `layout` must have been built by
    /// [`crate::debugger::symbols::build_symbol_elf`] with exactly one PC per
    /// observation point, in cursor order.
    pub fn start(
        engine: ReplayEngine,
        layout: SymbolLayout,
        signal_of_var: HashMap<String, SignalId>,
        options: &DebugOptions,
    ) -> Result<Self, DebugError> {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, options.port);
        let listener = TcpListener::bind(addr).map_err(|source| DebugError::Io {
            path: PathBuf::from(addr.to_string()),
            source,
        })?;
        let port = listener
            .local_addr()
            .map_err(|source| DebugError::Io {
                path: PathBuf::from(addr.to_string()),
                source,
            })?
            .port();
        Ok(Self {
            listener,
            port,
            engine,
            layout,
            signal_of_var,
        })
    }

    /// Build the symbol ELF and bind a socket, ready for one GDB.
    ///
    /// This is the whole CLI-facing path: resolve the metadata against the
    /// recording, write the synthetic debug ELF, and listen.
    pub fn build(
        engine: ReplayEngine,
        meta: &DesignMetadata,
        options: &DebugOptions,
    ) -> Result<Self, DebugError> {
        let waveform = engine.waveform();
        let binding = bind_signals(meta, waveform, &options.root)?;
        let mut recorded = meta.clone();
        recorded
            .variables
            .retain(|variable| binding.signal_of_var.contains_key(&variable.id));
        let elf_path = match &options.symbol_elf {
            Some(path) => path.clone(),
            None => {
                // One directory per server: two sessions (two runs of the same
                // design, or two runs of one process) must not overwrite each
                // other's symbols.
                let dir = std::env::temp_dir().join(format!(
                    "shyhdl-debug-{}-{}",
                    std::process::id(),
                    next_session_id()
                ));
                std::fs::create_dir_all(&dir).map_err(|source| DebugError::Io {
                    path: dir.clone(),
                    source,
                })?;
                dir.join("debug.elf")
            }
        };
        // The builder owns the addresses; these labels are the recorded times,
        // which is what GDB shows as the "source line" of each observation
        // point.
        let points = waveform.timestamps().to_vec();
        let layout = build_symbol_elf(&recorded, &points, &elf_path)?;
        Self::start(engine, layout, binding.signal_of_var, options)
    }

    /// Port GDB should connect to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The debug ELF GDB should open, so that `step` and `print` see the
    /// synthetic timeline and the typed signals.
    pub fn elf_path(&self) -> &Path {
        &self.layout.elf_path
    }

    /// GDB name of a metadata variable, so a caller can print the exact
    /// expression to type. Multi-instance designs get a unique name from the
    /// instance path; `None` means the variable has no symbol at all (it is not
    /// in the recording).
    pub fn symbol_name(&self, variable_id: &str) -> Option<&str> {
        self.layout
            .vars
            .iter()
            .find(|slot| slot.id == variable_id)
            .map(|slot| slot.symbol_name.as_str())
    }

    /// Command line that reproduces the session in a terminal.
    ///
    /// It starts by turning GDB's own memory and register writes off. A user
    /// who copies this line then gets an explicit reason the moment a write is
    /// attempted, instead of GDB's generic server error. That reason is GDB's
    /// ("Writing not allowed"), not the replay's: it is a client-side guard, and
    /// the server still refuses every write itself, which is what
    /// actually keeps the replay read-only.
    pub fn gdb_argv(&self) -> Vec<String> {
        vec![
            "gdb".to_string(),
            "-q".to_string(),
            self.layout.elf_path.display().to_string(),
            "-ex".to_string(),
            "set may-write-memory off".to_string(),
            "-ex".to_string(),
            "set may-write-registers off".to_string(),
            "-ex".to_string(),
            format!("target remote 127.0.0.1:{}", self.port),
        ]
    }

    /// Wait for one GDB, serve it until it disconnects, and return.
    pub fn serve_blocking(self) -> Result<(), DebugError> {
        self.accept_and_serve()
    }

    /// Serve one GDB at a time until the process ends, starting every session
    /// from the first observation point.
    ///
    /// A front end restarts a replay by reconnecting, so the listener has to
    /// outlive a session; the recording itself is untouched, only the cursor is
    /// rebuilt.
    pub fn serve_sessions(self) -> Result<(), DebugError> {
        loop {
            self.accept_and_serve()?;
        }
    }

    fn accept_and_serve(&self) -> Result<(), DebugError> {
        let (stream, _) = self.listener.accept().map_err(|source| DebugError::Io {
            path: PathBuf::from(format!("127.0.0.1:{}", self.port)),
            source,
        })?;
        stream.set_nodelay(true).map_err(|source| DebugError::Io {
            path: PathBuf::from("gdb connection"),
            source,
        })?;
        // A fresh cursor per session: a restart starts at the beginning, and no
        // state from the previous session (watchpoints, position) leaks in. The
        // recording is re-read rather than cloned, because the waveform owns the
        // decoder state; the file is the run's own artifact and does not change
        // while a session is being restarted.
        let recording = self.engine.waveform().path().to_path_buf();
        let waveform = crate::waveform::Waveform::load(&recording)
            .map_err(|error| DebugError::Waveform(error.to_string()))?;
        let engine = ReplayEngine::new(waveform, self.engine.mode().clone())
            .map_err(|error| DebugError::Waveform(error.to_string()))?;
        self.serve_stream(engine, stream)
    }

    fn serve_stream(&self, engine: ReplayEngine, stream: TcpStream) -> Result<(), DebugError> {
        let mut target = ReplayTarget::new(engine, self.layout.clone(), self.signal_of_var.clone())
            .map_err(|error| DebugError::Session(error.to_string()))?;
        let connection: Box<dyn ConnectionExt<Error = std::io::Error>> = Box::new(stream);
        let gdb = GdbStub::new(connection);

        match gdb.run_blocking::<ServerLoop>(&mut target) {
            Ok(DisconnectReason::Disconnect) => Ok(()),
            Ok(DisconnectReason::Kill) => Ok(()),
            Ok(DisconnectReason::TargetExited(code)) => Err(DebugError::Session(format!(
                "GDB reports exit status {code}"
            ))),
            Ok(DisconnectReason::TargetTerminated(signal)) => Err(DebugError::Session(format!(
                "GDB reports termination by signal {signal:?}"
            ))),
            Err(error) => Err(DebugError::Session(error.to_string())),
        }
    }
}

/// Drives `gdbstub`'s blocking loop for a target that never runs on its own:
/// the cursor either already moved (a pending stop) or nothing has happened and
/// the loop waits for the next packet from GDB.
enum ServerLoop {}

impl run_blocking::BlockingEventLoop for ServerLoop {
    type Target = ReplayTarget;
    type Connection = Box<dyn ConnectionExt<Error = std::io::Error>>;
    type StopReason = SingleThreadStopReason<u64>;

    fn wait_for_stop_reason(
        target: &mut Self::Target,
        conn: &mut Self::Connection,
    ) -> Result<
        run_blocking::Event<Self::StopReason>,
        run_blocking::WaitForStopReasonError<
            <Self::Target as gdbstub::target::Target>::Error,
            <Self::Connection as Connection>::Error,
        >,
    > {
        if let Some(reason) = target.take_pending() {
            return Ok(run_blocking::Event::TargetStopped(reason));
        }
        let byte = conn
            .read()
            .map_err(run_blocking::WaitForStopReasonError::Connection)?;
        Ok(run_blocking::Event::IncomingData(byte))
    }

    fn on_interrupt(
        _target: &mut Self::Target,
    ) -> Result<Option<Self::StopReason>, <Self::Target as gdbstub::target::Target>::Error> {
        Ok(Some(SingleThreadStopReason::Signal(
            gdbstub::common::Signal::SIGINT,
        )))
    }
}
