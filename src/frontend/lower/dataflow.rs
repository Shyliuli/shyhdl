//! Turning a purely combinational process into a data flow of continuous
//! assignments.
//!
//! A process that reads **no signal at all** has an empty sensitivity list, so
//! `always @(*)` never runs: Icarus leaves the output at `x` and Verilator warns
//! `ALWNEVER`. That is not a corner case - `self.y = 0;` is a legal, common
//! design - so such a process is emitted as a chain of `assign`s instead.
//!
//! The translation keeps the blocking order of the source by working on
//! **versions**: every write creates a wire holding the value at that point, and
//! every later read refers to the version current *then*, so a read after an
//! overwrite sees the snapshot and not the final value:
//!
//! ```text
//! y = 0; let x = y; y = 1; z = x;
//!   -> y = 1'b0; x = y; y__v2 = 1'b1; z = x;   // z is 0, not 1
//! ```
//!
//! Control flow is *merged*, never folded: `if c { a } else { b }` becomes
//! `c ? a : b` for every signal the branches write, and `match` becomes a chain
//! of ternaries in arm order. Each version is referenced once per use site, so
//! an unrolled `for x = x + x` stays linear, and no constant evaluator is
//! needed: a condition is just another expression over version wires.
//!
//! Every version is emitted as its own named wire and every target is driven
//! from its last version over the bits this process owned. Nothing is inlined or
//! elided: a version can feed several later ones, and dropping the wrong one
//! changes the design silently.

use super::*;
use crate::frontend::mir::BinOp;

/// The data flow of one process: the version holding each signal's value, and
/// the assignments that produce the versions.
struct Flow {
    /// The wire that currently holds each written signal's value.
    current: Versions,
    /// Continuous assignments, each wire driven exactly once, in source order.
    defs: Vec<(LValue, Expr, SourceSpan)>,
}

type Versions = HashMap<SignalId, SignalId>;

impl Compiler {
    /// Emit `body` as continuous assignments.
    ///
    /// Only called for a process with no external read, which is exactly the
    /// process Verilog cannot trigger.
    pub(super) fn emit_dataflow(
        &mut self,
        body: &[Stmt],
        locals: &[SignalId],
        driven: &HashMap<SignalId, Bits>,
        index: usize,
        label: &str,
        span: &SourceSpan,
    ) -> Result<(), ()> {
        // The process's temporaries become module-scope wires: an assignment
        // drives them now, so they cannot stay inside a block that is no longer
        // emitted.
        for local in locals {
            // Only a temporary this process actually wrote becomes a wire; one it
            // never wrote has no value and no declaration.
            let Some(bits) = driven.get(local) else {
                continue;
            };
            let runs = bits.runs();
            let name = format!("{}__{label}", self.cur.signal(*local).name);
            let unique = self.unique_signal_name(&name);
            let info = &mut self.cur.signals[*local];
            info.name = unique;
            info.source_name = None;
            info.scope = SignalScope::Module;
            info.kind = SignalKind::Wire;
            // The bits this process drove are converted from process ownership to
            // wiring; bits it never wrote stay unowned (they are dead, and a
            // compiler-introduced signal is not part of the driver check).
            for (lo, hi) in runs {
                self.convert_owner(*local, index, lo, hi);
            }
        }

        let mut flow = Flow {
            current: Versions::new(),
            defs: Vec::new(),
        };
        self.flow_statements(body, &mut flow)?;
        self.emit_flow(&flow, driven, index, span);
        Ok(())
    }

    /// Translate statements into version assignments, following blocking order.
    fn flow_statements(&mut self, statements: &[Stmt], flow: &mut Flow) -> Result<(), ()> {
        for statement in statements {
            match statement {
                Stmt::Assign {
                    target,
                    value,
                    source,
                    ..
                } => {
                    let value = self.substitute(value, flow);
                    let signal = target.signal();
                    let (lo, hi) = target
                        .range()
                        .unwrap_or_else(|| (0, self.cur.signal(signal).width()));
                    self.flow_write(signal, lo, hi, value, flow, source)?;
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                    source,
                } => {
                    let condition = self.substitute(cond, flow);
                    let entry = flow.current.clone();
                    let then_state = self.flow_branch(then_body, &entry, &mut flow.defs)?;
                    let else_state = self.flow_branch(else_body, &entry, &mut flow.defs)?;
                    flow.current = self.merge_states(
                        &then_state,
                        &else_state,
                        &condition,
                        &mut flow.defs,
                        source,
                    )?;
                }
                // A memory write is state, so it only exists in a clocked
                // process and never reaches this rewrite.
                Stmt::MemWrite { .. } => return Err(()),
                Stmt::Case {
                    scrutinee,
                    arms,
                    default,
                    source,
                    ..
                } => {
                    let scrutinee = self.substitute(scrutinee, flow);
                    let entry = flow.current.clone();

                    let mut acc = match default {
                        Some(default) => self.flow_branch(default, &entry, &mut flow.defs)?,
                        None => entry.clone(),
                    };
                    // Merge from the last arm back to the first: arm 1's test is
                    // the outermost ternary, so priority is kept.
                    for arm in arms.iter().rev() {
                        let state = self.flow_branch(&arm.body, &entry, &mut flow.defs)?;
                        let condition = self.arm_condition(&scrutinee, &arm.values, source);
                        acc =
                            self.merge_states(&state, &acc, &condition, &mut flow.defs, source)?;
                    }
                    flow.current = acc;
                }
            }
        }
        Ok(())
    }

