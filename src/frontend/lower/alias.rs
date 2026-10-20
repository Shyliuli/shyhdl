//! Making a signal that is driven by both continuous and procedural logic legal
//! in Verilog-2005.
//!
//! The language allows disjoint bits of one signal to have different drivers
//!, but Verilog-2005 does not allow a continuous
//! assignment to a `reg`. So a signal with a continuous driver on some bits and
//! a process driver on others is split:
//!
//! ```text
//! y[0] = 1'b0;                            // default combinational logic
//! always! { #[Pedge] self.clk => { y[1] = d; } }
//!
//! // becomes
//! wire [1:0] y;
//! reg  [1:0] y__proc;
//! assign y[0]   = 1'b0;
//! always @(posedge clk) y__proc[1] <= d;  // the process writes the alias
//! assign y[1]   = y__proc[1];             // and the alias drives the signal
//! ```
//!
//! The process writes and reads the alias for the bits it owns, and the original
//! signal for the rest, so blocking semantics inside the process are unchanged.
//! A run-time bit select reads the alias for the bits this instance of the
//! process owns and the signal for the others, as a bounded chain of ternaries.

use super::*;
use crate::frontend::mir::BinOp;

impl Compiler {
    /// Split every signal that mixes continuous and procedural drivers.
    ///
    /// Runs after the driver checks, so a genuinely conflicting design is still
    /// reported by them rather than being papered over here.
    pub(super) fn split_mixed_signals(&mut self, build: &mut ModuleBuild) -> Result<(), ()> {
        let mut mixed = Vec::new();
        for signal in 0..build.signals.len() {
            let Some(owners) = build.owners.get(&signal) else {
                continue;
            };
            let continuous = owners
                .iter()
                .any(|owner| matches!(owner, Some(Owner::Wiring) | Some(Owner::Instance(_))));
            let procedural = owners
                .iter()
                .any(|owner| matches!(owner, Some(Owner::Process(_))));
            if continuous && procedural {
                mixed.push(signal);
            }
        }

        for signal in mixed {
            let width = build.signals[signal].width();
            let mut owned = Bits::new(width);
            let mut owners_of_owned: Vec<Option<Owner>> = vec![None; width as usize];
            if let Some(owners) = build.owners.get(&signal) {
                for bit in 0..width {
                    if let Some(owner @ Owner::Process(_)) = owners[bit as usize] {
                        owned.set_range(bit, bit + 1);
                        owners_of_owned[bit as usize] = Some(owner);
                    }
                }
            }
            let runs = owned.runs();
            // Per-process ownership, taken from the copy above: the original's
            // owner table is rewritten below, and a process must read the alias
            // only for the bits it owns itself.
            let mut per_index: HashMap<usize, Bits> = HashMap::new();
            for (bit, owner) in owners_of_owned.iter().enumerate() {
                if let Some(Owner::Process(index)) = owner {
                    per_index
                        .entry(*index)
                        .or_insert_with(|| Bits::new(width))
                        .set_range(bit as u32, bit as u32 + 1);
                }
            }
            let source = build.signals[signal]
                .source
                .clone()
                .unwrap_or_else(|| build.struct_source.clone());
            let base = build.signals[signal]
                .source_name
                .clone()
                .unwrap_or_else(|| build.signals[signal].name.clone());
            let name = unique_name(build, &format!("{base}__proc"));
            let ty = build.signals[signal].ty.clone();

            let alias = build.signals.len();
            build.signals.push(Signal {
                name: name.clone(),
                source_name: None,
                ty: ty.clone(),
                kind: SignalKind::Reg,
                scope: SignalScope::Module,
                source: Some(source.clone()),
            });
            build.signal_by_name.insert(name, alias);
            build.owners.insert(alias, owners_of_owned);

            // The bits the process owns move to the alias; the original signal
            // keeps only the continuous drivers and becomes a wire.
            if let Some(owners) = build.owners.get_mut(&signal) {
                for bit in 0..width {
                    if matches!(owners[bit as usize], Some(Owner::Process(_))) {
                        owners[bit as usize] = Some(Owner::Wiring);
                    }
                }
            }
            build.signals[signal].kind = SignalKind::Wire;

            // Each process reads the alias only for the bits *it* owns; the bits
            // another process owns are read from the public signal, exactly as the
            // source says. The bridge below covers the union of all processes.
            let per_process: Vec<(String, Vec<(u32, u32)>)> = build
                .process_labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    let runs = per_index.get(&index).map(Bits::runs).unwrap_or_default();
                    (label.clone(), runs)
                })
                .collect();
            for item in build.body.iter_mut() {
                if let BodyItem::Process(process) = item {
                    let own = per_process
                        .iter()
                        .find(|(label, _)| *label == process.label)
                        .map(|(_, runs)| runs.clone())
                        .unwrap_or_default();
                    for statement in process.body.iter_mut() {
                        rewrite_statement(statement, signal, alias, &own, &ty);
                    }
                }
            }

