//! The synthetic architecture GDB debugs.
//!
//! The debug ELF is a virtual x86-64 program: GDB has no software single-step
//! for x86-64, so a source-level `step` becomes exactly one `vCont;s` request
//! and the target alone decides which observation point comes next. On an
//! architecture with `get_next_pcs` (loongarch64, for example) GDB instead
//! reads and simulates the instruction at the PC, inserts a temporary
//! breakpoint and resumes with `vCont;c`, which makes the same one-step-per-
//! observation-point guarantee depend on the bytes the target serves.
//!
//! `gdbstub_arch`'s `X86_64_SSE` also supplies the `target.xml`, so the
//! register packet layout comes from the architecture description instead of a
//! hard-coded byte count.

pub use gdbstub_arch::x86::X86_64_SSE as X86_64;

/// `e_machine` of the debug ELF, `EM_X86_64`.
pub const ELF_MACHINE: u16 = 62;
