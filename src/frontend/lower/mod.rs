//! Lowering: annotations and Rust bodies become typed modules.
//!
//! This is where the language rules live. Everything the design says about
//! hardware is settled here, and every violation is a source-level diagnostic:
//!
//! * widths are resolved at build time and never mixed implicitly,
//! * `run` statements are the default combinational process (blocking, all paths
//!   assigned) or `always!` (event arms, one regular edge branch, non-blocking
//!   state),
//! * every bit of every signal has exactly one driver process,
//! * combinational loops, module recursion, unconnected child inputs and
//!   undriven signals are errors, not surprises.

use std::collections::{HashMap, HashSet};
use std::mem;

use syn::spanned::Spanned;

use crate::frontend::analysis::{Bits, DependencyGraph};
use crate::frontend::consteval::{self, ConstEnv};
use crate::frontend::diagnostic::{codes, span_in, Diagnostic};
use crate::frontend::mir::{
    BodyItem, ClockSpec, Connection, Design, Edge, Expr, ExprKind, Instance, LValue, Memory,
    Module, Param, Port, PortDir, Process, ProcessKind, ResetSpec, Signal, SignalId, SignalKind,
    SignalScope, Stmt, Ty,
};
use crate::frontend::parse::{self, FieldKind, HwField, HwImpl, HwStruct, ParsedFile};
use crate::metadata::SourceSpan;

use place::*;
use util::*;

mod alias;
mod dataflow;
mod expr;
mod place;
mod stmt;
mod util;

/// Largest number of statements a static `for` may unroll into.
const MAX_UNROLL: u64 = 65536;

/// Lower a source file into a design.
pub(crate) fn lower(
    source: &str,
    source_name: &str,
    top_spec: &str,
) -> Result<Design, crate::frontend::Diagnostics> {
    let parsed = parse::parse(source, source_name)?;
    // A single-file compile cannot load another file: say so instead of compiling
    // a design with items missing.
    if let Some(declaration) = parsed.mods.first() {
        let mut diagnostics = crate::frontend::Diagnostics::new();
        diagnostics.push(
            crate::frontend::Diagnostic::error(
                crate::frontend::codes::DECLARATION,
                Some(declaration.span.clone()),
                format!(
                    "`mod {};` loads another file, which a single-source compile cannot read",
                    declaration.name
                ),
            )
            .with_note(
                "compile the entry file with `compile_file(path, top)` so `mod` is resolved",
                None,
            ),
        );
        return Err(diagnostics);
    }
    lower_parsed(parsed, source_name, top_spec)
}

/// Lower items that were already parsed and merged from several files.
pub(crate) fn lower_parsed(
    parsed: parse::ParsedFile,
    source_name: &str,
    top_spec: &str,
) -> Result<Design, crate::frontend::Diagnostics> {
    let mut compiler = Compiler::new(source_name, parsed);
    match compiler.run(source_name, top_spec) {
        Ok(design) => Ok(design),
        Err(()) => Err(compiler.diagnostics),
    }
}

/// Who drives a bit of a signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// A continuous assignment written directly in `run`.
    Wiring,
    /// Process number `n` of the module.
    Process(usize),
    /// Instance number `n` (a child's output port).
    Instance(usize),
}

/// One child instantiated by the module being built.
#[derive(Clone, Debug)]
struct InstanceBuild {
    field: String,
    verilog_name: String,
    module_source_name: String,
    module_verilog_name: String,
    params: Vec<Param>,
    source: SourceSpan,
    child_ports: Vec<ChildPort>,
    connections: Vec<Option<(Expr, SourceSpan)>>,
    read_outputs: HashSet<String>,
}

/// What an `#[instance]` field names: one instance, or an array of them.
#[derive(Clone, Copy, Debug)]
enum FieldInstances {
    /// `#[instance] child: Child`.
    Single(usize),
    /// `#[instance] children: [Child; N]`: instances `first..first + len`.
    Array { first: usize, len: u32 },
}

/// A port of an instantiated child, with the parent-side net for outputs.
#[derive(Clone, Debug)]
struct ChildPort {
    name: String,
    dir: PortDir,
    ty: Ty,
    /// Parent-side wire carrying the child's output.
    parent_signal: Option<SignalId>,
}

/// What was recorded about one process, for the combinational dependency graph.
#[derive(Clone, Debug)]
struct ProcessRecord {
    comb: bool,
    /// Dependencies of this process: the read they come from (with its bit
    /// range), the signal it drives, and the bits it drives.
    edges: Vec<(ReadRef, SignalId, u32, u32)>,
}

/// The module currently being built.
struct ModuleBuild {
    source_name: String,
    verilog_name: String,
    params: Vec<Param>,
    env: ConstEnv,
    struct_source: SourceSpan,
    ports: Vec<Port>,
    signals: Vec<Signal>,
    signal_by_name: HashMap<String, SignalId>,
    /// Memory resources, in declaration order.
    memories: Vec<Memory>,
    memory_by_field: HashMap<String, usize>,
    /// Process that already writes each memory, and where its first write was:
    /// one memory may only be written by one process, or the two would race.
    memory_writers: HashMap<usize, (usize, SourceSpan)>,
    instances: Vec<InstanceBuild>,
    instance_by_field: HashMap<String, FieldInstances>,
    body: Vec<BodyItem>,
    owners: HashMap<SignalId, Vec<Option<Owner>>>,
    drive_span: HashMap<SignalId, SourceSpan>,
    process_records: Vec<ProcessRecord>,
    process_labels: Vec<String>,
    comb_count: usize,
    seq_count: usize,
    temp_count: usize,
}

impl ModuleBuild {
    fn new(
        source_name: String,
        verilog_name: String,
        params: Vec<Param>,
        env: ConstEnv,
        struct_source: SourceSpan,
    ) -> Self {
        Self {
            source_name,
            verilog_name,
            params,
            env,
            struct_source,
            ports: Vec::new(),
            signals: Vec::new(),
            signal_by_name: HashMap::new(),
            memories: Vec::new(),
            memory_by_field: HashMap::new(),
            memory_writers: HashMap::new(),
            instances: Vec::new(),
            instance_by_field: HashMap::new(),
            body: Vec::new(),
            owners: HashMap::new(),
            drive_span: HashMap::new(),
            process_records: Vec::new(),
            process_labels: Vec::new(),
            comb_count: 0,
            seq_count: 0,
            temp_count: 0,
        }
    }

    fn signal(&self, id: SignalId) -> &Signal {
        &self.signals[id]
    }
}

/// State of the process currently being lowered.
struct ProcCtx {
    comb: bool,
    /// True while lowering the clock arm of `always!`: the only place a memory
    /// write is allowed.
    memory_write: bool,
    /// Index of this process in [`ModuleBuild::process_labels`]: the active
    /// authority for `Owner::Process`, instead of re-deriving it from a length.
    index: usize,
    /// True while lowering the statements written directly in `run` (not inside
    /// control flow), where a `let` that escapes the process is a module signal.
    at_run_top: bool,
    /// Bindings visible at the current point: a `let` inside control flow is
    /// scoped to that block, as in Rust.
    locals: HashMap<String, SignalId>,
    /// Generated wire names already used by this process's locals: every
    /// declaration gets its own signal, and the generated block still needs one
    /// unique name per declaration.
    local_names: HashSet<String>,
    local_list: Vec<SignalId>,
    definite: HashMap<SignalId, Bits>,
    anywhere: HashMap<SignalId, Bits>,
    provenance: Provenance,
    guard_reads: Vec<ReadRef>,
    /// Dependencies collected while lowering: the read (with its bit range) and
    /// the signal it feeds.
    edges: Vec<(ReadRef, SignalId, u32, u32)>,
    /// Reads that touched a bit this process had not assigned yet. At the end of
    /// the process they are intersected with the bits it drives: a read of a bit
    /// this same process drives is its own output, which is a feedback loop.
    feedback_reads: Vec<(SignalId, u32, u32, SourceSpan)>,
    drives: HashSet<SignalId>,
    pending: Vec<Stmt>,
    /// Product of the iteration counts of the loops currently being unrolled:
    /// the expansion budget applies to the whole nesting.
    unroll_product: u64,
}

