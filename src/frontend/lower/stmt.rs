//! Statements, processes and wiring: `run` bodies and `always!`.

use super::*;
use crate::pattern;

impl Compiler {
    /// A child connection written in `run`: structure, never a statement of the
    /// default combinational process.
    pub(super) fn lower_run_connection(&mut self, assign: &syn::ExprAssign) -> Result<(), ()> {
        let span = self.span(assign.span());
        let Ok(WritePlace::ChildInput { instance, port }) = self.resolve_write_place(&assign.left)
        else {
            return self.err(
                codes::UNSUPPORTED,
                span,
                "a child input can only be connected unconditionally in `run`",
            );
        };
        let source = span.unwrap_or_else(|| self.cur.struct_source.clone());
        let child_ty = self.cur.instances[instance].child_ports[port].ty.clone();
        let result = self.lower_expr_top(&assign.right, Some(&child_ty));
        let Some(value) = self.soft(result) else {
            return Err(());
        };
        self.check_assign_width(&child_ty, None, &value, source.clone())?;
        if let Some((_, previous)) = &self.cur.instances[instance].connections[port] {
            let field = self.cur.instances[instance].field.clone();
            let port_name = self.cur.instances[instance].child_ports[port].name.clone();
            let previous = previous.clone();
            return self.err(
                codes::MULTI_DRIVER,
                Some(source),
                format!("input `{port_name}` of instance `{field}` is connected twice (first at {previous})"),
            );
        }
        self.cur.instances[instance].connections[port] = Some((value, source));
        Ok(())
    }

    pub(super) fn lower_process_macro(&mut self, mac: &syn::Macro) -> Result<(), ()> {
        let name = parse::macro_name(mac);
        let span = self
            .span(mac.path.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        match name.as_str() {
            "comb" => self.err(
                codes::UNSUPPORTED,
                Some(span),
                "`comb!` was removed: statements written directly in `run` are the default combinational logic",
            ),
            "always" => self.lower_seq(mac, span),
            "seq" => self.err(
                codes::UNSUPPORTED,
                Some(span),
                "`seq!` does not exist: a sequential process is written with `always!` and its event arms",
            ),
            other => self.err(
                codes::UNSUPPORTED,
                Some(span),
                format!("unknown macro `{other}!` in `run`: expected `always!`"),
            ),
        }
    }

    /// The default combinational process: every behavioural statement written
    /// directly in `run`, in source order, as *one* process.
    ///
    /// One process for the whole body is what makes blocking order meaningful:
    /// a later assignment overwrites an earlier one, and a `let` reads its
    /// operands where it stands. Splitting the body into per-statement wiring
    /// and a process would silently change those values, so it is never done.
    pub(super) fn lower_comb_statements(
        &mut self,
        statements: &[syn::Stmt],
        span: SourceSpan,
    ) -> Result<(), ()> {
        let label = format!("comb{}", self.cur.comb_count);
        self.cur.comb_count += 1;
        let index = self.cur.process_labels.len();
        self.cur.process_labels.push(label.clone());
        let record_index = self.cur.process_records.len();
        self.cur.process_records.push(ProcessRecord {
            comb: true,
            edges: Vec::new(),
        });

        let mut context = ProcCtx::new(true, index);
        context.at_run_top = true;
        self.proc = Some(context);
        let mut body = Vec::new();
        // A broken statement does not hide the next one: independent errors in
        // one process are all reported (`compile_all`), and the process is not
        // emitted at all when anything failed.
        let mut result = Ok(());
        for statement in statements {
            if self.lower_stmt(statement, &mut body).is_err() {
                result = Err(());
            }
        }
        let mut context = self.proc.take().expect("process context");

        if result.is_err() {
            self.cur.process_labels.pop();
            self.cur.process_records.pop();
            self.cur.comb_count -= 1;
            return Err(());
        }

        // A read of a bit this process drives, before the process assigns it, is
        // the process reading its own output: a feedback loop, not a value from
        // anywhere else (REQ-LANG-023). Only bits this process drives matter; a
        // bit owned by another process is a legal external read.
        let mut feedback_failed = false;
        for (signal, lo, hi, source) in &context.feedback_reads {
            let Some(everywhere) = context.anywhere.get(signal) else {
                continue;
            };
            if let Some(bit) = (*lo..*hi).find(|bit| everywhere.get(*bit)) {
                let name = self.cur.signal(*signal).name.clone();
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::INCOMPLETE_PATH,
                        source.clone(),
                        format!("process `{label}` reads `{name}[{bit}]` before assigning it"),
                    )
                    .with_note(
                        format!("`{name}[{bit}]` is driven by this same process, so the read would use its own previous output; assign it first, or read it from a temporary"),
                        None,
                    ),
                );
                feedback_failed = true;
            }
        }
        if feedback_failed {
            self.cur.process_labels.pop();
            self.cur.process_records.pop();
            self.cur.comb_count -= 1;
            return Err(());
        }

        // Every driven bit must be assigned on every path, otherwise the always
        // block would have to remember its old value (REQ-LANG-015).
        let mut failed = false;
        for (signal, everywhere) in &context.anywhere {
            if !matches!(self.cur.signal(*signal).scope, SignalScope::Module) {
                continue;
            }
            let definite = context
                .definite
                .get(signal)
                .cloned()
                .unwrap_or_else(|| Bits::new(self.cur.signal(*signal).width()));
            if let Some(bit) = definite.first_missing(everywhere) {
                let name = self.cur.signal(*signal).name.clone();
                let source = self
                    .cur
                    .signal(*signal)
                    .source
                    .clone()
                    .unwrap_or_else(|| span.clone());
                self.diagnostics.push(Diagnostic::error(
                    codes::INCOMPLETE_PATH,
                    span.clone(),
                    format!("process `{label}` does not drive `{name}[{bit}]` on every path"),
                ).with_note(
                    format!("`{name}` is read here as a value that would have to be remembered: assign it unconditionally (or in every branch)"),
                    source,
                ));
                failed = true;
            }
        }
        if failed {
            return Err(());
        }

        if context.drives.is_empty() {
            self.diagnostics.push(Diagnostic::warning(
                codes::EMPTY_PROCESS,
                span.clone(),
                format!("process `{label}` drives no signal"),
            ));
        }

        // No read at all means an empty sensitivity list: `always @(*)` would
        // never run (Icarus leaves `x`, Verilator reports ALWNEVER), so such a
        // process is emitted as continuous assignments instead. The label and
        // its record are taken back so nothing refers to a process that is not
        // emitted.
        if context.edges.is_empty() {
            self.cur.process_labels.pop();
            self.cur.process_records.pop();
            self.cur.comb_count -= 1;
            let locals = context.local_list.clone();
            let driven = context.anywhere.clone();
            let index = context.index;
            return self.emit_dataflow(&body, &locals, &driven, index, &label, &span);
        }

        if let Some(record) = self.cur.process_records.get_mut(record_index) {
            record.edges = mem::take(&mut context.edges);
        }