    /// Lower one branch on its own copy of the entry state. Assignments go into
    /// the shared list: continuous assignments are order independent.
    fn flow_branch(
        &mut self,
        statements: &[Stmt],
        entry: &Versions,
        defs: &mut Vec<(LValue, Expr, SourceSpan)>,
    ) -> Result<Versions, ()> {
        let mut branch = Flow {
            current: entry.clone(),
            defs: mem::take(defs),
        };
        self.flow_statements(statements, &mut branch)?;
        *defs = mem::take(&mut branch.defs);
        Ok(branch.current)
    }

    /// `condition ? then : else` for every signal the branches wrote.
    fn merge_states(
        &mut self,
        then_state: &Versions,
        else_state: &Versions,
        condition: &Expr,
        defs: &mut Vec<(LValue, Expr, SourceSpan)>,
        span: &SourceSpan,
    ) -> Result<Versions, ()> {
        let mut signals: Vec<SignalId> = then_state
            .keys()
            .chain(else_state.keys())
            .copied()
            .collect();
        signals.sort_unstable();
        signals.dedup();
        let mut merged = Versions::new();
        for signal in signals {
            match (
                then_state.get(&signal).copied(),
                else_state.get(&signal).copied(),
            ) {
                (Some(then_wire), Some(else_wire)) if then_wire == else_wire => {
                    merged.insert(signal, then_wire);
                }
                (Some(then_wire), Some(else_wire)) => {
                    let value = self.ternary(signal, then_wire, else_wire, condition, span);
                    let wire = self.new_version_wire(signal, value, defs, span)?;
                    merged.insert(signal, wire);
                }
                // One path never wrote this signal. A module signal cannot be in
                // that position: the coverage check rejects a design that leaves
                // a driven bit unassigned on a path. It can only
                // be a temporary of this process, and a read of that temporary
                // on the path that skipped it is rejected as a read before
                // assignment, so the value is dead on that path; the written
                // version is what the surviving path sees.
                (Some(wire), None) | (None, Some(wire)) => {
                    merged.insert(signal, wire);
                }
                (None, None) => {}
            }
        }
        Ok(merged)
    }

    /// A `condition ? then : else` expression of a signal's type.
    fn ternary(
        &self,
        signal: SignalId,
        then_wire: SignalId,
        else_wire: SignalId,
        condition: &Expr,
        span: &SourceSpan,
    ) -> Expr {
        let ty = self.cur.signal(signal).ty.clone();
        let operand = |wire: SignalId| Expr {
            ty: ty.clone(),
            kind: ExprKind::Signal(wire),
            source: span.clone(),
        };
        let then_expr = operand(then_wire);
        let else_expr = operand(else_wire);
        Expr {
            ty,
            kind: ExprKind::Ternary {
                cond: Box::new(condition.clone()),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
            },
            source: span.clone(),
        }
    }

    /// One write of `[lo, hi)` of `signal`, as a new version.
    ///
    /// The first write of a bit range starts from a zero base, never from the
    /// target signal: reading the target here would make the target its own
    /// driver (a coarse combinational loop even when no bit depends on itself).
    /// Bits that no statement ever writes stay zero-based and are reported by the
    /// driver checks instead of silently becoming state.
    fn flow_write(
        &mut self,
        signal: SignalId,
        lo: u32,
        hi: u32,
        value: Expr,
        flow: &mut Flow,
        source: &SourceSpan,
    ) -> Result<(), ()> {
        let width = self.cur.signal(signal).width();
        let whole = lo == 0 && hi == width;

        let merged = match flow.current.get(&signal).copied() {
            None if whole => value,
            None => self.reassemble(signal, None, value, lo, hi, source),
            Some(_) if whole => value,
            Some(previous) => self.reassemble(signal, Some(previous), value, lo, hi, source),
        };
        let wire = self.new_version_wire(signal, merged, &mut flow.defs, source)?;
        flow.current.insert(signal, wire);
        Ok(())
    }

