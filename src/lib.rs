//! ShyHDL: Rust-style hardware description with readable Verilog-2005 output.
//!
//! A design is written in Rust syntax: `#[hardware_instance]` structs are
//! modules, their `#[In]` / `#[Out]` fields are ports, and the `run` body is the
//! default combinational process. The crate compiles that description into
//! Verilog, exports standalone testbenches, runs them on Verilator, and replays
//! a recording through standard GDB commands.
//!
//! # Entry points
//!
//! * [`interop`] - the Rust surface: `check`, `build`, `verilog`, `simulate`,
//!   `Debugger`, and the `#[hardware]` macros.
//! * [`frontend`] - source to Verilog, metadata and diagnostics.
//! * [`testbench`] - `#[hardware_test]` to a standalone Verilog-2005 testbench.
//! * [`simulation`] - the Verilator backend and the run manifest.
//! * [`waveform`] and [`replay`] - read a recording and step through it.
//! * [`debugger`] - a GDB remote-protocol stub over a recording.
//! * [`metadata`] and [`error`] - the shared design metadata and the error type.
//!
//! The language and library guides live in `docs/language.md` and
//! `docs/library.md`.

pub mod debugger;
#[cfg(rust_analyzer)]
mod editor;
pub mod error;
pub mod frontend;
pub mod interop;
pub mod lsp;
pub mod metadata;
#[cfg(feature = "nvboard")]
pub mod nvboard;
// Crate-private: normalized match patterns and coverage, shared by the
// frontend's `match` lowering and the testbench. Not a public API yet.
mod pattern;
pub mod replay;
pub mod simulation;
pub mod testbench;
pub mod waveform;

#[cfg(rust_analyzer)]
pub use editor::*;

pub use error::{Error, Result};
#[cfg(feature = "nvboard")]
pub use interop::board;
pub use interop::{
    __inventory, build, check, list_tests, module_verilog, run, simulate, source_for, test,
    test_verilog, verilog, Debugger, Hardware, PartKind, RegisteredPart, RunResult, Simulator,
    TestId, I, U,
};
pub use metadata::{
    compute_build_id, compute_design_build_id, DesignMetadata, DesignParameter, EnumVariant,
    SourceSpan, SourceUnit, TypeField, ValueType, Variable, SCHEMA_VERSION,
};
pub use simulation::Outcome;

// Re-exported so a user writes `use shyhdl::*;` once and gets the attributes too.
pub use shyhdl_macros::{hardware, hardware_instance, hardware_test};
