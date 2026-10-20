//! The GDB target: one RSP stop per cursor movement over the recording.
//!
//! Every rule in here is stated with the GDB behaviour it exists for, because
//! the interesting cases were measured against GDB 17.2 and are easy to
//! "simplify" back into a bug.

use std::collections::{BTreeSet, HashMap};

use gdbstub::common::Signal;
use gdbstub::stub::SingleThreadStopReason;
use gdbstub::target::ext::base::reverse_exec::ReplayLogPosition;
use gdbstub::target::ext::base::reverse_exec::{
    ReverseCont, ReverseContOps, ReverseStep, ReverseStepOps,
};
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadResume, SingleThreadResumeOps, SingleThreadSingleStep,
    SingleThreadSingleStepOps,
};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, HwWatchpoint, HwWatchpointOps, SwBreakpoint, SwBreakpointOps,
    WatchKind,
};
use gdbstub::target::{Target, TargetError, TargetResult};

use crate::debugger::arch::X86_64;
use crate::debugger::symbols::{SymbolLayout, VarKind, VarSlot};
use crate::debugger::FatalError;
use crate::replay::{Direction, ReplayEngine, StopReason};
use crate::waveform::{BitState, SignalId};

/// What GDB asked the target to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resume {
    Continue,
    Step,
    ReverseContinue,
    ReverseStep,
}

/// Which address GDB reads back as the program counter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReportPc {
    /// The observation point the cursor is on.
    Point,
    /// The `tick` anchor, which is where the breakpoint GDB evaluates lives.
    Anchor,
    /// One byte past the anchor: same source line, so GDB does not treat the
    /// step over the breakpoint as a line change, but not the breakpoint
    /// address, so GDB does not loop on it forever.
    Shadow,
}

/// Replay cursor presented to GDB as a single-threaded x86-64 process.
pub struct ReplayTarget {
    engine: ReplayEngine,
    layout: SymbolLayout,
    /// Synthetic debug address of each watched signal, and back.
    addr_of_signal: HashMap<SignalId, u64>,
    signal_of_addr: HashMap<u64, SignalId>,
    /// Recording signal behind each debug-visible variable.
    signal_of_var: HashMap<String, SignalId>,
    /// Observation points GDB placed a breakpoint on (Z0), by point index.
    break_points: BTreeSet<usize>,
    /// True while a breakpoint on the `tick` anchor is inserted: every resume
    /// then walks the recorded observation points one by one so that GDB can
    /// evaluate its condition at each of them.
    tick_armed: bool,
    /// The last stop we reported was the anchor, with the cursor left where the
    /// walk reached it.
    anchor_stop_at_cursor: bool,
    /// GDB removed the anchor right after such a stop: its next single step is
    /// the internal step over the breakpoint, which must not move the cursor.
    stepover_credit: bool,
    /// PC GDB reads back for the current stop.
    report_pc: ReportPc,
    /// Watches GDB currently has inserted, by address.
    watches: HashMap<u64, SignalId>,
    pending: Option<SingleThreadStopReason<u64>>,
}

impl ReplayTarget {
    /// Build a target for `engine`, serving memory through `layout`.
    ///
    /// `signal_of_var` maps each recorded variable to the waveform signal that
    /// carries it; variables the caller could not resolve are already absent
    /// from `layout`, so an unrecorded signal has no address and GDB reports
    /// "No symbol" instead of a fabricated zero.
    pub fn new(
        engine: ReplayEngine,
        layout: SymbolLayout,
        signal_of_var: HashMap<String, SignalId>,
    ) -> Result<Self, FatalError> {
        let mut addr_of_signal = HashMap::new();
        let mut signal_of_addr = HashMap::new();
        for slot in &layout.vars {
            let Some(&signal) = signal_of_var.get(&slot.id) else {
                return Err(FatalError(format!(
                    "variable {} has a symbol slot but no recording signal",
                    slot.id
                )));
            };
            addr_of_signal.insert(signal, slot.addr);
            signal_of_addr.insert(slot.addr, signal);
        }
        Ok(Self {
            engine,
            layout,
            addr_of_signal,
            signal_of_addr,
            signal_of_var,
            break_points: BTreeSet::new(),
            tick_armed: false,
            anchor_stop_at_cursor: false,
            stepover_credit: false,
            report_pc: ReportPc::Point,
            watches: HashMap::new(),
            pending: None,
        })
    }