impl ProcCtx {
    fn new(comb: bool, index: usize) -> Self {
        Self {
            comb,
            memory_write: false,
            index,
            at_run_top: false,
            locals: HashMap::new(),
            local_names: HashSet::new(),
            local_list: Vec::new(),
            definite: HashMap::new(),
            anywhere: HashMap::new(),
            provenance: Provenance::default(),
            guard_reads: Vec::new(),
            edges: Vec::new(),
            feedback_reads: Vec::new(),
            drives: HashSet::new(),
            pending: Vec::new(),
            unroll_product: 1,
        }
    }
}

/// The value a `match` used as an expression produces.
///
/// One process temporary holds the selected arm's value. Every arm assigns it
/// from inside its own branch, so the statements an arm runs only run for the
/// arm that was selected.
struct MatchValue {
    /// The type the arms are lowered against: the context the call site gave, or
    /// the type an arm names without needing one.
    expected: Option<Ty>,
    /// The result temporary, declared as soon as the arms' type is known.
    result: Option<SignalId>,
    /// The type the arm values have.
    ty: Option<Ty>,
    /// The target the arms write directly, in an assignment or a `let`.
    ///
    /// Then there is no result temporary and no copy: every arm assigns the
    /// target inside its own branch, exactly where the value would have been
    /// written. `result` stays `None` in this form.
    into: Option<IntoValue>,
}

/// The target one `match` writes into, when it stands in an assignment or a
/// `let`: the arm values go straight there.
#[derive(Clone, Debug)]
struct IntoValue {
    /// What every arm assigns.
    target: LValue,
    /// `<=` for state in a sequential process, `=` otherwise.
    nonblocking: bool,
}

/// A read of bits `[lo, hi)` of one signal: the granularity at which data flow
/// is tracked, so a partial write does not look like a loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadRef {
    signal: SignalId,
    lo: u32,
    hi: u32,
}

impl ReadRef {
    fn whole(signal: SignalId, module: &ModuleBuild) -> Self {
        Self {
            signal,
            lo: 0,
            hi: module.signals[signal].width(),
        }
    }

    fn width(&self) -> u32 {
        self.hi - self.lo
    }
}

/// One written bit range and the reads its value comes from.
#[derive(Clone, Debug)]
struct RangeEntry {
    lo: u32,
    hi: u32,
    sources: Vec<ReadRef>,
}

/// Where the current value of each written bit range comes from, inside one
/// process. Writes replace the ranges they cover, so a blocking overwrite is
/// tracked exactly.
///
/// The ranges of one signal are ascending and disjoint: `write` puts a range
/// where it belongs, `splice_signal` walks the ordered list, and `merge` relies
/// on the order to answer every range with one cursor per branch.
#[derive(Clone, Debug, Default)]
struct Provenance {
    entries: HashMap<SignalId, Vec<RangeEntry>>,
}

impl Provenance {
    /// Record that `target` now holds the values read in `sources`.
    ///
    /// The list stays ascending: a write that lands before an earlier one
    /// (`y[4..8]` first, then `y[0..4]`) is inserted where its `lo` belongs
    /// instead of being appended, which is the order `merge` walks.
    fn write(&mut self, target: ReadRef, sources: Vec<ReadRef>) {
        let entries = self.entries.entry(target.signal).or_default();
        let mut next: Vec<RangeEntry> = Vec::new();
        for entry in entries.drain(..) {
            if entry.hi <= target.lo || entry.lo >= target.hi {
                next.push(entry);
                continue;
            }
            if entry.lo < target.lo {
                next.push(RangeEntry {
                    lo: entry.lo,
                    hi: target.lo,
                    sources: entry.sources.clone(),
                });
            }
            if entry.hi > target.hi {
                next.push(RangeEntry {
                    lo: target.hi,
                    hi: entry.hi,
                    sources: entry.sources.clone(),
                });
            }
        }
        // What the write does not cover kept its order, so the insertion point is
        // the first range that starts after the target.
        let at = next.partition_point(|entry| entry.lo < target.lo);
        next.insert(
            at,
            RangeEntry {
                lo: target.lo,
                hi: target.hi,
                sources,
            },
        );
        *entries = next;
    }

    /// Merge the state of every branch into one, without touching them.
    ///
    /// Branch snapshots come from the same state, so a range some branch never
    /// wrote reads the signal itself there; keeping that contribution is what
    /// makes the merged sources the union over the branches instead of the last
    /// branch's sources. A range no branch touched stays untracked, so reads of
    /// it stay reads of the signal.
    ///
    /// Every boundary of every branch cuts the signal into atomic ranges, and on
    /// such a range a branch has at most one entry (its ranges are ascending and
    /// disjoint). One cursor per branch is therefore enough: a branch answers a
    /// range from the entry it is on, or with a read of the range itself when it
    /// has no entry there. Nothing scans a branch's list again per range.
    fn merge(states: &[Self]) -> Self {
        let mut merged = Self::default();

        // Signals in a fixed order keep the result deterministic.
        let mut signals: Vec<SignalId> = states
            .iter()
            .flat_map(|state| state.entries.keys().copied())
            .collect();
        signals.sort_unstable();
        signals.dedup();

        for signal in signals {
            // The entries of every branch for this signal, and the boundaries
            // that cut it into the atomic ranges.
            let mut branches: Vec<Option<&[RangeEntry]>> = Vec::with_capacity(states.len());
            let mut bounds: Vec<u32> = Vec::new();
            for state in states {
                let branch = state.entries.get(&signal).map(|entries| entries.as_slice());
                if let Some(branch) = branch {
                    debug_assert!(
                        branch.windows(2).all(|pair| pair[0].hi <= pair[1].lo),
                        "the entries of one signal are ascending and disjoint"
                    );
                    for entry in branch {
                        bounds.push(entry.lo);
                        bounds.push(entry.hi);
                    }
                }
                branches.push(branch);
            }
            bounds.sort_unstable();
            bounds.dedup();

            let mut cursors = vec![0usize; branches.len()];
            let entries = merged.entries.entry(signal).or_default();
            for window in bounds.windows(2) {
                let (lo, hi) = (window[0], window[1]);
                // A range only counts as tracked when some branch actually wrote
                // it. Whether its sources are empty says nothing: a branch that
                // wrote a constant has an entry with no reads, and dropping that
                // entry would make every later read of the range look like a read
                // of the signal's own value.
                let mut sources: Vec<ReadRef> = Vec::new();
                let mut written = false;
                for (index, cursor) in cursors.iter_mut().enumerate() {
                    let covering = match branches[index] {
                        Some(branch) => {
                            // Skip what this range has left behind; a cursor only
                            // moves forward, so every branch is walked once per
                            // signal.
                            while *cursor < branch.len() && branch[*cursor].hi <= lo {
                                *cursor += 1;
                            }
                            branch
                                .get(*cursor)
                                .filter(|entry| entry.lo <= lo && entry.hi >= hi)
                        }
                        None => None,
                    };
                    match covering {
                        Some(entry) => {
                            written = true;
                            for read in &entry.sources {
                                if !sources.contains(read) {
                                    sources.push(*read);
                                }
                            }
                        }
                        // A branch with no entry on this range, including a branch
                        // with no entry for the signal at all, contributes a read
                        // of the range itself: that is what `sources_for` reports
                        // for the uncovered part of a read.
                        None => {
                            let read = ReadRef { signal, lo, hi };
                            if !sources.contains(&read) {
                                sources.push(read);
                            }
                        }
                    }
                }
                if !written {
                    // A gap between two written ranges of separate branches: no
                    // branch has a value for it, so it stays untracked.
                    continue;
                }
                // Adjacent ranges with the same sources stay one entry.
                if let Some(last) = entries.last_mut() {
                    if last.hi == lo && last.sources == sources {
                        last.hi = hi;
                        continue;
                    }
                }
                entries.push(RangeEntry { lo, hi, sources });
            }
        }
        merged
    }

