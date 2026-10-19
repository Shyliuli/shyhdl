//! Expression lowering: widths, operators, slices and temporaries.

use super::*;
use crate::frontend::mir::{BinOp, EnumTy, EnumVariantTy, UnOp};

impl Compiler {
    pub(super) fn lower_expr_top(
        &mut self,
        expr: &syn::Expr,
        expected: Option<&Ty>,
    ) -> Result<Expr, Diagnostic> {
        let lowered = self.lower_expr(expr, expected)?;
        check_unsized(&lowered)?;
        Ok(lowered)
    }

    pub(super) fn lower_expr(
        &mut self,
        expr: &syn::Expr,
        expected: Option<&Ty>,
    ) -> Result<Expr, Diagnostic> {
        let span = self
            .span(expr.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        match expr {
            syn::Expr::Lit(literal) => self.lower_literal(&literal.lit, expected, span),
            syn::Expr::Path(path) => self.lower_path_expr(path, expected, span),
            syn::Expr::Field(field) => {
                // `cells[i].tag` is not a place: the element is selected first,
                // and the field is read from that value. A static chain
                // (`self.pkt.op`) is a place and takes the ordinary path.
                if self.has_run_time_index(expr) {
                    self.lower_value_field(field, span)
                } else {
                    self.lower_place_read(expr, span)
                }
            }
            syn::Expr::Index(index) => self.lower_index(index, span),
            syn::Expr::Array(array) => self.lower_array_literal(array, expected, span),
            syn::Expr::Tuple(tuple) => self.lower_tuple_literal(tuple, expected, span),
            syn::Expr::Repeat(repeat) => self.lower_repeat_literal(repeat, expected, span),
            syn::Expr::MethodCall(call) => self.lower_method(call, span),
            syn::Expr::Call(call) => self.lower_call_or_variant(call, expected, span),
            syn::Expr::Binary(binary) => self.lower_binary(binary, expr, expected, span),
            syn::Expr::Unary(unary) => self.lower_unary(unary, expected, span),
            syn::Expr::Paren(paren) => self.lower_expr(&paren.expr, expected),
            syn::Expr::Group(group) => self.lower_expr(&group.expr, expected),
            syn::Expr::If(item) => self.lower_if_expression(item, expected, span),
            syn::Expr::Struct(item) => self.lower_struct_literal(item, span),
            syn::Expr::Cast(cast) => self.lower_cast(cast, span),
            syn::Expr::Match(item) => self.lower_match_expression(item, expr, expected),
            other => Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                format!("unsupported expression: {}", parse::describe_expr(other)),
            )),
        }
    }

    pub(super) fn lower_literal(
        &mut self,
        literal: &syn::Lit,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        match literal {
            syn::Lit::Int(int) => {
                let value = consteval::int_value(int).map_err(|message| {
                    Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message)
                })? as i128;
                // An explicit `uN` suffix is a type of its own: it is how a bit
                // pattern is written (`0xffu8`), so a context never overrides it.
                // Every other suffix stays ignored, exactly as before.
                let suffix = suffix_type(int.suffix(), &span)?;
                match suffix.as_ref().or(expected) {
                    Some(ty @ (Ty::UInt(_) | Ty::SInt(_))) => {
                        typed_constant(value, ty.clone(), span)
                    }
                    Some(Ty::Bool) => Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!("{value} is not a `bool`: use `true` or `false`"),
                    )),
                    Some(other) => Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!("an integer literal is not a value of {}", other.describe()),
                    )),
                    None => Ok(Expr {
                        ty: Ty::UInt(0),
                        kind: ExprKind::Const { value, width: 0 },
                        source: span,
                    }),
                }
            }
            syn::Lit::Bool(value) => Ok(Expr {
                ty: Ty::Bool,
                kind: ExprKind::Const {
                    value: i128::from(value.value),
                    width: 1,
                },
                source: span,
            }),
            other => Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                format!(
                    "unsupported literal ({}): hardware values are `bool`, `uint<N>` and `int<N>`",
                    describe_literal(other)
                ),
            )),
        }
    }

    pub(super) fn lower_name(
        &mut self,
        name: &str,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if let Some(context) = &self.proc {
            if let Some(signal) = context.locals.get(name).copied() {
                self.check_local_read(signal, None, &span)?;
                self.reads.insert(signal);
                let ty = self.cur.signal(signal).ty.clone();
                if let Some(expected) = expected {
                    if expected.width() != ty.width() {
                        return Err(Diagnostic::error(
                            codes::WIDTH_MISMATCH,
                            Some(span),
                            format!(
                                "`{name}` is {} wide, but {} was expected",
                                ty.width(),
                                expected.width()
                            ),
                        ));
                    }
                }
                return Ok(Expr {
                    ty,
                    kind: ExprKind::Signal(signal),
                    source: span,
                });
            }
        }
        if let Some(value) = self.cur.env.get(name).copied() {
            return Ok(Expr {
                ty: Ty::UInt(0),
                kind: ExprKind::Const {
                    value: value as i128,
                    width: 0,
                },
                source: span,
            });
        }
        // A hardware field is the last lookup step for an unqualified name in
        // `run`: it is the DSL equivalent of Rust's implicit `self.<name>`.
        // Keep this after process locals and build-time parameters so `let x`
        // (including `let mut x`) and `N` retain ordinary lexical precedence.
        if let Some(signal) = self.cur.signal_by_name.get(name).copied() {
            self.reads.insert(signal);
            let ty = self.cur.signal(signal).ty.clone();
            if let Some(expected) = expected {
                if expected.width() != ty.width() {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span.clone()),
                        format!(
                            "`{name}` is {} wide, but {} was expected",
                            ty.width(),
                            expected.width()
                        ),
                    ));
                }
            }
            return Ok(Expr {
                ty,
                kind: ExprKind::Signal(signal),
                source: span,
            });
        }
        let mut diagnostic = Diagnostic::error(
            codes::UNKNOWN_NAME,
            Some(span),
            format!("unknown name `{name}`"),
        );
        if self.cur.signal_by_name.contains_key(name) {
            diagnostic = diagnostic.with_note(
                format!("a field of this module is written `self.{name}`"),
                None,
            );
        } else {
            diagnostic = diagnostic.with_note(
                format!("`{name}` is not a local of this process, a signal of this module, or a const parameter"),
                None,
            );
        }
        Err(diagnostic)
    }

    pub(super) fn check_local_read(
        &self,
        signal: SignalId,
        range: Option<(u32, u32)>,
        span: &SourceSpan,
    ) -> Result<(), Diagnostic> {
        let Some(context) = &self.proc else {
            return Ok(());
        };
        if !matches!(self.cur.signal(signal).scope, SignalScope::Process(_)) {
            return Ok(());
        }
        let width = self.cur.signal(signal).width();
        let (lo, hi) = range.unwrap_or((0, width));
        let mut needed = Bits::new(width);
        needed.set_range(lo, hi);
        let definite = context.definite.get(&signal);
        match definite {
            Some(bits) if bits.contains_all(&needed) => Ok(()),
            _ => {
                let name = self.cur.signal(signal).name.clone();
                Err(Diagnostic::error(
                    codes::INCOMPLETE_PATH,
                    Some(span.clone()),
                    format!("`{name}` is read before it is assigned on this path"),
                ))
            }
        }
    }

    pub(super) fn lower_place_read(
        &mut self,
        expr: &syn::Expr,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let place = self.resolve_read_place(expr)?;
        match place {
            ReadPlace::Signal { signal, range, ty } => {
                self.check_local_read(signal, range, &span)?;
                self.reads.insert(signal);
                Ok(self.range_expr(signal, range, &ty, span))
            }
        }
    }

    /// Build an expression that reads `range` out of `signal`, typed as `ty`.
    pub(super) fn range_expr(
        &self,
        signal: SignalId,
        range: Option<(u32, u32)>,
        ty: &Ty,
        span: SourceSpan,
    ) -> Expr {
        let width = self.cur.signal(signal).width();
        match range {
            None => Expr {
                ty: ty.clone(),
                kind: ExprKind::Signal(signal),
                source: span,
            },
            Some((lo, hi)) => {
                // A one-bit signal is a scalar: selecting it would be a bit
                // select on a scalar, so the name is used directly.
                if width == 1 || (lo == 0 && hi == width) {
                    Expr {
                        ty: ty.clone(),
                        kind: ExprKind::Signal(signal),
                        source: span,
                    }
                } else {
                    Expr {
                        ty: ty.clone(),
                        kind: ExprKind::Slice { signal, lo, hi },
                        source: span,
                    }
                }
            }
        }
    }

    pub(super) fn lower_binary(
        &mut self,
        binary: &syn::ExprBinary,
        expression: &syn::Expr,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let op = match binary.op {
            syn::BinOp::Add(_) => BinOp::Add,
            syn::BinOp::Sub(_) => BinOp::Sub,
            syn::BinOp::Mul(_) => BinOp::Mul,
            syn::BinOp::BitAnd(_) => BinOp::BitAnd,
            syn::BinOp::BitOr(_) => BinOp::BitOr,
            syn::BinOp::BitXor(_) => BinOp::BitXor,
            syn::BinOp::Shl(_) => BinOp::Shl,
            syn::BinOp::Shr(_) => BinOp::Shr,
            syn::BinOp::Eq(_) => BinOp::Eq,
            syn::BinOp::Ne(_) => BinOp::Ne,
            syn::BinOp::Lt(_) => BinOp::Lt,
            syn::BinOp::Le(_) => BinOp::Le,
            syn::BinOp::Gt(_) => BinOp::Gt,
            syn::BinOp::Ge(_) => BinOp::Ge,
            syn::BinOp::And(_) => BinOp::LogAnd,
            syn::BinOp::Or(_) => BinOp::LogOr,
            _ => return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "compound assignment and unsupported operators are not part of the hardware subset",
            )),
        };

        let is_shift = matches!(op, BinOp::Shl | BinOp::Shr);
        // The context type is the operand type only for an operator whose result
        // has it. A comparison's result is a `bool`, which says nothing about how
        // wide the compared values are, so it is lowered from its operands alone.
        let operand_expected = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => None,
            _ => expected,
        };
        let mut lhs = self.lower_expr(&binary.left, operand_expected)?;

        // `x + 1`: the literal on either side takes the width of the other side.
        if is_shift {
            // The shifted value is read before the count is evaluated: a count
            // that runs statements must not change it afterwards.
            lhs = self.snapshot_before(lhs, &binary.right, &span)?;
            let rhs = self.lower_expr(&binary.right, None)?;
            if !matches!(lhs.ty, Ty::UInt(_) | Ty::SInt(_)) {
                return Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!(
                        "`{}` needs an integer left operand, found {}",
                        op.verilog(),
                        lhs.ty.describe()
                    ),
                ));
            }
            check_shift_count(&rhs, &span)?;
            // The shifted value keeps its own type: a signed value shifts
            // arithmetically, an unsigned one logically.
            let ty = lhs.ty.clone();
            return Ok(Expr {
                ty,
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                source: span,
            });
        }

        let rhs_expected = if lhs.ty.width() == 0 {
            None
        } else {
            Some(lhs.ty.clone())
        };
        // Rust evaluates the operands left to right: a statement the right
        // operand runs must not change what the left one already read.
        lhs = self.snapshot_before(lhs, &binary.right, &span)?;
        // `&&` and `||` short-circuit, so a right operand that runs statements
        // must run only when the left one does not decide the result.
        if matches!(op, BinOp::LogAnd | BinOp::LogOr) && may_emit_statements(&binary.right) {
            if self.proc.is_none() {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    "an operand that runs statements needs a process: write it in `run` or inside `always!`",
                ));
            }
            return self.lower_lazy_logical(op, lhs, &binary.right, span);
        }
        let mut rhs = self.lower_expr(&binary.right, rhs_expected.as_ref())?;

        if lhs.ty.width() == 0 && rhs.ty.width() > 0 {
            lhs = self.lower_expr(&binary.left, Some(&rhs.ty))?;
        } else if rhs.ty.width() == 0 && lhs.ty.width() > 0 {
            rhs = self.lower_expr(&binary.right, Some(&lhs.ty))?;
        }
        if lhs.ty.width() == 0 || rhs.ty.width() == 0 {
            return Err(Diagnostic::error(
                codes::WIDTH_UNKNOWN,
                Some(span),
                "the width of this integer literal cannot be inferred; annotate the value's type (REQ-LANG-030)",
            ));
        }

        // An enum is compared by type, not by its packed bits (REQ-LANG-042):
        // the tag, and the payload of the variant the tag selects. Every shape
        // involved has to match exactly, so an enum is never another value with
        // the same bits.
        if matches!(op, BinOp::Eq | BinOp::Ne)
            && (mentions_shape(&lhs.ty) || mentions_shape(&rhs.ty))
        {
            require_same_width(&lhs.ty, &rhs.ty, op, &span)?;
            require_same_signedness(&lhs.ty, &rhs.ty, op, &span)?;
            if !same_shape(&lhs.ty, &rhs.ty) {
                return Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!(
                        "cannot compare {} with {}: an enum or an array is not another value with the same bits",
                        lhs.ty.describe(),
                        rhs.ty.describe()
                    ),
                ));
            }
            if mentions_enum(&lhs.ty) {
                let equal = self.typed_equality(&lhs, &rhs, &lhs.ty.clone(), &span)?;
                let kind = if op == BinOp::Ne {
                    ExprKind::Unary {
                        op: UnOp::Not,
                        operand: Box::new(equal),
                    }
                } else {
                    equal.kind
                };
                return Ok(Expr {
                    ty: Ty::Bool,
                    kind,
                    source: span,
                });
            }
        }

        let _ = expression;
        let result_ty = match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                if !matches!(lhs.ty, Ty::UInt(_) | Ty::SInt(_))
                    || !matches!(rhs.ty, Ty::UInt(_) | Ty::SInt(_))
                {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!(
                            "`{}` needs two integer operands of the same width, found {} and {}",
                            op.verilog(),
                            lhs.ty.describe(),
                            rhs.ty.describe()
                        ),
                    ));
                }
                require_same_width(&lhs.ty, &rhs.ty, op, &span)?;
                require_same_signedness(&lhs.ty, &rhs.ty, op, &span)?;
                lhs.ty.clone()
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                if matches!(lhs.ty, Ty::Struct(_)) || matches!(rhs.ty, Ty::Struct(_)) {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        "bitwise operators are not supported on struct values",
                    ));
                }
                require_same_width(&lhs.ty, &rhs.ty, op, &span)?;
                require_same_signedness(&lhs.ty, &rhs.ty, op, &span)?;
                match (&lhs.ty, &rhs.ty) {
                    (Ty::Bool, Ty::Bool) => Ty::Bool,
                    (Ty::UInt(width), _) => Ty::UInt(*width),
                    // A bitwise operation changes no bit's meaning, so a signed
                    // operand keeps its signed type.
                    (Ty::SInt(width), _) => Ty::SInt(*width),
                    _ => lhs.ty.clone(),
                }
            }
            BinOp::Eq | BinOp::Ne => {
                require_same_width(&lhs.ty, &rhs.ty, op, &span)?;
                require_same_signedness(&lhs.ty, &rhs.ty, op, &span)?;
                Ty::Bool
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if matches!(lhs.ty, Ty::Struct(_))
                    || matches!(rhs.ty, Ty::Struct(_))
                    || matches!(lhs.ty, Ty::Enum(_))
                    || matches!(rhs.ty, Ty::Enum(_))
                {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!(
                            "ordering comparisons are not supported on {} and {}",
                            lhs.ty.describe(),
                            rhs.ty.describe()
                        ),
                    ));
                }
                require_same_width(&lhs.ty, &rhs.ty, op, &span)?;
                require_same_signedness(&lhs.ty, &rhs.ty, op, &span)?;
                Ty::Bool
            }
            BinOp::LogAnd | BinOp::LogOr => {
                if lhs.ty != Ty::Bool || rhs.ty != Ty::Bool {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!("`{}` needs two `bool` operands", op.verilog()),
                    ));
                }
                Ty::Bool
            }
            BinOp::Shl | BinOp::Shr => unreachable!("handled above"),
        };

        Ok(Expr {
            ty: result_ty,
            kind: ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            source: span,
        })
    }

    pub(super) fn lower_unary(
        &mut self,
        unary: &syn::ExprUnary,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        match unary.op {
            // Rust has one `!`: logical on a `bool`, bitwise on an integer. The
            // operand decides which one is emitted, and the two are never
            // confused (a `!` on a `uint` becomes Verilog `~`).
            syn::UnOp::Not(_) => {
                let operand = self.lower_expr(&unary.expr, None)?;
                let ty = match &operand.ty {
                    Ty::Bool => Ty::Bool,
                    Ty::UInt(width) => Ty::UInt(*width),
                    // Inverting the bits of a two's-complement value keeps it
                    // signed: it is the same width, read the same way.
                    Ty::SInt(width) => Ty::SInt(*width),
                    Ty::Struct(_) => {
                        return Err(Diagnostic::error(
                            codes::WIDTH_MISMATCH,
                            Some(span),
                            "`!` is not supported on struct values",
                        ))
                    }
                    Ty::Array { .. } | Ty::Enum(_) => {
                        return Err(Diagnostic::error(
                            codes::WIDTH_MISMATCH,
                            Some(span),
                            format!("`!` is not supported on {}", operand.ty.describe()),
                        ))
                    }
                };
                let op = if ty == Ty::Bool {
                    UnOp::Not
                } else {
                    UnOp::BitNot
                };
                Ok(Expr {
                    ty,
                    kind: ExprKind::Unary {
                        op,
                        operand: Box::new(operand),
                    },
                    source: span,
                })
            }
            syn::UnOp::Neg(_) => self.lower_negation(&unary.expr, expected, span),
            _ => Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "unsupported unary operator",
            )),
        }
    }

    /// Unary `-`: a signed value negated at its own width, wrapping.
    ///
    /// A negated literal is a constant and is folded here; negating anything
    /// else is a real operation. There is no unsigned negation: reading an
    /// unsigned value as negative is a reinterpretation the source did not
    /// write.
    fn lower_negation(
        &mut self,
        operand: &syn::Expr,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if let Some(literal) = integer_literal(operand) {
            let magnitude = consteval::int_value(literal).map_err(|message| {
                Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message)
            })? as i128;
            let value = -magnitude;
            let suffix = suffix_type(literal.suffix(), &span)?;
            return match suffix.as_ref().or(expected) {
                Some(ty @ (Ty::UInt(_) | Ty::SInt(_))) => typed_constant(value, ty.clone(), span),
                Some(Ty::Bool) => Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!("`{value}` is not a `bool`: use `true` or `false`"),
                )),
                Some(other) => Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!(
                        "a negative integer literal is not a value of {}",
                        other.describe()
                    ),
                )),
                // The width of an untyped negative literal is unknowable, so it
                // is reported by `check_unsized` like any other unsized literal.
                None => Ok(Expr {
                    ty: Ty::SInt(0),
                    kind: ExprKind::Const { value, width: 0 },
                    source: span,
                }),
            };
        }

        let value = self.lower_expr(operand, None)?;
        let Ty::SInt(width) = value.ty else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                format!(
                    "`-` is only defined for `int<W>` values, and this is {}",
                    value.ty.describe()
                ),
            )
            .with_note(
                "negate a signed value, or reinterpret the bits with `as int<W>` first",
                None,
            ));
        };
        Ok(Expr {
            ty: Ty::SInt(width),
            kind: ExprKind::Unary {
                op: UnOp::Neg,
                operand: Box::new(value),
            },
            source: span,
        })
    }

    /// `expr as uint<W>` / `expr as int<W>`: the same bits, read the other way.
    ///
    /// Only a reinterpretation is a cast: the width never changes, and a
    /// literal with no type of its own takes the cast's type (and its range
    /// check), exactly as it takes the type of the position it is written in.
    /// An operand that already has a type is lowered on its own, so the cast can
    /// never retype the operators inside it.
    fn lower_cast(&mut self, cast: &syn::ExprCast, span: SourceSpan) -> Result<Expr, Diagnostic> {
        let target = self.resolve_type(&cast.ty, span.clone())?;
        if !matches!(target, Ty::UInt(_) | Ty::SInt(_)) {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                format!(
                    "`as` reinterprets the bits of a `uint<W>` or `int<W>` value, so {} cannot be the target",
                    target.describe()
                ),
            ));
        }

        let mut operand = self.lower_expr(&cast.expr, None)?;
        if operand.ty.width() == 0 {
            operand = self.lower_expr(&cast.expr, Some(&target))?;
            if operand.ty.width() == 0 {
                return Err(Diagnostic::error(
                    codes::WIDTH_UNKNOWN,
                    Some(span),
                    "cannot infer the width of this integer literal; annotate the value's type (REQ-LANG-030)",
                ));
            }
        }
        if !matches!(operand.ty, Ty::UInt(_) | Ty::SInt(_)) {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                format!(
                    "`as` reinterprets a `uint<W>` or `int<W>` value, and this is {}",
                    operand.ty.describe()
                ),
            ));
        }
        if operand.ty.width() != target.width() {
            return Err(Diagnostic::error(
                codes::OUT_OF_RANGE,
                Some(span),
                format!(
                    "`as` reinterprets bits at the same width and cannot change {} to {}; \
                     write the extension explicitly (`zext::<{}>()`, `sext::<{}>()` or `trunc::<{}>()`)",
                    operand.ty.describe(),
                    target.describe(),
                    target.width(),
                    target.width(),
                    target.width()
                ),
            ));
        }

        let kind = match operand.kind {
            // Reinterpreting a constant re-reads its bits: the value follows the
            // type, the bits do not change.
            ExprKind::Const { value, width } => match reinterpreted_value(value, width, &target) {
                Some(value) => ExprKind::Const { value, width },
                // A pattern no `i128` holds - a negative value read as unsigned
                // at 128 bits or more - keeps its bits in a temporary of its own
                // (signed) type, and the cast reads *that* as the target. It is a
                // signal then, so the reading comes from the type at every use
                // (`$unsigned` on an operand of a comparison), exactly as it does
                // for any other value.
                None => {
                    let constant = Expr {
                        ty: operand.ty,
                        kind: ExprKind::Const { value, width },
                        source: span.clone(),
                    };
                    ExprKind::Signal(self.materialize(constant, &span)?)
                }
            },
            kind => kind,
        };
        Ok(Expr {
            ty: target,
            kind,
            source: span,
        })
    }

    pub(super) fn lower_if_expression(
        &mut self,
        item: &syn::ExprIf,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let condition = self.lower_expr(&item.cond, Some(&Ty::Bool))?;
        if condition.ty != Ty::Bool {
            return Err(Diagnostic::error(
                codes::WIDTH_MISMATCH,
                Some(span),
                "an `if` condition must be `bool`",
            ));
        }
        let Some((_, else_expr)) = &item.else_branch else {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "an `if` used as a value needs an `else` branch",
            ));
        };
        // Only the selected branch runs, so each branch's statements are taken
        // out of the pending list here and emitted inside the `if` that selects
        // them: they must never be hoisted before it.
        let then_mark = self.pending_mark();
        let then_expr = self.lower_block_value(&item.then_branch, expected)?;
        let then_statements = self.take_pending_since(then_mark);
        let else_mark = self.pending_mark();
        let else_expr = match &**else_expr {
            syn::Expr::Block(block) => self.lower_block_value(&block.block, Some(&then_expr.ty))?,
            other => self.lower_expr(other, Some(&then_expr.ty))?,
        };
        let else_statements = self.take_pending_since(else_mark);
        if then_expr.ty != else_expr.ty {
            return Err(Diagnostic::error(
                codes::WIDTH_MISMATCH,
                Some(span),
                format!(
                    "the branches of an `if` value have different widths: {} and {}",
                    then_expr.ty.describe(),
                    else_expr.ty.describe()
                ),
            ));
        }
        let ty = then_expr.ty.clone();
        if then_statements.is_empty() && else_statements.is_empty() {
            return Ok(Expr {
                ty,
                kind: ExprKind::Ternary {
                    cond: Box::new(condition),
                    then_expr: Box::new(then_expr),
                    else_expr: Box::new(else_expr),
                },
                source: span,
            });
        }
        if self.proc.is_none() {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "a branch that runs statements needs a process: write the `if` in `run` or inside `always!`",
            ));
        }
        self.lower_selected_if(
            condition,
            (then_statements, then_expr),
            (else_statements, else_expr),
            ty,
            span,
        )
    }

    /// The value of a block used as an expression: one tail expression.
    pub(super) fn lower_block_value(
        &mut self,
        block: &syn::Block,
        expected: Option<&Ty>,
    ) -> Result<Expr, Diagnostic> {
        let span = self
            .span(block.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        let mut statements = block.stmts.iter();
        match (statements.next(), statements.next()) {
            (Some(syn::Stmt::Expr(value, None)), None) => self.lower_expr(value, expected),
            _ => Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "a block used as a value must contain exactly one expression; write the statements directly in `run`",
            )),
        }
    }

    pub(super) fn lower_struct_literal(
        &mut self,
        item: &syn::ExprStruct,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if item.rest.is_some() || item.dot2_token.is_some() {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "struct update syntax (`..base`) is not supported; write every field",
            ));
        }
        // `Msg::Pair { hi, lo }` is an enum variant with a named payload, not a
        // struct: the qualified path gives the enum.
        if item.path.segments.len() == 2 {
            let enum_name = item.path.segments[0].ident.to_string();
            let variant_name = item.path.segments[1].ident.to_string();
            return self.lower_variant(
                Some(&enum_name),
                &variant_name,
                VariantArgs::Named(&item.fields),
                None,
                span,
            );
        }
        let Some(name) = item
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "struct literal without a name",
            ));
        };
        let ty = self.layout(&name, &span)?;
        let Ty::Struct(struct_ty) = ty.clone() else {
            unreachable!()
        };

        let mut provided: HashMap<String, Expr> = HashMap::new();
        for field in &item.fields {
            let syn::Member::Named(member) = &field.member else {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    "tuple structs are not supported",
                ));
            };
            let field_name = member.to_string();
            let Some(layout_field) = struct_ty
                .fields
                .iter()
                .find(|candidate| candidate.name == field_name)
                .cloned()
            else {
                let available: Vec<String> =
                    struct_ty.fields.iter().map(|f| f.name.clone()).collect();
                return Err(Diagnostic::error(
                    codes::UNKNOWN_NAME,
                    self.span(field.span()),
                    format!("`{name}` has no field `{field_name}`"),
                )
                .with_note(
                    format!("fields of `{name}`: {}", available.join(", ")),
                    None,
                ));
            };
            let value = self.lower_expr(&field.expr, Some(&layout_field.ty))?;
            let field_span = self.span(field.span()).unwrap_or_else(|| span.clone());
            check_width(&layout_field.ty, None, &value, field_span)?;
            provided.insert(field_name, value);
        }

        let missing: Vec<String> = struct_ty
            .fields
            .iter()
            .filter(|field| !provided.contains_key(&field.name))
            .map(|field| field.name.clone())
            .collect();
        if !missing.is_empty() {
            return Err(Diagnostic::error(
                codes::DECLARATION,
                Some(span),
                format!(
                    "`{name}` literal is missing field(s): {}",
                    missing.join(", ")
                ),
            ));
        }

        let elements: Vec<Expr> = struct_ty
            .fields
            .iter()
            .map(|field| provided.remove(&field.name).expect("checked above"))
            .collect();
        Ok(Expr {
            ty,
            kind: ExprKind::Concat(elements),
            source: span,
        })
    }

    /// `(a, b)`: a tuple value, field `0` most significant.
    ///
    /// The value is the same concatenation a struct literal produces, in field
    /// order. With an expected tuple type every element is lowered against its
    /// field's type; without one, the element types themselves make the tuple
    /// type, so the literal is never silently given a width from nowhere.
    pub(super) fn lower_tuple_literal(
        &mut self,
        tuple: &syn::ExprTuple,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if tuple.elems.is_empty() {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "the unit value `()` has no bits and is not a hardware value",
            ));
        }
        let expected_tuple = match expected {
            Some(Ty::Struct(strukt)) if strukt.is_tuple() => {
                if tuple.elems.len() != strukt.fields.len() {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!(
                            "this tuple has {} element(s), but {} were expected",
                            tuple.elems.len(),
                            strukt.fields.len()
                        ),
                    ));
                }
                Some(strukt.clone())
            }
            Some(other) => {
                return Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!("a tuple is not a value of {}", other.describe()),
                ))
            }
            None => None,
        };

        let mut elements = Vec::new();
        for (index, item) in tuple.elems.iter().enumerate() {
            let item_span = self.span(item.span()).unwrap_or_else(|| span.clone());
            let expected_field = expected_tuple
                .as_ref()
                .map(|strukt| strukt.fields[index].ty.clone());
            let value = self.lower_expr(item, expected_field.as_ref())?;
            match &expected_field {
                Some(field_ty) => check_width(field_ty, None, &value, item_span)?,
                None if value.ty.width() == 0 => {
                    return Err(Diagnostic::error(
                        codes::WIDTH_UNKNOWN,
                        Some(item_span),
                        "cannot infer the width of this integer literal; annotate the value's type (REQ-LANG-030)",
                    ))
                }
                None => {}
            }
            elements.push(value);
        }
        let ty = match expected_tuple {
            Some(strukt) => Ty::Struct(strukt),
            None => self.tuple_type(
                elements.iter().map(|element| element.ty.clone()).collect(),
                &span,
            )?,
        };
        Ok(Expr {
            ty,
            kind: ExprKind::Concat(elements),
            source: span,
        })
    }

    pub(super) fn lower_method(
        &mut self,
        call: &syn::ExprMethodCall,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let method = call.method.to_string();
        let receiver = self.lower_expr(&call.receiver, None)?;

        match method.as_str() {
            "zext" | "sext" | "trunc" => {
                let width = self.const_generic_arg(call.turbofish.as_ref(), &method, &span)?;
                let source_width = receiver.ty.width();
                if matches!(receiver.ty, Ty::Struct(_) | Ty::Array { .. } | Ty::Enum(_)) {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!("`{method}` is not supported on {}", receiver.ty.describe()),
                    ));
                }
                if method == "zext" && receiver.ty.is_signed() {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!(
                            "`zext::<{width}>()` is not defined for {}: it would drop the sign",
                            receiver.ty.describe()
                        ),
                    )
                    .with_note(
                        format!(
                            "sign-extend with `sext::<{width}>()`, or reinterpret the bits with `as uint<{source_width}>` first"
                        ),
                        None,
                    ));
                }
                match method.as_str() {
                    "zext" if width < source_width => {
                        return Err(Diagnostic::error(
                            codes::OUT_OF_RANGE,
                            Some(span),
                            format!(
                                "`zext::<{width}>()` would narrow {} to {width} bits",
                                receiver.ty.describe()
                            ),
                        ))
                    }
                    "trunc" if width > source_width => {
                        return Err(Diagnostic::error(
                            codes::OUT_OF_RANGE,
                            Some(span),
                            format!(
                                "`trunc::<{width}>()` would widen {} to {width} bits",
                                receiver.ty.describe()
                            ),
                        ))
                    }
                    "sext" if width < source_width => {
                        return Err(Diagnostic::error(
                            codes::OUT_OF_RANGE,
                            Some(span),
                            format!(
                                "`sext::<{width}>()` would narrow {} to {width} bits",
                                receiver.ty.describe()
                            ),
                        ))
                    }
                    _ => {}
                }
                // Sign extension and truncation keep a signed value signed: the
                // extension follows the sign, and the truncation keeps the low
                // bits, which are the wrapped two's-complement value.
                let ty = if receiver.ty.is_signed() {
                    Ty::SInt(width)
                } else {
                    Ty::UInt(width)
                };
                let kind = match method.as_str() {
                    "trunc" => {
                        if width == source_width {
                            receiver.kind.clone()
                        } else {
                            match self.as_signal(&receiver) {
                                Some((signal, base_range)) => {
                                    let lo = base_range.map(|range| range.0).unwrap_or(0);
                                    ExprKind::Slice {
                                        signal,
                                        lo,
                                        hi: lo + width,
                                    }
                                }
                                None => {
                                    let signal = self.materialize(receiver.clone(), &span)?;
                                    ExprKind::Slice {
                                        signal,
                                        lo: 0,
                                        hi: width,
                                    }
                                }
                            }
                        }
                    }
                    "zext" => {
                        if width == source_width {
                            receiver.kind.clone()
                        } else {
                            let filler = Expr {
                                ty: Ty::UInt(width - source_width),
                                kind: ExprKind::Const {
                                    value: 0,
                                    width: width - source_width,
                                },
                                source: span.clone(),
                            };
                            ExprKind::Concat(vec![filler, receiver.clone()])
                        }
                    }
                    "sext" => {
                        if width == source_width {
                            receiver.kind.clone()
                        } else {
                            let sign_bit = self.sign_bit_expr(&receiver, &span)?;
                            let filler = Expr {
                                ty: Ty::UInt(width - source_width),
                                kind: ExprKind::Replicate {
                                    count: width - source_width,
                                    value: Box::new(sign_bit),
                                },
                                source: span.clone(),
                            };
                            ExprKind::Concat(vec![filler, receiver.clone()])
                        }
                    }
                    _ => unreachable!(),
                };
                let _ = source_width;
                Ok(Expr {
                    ty,
                    kind,
                    source: span,
                })
            }
            "slice" => {
                // `.slice(lo, hi)` names consecutive elements of an array, and a
                // sub-array stays an array - a one-element one included. It is
                // not a bit range: that is `[lo..hi]`.
                let Ty::Array { element, len } = receiver.ty.clone() else {
                    return Err(slice_removed(&receiver.ty, &span));
                };
                if call.args.len() != 2 {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        "`.slice(lo, hi)` takes two element bounds, with `hi` excluded",
                    ));
                }
                let mut bounds = call.args.iter();
                let start = bounds.next().expect("checked: two bounds");
                let end = bounds.next().expect("checked: two bounds");
                let (element_start, element_end) =
                    self.two_bounds(start, end, len, "element", &span)?;
                let ty = Ty::Array {
                    element: element.clone(),
                    len: element_end - element_start,
                };
                let width = element.width();
                let (bit_lo, bit_hi) = (element_start * width, element_end * width);
                match self.as_signal(&receiver) {
                    Some((signal, base_range)) => {
                        let base = base_range.map(|(lo, _)| lo).unwrap_or(0);
                        Ok(self.range_expr(
                            signal,
                            Some((base + bit_lo, base + bit_hi)),
                            &ty,
                            span,
                        ))
                    }
                    None => {
                        let signal = self.materialize(receiver, &span)?;
                        Ok(self.range_expr(signal, Some((bit_lo, bit_hi)), &ty, span))
                    }
                }
            }
            "get" | "bit" => {
                if call.args.len() != 1 {
                    return Err(Diagnostic::error(
                        codes::UNSUPPORTED,
                        Some(span),
                        format!("`{method}` takes exactly one index"),
                    ));
                }
                let index = consteval::eval(&call.args[0], &self.cur.env).map_err(|message| {
                    Diagnostic::error(codes::NOT_CONSTANT, self.span(call.args[0].span()), message)
                })?;
                let width = receiver.ty.width();
                if index >= u64::from(width) {
                    return Err(Diagnostic::error(
                        codes::OUT_OF_RANGE,
                        Some(span),
                        format!(
                            "bit {index} is out of range for a {width}-bit value (valid: 0..{})",
                            width.saturating_sub(1)
                        ),
                    ));
                }
                let index = index as u32;
                self.bit_of(receiver, index, span)
            }
            "extern" => {
                Err(
                    Diagnostic::error(codes::UNKNOWN_NAME, Some(span), "unknown method `extern`")
                        .with_note("zero extension is written `zext::<W>()`", None),
                )
            }
            other => Err(Diagnostic::error(
                codes::UNKNOWN_NAME,
                Some(span),
                format!("unknown method `{other}`"),
            )
            .with_note(
                "supported: zext::<W>(), sext::<W>(), trunc::<W>(), get(i), bit(i), and `.slice(lo, hi)` for an array sub-array",
                None,
            )),
        }
    }

    /// Bits `[lo, hi)` of a value.
    pub(super) fn slice_of(
        &mut self,
        receiver: Expr,
        lo: u32,
        hi: u32,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let width = receiver.ty.width();
        if lo >= hi || hi > width {
            return Err(Diagnostic::error(
                codes::OUT_OF_RANGE,
                Some(span),
                format!("slice {lo}..{hi} is out of range for a {width}-bit value"),
            ));
        }
        // One bit is a `bool`, whatever the value it was cut out of: a bit is a
        // bit, and a one-bit `uint` would be a second spelling of the same thing.
        // A wider range is raw bits, `uint<hi - lo>`.
        let ty = if hi - lo == 1 {
            Ty::Bool
        } else {
            Ty::UInt(hi - lo)
        };
        match self.as_signal(&receiver) {
            Some((signal, base_range)) => {
                let base = base_range.map(|range| range.0).unwrap_or(0);
                let absolute = (base + lo, base + hi);
                let signal_width = self.cur.signal(signal).width();
                let kind = if absolute.0 == 0 && absolute.1 == signal_width && !ty.is_bool() {
                    ExprKind::Signal(signal)
                } else if ty.is_bool() && absolute.1 - absolute.0 == 1 && signal_width > 1 {
                    ExprKind::Slice {
                        signal,
                        lo: absolute.0,
                        hi: absolute.1,
                    }
                } else if ty.is_bool() {
                    ExprKind::Signal(signal)
                } else {
                    ExprKind::Slice {
                        signal,
                        lo: absolute.0,
                        hi: absolute.1,
                    }
                };
                Ok(Expr {
                    ty,
                    kind,
                    source: span,
                })
            }
            None => {
                let signal = self.materialize(receiver, &span)?;
                let kind = if ty.is_bool() && self.cur.signal(signal).width() > 1 {
                    ExprKind::Slice { signal, lo, hi }
                } else if ty.is_bool() {
                    ExprKind::Signal(signal)
                } else {
                    ExprKind::Slice { signal, lo, hi }
                };
                Ok(Expr {
                    ty,
                    kind,
                    source: span,
                })
            }
        }
    }

    /// Bit `index` of a value.
    pub(super) fn bit_of(
        &mut self,
        receiver: Expr,
        index: u32,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        match self.as_signal(&receiver) {
            Some((signal, base_range)) => {
                let base = base_range.map(|range| range.0).unwrap_or(0);
                let absolute = base + index;
                let signal_width = self.cur.signal(signal).width();
                let kind = if signal_width == 1 {
                    ExprKind::Signal(signal)
                } else {
                    ExprKind::Slice {
                        signal,
                        lo: absolute,
                        hi: absolute + 1,
                    }
                };
                Ok(Expr {
                    ty: Ty::Bool,
                    kind,
                    source: span,
                })
            }
            None => {
                let signal = self.materialize(receiver, &span)?;
                let kind = if self.cur.signal(signal).width() == 1 {
                    ExprKind::Signal(signal)
                } else {
                    ExprKind::Slice {
                        signal,
                        lo: index,
                        hi: index + 1,
                    }
                };
                Ok(Expr {
                    ty: Ty::Bool,
                    kind,
                    source: span,
                })
            }
        }
    }

    pub(super) fn sign_bit_expr(
        &mut self,
        receiver: &Expr,
        span: &SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let width = receiver.ty.width();
        match self.as_signal(receiver) {
            Some((signal, base_range)) => {
                let base = base_range.map(|range| range.0).unwrap_or(0);
                let bit = base + width - 1;
                let kind = if self.cur.signal(signal).width() == 1 {
                    ExprKind::Signal(signal)
                } else {
                    ExprKind::Slice {
                        signal,
                        lo: bit,
                        hi: bit + 1,
                    }
                };
                Ok(Expr {
                    ty: Ty::Bool,
                    kind,
                    source: span.clone(),
                })
            }
            None => {
                let signal = self.materialize(receiver.clone(), span)?;
                let bit = width - 1;
                let kind = if self.cur.signal(signal).width() == 1 {
                    ExprKind::Signal(signal)
                } else {
                    ExprKind::Slice {
                        signal,
                        lo: bit,
                        hi: bit + 1,
                    }
                };
                Ok(Expr {
                    ty: Ty::Bool,
                    kind,
                    source: span.clone(),
                })
            }
        }
    }

    /// The signal and current range behind an expression, when it is one.
    pub(super) fn as_signal(&self, expr: &Expr) -> Option<(SignalId, Option<(u32, u32)>)> {
        match &expr.kind {
            ExprKind::Signal(signal) => Some((*signal, None)),
            ExprKind::Slice { signal, lo, hi } => Some((*signal, Some((*lo, *hi)))),
            _ => None,
        }
    }

    pub(super) fn const_generic_arg(
        &self,
        turbofish: Option<&syn::AngleBracketedGenericArguments>,
        method: &str,
        span: &SourceSpan,
    ) -> Result<u32, Diagnostic> {
        let Some(turbofish) = turbofish else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("`{method}` needs an explicit width, for example `{method}::<8>()`"),
            ));
        };
        let mut args = turbofish.args.iter();
        let Some(value) = args.next().and_then(parse::const_argument) else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("`{method}::<W>()` needs a build-time width"),
            ));
        };
        consteval::eval_width(&value, &self.cur.env)
            .map_err(|message| Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message))
    }

    /// The hardware type a `...::from(...)` call path names.
    ///
    /// `uint::<W>` and `int::<W>` take the width as a const argument; `uint8`
    /// and `int8` carry it in the name, exactly as in a type position.
    fn call_type(
        &self,
        path: &syn::Path,
        type_name: &str,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        if type_name == "bool" {
            return Ok(Ty::Bool);
        }
        let (signed, width_text) = match type_name.strip_prefix("int") {
            Some(rest) => (true, rest),
            None => match type_name.strip_prefix("uint") {
                Some(rest) => (false, rest),
                None => return Err(not_a_hardware_type(type_name, span)),
            },
        };
        let width = if width_text.is_empty() {
            let Some(segment) = path.segments.iter().rev().nth(1) else {
                return Err(Diagnostic::error(
                    codes::BAD_TYPE,
                    Some(span.clone()),
                    format!("`{type_name}` needs a width"),
                ));
            };
            let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                return Err(Diagnostic::error(
                    codes::BAD_TYPE,
                    Some(span.clone()),
                    format!("`{type_name}::<W>::from(...)` needs an explicit width"),
                ));
            };
            let Some(value) = arguments.args.iter().next().and_then(parse::const_argument) else {
                return Err(Diagnostic::error(
                    codes::BAD_TYPE,
                    Some(span.clone()),
                    format!("`{type_name}::<W>` needs a build-time width"),
                ));
            };
            consteval::eval_width(&value, &self.cur.env).map_err(|message| {
                Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message)
            })?
        } else {
            let Ok(width) = width_text.parse::<u64>() else {
                return Err(not_a_hardware_type(type_name, span));
            };
            consteval::check_width(width).map_err(|message| {
                Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message)
            })?
        };
        Ok(if signed {
            Ty::SInt(width)
        } else {
            Ty::UInt(width)
        })
    }

    pub(super) fn lower_call(
        &mut self,
        call: &syn::ExprCall,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        // `uint::<8>::from(0)`, `int::<8>::from(-1)` and `bool::from(true)`: an
        // explicitly typed literal.
        let syn::Expr::Path(path) = &*call.func else {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "only `uint::<W>::from(value)`, `int::<W>::from(value)` and `bool::from(value)` calls are supported in hardware expressions",
            ));
        };
        let segments: Vec<String> = path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        let Some(last) = segments.last() else {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "empty call path",
            ));
        };
        if last != "from" || segments.len() < 2 {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "only `uint::<W>::from(value)`, `int::<W>::from(value)` and `bool::from(value)` calls are supported in hardware expressions",
            ));
        }
        if call.args.len() != 1 {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "`from` takes exactly one value",
            ));
        }
        let ty = self.call_type(&path.path, &segments[segments.len() - 2], &span)?;

        match &ty {
            // A stored value is a value, so a signed target reads a negated
            // argument (`int::<8>::from(-1)`); an unsigned one does not, and
            // says so with the usual build-time-constant diagnostic.
            Ty::SInt(_) => {
                let value =
                    signed_const_value(&call.args[0], &self.cur.env).map_err(|message| {
                        Diagnostic::error(
                            codes::NOT_CONSTANT,
                            self.span(call.args[0].span()),
                            message,
                        )
                    })?;
                typed_constant(value, ty.clone(), span)
            }
            Ty::UInt(_) => {
                let value = consteval::eval(&call.args[0], &self.cur.env).map_err(|message| {
                    Diagnostic::error(codes::NOT_CONSTANT, self.span(call.args[0].span()), message)
                })? as i128;
                typed_constant(value, ty.clone(), span)
            }
            Ty::Bool => {
                let value = consteval::eval(&call.args[0], &self.cur.env).map_err(|message| {
                    Diagnostic::error(codes::NOT_CONSTANT, self.span(call.args[0].span()), message)
                })?;
                if value > 1 {
                    return Err(Diagnostic::error(
                        codes::OUT_OF_RANGE,
                        Some(span),
                        format!("{value} is not a `bool` value"),
                    ));
                }
                Ok(Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::Const {
                        value: value as i128,
                        width: 1,
                    },
                    source: span,
                })
            }
            Ty::Struct(_) | Ty::Array { .. } | Ty::Enum(_) => {
                unreachable!("only bool, uint and int are constructed here")
            }
        }
    }

    pub(super) fn lower_index(
        &mut self,
        index: &syn::ExprIndex,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let receiver = self.lower_expr(&index.expr, None)?;

        // A range is a bit range of the packed value, whatever that value is:
        // `[lo..hi]` selects bits of a `uint`, an `int`, a struct or an array
        // alike, so it is decided before the type says anything else. The bits
        // are raw: the result is a `uint` (or a `bool` for one bit), never an
        // array.
        if let syn::Expr::Range(range) = &*index.index {
            let lo = match &range.start {
                Some(start) => consteval::eval(start, &self.cur.env).map_err(|message| {
                    Diagnostic::error(codes::NOT_CONSTANT, self.span(start.span()), message)
                })?,
                None => 0,
            };
            let hi = match &range.end {
                Some(end) => consteval::eval(end, &self.cur.env).map_err(|message| {
                    Diagnostic::error(codes::NOT_CONSTANT, self.span(end.span()), message)
                })?,
                None => {
                    return Err(Diagnostic::error(
                        codes::NOT_CONSTANT,
                        Some(span),
                        "a bit range needs explicit bounds, for example `x[0..4]`",
                    ))
                }
            };
            let hi = match range.limits {
                syn::RangeLimits::Closed(_) => hi.checked_add(1).ok_or_else(|| {
                    Diagnostic::error(
                        codes::OUT_OF_RANGE,
                        Some(span.clone()),
                        "the end of this inclusive range overflows a 64-bit constant",
                    )
                })?,
                syn::RangeLimits::HalfOpen(_) => hi,
            };
            let (lo, hi) = (check_bound(lo, &span)?, check_bound(hi, &span)?);
            return self.slice_of(receiver, lo, hi, span);
        }

        // A single index names an element of an array, a bit of an integer, and
        // nothing at all of a struct: a struct's bits are not addressed one at a
        // time, its fields are selected by name.
        if let Ty::Array { element, len } = receiver.ty.clone() {
            return self.lower_array_index(receiver, *element, len, &index.index, span);
        }
        match &receiver.ty {
            Ty::Struct(_) => {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    "a struct value cannot be indexed; select a field by name",
                ))
            }
            Ty::Enum(_) => {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    "an enum value cannot be indexed; compare it, or match on its variants",
                ))
            }
            _ => {}
        }

        // A build-time constant selects a bit, a runtime value is an indexed bit
        // select.
        match consteval::eval(&index.index, &self.cur.env) {
            Ok(value) => {
                let width = receiver.ty.width();
                if value >= u64::from(width) {
                    return Err(Diagnostic::error(
                        codes::OUT_OF_RANGE,
                        Some(span),
                        format!(
                            "bit {value} is out of range for a {width}-bit value (valid: 0..{})",
                            width.saturating_sub(1)
                        ),
                    ));
                }
                self.bit_of(receiver, value as u32, span)
            }
            Err(_) => {
                let index_expr =
                    self.lower_expr(&index.index, Some(&Ty::UInt(receiver.ty.width())))?;
                if !matches!(index_expr.ty, Ty::UInt(_)) {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        "an index must be an unsigned integer",
                    ));
                }
                let signal = self.select_signal(receiver, &span)?;
                Ok(Expr {
                    ty: Ty::Bool,
                    kind: ExprKind::BitSelect {
                        signal,
                        index: Box::new(index_expr),
                    },
                    source: span,
                })
            }
        }
    }

    /// `arr[i]`: the element a build-time or run-time index selects.
    ///
    /// Consecutive elements are not selected here: `[lo..hi]` is a bit range of
    /// the array value, and `.slice(lo, hi)` names a sub-array.
    fn lower_array_index(
        &mut self,
        receiver: Expr,
        element: Ty,
        len: u32,
        index: &syn::Expr,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if consteval::eval(index, &self.cur.env).is_err() {
            let index_expr = self.lower_expr(index, None)?;
            if !matches!(index_expr.ty, Ty::UInt(_)) {
                return Err(Diagnostic::error(
                    codes::BAD_TYPE,
                    Some(span),
                    format!(
                        "an element index must be an unsigned value, and this is {}",
                        index_expr.ty.describe()
                    ),
                ));
            }
            let signal = self.select_signal(receiver, &span)?;
            return Ok(Expr {
                ty: element,
                kind: ExprKind::BitSelect {
                    signal,
                    index: Box::new(index_expr),
                },
                source: span,
            });
        }

        let (element_index, _) = self.bits_range(index, len, "element", &span)?;
        let width = element.width();
        let (bit_lo, bit_hi) = (element_index * width, (element_index + 1) * width);
        match self.as_signal(&receiver) {
            Some((signal, base_range)) => {
                let base = base_range.map(|(lo, _)| lo).unwrap_or(0);
                Ok(self.range_expr(signal, Some((base + bit_lo, base + bit_hi)), &element, span))
            }
            None => {
                let signal = self.materialize(receiver, &span)?;
                Ok(self.range_expr(signal, Some((bit_lo, bit_hi)), &element, span))
            }
        }
    }

    /// The signal a run-time select reads.
    ///
    /// The value itself when it is one whole signal of exactly that type;
    /// otherwise the value gets a name first, so the select never reaches past
    /// what it indexes: a field, a sub-array or a low slice must not spill into
    /// its neighbour, and a select of an array element must not be taken for a
    /// bit select of the whole array signal.
    fn select_signal(&mut self, receiver: Expr, span: &SourceSpan) -> Result<SignalId, Diagnostic> {
        match self.as_signal(&receiver) {
            Some((signal, None)) if self.cur.signal(signal).ty == receiver.ty => Ok(signal),
            _ => self.materialize(receiver, span),
        }
    }

    /// Does this expression read through a run-time element index?
    ///
    /// `cells[i].tag` is a value read of the selected element, not a place; a
    /// static chain (`self.pkt.op`, `b.items[0].tag`) is a place and takes the
    /// ordinary path.
    fn has_run_time_index(&self, expr: &syn::Expr) -> bool {
        match expr {
            syn::Expr::Index(index) => {
                self.is_run_time_index(&index.index) || self.has_run_time_index(&index.expr)
            }
            syn::Expr::Field(field) => self.has_run_time_index(&field.base),
            syn::Expr::Paren(paren) => self.has_run_time_index(&paren.expr),
            syn::Expr::Group(group) => self.has_run_time_index(&group.expr),
            _ => false,
        }
    }

    /// `value.field` where `value` is a value, not a place.
    fn lower_value_field(
        &mut self,
        field: &syn::ExprField,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let name = match &field.member {
            syn::Member::Named(member) => member.to_string(),
            syn::Member::Unnamed(index) => index.index.to_string(),
        };
        let base = self.lower_expr(&field.base, None)?;
        let Some(layout) = base.ty.field(&name).cloned() else {
            return Err(Diagnostic::error(
                codes::UNKNOWN_NAME,
                Some(span),
                format!("{} has no field `{name}`", base.ty.describe()),
            ));
        };
        match self.as_signal(&base) {
            Some((signal, base_range)) => {
                let start = base_range.map(|(lo, _)| lo).unwrap_or(0) + layout.offset;
                Ok(self.range_expr(
                    signal,
                    Some((start, start + layout.width)),
                    &layout.ty,
                    span,
                ))
            }
            None => {
                let signal = self.materialize(base, &span)?;
                Ok(self.range_expr(
                    signal,
                    Some((layout.offset, layout.offset + layout.width)),
                    &layout.ty,
                    span,
                ))
            }
        }
    }

    /// `[a, b, c]`: an array literal, element 0 first.
    ///
    /// The elements are lowered against the element type when the position gives
    /// one. Without a context the first element fixes the element type and the
    /// rest have to agree with it, so a list never mixes shapes silently.
    pub(super) fn lower_array_literal(
        &mut self,
        array: &syn::ExprArray,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let expected_element = match expected {
            Some(Ty::Array { element, len }) => {
                if array.elems.len() as u64 != u64::from(*len) {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!(
                            "this array literal has {} element(s), but {len} were expected",
                            array.elems.len()
                        ),
                    ));
                }
                Some(element.as_ref().clone())
            }
            Some(other) => {
                return Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!("an array literal is not a value of {}", other.describe()),
                ))
            }
            None => None,
        };

        let mut elements = Vec::new();
        let mut element_ty = expected_element;
        for item in &array.elems {
            let value = self.lower_expr(item, element_ty.as_ref())?;
            if value.ty.width() == 0 {
                return Err(Diagnostic::error(
                    codes::WIDTH_UNKNOWN,
                    Some(span),
                    "cannot infer the width of this integer literal; annotate the value's type (REQ-LANG-030)",
                ));
            }
            match &element_ty {
                Some(ty) => check_width(ty, None, &value, span.clone())?,
                None => element_ty = Some(value.ty.clone()),
            }
            elements.push(value);
        }
        let Some(element) = element_ty else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "an array literal needs at least one element",
            ));
        };
        let ty = self.array_type(element, elements.len() as u64, &span)?;
        // A concatenation lists its most significant element first.
        elements.reverse();
        Ok(Expr {
            ty,
            kind: ExprKind::Concat(elements),
            source: span,
        })
    }

    /// `[value; N]`: `N` copies of one value.
    pub(super) fn lower_repeat_literal(
        &mut self,
        repeat: &syn::ExprRepeat,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let len = consteval::eval(&repeat.len, &self.cur.env).map_err(|message| {
            Diagnostic::error(codes::NOT_CONSTANT, self.span(repeat.len.span()), message)
        })?;
        let expected_element = match expected {
            Some(Ty::Array {
                element,
                len: expected_len,
            }) => {
                if u64::from(*expected_len) != len {
                    return Err(Diagnostic::error(
                        codes::WIDTH_MISMATCH,
                        Some(span),
                        format!(
                            "this repeat has {len} element(s), but {expected_len} were expected"
                        ),
                    ));
                }
                Some(element.as_ref().clone())
            }
            Some(other) => {
                return Err(Diagnostic::error(
                    codes::WIDTH_MISMATCH,
                    Some(span),
                    format!("an array literal is not a value of {}", other.describe()),
                ))
            }
            None => None,
        };

        let value = self.lower_expr(&repeat.expr, expected_element.as_ref())?;
        if value.ty.width() == 0 {
            return Err(Diagnostic::error(
                codes::WIDTH_UNKNOWN,
                Some(span),
                "cannot infer the width of this integer literal; annotate the value's type (REQ-LANG-030)",
            ));
        }
        if let Some(ty) = &expected_element {
            check_width(ty, None, &value, span.clone())?;
        }
        let ty = self.array_type(value.ty.clone(), len, &span)?;
        let count = match &ty {
            Ty::Array { len, .. } => *len,
            _ => unreachable!("array_type builds an array"),
        };
        Ok(Expr {
            ty,
            kind: ExprKind::Replicate {
                count,
                value: Box::new(value),
            },
            source: span,
        })
    }

    /// A single name, a qualified variant (`Msg::Idle`), or a bare variant of the
    /// enum the position expects (`None`).
    fn lower_path_expr(
        &mut self,
        path: &syn::ExprPath,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if path.qself.is_some() {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "qualified paths are not supported in hardware expressions",
            ));
        }
        match path.path.segments.len() {
            1 => {
                let name = path.path.segments[0].ident.to_string();
                // A signal, a local or a const parameter of that name is a value
                // and wins over a variant spelling.
                if self.name_resolves(&name) {
                    return self.lower_name(&name, expected, span);
                }
                if let Some(Ty::Enum(enum_ty)) = expected {
                    if enum_ty.variant(&name).is_some() {
                        return self.lower_variant(None, &name, VariantArgs::Unit, expected, span);
                    }
                }
                // Not a value and not a variant: the ordinary unknown name.
                self.lower_name(&name, expected, span)
            }
            2 => {
                let enum_name = path.path.segments[0].ident.to_string();
                let variant_name = path.path.segments[1].ident.to_string();
                self.lower_variant(
                    Some(&enum_name),
                    &variant_name,
                    VariantArgs::Unit,
                    expected,
                    span,
                )
            }
            count => Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                format!("unsupported path in expression ({count} segments)"),
            )),
        }
    }

    /// Is this bare name a value this process can read?
    fn name_resolves(&self, name: &str) -> bool {
        self.proc
            .as_ref()
            .is_some_and(|context| context.locals.contains_key(name))
            || self.cur.signal_by_name.contains_key(name)
            || self.cur.env.contains_key(name)
    }

    /// A call: a variant construction (`Some(v)`, `Msg::Data(v)`) or the
    /// explicitly typed literal `uint::<W>::from(v)`.
    fn lower_call_or_variant(
        &mut self,
        call: &syn::ExprCall,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let syn::Expr::Path(path) = &*call.func else {
            return self.lower_call(call, span);
        };
        let segments: Vec<String> = path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        // A call of a `#[hardware_instance] fn` is an instance of that
        // function's module, not a Rust call and not a value constructor.
        if segments.len() == 1
            && self
                .parsed
                .functions
                .iter()
                .any(|function| function.name == segments[0])
        {
            return self.lower_function_call(&segments[0], call, span);
        }
        if segments.last().map(String::as_str) == Some("from") {
            return self.lower_call(call, span);
        }
        let (enum_name, variant_name) = match segments.len() {
            1 => (None, segments[0].clone()),
            2 => (Some(segments[0].clone()), segments[1].clone()),
            _ => return self.lower_call(call, span),
        };
        // Only an enum position makes a call a construction; anything else is
        // left to the ordinary call path, which names what it expects.
        let names_an_enum = enum_name.as_deref().is_some_and(|name| {
            self.parsed.enums.contains_key(name) || parse::is_builtin_type_name(name)
        });
        if !names_an_enum && !matches!(expected, Some(Ty::Enum(_))) {
            return self.lower_call(call, span);
        }
        self.lower_variant(
            enum_name.as_deref(),
            &variant_name,
            VariantArgs::Positional(&call.args),
            expected,
            span,
        )
    }

    /// Lower a call of a combinational function into an instance.
    ///
    /// The instance is created at the call site with a unique name, its inputs
    /// are connected from the arguments and its outputs become the value of the
    /// call: a single output is that signal, several outputs are a tuple value
    /// the caller destructures.
    fn lower_function_call(
        &mut self,
        name: &str,
        call: &syn::ExprCall,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let module = self.ensure_function_module(name).map_err(|()| {
            Diagnostic::error(
                codes::INTERNAL,
                Some(span.clone()),
                format!("hardware function `{name}` could not be lowered"),
            )
        })?;
        let ports: Vec<ChildPort> = self.modules[module]
            .ports
            .iter()
            .map(|port| ChildPort {
                name: port.verilog_name.clone(),
                dir: port.dir,
                ty: self.modules[module].signals[port.signal].ty.clone(),
                parent_signal: None,
            })
            .collect();
        let inputs: Vec<ChildPort> = ports
            .iter()
            .filter(|port| port.dir == PortDir::In)
            .cloned()
            .collect();
        if call.args.len() != inputs.len() {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                format!(
                    "`{name}` takes {} argument(s), {} given",
                    inputs.len(),
                    call.args.len()
                ),
            ));
        }

        self.cur.temp_count += 1;
        let field = format!("{name}__{}", self.cur.temp_count);
        let index = self.cur.instances.len();
        let mut child_ports = ports;
        for child_port in child_ports.iter_mut() {
            if child_port.dir != PortDir::Out {
                continue;
            }
            let net_name = format!("{field}_{}", child_port.name);
            self.declare_signal(
                &net_name,
                None,
                child_port.ty.clone(),
                SignalKind::Wire,
                Some(span.clone()),
            )
            .map_err(|()| {
                Diagnostic::error(
                    codes::NAME_COLLISION,
                    Some(span.clone()),
                    format!("`{net_name}` is already declared"),
                )
            })?;
            let signal = self.cur.signal_by_name[&net_name];
            if self
                .claim_or_report(signal, None, Owner::Instance(index), &span)
                .is_err()
            {
                return Err(Diagnostic::error(
                    codes::INTERNAL,
                    Some(span.clone()),
                    format!("`{net_name}` cannot be driven by the function instance"),
                ));
            }
            child_port.parent_signal = Some(signal);
        }

        let mut connections: Vec<Option<(Expr, SourceSpan)>> = vec![None; child_ports.len()];
        for (position, argument) in call.args.iter().enumerate() {
            let port = &inputs[position];
            let port_index = child_ports
                .iter()
                .position(|candidate| candidate.name == port.name)
                .expect("input port is part of the instance");
            let value = self.lower_expr_top(argument, Some(&port.ty))?;
            check_width(&port.ty, None, &value, span.clone())?;
            connections[port_index] = Some((value, span.clone()));
        }

        let ports_read: HashSet<String> = child_ports
            .iter()
            .filter(|port| port.dir == PortDir::Out)
            .map(|port| port.name.clone())
            .collect();
        let output_values: Vec<(Ty, SignalId)> = child_ports
            .iter()
            .filter(|port| port.dir == PortDir::Out)
            .map(|port| {
                (
                    port.ty.clone(),
                    port.parent_signal.expect("output ports have a net"),
                )
            })
            .collect();

        self.cur.instances.push(InstanceBuild {
            field,
            verilog_name: String::new(),
            module_source_name: name.to_string(),
            module_verilog_name: self.modules[module].verilog_name.clone(),
            params: Vec::new(),
            source: span.clone(),
            child_ports,
            connections,
            read_outputs: ports_read,
        });
        // The emitter names the instance from `verilog_name`; the call site is
        // anonymous, so it gets the same unique name as the field.
        let instance = self.cur.instances.last_mut().expect("just pushed");
        instance.verilog_name = instance.field.clone();

        let mut values = Vec::new();
        for (ty, signal) in output_values {
            self.reads.insert(signal);
            values.push(Expr {
                ty,
                kind: ExprKind::Signal(signal),
                source: span.clone(),
            });
        }
        if values.len() == 1 {
            return Ok(values.pop().expect("one value"));
        }
        let elements: Vec<Ty> = values.iter().map(|value| value.ty.clone()).collect();
        let ty = self.tuple_type(elements, &span)?;
        Ok(Expr {
            ty,
            kind: ExprKind::Concat(values),
            source: span,
        })
    }

    /// One enum value: `None`, `Some(v)`, `Msg::Data(v)` or `Msg::Pair { hi, lo }`.
    ///
    /// The value is the packed layout: the tag in the lowest bits, the payload
    /// above it, and zero in the payload room the variant does not use
    /// (REQ-LANG-040, REQ-LANG-041).
    fn lower_variant(
        &mut self,
        enum_name: Option<&str>,
        variant_name: &str,
        args: VariantArgs<'_>,
        expected: Option<&Ty>,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        // The enum is the one written, or the one the position expects.
        let enum_ty = match enum_name {
            Some(name) => {
                // A built-in is never a declared data type, and a qualified path
                // cannot carry its generic argument: it comes from the type the
                // position expects, never from a sibling type.
                if parse::is_builtin_type_name(name) {
                    match expected {
                        Some(Ty::Enum(enum_ty)) if type_root(&enum_ty.name) == name => {
                            (*enum_ty).clone()
                        }
                        _ => {
                            return Err(Diagnostic::error(
                                codes::WIDTH_UNKNOWN,
                                Some(span),
                                format!(
                                    "cannot tell which `{name}` this is; annotate the value's type (REQ-LANG-030)"
                                ),
                            ))
                        }
                    }
                } else {
                    let ty = self.layout(name, &span)?;
                    let Ty::Enum(enum_ty) = ty else {
                        return Err(Diagnostic::error(
                            codes::BAD_TYPE,
                            Some(span),
                            format!("`{name}` is not an enum, so it has no variant `{variant_name}`"),
                        ));
                    };
                    enum_ty
                }
            }
            None => match expected {
                Some(Ty::Enum(enum_ty)) => (*enum_ty).clone(),
                _ => {
                    return Err(Diagnostic::error(
                        codes::WIDTH_UNKNOWN,
                        Some(span),
                        format!(
                            "cannot tell which enum `{variant_name}` belongs to; annotate the value's type (REQ-LANG-030)"
                        ),
                    ))
                }
            },
        };
        let Some(variant) = enum_ty.variant(variant_name).cloned() else {
            let available: Vec<String> = enum_ty
                .variants
                .iter()
                .map(|variant| variant.name.clone())
                .collect();
            return Err(Diagnostic::error(
                codes::UNKNOWN_NAME,
                Some(span),
                format!("enum `{}` has no variant `{variant_name}`", enum_ty.name),
            )
            .with_note(
                format!("variants of `{}`: {}", enum_ty.name, available.join(", ")),
                None,
            ));
        };

        let payload = self.lower_payload(&enum_ty, &variant, variant_name, args, &span)?;
        let tag = Expr {
            ty: Ty::UInt(enum_ty.tag_width),
            kind: ExprKind::Const {
                value: i128::from(variant.tag),
                width: enum_ty.tag_width,
            },
            source: span.clone(),
        };
        // A concatenation lists its most significant part first, so the room the
        // variant does not use comes first, then its payload, then the tag.
        let unused = enum_ty.payload_width() - variant.payload_width();
        let mut elements = Vec::new();
        if unused > 0 {
            elements.push(Expr {
                ty: Ty::UInt(unused),
                kind: ExprKind::Const {
                    value: 0,
                    width: unused,
                },
                source: span.clone(),
            });
        }
        if let Some(payload) = payload {
            elements.push(payload);
        }
        elements.push(tag);
        Ok(Expr {
            ty: Ty::Enum(enum_ty),
            kind: ExprKind::Concat(elements),
            source: span,
        })
    }

    /// The payload value of a variant construction, checked against the
    /// variant's own payload type.
    fn lower_payload(
        &mut self,
        enum_ty: &EnumTy,
        variant: &EnumVariantTy,
        variant_name: &str,
        args: VariantArgs<'_>,
        span: &SourceSpan,
    ) -> Result<Option<Expr>, Diagnostic> {
        let enum_name = &enum_ty.name;
        match (&variant.payload, args) {
            (None, VariantArgs::Unit) => Ok(None),
            (None, _) => Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("`{enum_name}::{variant_name}` carries no payload"),
            )),
            (Some(payload_ty), VariantArgs::Unit) => Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!(
                    "`{enum_name}::{variant_name}` needs a payload of type {}",
                    payload_ty.describe()
                ),
            )),
            (Some(payload_ty), VariantArgs::Positional(values)) => {
                match payload_form(payload_ty, enum_name, variant_name) {
                    // `V(T, U)`: one value per declared positional field.
                    PayloadForm::Tuple => {
                        let Ty::Struct(strukt) = payload_ty else {
                            unreachable!("a tuple payload is laid out as a struct")
                        };
                        if values.len() != strukt.fields.len() {
                            return Err(Diagnostic::error(
                                codes::BAD_TYPE,
                                Some(span.clone()),
                                format!(
                                    "`{enum_name}::{variant_name}` takes {} payload value(s)",
                                    strukt.fields.len()
                                ),
                            ));
                        }
                        let mut elements = Vec::new();
                        for (value, field) in values.iter().zip(&strukt.fields) {
                            let lowered = self.lower_expr(value, Some(&field.ty))?;
                            check_width(
                                &field.ty,
                                None,
                                &lowered,
                                self.span(value.span()).unwrap_or_else(|| span.clone()),
                            )?;
                            elements.push(lowered);
                        }
                        Ok(Some(Expr {
                            ty: payload_ty.clone(),
                            kind: ExprKind::Concat(elements),
                            source: span.clone(),
                        }))
                    }
                    // One value of its own type: `V(Packet)` is not several
                    // values because `Packet` happens to have fields.
                    PayloadForm::Single => {
                        if values.len() != 1 {
                            return Err(Diagnostic::error(
                                codes::BAD_TYPE,
                                Some(span.clone()),
                                format!(
                                    "`{enum_name}::{variant_name}` takes one payload value of type {}",
                                    payload_ty.describe()
                                ),
                            ));
                        }
                        let value = self.lower_expr(&values[0], Some(payload_ty))?;
                        check_width(
                            payload_ty,
                            None,
                            &value,
                            self.span(values[0].span()).unwrap_or_else(|| span.clone()),
                        )?;
                        Ok(Some(value))
                    }
                    // Named fields are not passed positionally.
                    PayloadForm::Named => Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span.clone()),
                        format!(
                            "`{enum_name}::{variant_name}` declares named fields; write `{enum_name}::{variant_name} {{ .. }}`"
                        ),
                    )),
                }
            }
            (Some(payload_ty), VariantArgs::Named(values)) => {
                if payload_form(payload_ty, enum_name, variant_name) != PayloadForm::Named {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span.clone()),
                        format!(
                            "`{enum_name}::{variant_name}` takes a positional payload, not named fields"
                        ),
                    ));
                }
                let Ty::Struct(strukt) = payload_ty else {
                    unreachable!("a named payload is laid out as a struct")
                };
                let mut provided: HashMap<String, Expr> = HashMap::new();
                for value in values {
                    let syn::Member::Named(member) = &value.member else {
                        return Err(Diagnostic::error(
                            codes::UNSUPPORTED,
                            Some(span.clone()),
                            "payload fields need names",
                        ));
                    };
                    let name = member.to_string();
                    let Some(field) = strukt.fields.iter().find(|field| field.name == name) else {
                        return Err(Diagnostic::error(
                            codes::UNKNOWN_NAME,
                            self.span(member.span()).or_else(|| Some(span.clone())),
                            format!("`{enum_name}::{variant_name}` has no field `{name}`"),
                        ));
                    };
                    let lowered = self.lower_expr(&value.expr, Some(&field.ty))?;
                    check_width(
                        &field.ty,
                        None,
                        &lowered,
                        self.span(value.span()).unwrap_or_else(|| span.clone()),
                    )?;
                    provided.insert(name, lowered);
                }
                let missing: Vec<String> = strukt
                    .fields
                    .iter()
                    .filter(|field| !provided.contains_key(&field.name))
                    .map(|field| field.name.clone())
                    .collect();
                if !missing.is_empty() {
                    return Err(Diagnostic::error(
                        codes::DECLARATION,
                        Some(span.clone()),
                        format!(
                            "`{enum_name}::{variant_name}` is missing field(s): {}",
                            missing.join(", ")
                        ),
                    ));
                }
                let elements = strukt
                    .fields
                    .iter()
                    .map(|field| provided.remove(&field.name).expect("checked above"))
                    .collect();
                Ok(Some(Expr {
                    ty: payload_ty.clone(),
                    kind: ExprKind::Concat(elements),
                    source: span.clone(),
                }))
            }
        }
    }

    /// `lhs == rhs`, read as the type says rather than as the packed bits say.
    ///
    /// An enum compares its tag and the payload of the variant that is active:
    /// the bits an inactive payload happens to hold are not part of the value
    /// (REQ-LANG-042). A struct or an array recurses field by field and element
    /// by element, and a part with no enum inside compares as bits.
    fn typed_equality(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        ty: &Ty,
        span: &SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let boolean = |kind: ExprKind| Expr {
            ty: Ty::Bool,
            kind,
            source: span.clone(),
        };
        let equal_parts = |lhs: Expr, rhs: Expr| {
            boolean(ExprKind::Binary {
                op: BinOp::Eq,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        };
        if !mentions_enum(ty) {
            // Nothing enum-shaped inside: the bits are the value.
            return Ok(equal_parts(lhs.clone(), rhs.clone()));
        }
        match ty {
            Ty::Enum(enum_ty) => {
                let tag_width = enum_ty.tag_width;
                let lhs_tag = self.slice_value(lhs, 0, tag_width, span)?;
                let rhs_tag = self.slice_value(rhs, 0, tag_width, span)?;
                let tags_equal = equal_parts(lhs_tag.clone(), rhs_tag);
                let mut payload_equal: Option<Expr> = None;
                for variant in enum_ty.variants.iter().rev() {
                    let Some(payload_ty) = &variant.payload else {
                        continue;
                    };
                    let (lo, hi) = variant.payload_range(tag_width);
                    let lhs_part = self.slice_value(lhs, lo, hi, span)?;
                    let rhs_part = self.slice_value(rhs, lo, hi, span)?;
                    let part_equal = self.typed_equality(&lhs_part, &rhs_part, payload_ty, span)?;
                    let tag_is = equal_parts(
                        lhs_tag.clone(),
                        self.unsigned_constant(variant.tag, tag_width, span),
                    );
                    let selected = boolean(ExprKind::Ternary {
                        cond: Box::new(tag_is),
                        then_expr: Box::new(part_equal),
                        else_expr: Box::new(
                            payload_equal.unwrap_or_else(|| self.true_constant(span)),
                        ),
                    });
                    payload_equal = Some(selected);
                }
                let payload = payload_equal.unwrap_or_else(|| self.true_constant(span));
                Ok(boolean(ExprKind::Binary {
                    op: BinOp::LogAnd,
                    lhs: Box::new(tags_equal),
                    rhs: Box::new(payload),
                }))
            }
            Ty::Struct(strukt) => {
                let mut result: Option<Expr> = None;
                for field in &strukt.fields {
                    let (lo, hi) = (field.offset, field.offset + field.width);
                    let lhs_part = self.slice_value(lhs, lo, hi, span)?;
                    let rhs_part = self.slice_value(rhs, lo, hi, span)?;
                    let equal = self.typed_equality(&lhs_part, &rhs_part, &field.ty, span)?;
                    result = Some(match result {
                        Some(previous) => boolean(ExprKind::Binary {
                            op: BinOp::LogAnd,
                            lhs: Box::new(previous),
                            rhs: Box::new(equal),
                        }),
                        None => equal,
                    });
                }
                Ok(result.unwrap_or_else(|| self.true_constant(span)))
            }
            Ty::Array { element, len } => {
                let width = element.width();
                let mut result: Option<Expr> = None;
                for index in 0..*len {
                    let (lo, hi) = (index * width, (index + 1) * width);
                    let lhs_part = self.slice_value(lhs, lo, hi, span)?;
                    let rhs_part = self.slice_value(rhs, lo, hi, span)?;
                    let equal = self.typed_equality(&lhs_part, &rhs_part, element, span)?;
                    result = Some(match result {
                        Some(previous) => boolean(ExprKind::Binary {
                            op: BinOp::LogAnd,
                            lhs: Box::new(previous),
                            rhs: Box::new(equal),
                        }),
                        None => equal,
                    });
                }
                Ok(result.unwrap_or_else(|| self.true_constant(span)))
            }
            _ => Ok(equal_parts(lhs.clone(), rhs.clone())),
        }
    }

    /// Bits `[lo, hi)` of a value, naming it first when it is not a signal.
    fn slice_value(
        &mut self,
        value: &Expr,
        lo: u32,
        hi: u32,
        span: &SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        // One bit is a `bool`, whatever the value it was cut out of.
        let ty = if hi - lo == 1 {
            Ty::Bool
        } else {
            Ty::UInt(hi - lo)
        };
        match self.as_signal(value) {
            Some((signal, base_range)) => {
                let base = base_range.map(|(lo, _)| lo).unwrap_or(0);
                Ok(self.range_expr(signal, Some((base + lo, base + hi)), &ty, span.clone()))
            }
            None => {
                let signal = self.materialize(value.clone(), span)?;
                Ok(self.range_expr(signal, Some((lo, hi)), &ty, span.clone()))
            }
        }
    }

    /// The constant `value`, of an unsigned width, for a comparison.
    fn unsigned_constant(&self, value: u64, width: u32, span: &SourceSpan) -> Expr {
        Expr {
            ty: Ty::UInt(width),
            kind: ExprKind::Const {
                value: i128::from(value),
                width,
            },
            source: span.clone(),
        }
    }

    /// The constant `true`.
    fn true_constant(&self, span: &SourceSpan) -> Expr {
        Expr {
            ty: Ty::Bool,
            kind: ExprKind::Const { value: 1, width: 1 },
            source: span.clone(),
        }
    }

    /// Copy `value` into a temporary when `later` may run statements.
    ///
    /// Rust evaluates subexpressions in order, so a statement a later sibling
    /// runs cannot change what an earlier one already read. The copy is taken
    /// before the later sibling is lowered, which keeps both the value and the
    /// dataflow the original read saw.
    fn snapshot_before(
        &mut self,
        value: Expr,
        later: &syn::Expr,
        span: &SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if value.ty.width() == 0 || self.proc.is_none() || !may_emit_statements(later) {
            return Ok(value);
        }
        let ty = value.ty.clone();
        let signal = self.materialize(value, span)?;
        Ok(self.range_expr(signal, None, &ty, span.clone()))
    }

    /// `a && b` / `a || b` with a right operand that runs statements.
    ///
    /// Rust short-circuits: the right operand only runs when the left one does
    /// not decide the result. The statements the right operand needs are emitted
    /// inside an `if` on a snapshot of the left value, so neither the value nor
    /// the effects of the side that was skipped happen.
    fn lower_lazy_logical(
        &mut self,
        op: BinOp,
        left: Expr,
        right: &syn::Expr,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        if left.ty != Ty::Bool {
            return Err(Diagnostic::error(
                codes::WIDTH_MISMATCH,
                Some(span),
                format!("`{}` needs two `bool` operands", op.verilog()),
            ));
        }
        // The left value decides the result, so it is a snapshot: the right
        // operand's statements must not change what it read.
        let left_ty = left.ty.clone();
        let left_signal = self.materialize(left, &span)?;
        let left_read = self.range_expr(left_signal, None, &left_ty, span.clone());

        let guard = self.proc_ref().guard_reads.clone();
        let mut right_guard = guard.clone();
        right_guard.extend(self.sources_of(&left_read));
        self.proc.as_mut().expect("process context").guard_reads = right_guard;

        let mark = self.pending_mark();
        let right_value = self.lower_expr(right, Some(&Ty::Bool))?;
        let mut right_body = self.take_pending_since(mark);
        if right_value.ty != Ty::Bool {
            return Err(Diagnostic::error(
                codes::WIDTH_MISMATCH,
                Some(span),
                format!("`{}` needs two `bool` operands", op.verilog()),
            ));
        }

        // One temporary holds the result: the right side writes it in the branch
        // that runs it, and the other branch writes what short-circuiting
        // decides.
        let result = self.declare_process_temp(Ty::Bool, &span)?;
        let target = LValue::Signal {
            signal: result,
            range: None,
            source: span.clone(),
        };
        let decides = Expr {
            ty: Ty::Bool,
            kind: ExprKind::Const {
                value: if matches!(op, BinOp::LogOr) { 1 } else { 0 },
                width: 1,
            },
            source: span.clone(),
        };
        let before_provenance = self.proc_ref().provenance.clone();
        self.record_process_assign(&target, &right_value);
        right_body.push(Stmt::Assign {
            target: target.clone(),
            value: right_value,
            nonblocking: false,
            source: span.clone(),
        });
        let right_provenance = self.proc_ref().provenance.clone();
        self.proc.as_mut().expect("process context").provenance = before_provenance;
        self.record_process_assign(&target, &decides);
        let decides_provenance = self.proc_ref().provenance.clone();
        self.proc.as_mut().expect("process context").provenance =
            Provenance::merge(&[right_provenance, decides_provenance]);
        self.proc.as_mut().expect("process context").guard_reads = guard;

        let decides_statement = Stmt::Assign {
            target,
            value: decides,
            nonblocking: false,
            source: span.clone(),
        };
        let (then_body, else_body) = match op {
            BinOp::LogOr => (vec![decides_statement], right_body),
            _ => (right_body, vec![decides_statement]),
        };
        self.proc
            .as_mut()
            .expect("process context")
            .pending
            .push(Stmt::If {
                cond: left_read,
                then_body,
                else_body,
                source: span.clone(),
            });
        Ok(self.range_expr(result, None, &Ty::Bool, span))
    }

    /// An `if` value whose branches run statements.
    ///
    /// The result goes to one temporary that each side assigns from inside its
    /// own branch: only the branch that was selected runs, and the value it
    /// produced is what the `if` evaluates to.
    fn lower_selected_if(
        &mut self,
        condition: Expr,
        then_branch: (Vec<Stmt>, Expr),
        else_branch: (Vec<Stmt>, Expr),
        ty: Ty,
        span: SourceSpan,
    ) -> Result<Expr, Diagnostic> {
        let (then_body, then_value) = then_branch;
        let (else_body, else_value) = else_branch;
        let result = self.declare_process_temp(ty.clone(), &span)?;
        let target = LValue::Signal {
            signal: result,
            range: None,
            source: span.clone(),
        };
        let guard = self.proc_ref().guard_reads.clone();
        let mut branch_guard = guard.clone();
        branch_guard.extend(self.sources_of(&condition));
        self.proc.as_mut().expect("process context").guard_reads = branch_guard;

        let before_provenance = self.proc_ref().provenance.clone();
        let mut sides = Vec::with_capacity(2);
        for (mut body, value) in [(then_body, then_value), (else_body, else_value)] {
            self.record_process_assign(&target, &value);
            body.push(Stmt::Assign {
                target: target.clone(),
                value,
                nonblocking: false,
                source: span.clone(),
            });
            sides.push((body, self.proc_ref().provenance.clone()));
            self.proc.as_mut().expect("process context").provenance = before_provenance.clone();
        }
        let mut sides = sides.into_iter();
        let (then_body, then_provenance) = sides.next().expect("two branches");
        let (else_body, else_provenance) = sides.next().expect("two branches");
        self.proc.as_mut().expect("process context").provenance =
            Provenance::merge(&[then_provenance, else_provenance]);
        self.proc.as_mut().expect("process context").guard_reads = guard;
        self.proc
            .as_mut()
            .expect("process context")
            .pending
            .push(Stmt::If {
                cond: condition,
                then_body,
                else_body,
                source: span.clone(),
            });
        Ok(self.range_expr(result, None, &ty, span))
    }

    /// A `match` used as a value: the selected arm's value goes to a temporary.
    ///
    /// The arms are the shared `match` lowering ([`Compiler::lower_match_core`]);
    /// the difference is that every arm assigns the result temporary instead of
    /// running statements. The statements the chain needs go to `pending`, exactly
    /// like any other nested temporary, so the statement that reads the value
    /// emits them first.
    ///
    /// An assignment or a `let` whose whole right-hand side is a `match` does not
    /// come here: the caller names the target and every arm writes it directly.
    fn lower_match_expression(
        &mut self,
        item: &syn::ExprMatch,
        expression: &syn::Expr,
        expected: Option<&Ty>,
    ) -> Result<Expr, Diagnostic> {
        let span = self
            .span(expression.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        if self.proc.is_none() {
            return Err(Diagnostic::error(
                codes::NOT_IMPLEMENTED,
                Some(span),
                "a `match` used as a value needs a process: write it in `run` or inside `always!`",
            )
            .with_note(
                "a structural connection runs no statements; bind the value to a local in `run` first",
                None,
            ));
        }
        // The arms are lowered against the context the call site gives, or else
        // against the first arm that names a type without needing one. A literal
        // is never defaulted to a Rust integer type: the width of a value is
        // always the program's (REQ-LANG-030).
        let expected = expected
            .cloned()
            .or_else(|| self.match_context_ty(&item.arms));
        let mut state = MatchValue {
            expected,
            result: None,
            ty: None,
            into: None,
        };
        let mut chain = Vec::new();
        // The core reports the way a statement does, by pushing and returning
        // `()`. Run it against a fresh channel: its diagnostics are then returned
        // once instead of being reported here and pushed again by the caller.
        let mut reported = crate::frontend::Diagnostics::new();
        mem::swap(&mut self.diagnostics, &mut reported);
        let outcome = self.lower_match_core(item, expression, &mut chain, Some(&mut state));
        mem::swap(&mut self.diagnostics, &mut reported);
        let mut reported = reported.into_vec().into_iter();
        match outcome {
            Ok(()) => {
                for diagnostic in reported {
                    self.diagnostics.push(diagnostic);
                }
                self.proc
                    .as_mut()
                    .expect("process context")
                    .pending
                    .extend(chain);
                let (Some(ty), Some(signal)) = (state.ty, state.result) else {
                    return Err(Diagnostic::error(
                        codes::UNSUPPORTED,
                        Some(span),
                        "this `match` produces no value",
                    ));
                };
                Ok(self.range_expr(signal, None, &ty, span))
            }
            Err(()) => {
                // The core already reported. The first diagnostic is the one the
                // caller pushes, so it keeps its span; the rest are kept in their
                // original order.
                let first = reported.next();
                for diagnostic in reported {
                    self.diagnostics.push(diagnostic);
                }
                Err(first.unwrap_or_else(|| {
                    Diagnostic::error(
                        codes::UNSUPPORTED,
                        Some(span),
                        "this `match` value could not be lowered",
                    )
                }))
            }
        }
    }

    /// The type the arms of a `match` value are lowered against when the call
    /// site gives no context.
    ///
    /// The first arm normally fixes the type, but an arm that is only a literal
    /// needs a type before it can be lowered. The arms are scanned first for one
    /// that names a type on its own, so `match s { A => 1, B => 2u8 }` is a
    /// `uint<8>` instead of an error or an implicit Rust integer width.
    fn match_context_ty(&self, arms: &[syn::Arm]) -> Option<Ty> {
        arms.iter().find_map(|arm| self.named_ty(&arm.body))
    }

    /// The type `expr` names without needing a context: a suffixed literal, a
    /// `bool` literal, or a signal read. Anything else has its type decided by
    /// being lowered.
    fn named_ty(&self, expr: &syn::Expr) -> Option<Ty> {
        match expr {
            syn::Expr::Paren(paren) => self.named_ty(&paren.expr),
            syn::Expr::Group(group) => self.named_ty(&group.expr),
            syn::Expr::Block(block) => match block.block.stmts.last() {
                Some(syn::Stmt::Expr(value, None)) => self.named_ty(value),
                _ => None,
            },
            syn::Expr::Lit(literal) => match &literal.lit {
                syn::Lit::Bool(_) => Some(Ty::Bool),
                syn::Lit::Int(int) => suffix_type(int.suffix(), &self.cur.struct_source)
                    .ok()
                    .flatten(),
                _ => None,
            },
            syn::Expr::Path(path) => self.named_signal_ty(&path.path),
            syn::Expr::Field(field) => match (&*field.base, &field.member) {
                (syn::Expr::Path(base), syn::Member::Named(member))
                    if base.path.is_ident("self") =>
                {
                    self.module_signal_ty(&member.to_string())
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn named_signal_ty(&self, path: &syn::Path) -> Option<Ty> {
        if path.segments.len() != 1 {
            return None;
        }
        let name = path.segments.first()?.ident.to_string();
        match self
            .proc
            .as_ref()
            .and_then(|context| context.locals.get(&name).copied())
        {
            Some(signal) => Some(self.cur.signal(signal).ty.clone()),
            None => self.module_signal_ty(&name),
        }
    }

    fn module_signal_ty(&self, name: &str) -> Option<Ty> {
        let signal = self.cur.signal_by_name.get(name).copied()?;
        Some(self.cur.signal(signal).ty.clone())
    }

    /// The name of one compiler temporary.
    ///
    /// It carries the source position *and* a stable ordinal: a static `for` can
    /// materialise several different values at the same position, and each one
    /// needs its own signal (REQ-VER-009).
    /// Name of a compiler temporary.
    ///
    /// A temporary that stands for something the source names is called after it:
    /// the snapshot of a `match` scrutinee over `sig` is `sig_reg`, and a second
    /// one is `sig_reg2`. Only a temporary with no such name falls back to its
    /// source position (`t_l12_c9`), which is stable, unique and cannot collide
    /// with a signal the program declares.
    fn temp_name_hinted(
        &mut self,
        hint: Option<&str>,
        span: &SourceSpan,
    ) -> Result<String, Diagnostic> {
        if let Some(hint) = hint {
            let mut name = format!("{hint}_reg");
            let mut suffix = 2;
            while self.cur.signal_by_name.contains_key(&name)
                || self
                    .proc
                    .as_ref()
                    .is_some_and(|proc| proc.local_names.contains(&name))
            {
                name = format!("{hint}_reg{suffix}");
                suffix += 1;
            }
            return Ok(name);
        }
        self.cur.temp_count += 1;
        let name = format!("t_l{}_c{}_{}", span.line, span.column, self.cur.temp_count);
        if self.cur.signal_by_name.contains_key(&name) {
            return Err(Diagnostic::error(
                codes::NAME_COLLISION,
                Some(span.clone()),
                format!("the compiler temporary `{name}` collides with a signal of this module; rename the signal"),
            ));
        }
        Ok(name)
    }

    /// Declare a process-local temporary of `ty`.
    ///
    /// The caller records the assignment, so a value can be written from inside
    /// the branch that selects it instead of being appended to `pending`.
    pub(super) fn declare_process_temp(
        &mut self,
        ty: Ty,
        span: &SourceSpan,
    ) -> Result<SignalId, Diagnostic> {
        self.declare_process_temp_hinted(ty, None, span)
    }

    pub(super) fn declare_process_temp_hinted(
        &mut self,
        ty: Ty,
        hint: Option<&str>,
        span: &SourceSpan,
    ) -> Result<SignalId, Diagnostic> {
        let name = self.temp_name_hinted(hint, span)?;
        let process = self.current_process();
        let id = self.cur.signals.len();
        self.cur.signals.push(Signal {
            name: name.clone(),
            source_name: None,
            ty,
            kind: SignalKind::Reg,
            scope: SignalScope::Process(process),
            source: Some(span.clone()),
        });
        let context = self.proc.as_mut().expect("inside a process");
        context.local_names.insert(name);
        context.local_list.push(id);
        Ok(id)
    }

    pub(super) fn materialize(
        &mut self,
        value: Expr,
        span: &SourceSpan,
    ) -> Result<SignalId, Diagnostic> {
        self.materialize_hinted(value, None, span)
    }

    /// Give `value` a signal of its own, named after `hint` when it names one.
    pub(super) fn materialize_hinted(
        &mut self,
        value: Expr,
        hint: Option<&str>,
        span: &SourceSpan,
    ) -> Result<SignalId, Diagnostic> {
        let ty = value.ty.clone();
        let signal = if self.proc.is_some() {
            let id = self.declare_process_temp_hinted(ty, hint, span)?;
            let target = LValue::Signal {
                signal: id,
                range: None,
                source: span.clone(),
            };
            self.record_process_assign(&target, &value);
            self.proc
                .as_mut()
                .expect("process context")
                .pending
                .push(Stmt::Assign {
                    target,
                    value,
                    nonblocking: false,
                    source: span.clone(),
                });
            id
        } else {
            let name = self.temp_name_hinted(hint, span)?;
            let id = self.cur.signals.len();
            self.cur.signals.push(Signal {
                name: name.clone(),
                source_name: None,
                ty,
                kind: SignalKind::Wire,
                scope: SignalScope::Module,
                source: Some(span.clone()),
            });
            self.cur.signal_by_name.insert(name, id);
            let target = LValue::Signal {
                signal: id,
                range: None,
                source: span.clone(),
            };
            self.claim(id, None, Owner::Wiring, span)?;
            self.cur.body.push(BodyItem::Assign {
                target,
                value,
                source: span.clone(),
            });
            id
        };
        Ok(signal)
    }
}

/// The type an integer literal's suffix names, when it names one.
///
/// `0xffu8` is a `uint<8>`: the suffix is the spelling that gives a literal a
/// type of its own, and therefore the way to write a bit pattern that will be
/// reinterpreted. A *numeric* suffix whose width is impossible is reported -
/// `1u0` names no width, and silently letting the context decide would accept an
/// explicitly invalid one - while suffixes that name no hardware width at all
/// (`usize`, `i8`, ...) stay ignored exactly as they always were.
fn suffix_type(suffix: &str, span: &SourceSpan) -> Result<Option<Ty>, Diagnostic> {
    let Some(digits) = suffix.strip_prefix('u') else {
        return Ok(None);
    };
    let Ok(width) = digits.parse::<u64>() else {
        return Ok(None);
    };
    match crate::frontend::consteval::check_width(width) {
        Ok(width) => Ok(Some(Ty::UInt(width))),
        Err(message) => Err(Diagnostic::error(
            codes::BAD_TYPE,
            Some(span.clone()),
            message,
        )),
    }
}

/// A name in a `::from` call that is not a hardware type.
fn not_a_hardware_type(type_name: &str, span: &SourceSpan) -> Diagnostic {
    Diagnostic::error(
        codes::BAD_TYPE,
        Some(span.clone()),
        format!("`{type_name}` is not a hardware type; use `uint::<W>`, `int::<W>` or `bool`"),
    )
}

/// The build-time value of a `::from(...)` argument of a signed type.
///
/// A negation is part of the value here, the way `-1` is part of a negative
/// literal; `consteval::eval` itself stays unsigned, because the widths it
/// evaluates are.
fn signed_const_value(expr: &syn::Expr, env: &consteval::ConstEnv) -> Result<i128, String> {
    match expr {
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            Ok(-(consteval::eval(&unary.expr, env)? as i128))
        }
        other => Ok(consteval::eval(other, env)? as i128),
    }
}

/// A constant of a known type, with that type's range check.
///
/// The value is stored as the value itself, so a negative one stays negative:
/// Verilog has no `8'sd-1`, and `-8'sd1` is what a negative constant has to be
/// written as.
fn typed_constant(value: i128, ty: Ty, span: SourceSpan) -> Result<Expr, Diagnostic> {
    let fits = match &ty {
        Ty::UInt(width) => fits_unsigned(value, *width),
        Ty::SInt(width) => fits_signed(value, *width),
        Ty::Bool => (0..=1).contains(&value),
        // An aggregate is never the type of an integer constant; the callers
        // report that before reaching here.
        Ty::Struct(_) | Ty::Array { .. } | Ty::Enum(_) => false,
    };
    if !fits {
        let mut diagnostic = Diagnostic::error(
            codes::OUT_OF_RANGE,
            Some(span.clone()),
            format!("{value} does not fit in {}", ty.describe()),
        );
        if value < 0 && matches!(ty, Ty::UInt(_)) {
            diagnostic = diagnostic.with_note(
                format!(
                    "an unsigned value cannot be negative; `int<{}>` is the signed type",
                    ty.width()
                ),
                None,
            );
        }
        if value > 0 && ty.is_signed() {
            diagnostic = diagnostic.with_note(
                "a positive literal outside the signed range is not a bit pattern; \
                 write the pattern with its own type and reinterpret it, for example `0xffu8 as int<8>`",
                None,
            );
        }
        return Err(diagnostic);
    }
    Ok(Expr {
        ty: ty.clone(),
        kind: ExprKind::Const {
            value,
            width: ty.width(),
        },
        source: span,
    })
}

/// Does `value` fit in an unsigned `width`-bit value?
///
/// The comparison is done in `u128`: `1i128 << 127` is negative, so shifting an
/// `i128` by a width would refuse `uint<127>` values that fit perfectly well.
fn fits_unsigned(value: i128, width: u32) -> bool {
    value >= 0 && (value as u128) <= low_mask(width)
}

/// The low `width` bits set: `2^width - 1`, saturated at `u128::MAX`.
fn low_mask(width: u32) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

/// Does `value` fit in a two's-complement `width`-bit value?
fn fits_signed(value: i128, width: u32) -> bool {
    if width >= 128 {
        return true;
    }
    let bound = 1i128 << (width - 1);
    value >= -bound && value < bound
}

/// The integer literal behind an expression, looking through parentheses.
fn integer_literal(expr: &syn::Expr) -> Option<&syn::LitInt> {
    match expr {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(int) => Some(int),
            _ => None,
        },
        syn::Expr::Paren(paren) => integer_literal(&paren.expr),
        syn::Expr::Group(group) => integer_literal(&group.expr),
        _ => None,
    }
}

/// Re-read the bit pattern of a constant as `target`, when the reading fits an
/// `i128`.
///
/// The bits do not change, the value read from them does: `0xffu8 as int<8>` is
/// -1. The pattern is kept in a `u128` mask, so no `i128` shift overflows at 127
/// bits. `None` means the reading needs a pattern no `i128` holds - a negative
/// value read as unsigned at 128 bits or more - which the caller keeps as bits
/// instead of as a number.
fn reinterpreted_value(value: i128, width: u32, target: &Ty) -> Option<i128> {
    if width <= 128 {
        let mask = low_mask(width);
        let bits = (value as u128) & mask;
        if target.is_signed() {
            // Sign-extend from bit `width - 1` into the full `i128`.
            let sign = 1u128 << (width - 1);
            return Some(if bits & sign != 0 {
                (bits | !mask) as i128
            } else {
                bits as i128
            });
        }
        if bits <= i128::MAX as u128 {
            return Some(bits as i128);
        }
    } else if value >= 0 || target.is_signed() {
        // Above 128 bits the sign bit cannot be set by a source literal, so both
        // readings already agree.
        return Some(value);
    }
    None
}

/// The root of a type name: `Option<uint<4>>` and `Option` both root at
/// `Option`, which is what a qualified path writes.
fn type_root(name: &str) -> &str {
    match name.split_once('<') {
        Some((root, _)) => root.trim_end(),
        None => name,
    }
}

/// How a variant's payload is declared.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PayloadForm {
    /// One value whose type is the payload itself: `V(T)`.
    Single,
    /// Several positional values: `V(T, U)`.
    Tuple,
    /// Named fields: `V { a: T, b: U }`.
    Named,
}

/// How this payload was declared.
///
/// The layout keeps the forms apart by itself: several positional values and
/// named fields both become a struct named `Enum::Variant`, with numbered fields
/// (`0`, `1`, ...) for the tuple form and the written names for the named one.
/// Anything else - a plain struct such as `Packet` - is one value of its own
/// type, however many fields it has.
fn payload_form(payload: &Ty, enum_name: &str, variant_name: &str) -> PayloadForm {
    let Ty::Struct(strukt) = payload else {
        return PayloadForm::Single;
    };
    if strukt.name != format!("{enum_name}::{variant_name}") {
        return PayloadForm::Single;
    }
    let numbered = strukt
        .fields
        .iter()
        .enumerate()
        .all(|(index, field)| field.name == index.to_string());
    if numbered {
        PayloadForm::Tuple
    } else {
        PayloadForm::Named
    }
}

/// The payload spelling of a variant construction.
enum VariantArgs<'a> {
    /// `Variant`
    Unit,
    /// `Variant(a, b, ...)`
    Positional(&'a syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>),
    /// `Variant { field: value, ... }`
    Named(&'a syn::punctuated::Punctuated<syn::FieldValue, syn::Token![,]>),
}

/// True when lowering `expr` may run statements.
///
/// A `match` used as a value computes its value with statements, one per arm, and
/// it is the only expression that can; every other construct only creates
/// temporaries. A construct that lowers one subexpression before another asks
/// this so it can copy the earlier value first: a statement in the later one must
/// not change what the earlier one read.
fn may_emit_statements(expr: &syn::Expr) -> bool {
    struct Finder {
        found: bool,
    }
    impl syn::visit::Visit<'_> for Finder {
        fn visit_expr_match(&mut self, _: &syn::ExprMatch) {
            self.found = true;
        }
    }
    let mut finder = Finder { found: false };
    syn::visit::Visit::visit_expr(&mut finder, expr);
    finder.found
}