        self.cur.body.push(BodyItem::Process(Process {
            label,
            kind: ProcessKind::Comb,
            locals: context.local_list,
            body,
            source: span,
        }));
        Ok(())
    }

    /// `always!`, in either spelling.
    ///
    /// The event form is the canonical one:
    ///
    /// ```text
    /// always! {
    ///     #[Nedge] self.rst_n => { self.q = 0; }
    ///     #[Pedge] self.clk => { if self.enable { self.q = self.d; } }
    /// }
    /// ```
    ///
    /// Where an arm stands says what it is: the arms before the last one are the
    /// asynchronous resets, in source order (the first has the highest priority),
    /// and the last arm is the regular edge branch. Nothing is inferred from a
    /// signal name or an edge polarity, so a clock is an ordinary `#[In]` and a
    /// single arm needs no marking. An edge chain (`if posedge(self.clk) { ... }`)
    /// is accepted as an equivalent spelling: every branch before the last is a
    /// reset and the last edge branch is the regular one.
    pub(super) fn lower_seq(&mut self, mac: &syn::Macro, span: SourceSpan) -> Result<(), ()> {
        if parse::has_event_arms(&mac.tokens) {
            let arms = match parse::parse_seq_arms(mac) {
                Ok(arms) => arms,
                Err(error) => {
                    return self.err(
                        codes::SEQ_SHAPE,
                        span_in(&self.source_name, error.span()),
                        error.to_string(),
                    )
                }
            };
            self.lower_seq_arms(&arms, span)
        } else {
            let block = match parse::macro_body_block(&self.source_name, mac) {
                Ok(block) => block,
                Err(diagnostic) => return self.error(diagnostic),
            };
            self.lower_seq_chain(&block, span)
        }
    }

    /// The event form of `always!`.
    pub(super) fn lower_seq_arms(
        &mut self,
        arms: &[parse::SeqArm],
        span: SourceSpan,
    ) -> Result<(), ()> {
        if arms.is_empty() {
            return self.err(codes::SEQ_SHAPE, Some(span), "`always!` has no arms");
        }

        struct Resolved {
            edge: Edge,
            signal: SignalId,
            body: syn::Block,
        }

        let mut resolved = Vec::new();
        for arm in arms {
            let arm_span = self.span(arm.span).unwrap_or_else(|| span.clone());
            let edge = match arm.edge {
                parse::ArmEdge::Posedge => Edge::Posedge,
                parse::ArmEdge::Negedge => Edge::Negedge,
            };
            self.reads.clear();
            let result = self.resolve_read_place(&arm.signal);
            let Some(place) = self.soft(result) else {
                return Err(());
            };
            let ReadPlace::Signal { signal, range, ty } = place;
            if range.is_some() {
                return self.err(
                    codes::SEQ_SHAPE,
                    Some(arm_span),
                    "the signal of an `always!` arm must be a whole signal, not a bit range",
                );
            }
            if ty != Ty::Bool {
                let name = self.cur.signal(signal).name.clone();
                let found = ty.describe();
                return self.err(
                    codes::SEQ_SHAPE,
                    Some(arm_span),
                    format!("`{name}` must be a `bool` signal to be an event, found {found}"),
                );
            }
            match self.cur.signal(signal).kind {
                SignalKind::Port(PortDir::In) => {}
                SignalKind::Port(PortDir::Out) => {
                    let name = self.cur.signal(signal).name.clone();
                    return self.err(
                        codes::SEQ_SHAPE,
                        Some(arm_span),
                        format!("`{name}` is an output port: an event must come from an input"),
                    );
                }
                _ => {
                    let name = self.cur.signal(signal).name.clone();
                    return self.err(
                        codes::SEQ_SHAPE,
                        Some(arm_span),
                        format!("`{name}` is not a port of this module, so it cannot be an event"),
                    );
                }
            }
            resolved.push(Resolved {
                edge,
                signal,
                body: arm.body.clone(),
            });
        }

        // The last arm is the regular edge branch; every arm before it is an
        // asynchronous reset, in source order. Neither a signal name nor an edge
        // polarity has a say in that, so a clock is an ordinary `#[In]` and a
        // single arm is a plain edge-triggered process.
        let clock_index = resolved.len() - 1;

        let clock = ClockSpec {
            signal: resolved[clock_index].signal,
            edge: resolved[clock_index].edge,
        };
        let reset_indices: Vec<usize> = (0..clock_index).collect();
        let resets: Vec<ResetSpec> = reset_indices
            .iter()
            .map(|index| ResetSpec {
                signal: resolved[*index].signal,
                edge: resolved[*index].edge,
            })
            .collect();

        let label = format!("seq{}", self.cur.seq_count);
        self.cur.seq_count += 1;
        self.cur.process_labels.push(label.clone());
        self.cur.process_records.push(ProcessRecord {
            comb: false,
            edges: Vec::new(),
        });
        let index = self.cur.process_labels.len() - 1;
        self.proc = Some(ProcCtx::new(false, index));

        let mut reset_bodies = Vec::new();
        for index in &reset_indices {
            let mut body = Vec::new();
            self.lower_stmts(&resolved[*index].body.stmts, &mut body)?;
            reset_bodies.push(body);
        }
        let mut clock_body = Vec::new();
        self.lower_stmts(&resolved[clock_index].body.stmts, &mut clock_body)?;
        let context = self.proc.take().expect("process context");

        let body = self.nest_resets(&resets, reset_bodies, clock_body, &span);
        self.cur.body.push(BodyItem::Process(Process {
            label,
            kind: ProcessKind::Seq { clock, resets },
            locals: context.local_list,
            body,
            source: span,
        }));
        Ok(())
    }

    /// Wrap the clock body in one `if` per asynchronous reset, in written
    /// order: the first reset has the highest priority (REQ-LANG-020).
    pub(super) fn nest_resets(
        &mut self,
        resets: &[ResetSpec],
        reset_bodies: Vec<Vec<Stmt>>,
        clock_body: Vec<Stmt>,
        span: &SourceSpan,
    ) -> Vec<Stmt> {
        let mut body = clock_body;
        for (reset, reset_body) in resets.iter().zip(reset_bodies).rev() {
            let ty = self.cur.signal(reset.signal).ty.clone();
            let operand = Expr {
                ty: ty.clone(),
                kind: ExprKind::Signal(reset.signal),
                source: span.clone(),
            };
            // The event says which transition enters the arm, so the level that
            // keeps it asserted is `!signal` after a negative edge and `signal`
            // after a positive one. This is derived from the declared event, not
            // guessed from the signal's name or polarity.
            let condition = match reset.edge {
                Edge::Negedge => Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Unary {
                        op: crate::frontend::mir::UnOp::Not,
                        operand: Box::new(operand),
                    },
                    source: span.clone(),
                },
                Edge::Posedge => operand,
            };
            body = vec![Stmt::If {
                cond: condition,
                then_body: reset_body,
                else_body: body,
                source: span.clone(),
            }];
        }
        body
    }

    pub(super) fn lower_seq_chain(
        &mut self,
        block: &syn::Block,
        span: SourceSpan,
    ) -> Result<(), ()> {
        // The body must be one edge chain: resets first, the clock branch last.
        let Some(statement) = block.stmts.first() else {
            return self.err(codes::SEQ_SHAPE, Some(span), "`always!` has an empty body");
        };
        if block.stmts.len() > 1 {
            return self.err(
                codes::SEQ_SHAPE,
                Some(span),
                "`always!` takes exactly one edge chain: `if negedge(self.rst_n) { ... } else if posedge(self.clk) { ... }`",
            );
        }
        let syn::Stmt::Expr(syn::Expr::If(_), _) = statement else {
            return self.err(
                codes::SEQ_SHAPE,
                Some(span),
                "`always!` must contain one `if posedge(self.clk) { ... }` (with an optional leading \
                 `if negedge(self.rst_n) { ... } else if` for an asynchronous reset)",
            );
        };

        let mut branches: Vec<(&syn::Expr, &syn::Block)> = Vec::new();
        let mut trailing_else: Option<&syn::Block> = None;
        let mut current = match statement {
            syn::Stmt::Expr(syn::Expr::If(item), _) => Some(item),
            _ => None,
        };
        while let Some(item) = current {
            branches.push((&item.cond, &item.then_branch));
            match &item.else_branch {
                Some((_, else_expr)) => match &**else_expr {
                    syn::Expr::If(inner) => current = Some(inner),
                    syn::Expr::Block(inner) => {
                        trailing_else = Some(&inner.block);
                        current = None;
                    }
                    other => {
                        return self.err(
                            codes::SEQ_SHAPE,
                            self.span(other.span()),
                            "the `else` of an `always!` edge chain must be a block or another `else if`",
                        )
                    }
                },
                None => current = None,
            }
        }

        // Read the edge chain: the last branch is the regular edge branch and
        // every earlier branch is an asynchronous reset.
        let mut edges: Vec<(Edge, SignalId, Option<SourceSpan>)> = Vec::new();
        for (condition, _) in &branches {
            let Some((edge, signal_expr)) = edge_condition(condition) else {
                return self.err(
                    codes::SEQ_SHAPE,
                    self.span(condition.span()),
                    "every branch of an `always!` edge chain must be `posedge(signal)` or `negedge(signal)`",
                );
            };
            let signal_span = self.span(signal_expr.span());
            self.reads.clear();
            let result = self.resolve_read_place(signal_expr);
            let Some(place) = self.soft(result) else {
                return Err(());
            };
            let ReadPlace::Signal { signal, range, ty } = place;
            if range.is_some() {
                return self.err(
                    codes::SEQ_SHAPE,
                    signal_span,
                    "the clock and reset of `always!` must be whole signals, not bit ranges",
                );
            }
            if ty != Ty::Bool {
                let name = self.cur.signal(signal).name.clone();
                let found = ty.describe();
                return self.err(
                    codes::SEQ_SHAPE,
                    signal_span,
                    format!("clock/reset `{name}` must be a `bool` signal, found {found}"),
                );
            }
            edges.push((edge, signal, signal_span));
        }

        // Control flow says which branch is which: every branch before the last
        // is an asynchronous reset, and the last edge branch is the regular edge
        // branch (the clock). Neither a signal name nor an edge polarity decides
        // a role, so a plain `#[In]` is all an event signal needs.
        if trailing_else.is_some() {
            return self.err(
                codes::SEQ_SHAPE,
                Some(span.clone()),
                "a bare `else` is not allowed after the last edge branch of `always!`",
            );
        }
        let clock_index = branches.len() - 1;
        let resets: Vec<usize> = (0..clock_index).collect();
        for index in &resets {
            match self.cur.signal(edges[*index].1).kind {
                SignalKind::Port(PortDir::In) => {}
                SignalKind::Port(PortDir::Out) => {
                    let name = self.cur.signal(edges[*index].1).name.clone();
                    return self.err(
                        codes::SEQ_SHAPE,
                        edges[*index].2.clone(),
                        format!("`{name}` is an output port: an event must come from an input"),
                    );
                }
                _ => {
                    let name = self.cur.signal(edges[*index].1).name.clone();
                    return self.err(
                        codes::SEQ_SHAPE,
                        edges[*index].2.clone(),
                        format!("reset `{name}` must be an input port of this module"),
                    );
                }
            }
        }

        let clock = ClockSpec {
            signal: edges[clock_index].1,
            edge: edges[clock_index].0,
        };
        let reset_specs: Vec<ResetSpec> = resets
            .iter()
            .map(|index| ResetSpec {
                signal: edges[*index].1,
                edge: edges[*index].0,
            })
            .collect();

        let label = format!("seq{}", self.cur.seq_count);
        self.cur.seq_count += 1;
        self.cur.process_labels.push(label.clone());
        self.cur.process_records.push(ProcessRecord {
            comb: false,
            edges: Vec::new(),
        });

        let index = self.cur.process_labels.len() - 1;
        self.proc = Some(ProcCtx::new(false, index));

        // Asynchronous reset bodies first: they keep priority in the generated
        // `if (reset) ... else ...` chain, in written order (REQ-LANG-020).
        let mut reset_bodies = Vec::new();
        let mut result = Ok(());
        for index in &resets {
            let mut body = Vec::new();
            result = self.lower_stmts(&branches[*index].1.stmts, &mut body);
            reset_bodies.push(body);
            if result.is_err() {
                break;
            }
        }
        let mut clock_body = Vec::new();
        if result.is_ok() {
            result = self.lower_stmts(&branches[clock_index].1.stmts, &mut clock_body);
        }
        let context = self.proc.take().expect("process context");
        result?;

        let body = self.nest_resets(&reset_specs, reset_bodies, clock_body, &span);

        self.cur.body.push(BodyItem::Process(Process {
            label,
            kind: ProcessKind::Seq {
                clock,
                resets: reset_specs,
            },
            locals: context.local_list,
            body,
            source: span,
        }));
        Ok(())
    }

    // ----------------------------------------------------------- statements

    /// Lower a nested block: a `let` there is a temporary of this process, not a
    /// module signal, whatever the enclosing statement is.
    fn lower_nested(&mut self, statements: &[syn::Stmt], out: &mut Vec<Stmt>) -> Result<(), ()> {
        let saved = self.proc_ref().at_run_top;
        self.proc.as_mut().expect("process context").at_run_top = false;
        let result = self.lower_stmts(statements, out);
        self.proc.as_mut().expect("process context").at_run_top = saved;
        result
    }

    pub(super) fn lower_stmts(
        &mut self,
        statements: &[syn::Stmt],
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        for statement in statements {
            self.lower_stmt(statement, out)?;
        }
        Ok(())
    }

    pub(super) fn lower_stmt(
        &mut self,
        statement: &syn::Stmt,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        match statement {
            syn::Stmt::Local(local) => self.lower_process_local(local, statement, out),
            syn::Stmt::Macro(mac) => {
                let name = parse::macro_name(&mac.mac);
                self.err(
                    codes::UNSUPPORTED,
                    self.span(mac.mac.path.span()),
                    format!("`{name}!` is not allowed inside a process"),
                )
            }
            syn::Stmt::Expr(expr, _) => self.lower_process_expr(expr, out),
            _ => self.err(
                codes::UNSUPPORTED,
                self.span(statement.span()),
                "unsupported statement inside a process",
            ),
        }
    }

    pub(super) fn lower_process_local(
        &mut self,
        local: &syn::Local,
        statement: &syn::Stmt,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self.span(statement.span());
        let (pattern, annotation) = split_let_pattern(&local.pat);
        if is_destructuring_pattern(pattern) {
            return self.lower_destructuring(pattern, annotation, local, out);
        }
        let name = match pattern_name(pattern) {
            Ok((name, _)) => name,
            Err(message) => return self.err(codes::UNSUPPORTED, span, message),
        };

        // A `let` written directly in `run` is a module signal: the whole body
        // of `run` is one process, and a name declared at its top level is
        // visible to `always!` and to connections exactly as it is written.
        // A `let` inside control flow stays a blocking temporary scoped to its
        // block.
        if self.proc_ref().at_run_top {
            return self.lower_run_let(&name, annotation, local, out);
        }

        let init = local.init.as_ref();
        let target_span = span
            .clone()
            .unwrap_or_else(|| self.cur.struct_source.clone());
        // Temporaries the initialiser itself declares: one of them may become the
        // binding instead of being copied into a second signal.
        let locals_before = self.proc_ref().local_list.len();
        // The initialiser is lowered *before* the new name is bound, as in Rust:
        // `let t = t + 1;` reads the outer `t`, not the one being declared. It is
        // lowered exactly once: with no annotation its own type is the declared
        // type, so lowering it a second time against that type would run its side
        // effects twice.
        let (ty, inferred) = match annotation {
            Some(ty) => {
                let annotation_span = span
                    .clone()
                    .unwrap_or_else(|| self.cur.struct_source.clone());
                let resolved = self.resolve_type(ty, annotation_span);
                match self.soft(resolved) {
                    Some(ty) => (ty, None),
                    None => return Err(()),
                }
            }
            None => {
                let Some(init) = init else {
                    return self.err(
                        codes::BAD_TYPE,
                        span,
                        format!("`let {name};` inside a process needs a type annotation"),
                    );
                };
                self.reads.clear();
                let result = self.lower_expr_top(&init.expr, None);
                let Some(value) = self.soft(result) else {
                    return Err(());
                };
                (value.ty.clone(), Some(value))
            }
        };

        let value = match inferred {
            Some(value) => {
                self.check_assign_width(&ty, None, &value, target_span.clone())?;
                Some(value)
            }
            None => match init {
                Some(init) => {
                    let result = self.lower_expr_top(&init.expr, Some(&ty));
                    let Some(value) = self.soft(result) else {
                        return Err(());
                    };
                    self.check_assign_width(&ty, None, &value, target_span.clone())?;
                    Some(value)
                }
                None => None,
            },
        };

        // The initialiser already produced the value: when that value is one of
        // the temporaries it just created - the result of a `match` or of an `if`
        // - the binding takes that temporary over instead of copying it into a
        // second signal. Same bits, same assignments, one declaration fewer.
        if let Some(value) = &value {
            if let ExprKind::Signal(id) = value.kind {
                if self.proc_ref().local_list[locals_before..].contains(&id) {
                    return self.adopt_local(&name, id, span);
                }
            }
        }

        let signal = self.declare_local(&name, ty.clone(), span.clone())?;
        if let Some(value) = value {
            let target = LValue::Signal {
                signal,
                range: None,
                source: target_span.clone(),
            };
            let index = self.current_process();
            self.claim_or_report(signal, None, Owner::Process(index), &target_span)?;
            self.record_process_assign(&target, &value);
            self.drain_pending(out);
            out.push(Stmt::Assign {
                target,
                value,
                nonblocking: false,
                source: target_span,
            });
        }
        Ok(())
    }

    /// A `let` whose pattern destructures a tuple or a struct.
    ///
    /// The initialiser is lowered once and stored in one hidden temporary; each
    /// binding is then a slice of that temporary. Lowering it once matters: a
    /// `match` or an `if` in the initialiser must not run once per binding.
    ///
    /// At the top level of `run` the bindings are module signals, exactly like a
    /// plain `let` there; a nested `let` declares process locals.
    fn lower_destructuring(
        &mut self,
        pattern: &syn::Pat,
        annotation: Option<&syn::Type>,
        local: &syn::Local,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self.span(local.span());
        let target_span = self.span_or_default(span.clone());
        let init = local.init.as_ref();

        // The pattern's type comes from the annotation, or from the initialiser
        // when there is none. The initialiser is lowered exactly once.
        let (ty, inferred) = match annotation {
            Some(annotation) => {
                let result = self.resolve_type(annotation, target_span.clone());
                match self.soft(result) {
                    Some(ty) => (ty, None),
                    None => return Err(()),
                }
            }
            None => {
                let Some(init) = init else {
                    return self.err(
                        codes::BAD_TYPE,
                        span,
                        "this `let` needs a type annotation: there is no initialiser to infer the pattern's type from",
                    );
                };
                let result = self.lower_expr_top(&init.expr, None);
                let Some(value) = self.soft(result) else {
                    return Err(());
                };
                (value.ty.clone(), Some(value))
            }
        };

        let value = match inferred {
            Some(value) => {
                self.check_assign_width(&ty, None, &value, target_span.clone())?;
                Some(value)
            }
            None => match init {
                Some(init) => {
                    let result = self.lower_expr_top(&init.expr, Some(&ty));
                    let Some(value) = self.soft(result) else {
                        return Err(());
                    };
                    self.check_assign_width(&ty, None, &value, target_span.clone())?;
                    Some(value)
                }
                None => None,
            },
        };

        let Some(value) = value else {
            // No initialiser: the pattern only declares its bindings.
            return self.bind_pattern(pattern, &ty, None, &mut Vec::new(), out);
        };

        // One hidden temporary holds the whole value, so the initialiser's
        // statements stay in one place however many names the pattern binds.
        let temp = match self.materialize(value, &target_span) {
            Ok(signal) => signal,
            Err(diagnostic) => return self.error(diagnostic),
        };
        self.drain_pending(out);
        self.bind_pattern(pattern, &ty, Some(temp), &mut Vec::new(), out)
    }

    /// Bind one pattern against the value at `path` of `root_ty`.
    ///
    /// `temp` is the hidden temporary holding the whole value, when the `let`
    /// has an initialiser; without one the bindings are only declared.
    fn bind_pattern(
        &mut self,
        pattern: &syn::Pat,
        root_ty: &Ty,
        temp: Option<SignalId>,
        path: &mut Vec<Component>,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self
            .span(pattern.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        match pattern {
            syn::Pat::Ident(ident) => {
                if ident.subpat.is_some() {
                    return self.err(
                        codes::UNSUPPORTED,
                        Some(span),
                        "an `@` subpattern is not supported in a `let`",
                    );
                }
                let name = ident.ident.to_string();
                let (range, ty) = self.pattern_part(root_ty, path, &span)?;
                let value = temp.map(|temp| self.range_expr(temp, range, &ty, span.clone()));
                self.declare_binding(&name, ty, value, span, out)
            }
            syn::Pat::Wild(_) => Ok(()),
            syn::Pat::Rest(_) => self.err(
                codes::UNSUPPORTED,
                Some(span),
                "`..` is not a binding; a struct pattern may use it to ignore the remaining fields",
            ),
            syn::Pat::Paren(paren) => self.bind_pattern(&paren.pat, root_ty, temp, path, out),
            // A type inside the pattern is redundant: the pattern's type is
            // already known from the annotation or the initialiser.
            syn::Pat::Type(typed) => self.bind_pattern(&typed.pat, root_ty, temp, path, out),
            syn::Pat::Tuple(tuple) => {
                if tuple.elems.is_empty() {
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span),
                        "the unit pattern `()` has no bits; bind a name instead",
                    );
                }
                if tuple
                    .elems
                    .iter()
                    .any(|element| matches!(element, syn::Pat::Rest(_)))
                {
                    return self.err(
                        codes::UNSUPPORTED,
                        Some(span),
                        "a `..` rest pattern is not supported in a tuple pattern; list every element",
                    );
                }
                let (_, ty) = self.pattern_part(root_ty, path, &span)?;
                let Ty::Struct(strukt) = &ty else {
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("cannot destructure {} as a tuple", ty.describe()),
                    );
                };
                if !strukt.is_tuple() {
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span),
                        format!(
                            "cannot destructure {} as a tuple; write a struct pattern instead",
                            ty.describe()
                        ),
                    );
                }
                if tuple.elems.len() != strukt.fields.len() {
                    return self.err(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!(
                            "this tuple pattern has {} element(s), but {} has {}",
                            tuple.elems.len(),
                            ty.describe(),
                            strukt.fields.len()
                        ),
                    );
                }
                for (index, element) in tuple.elems.iter().enumerate() {
                    path.push(Component::Field(index.to_string()));
                    let result = self.bind_pattern(element, root_ty, temp, path, out);
                    path.pop();
                    result?;
                }
                Ok(())
            }
            syn::Pat::Struct(strukt) => {
                let (_, ty) = self.pattern_part(root_ty, path, &span)?;
                let Ty::Struct(layout) = &ty else {
                    if matches!(ty, Ty::Enum(_)) {
                        return self.err(
                            codes::UNSUPPORTED,
                            Some(span),
                            format!(
                                "destructuring the payload of {} in a `let` is not supported: an enum variant is selected by `match`",
                                ty.describe()
                            ),
                        );
                    }
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("cannot destructure {} with a struct pattern", ty.describe()),
                    );
                };
                if layout.is_tuple() {
                    return self.err(
                        codes::BAD_TYPE,
                        Some(span),
                        format!(
                            "{} is a tuple; write a tuple pattern `(a, b)` instead",
                            ty.describe()
                        ),
                    );
                }
                let mut bound: HashSet<String> = HashSet::new();
                for field in &strukt.fields {
                    let syn::Member::Named(member) = &field.member else {
                        return self.err(
                            codes::UNSUPPORTED,
                            Some(span),
                            "struct pattern fields need names",
                        );
                    };
                    let name = member.to_string();
                    if layout.fields.iter().all(|candidate| candidate.name != name) {
                        let available: Vec<String> = layout
                            .fields
                            .iter()
                            .map(|field| field.name.clone())
                            .collect();
                        let mut diagnostic = Diagnostic::error(
                            codes::UNKNOWN_NAME,
                            Some(span.clone()),
                            format!("{} has no field `{name}`", ty.describe()),
                        );
                        diagnostic = diagnostic.with_note(
                            format!("fields of `{}`: {}", layout.name, available.join(", ")),
                            None,
                        );
                        return self.error(diagnostic);
                    }
                    bound.insert(name.clone());
                    path.push(Component::Field(name));
                    let result = self.bind_pattern(&field.pat, root_ty, temp, path, out);
                    path.pop();
                    result?;
                }
                if strukt.rest.is_none() {
                    let missing: Vec<String> = layout
                        .fields
                        .iter()
                        .filter(|field| !bound.contains(&field.name))
                        .map(|field| field.name.clone())
                        .collect();
                    if !missing.is_empty() {
                        return self.err(
                            codes::DECLARATION,
                            Some(span),
                            format!(
                                "this struct pattern does not bind field(s): {}; add `..` to ignore them",
                                missing.join(", ")
                            ),
                        );
                    }
                }
                Ok(())
            }
            syn::Pat::TupleStruct(tuple) => {
                let name = tuple
                    .path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .unwrap_or_default();
                let mut diagnostic = Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    format!(
                        "destructuring `{name}(...)` in a `let` is not supported: an enum variant is selected by `match`"
                    ),
                );
                diagnostic = diagnostic.with_note(
                    "write `match value { ... }` and bind the payload in the arm",
                    None,
                );
                self.error(diagnostic)
            }
            other => self.err(
                codes::UNSUPPORTED,
                Some(span),
                format!("unsupported pattern in a `let`: {}", describe_pat(other)),
            ),
        }
    }

    /// The bit range and type of one step of a pattern, resolved against the
    /// pattern's whole type.
    fn pattern_part(
        &mut self,
        root_ty: &Ty,
        path: &[Component],
        span: &SourceSpan,
    ) -> Result<(Option<(u32, u32)>, Ty), ()> {
        match self.place_range(root_ty, path, span) {
            Ok(parts) => Ok(parts),
            Err(diagnostic) => self.error(diagnostic),
        }
    }

    /// Declare one binding of a destructuring pattern and, when there is a
    /// value, drive it from the hidden temporary.
    ///
    /// At the top level of `run` the binding is a module signal (the same rule a
    /// plain `let` follows); nested it is a process local.
    fn declare_binding(
        &mut self,
        name: &str,
        ty: Ty,
        value: Option<Expr>,
        span: SourceSpan,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let signal = if self.proc_ref().at_run_top {
            if self.cur.signal_by_name.contains_key(name)
                || self.cur.instance_by_field.contains_key(name)
            {
                let mut diagnostic = Diagnostic::error(
                    codes::NAME_COLLISION,
                    Some(span),
                    format!(
                        "`{name}` is already a signal of this module, so this `let` cannot declare it again"
                    ),
                );
                if self.cur.signal_by_name.contains_key(name) {
                    diagnostic = diagnostic.with_note(
                        format!(
                            "a run-level `let` declares a module signal; rename the local (for example `{name}_v`)"
                        ),
                        None,
                    );
                }
                return self.error(diagnostic);
            }
            self.declare_signal(
                name,
                Some(name.to_string()),
                ty,
                SignalKind::Wire,
                Some(span.clone()),
            )?
        } else {
            self.declare_local(name, ty, Some(span.clone()))?
        };
        let Some(value) = value else {
            return Ok(());
        };
        let target = LValue::Signal {
            signal,
            range: None,
            source: span.clone(),
        };
        let index = self.current_process();
        self.claim_or_report(signal, None, Owner::Process(index), &span)?;
        self.record_process_assign(&target, &value);
        self.drain_pending(out);
        out.push(Stmt::Assign {
            target,
            value,
            nonblocking: false,
            source: span,
        });
        Ok(())
    }

    /// A `let` at the top level of `run`: a module signal, driven by the default
    /// combinational process when it has an initialiser.
    fn lower_run_let(
        &mut self,
        name: &str,
        annotation: Option<&syn::Type>,
        local: &syn::Local,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self.span(local.span());
        let target_span = self.span_or_default(span.clone());
        let init = local.init.as_ref();
        // Temporaries the initialiser itself declares: one of them may become the
        // module signal instead of being copied into a second one.
        let locals_before = self.proc_ref().local_list.len();

        // The initialiser resolves against the enclosing scope: the new signal is
        // declared only after it has been lowered. It is lowered exactly once:
        // with no annotation its own type *is* the declared type, so lowering it
        // again against that type would run its side effects twice (a `match` arm
        // that writes a signal must not run twice).
        let (ty, inferred) = match annotation {
            Some(ty) => {
                let annotation_span = self.span_or_default(span.clone());
                let result = self.resolve_type(ty, annotation_span);
                match self.soft(result) {
                    Some(ty) => (ty, None),
                    None => return Err(()),
                }
            }
            None => {
                let Some(init) = init else {
                    return self.err(
                        codes::BAD_TYPE,
                        span,
                        format!("`let {name};` at the top level of `run` needs a type annotation"),
                    );
                };
                let result = self.lower_expr_top(&init.expr, None);
                let Some(value) = self.soft(result) else {
                    return Err(());
                };
                (value.ty.clone(), Some(value))
            }
        };

        let value = match inferred {
            Some(value) => {
                self.check_assign_width(&ty, None, &value, target_span.clone())?;
                Some(value)
            }
            None => match init {
                Some(init) => {
                    let result = self.lower_expr_top(&init.expr, Some(&ty));
                    let Some(value) = self.soft(result) else {
                        return Err(());
                    };
                    self.check_assign_width(&ty, None, &value, target_span.clone())?;
                    Some(value)
                }
                None => None,
            },
        };

        // A run-level `let` names one module signal. A name that is already a
        // field of this module (`self.q`) or another signal of the same `run`
        // would need a distinct generated name; until that exists the collision
        // is reported, never resolved silently.
        if self.cur.signal_by_name.contains_key(name)
            || self.cur.instance_by_field.contains_key(name)
        {
            let mut diagnostic = Diagnostic::error(
                codes::NAME_COLLISION,
                span.clone(),
                format!("`{name}` is already a signal of this module, so `let {name} = ...` cannot declare it again"),
            );
            if self.cur.signal_by_name.contains_key(name) {
                diagnostic = diagnostic.with_note(
                    format!("a run-level `let` declares a module signal; rename the local (for example `{name}_v`)"),
                    None,
                );
            }
            return self.error(diagnostic);
        }

        // The initialiser already holds its value in a temporary it created:
        // `let x = match ...;` needs no second signal. The temporary becomes the
        // module signal `x` and keeps the assignments it already has.
        if let Some(value) = &value {
            if let ExprKind::Signal(id) = value.kind {
                if self.proc_ref().local_list[locals_before..].contains(&id) {
                    return self.adopt_run_local(name, id, span);
                }
            }
        }

        self.declare_signal(
            name,
            Some(name.to_string()),
            ty.clone(),
            SignalKind::Wire,
            span.clone(),
        )?;
        let signal = self.cur.signal_by_name[name];

        if let Some(value) = value {
            let target = LValue::Signal {
                signal,
                range: None,
                source: target_span.clone(),
            };
            let index = self.current_process();
            self.claim_or_report(signal, None, Owner::Process(index), &target_span)?;
            self.record_process_assign(&target, &value);
            self.drain_pending(out);
            out.push(Stmt::Assign {
                target,
                value,
                nonblocking: false,
                source: target_span,
            });
        }
        Ok(())
    }

    /// `let x = match ...;` at the top level of `run`: the initialiser's
    /// temporary becomes the module signal `x`, exactly as a process-local `let`
    /// takes its temporary over.
    ///
    /// The signal keeps the assignments the initialiser already emitted; only its
    /// name, scope and kind change. The process drives it, so `finish_module`
    /// declares it a `reg` and the driver check sees the process as its owner.
    fn adopt_run_local(
        &mut self,
        name: &str,
        id: SignalId,
        span: Option<SourceSpan>,
    ) -> Result<(), ()> {
        let target_span = self.span_or_default(span.clone());
        if let Err(diagnostic) = check_verilog_name(name, "signal", span) {
            self.diagnostics.push(diagnostic);
            return Err(());
        }
        let index = self.current_process();
        self.claim_or_report(id, None, Owner::Process(index), &target_span)?;
        let old_name = self.cur.signal(id).name.clone();
        {
            let context = self.proc.as_mut().expect("inside a process");
            context.local_list.retain(|local| *local != id);
            context.local_names.retain(|local| *local != old_name);
        }
        let signal = &mut self.cur.signals[id];
        signal.name = name.to_string();
        signal.source_name = Some(name.to_string());
        signal.scope = SignalScope::Module;
        signal.kind = SignalKind::Wire;
        self.cur.signal_by_name.insert(name.to_string(), id);
        Ok(())
    }

    pub(super) fn lower_process_expr(
        &mut self,
        expr: &syn::Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        match expr {
            syn::Expr::Assign(assign) => self.lower_process_assign(assign, expr, out),
            syn::Expr::If(item) => self.lower_process_if(item, expr, out),
            syn::Expr::Match(item) => self.lower_process_match(item, expr, out),
            syn::Expr::ForLoop(item) => self.lower_process_for(item, expr, out),
            syn::Expr::MethodCall(call) if call.method == "run" => self.err(
                codes::UNSUPPORTED,
                self.span(call.span()),
                "a child module does not need `child.run()`: connections are written as assignments in `run`",
            ),
            syn::Expr::While(_) | syn::Expr::Loop(_) => self.err(
                codes::NOT_CONSTANT,
                self.span(expr.span()),
                "a process cannot loop until a runtime condition in this version; hardware loops must have a build-time bound",
            ),
            syn::Expr::Return(_) | syn::Expr::Break(_) | syn::Expr::Continue(_) => self.err(
                codes::UNSUPPORTED,
                self.span(expr.span()),
                "control flow out of a process is not supported",
            ),
            other => self.err(
                codes::UNSUPPORTED,
                self.span(other.span()),
                format!("unsupported statement inside a process ({})", parse::describe_expr(other)),
            ),
        }
    }

    pub(super) fn lower_process_assign(
        &mut self,
        assign: &syn::ExprAssign,
        expression: &syn::Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self.span(expression.span());
        let target_span = span
            .clone()
            .unwrap_or_else(|| self.cur.struct_source.clone());
        let (signal, range, target_ty) = match self.resolve_write_place(&assign.left) {
            Ok(WritePlace::Signal { signal, range, ty }) => (signal, range, ty),
            // One element per arm, not one bit range: the write expands.
            Ok(WritePlace::DynamicWrite(dynamic)) => {
                return self.lower_dynamic_write(dynamic, &assign.right, target_span, out);
            }
            Ok(WritePlace::ChildInput { instance, port }) => {
                let field = self.cur.instances[instance].field.clone();
                let port_name = self.cur.instances[instance].child_ports[port].name.clone();
                return self.err(
                    codes::UNSUPPORTED,
                    Some(target_span),
                    format!(
                        "`{field}.{port_name}` is a child input: connect it in `run` wiring, not inside a process"
                    ),
                );
            }
            Err(diagnostic) => return self.error(diagnostic),
        };

        let ty = target_ty;

        // A `match` that is the whole right-hand side writes the target from
        // inside each arm: no result temporary and no copy back, and each arm
        // gets the width check an assignment would have got.
        if let syn::Expr::Match(item) = &*assign.right {
            let nonblocking = !self.proc_ref().comb
                && matches!(self.cur.signal(signal).scope, SignalScope::Module);
            let target = LValue::Signal {
                signal,
                range,
                source: target_span.clone(),
            };
            self.claim_or_report(
                signal,
                range,
                Owner::Process(self.current_process()),
                &target_span,
            )?;
            let mut state = MatchValue {
                expected: Some(ty.clone()),
                result: None,
                ty: Some(ty),
                into: Some(IntoValue {
                    target,
                    nonblocking,
                }),
            };
            return self.lower_match_core(item, &assign.right, out, Some(&mut state));
        }

        let result = self.lower_expr_top(&assign.right, Some(&ty));
        let Some(value) = self.soft(result) else {
            return Err(());
        };
        self.check_assign_width(&ty, range, &value, target_span.clone())?;

        let nonblocking =
            !self.proc_ref().comb && matches!(self.cur.signal(signal).scope, SignalScope::Module);
        let target = LValue::Signal {
            signal,
            range,
            source: target_span.clone(),
        };
        self.claim_or_report(
            signal,
            range,
            Owner::Process(self.current_process()),
            &target_span,
        )?;
        self.record_process_assign(&target, &value);
        self.drain_pending(out);
        out.push(Stmt::Assign {
            target,
            value,
            nonblocking,
            source: target_span,
        });
        Ok(())
    }

    /// `arr[i] = value`, `arr[i].field = value`, `arr[i][j] = value`, ...
    ///
    /// A run-time index is not an address: each one becomes a `case` over the
    /// elements it can select, and the innermost arms write constant bit ranges.
    /// The analysis therefore sees exactly what the same write with a
    /// build-time index would, with two deliberate differences:
    ///
    /// * `definite` gains nothing. Every arm writes a different element, so the
    ///   intersection over the arms is the state before the write: a
    ///   combinational process that writes through an index still has to drive
    ///   the whole array first, or the untouched elements would latch.
    /// * `anywhere` gains every element the index can reach, and only those: an
    ///   element a narrow index cannot name is not driven here, so another
    ///   process may own it.
    fn lower_dynamic_write(
        &mut self,
        write: DynamicWrite,
        value_expr: &syn::Expr,
        span: SourceSpan,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let mut indices = Vec::new();
        for layer in &write.layers {
            let Some(Component::Bits(index)) = write.components.get(layer.position) else {
                unreachable!("a layer is an index step");
            };
            let lowered = match self.lower_expr_top(index, None) {
                Ok(value) => value,
                Err(diagnostic) => return self.error(diagnostic),
            };
            let Ty::UInt(width) = lowered.ty else {
                return self.err(
                    codes::BAD_TYPE,
                    Some(lowered.source.clone()),
                    format!(
                        "an element index must be an unsigned value, and this is {}",
                        lowered.ty.describe()
                    ),
                );
            };
            // An index narrower than the array can only name the first `2^width`
            // elements, and an arm value that does not fit the index's width
            // would alias another one. A wide index reaches everything, so it
            // takes the array's length as it is (`2^width` would overflow).
            let count = if width >= 32 {
                layer.len
            } else {
                layer.len.min(1u32 << width)
            };
            indices.push(DynamicIndex {
                index: lowered,
                count,
            });
        }

        let combinations = indices.iter().fold(1u64, |total, index| {
            total.saturating_mul(u64::from(index.count))
        });
        let expansion = self.proc_ref().unroll_product.saturating_mul(combinations);
        if expansion > MAX_UNROLL {
            return self.err(
                codes::NOT_CONSTANT,
                Some(span),
                format!(
                    "this run-time element write expands to {expansion} statements, \
                     more than the supported {MAX_UNROLL}"
                ),
            );
        }

        let target_ty = match self.dynamic_write_target(&write, &span) {
            Ok(ty) => ty,
            Err(diagnostic) => return self.error(diagnostic),
        };
        let value = match self.lower_expr_top(value_expr, Some(&target_ty)) {
            Ok(value) => value,
            Err(diagnostic) => return self.error(diagnostic),
        };
        self.check_assign_width(&target_ty, None, &value, span.clone())?;
        self.drain_pending(out);

        // Every arm writes the same value and reads the same index, so the sources
        // are resolved once, against the state as the write found it: the guards
        // the write sits under, the index reads, and the value's own reads.
        // `sources_for` already reports the not-yet-assigned parts as reads of
        // their own ranges, so there is nothing extra to add.
        let definite = self.proc_ref().definite.clone();
        let before_provenance = self.proc_ref().provenance.clone();
        let mut sources: Vec<ReadRef> = Vec::new();
        for read in self.proc_ref().guard_reads.iter().copied() {
            if !sources.contains(&read) {
                sources.push(read);
            }
        }
        for index in &indices {
            for source in self.sources_of(&index.index) {
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
        }
        for read in collect_read_refs(&value, &self.cur) {
            let (resolved, _) = before_provenance.sources_for(read);
            for source in resolved {
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
            // A read of a bit this process drives but has not assigned yet uses its
            // own output: the same candidate a plain assignment records, checked
            // against the process's final drivers when the process finishes.
            for (lo, hi) in unassigned_runs(
                definite.get(&read.signal),
                read,
                self.cur.signal(read.signal).width(),
            ) {
                self.proc
                    .as_mut()
                    .expect("process context")
                    .feedback_reads
                    .push((read.signal, lo, hi, value.source.clone()));
            }
        }

        let mut expansion = DynamicWriteExpansion {
            write: &write,
            indices: &indices,
            value: &value,
            span: &span,
            chosen: vec![0; indices.len()],
            sources,
            leaves: Vec::new(),
        };
        let arms = self.dynamic_write_arms(&mut expansion, 0)?;

        // `anywhere`, `drives` and the dependencies are the union over the arms,
        // so they stay. `definite` gains nothing: only one arm runs, and an index
        // that matches no arm writes nothing (`default: Some(Vec::new())` is that
        // path), so a combinational process that writes through an index still has
        // to drive the whole array itself. The arms change neither `definite` nor
        // `guard_reads` nor the bindings, so none of them has to be put back.
        //
        // Every arm resolves the same sources, so the dependencies are recorded
        // once instead of once per leaf.
        // A run-time index can land on any element, so the dependency covers the
        // whole signal.
        let write_width = self.cur.signal(write.signal).width();
        for source in &expansion.sources {
            self.proc
                .as_mut()
                .expect("process context")
                .edges
                .push((*source, write.signal, 0, write_width));
        }
        // Provenance is tracked per assignment in the default combinational process
        // only: a sequential process reads the old value of everything it writes,
        // so its reads stay reads of the signal and its map is left alone. In the
        // combinational process the leaf ranges replace what they cover, unioned
        // with the sources of the range they replace (an index that matches no leaf
        // keeps that value); every range the leaves do not cover keeps the state
        // before the write.
        if self.proc_ref().comb {
            let mut merged_provenance = before_provenance;
            merged_provenance.splice_signal(write.signal, &expansion.leaves);
            self.proc.as_mut().expect("process context").provenance = merged_provenance;
        }

        out.push(Stmt::Case {
            kind: crate::frontend::mir::CaseKind::Case,
            scrutinee: indices[0].index.clone(),
            arms,
            default: Some(Vec::new()),
            source: span,
        });
        Ok(())
    }

    /// The arms of the run-time index at `depth`: either the next index down, or
    /// the constant element range the innermost combination writes.
    fn dynamic_write_arms(
        &mut self,
        expansion: &mut DynamicWriteExpansion<'_>,
        depth: usize,
    ) -> Result<Vec<crate::frontend::mir::CaseArm>, ()> {
        let mut arms = Vec::new();
        for element in 0..expansion.indices[depth].count {
            expansion.chosen[depth] = element;
            let body = if depth + 1 == expansion.indices.len() {
                vec![self.dynamic_write_assign(expansion)?]
            } else {
                vec![self.dynamic_write_case(expansion, depth + 1)?]
            };

            arms.push(crate::frontend::mir::CaseArm {
                values: vec![index_constant(&expansion.indices[depth].index, element)],
                body,
                source: expansion.indices[depth].index.source.clone(),
            });
        }
        Ok(arms)
    }

    /// The `case` that expands the run-time index at `depth`.
    fn dynamic_write_case(
        &mut self,
        expansion: &mut DynamicWriteExpansion<'_>,
        depth: usize,
    ) -> Result<Stmt, ()> {
        let scrutinee = expansion.indices[depth].index.clone();
        Ok(Stmt::Case {
            kind: crate::frontend::mir::CaseKind::Case,
            arms: self.dynamic_write_arms(expansion, depth)?,
            source: scrutinee.source.clone(),
            scrutinee,
            default: Some(Vec::new()),
        })
    }

    /// The assignment one concrete combination writes.
    fn dynamic_write_assign(
        &mut self,
        expansion: &mut DynamicWriteExpansion<'_>,
    ) -> Result<Stmt, ()> {
        let write = expansion.write;
        let span = expansion.span.clone();
        let path = concrete_path(&write.components, &write.layers, &expansion.chosen);
        let base_ty = self.cur.signal(write.signal).ty.clone();
        let (range, _ty) = match self.place_range(&base_ty, &path, &span) {
            Ok(selected) => selected,
            Err(diagnostic) => return self.error(diagnostic),
        };
        let target = LValue::Signal {
            signal: write.signal,
            range,
            source: span.clone(),
        };
        self.claim_or_report(
            write.signal,
            range,
            Owner::Process(self.current_process()),
            &span,
        )?;
        let read = ReadRef {
            signal: write.signal,
            lo: range.map(|(lo, _)| lo).unwrap_or(0),
            hi: range
                .map(|(_, hi)| hi)
                .unwrap_or_else(|| self.cur.signal(write.signal).width()),
        };
        // This arm's own sources: the reads of the value and the index guard. The
        // value a leaf replaces may also be the one stored before the write (an
        // index that matches no leaf), so `splice_signal` unions these with the
        // sources of the range they cover. Every arm resolves the same list, so
        // the dependencies are recorded once by the caller.
        let sources = expansion.sources.clone();
        expansion.leaves.push((read.lo, read.hi, sources));
        // Path coverage is unaffected by *which* arm runs: every one of them
        // writes its element, and an index that matches no arm writes nothing.
        // `anywhere` is the union over the arms, and it is the one thing that has
        // to grow per leaf, because every leaf covers a different range.
        {
            let context = self.proc.as_mut().expect("process context");
            context
                .anywhere
                .entry(write.signal)
                .or_insert_with(|| Bits::new(self.cur.signal(write.signal).width()))
                .set_range(read.lo, read.hi);
            context.drives.insert(write.signal);
        }
        let nonblocking = !self.proc_ref().comb
            && matches!(self.cur.signal(write.signal).scope, SignalScope::Module);
        Ok(Stmt::Assign {
            target,
            value: expansion.value.clone(),
            nonblocking,
            source: span,
        })
    }

    /// Record data flow and path coverage for an assignment in a process.
    ///
    /// Dependencies are tracked per bit range: a read of bits that were already
    /// assigned in this process resolves to *their* sources (a blocking
    /// temporary), while a read of bits that were not resolves to the signal
    /// itself, which is what makes a genuine loop visible (REQ-LANG-023).
    pub(super) fn record_process_assign(&mut self, target: &LValue, value: &Expr) {
        let signal = target.signal();
        let range = target.range();
        let width = self.cur.signal(signal).width();
        let (lo, hi) = range.unwrap_or((0, width));
        let reads = collect_read_refs(value, &self.cur);

        let Some(context) = self.proc.as_mut() else {
            return;
        };

        // Reads are resolved *before* this assignment marks its target, so a read
        // inside the value (`y = y + 1`) cannot look like it saw the new value.
        // The resolved reads already carry the not-yet-assigned bits as reads of
        // their own ranges, so a dependency is never widened over bits this
        // process assigned itself.
        let mut sources = context.guard_reads.clone();
        for read in reads {
            let (resolved, _) = context.provenance.sources_for(read);
            sources.extend(resolved);
            // `definite` is the authority on "not assigned on some path": a merged
            // entry can cover a range while a branch never wrote it, so gating the
            // candidate on `uncovered` would hide exactly those reads.
            let runs = unassigned_runs(
                context.definite.get(&read.signal),
                read,
                self.cur.signal(read.signal).width(),
            );
            for (lo, hi) in runs {
                context
                    .feedback_reads
                    .push((read.signal, lo, hi, value.source.clone()));
            }
        }

        for map in [&mut context.anywhere, &mut context.definite] {
            map.entry(signal)
                .or_insert_with(|| Bits::new(width))
                .set_range(lo, hi);
        }
        context.drives.insert(signal);
        if !context.comb {
            return;
        }

        // Inside one combinational process an assigned signal holds the value
        // computed so far, so dependencies are tracked per assignment instead of
        // "everything the process reads depends on everything it drives". That
        // keeps a legal blocking overwrite from looking like a loop.
        context
            .provenance
            .write(ReadRef { signal, lo, hi }, sources.clone());
        for source in sources {
            context.edges.push((source, signal, lo, hi));
        }
    }

    /// Sources of the value of an expression, as seen inside a process.
    ///
    /// Used for branch conditions: they become guards of every assignment in the
    /// branch, so a loop through a condition is detected too.
    pub(super) fn sources_of(&mut self, value: &Expr) -> Vec<ReadRef> {
        let reads = collect_read_refs(value, &self.cur);
        let source = value.source.clone();
        let Some(context) = self.proc.as_mut() else {
            return Vec::new();
        };
        let mut sources = Vec::new();
        for read in reads {
            let (resolved, _) = context.provenance.sources_for(read);
            sources.extend(resolved);
            // A guard reads exactly like an assignment value, and `definite` is
            // again the authority on which bits are not assigned yet.
            let runs = unassigned_runs(
                context.definite.get(&read.signal),
                read,
                self.cur.signal(read.signal).width(),
            );
            for (lo, hi) in runs {
                context
                    .feedback_reads
                    .push((read.signal, lo, hi, source.clone()));
            }
        }
        sources
    }

    pub(super) fn lower_process_if(
        &mut self,
        item: &syn::ExprIf,
        expression: &syn::Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self
            .span(expression.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        let result = self.lower_expr_top(&item.cond, Some(&Ty::Bool));
        let Some(condition) = self.soft(result) else {
            return Err(());
        };
        self.drain_pending(out);

        let guard = self.proc_ref().guard_reads.clone();
        let condition_sources = self.sources_of(&condition);
        let branch_guard = {
            let mut extended = guard.clone();
            extended.extend(condition_sources.iter().copied());
            extended
        };
        let before = self.proc_ref().definite.clone();
        let bindings = self.proc_ref().locals.clone();
        // A branch starts from the state as it is; its snapshot joins the merge
        // below, so a read after the `if` sees the union of both paths.
        let before_provenance = self.proc_ref().provenance.clone();

        {
            let context = self.proc.as_mut().expect("process context");
            context.guard_reads = branch_guard.clone();
            context.definite = before.clone();
            context.provenance = before_provenance.clone();
        }
        let mut then_body = Vec::new();
        self.lower_nested(&item.then_branch.stmts, &mut then_body)?;
        let then_state = mem::replace(
            &mut self.proc.as_mut().expect("process context").definite,
            before.clone(),
        );
        let then_provenance = self.proc_ref().provenance.clone();

        {
            // A `let` declared in one branch is scoped to that branch.
            let context = self.proc.as_mut().expect("process context");
            context.guard_reads = branch_guard;
            context.definite = before.clone();
            context.locals = bindings.clone();
            context.provenance = before_provenance.clone();
        }
        let has_else = item.else_branch.is_some();
        let mut else_body = Vec::new();
        if let Some((_, else_expr)) = &item.else_branch {
            match &**else_expr {
                syn::Expr::Block(block) => self.lower_nested(&block.block.stmts, &mut else_body)?,
                // An `else if` is an `if` in the else branch.
                syn::Expr::If(inner) => self.lower_process_if(inner, else_expr, &mut else_body)?,
                other => {
                    return self.err(
                        codes::UNSUPPORTED,
                        self.span(other.span()),
                        "the `else` of an `if` in a process must be a block or another `else if`",
                    )
                }
            }
        }
        let else_state = mem::replace(
            &mut self.proc.as_mut().expect("process context").definite,
            before,
        );
        let else_provenance = self.proc_ref().provenance.clone();
        self.proc.as_mut().expect("process context").locals = bindings;

        // A bit counts as assigned after the `if` only when both paths assigned
        // it, otherwise the process would have to remember its old value
        // (REQ-LANG-015).
        let merged = merge_definite(&then_state, &else_state, |signal| {
            self.cur.signal(signal).width()
        });
        // The sources of a read after the `if` are the union over the paths. An
        // absent `else` is a path of its own, and it is the state before the `if`.
        let paths = match has_else {
            true => vec![then_provenance, else_provenance],
            false => vec![then_provenance, before_provenance],
        };
        let merged_provenance = Provenance::merge(&paths);
        let context = self.proc.as_mut().expect("process context");
        context.guard_reads = guard;
        context.definite = merged;
        context.provenance = merged_provenance;

        out.push(Stmt::If {
            cond: condition,
            then_body,
            else_body,
            source: span,
        });
        Ok(())
    }

    /// `match` inside a process.
    ///
    /// Every `match` becomes a source-ordered `if`/`else if` chain, whether or
    /// not it has guards: arm order is the priority, so the first arm whose
    /// pattern matches *and* whose guard holds runs (REQ-LANG-044). A `case`
    /// statement cannot express that, because its arms do not fall through.
    ///
    /// Patterns are normalized once (the shared `pattern` module) and each
    /// alternative's test - tag plus payload, as [`pattern::Test`] describes -
    /// becomes a condition on the matched value's bits. Coverage is proven over
    /// the typed domain, and a guarded arm neither contributes coverage nor hides
    /// a later arm.
    ///
    /// A binding is a *value*, and an arm's guard reads it, so an arm's branch
    /// starts with an assignment prelude: every bound name is copied out of the
    /// matched value first, and only then is the arm's condition
    /// (`pattern && guard`) evaluated. The prelude sits in the branch that
    /// reaches the arm, so it runs exactly when that arm is tried. When the
    /// alternatives of one arm bind a name at different offsets, each alternative
    /// is tried with its own bindings - every matching alternative is tried before
    /// the arm is rejected - and the value the body reads is the one of the first
    /// alternative whose condition held.
    pub(super) fn lower_process_match(
        &mut self,
        item: &syn::ExprMatch,
        expression: &syn::Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        self.lower_match_core(item, expression, out, None)
    }

    /// The shared `match` lowering.
    ///
    /// `value` picks the form. With `None` the arms run statements, which is what
    /// a `match` written as a statement does. With `Some(state)` every arm also
    /// writes the arm's trailing expression into one result temporary, or - when
    /// `state.into` names a target - directly into that target, so the whole
    /// `match` is a value of `state.ty` and the caller can read it where it
    /// stands. Both forms share the pattern analysis, the guards, the bindings and
    /// the exhaustiveness proof, and the statement form is emitted exactly as it
    /// was before.
    ///
    /// A `match` whose arms are all unguarded plain bit patterns - constants,
    /// `mask!` patterns, variant tags and payloads - becomes one `case`/`casez`
    /// (see [`Compiler::plain_case_arms`]). Everything a `case` cannot say - a
    /// guard, a range pattern, a pattern wider than 128 bits - keeps the explicit
    /// `if` chain, which is also what carries the source-order priority of an
    /// arm whose guard rejects it.
    pub(super) fn lower_match_core(
        &mut self,
        item: &syn::ExprMatch,
        expression: &syn::Expr,
        out: &mut Vec<Stmt>,
        mut form: Option<&mut MatchValue>,
    ) -> Result<(), ()> {
        let span = self
            .span(expression.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        let result = self.lower_expr_top(&item.expr, None);
        let Some(value) = self.soft(result) else {
            return Err(());
        };
        self.drain_pending(out);

        // The matched value is read **once**: every test and every binding reads
        // this copy, so a body that writes the original - a blocking assignment
        // in the default combinational process - cannot change what a later arm
        // sees. A whole signal needs no copy: the tests run in source order, and
        // an arm's body only runs after that arm was selected, so every test
        // still sees the value the `match` found.
        let scrutinee = match value.kind {
            ExprKind::Signal(signal) => signal,
            kind => {
                let rebuilt = Expr {
                    ty: value.ty,
                    kind,
                    source: value.source,
                };
                // The snapshot is named after the signal it copies, when there is
                // one: `sig_reg` says more than a source position.
                let hint = collect_read_refs(&rebuilt, &self.cur).first().map(|read| {
                    let signal = self.cur.signal(read.signal);
                    signal
                        .source_name
                        .clone()
                        .unwrap_or_else(|| signal.name.clone())
                });
                match self.materialize_hinted(rebuilt, hint.as_deref(), &span) {
                    Ok(signal) => signal,
                    Err(diagnostic) => return self.error(diagnostic),
                }
            }
        };
        self.drain_pending(out);
        let scrutinee_ty = self.cur.signal(scrutinee).ty.clone();
        let value_ty = scrutinee_ty.to_value_type();
        let scrutinee_read = self.range_expr(scrutinee, None, &scrutinee_ty, span.clone());
        let scrutinee_width = scrutinee_ty.width();

        // The arms of a `case` compare their own pattern against the whole
        // matched value, so the same arms also fit a `case`/`casez` when nothing
        // but plain bit patterns is involved. `case_arms` collects the pattern of
        // each reachable arm in source order and stays `None` as soon as one of
        // them needs something a `case` cannot express.
        let mut case_arms: Option<Vec<Vec<(u128, u128)>>> = if scrutinee_width <= 128 {
            Some(Vec::new())
        } else {
            None
        };

        // `lower_arm` holds the constant environment while the calls around it
        // need `&mut self`, so the resolver borrows a copy of it.
        let env = self.cur.env.clone();
        let consts = ConstEnvResolver { env: &env };

        let mut coverage = pattern::Coverage::new(&value_ty);
        let mut branch_states: Vec<HashMap<SignalId, Bits>> = Vec::new();
        let mut branch_provenances: Vec<Provenance> = Vec::new();
        // One entry per arm that is still reachable, in source order: the
        // condition that selects it, the prelude that binds it and its body. The
        // first arm's condition is the outermost `if` and every later arm is the
        // `else` branch of the arm before it, so source order is the priority.
        let mut chain: Vec<(Expr, Vec<Stmt>, Vec<Stmt>)> = Vec::new();
        // The arm that matches everything (a `_`, a bare binding, or an `a | b`
        // whose first alternative is one) needs no condition and becomes the
        // `else` of the whole chain.
        let mut default_block: Option<Vec<Stmt>> = None;

        let guard = self.proc_ref().guard_reads.clone();
        let scrutinee_sources = self.sources_of(&scrutinee_read);
        let branch_guard = {
            let mut extended = guard.clone();
            extended.extend(scrutinee_sources.iter().copied());
            extended
        };
        // The state the `match` found: every arm starts from it and is merged
        // back against it.
        let before = self.proc_ref().definite.clone();
        let before_provenance = self.proc_ref().provenance.clone();

        for arm in &item.arms {
            let lowered = match pattern::lower_arm(&arm.pat, &value_ty, &consts) {
                Ok(lowered) => lowered,
                Err(error) => {
                    return self.err(
                        self.pat_code(&error.kind),
                        self.span(error.span),
                        error.message(),
                    )
                }
            };

            // An arm after the one that matches everything can never run. Its
            // pattern is still normalized - a bad pattern is reported - but it
            // contributes nothing to the emitted chain. A dead arm is not an
            // error by itself, and the arms are never reordered to hide it.
            if default_block.is_some() {
                continue;
            }

            // The arm starts from the state the `match` found.
            let bindings = self.proc_ref().locals.clone();
            {
                let context = self.proc.as_mut().expect("process context");
                context.guard_reads = branch_guard.clone();
                context.definite = before.clone();
                context.provenance = before_provenance.clone();
            }

            // Each alternative's own test, on the matched value's bits.
            let mut tests = Vec::with_capacity(lowered.alternatives.len());
            // The same tests, kept as patterns: a `case` arm needs the bits, not
            // the rendered condition.
            let mut patterns = Vec::with_capacity(lowered.alternatives.len());
            let mut always_true = false;
            for alternative in &lowered.alternatives {
                let test = match pattern::pattern_test(alternative, &value_ty) {
                    Ok(test) => test,
                    Err(error) => {
                        return self.err(
                            self.pat_code(&error.kind),
                            self.span(error.span),
                            error.message(),
                        )
                    }
                };
                always_true |= test == pattern::Test::AlwaysTrue;
                patterns.push(pattern::case_pattern(&test));
                tests.push(self.render_test(&test, scrutinee, 0, &span));
            }

            // The names the arm binds. `lower_arm` has proven that every
            // alternative binds the same names with the same types, so the first
            // alternative says which names there are while each alternative keeps
            // its own offsets.
            let names = match lowered.alternatives.first() {
                Some(first) => {
                    let mut names = Vec::with_capacity(first.bindings.len());
                    for binding in &first.bindings {
                        let ty = match Ty::from_value_type(&binding.ty) {
                            Ok(ty) => ty,
                            Err(message) => {
                                return self.err(
                                    codes::BAD_TYPE,
                                    self.span(binding.span),
                                    format!("`{}`: {message}", binding.name),
                                )
                            }
                        };
                        names.push((binding.name.clone(), ty, self.span(binding.span)));
                    }
                    names
                }
                None => Vec::new(),
            };

            // Whether the guard has to be evaluated once per alternative: a name
            // the alternatives bind at *different* offsets holds a different
            // value depending on which one matched, so one shared guard could
            // reject an arm that a later alternative accepts.
            let mut guard_per_alternative = false;
            if arm.guard.is_some() && lowered.alternatives.len() > 1 {
                for (name, _, _) in &names {
                    let mut offsets = lowered.alternatives.iter().map(|alternative| {
                        alternative
                            .bindings
                            .iter()
                            .find(|binding| binding.name == *name)
                            .expect("every alternative binds the same names")
                            .offset
                    });
                    if let Some(first) = offsets.next() {
                        if offsets.any(|offset| offset != first) {
                            guard_per_alternative = true;
                            break;
                        }
                    }
                }
            }

            let (prelude, condition) = if guard_per_alternative {
                // Each alternative's own bindings, as hidden temporaries: the
                // per-alternative conditions below read them.
                let mut per_alternative = Vec::with_capacity(lowered.alternatives.len());
                for alternative in &lowered.alternatives {
                    let mut locals = HashMap::new();
                    for (name, ty, binding_span) in &names {
                        let binding = alternative
                            .bindings
                            .iter()
                            .find(|binding| binding.name == *name)
                            .expect("every alternative binds the same names");
                        let selected = self.range_expr(
                            scrutinee,
                            Some((binding.offset, binding.offset + ty.width())),
                            ty,
                            binding_span.clone().unwrap_or_else(|| span.clone()),
                        );
                        let temporary = match self.materialize(selected, &span) {
                            Ok(signal) => signal,
                            Err(diagnostic) => return self.error(diagnostic),
                        };
                        locals.insert(name.clone(), temporary);
                    }
                    per_alternative.push(locals);
                }

                // One condition per alternative: its test *and* its own guard.
                // The arm is taken when any of them holds, which is what makes a
                // guard try the alternative after the one it rejected.
                let mut conditions = Vec::with_capacity(lowered.alternatives.len());
                for (index, test) in tests.iter().enumerate() {
                    let mut condition = test.clone();
                    if let Some((_, guard_expr)) = &arm.guard {
                        // While the guard is lowered, the bound names mean this
                        // alternative's own values.
                        let saved = self.proc_ref().locals.clone();
                        {
                            let context = self.proc.as_mut().expect("process context");
                            for (name, signal) in &per_alternative[index] {
                                context.locals.insert(name.clone(), *signal);
                            }
                        }
                        let result = self.lower_expr_top(guard_expr, Some(&Ty::Bool));
                        self.proc.as_mut().expect("process context").locals = saved;
                        let Some(guard_condition) = self.soft(result) else {
                            return Err(());
                        };
                        if guard_condition.ty != Ty::Bool {
                            return self.err(
                                codes::BAD_TYPE,
                                self.span(guard_expr.span()),
                                "a `match` guard must be a `bool`",
                            );
                        }
                        condition = Expr {
                            ty: Ty::Bool,
                            kind: ExprKind::Binary {
                                op: crate::frontend::mir::BinOp::LogAnd,
                                lhs: Box::new(condition),
                                rhs: Box::new(guard_condition),
                            },
                            source: span.clone(),
                        };
                    }
                    conditions.push(match self.materialize(condition, &span) {
                        Ok(signal) => signal,
                        Err(diagnostic) => return self.error(diagnostic),
                    });
                }

                // The copies and the conditions are statements of this arm, and
                // the bindings below read them, so they are emitted first.
                let mut prelude = Vec::new();
                self.drain_pending(&mut prelude);

                // What the body reads: the value of the first alternative whose
                // own condition held.
                for (name, ty, binding_span) in &names {
                    let local = self.declare_local(name, ty.clone(), binding_span.clone())?;
                    let mut value = self.range_expr(
                        per_alternative[per_alternative.len() - 1][name],
                        None,
                        ty,
                        span.clone(),
                    );
                    for index in (0..conditions.len() - 1).rev() {
                        value = Expr {
                            ty: ty.clone(),
                            kind: ExprKind::Ternary {
                                cond: Box::new(self.range_expr(
                                    conditions[index],
                                    None,
                                    &Ty::Bool,
                                    span.clone(),
                                )),
                                then_expr: Box::new(self.range_expr(
                                    per_alternative[index][name],
                                    None,
                                    ty,
                                    span.clone(),
                                )),
                                else_expr: Box::new(value),
                            },
                            source: span.clone(),
                        };
                    }
                    let target = LValue::Signal {
                        signal: local,
                        range: None,
                        source: span.clone(),
                    };
                    self.record_process_assign(&target, &value);
                    prelude.push(Stmt::Assign {
                        target,
                        value,
                        nonblocking: false,
                        source: span.clone(),
                    });
                }

                let condition = Self::fold_conditions(
                    crate::frontend::mir::BinOp::LogOr,
                    conditions
                        .iter()
                        .map(|signal| self.range_expr(*signal, None, &Ty::Bool, span.clone()))
                        .collect(),
                    &span,
                );
                (prelude, condition)
            } else {
                // Every alternative that can match gives a name the same value,
                // so one shared copy per name is enough - and it has to happen
                // before the guard that reads it. A failed pattern test only means
                // the arm is not taken, so copying the bits first is safe even
                // when the tag does not match.
                let mut prelude = Vec::new();
                for (name, ty, binding_span) in &names {
                    // Every declaration gets its own signal under a name no other
                    // declaration of this process uses, so a binding shadows an
                    // outer signal instead of overwriting it.
                    let local = self.declare_local(name, ty.clone(), binding_span.clone())?;
                    let mut values = Vec::with_capacity(lowered.alternatives.len());
                    for (index, alternative) in lowered.alternatives.iter().enumerate() {
                        let binding = alternative
                            .bindings
                            .iter()
                            .find(|binding| binding.name == *name)
                            .expect("every alternative binds the same names");
                        let selected = self.range_expr(
                            scrutinee,
                            Some((binding.offset, binding.offset + ty.width())),
                            ty,
                            binding_span.clone().unwrap_or_else(|| span.clone()),
                        );
                        values.push((tests[index].clone(), selected));
                    }
                    // The alternatives are mutually exclusive and in source
                    // order, so the first one whose test holds matched it. When
                    // they bind at one offset the value is the same either way and
                    // no selection is needed.
                    let mut value = values.pop().expect("one alternative at least").1;
                    if values.iter().any(|(_, selected)| *selected != value) {
                        for (test, selected) in values.into_iter().rev() {
                            value = Expr {
                                ty: ty.clone(),
                                kind: ExprKind::Ternary {
                                    cond: Box::new(test),
                                    then_expr: Box::new(selected),
                                    else_expr: Box::new(value),
                                },
                                source: span.clone(),
                            };
                        }
                    }
                    let target = LValue::Signal {
                        signal: local,
                        range: None,
                        source: span.clone(),
                    };
                    self.record_process_assign(&target, &value);
                    prelude.push(Stmt::Assign {
                        target,
                        value,
                        nonblocking: false,
                        source: span.clone(),
                    });
                }

                // The guard is lowered here, with the bindings in scope and
                // already holding their value, and before the body: a body that
                // writes an output cannot change the guard that selected its own
                // arm.
                let guard_condition = match &arm.guard {
                    Some((_, guard_expr)) => {
                        let result = self.lower_expr_top(guard_expr, Some(&Ty::Bool));
                        let Some(guard_condition) = self.soft(result) else {
                            return Err(());
                        };
                        if guard_condition.ty != Ty::Bool {
                            return self.err(
                                codes::BAD_TYPE,
                                self.span(guard_expr.span()),
                                "a `match` guard must be a `bool`",
                            );
                        }
                        Some(guard_condition)
                    }
                    None => None,
                };
                // A temporary the guard needed is a statement of this arm and
                // belongs before the condition that reads it.
                self.drain_pending(&mut prelude);

                // The arm's condition is `pattern && guard`. A pattern that
                // matches everything has no test of its own, so an unguarded one
                // selects the arm unconditionally.
                let test = if always_true {
                    None
                } else {
                    Self::fold_conditions(crate::frontend::mir::BinOp::LogOr, tests, &span)
                };
                let condition = match (test, guard_condition) {
                    (Some(test), Some(guard_condition)) => Some(Expr {
                        ty: Ty::Bool,
                        kind: ExprKind::Binary {
                            op: crate::frontend::mir::BinOp::LogAnd,
                            lhs: Box::new(test),
                            rhs: Box::new(guard_condition),
                        },
                        source: span.clone(),
                    }),
                    (Some(test), None) => Some(test),
                    (None, guard_condition) => guard_condition,
                };
                (prelude, condition)
            };

            // Every assignment in the arm is guarded by the arm's condition: the
            // tests read the matched value's bits and the guard reads the
            // bindings, whose prelude already points at the matched value.
            if let Some(condition) = &condition {
                let mut extended = branch_guard.clone();
                extended.extend(self.sources_of(condition));
                self.proc.as_mut().expect("process context").guard_reads = extended;
            }

            // The body. In statement form a `let` in it is scoped to the arm,
            // exactly as in a branch of an `if`. In value form the arm's trailing
            // expression is the value, and the statements before it still run -
            // only for the arm that was selected.
            let mut body = Vec::new();
            if let Some(state) = form.as_mut() {
                let state: &mut MatchValue = state;
                self.lower_match_arm_value(arm, state, &mut body, &span)?;
            } else {
                match &*arm.body {
                    syn::Expr::Block(block) => self.lower_nested(&block.block.stmts, &mut body)?,
                    other => {
                        let saved = self.proc_ref().at_run_top;
                        self.proc.as_mut().expect("process context").at_run_top = false;
                        let owned = syn::Stmt::Expr(other.clone(), None);
                        let result = self.lower_stmt(&owned, &mut body);
                        self.proc.as_mut().expect("process context").at_run_top = saved;
                        result?;
                    }
                }
            }
            self.drain_pending(&mut body);

            let arm_state = mem::replace(
                &mut self.proc.as_mut().expect("process context").definite,
                before.clone(),
            );
            let arm_provenance = self.proc_ref().provenance.clone();
            // A binding shadows an outer name only inside its own arm, and a
            // `let` declared in the body is scoped to it as well.
            self.proc.as_mut().expect("process context").locals = bindings;
            branch_states.push(arm_state);
            branch_provenances.push(arm_provenance);

            // Coverage only sees unguarded patterns (REQ-LANG-044).
            if arm.guard.is_none() {
                if let Err(error) = coverage.add(&lowered.alternatives) {
                    return self.err(
                        self.pat_code(&error.kind),
                        self.span(error.span),
                        error.message(),
                    );
                }
            }

            match condition {
                Some(condition) => {
                    // A guarded arm cannot be one arm of a `case`: the guard has to
                    // be able to reject the arm and let a later one match.
                    if arm.guard.is_some() {
                        case_arms = None;
                    }
                    if case_arms.is_some() {
                        let mut values = Vec::with_capacity(patterns.len());
                        let mut plain = true;
                        for pattern in &patterns {
                            match pattern {
                                Some(pattern) => values.push(*pattern),
                                None => {
                                    // A range pattern, or one too wide for the
                                    // constants a `case` value can hold.
                                    plain = false;
                                    break;
                                }
                            }
                        }
                        if plain {
                            if let Some(arms) = case_arms.as_mut() {
                                arms.push(values);
                            }
                        } else {
                            case_arms = None;
                        }
                    }
                    chain.push((condition, prelude, body));
                }
                None => {
                    let mut block = prelude;
                    block.extend(body);
                    default_block = Some(block);
                }
            }
        }

        self.proc.as_mut().expect("process context").guard_reads = guard;

        // Every value has to reach an arm; a `_` arm (or an irrefutable binding)
        // is not required when the patterns already cover the type. Otherwise the
        // remaining values fall through and the process would have to remember
        // its old value (REQ-LANG-022).
        if default_block.is_none() {
            let exhaustive = match coverage.is_exhaustive() {
                Ok(exhaustive) => exhaustive,
                Err(error) => {
                    return self.err(
                        self.pat_code(&error.kind),
                        self.span(error.span),
                        error.message(),
                    )
                }
            };
            if !exhaustive {
                let missing = match coverage.missing(3) {
                    Ok(missing) => missing,
                    Err(error) => {
                        return self.err(
                            self.pat_code(&error.kind),
                            self.span(error.span),
                            error.message(),
                        )
                    }
                };
                let detail = if missing.is_empty() {
                    "some value is not covered".to_string()
                } else {
                    format!(
                        "not covered: {}",
                        missing
                            .iter()
                            .map(describe_missing)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                return self.err(
                    codes::UNSUPPORTED,
                    Some(span),
                    format!("`match` is not exhaustive: {detail}"),
                );
            }
        }

        // The state after the `match` is the merge of the paths that can reach
        // it, one per arm and nothing else: an exhaustive `match` without a `_`
        // has no fall-through path, so no "nothing matched" path is merged in,
        // and a `_` arm is a path of its own.
        let merged = merge_all_definite(&branch_states, |signal| self.cur.signal(signal).width());
        let merged_provenance = Provenance::merge(&branch_provenances);
        let context = self.proc.as_mut().expect("process context");
        context.definite = merged;
        context.provenance = merged_provenance;

        // Source order is the priority, so the arms nest: the first arm's
        // condition is the outermost `if`, and every later arm is the `else` of
        // the one before it. An arm's prelude sits in the branch that reaches it.
        //
        // A `match` whose reachable arms are all plain bit patterns is one
        // `case`/`casez` instead: the arms compare their pattern against the
        // whole matched value, and the first match wins exactly as the first
        // condition that holds does, so the priority is the same. Only the shape
        // that a `case` cannot express - a guard, a range, a value wider than
        // 128 bits - falls back to the chain.
        if let Some(case_arms) = case_arms {
            if !case_arms.is_empty() {
                let covered = if scrutinee_width >= 128 {
                    u128::MAX
                } else {
                    (1u128 << scrutinee_width) - 1
                };
                let all_cared = case_arms
                    .iter()
                    .flatten()
                    .all(|(_, care)| care & covered == covered);
                let kind = match all_cared {
                    true => crate::frontend::mir::CaseKind::Case,
                    false => crate::frontend::mir::CaseKind::Casez,
                };
                let mut arms = Vec::with_capacity(case_arms.len());
                for (values, (_, prelude, body)) in case_arms.into_iter().zip(chain) {
                    let mut statements = prelude;
                    statements.extend(body);
                    arms.push(crate::frontend::mir::CaseArm {
                        values: values
                            .into_iter()
                            .map(|(value, care)| Expr {
                                ty: Ty::UInt(scrutinee_width),
                                kind: ExprKind::Pattern {
                                    value,
                                    care,
                                    width: scrutinee_width,
                                },
                                source: span.clone(),
                            })
                            .collect(),
                        body: statements,
                        source: span.clone(),
                    });
                }
                out.push(Stmt::Case {
                    kind,
                    scrutinee: scrutinee_read,
                    arms,
                    default: default_block,
                    source: span,
                });
                return Ok(());
            }
        }

        let mut else_body = default_block.unwrap_or_default();
        for (condition, prelude, body) in chain.into_iter().rev() {
            let mut statements = prelude;
            statements.push(Stmt::If {
                cond: condition,
                then_body: body,
                else_body,
                source: span.clone(),
            });
            else_body = statements;
        }
        out.extend(else_body);
        Ok(())
    }

    /// The value one arm of an expression `match` produces.
    ///
    /// A block arm runs its statements first and its trailing expression is the
    /// value; any other arm body is the value itself. Every arm writes the same
    /// place: the result temporary of a `match` used as a value, or - in an
    /// assignment or a `let` - the target itself, so the statements an arm
    /// contains run only when that arm is selected and no copy is needed.
    fn lower_match_arm_value(
        &mut self,
        arm: &syn::Arm,
        state: &mut MatchValue,
        body: &mut Vec<Stmt>,
        span: &SourceSpan,
    ) -> Result<(), ()> {
        let tail = match &*arm.body {
            syn::Expr::Block(block) => {
                let Some((syn::Stmt::Expr(tail, None), rest)) = block.block.stmts.split_last()
                else {
                    return self.err(
                        codes::UNSUPPORTED,
                        self.span(block.span()),
                        "this `match` arm produces no value: end its block with an expression",
                    );
                };
                self.lower_nested(rest, body)?;
                tail
            }
            other => other,
        };

        // A `match` in the tail is the arm's value. In an assignment context the
        // arm writes the target directly, so the nested `match` does too: one
        // target, no result temporary in the middle.
        if let Some(into) = state.into.clone() {
            if let syn::Expr::Match(item) = tail {
                let mut inner = MatchValue {
                    expected: state.ty.clone(),
                    result: None,
                    ty: state.ty.clone(),
                    into: Some(into),
                };
                return self.lower_match_core(item, tail, body, Some(&mut inner));
            }
        }

        // The arms of one `match` all have one type: the context the call site
        // gave, or the type the first arm that names one produces. Once it is
        // known, every later arm is lowered against it, which is what gives a
        // literal arm its width.
        let expected = state.ty.clone().or_else(|| state.expected.clone());
        let result = self.lower_expr_top(tail, expected.as_ref());
        let Some(value_expr) = self.soft(result) else {
            return Err(());
        };

        // An assignment context names the target: the arm assigns it inside its
        // own branch, and the width check is the one every assignment gets.
        if let Some(into) = state.into.clone() {
            let ty = state
                .ty
                .clone()
                .expect("an into match has its target's type");
            let target_span = self.span(arm.body.span()).unwrap_or_else(|| span.clone());
            self.check_assign_width(&ty, into.target.range(), &value_expr, target_span.clone())?;
            self.drain_pending(body);
            self.record_process_assign(&into.target, &value_expr);
            body.push(Stmt::Assign {
                target: into.target,
                value: value_expr,
                nonblocking: into.nonblocking,
                source: target_span,
            });
            return Ok(());
        }

        let ty = match &state.ty {
            Some(have) => {
                if value_expr.ty != *have {
                    return self.err(
                        codes::WIDTH_MISMATCH,
                        self.span(tail.span()),
                        format!(
                            "the arms of this `match` have different types: {} and {}",
                            have.describe(),
                            value_expr.ty.describe()
                        ),
                    );
                }
                have.clone()
            }
            None => value_expr.ty.clone(),
        };
        // A hardware width is always explicit (REQ-LANG-030): an all-literal
        // `match` with no context has no width to infer from.
        if ty.width() == 0 {
            return self.err(
                codes::WIDTH_UNKNOWN,
                self.span(tail.span()),
                "the type of this `match` cannot be inferred; annotate the value, for example \
                 `let x: uint<8> = match ...`, or write a suffix such as `1u8`",
            );
        }

        let signal = match state.result {
            Some(signal) => signal,
            None => {
                let signal = match self.declare_process_temp(ty.clone(), span) {
                    Ok(signal) => signal,
                    Err(diagnostic) => return self.error(diagnostic),
                };
                state.result = Some(signal);
                state.ty = Some(ty);
                signal
            }
        };
        let target_span = self.span(arm.body.span()).unwrap_or_else(|| span.clone());
        let target = LValue::Signal {
            signal,
            range: None,
            source: target_span.clone(),
        };
        // A `match` nested in the tail already queued its chain in `pending`, so
        // the assignment has to be emitted after that chain, not before it.
        self.drain_pending(body);
        self.record_process_assign(&target, &value_expr);
        body.push(Stmt::Assign {
            target,
            value: value_expr,
            nonblocking: false,
            source: target_span,
        });
        Ok(())
    }

    /// Render one [`pattern::Test`] as a condition on the matched value's bits.
    ///
    /// `base` is where the tested value starts inside `signal`: always zero for
    /// the copy a `match` makes, while the recursive call for a composite test
    /// carries the offset of the part it renders.
    fn render_test(
        &self,
        test: &crate::pattern::Test,
        signal: SignalId,
        base: u32,
        span: &SourceSpan,
    ) -> Expr {
        use crate::pattern::Test;
        match test {
            Test::AlwaysTrue => Expr {
                ty: Ty::Bool,
                kind: ExprKind::Const { value: 1, width: 1 },
                source: span.clone(),
            },
            Test::Bits {
                offset,
                width,
                value,
            } => {
                let field_ty = if *width == 1 {
                    Ty::Bool
                } else {
                    Ty::UInt(*width)
                };
                let selected = self.range_expr(
                    signal,
                    Some((base + *offset, base + *offset + *width)),
                    &field_ty,
                    span.clone(),
                );
                Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Binary {
                        op: crate::frontend::mir::BinOp::Eq,
                        lhs: Box::new(selected),
                        rhs: Box::new(Expr {
                            ty: field_ty,
                            kind: ExprKind::Const {
                                value: *value as i128,
                                width: *width,
                            },
                            source: span.clone(),
                        }),
                    },
                    source: span.clone(),
                }
            }
            Test::Range {
                offset,
                width,
                lo,
                hi,
            } => {
                let field_ty = if *width == 1 {
                    Ty::Bool
                } else {
                    Ty::UInt(*width)
                };
                let selected = self.range_expr(
                    signal,
                    Some((base + *offset, base + *offset + *width)),
                    &field_ty,
                    span.clone(),
                );
                let constant = |value: u128| Expr {
                    ty: field_ty.clone(),
                    kind: ExprKind::Const {
                        value: value as i128,
                        width: *width,
                    },
                    source: span.clone(),
                };
                let lower = Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Binary {
                        op: crate::frontend::mir::BinOp::Ge,
                        lhs: Box::new(selected.clone()),
                        rhs: Box::new(constant(*lo)),
                    },
                    source: span.clone(),
                };
                let upper = Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Binary {
                        op: crate::frontend::mir::BinOp::Le,
                        lhs: Box::new(selected),
                        rhs: Box::new(constant(*hi)),
                    },
                    source: span.clone(),
                };
                Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Binary {
                        op: crate::frontend::mir::BinOp::LogAnd,
                        lhs: Box::new(lower),
                        rhs: Box::new(upper),
                    },
                    source: span.clone(),
                }
            }
            Test::Mask {
                offset,
                width,
                care,
                value,
            } => {
                let mut condition: Option<Expr> = None;
                for (lo, hi) in crate::pattern::care_segments(*care, *width) {
                    let segment_width = hi - lo;
                    let selected = self.range_expr(
                        signal,
                        Some((base + *offset + lo, base + *offset + hi)),
                        &Ty::UInt(segment_width),
                        span.clone(),
                    );
                    let segment_value =
                        ((value >> lo) & crate::pattern::bit_mask(segment_width)) as i128;
                    let equal = Expr {
                        ty: Ty::Bool,
                        kind: ExprKind::Binary {
                            op: crate::frontend::mir::BinOp::Eq,
                            lhs: Box::new(selected),
                            rhs: Box::new(Expr {
                                ty: Ty::UInt(segment_width),
                                kind: ExprKind::Const {
                                    value: segment_value,
                                    width: segment_width,
                                },
                                source: span.clone(),
                            }),
                        },
                        source: span.clone(),
                    };
                    condition = Some(match condition {
                        None => equal,
                        Some(previous) => Expr {
                            ty: Ty::Bool,
                            kind: ExprKind::Binary {
                                op: crate::frontend::mir::BinOp::LogAnd,
                                lhs: Box::new(previous),
                                rhs: Box::new(equal),
                            },
                            source: span.clone(),
                        },
                    });
                }
                condition.unwrap_or_else(|| Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Const { value: 1, width: 1 },
                    source: span.clone(),
                })
            }
            Test::All(tests) => Self::fold_conditions(
                crate::frontend::mir::BinOp::LogAnd,
                tests
                    .iter()
                    .map(|test| self.render_test(test, signal, base, span))
                    .collect(),
                span,
            )
            .unwrap_or_else(|| Expr {
                ty: Ty::Bool,
                kind: ExprKind::Const { value: 1, width: 1 },
                source: span.clone(),
            }),
        }
    }

    /// Fold `conditions` with one operator, left to right: `a op b op c`. `None`
    /// for an empty list, a single condition unchanged.
    fn fold_conditions(
        op: crate::frontend::mir::BinOp,
        conditions: Vec<Expr>,
        span: &SourceSpan,
    ) -> Option<Expr> {
        let mut conditions = conditions.into_iter();
        let mut merged = conditions.next()?;
        for other in conditions {
            merged = Expr {
                ty: Ty::Bool,
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(merged),
                    rhs: Box::new(other),
                },
                source: span.clone(),
            };
        }
        Some(merged)
    }

    /// Map a pattern error to the diagnostic code the contract names.
    fn pat_code(&self, kind: &crate::pattern::PatErrorKind) -> &'static str {
        use crate::pattern::PatErrorKind;
        match kind {
            PatErrorKind::UnknownName(_) => codes::UNKNOWN_NAME,
            PatErrorKind::NotAConstructor(_) => codes::BAD_TYPE,
            PatErrorKind::LiteralOutOfRange { .. } => codes::OUT_OF_RANGE,
            PatErrorKind::ArityMismatch { .. } => codes::WIDTH_MISMATCH,
            PatErrorKind::PayloadArity { .. } => codes::WIDTH_MISMATCH,
            PatErrorKind::BindingMismatch { .. } => codes::DECLARATION,
            PatErrorKind::UnsupportedPattern(_) => codes::UNSUPPORTED,
            PatErrorKind::TooManyAlternatives { .. } => codes::UNSUPPORTED,
            PatErrorKind::TooDeep { .. } => codes::UNSUPPORTED,
            PatErrorKind::CoverageLimit(_) => codes::UNSUPPORTED,
            PatErrorKind::Layout(_) => codes::LAYOUT,
        }
    }

    pub(super) fn lower_process_for(
        &mut self,
        item: &syn::ExprForLoop,
        expression: &syn::Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ()> {
        let span = self
            .span(expression.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        let (name, _) = match pattern_name(&item.pat) {
            Ok(parts) => parts,
            Err(message) => return self.err(codes::UNSUPPORTED, Some(span), message),
        };
        let syn::Expr::Range(range) = &*item.expr else {
            return self.err(
                codes::NOT_CONSTANT,
                Some(span),
                "a `for` inside a process must iterate over a build-time range such as `0..N`",
            );
        };
        let start_value = match &range.start {
            Some(start) => match consteval::eval(start, &self.cur.env) {
                Ok(value) => value,
                Err(message) => {
                    return self.err(codes::NOT_CONSTANT, self.span(start.span()), message)
                }
            },
            None => 0,
        };
        let end_value = match &range.end {
            Some(end) => match consteval::eval(end, &self.cur.env) {
                Ok(value) => value,
                Err(message) => {
                    return self.err(codes::NOT_CONSTANT, self.span(end.span()), message)
                }
            },
            None => {
                return self.err(
                    codes::NOT_CONSTANT,
                    Some(span),
                    "a `for` inside a process needs an explicit build-time end such as `0..N`",
                )
            }
        };
        let end_value = match range.limits {
            syn::RangeLimits::Closed(_) => match end_value.checked_add(1) {
                Some(value) => value,
                None => {
                    return self.err(
                        codes::NOT_CONSTANT,
                        Some(span),
                        "the end of this inclusive range overflows a 64-bit constant",
                    )
                }
            },
            syn::RangeLimits::HalfOpen(_) => end_value,
        };

        // An empty range is not an error: it simply produces no statements. Any
        // signal that then has no driver is reported by the driver checks.
        if start_value >= end_value {
            return Ok(());
        }

        // The budget is on the whole loop *nesting*, not on one loop: 100 x 700
        // iterations is 70 000 unrolled bodies and must be refused as a whole.
        let iterations = end_value - start_value;
        let enclosing = self.proc_ref().unroll_product;
        let product = enclosing.saturating_mul(iterations);
        if product > MAX_UNROLL {
            return self.err(
                codes::NOT_CONSTANT,
                Some(span),
                format!(
                    "these static loops expand to {product} statements, more than the supported {MAX_UNROLL}"
                ),
            );
        }

        let saved_env = self.cur.env.clone();
        let saved_product = self.proc.as_mut().expect("process context").unroll_product;
        self.proc.as_mut().expect("process context").unroll_product = product;
        let mut result = Ok(());
        let bindings = self.proc_ref().locals.clone();
        for value in start_value..end_value {
            self.cur.env.insert(name.clone(), value);
            result = self.lower_nested(&item.body.stmts, out);
            // A `let` inside the body is scoped to that iteration.
            self.proc.as_mut().expect("process context").locals = bindings.clone();
            if result.is_err() {
                break;
            }
        }
        self.cur.env = saved_env;
        self.proc.as_mut().expect("process context").unroll_product = saved_product;
        result
    }

    // ---------------------------------------------------------- expressions

    pub(super) fn drain_pending(&mut self, out: &mut Vec<Stmt>) {
        if let Some(context) = self.proc.as_mut() {
            let pending = mem::take(&mut context.pending);
            out.extend(pending);
        }
    }

    /// Declare (or reuse) a process-local signal.
    pub(super) fn declare_local(
        &mut self,
        name: &str,
        ty: Ty,
        span: Option<SourceSpan>,
    ) -> Result<SignalId, ()> {
        if let Err(diagnostic) = check_verilog_name(name, "local signal", span.clone()) {
            self.diagnostics.push(diagnostic);
            return Err(());
        }
        // Every declaration is its own value: two blocks may declare `t` with
        // different types, and a nested `let` shadows instead of aliasing. The
        // bindings follow Rust's block scope (see `ProcCtx::locals`), while the
        // generated block needs one unique name per declaration.
        // The generated name must not collide with any signal of the module
        // either: a local that shadows a module signal would make every read of
        // that signal inside the block pick up the wrong value.
        let mut wire_name = name.to_string();
        let mut suffix = 1;
        while self.proc_ref().local_names.contains(&wire_name)
            || self.cur.signal_by_name.contains_key(&wire_name)
        {
            wire_name = format!("{name}__{suffix}");
            suffix += 1;
        }

        let id = self.cur.signals.len();
        let process = self.current_process();
        self.cur.signals.push(Signal {
            name: wire_name.clone(),
            source_name: Some(name.to_string()),
            ty,
            kind: SignalKind::Reg,
            scope: SignalScope::Process(process),
            source: span,
        });
        let context = self.proc.as_mut().expect("inside a process");
        context.local_names.insert(wire_name);
        context.locals.insert(name.to_string(), id);
        context.local_list.push(id);
        Ok(id)
    }

    /// Turn a temporary the initialiser created into the `let` local it is bound
    /// to.
    ///
    /// `let x = match ...;` lowers the initialiser to a value, and a `match` (or
    /// an `if`) produces one temporary holding the selected arm. Copying that
    /// temporary into a second signal only renames it, so the binding takes it
    /// over instead: the statements, the bits and the reads stay as they are.
    ///
    /// The name follows [`Compiler::declare_local`]: the source name when it is
    /// free, `x__1`, `x__2` otherwise. The old temporary name is not kept, so no
    /// read can be left pointing at a name that no longer exists.
    fn adopt_local(
        &mut self,
        name: &str,
        id: SignalId,
        span: Option<SourceSpan>,
    ) -> Result<(), ()> {
        if let Err(diagnostic) = check_verilog_name(name, "local signal", span) {
            self.diagnostics.push(diagnostic);
            return Err(());
        }
        let mut wire_name = name.to_string();
        let mut suffix = 1;
        while self.proc_ref().local_names.contains(&wire_name)
            || self.cur.signal_by_name.contains_key(&wire_name)
        {
            wire_name = format!("{name}__{suffix}");
            suffix += 1;
        }
        let signal = &mut self.cur.signals[id];
        signal.name = wire_name.clone();
        signal.source_name = Some(name.to_string());
        self.cur.signal_by_name.insert(wire_name.clone(), id);
        let context = self.proc.as_mut().expect("inside a process");
        context.local_names.insert(wire_name);
        context.locals.insert(name.to_string(), id);
        Ok(())
    }

    // ------------------------------------------------------------- types
}

/// One run-time index of a dynamic element write, lowered once for all arms.
struct DynamicIndex {
    /// The index expression itself.
    index: Expr,
    /// How many elements it can select: its own reach, never more than the array
    /// holds.
    count: u32,
}

/// What every arm of one run-time element write needs.
struct DynamicWriteExpansion<'a> {
    /// The write being expanded.
    write: &'a DynamicWrite,
    /// The lowered run-time indices, outermost first.
    indices: &'a [DynamicIndex],
    /// The value every arm writes.
    value: &'a Expr,
    /// Source position of the assignment.
    span: &'a SourceSpan,
    /// The element each index takes in the arm being built.
    chosen: Vec<u32>,
    /// Sources of the value and of the guards and index reads above it, as the
    /// state before the write resolves them. Every arm writes the same value and
    /// reads the same index, so this is resolved once instead of per leaf.
    sources: Vec<ReadRef>,
    /// One `(lo, hi, sources)` per written leaf, collected while the arms are
    /// built in ascending order. The ranges are disjoint, so one splice of this
    /// list is the union over them.
    leaves: Vec<(u32, u32, Vec<ReadRef>)>,
}