    /// The reads behind `read`, and whether part of it was never written in this
    /// process (that part reads the signal's value from outside the process).
    ///
    /// The returned reads cover exactly `read`: for every bit that was already
    /// written in this process, the read its value comes from; for every bit
    /// that was not, a read of that bit range itself. A bit range is returned
    /// for the not-yet-written part instead of the whole `read`, because the
    /// whole read would also claim the bits this process *did* write: reading
    /// `y[i]` after `y[1] = a` would then look like a dependency on `y[1]`,
    /// which is this process's own value, instead of only on the external bits.
    fn sources_for(&self, read: ReadRef) -> (Vec<ReadRef>, bool) {
        let mut sources = Vec::new();
        let mut covered = Vec::new();
        if let Some(entries) = self.entries.get(&read.signal) {
            for entry in entries {
                let lo = entry.lo.max(read.lo);
                let hi = entry.hi.min(read.hi);
                if lo >= hi {
                    continue;
                }
                covered.push((lo, hi));
                sources.extend(entry.sources.iter().copied());
            }
        }
        let missing = uncovered_runs(&mut covered, read.lo, read.hi);
        let incomplete = !missing.is_empty();
        sources.extend(missing.into_iter().map(|(lo, hi)| ReadRef {
            signal: read.signal,
            lo,
            hi,
        }));
        (sources, incomplete)
    }

    /// Replace the ranges this signal's leaves wrote, keeping everything else.
    ///
    /// `leaves` is ascending and its ranges are disjoint: one arm per element of
    /// a run-time index. The stored entries are sorted first, so both lists are
    /// ordered, and every boundary of the two cuts the signal into atomic ranges
    /// that at most one entry and at most one leaf cover. Two cursors walk both
    /// lists once, so nothing scans the entries again per leaf.
    ///
    /// * a leaf over an entry unions the leaf's own sources with the old ones,
    /// * an entry no leaf covers keeps its sources - the neighbouring struct
    ///   field, the rest of a nested index, a gap between two leaves,
    /// * a leaf no entry covers takes its own sources alone: there is no old
    ///   value to keep there,
    /// * a range neither covers stays untracked, exactly as before the write.
    fn splice_signal(&mut self, signal: SignalId, leaves: &[(u32, u32, Vec<ReadRef>)]) {
        if leaves.is_empty() {
            return;
        }
        let entries = self.entries.entry(signal).or_default();
        entries.sort_by_key(|entry| entry.lo);

        // What this signal's ranges held before the write. A leaf replaces its
        // range, so the value it drives there can also be the old one: an index
        // that matches no leaf keeps it. That is why each leaf takes the sources
        // of the range it covers as a union with its own.
        let before = entries.clone();

        let mut bounds: Vec<u32> = Vec::with_capacity(2 * (before.len() + leaves.len()));
        for entry in &before {
            bounds.push(entry.lo);
            bounds.push(entry.hi);
        }
        for (lo, hi, _) in leaves {
            bounds.push(*lo);
            bounds.push(*hi);
        }
        bounds.sort_unstable();
        bounds.dedup();

        let mut next: Vec<RangeEntry> = Vec::new();
        let mut entry = 0;
        let mut leaf = 0;
        for window in bounds.windows(2) {
            let (lo, hi) = (window[0], window[1]);
            // Both lists are ascending and disjoint, so an entry or a leaf that
            // ends at or before this range can never cover it or a later one.
            while entry < before.len() && before[entry].hi <= lo {
                entry += 1;
            }
            while leaf < leaves.len() && leaves[leaf].1 <= lo {
                leaf += 1;
            }
            let covered_entry =
                entry < before.len() && before[entry].lo <= lo && before[entry].hi >= hi;
            let covered_leaf = leaf < leaves.len() && leaves[leaf].0 <= lo && leaves[leaf].1 >= hi;
            match (covered_entry, covered_leaf) {
                (true, true) => {
                    let mut sources = leaves[leaf].2.clone();
                    for source in &before[entry].sources {
                        if !sources.contains(source) {
                            sources.push(*source);
                        }
                    }
                    next.push(RangeEntry { lo, hi, sources });
                }
                // A written range keeps its entry even when the entry has no
                // sources at all: that is a constant, and dropping it would make
                // a later read look like a read of the signal's own value.
                (true, false) => next.push(RangeEntry {
                    lo,
                    hi,
                    sources: before[entry].sources.clone(),
                }),
                (false, true) => next.push(RangeEntry {
                    lo,
                    hi,
                    sources: leaves[leaf].2.clone(),
                }),
                (false, false) => {}
            }
        }
        *entries = next;
    }
}

/// The sub-ranges of `[lo, hi)` that no range in `covered` covers, ascending.
///
/// `covered` is sorted in place.
fn uncovered_runs(covered: &mut [(u32, u32)], lo: u32, hi: u32) -> Vec<(u32, u32)> {
    covered.sort_unstable();
    let mut missing = Vec::new();
    let mut bit = lo;
    for (cover_lo, cover_hi) in covered.iter().copied() {
        if cover_hi <= bit {
            continue;
        }
        if cover_lo > bit {
            missing.push((bit, cover_lo.min(hi)));
        }
        bit = bit.max(cover_hi);
        if bit >= hi {
            break;
        }
    }
    if bit < hi {
        missing.push((bit, hi));
    }
    missing
}

/// One continuous assignment, as a driver of a bit range.
struct WiringDriver {
    signal: SignalId,
    lo: u32,
    hi: u32,
    reads: Vec<ReadRef>,
}

/// The lowering driver.
struct Compiler {
    source_name: String,
    parsed: ParsedFile,
    diagnostics: crate::frontend::Diagnostics,
    /// Layout of every named data type (a struct or an enum), computed once.
    layouts: HashMap<String, Result<Ty, Diagnostic>>,
    layout_stack: Vec<String>,
    modules: Vec<Module>,
    index_by_name: HashMap<String, usize>,
    reached: HashSet<String>,
    in_progress: Vec<String>,
    /// Per module: input port name -> output port names that combinationally
    /// depend on it. Needed to see loops through instances.
    comb_deps: HashMap<String, HashMap<String, Vec<String>>>,
    cur: ModuleBuild,
    proc: Option<ProcCtx>,
    /// Reads collected while lowering the expression currently in flight.
    reads: HashSet<SignalId>,
    /// Combinational functions, by name: their synthesized module is lowered
    /// once and every call site instantiates it.
    function_modules: HashMap<String, usize>,
}

impl Compiler {
    fn new(source_name: &str, parsed: ParsedFile) -> Self {
        Self {
            source_name: source_name.to_string(),
            parsed,
            diagnostics: crate::frontend::Diagnostics::new(),
            layouts: HashMap::new(),
            layout_stack: Vec::new(),
            modules: Vec::new(),
            index_by_name: HashMap::new(),
            reached: HashSet::new(),
            in_progress: Vec::new(),
            comb_deps: HashMap::new(),
            cur: ModuleBuild::new(
                source_name.to_string(),
                String::new(),
                Vec::new(),
                ConstEnv::new(),
                SourceSpan::new(source_name, 1, 1),
            ),
            proc: None,
            reads: HashSet::new(),
            function_modules: HashMap::new(),
        }
    }

    /// Synthesize one module per `#[hardware_instance] fn`.
    ///
    /// A function is a combinational module whose inputs are its parameters and
    /// whose outputs are the elements of its return type. The body is lowered
    /// exactly like a `run` body, with the tail expression assigned to the
    /// outputs, so the Verilog emitter and the debugger see an ordinary module.
    fn prepare_functions(&mut self) -> Result<(), ()> {
        let functions = self.parsed.functions.clone();
        // Every function becomes an ordinary module declaration first, so a
        // call in any body can name one whatever order they are written in.
        for function in &functions {
            let hw = self.function_struct(function)?;
            let implementation = self.function_impl(function, &hw)?;
            self.parsed.hw.push(hw.clone());
            self.parsed.impls.insert(hw.name.clone(), implementation);
        }
        for function in &functions {
            if self.ensure_function_module(&function.name).is_err() {
                return Err(());
            }
        }
        Ok(())
    }

    /// Lower a function's module on first use, and return its index.
    fn ensure_function_module(&mut self, name: &str) -> Result<usize, ()> {
        if let Some(index) = self.function_modules.get(name) {
            return Ok(*index);
        }
        let Some(hw) = self
            .parsed
            .hw
            .iter()
            .find(|candidate| candidate.name == name)
            .cloned()
        else {
            return Err(());
        };
        let index = self.lower_module(&hw, ConstEnv::new(), false)?;
        self.function_modules.insert(name.to_string(), index);
        Ok(index)
    }