    /// `{base[hi..W], value, base[0..lo]}`: a partial write over a base value.
    ///
    /// `base` is `None` for the first write of a signal, where the untouched bits
    /// have no previous value and are assembled from zeros.
    fn reassemble(
        &self,
        signal: SignalId,
        base: Option<SignalId>,
        value: Expr,
        lo: u32,
        hi: u32,
        source: &SourceSpan,
    ) -> Expr {
        let width = self.cur.signal(signal).width();
        let part = |lo: u32, hi: u32| match base {
            Some(base) => Expr {
                ty: Ty::UInt(hi - lo),
                kind: ExprKind::Slice {
                    signal: base,
                    lo,
                    hi,
                },
                source: source.clone(),
            },
            None => zero_bits(hi - lo, source),
        };
        let mut parts = Vec::new();
        if hi < width {
            parts.push(part(hi, width));
        }
        parts.push(value);
        if lo > 0 {
            parts.push(part(0, lo));
        }
        Expr {
            ty: self.cur.signal(signal).ty.clone(),
            kind: ExprKind::Concat(parts),
            source: source.clone(),
        }
    }

    /// The condition that selects one `case` arm.
    ///
    /// A `case` value is an ordinary constant. A `casez` value is a pattern with
    /// don't-care bits, so the condition compares only the bits it cares about:
    /// `(scrutinee & care) == value`.
    fn arm_condition(&self, scrutinee: &Expr, values: &[Expr], source: &SourceSpan) -> Expr {
        let mut condition: Option<Expr> = None;
        for value in values {
            let comparison = match &value.kind {
                ExprKind::Pattern {
                    value: bits,
                    care,
                    width,
                } => {
                    let cared = Expr {
                        ty: scrutinee.ty.clone(),
                        kind: ExprKind::Binary {
                            op: BinOp::BitAnd,
                            lhs: Box::new(scrutinee.clone()),
                            rhs: Box::new(Expr {
                                ty: scrutinee.ty.clone(),
                                kind: ExprKind::Const {
                                    value: *care as i128,
                                    width: *width,
                                },
                                source: source.clone(),
                            }),
                        },
                        source: source.clone(),
                    };
                    Expr {
                        ty: Ty::Bool,
                        kind: ExprKind::Binary {
                            op: BinOp::Eq,
                            lhs: Box::new(cared),
                            rhs: Box::new(Expr {
                                ty: scrutinee.ty.clone(),
                                kind: ExprKind::Const {
                                    value: *bits as i128,
                                    width: *width,
                                },
                                source: source.clone(),
                            }),
                        },
                        source: source.clone(),
                    }
                }
                _ => Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(scrutinee.clone()),
                        rhs: Box::new(value.clone()),
                    },
                    source: source.clone(),
                },
            };
            condition = Some(match condition {
                None => comparison,
                Some(previous) => Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Binary {
                        op: BinOp::LogOr,
                        lhs: Box::new(previous),
                        rhs: Box::new(comparison),
                    },
                    source: source.clone(),
                },
            });
        }
        condition.unwrap_or(Expr {
            ty: Ty::Bool,
            kind: ExprKind::Const { value: 0, width: 1 },
            source: source.clone(),
        })
    }

    /// Create a wire holding a new version of `signal`.
    fn new_version_wire(
        &mut self,
        signal: SignalId,
        value: Expr,
        defs: &mut Vec<(LValue, Expr, SourceSpan)>,
        source: &SourceSpan,
    ) -> Result<SignalId, ()> {
        let base = self
            .cur
            .signal(signal)
            .source_name
            .clone()
            .unwrap_or_else(|| self.cur.signal(signal).name.clone());
        let ty = self.cur.signal(signal).ty.clone();
        let name = self.unique_signal_name(&format!("{base}__v{}", defs.len() + 1));
        let id = self.cur.signals.len();
        self.cur.signals.push(Signal {
            name: name.clone(),
            source_name: None,
            ty,
            kind: SignalKind::Wire,
            scope: SignalScope::Module,
            source: Some(source.clone()),
        });
        self.cur.signal_by_name.insert(name, id);
        self.claim_or_report(id, None, Owner::Wiring, source)?;
        defs.push((
            LValue::Signal {
                signal: id,
                range: None,
                source: source.clone(),
            },
            value,
            source.clone(),
        ));
        Ok(id)
    }

    /// Emit the version assignments, then drive every written signal from its
    /// last version over the bit ranges this process owned.
    ///
    /// Every version keeps its own named wire: no inlining and no dead-definition
    /// elimination. A version can be read by several later ones, and eliding the
    /// wrong one silently changes the design, so the chain is emitted as it is.
    fn emit_flow(
        &mut self,
        flow: &Flow,
        driven: &HashMap<SignalId, Bits>,
        index: usize,
        span: &SourceSpan,
    ) {
        for (target, value, source) in &flow.defs {
            self.cur.body.push(BodyItem::Assign {
                target: target.clone(),
                value: value.clone(),
                source: source.clone(),
            });
        }

        let mut targets: Vec<SignalId> = flow.current.keys().copied().collect();
        targets.sort_unstable();
        for signal in targets {
            let version = flow.current[&signal];
            let width = self.cur.signal(signal).width();
            let owned = driven
                .get(&signal)
                .cloned()
                .unwrap_or_else(|| Bits::new(width));
            // Only the bits this process drove are written: another process may
            // own the remaining bits of the same signal, and a whole-vector
            // assignment would collide with it.
            for (lo, hi) in owned.runs() {
                self.convert_owner(signal, index, lo, hi);
                let whole = lo == 0 && hi == width;
                let value = if whole {
                    Expr {
                        ty: self.cur.signal(signal).ty.clone(),
                        kind: ExprKind::Signal(version),
                        source: span.clone(),
                    }
                } else {
                    Expr {
                        ty: Ty::UInt(hi - lo),
                        kind: ExprKind::Slice {
                            signal: version,
                            lo,
                            hi,
                        },
                        source: span.clone(),
                    }
                };
                self.cur.body.push(BodyItem::Assign {
                    target: LValue::Signal {
                        signal,
                        range: if whole { None } else { Some((lo, hi)) },
                        source: span.clone(),
                    },
                    value,
                    source: span.clone(),
                });
            }
        }
    }

    /// Turn this process's ownership of `[lo, hi)` into wiring ownership.
    ///
    /// Only the bits this process actually drove are converted: a bit owned by
    /// another process keeps its owner.
    fn convert_owner(&mut self, signal: SignalId, index: usize, lo: u32, hi: u32) {
        if let Some(owners) = self.cur.owners.get_mut(&signal) {
            for bit in lo..hi {
                if owners[bit as usize] == Some(Owner::Process(index)) {
                    owners[bit as usize] = Some(Owner::Wiring);
                }
            }
        }
    }

    /// A signal name that is not used yet, with a stable suffix if needed.
    fn unique_signal_name(&self, name: &str) -> String {
        let used = |name: &str| {
            self.cur.signal_by_name.contains_key(name)
                || self.cur.memory_by_field.contains_key(name)
        };
        if !used(name) {
            return name.to_string();
        }
        let mut index = 1;
        loop {
            let candidate = format!("{name}_{index}");
            if !used(&candidate) {
                return candidate;
            }
            index += 1;
        }
    }

    /// Rewrite reads to the version current at this point.
    fn substitute(&self, expr: &Expr, flow: &Flow) -> Expr {
        let kind = match &expr.kind {
            ExprKind::Signal(signal) => {
                ExprKind::Signal(flow.current.get(signal).copied().unwrap_or(*signal))
            }
            ExprKind::Slice { signal, lo, hi } => ExprKind::Slice {
                signal: flow.current.get(signal).copied().unwrap_or(*signal),
                lo: *lo,
                hi: *hi,
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: Box::new(self.substitute(lhs, flow)),
                rhs: Box::new(self.substitute(rhs, flow)),
            },
            ExprKind::Unary { op, operand } => ExprKind::Unary {
                op: *op,
                operand: Box::new(self.substitute(operand, flow)),
            },
            ExprKind::Ternary {
                cond,
                then_expr,
                else_expr,
            } => ExprKind::Ternary {
                cond: Box::new(self.substitute(cond, flow)),
                then_expr: Box::new(self.substitute(then_expr, flow)),
                else_expr: Box::new(self.substitute(else_expr, flow)),
            },
            ExprKind::Concat(elements) => ExprKind::Concat(
                elements
                    .iter()
                    .map(|element| self.substitute(element, flow))
                    .collect(),
            ),
            ExprKind::Replicate { count, value } => ExprKind::Replicate {
                count: *count,
                value: Box::new(self.substitute(value, flow)),
            },
            ExprKind::BitSelect { signal, index } => ExprKind::BitSelect {
                signal: flow.current.get(signal).copied().unwrap_or(*signal),
                index: Box::new(self.substitute(index, flow)),
            },
            // The memory is not a versioned signal; only its address is
            // rewritten, so a read still depends on the address this point sees.
            ExprKind::MemRead { memory, addr } => ExprKind::MemRead {
                memory: *memory,
                addr: Box::new(self.substitute(addr, flow)),
            },
            ExprKind::Const { .. } => expr.kind.clone(),
            ExprKind::Pattern { .. } => expr.kind.clone(),
        };
        Expr {
            ty: expr.ty.clone(),
            kind,
            source: expr.source.clone(),
        }
    }
}

/// A constant of `width` zero bits.
fn zero_bits(width: u32, source: &SourceSpan) -> Expr {
    Expr {
        ty: Ty::UInt(width),
        kind: ExprKind::Const { value: 0, width },
        source: source.clone(),
    }
}