    /// Take the stop reason produced by the last resume.
    pub fn take_pending(&mut self) -> Option<SingleThreadStopReason<u64>> {
        self.pending.take()
    }

    fn fatal(&self, what: &str, error: crate::waveform::WaveError) -> FatalError {
        FatalError(format!("{what}: {error}"))
    }

    /// Observation point the cursor is on. The session seeks to the first
    /// recorded point before serving, so the `None` cursor only means "before
    /// the first observation", which reads point 0.
    fn point(&self) -> usize {
        self.engine.cursor().index.unwrap_or(0)
    }

    fn seek(&mut self, point: usize) -> Result<(), FatalError> {
        self.engine
            .seek_index(point)
            .map_err(|e| self.fatal("cannot move the replay cursor", e))?;
        Ok(())
    }

    /// The PC GDB reads back. During a tick walk every stop must be reported at
    /// the breakpoint's own address, otherwise GDB does not match the stop to
    /// the breakpoint and never evaluates the condition it holds host side.
    fn pc(&self) -> u64 {
        match self.report_pc {
            ReportPc::Anchor => self.layout.tick_pc,
            ReportPc::Shadow => self.layout.tick_pc + 1,
            ReportPc::Point => self
                .layout
                .point_pc
                .get(self.point())
                .copied()
                .unwrap_or(self.layout.code_base),
        }
    }

    /// Point index of a debug address, if it is one of the point slots.
    fn point_of_pc(&self, addr: u64) -> Option<usize> {
        self.layout.pc_point.get(&addr).copied()
    }

    fn next_point(&self) -> Option<usize> {
        let next = self.point() + 1;
        (next < self.layout.point_pc.len()).then_some(next)
    }

    /// Nearest breakpoint point strictly after `from` and at or before `to`.
    fn break_in(&self, from: usize, to: usize) -> Option<usize> {
        self.break_points.range(from + 1..=to).next().copied()
    }

    /// Nearest breakpoint point at or after `from` and strictly before `to`.
    fn break_before(&self, from: usize, to: usize) -> Option<usize> {
        if from >= to {
            return None;
        }
        self.break_points.range(from..to).next_back().copied()
    }