    /// The module shape of a function: parameters in, return elements out.
    fn function_struct(&mut self, function: &parse::HwFunction) -> Result<HwStruct, ()> {
        let span = function.span.clone();
        let mut fields = Vec::new();
        for parameter in &function.params {
            if let Err(diagnostic) = check_verilog_name(
                &parameter.name,
                "parameter name",
                Some(parameter.span.clone()),
            ) {
                self.diagnostics.push(diagnostic);
                return Err(());
            }
            fields.push(HwField {
                name: parameter.name.clone(),
                kind: FieldKind::In,
                ty: parameter.ty.clone(),
                span: parameter.span.clone(),
            });
        }
        let outputs = match self.function_outputs(function) {
            Ok(outputs) => outputs,
            Err(diagnostic) => return self.error(diagnostic),
        };
        for (name, ty) in outputs {
            fields.push(HwField {
                name,
                kind: FieldKind::Out,
                ty,
                span: span.clone(),
            });
        }
        Ok(HwStruct {
            name: function.name.clone(),
            params: Vec::new(),
            fields,
            span,
        })
    }

    /// The output types of a function, with the port name each one gets.
    fn function_outputs(
        &mut self,
        function: &parse::HwFunction,
    ) -> Result<Vec<(String, syn::Type)>, Diagnostic> {
        let span = function.span.clone();
        match &function.ret {
            syn::Type::Tuple(tuple) => {
                if tuple.elems.is_empty() {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!(
                            "hardware function `{}` returns `()`, which has no outputs",
                            function.name
                        ),
                    ));
                }
                Ok(tuple
                    .elems
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| (format!("out{index}"), ty.clone()))
                    .collect())
            }
            ty => Ok(vec![("out0".to_string(), ty.clone())]),
        }
    }

    /// The `run` body of a function: its statements, then the tail expression
    /// assigned to the outputs.
    fn function_impl(&mut self, function: &parse::HwFunction, hw: &HwStruct) -> Result<HwImpl, ()> {
        let span = function.span.clone();
        let mut statements: Vec<syn::Stmt> = function.body.stmts.clone();
        let tail = match statements.pop() {
            Some(syn::Stmt::Expr(expression, None)) => Some(expression),
            Some(other) => {
                statements.push(other);
                None
            }
            None => None,
        };
        let Some(tail) = tail else {
            return self.err(
                codes::BAD_TYPE,
                Some(span),
                format!(
                    "hardware function `{}` must end with the value it returns",
                    function.name
                ),
            );
        };

        let outputs: Vec<String> = hw
            .fields
            .iter()
            .filter(|field| field.kind == FieldKind::Out)
            .map(|field| field.name.clone())
            .collect();
        let mut assignments: Vec<syn::Stmt> = Vec::new();
        match &tail {
            syn::Expr::Tuple(tuple) if outputs.len() > 1 => {
                if tuple.elems.len() != outputs.len() {
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span),
                        format!(
                            "hardware function `{}` returns {} value(s) but its type declares {}",
                            function.name,
                            tuple.elems.len(),
                            outputs.len()
                        ),
                    );
                }
                for (output, value) in outputs.iter().zip(tuple.elems.iter()) {
                    let name = syn::Ident::new(output, proc_macro2::Span::call_site());
                    assignments.push(syn::parse_quote!(self.#name = #value;));
                }
            }
            value if outputs.len() == 1 => {
                let name = syn::Ident::new(&outputs[0], proc_macro2::Span::call_site());
                assignments.push(syn::parse_quote!(self.#name = #value;));
            }
            _ => {
                return self.err(
                    codes::BAD_TYPE,
                    Some(span),
                    format!(
                    "hardware function `{}` declares several outputs, so it must end with a tuple",
                    function.name
                ),
                )
            }
        }
        statements.extend(assignments);

        let block: syn::Block = syn::parse_quote!({
            #(#statements)*
        });
        Ok(HwImpl {
            struct_name: function.name.clone(),
            params: Vec::new(),
            args: Vec::new(),
            run: block,
            span,
        })
    }

    fn span(&self, span: proc_macro2::Span) -> Option<SourceSpan> {
        span_in(&self.source_name, span)
    }

    /// A span is only optional when `proc_macro2` could not give a location; a
    /// diagnostic still needs a place to point at, so it falls back to the
    /// struct being compiled.
    fn span_or_default(&self, span: Option<SourceSpan>) -> SourceSpan {
        span.unwrap_or_else(|| self.cur.struct_source.clone())
    }

    fn error<T>(&mut self, diagnostic: Diagnostic) -> Result<T, ()> {
        self.diagnostics.push(diagnostic);
        Err(())
    }

    fn err<T>(
        &mut self,
        code: &'static str,
        span: Option<SourceSpan>,
        message: impl Into<String>,
    ) -> Result<T, ()> {
        self.error(Diagnostic::error(code, span, message))
    }

    /// Push a leaf diagnostic and keep going where that is meaningful.
    fn soft<T>(&mut self, result: Result<T, Diagnostic>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                None
            }
        }
    }

    // ---------------------------------------------------------------- driver

    fn run(&mut self, source_name: &str, top_spec: &str) -> Result<Design, ()> {
        let (top_name, top_args) = match parse_top_spec(top_spec) {
            Ok(parsed) => parsed,
            Err(message) => return self.err(codes::TOP, None, message),
        };

        let Some(hw) = self
            .parsed
            .hw
            .iter()
            .find(|hw| hw.name == top_name)
            .cloned()
        else {
            let mut diagnostic = Diagnostic::error(
                codes::TOP,
                None,
                format!("`{top_name}` is not a #[hardware_instance] struct in {source_name}"),
            );
            let available: Vec<String> = self.parsed.hw.iter().map(|hw| hw.name.clone()).collect();
            if available.is_empty() {
                diagnostic = diagnostic.with_note(
                    "this file declares no #[hardware_instance] struct, so it has no top module",
                    None,
                );
            } else {
                diagnostic = diagnostic.with_note(
                    format!("declared hardware instances: {}", available.join(", ")),
                    None,
                );
            }
            if self.parsed.data.contains_key(&top_name) {
                diagnostic = diagnostic.with_note(
                    format!(
                        "`{top_name}` is a plain data struct: data structs are values, not modules."
                    ),
                    None,
                );
            }
            return self.error(diagnostic);
        };

        if self.prepare_functions().is_err() {
            return Err(());
        }

        let env = match bind_params(
            &hw,
            &top_args,
            &ConstEnv::new(),
            &self.source_name,
            hw.span.clone(),
        ) {
            Ok(env) => env,
            Err(diagnostic) => return self.error(diagnostic),
        };

        let top_index = match self.lower_module(&hw, env, true) {
            Ok(index) => index,
            Err(()) => return Err(()),
        };

        for hw in &self.parsed.hw {
            if self.function_modules.contains_key(&hw.name) {
                continue;
            }
            if !self.reached.contains(&hw.name) {
                self.diagnostics.push(Diagnostic::warning(
                    codes::UNREACHABLE,
                    hw.span.clone(),
                    format!(
                        "hardware instance `{}` is not reachable from top module `{top_name}`",
                        hw.name
                    ),
                ));
            }
        }

        let warnings = self
            .diagnostics
            .as_slice()
            .iter()
            .filter(|d| !d.is_error())
            .cloned()
            .collect();
        Ok(Design {
            source_name: source_name.to_string(),
            modules: mem::take(&mut self.modules),
            top: top_index,
            warnings,
        })
    }

    // --------------------------------------------------------------- modules

    /// Lower one hardware struct at one parameter binding.
    fn lower_module(&mut self, hw: &HwStruct, env: ConstEnv, is_top: bool) -> Result<usize, ()> {
        let verilog_name = mangle(&hw.name, &hw.params, &env);
        if let Some(index) = self.index_by_name.get(&verilog_name) {
            return Ok(*index);
        }

        if let Some(position) = self
            .in_progress
            .iter()
            .position(|name| *name == verilog_name)
        {
            let chain: Vec<String> = self.in_progress[position..].to_vec();
            let mut message = format!("module recursion: {}", chain.join(" -> "));
            message.push_str(&format!(" -> {verilog_name}"));
            return self.err(codes::MODULE_RECURSION, Some(hw.span.clone()), message);
        }

        for other in self.parsed.hw.iter() {
            if other.name != hw.name {
                let other_name = mangle(&other.name, &other.params, &ConstEnv::new());
                if other_name == verilog_name {
                    return self.err(
                        codes::NAME_COLLISION,
                        Some(hw.span.clone()),
                        format!(
                            "`{}` specialised to `{verilog_name}` collides with hardware instance `{}`",
                            hw.name, other.name
                        ),
                    );
                }
            }
        }

        let Some(implementation) = self.parsed.impls.get(&hw.name).cloned() else {
            return self.err(
                codes::IMPL,
                Some(hw.span.clone()),
                format!(
                    "hardware instance `{}` has no `impl Hardware` with `fn run(&mut self)`",
                    hw.name
                ),
            );
        };

        if let Some(diagnostic) = impl_matches_params(hw, &implementation, &env, &self.source_name)
        {
            return self.error(diagnostic);
        }

        if let Err(diagnostic) = check_verilog_name(&hw.name, "module name", Some(hw.span.clone()))
        {
            return self.error(diagnostic);
        }
        let previous = mem::replace(
            &mut self.cur,
            ModuleBuild::new(
                hw.name.clone(),
                verilog_name.clone(),
                params_of(hw, &env),
                env,
                hw.span.clone(),
            ),
        );
        self.in_progress.push(verilog_name.clone());
        self.reached.insert(hw.name.clone());

        let result = self.build_module(hw, &implementation);

        self.in_progress.pop();
        let build = mem::replace(&mut self.cur, previous);

        let build = match result {
            Ok(()) => build,
            Err(()) => return Err(()),
        };

        let module = match self.finish_module(build, is_top) {
            Ok(module) => module,
            Err(()) => return Err(()),
        };

        let index = self.modules.len();
        self.index_by_name.insert(verilog_name, index);
        self.modules.push(module);
        Ok(index)
    }

    /// Ports, instances and body of one module.
    fn build_module(&mut self, hw: &HwStruct, implementation: &HwImpl) -> Result<(), ()> {
        let mut failed = false;

        for field in &hw.fields {
            match field.kind {
                FieldKind::Instance => {
                    if self.build_instance(field).is_err() {
                        failed = true;
                    }
                }
                FieldKind::Mem => {
                    if self.build_memory(field).is_err() {
                        failed = true;
                    }
                }
                _ => {
                    if self.build_port(field).is_err() {
                        failed = true;
                    }
                }
            }
        }
        if failed {
            return Err(());
        }

        // The body of `run` is split into three things, and the split is decided
        // before anything is lowered so that ordering cannot change the meaning:
        //
        // * a child connection is structure and never enters a process,
        // * `always!` stays a sequential process of its own,
        // * everything else is the default combinational process: a *single*
        //   process for the whole body, so blocking order and last-write-wins
        //   mean exactly what they say.
        let run = implementation.run.clone();
        let process_span = implementation.span.clone();

        let mut behavioral: Vec<syn::Stmt> = Vec::new();
        let mut connections: Vec<syn::ExprAssign> = Vec::new();
        let mut sequential: Vec<syn::Macro> = Vec::new();
        let mut failed = false;

        // A build-time `for` at the top of `run` is expanded before the split, so
        // a connection written inside it (`self.bank[i].a = ...`) stays wiring
        // instead of being rejected as a statement of a process.
        let mut pending: Vec<syn::Stmt> = run.stmts.iter().rev().cloned().collect();
        while let Some(statement) = pending.pop() {
            if let syn::Stmt::Expr(syn::Expr::ForLoop(item), _) = &statement {
                // A loop that connects child inputs is structure, not behavior:
                // expand it before the split. Everything else stays a process
                // `for`, with its own scoping and unroll accounting.
                if self.statements_have_connection(&item.body.stmts) {
                    if let Some(expanded) = self.expand_run_for(item) {
                        pending.extend(expanded.into_iter().rev());
                        continue;
                    }
                }
            }
            let statement = &statement;
            let macro_statement = match statement {
                syn::Stmt::Macro(value) => Some(&value.mac),
                syn::Stmt::Expr(syn::Expr::Macro(value), _) => Some(&value.mac),
                _ => None,
            };
            if let Some(mac) = macro_statement {
                if parse::macro_name(mac) == "always" {
                    sequential.push(mac.clone());
                } else if self.lower_process_macro(mac).is_err() {
                    failed = true;
                }
                continue;
            }
            match statement {
                syn::Stmt::Expr(syn::Expr::Assign(assign), _) if self.is_connection(assign) => {
                    connections.push(assign.clone());
                }
                _ => behavioral.push(statement.clone()),
            }
        }

        // The default combinational process is lowered first, so a run-level
        // `let` read from an `always!` is already declared whatever order they
        // are written in.
        if !behavioral.is_empty()
            && self
                .lower_comb_statements(&behavioral, process_span)
                .is_err()
        {
            failed = true;
        }
        for mac in &sequential {
            let span = self
                .span(mac.path.span())
                .unwrap_or_else(|| self.cur.struct_source.clone());
            if self.lower_seq(mac, span).is_err() {
                failed = true;
            }
        }
        for assign in &connections {
            if self.lower_run_connection(assign).is_err() {
                failed = true;
            }
        }

        if failed {
            return Err(());
        }
        Ok(())
    }

    /// Whether these statements connect a child input anywhere inside them.
    fn statements_have_connection(&self, statements: &[syn::Stmt]) -> bool {
        statements.iter().any(|statement| match statement {
            syn::Stmt::Expr(syn::Expr::Assign(assign), _) => self.is_connection(assign),
            syn::Stmt::Expr(syn::Expr::ForLoop(item), _) => {
                self.statements_have_connection(&item.body.stmts)
            }
            syn::Stmt::Expr(syn::Expr::If(item), _) => {
                self.statements_have_connection(&item.then_branch.stmts)
                    || item.else_branch.as_ref().is_some_and(|(_, branch)| {
                        matches!(&**branch, syn::Expr::Block(block)
                            if self.statements_have_connection(&block.block.stmts))
                    })
            }
            _ => false,
        })
    }

    /// Expand a top-level build-time `for` into its iterations, with the loop
    /// variable replaced by the literal value.
    ///
    /// Returns `None` when the range is not a build-time constant: the statement
    /// is then left to the normal path, which reports the diagnostic with the
    /// right location and wording.
    fn expand_run_for(&mut self, item: &syn::ExprForLoop) -> Option<Vec<syn::Stmt>> {
        let (name, _) = pattern_name(&item.pat).ok()?;
        let syn::Expr::Range(range) = &*item.expr else {
            return None;
        };
        let start = match &range.start {
            Some(start) => consteval::eval(start, &self.cur.env).ok()?,
            None => 0,
        };
        let mut end = match &range.end {
            Some(end) => consteval::eval(end, &self.cur.env).ok()?,
            None => return None,
        };
        if matches!(range.limits, syn::RangeLimits::Closed(_)) {
            end = end.checked_add(1)?;
        }
        if end.saturating_sub(start) > MAX_UNROLL {
            return None;
        }
        let mut out = Vec::new();
        for value in start..end {
            for statement in &item.body.stmts {
                let mut statement = statement.clone();
                syn::visit_mut::VisitMut::visit_stmt_mut(
                    &mut Subst { name: &name, value },
                    &mut statement,
                );
                out.push(statement);
            }
        }
        Some(out)
    }

    /// True when an assignment drives a child input port: that is structure, not
    /// behavior, so it must not end up inside a process.
    fn is_connection(&self, assign: &syn::ExprAssign) -> bool {
        let Ok((is_self, base_name, path)) = decompose_place(&assign.left) else {
            return false;
        };
        // `self.kid.port` and the implicit `kid.port` name the same instance
        // field; only the prefix differs.
        let (field, path) = if is_self {
            let Some(Component::Field(field)) = path.first() else {
                return false;
            };
            (field.clone(), &path[1..])
        } else {
            let Some(name) = base_name else {
                return false;
            };
            (name, &path[..])
        };
        let Some(entry) = self.cur.instance_by_field.get(&field).copied() else {
            return false;
        };
        // An array of instances needs an index before the port; the index is
        // checked when the place is resolved, not here.
        let (instance, path) = match entry {
            FieldInstances::Single(instance) => (instance, path),
            FieldInstances::Array { first, .. } => {
                if !matches!(path.first(), Some(Component::Bits(_))) {
                    return false;
                }
                (first, &path[1..])
            }
        };
        let Some(Component::Field(port)) = path.first() else {
            return false;
        };
        self.cur.instances[instance]
            .child_ports
            .iter()
            .any(|candidate| candidate.name == *port && candidate.dir.is_input())
    }

    fn build_port(&mut self, field: &HwField) -> Result<(), ()> {
        let dir = match field.kind {
            FieldKind::In => PortDir::In,
            FieldKind::Out => PortDir::Out,
            // Instances and memories are structure, declared elsewhere.
            FieldKind::Instance | FieldKind::Mem => return Ok(()),
        };
        let result = self.resolve_type(&field.ty, field.span.clone());
        let Some(ty) = self.soft(result) else {
            return Err(());
        };
        self.declare_signal(
            &field.name,
            Some(field.name.clone()),
            ty.clone(),
            SignalKind::Port(dir),
            Some(field.span.clone()),
        )?;
        let signal = self.cur.signal_by_name[&field.name];
        self.cur.ports.push(Port {
            source_name: field.name.clone(),
            verilog_name: field.name.clone(),
            dir,
            width: ty.width(),
            ty: ty.to_value_type(),
            source: field.span.clone(),
            signal,
        });
        Ok(())
    }

    /// A `Mem<T, N>` field: an independent memory resource of the module.
    ///
    /// The element type is resolved like any other value type, so it reuses the
    /// fixed layouts (`uint`, `int`, `bool`, structs, ...) and cannot itself be
    /// a memory.
    fn build_memory(&mut self, field: &HwField) -> Result<(), ()> {
        let span = field.span.clone();
        let Some((element, depth)) = parse::memory_type(&field.ty) else {
            return self.err(
                codes::BAD_TYPE,
                Some(span),
                format!("`{}` is not a memory declaration", field.name),
            );
        };
        if let Err(diagnostic) = check_verilog_name(&field.name, "memory name", Some(span.clone()))
        {
            self.diagnostics.push(diagnostic);
            return Err(());
        }
        let element = match self.resolve_type(&element, span.clone()) {
            Ok(ty) => ty,
            Err(diagnostic) => return self.error(diagnostic),
        };
        let depth = match consteval::eval(&depth, &self.cur.env) {
            Ok(depth) => depth,
            Err(message) => return self.err(codes::NOT_CONSTANT, Some(span), message),
        };
        if depth == 0 {
            return self.err(
                codes::BAD_TYPE,
                Some(span),
                format!("memory `{}` has depth 0", field.name),
            );
        }
        if depth > u64::from(u32::MAX) {
            return self.err(
                codes::OUT_OF_RANGE,
                Some(span),
                format!(
                    "memory `{}` has {depth} elements, more than the supported maximum",
                    field.name
                ),
            );
        }
        if self.cur.signal_by_name.contains_key(&field.name)
            || self.cur.instance_by_field.contains_key(&field.name)
            || self.cur.memory_by_field.contains_key(&field.name)
        {
            return self.err(
                codes::NAME_COLLISION,
                Some(span),
                format!(
                    "`{}` collides with another signal, instance or memory in this module",
                    field.name
                ),
            );
        }
        let index = self.cur.memories.len();
        self.cur.memories.push(Memory {
            name: field.name.clone(),
            element,
            depth: depth as u32,
            source: span,
        });
        self.cur.memory_by_field.insert(field.name.clone(), index);
        Ok(())
    }

    fn build_instance(&mut self, field: &HwField) -> Result<(), ()> {
        let span = field.span.clone();
        if let Err(diagnostic) =
            check_verilog_name(&field.name, "instance name", Some(span.clone()))
        {
            self.diagnostics.push(diagnostic);
            return Err(());
        }
        if self.cur.memory_by_field.contains_key(&field.name) {
            return self.err(
                codes::NAME_COLLISION,
                Some(span),
                format!(
                    "`{}` is already declared in this module as a memory",
                    field.name
                ),
            );
        }

        // `#[instance] bank: [Child; N]` is N instances of `Child`. Each element
        // becomes a real instance with its own name, so the Verilog emitter, the
        // metadata paths and the debugger keep seeing plain instances.
        let (element_ty, count) = match &field.ty {
            syn::Type::Array(array) => {
                let count = match consteval::eval(&array.len, &self.cur.env) {
                    Ok(count) => count,
                    Err(message) => {
                        return self.err(codes::NOT_CONSTANT, Some(span.clone()), message)
                    }
                };
                if count == 0 {
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span.clone()),
                        format!("instance array `{}` has length 0", field.name),
                    );
                }
                ((*array.elem).clone(), count)
            }
            other => (other.clone(), 1),
        };

        let (child_name, child_args) = match parse::type_path_args(&element_ty) {
            Ok(parts) => parts,
            Err(message) => return self.err(codes::BAD_TYPE, Some(span), message),
        };
        let Some(child) = self
            .parsed
            .hw
            .iter()
            .find(|hw| hw.name == child_name)
            .cloned()
        else {
            if self.parsed.data.contains_key(&child_name) {
                return self.err(
                    codes::BAD_TYPE,
                    Some(span),
                    format!(
                        "`{child_name}` is a plain data struct; #[instance] fields must name a #[hardware_instance]"
                    ),
                );
            }
            return self.err(
                codes::BAD_TYPE,
                Some(span),
                format!("unknown child module `{child_name}`"),
            );
        };

        let env = match bind_params(
            &child,
            &child_args,
            &self.cur.env,
            &self.source_name,
            span.clone(),
        ) {
            Ok(env) => env,
            Err(diagnostic) => return self.error(diagnostic),
        };
        let child_verilog_name = mangle(&child.name, &child.params, &env);
        let params = params_of(&child, &env);

        let child_module = self.lower_module(&child, env, false)?;
        let child_ports_template: Vec<ChildPort> = self.modules[child_module]
            .ports
            .iter()
            .map(|port| ChildPort {
                name: port.verilog_name.clone(),
                dir: port.dir,
                ty: self.modules[child_module].signals[port.signal].ty.clone(),
                parent_signal: None,
            })
            .collect();
        let port_count = child_ports_template.len();

        let first = self.cur.instances.len();
        for element in 0..count {
            let (display_name, verilog_name) = if count == 1 {
                (field.name.clone(), field.name.clone())
            } else {
                (
                    format!("{}[{}]", field.name, element),
                    format!("{}__{}", field.name, element),
                )
            };
            let index = first + element as usize;
            let mut child_ports = child_ports_template.clone();

            // A child's output needs a parent-side net to be connected to; it is
            // driven by the instance itself (owner Instance) and may not be
            // written by the parent.
            for child_port in child_ports.iter_mut() {
                if child_port.dir != PortDir::Out {
                    continue;
                }
                let net_name = format!("{}_{}", verilog_name, child_port.name);
                self.declare_signal(
                    &net_name,
                    None,
                    child_port.ty.clone(),
                    SignalKind::Wire,
                    Some(span.clone()),
                )?;
                let signal = self.cur.signal_by_name[&net_name];
                self.claim_or_report(signal, None, Owner::Instance(index), &span)?;
                child_port.parent_signal = Some(signal);
            }

            self.cur.instances.push(InstanceBuild {
                field: display_name,
                verilog_name,
                module_source_name: child.name.clone(),
                module_verilog_name: child_verilog_name.clone(),
                params: params.clone(),
                source: span.clone(),
                child_ports,
                connections: vec![None; port_count],
                read_outputs: HashSet::new(),
            });
        }

        let entry = if count == 1 {
            FieldInstances::Single(first)
        } else {
            FieldInstances::Array {
                first,
                len: u32::try_from(count).unwrap_or(u32::MAX),
            }
        };
        self.cur.instance_by_field.insert(field.name.clone(), entry);
        Ok(())
    }

    /// Record that `owner` drives `range` of `signal`, rejecting a second
    /// driver for the same bit.
    ///
    /// Within one process, or within the wiring of one module, repeated writes
    /// to the same bit are legal: the last one wins in a process,
    /// while continuous assignments to the same bit are a real conflict and are
    /// rejected.
    fn claim(
        &mut self,
        signal: SignalId,
        range: Option<(u32, u32)>,
        owner: Owner,
        span: &SourceSpan,
    ) -> Result<(), Diagnostic> {
        let width = self.cur.signal(signal).width();
        let (lo, hi) = range.unwrap_or((0, width));
        let hi = hi.min(width);
        if lo >= hi {
            return Ok(());
        }

        // Find the conflict first, then build the message: the borrow of the
        // owner table must not outlive the loop.
        let conflict = {
            let bits = self
                .cur
                .owners
                .entry(signal)
                .or_insert_with(|| vec![None; width as usize]);
            let mut conflict = None;
            for bit in lo..hi {
                match bits[bit as usize] {
                    None => bits[bit as usize] = Some(owner),
                    Some(existing) if existing == owner && !matches!(owner, Owner::Wiring) => {}
                    Some(existing) => {
                        conflict = Some((bit, existing));
                        break;
                    }
                }
            }
            conflict
        };

        if let Some((bit, existing)) = conflict {
            let name = self.cur.signal(signal).name.clone();
            let existing_label = self.owner_label(existing);
            let new_label = self.owner_label(owner);
            return Err(Diagnostic::error(
                codes::MULTI_DRIVER,
                Some(span.clone()),
                format!(
                    "`{name}` bit {bit} is driven by both {existing_label} and {new_label}; \
                     each bit may have exactly one driver"
                ),
            ));
        }
        self.cur
            .drive_span
            .entry(signal)
            .or_insert_with(|| span.clone());
        Ok(())
    }

    /// Check an assignment width, reporting the diagnostic.
    fn check_assign_width(
        &mut self,
        target: &Ty,
        range: Option<(u32, u32)>,
        value: &Expr,
        span: SourceSpan,
    ) -> Result<(), ()> {
        match check_width(target, range, value, span) {
            Ok(()) => Ok(()),
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                Err(())
            }
        }
    }

    /// [`Compiler::claim`] for callers that accumulate diagnostics themselves.
    fn claim_or_report(
        &mut self,
        signal: SignalId,
        range: Option<(u32, u32)>,
        owner: Owner,
        span: &SourceSpan,
    ) -> Result<(), ()> {
        match self.claim(signal, range, owner, span) {
            Ok(()) => Ok(()),
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                Err(())
            }
        }
    }

    fn owner_label(&self, owner: Owner) -> String {
        match owner {
            Owner::Wiring => "the continuous wiring in `run`".to_string(),
            Owner::Process(index) => match self.cur.process_labels.get(index) {
                Some(label) => format!("process `{label}`"),
                None => format!("process #{index}"),
            },
            Owner::Instance(index) => match self.cur.instances.get(index) {
                Some(instance) => format!("instance `{}`", instance.field),
                None => format!("instance #{index}"),
            },
        }
    }

    /// Declare a module-scope signal, rejecting duplicate names.
    fn declare_signal(
        &mut self,
        name: &str,
        source_name: Option<String>,
        ty: Ty,
        kind: SignalKind,
        source: Option<SourceSpan>,
    ) -> Result<SignalId, ()> {
        if let Err(diagnostic) = check_verilog_name(name, "signal", source.clone()) {
            self.diagnostics.push(diagnostic);
            return Err(());
        }
        if let Some(existing) = self.cur.signal_by_name.get(name) {
            let existing_ty = self.cur.signal(*existing).ty.describe();
            return self.err(
                codes::NAME_COLLISION,
                source,
                format!("`{name}` is already declared in this module as {existing_ty}"),
            );
        }
        if self.cur.memory_by_field.contains_key(name) {
            return self.err(
                codes::NAME_COLLISION,
                source,
                format!("`{name}` is already declared in this module as a memory"),
            );
        }
        let id = self.cur.signals.len();
        self.cur.signals.push(Signal {
            name: name.to_string(),
            source_name,
            ty,
            kind,
            scope: SignalScope::Module,
            source,
        });
        self.cur.signal_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    // ------------------------------------------------------------- finish

    fn finish_module(&mut self, build: ModuleBuild, is_top: bool) -> Result<Module, ()> {
        let mut build = build;
        let module_source = build.struct_source.clone();

        // Output ports driven by a process become registers.
        for port_index in 0..build.ports.len() {
            let signal = build.ports[port_index].signal;
            if build.ports[port_index].dir != PortDir::Out {
                continue;
            }
            let process_driven = build
                .owners
                .get(&signal)
                .map(|owners| {
                    owners
                        .iter()
                        .any(|owner| matches!(owner, Some(Owner::Process(_))))
                })
                .unwrap_or(false);
            if process_driven {
                build.signals[signal].kind = SignalKind::Reg;
            }
        }

        // Internal signals driven by a process become registers as well.
        for signal in 0..build.signals.len() {
            if matches!(build.signals[signal].kind, SignalKind::Port(_)) {
                continue;
            }
            let process_driven = build
                .owners
                .get(&signal)
                .map(|owners| {
                    owners
                        .iter()
                        .any(|owner| matches!(owner, Some(Owner::Process(_))))
                })
                .unwrap_or(false);
            build.signals[signal].kind = if process_driven {
                SignalKind::Reg
            } else {
                SignalKind::Wire
            };
        }

        let mut failed = false;

        // Every bit of every driven signal must be driven by something: an
        // unwritten bit would silently become a latch (or `z`) in Verilog.
        for signal in 0..build.signals.len() {
            let info = build.signals[signal].clone();
            if matches!(info.kind, SignalKind::Port(PortDir::In)) {
                continue;
            }
            // A process temporary's drivenness is established by that process's
            // own path analysis, and a compiler-introduced signal (version wire,
            // re-scoped temporary) is not a source signal at all.
            if matches!(info.scope, SignalScope::Process(_)) || info.source_name.is_none() {
                continue;
            }
            let missing: Vec<u32> = (0..info.width())
                .filter(|bit| {
                    build
                        .owners
                        .get(&signal)
                        .map(|owners| owners[*bit as usize].is_none())
                        .unwrap_or(true)
                })
                .collect();
            if missing.is_empty() {
                continue;
            }
            let span = info.source.clone().unwrap_or_else(|| module_source.clone());
            let what = match info.kind {
                SignalKind::Port(_) => format!("output port `{}`", info.name),
                _ => format!("signal `{}`", info.name),
            };
            let message = if missing.len() as u32 == info.width() {
                format!("{what} is never driven")
            } else {
                format!(
                    "{what} has bits that are never driven: {}",
                    format_bit_runs(&info.name, &missing)
                )
            };
            self.diagnostics
                .push(Diagnostic::error(codes::UNDRIVEN, span, message));
            failed = true;
        }

        // Child inputs must all be connected; unread outputs are reported.
        for instance in &build.instances {
            for (port_index, child_port) in instance.child_ports.iter().enumerate() {
                if child_port.dir.is_input() && instance.connections[port_index].is_none() {
                    self.diagnostics.push(Diagnostic::error(
                        codes::UNCONNECTED_INPUT,
                        instance.source.clone(),
                        format!(
                            "input `{}` of instance `{}` ({}) is not connected",
                            child_port.name, instance.field, instance.module_verilog_name
                        ),
                    ));
                    failed = true;
                }
            }
            for child_port in &instance.child_ports {
                if child_port.dir == PortDir::Out
                    && !instance.read_outputs.contains(&child_port.name)
                {
                    self.diagnostics.push(Diagnostic::warning(
                        codes::UNREAD_OUTPUT,
                        instance.source.clone(),
                        format!(
                            "output `{}` of instance `{}` is never read",
                            child_port.name, instance.field
                        ),
                    ));
                }
            }
        }

        if failed {
            return Err(());
        }

        // Combinational loop detection and cross-module dependency analysis.
        let graph = self.build_graph(&build);
        if let Some(cycle) = graph.find_bit_cycle() {
            let names: Vec<String> = cycle
                .iter()
                .map(|(signal, bit)| {
                    let signal = build.signal(*signal);
                    if signal.width() > 1 {
                        format!("{}[{}]", signal.name, bit)
                    } else {
                        signal.name.clone()
                    }
                })
                .collect();
            let span = cycle
                .iter()
                .find_map(|(signal, _)| build.drive_span.get(signal).cloned())
                .unwrap_or_else(|| module_source.clone());
            return self.err(
                codes::COMB_LOOP,
                Some(span),
                format!("combinational loop: {}", names.join(" -> ")),
            );
        }

        let deps = self.comb_dependency_of_inputs(&build, &graph);
        self.comb_deps.insert(build.verilog_name.clone(), deps);

        // Only now, after the semantic analysis saw the original source owners, a
        // signal driven by continuous logic on some bits and a process on others
        // is split into a public wire and a private register: that rewrite is a
        // Verilog-2005 legality pass, it must not change what the analysis sees.
        if self.split_mixed_signals(&mut build).is_err() {
            failed = true;
        }
        if failed {
            return Err(());
        }

        // Public projections.
        let instances = build
            .instances
            .iter()
            .map(|instance| {
                let connections = instance
                    .child_ports
                    .iter()
                    .enumerate()
                    .filter_map(|(port_index, child_port)| {
                        // Inputs carry the expression the parent drives them
                        // with; an output is connected to the parent-side net
                        // that carries the child's result back. Both have to be
                        // listed, or the net would be left undriven.
                        let (expr, source) = match &instance.connections[port_index] {
                            Some(connection) => connection.clone(),
                            None if child_port.dir == PortDir::Out => {
                                let signal = child_port
                                    .parent_signal
                                    .expect("output nets are created up front");
                                (
                                    Expr {
                                        ty: child_port.ty.clone(),
                                        kind: ExprKind::Signal(signal),
                                        source: instance.source.clone(),
                                    },
                                    instance.source.clone(),
                                )
                            }
                            None => return None,
                        };
                        Some(Connection {
                            port: child_port.name.clone(),
                            expr,
                            source,
                        })
                    })
                    .collect();
                Instance {
                    field: instance.field.clone(),
                    verilog_name: instance.verilog_name.clone(),
                    module_source_name: instance.module_source_name.clone(),
                    module_verilog_name: instance.module_verilog_name.clone(),
                    params: instance.params.clone(),
                    connections,
                    source: instance.source.clone(),
                }
            })
            .collect();

        Ok(Module {
            source_name: build.source_name,
            verilog_name: build.verilog_name,
            params: build.params,
            is_top,
            source: module_source,
            ports: build.ports,
            instances,
            signals: build.signals,
            memories: build.memories,
            body: build.body,
        })
    }

    /// True when every bit this read refers to is driven by a *sequential*
    /// process: a register output cuts the combinational dependency, so the read
    /// cannot close a combinational loop.
    ///
    /// Bits driven by wiring, by a combinational process, by an instance, or by
    /// nothing at all keep the edge: a combinational child still feeds back, and
    /// an input is a real source.
    fn cut_by_register(&self, build: &ModuleBuild, read: ReadRef) -> bool {
        let Some(owners) = build.owners.get(&read.signal) else {
            return false;
        };
        (read.lo..read.hi).all(|bit| match owners[bit as usize] {
            // Only a known sequential process cuts the dependency; a missing
            // record keeps it, so no dependency is dropped for lack of
            // information.
            Some(Owner::Process(index)) => build
                .process_records
                .get(index)
                .is_some_and(|record| !record.comb),
            _ => false,
        })
    }

    /// Dependency graph of one module: `a -> b` means b is computed from a.
    ///
    /// Reads are resolved through the other drivers of the module, so reading a
    /// bit range that another assignment drives follows the value instead of
    /// looking like a self loop; a chain that comes back to where it started is
    /// a real loop and is reported as an edge.
    fn build_graph(&self, build: &ModuleBuild) -> DependencyGraph {
        let mut graph = DependencyGraph::default();

        for record in &build.process_records {
            if !record.comb {
                continue;
            }
            for (read, to, lo, hi) in &record.edges {
                if self.cut_by_register(build, *read) {
                    continue;
                }
                graph.add_edge(read.signal, *to);
                graph.add_bit_edge((read.signal, read.lo, read.hi), (*to, *lo, *hi));
            }
        }

        let drivers = collect_wiring_drivers(build);
        for item in &build.body {
            if let BodyItem::Assign { target, value, .. } = item {
                let (lo, hi) = target
                    .range()
                    .unwrap_or_else(|| (0, build.signal(target.signal()).width()));
                let mut stack = vec![ReadRef {
                    signal: target.signal(),
                    lo,
                    hi,
                }];
                for read in collect_read_refs(value, build) {
                    resolve_wiring_read(
                        &drivers,
                        read,
                        target.signal(),
                        (lo, hi),
                        &mut stack,
                        &mut graph,
                    );
                }
            }
        }

        for instance in &build.instances {
            let Some(dependencies) = self.comb_deps.get(&instance.module_verilog_name) else {
                continue;
            };
            for (port_index, child_port) in instance.child_ports.iter().enumerate() {
                if !child_port.dir.is_input() {
                    continue;
                }
                let Some(dependent_outputs) = dependencies.get(&child_port.name) else {
                    continue;
                };
                let Some((expr, _)) = &instance.connections[port_index] else {
                    continue;
                };
                let reads = collect_read_refs(expr, build);
                for output_name in dependent_outputs {
                    let Some(output_port) = instance
                        .child_ports
                        .iter()
                        .find(|port| port.name == *output_name)
                    else {
                        continue;
                    };
                    let Some(net) = output_port.parent_signal else {
                        continue;
                    };
                    for read in &reads {
                        if self.cut_by_register(build, *read) {
                            continue;
                        }
                        graph.add_edge(read.signal, net);
                        graph.add_bit_edge(
                            (read.signal, read.lo, read.hi),
                            (net, 0, build.signal(net).width()),
                        );
                    }
                }
            }
        }

        graph
    }

    /// For every input port, the output ports that combinationally depend on it.
    fn comb_dependency_of_inputs(
        &self,
        build: &ModuleBuild,
        graph: &DependencyGraph,
    ) -> HashMap<String, Vec<String>> {
        let mut deps = HashMap::new();
        for port in &build.ports {
            if !port.dir.is_input() {
                continue;
            }
            let reachable = graph.reachable_from(port.signal);
            let mut outputs: Vec<String> = build
                .ports
                .iter()
                .filter(|other| other.dir == PortDir::Out && reachable.contains(&other.signal))
                .map(|other| other.verilog_name.clone())
                .collect();
            outputs.sort();
            if !outputs.is_empty() {
                deps.insert(port.verilog_name.clone(), outputs);
            }
        }
        deps
    }

    // ------------------------------------------------------- run() wiring

    fn proc_ref(&self) -> &ProcCtx {
        self.proc.as_ref().expect("inside a process")
    }

    /// Where the statements of the expression currently in flight start.
    ///
    /// A construct that lowers several subexpressions takes the statements out
    /// from this mark: those are the ones the subexpression it just lowered has
    /// to run, and they belong where that subexpression is evaluated.
    fn pending_mark(&self) -> usize {
        self.proc
            .as_ref()
            .map_or(0, |context| context.pending.len())
    }

    /// Take the statements lowered since `mark`, in evaluation order.
    fn take_pending_since(&mut self, mark: usize) -> Vec<Stmt> {
        match self.proc.as_mut() {
            Some(context) => context.pending.split_off(mark),
            None => Vec::new(),
        }
    }

    /// Index of the process being lowered. Only valid while one is active: the
    /// index is fixed when the process starts, so nesting or later processes can
    /// never shift it.
    fn current_process(&self) -> usize {
        self.proc.as_ref().expect("inside a process").index
    }
}

/// Replaces the loop variable of a build-time `for` with its literal value.
struct Subst<'a> {
    name: &'a str,
    value: u64,
}

impl syn::visit_mut::VisitMut for Subst<'_> {
    fn visit_expr_mut(&mut self, node: &mut syn::Expr) {
        syn::visit_mut::visit_expr_mut(self, node);
        let syn::Expr::Path(path) = node else {
            return;
        };
        if !path.path.is_ident(self.name) {
            return;
        }
        let literal = syn::LitInt::new(&self.value.to_string(), proc_macro2::Span::call_site());
        *node = syn::Expr::Lit(syn::ExprLit {
            attrs: Vec::new(),
            lit: syn::Lit::Int(literal),
        });
    }
}