            for (lo, hi) in &runs {
                build.body.push(BodyItem::Assign {
                    target: LValue::Signal {
                        signal,
                        range: Some((*lo, *hi)),
                        source: source.clone(),
                    },
                    value: Expr {
                        ty: Ty::UInt(hi - lo),
                        kind: ExprKind::Slice {
                            signal: alias,
                            lo: *lo,
                            hi: *hi,
                        },
                        source: source.clone(),
                    },
                    source: source.clone(),
                });
            }
        }
        Ok(())
    }
}

/// A name that is not used in the module yet.
fn unique_name(build: &ModuleBuild, name: &str) -> String {
    if !build.signal_by_name.contains_key(name) {
        return name.to_string();
    }
    let mut index = 1;
    loop {
        let candidate = format!("{name}_{index}");
        if !build.signal_by_name.contains_key(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn rewrite_statement(
    statement: &mut Stmt,
    original: SignalId,
    alias: SignalId,
    owned: &[(u32, u32)],
    ty: &Ty,
) {
    match statement {
        Stmt::Assign { target, value, .. } => {
            let LValue::Signal { signal, .. } = target;
            if *signal == original {
                *signal = alias;
            }
            rewrite_expr(value, original, alias, owned, ty);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            rewrite_expr(cond, original, alias, owned, ty);
            for statement in then_body.iter_mut() {
                rewrite_statement(statement, original, alias, owned, ty);
            }
            for statement in else_body.iter_mut() {
                rewrite_statement(statement, original, alias, owned, ty);
            }
        }
        Stmt::Case {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_expr(scrutinee, original, alias, owned, ty);
            for arm in arms.iter_mut() {
                for statement in arm.body.iter_mut() {
                    rewrite_statement(statement, original, alias, owned, ty);
                }
            }
            if let Some(default) = default {
                for statement in default.iter_mut() {
                    rewrite_statement(statement, original, alias, owned, ty);
                }
            }
        }
    }
}

fn rewrite_expr(
    expr: &mut Expr,
    original: SignalId,
    alias: SignalId,
    owned: &[(u32, u32)],
    ty: &Ty,
) {
    let split = Split {
        original,
        alias,
        owned,
        signal_ty: ty,
    };
    let width = ty.width();
    match &mut expr.kind {
        ExprKind::Signal(signal) if *signal == original => {
            *expr = range_view(&split, 0, width, &expr.source, &expr.ty);
        }
        ExprKind::Slice { signal, lo, hi } if *signal == original => {
            let (lo, hi) = (*lo, *hi);
            *expr = range_view(&split, lo, hi, &expr.source, &expr.ty);
        }
        ExprKind::BitSelect { signal, index } if *signal == original => {
            rewrite_expr(index, original, alias, owned, ty);
            let index = index.clone();
            let replaced = dynamic_view(&split, &index, &expr.source);
            *expr = replaced;
        }
        ExprKind::Slice { .. } | ExprKind::Signal(_) => {}
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_expr(lhs, original, alias, owned, ty);
            rewrite_expr(rhs, original, alias, owned, ty);
        }
        ExprKind::Unary { operand, .. } => rewrite_expr(operand, original, alias, owned, ty),
        ExprKind::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            rewrite_expr(cond, original, alias, owned, ty);
            rewrite_expr(then_expr, original, alias, owned, ty);
            rewrite_expr(else_expr, original, alias, owned, ty);
        }
        ExprKind::Concat(elements) => {
            for element in elements {
                rewrite_expr(element, original, alias, owned, ty);
            }
        }
        ExprKind::Replicate { value, .. } => rewrite_expr(value, original, alias, owned, ty),
        ExprKind::BitSelect { index, .. } => rewrite_expr(index, original, alias, owned, ty),
        ExprKind::Const { .. } => {}
        ExprKind::Pattern { .. } => {}
    }
}

/// One signal that has been split into a public wire and a process alias.
struct Split<'a> {
    original: SignalId,
    alias: SignalId,
    /// Bit ranges the process owns, in ascending order.
    owned: &'a [(u32, u32)],
    /// Type of the whole signal.
    signal_ty: &'a Ty,
}

