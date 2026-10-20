//! Real GDB integration for record/replay debugging.
//!
//! A recording is replayed read-only, and standard GDB commands are given the
//! semantics they get here:
//!
//! | GDB command | recording semantics |
//! | --- | --- |
//! | `step`, `next` | one reference point (clock edge or grid point), earlier if a watched signal changed in between |
//! | `reverse-step` | the previous reference point and nothing else |
//! | `continue` | the earliest hit or the end of the recording |
//! | `watch` | every recorded change of that signal, including changes between reference points |
//! | `print`, `ptype` | typed values of the current observation point |
//! | `break` | an observation-point breakpoint; with a condition GDB evaluates it at every recorded point |
//! | `set var`, any write | refused with an explicit error |
//!
//! # How GDB is made to see a timeline
//!
//! [`symbols::build_symbol_elf`] writes a synthetic debug ELF: one address per
//! observation point, each on its own source line, plus a `DW_TAG_variable` per
//! recorded signal. GDB is started on that ELF with `target remote`, and
//! [`target::ReplayTarget`] answers RSP from the [`crate::replay::ReplayEngine`]
//! cursor. Nothing is simulated again and nothing is written.
//!
//! Two facts about GDB's behaviour are load bearing, and both were measured
//! against GDB 17.2 rather than assumed:
//!
//! * A source-level `step` stops only when the new PC is the first address of a
//!   line (`sal.pc`), the line changed, and the line-table entry is a statement
//!   (`infrun.c` `process_event_stop_test`). Hence one address and one
//!   `is_stmt` line per observation point, and one `step` advances exactly one
//!   address.
//! * A watchpoint hit during a `step` is reported by the target at the
//!   observation point where the value changed, in the same resume: measured on
//!   this x86-64 target, `gdbarch_have_nonsteppable_watchpoint` is false, so
//!   `infrun.c`'s "step over this watchpoint" branch does not apply and the
//!   cursor must not be frozen around it. (On loongarch64 GDB simulates the
//!   instruction instead of using `vCont;s` and does take that branch, which is
//!   one reason the default target here is x86-64.)
//!
//! The default architecture is a virtual x86-64 target: GDB has no software
//! single-step for it, so `step` is one `vCont;s` packet and the cursor decides
//! where the timeline goes, instead of GDB simulating instructions.

pub mod arch;
pub mod session;
pub mod symbols;
pub mod target;

pub use arch::X86_64;
pub use session::{DebugOptions, DebugServer, RootMapping};
pub use symbols::SymbolLayout;
pub use target::ReplayTarget;

/// Failure of the debugger's public surface.
///
/// The target's non-fatal answers (a refused write, an address that cannot be
/// read here) stay inside the GDB session and never surface as this error.
#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    /// Filesystem or socket failure, always carrying the path being touched.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// Path or address that was being used.
        path: std::path::PathBuf,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },
    /// An instance path the explicit root mapping cannot place. Guessing from a
    /// suffix would silently pick one of two same-named instances.
    #[error("instance path {instance_path} is not under design root {design_root}: {reason}")]
    RootMapping {
        /// Instance path that did not match.
        instance_path: String,
        /// Root the caller declared.
        design_root: String,
        /// Why the mapping refused, in the caller's words.
        reason: String,
    },
    /// The metadata and the recording disagree about a signal's width, so no
    /// value read from it would be the simulated one.
    #[error(
        "variable {variable} ({path}) is {declared} bits in the metadata but {recorded} bits in the recording"
    )]
    WidthMismatch {
        /// Metadata variable id.
        variable: String,
        /// Recording path that was looked up.
        path: String,
        /// Width the metadata declares.
        declared: u32,
        /// Width the recording has.
        recorded: u32,
    },
    /// A type the debugger cannot lay out honestly, so the variable is refused
    /// instead of being given a silently wrong layout.
    #[error("variable {variable} cannot be debugged yet: {reason}")]
    UnsupportedType {
        /// Metadata variable id.
        variable: String,
        /// Why it is refused.
        reason: String,
    },
    /// The recording could not be searched for a signal (unreadable, ambiguous
    /// path, ...). A signal that is simply absent is `unavailable` instead.
    #[error("recording lookup failed: {0}")]
    Waveform(String),
    /// The symbol ELF could not be produced.
    #[error(transparent)]
    Symbols(#[from] crate::error::Error),
    /// GDB session failure reported by the protocol layer.
    #[error("GDB session failed: {0}")]
    Session(String),
}

/// Fatal error reported to GDB when the debugger itself cannot continue.
///
/// Non-fatal errors (a read of an unrecorded address, a refused write) are
/// reported per request instead, so GDB can keep the session alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatalError(pub String);

impl std::fmt::Display for FatalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FatalError {}

impl From<crate::error::Error> for FatalError {
    fn from(error: crate::error::Error) -> Self {
        FatalError(error.to_string())
    }
}