    /// Move the cursor by one observation point, if there is one.
    fn step_one_point(&mut self) -> Result<bool, FatalError> {
        match self.next_point() {
            Some(next) => {
                self.seek(next)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One `step` / `continue` from GDB, resolved against the recording.
    fn resume(&mut self, kind: Resume) -> Result<(), FatalError> {
        // GDB's internal step over the anchor breakpoint, after it removed the
        // breakpoint to evaluate a condition that turned out false. Moving the
        // cursor here would skip an observation point; answering with the
        // anchor again would loop forever (measured: tens of thousands of
        // step-overs). The cursor stays, and GDB is told the step completed one
        // byte past the anchor, which is on the anchor's own source line.
        if self.stepover_credit && matches!(kind, Resume::Step) {
            self.stepover_credit = false;
            self.anchor_stop_at_cursor = false;
            self.report_pc = ReportPc::Shadow;
            self.pending = Some(SingleThreadStopReason::DoneStep);
            return Ok(());
        }
        if matches!(kind, Resume::Step) {
            self.stepover_credit = false;
        }

        // A walk over the recorded observation points, for a breakpoint on the
        // `tick` anchor. GDB holds the condition host side and only evaluates it
        // at a stop whose PC is the breakpoint's, so a conditional breakpoint
        // must be offered every recorded point, not only the reference points
        // GDB removes and re-inserts its
        // breakpoints around every resume, so `tick_armed` is accurate at the
        // moment of the resume.
        if self.tick_armed && matches!(kind, Resume::Continue) {
            self.anchor_stop_at_cursor = false;
            if self.step_one_point()? {
                self.report_pc = ReportPc::Anchor;
                self.anchor_stop_at_cursor = true;
                self.pending = Some(SingleThreadStopReason::SwBreak(()));
            } else {
                self.report_pc = ReportPc::Point;
                self.pending = Some(SingleThreadStopReason::ReplayLog {
                    tid: None,
                    pos: ReplayLogPosition::End,
                });
            }
            return Ok(());
        }

        if matches!(kind, Resume::Continue) {
            self.stepover_credit = false;
        }
        self.anchor_stop_at_cursor = false;
        self.report_pc = ReportPc::Point;
        let from = self.point();
        let outcome = match kind {
            Resume::Step => self.engine.step(Direction::Forward),
            Resume::Continue => self.engine.continue_run(Direction::Forward),
            Resume::ReverseStep => self.engine.step(Direction::Backward),
            Resume::ReverseContinue => self.engine.continue_run(Direction::Backward),
        };
        let reached = self.point();

        // A user breakpoint on an observation point stops before the point the
        // movement would have reached.
        let hit = match kind {
            Resume::Step | Resume::Continue => self.break_in(from, reached),
            Resume::ReverseStep | Resume::ReverseContinue => self.break_before(reached, from),
        };
        if let Some(point) = hit {
            self.seek(point)?;
            self.pending = Some(SingleThreadStopReason::SwBreak(()));
            return Ok(());
        }

        self.pending = Some(match &outcome.reason {
            StopReason::Watch(hits) => {
                let addr = hits
                    .first()
                    .and_then(|hit| self.addr_of_signal.get(&hit.signal).copied())
                    .unwrap_or(0);
                SingleThreadStopReason::Watch {
                    tid: (),
                    kind: WatchKind::Write,
                    addr,
                }
            }
            StopReason::ReferencePoint => SingleThreadStopReason::DoneStep,
            // "No further observation value" is not a failure;
            // the replay log position is exactly that statement to GDB.
            StopReason::End => SingleThreadStopReason::ReplayLog {
                tid: None,
                pos: ReplayLogPosition::End,
            },
            StopReason::Start => SingleThreadStopReason::ReplayLog {
                tid: None,
                pos: ReplayLogPosition::Begin,
            },
        });
        Ok(())
    }

    /// Bytes of one variable at the current observation point, in the debug
    /// layout the symbol ELF declares. `None` means "not readable here": the
    /// value has `x`/`z` bits or is not recorded at this point, and
    /// forbids standing in a zero for it.
    fn read_variable(&self, slot: &VarSlot) -> Result<Option<Vec<u8>>, FatalError> {
        let Some(&signal) = self.signal_of_var.get(&slot.id) else {
            return Ok(None);
        };
        let value = self
            .engine
            .value(signal)
            .map_err(|e| self.fatal("cannot read the recorded value", e))?;
        let Some(value) = value else {
            return Ok(None);
        };
        let Some(bits) = value.as_bits() else {
            return Ok(None);
        };
        let mut out = vec![0u8; slot.size as usize];
        if !encode(bits.bits(), &slot.kind, &mut out, 0) {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Variable covering `addr`, if any.
    fn variable_at(&self, addr: u64) -> Option<&VarSlot> {
        let (_, &index) = self.layout.addr_vars.range(..=addr).next_back()?;
        let slot = self.layout.vars.get(index)?;
        (addr < slot.addr + slot.size).then_some(slot)
    }
}

/// Serialize `bits` (least significant bit first) into the debug byte layout of
/// `kind`, starting at `out[offset]`.
///
/// The debug layout is deliberately not the hardware layout: the
/// hardware packs a struct most significant field first with no padding, while
/// the debug layout gives every field and every array element its own
/// `ceil(bits / 8)` bytes in declaration order. A field is therefore sliced out
/// of the packed bit vector instead of being copied byte for byte.
fn encode(bits: &[BitState], kind: &VarKind, out: &mut [u8], mut offset: usize) -> bool {
    match kind {
        VarKind::Bool | VarKind::UInt { .. } => encode_scalar(bits, out, offset),
        // A signed value occupies whole bytes too, and a negative one must be
        // sign extended into them: `int<12>` of -2048 is 0xF800, not 0x0800.
        VarKind::SInt { .. } => encode_signed(bits, out, offset),
        VarKind::Struct { fields, .. } => {
            let total: usize = fields.iter().map(|f| bit_width(&f.kind)).sum();
            if total > bits.len() {
                return false;
            }
            let mut top = total; // fields are packed most significant field first
            for field in fields {
                let width = bit_width(&field.kind);
                top -= width;
                let field_bits = &bits[top..top + width];
                let size = byte_size(&field.kind);
                if offset + size > out.len() {
                    return false;
                }
                if !encode(field_bits, &field.kind, out, offset) {
                    return false;
                }
                offset += size;
            }
            true
        }
        VarKind::Array { element, len } => {
            let width = bit_width(element);
            if width * (*len as usize) > bits.len() {
                return false;
            }
            // Element 0 is the least significant element in the recorded value.
            for index in 0..*len as usize {
                let element_bits = &bits[index * width..(index + 1) * width];
                let size = byte_size(element);
                if offset + size > out.len() {
                    return false;
                }
                if !encode(element_bits, element, out, offset) {
                    return false;
                }
                offset += size;
            }
            true
        }
        // The tag is the lowest bits of the recorded value and the active
        // variant's payload sits directly above it. Only that payload is sliced
        // out: the payload bits of the other variants are never read, so an `x`
        // there cannot make a value this tag does have unreadable.
        VarKind::Enum {
            tag_width,
            variants,
            ..
        } => {
            let tag_bits = *tag_width as usize;
            if tag_bits > bits.len() {
                return false;
            }
            let Some(tag) = bit_value(&bits[..tag_bits]) else {
                // A tag holding x/z names no variant: there is no value here.
                return false;
            };
            let Some(variant) = variants.iter().find(|variant| variant.tag == tag) else {
                // A tag this design does not define reads as nothing, never as
                // the first variant and never as zero.
                return false;
            };
            if !encode_scalar(&bits[..tag_bits], out, offset) {
                return false;
            }
            let Some(payload) = variant.payload.as_deref() else {
                // Payload-free variant: the payload region keeps the zeros the
                // caller started with.
                return true;
            };
            let payload_start = tag_bits;
            let payload_end = payload_start + bit_width(payload);
            if payload_end > bits.len() {
                return false;
            }
            let payload_offset = offset + tag_bits.div_ceil(8);
            if payload_offset + byte_size(payload) > out.len() {
                return false;
            }
            encode(
                &bits[payload_start..payload_end],
                payload,
                out,
                payload_offset,
            )
        }
    }
}

/// The numeric value of a two-state bit slice (bit 0 is the lowest).
///
/// `None` when any bit is unknown, or when a set bit does not fit in the `u64`
/// a variant tag is encoded in.
fn bit_value(bits: &[BitState]) -> Option<u64> {
    let mut value: u64 = 0;
    for (index, bit) in bits.iter().enumerate() {
        match bit {
            BitState::Zero => {}
            BitState::One => value |= 1u64.checked_shl(index as u32)?,
            _ => return None,
        }
    }
    Some(value)
}

/// Write a two-state value into `ceil(width / 8)` little-endian bytes, filling
/// the bits above the value with its sign (two's complement).
fn encode_signed(bits: &[BitState], out: &mut [u8], offset: usize) -> bool {
    if bits.is_empty() || !encode_scalar(bits, out, offset) {
        return false;
    }
    if bits[bits.len() - 1] != BitState::One {
        return true;
    }
    let size = bits.len().div_ceil(8);
    for bit in bits.len()..size * 8 {
        out[offset + bit / 8] |= 1 << (bit % 8);
    }
    true
}

/// Write a two-state value into `ceil(width / 8)` little-endian bytes.
fn encode_scalar(bits: &[BitState], out: &mut [u8], offset: usize) -> bool {
    let size = bits.len().div_ceil(8);
    if offset + size > out.len() {
        return false;
    }
    for (index, bit) in bits.iter().enumerate() {
        match bit {
            BitState::Zero => {}
            BitState::One => out[offset + index / 8] |= 1 << (index % 8),
            // `x`/`z` (and the rarely used `h`/`l`/`u`/`w`) are not values we
            // may turn into numbers; the caller reports them as unreadable.
            _ => return false,
        }
    }
    true
}

/// Byte size of a value in the *debug* layout.
///
/// This is not `ceil(packed bits / 8)`: the debug layout gives
/// every struct field and every array element its own whole number of bytes, so
/// `struct { uint<1>, uint<1> }` occupies two bytes even though its packed
/// width is two bits, and a nested struct's size is the sum of its fields'
/// debug sizes rather than the rounding of its packed width.
pub fn byte_size(kind: &VarKind) -> usize {
    // `VarKind::byte_size` is the single authority for the debug layout; this
    // wrapper exists because the target and its tests read a `usize`.
    kind.byte_size() as usize
}

fn bit_width(kind: &VarKind) -> usize {
    match kind {
        VarKind::Bool => 1,
        VarKind::UInt { width } | VarKind::SInt { width } => *width as usize,
        VarKind::Struct { fields, .. } => fields.iter().map(|f| bit_width(&f.kind)).sum(),
        VarKind::Array { element, len } => bit_width(element) * (*len as usize),
        // The payload sits above the tag, so a field after a payload enum starts
        // after the widest payload, not right after the tag.
        VarKind::Enum {
            tag_width,
            variants,
            ..
        } => {
            *tag_width as usize
                + variants
                    .iter()
                    .filter_map(|variant| variant.payload.as_deref())
                    .map(bit_width)
                    .max()
                    .unwrap_or(0)
        }
    }
}

impl Target for ReplayTarget {
    type Arch = X86_64;
    type Error = FatalError;

    #[inline(always)]
    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    #[inline(always)]
    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }

    /// Software breakpoints are recorded, never written into the synthetic
    /// text: the replay is read only, and GDB must not expect an
    /// instruction-stream trap (gdbstub's own guard rail for that mistake).
    #[inline(always)]
    fn guard_rail_implicit_sw_breakpoints(&self) -> bool {
        false
    }
}

impl SingleThreadBase for ReplayTarget {
    fn read_registers(
        &mut self,
        regs: &mut <Self::Arch as gdbstub::arch::Arch>::Registers,
    ) -> TargetResult<(), Self> {
        *regs = Default::default();
        regs.rip = self.pc();
        Ok(())
    }

    /// The replay is read only: no register write is ever accepted. Same
    /// refusal, and the same limitation on the reason GDB can show, as
    /// `write_addrs`.
    fn write_registers(
        &mut self,
        _regs: &<Self::Arch as gdbstub::arch::Arch>::Registers,
    ) -> TargetResult<(), Self> {
        Err(TargetError::NonFatal)
    }

    /// Serve the current observation point's values at the debug addresses the
    /// symbol ELF declares. A byte that no symbol covers reads as zero so GDB's
    /// own bookkeeping works; a byte whose signal is not readable here stops
    /// the read short, which GDB reports as an inaccessible address instead of
    /// a value we invented.
    fn read_addrs(&mut self, start_addr: u64, data: &mut [u8]) -> TargetResult<usize, Self> {
        let mut addr = start_addr;
        let end = start_addr.saturating_add(data.len() as u64);
        let mut served = 0usize;
        while addr < end {
            // The synthetic text is real memory as far as GDB is concerned, and
            // the symbol layout owns its contents: asking it keeps one
            // definition of those bytes instead of a second one here.
            if let Some(byte) = self.layout.text_byte(addr) {
                data[(addr - start_addr) as usize] = byte;
                addr += 1;
                served += 1;
                continue;
            }
            let Some(slot) = self.variable_at(addr) else {
                // No symbol covers this address: it is not memory of this
                // design, so the request stops here instead of reading zeros.
                break;
            };
            let offset = (addr - slot.addr) as usize;
            let chunk = ((slot.size as usize) - offset).min((end - addr) as usize);
            let Some(bytes) = self.read_variable(slot).map_err(TargetError::Fatal)? else {
                // Not readable here (unrecorded, or holding x/z): everything
                // before this byte was served, so report a short read. A zero
                // would be a value the recording never had.
                break;
            };
            data[(addr - start_addr) as usize..(addr - start_addr) as usize + chunk]
                .copy_from_slice(&bytes[offset..offset + chunk]);
            addr += chunk as u64;
            served += chunk;
        }
        if served == 0 {
            return Err(TargetError::NonFatal);
        }
        Ok(served)
    }

    /// The replay is read only: a memory write is always refused.
    ///
    /// `TargetError::NonFatal` is all gdbstub 0.7.10 can send here: its
    /// `TargetError` has no variant carrying a message, so the refusal reaches
    /// GDB as a numeric error reply and GDB prints its own "Cannot access memory
    /// at address..." without a reason from the target. A different errno does
    /// not change that (measured: `E0d` prints the same line), and an RSP memory
    /// map is deliberately not offered: GDB then treats every address outside
    /// the mapped regions as non-`rw` and switches its breakpoints to hardware
    /// `Z1` (measured), which a target without `HwBreakpoint` cannot serve.
    fn write_addrs(&mut self, _start_addr: u64, _data: &[u8]) -> TargetResult<(), Self> {
        Err(TargetError::NonFatal)
    }

    #[inline(always)]
    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for ReplayTarget {
    fn resume(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err(FatalError("the replay cannot resume with a signal".into()));
        }
        self.resume(Resume::Continue)
    }

    #[inline(always)]
    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_reverse_step(&mut self) -> Option<ReverseStepOps<'_, (), Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_reverse_cont(&mut self) -> Option<ReverseContOps<'_, (), Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for ReplayTarget {
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err(FatalError("the replay cannot step with a signal".into()));
        }
        self.resume(Resume::Step)
    }
}

impl ReverseStep<()> for ReplayTarget {
    fn reverse_step(&mut self, _tid: ()) -> Result<(), Self::Error> {
        self.resume(Resume::ReverseStep)
    }
}

impl ReverseCont<()> for ReplayTarget {
    fn reverse_cont(&mut self) -> Result<(), Self::Error> {
        self.resume(Resume::ReverseContinue)
    }
}

impl Breakpoints for ReplayTarget {
    #[inline(always)]
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_hw_watchpoint(&mut self) -> Option<HwWatchpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for ReplayTarget {
    /// Record a breakpoint. Returning `Ok(true)` keeps GDB from falling back to
    /// writing a trap instruction into the synthetic text: a stub that answers
    /// a `Z0` with "unknown" makes GDB disable `Z0` for the whole session and
    /// patch memory through `X`, which a read-only replay must never allow.
    fn add_sw_breakpoint(
        &mut self,
        addr: u64,
        _kind: <Self::Arch as gdbstub::arch::Arch>::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        if addr == self.layout.tick_pc {
            self.tick_armed = true;
            return Ok(true);
        }
        match self.point_of_pc(addr) {
            Some(point) => {
                self.break_points.insert(point);
                Ok(true)
            }
            // GDB places internal breakpoints of its own (startup, longjmp,
            // solib events). They are recorded and never hit, which is the
            // only correct answer for a target that does not execute.
            None => Ok(true),
        }
    }

    fn remove_sw_breakpoint(
        &mut self,
        addr: u64,
        _kind: <Self::Arch as gdbstub::arch::Arch>::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        if addr == self.layout.tick_pc {
            self.tick_armed = false;
            // A removal that follows an anchor stop is GDB stepping over the
            // breakpoint to evaluate its condition, not the user deleting it.
            if self.anchor_stop_at_cursor {
                self.stepover_credit = true;
            }
            return Ok(true);
        }
        if let Some(point) = self.point_of_pc(addr) {
            self.break_points.remove(&point);
        }
        Ok(true)
    }
}

impl HwWatchpoint for ReplayTarget {
    /// Attach a watch to the recorded signal behind `addr`, so a change is
    /// reported at the recorded observation point where it happened, including
    /// changes between reference points.
    fn add_hw_watchpoint(
        &mut self,
        addr: u64,
        _len: u64,
        _kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        let Some(&signal) = self.signal_of_addr.get(&addr) else {
            return Ok(false);
        };
        self.engine
            .watch(&[signal])
            .map_err(|e| TargetError::Fatal(FatalError(e.to_string())))?;
        self.watches.insert(addr, signal);
        Ok(true)
    }

    fn remove_hw_watchpoint(
        &mut self,
        addr: u64,
        _len: u64,
        _kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        let Some(signal) = self.watches.remove(&addr) else {
            return Ok(true);
        };
        self.engine.unwatch(signal);
        Ok(true)
    }
}