/// Bits `[lo, hi)` of the split signal: the alias where the process owns them,
/// the signal itself elsewhere.
fn range_view(split: &Split<'_>, lo: u32, hi: u32, source: &SourceSpan, ty: &Ty) -> Expr {
    let Split {
        original,
        alias,
        owned,
        signal_ty,
    } = *split;
    let width = signal_ty.width();
    let mut parts = Vec::new();
    let mut bit = lo;
    while bit < hi {
        let run = owned
            .iter()
            .find(|(run_lo, run_hi)| *run_lo <= bit && bit < *run_hi);
        let (end, source_signal) = match run {
            Some((_, run_hi)) => (hi.min(*run_hi), alias),
            None => (
                hi.min(
                    owned
                        .iter()
                        .filter(|(run_lo, _)| *run_lo > bit)
                        .map(|(run_lo, _)| *run_lo)
                        .min()
                        .unwrap_or(hi),
                ),
                original,
            ),
        };
        parts.push(Expr {
            ty: Ty::UInt(end - bit),
            kind: ExprKind::Slice {
                signal: source_signal,
                lo: bit,
                hi: end,
            },
            source: source.clone(),
        });
        bit = end;
    }
    // `ExprKind::Concat` is most significant element first, and the loop above
    // walks from the least significant bit.
    parts.reverse();
    match parts.len() {
        1 => {
            let mut parts = parts;
            let only = parts.pop().expect("one part");
            let kind = if lo == 0 && hi == width {
                // A whole-value read keeps the original type (it may be a struct).
                match only.kind {
                    ExprKind::Slice { signal, .. } => ExprKind::Signal(signal),
                    other => other,
                }
            } else {
                only.kind
            };
            // The part covers exactly the requested range, so the read keeps the
            // type it was written with: signedness, struct layout and all.
            Expr {
                ty: ty.clone(),
                kind,
                source: source.clone(),
            }
        }
        // The parts are cut out of one value, so the value they rebuild has that
        // value's type - signedness included: a signed signal that is driven
        // partly by an alias and partly by the wiring must stay signed, or the
        // operations on it would read its bits as a magnitude.
        _ => Expr {
            ty: ty.clone(),
            kind: ExprKind::Concat(parts),
            source: source.clone(),
        },
    }
}