/// Human-readable name of a value the coverage proof could not find.
fn describe_missing(missing: &crate::pattern::Missing) -> String {
    use crate::pattern::Missing;
    match missing {
        Missing::Bool(value) => value.to_string(),
        Missing::Int(value) => value.to_string(),
        Missing::Variant { name, .. } => name.clone(),
        Missing::Invalid => "an unused tag encoding".to_string(),
    }
}

/// The constant `element` of an index's own type, for a `case` value.
fn index_constant(index: &Expr, element: u32) -> Expr {
    Expr {
        ty: index.ty.clone(),
        kind: ExprKind::Const {
            value: i128::from(element),
            width: index.ty.width(),
        },
        source: index.source.clone(),
    }
}

/// Resolves a bare pattern identifier to a build-time constant: the const
/// parameters of the module (a `for` index is not in scope as a constant here).
struct ConstEnvResolver<'a> {
    env: &'a crate::frontend::consteval::ConstEnv,
}

impl crate::pattern::ConstResolver for ConstEnvResolver<'_> {
    fn resolve(&self, ident: &syn::Ident) -> Option<u128> {
        self.env
            .get(&ident.to_string())
            .map(|value| u128::from(*value))
    }
}

/// True for a `let` pattern that binds several names out of one value.
fn is_destructuring_pattern(pat: &syn::Pat) -> bool {
    match pat {
        syn::Pat::Tuple(tuple) => !tuple.elems.is_empty(),
        syn::Pat::TupleStruct(_) | syn::Pat::Struct(_) => true,
        syn::Pat::Paren(paren) => is_destructuring_pattern(&paren.pat),
        _ => false,
    }
}