/// A run-time select on the split signal: the alias for what the process owns,
/// the public signal for the rest.
///
/// An array selects one element at a time, because an element is the unit the
/// values are read in: each arm reads a constant element range (so the read
/// keeps the element's own type), and the last arm is the select on the whole
/// signal, which is what an index outside the array reads - the standard unknown.
fn dynamic_view(split: &Split<'_>, index: &Expr, source: &SourceSpan) -> Expr {
    if let Ty::Array { element, len } = split.signal_ty {
        let width = element.width();
        let mut value = Expr {
            ty: element.as_ref().clone(),
            kind: ExprKind::BitSelect {
                signal: split.original,
                index: Box::new(index.clone()),
            },
            source: source.clone(),
        };
        // An index narrower than the array cannot reach the elements above its
        // own range, so those arms would never be taken.
        let reachable = if index.ty.width() >= 32 {
            *len
        } else {
            (*len).min(1u32 << index.ty.width())
        };
        let constant = |value: u32| Expr {
            ty: index.ty.clone(),
            kind: ExprKind::Const {
                value: i128::from(value),
                width: index.ty.width(),
            },
            source: source.clone(),
        };
        for element_index in (0..reachable).rev() {
            let (bit_lo, bit_hi) = (element_index * width, (element_index + 1) * width);
            // An element this process owns no part of reads the public signal
            // exactly as the fallback does, so it needs no arm: a large array
            // with one owned element stays one ternary, not one per element.
            if !split
                .owned
                .iter()
                .any(|(lo, hi)| *lo < bit_hi && bit_lo < *hi)
            {
                continue;
            }
            let view = range_view(
                split,
                element_index * width,
                (element_index + 1) * width,
                source,
                element,
            );
            let matches = Expr {
                ty: Ty::Bool,
                kind: ExprKind::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(index.clone()),
                    rhs: Box::new(constant(element_index)),
                },
                source: source.clone(),
            };
            value = Expr {
                ty: element.as_ref().clone(),
                kind: ExprKind::Ternary {
                    cond: Box::new(matches),
                    then_expr: Box::new(view),
                    else_expr: Box::new(value),
                },
                source: source.clone(),
            };
        }
        return value;
    }

    let Split {
        original,
        alias,
        owned,
        ..
    } = *split;
    let bit = |signal: SignalId| Expr {
        ty: Ty::Bool,
        kind: ExprKind::BitSelect {
            signal,
            index: Box::new(index.clone()),
        },
        source: source.clone(),
    };
    let mut value = bit(original);
    // Inside the loop: a run bound like `2` has to be written wide enough to
    // hold itself. A `uint<1>` index would otherwise compare against `1'd2`,
    // which Verilog truncates to 0, and the run would never be recognised.
    let constant_width = |bound: u32| (32 - bound.leading_zeros()).max(1).max(index.ty.width());
    // Innermost first: a run appended later tests earlier, which matches no
    // priority requirement here because the runs are disjoint.
    for (lo, hi) in owned.iter().rev() {
        let constant = |value: u32| Expr {
            ty: Ty::UInt(constant_width(value)),
            kind: ExprKind::Const {
                value: i128::from(value),
                width: constant_width(value),
            },
            source: source.clone(),
        };
        let at_or_above = Expr {
            ty: Ty::Bool,
            kind: ExprKind::Binary {
                op: BinOp::Ge,
                lhs: Box::new(index.clone()),
                rhs: Box::new(constant(*lo)),
            },
            source: source.clone(),
        };
        let below = Expr {
            ty: Ty::Bool,
            kind: ExprKind::Binary {
                op: BinOp::Lt,
                lhs: Box::new(index.clone()),
                rhs: Box::new(constant(*hi)),
            },
            source: source.clone(),
        };
        let inside = Expr {
            ty: Ty::Bool,
            kind: ExprKind::Binary {
                op: BinOp::LogAnd,
                lhs: Box::new(at_or_above),
                rhs: Box::new(below),
            },
            source: source.clone(),
        };
        value = Expr {
            ty: Ty::Bool,
            kind: ExprKind::Ternary {
                cond: Box::new(inside),
                then_expr: Box::new(bit(alias)),
                else_expr: Box::new(value),
            },
            source: source.clone(),
        };
    }
    value
}
