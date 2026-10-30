//! Expression lowering: ShyHDL test expressions to Verilog-2005 expressions.
//!
//! Every expression is lowered with the width and the kind it is used at,
//! because the generated Verilog must not silently change meaning:
//!
//! * a value that does not fit its context is a source-located error instead of
//!   a quietly truncated comparison;
//! * `!` follows Rust: `!x` on a `bool` is logical negation and produces one
//!   bit, while `!x` on a `uint<N>` is the bitwise complement of all N bits;
//! * arithmetic, shifts and comparisons need integer operands, conditions need
//!   a `bool`, so `assert!(x)` with an 8-bit `x` is rejected instead of being
//!   read as "non zero";
//! * signedness is carried once, by [`Kind::Int`]: a `int<N>` becomes a Verilog
//!   `signed` signal, a negative literal is written `-N'sd<v>` after a two's
//!   complement range check, `-x` wraps at the operand's own width, `int >> n`
//!   emits `>>>` (Verilog's `>>` is logical even for a signed value), and the six
//!   comparisons need two operands of the same width and signedness;
//! * a Verilog part-select loses `signed`, so a *typed* signed struct field is
//!   read back through `$signed(...)`, while a bare slice stays an unsigned bit
//!   vector;
//! * an assignment may widen a value (Verilog extends it) but never truncates it,
//!   `x[lo..hi]` is always a raw bit range of the receiver's packed bits (bounds
//!   in bits, result unsigned), while `x.slice(lo, hi)` is an array sub-array
//!   (bounds in elements) and is refused on a non-array;
//!   and signedness is never converted implicitly: `as int<W>` / `as uint<W>`
//!   with an equal width is the one explicit reinterpretation.
//!
//! Literals are emitted with an explicit width (`8'd42`, `8'sd42`, `-8'sd1`), so a
//! bare unsized literal never silently becomes 32 bits wide.

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{BinOp, Expr, Lit, UnOp};

use crate::error::Result;
use crate::metadata::{TypeField, ValueType};
use crate::packed;

use super::lower::{Generator, Name};
use super::PortDirection;

/// What drives a signal, which decides whether a test may assign to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Driver {
    /// TB local, or a DUT input the test drives.
    Test,
    /// DUT output: the DUT drives it, the test may only read it.
    DutOutput,
    /// Driven by the generated `clock!()` process.
    ClockNet,
    /// Driven by the generated `reset!()` process.
    ResetNet,
    /// A `for` loop variable.
    LoopVar,
}

impl Driver {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Driver::Test => "a testbench signal",
            Driver::DutOutput => "a DUT output, which the DUT drives",
            Driver::ClockNet => "a clock driven by a clock wrapper (`clock!` or `gen_clk`)",
            Driver::ResetNet => "a reset driven by `reset!()`",
            Driver::LoopVar => "a `for` loop variable",
        }
    }
}

/// The kind of value an expression has, which decides operator meaning.
///
/// Signedness lives here and only here: `Int { signed }` is the single place an
/// expression states it, so no parallel type flag can drift out of step with the
/// operators that read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A `bool`: one bit with boolean operators.
    Bool,
    /// A `uint<N>` (unsigned) or `int<N>` (two's complement) of the given width.
    Int {
        /// Whether the value is a two's complement signed integer.
        signed: bool,
    },
    /// A packed struct value: only whole-value copies are meaningful.
    Struct,
}

/// The width and signedness an expression is used at.
///
/// They always travel together: a literal adopts both from its context, and two
/// operands of an operator have to agree on both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WidthContext {
    /// Width in bits.
    pub width: u32,
    /// Whether the context is a two's complement signed value.
    pub signed: bool,
}

/// A resolved reference to bits of a net in the generated testbench.
#[derive(Debug, Clone)]
pub(super) struct ValueRef {
    /// Verilog text selecting the referenced bits, for example `dut_res[3:0]`.
    ///
    /// For a selection with dynamic layers this is the plain indexed part-select
    /// (`dut_a[(i) +: 4]`), which is also a legal assignment target. Reading it
    /// goes through [`operand_text`], which adds the in-range guard; writing goes
    /// through the guard separately so a ternery never ends up on the left of an
    /// assignment.
    pub text: String,
    /// Type of the selected value.
    pub ty: ValueType,
    /// Kind derived from the type.
    pub kind: Kind,
    /// Width of the selected value in bits.
    pub width: u32,
    /// What drives the net, used to reject writes to a driven signal.
    pub driver: Driver,
    /// In-range condition of the dynamic index layers, when the selection has any.
    ///
    /// Verilog would let an out-of-range index of a *slice* of an array read a
    /// neighbouring element; the guard keeps every layer inside its own element
    /// (read: the guarded ternary yields X, write: the assignment is skipped).
    pub guard: Option<String>,
}

/// A lowered expression plus the width and kind it has in Verilog.
#[derive(Debug, Clone)]
pub(super) struct Lowered {
    pub text: String,
    pub width: u32,
    pub kind: Kind,
    /// The value's own type when it has one (a signal, a slice, a cast).
    ///
    /// Signedness stays in `kind`; this is the *shape* authority, so an aggregate
    /// copy and `assert_eq!` can require exactly the same `ValueType` instead of a
    /// second type system.
    pub ty: Option<ValueType>,
    /// Kept while an integer literal is still context-free, so a surrounding
    /// context can size it instead of the other way round.
    literal: Option<Literal>,
}

#[derive(Debug, Clone, Copy)]
struct Literal {
    /// Magnitude of the literal.
    value: u128,
    /// Whether the literal was written negative (`-1`); the sign is emitted in
    /// front of the sized literal (`-8'sd1`), which is what Verilog accepts.
    negative: bool,
}

impl Lowered {
    fn scalar(text: impl Into<String>, width: u32, kind: Kind) -> Self {
        Self {
            text: text.into(),
            width,
            kind,
            ty: None,
            literal: None,
        }
    }

    /// A scalar with the type it came from, for shape checks.
    pub(super) fn typed(text: impl Into<String>, ty: ValueType, width: u32, kind: Kind) -> Self {
        Self {
            text: text.into(),
            width,
            kind,
            ty: Some(ty),
            literal: None,
        }
    }

    fn integer(magnitude: u128, negative: bool, width: u32, signed: bool) -> Self {
        Self {
            text: literal_text(magnitude, negative, width, signed),
            width,
            kind: Kind::Int { signed },
            ty: None,
            literal: Some(Literal {
                value: magnitude,
                negative,
            }),
        }
    }

    /// Re-emit this expression at the context's width and signedness, rejecting
    /// an implicit truncation, an implicit sign change, and a literal that is out
    /// of range for the context.
    pub(super) fn adapt(
        mut self,
        context: WidthContext,
        span: Span,
        generator: &Generator<'_>,
    ) -> Result<Self> {
        let WidthContext { width, signed } = context;
        match self.literal {
            Some(literal) => {
                check_literal_fits(literal, context, span, generator)?;
                self.text = literal_text(literal.value, literal.negative, width, signed);
                self.width = width;
                self.kind = Kind::Int { signed };
                self.literal = None;
                Ok(self)
            }
            None => {
                if signed {
                    // Type before width: a `bool` or a struct is never a signed
                    // value, whatever its width, so the diagnostic is deterministic.
                    return check_signed_exact(self, context, span, generator);
                }
                if self.width > width {
                    return Err(generator.error(
                        span,
                        format!(
                            "a {} bit value is used where {width} bits are expected; truncation \
                             must be explicit, for example `x[0..{width}]`",
                            self.width
                        ),
                    ));
                }
                if signed {
                    // Signed arithmetic wraps at its own width, so a signed value
                    // is never extended implicitly: Verilog would otherwise size
                    // the whole expression at the assignment's width and quietly
                    // produce a value the signed type says cannot happen.
                    return check_signed_exact(self, context, span, generator);
                }
                if let Kind::Int {
                    signed: value_signed,
                } = self.kind
                {
                    if value_signed != signed {
                        return Err(
                            generator.error(span, sign_mismatch_message(value_signed, signed))
                        );
                    }
                }
                Ok(self)
            }
        }
    }
}

/// Smallest number of bits that holds `value` (at least one).
fn minimal_bits(value: u128) -> u32 {
    let bits = 128 - value.leading_zeros();
    bits.max(1)
}

fn kind_of(ty: &ValueType) -> Kind {
    match ty {
        ValueType::Bool => Kind::Bool,
        ValueType::UInt { .. } => Kind::Int { signed: false },
        ValueType::SInt { .. } => Kind::Int { signed: true },
        _ => Kind::Struct,
    }
}

/// The signedness of an integer kind, or `None` for a non-integer.
fn int_sign(kind: Kind) -> Option<bool> {
    match kind {
        Kind::Int { signed } => Some(signed),
        _ => None,
    }
}

/// Text of an integer literal: `-8'sd1` for a negative signed value, `8'sd1` for
/// a signed value, `8'd1` for an unsigned one. Verilog wants the sign in front of
/// the sized literal, never inside it.
fn literal_text(magnitude: u128, negative: bool, width: u32, signed: bool) -> String {
    let sign = if negative { "-" } else { "" };
    if signed {
        format!("{sign}{width}'sd{magnitude}")
    } else {
        format!("{sign}{width}'d{magnitude}")
    }
}

/// A signed context takes only a signed integer of exactly its width.
///
/// `int<N>` arithmetic wraps at N bits, so extending a value implicitly would let
/// Verilog evaluate the expression at the assignment's width and produce a result
/// the signed type says cannot exist. Literals are not affected: they take the
/// context width and are range checked there.
pub(super) fn check_signed_exact(
    value: Lowered,
    context: WidthContext,
    span: Span,
    generator: &Generator<'_>,
) -> Result<Lowered> {
    match value.kind {
        Kind::Int { signed: true } => {
            if value.width != context.width {
                return Err(generator.error(
                    span,
                    format!(
                        "a signed value keeps its own width: this is {} bits where {} bits are \
                         expected, and a value is never extended or truncated implicitly because \
                         signed arithmetic wraps at its own width; compute at one width, write the \
                         wider value as a literal, or truncate with an explicit slice",
                        value.width, context.width
                    ),
                ));
            }
            Ok(value)
        }
        Kind::Int { signed: false } => {
            Err(generator.error(span, sign_mismatch_message(false, true)))
        }
        Kind::Bool => Err(generator.error(
            span,
            format!(
                "a signed {} bit value is expected, but this is a `bool` (one bit): a comparison, \
                 a logical operator and a `bool` literal are not integers, so they are never \
                 converted into a signed value; write the integer explicitly, for example with an \
                 `if` that assigns 1 or 0",
                context.width
            ),
        )),
        Kind::Struct => Err(generator.error(
            span,
            format!(
                "a signed {} bit value is expected, but this is a packed struct; select a field or \
                 use a value of the target's own width",
                context.width
            ),
        )),
    }
}

fn sign_mismatch_message(value_signed: bool, context_signed: bool) -> String {
    format!(
        "this is {} value where a {} one is expected; signedness is never converted implicitly, \
         use `as int<W>` or `as uint<W>` (same width) to reinterpret the bits",
        if value_signed {
            "a signed"
        } else {
            "an unsigned"
        },
        if context_signed { "signed" } else { "unsigned" },
    )
}

/// A literal has to fit the context it is used at: unsigned values in their full
/// range, signed values in two's complement, and a negative literal only where
/// the context is signed (there is no implicit conversion to a bit pattern).
fn check_literal_fits(
    literal: Literal,
    context: WidthContext,
    span: Span,
    generator: &Generator<'_>,
) -> Result<()> {
    let WidthContext { width, signed } = context;
    if width == 0 || width > 128 {
        return Err(generator.error(span, "a value must be between 1 and 128 bits wide"));
    }
    if packed::literal_fits(literal.negative, literal.value, width, signed) {
        return Ok(());
    }
    // The message names the range that refused the value.
    if signed {
        let bound = packed::signed_bound(width);
        if literal.negative {
            return Err(generator.error(
                span,
                format!(
                    "the value -{} does not fit a signed {width} bit value (smallest -{bound})",
                    literal.value
                ),
            ));
        }
        return Err(generator.error(
            span,
            format!(
                "the value {} does not fit a signed {width} bit value (largest {})",
                literal.value,
                bound - 1
            ),
        ));
    }
    if literal.negative {
        return Err(generator.error(
            span,
            format!(
                "a negative literal needs a signed target; use `int<{width}>` or `as \
                 int<{width}>` for a signed value, or write the bit pattern explicitly"
            ),
        ));
    }
    Err(generator.error(
        span,
        format!(
            "the value {} does not fit in {width} bits; widen the target or write a smaller \
             value",
            literal.value
        ),
    ))
}

impl<'a> Generator<'a> {
    /// Lower an expression used at an optional context width.
    ///
    /// `context` is the width the surrounding Verilog will evaluate this
    /// expression at (an assignment target, the other side of a comparison);
    /// literals adopt it so their emitted width matches what the test means.
    pub(super) fn lower_expr(
        &self,
        expr: &Expr,
        context: Option<WidthContext>,
        span: Span,
    ) -> Result<Lowered> {
        let lowered = match expr {
            Expr::Paren(paren) => self.lower_expr(&paren.expr, context, span),
            Expr::Group(group) => self.lower_expr(&group.expr, context, span),
            Expr::Lit(item) => self.lower_literal(&item.lit, context, span),
            Expr::Unary(unary) => self.lower_unary(unary, context, span),
            Expr::Binary(binary) => self.lower_binary(binary, context, span),
            Expr::Cast(cast) => self.lower_cast(cast, context, span),
            Expr::Path(_) | Expr::Field(_) | Expr::Index(_) | Expr::MethodCall(_) => {
                match self.try_value_ref(expr)? {
                    Some(value) => match context {
                        Some(context) => self.adapt_value_ref(value, context, span),
                        None => {
                            let width = value.width;
                            Ok(Lowered::typed(
                                operand_text(&value),
                                value.ty,
                                width,
                                value.kind,
                            ))
                        }
                    },
                    None => Err(self.error(
                        span,
                        format!(
                            "`{}` is not a signal of this testbench; only DUT ports (`dut.port`), \
                             `let` locals and loop variables can be read",
                            super::lower::render_expr(expr)
                        ),
                    )),
                }
            }
            Expr::Array(_) | Expr::Repeat(_) => Err(self.error(
                span,
                "an array literal is lowered where the target's shape is known: assign it to an \
                 array local or a DUT array port, or compare it through such a value",
            )),
            Expr::Call(call) => Err(self.error(
                call.span(),
                "function calls are not part of the supported testbench expressions; supported \
                 calls are `delay(..)`, `wait(..)`, `wait_until(..)`, `sample_after(..)` and the \
                 time constructors `ns(..)`/`ps(..)`",
            )),
            // A `match` is lowered into statements plus a hidden net by the mutable
            // lowering in `lower`, before this pure pass runs. Reaching this arm means
            // the construct sits somewhere that lowering does not extend, and a
            // ternary could not stand in: it would lose the arm guard, the pattern
            // bindings and the exhaustiveness proof.
            Expr::Match(item) => Err(self.error(
                item.span(),
                "this `match` value cannot be lowered here; a `match` used as a value \
                 becomes statements in the process, so it is lowered in an initialiser, an \
                 assignment, an assertion, a `match` arm value or an operator/argument around \
                 one. Write it into a local first",
            )),
            Expr::If(item) => Err(self.error(
                item.span(),
                "this `if` value cannot be lowered here; an `if` used as a value needs an \
                 `else` branch and is lowered where the process can run only the selected \
                 branch (an initialiser, an assignment, an assertion or a `match` arm value). \
                 Write it into a local first",
            )),
            other => Err(self.error(
                other.span(),
                format!(
                    "unsupported expression `{}` in a testbench; supported expressions are \
                     signals, integer and boolean literals, slices (\"x[lo..hi]\", \".get(i)\"), \
                     arithmetic and comparison operators",
                    super::lower::render_expr(other)
                ),
            )),
        }?;
        // The single exit for a context-carrying expression: a signed context only
        // ever takes a signed integer of exactly its width. Checking here rather
        // than in each operator means a producer that does not size itself (a
        // `bool` literal, a comparison, a logical operator) cannot slip a one bit
        // value into a `int<N>`. The unsigned path is unchanged.
        match context {
            Some(context) if context.signed => check_signed_exact(lowered, context, span, self),
            _ => Ok(lowered),
        }
    }

    /// Lower an expression that must be a boolean condition, such as the
    /// argument of `assert!` or `if`.
    pub(super) fn lower_condition(&self, expr: &Expr, what: &str) -> Result<Lowered> {
        let lowered = self.lower_expr(expr, None, expr.span())?;
        if lowered.kind != Kind::Bool {
            return Err(self.error(
                expr.span(),
                format!(
                    "{what} needs a `bool` condition, but `{}` is {} bits wide; write the \
                     comparison explicitly, for example `{} != 0`",
                    super::lower::render_expr(expr),
                    lowered.width,
                    super::lower::render_expr(expr)
                ),
            ));
        }
        Ok(lowered)
    }

    /// A value reference can only widen, so a non-literal context is checked for
    /// width (no implicit truncation) and signedness (no implicit sign change).
    fn adapt_value_ref(
        &self,
        value: ValueRef,
        context: WidthContext,
        span: Span,
    ) -> Result<Lowered> {
        let width = value.width;
        let lowered = Lowered::typed(operand_text(&value), value.ty.clone(), width, value.kind);
        if context.signed {
            // A signed target takes only a signed integer of exactly its width, so a
            // type problem (`bool`, packed struct) is reported before any width
            // problem; the unsigned path below keeps its documented widening rule.
            return check_signed_exact(lowered, context, span, self);
        }
        if value.width > context.width {
            return Err(self.error(
                span,
                format!(
                    "a {} bit value is used where {} bits are expected; truncation must be \
                     explicit, for example `x[0..{}]`",
                    value.width, context.width, context.width
                ),
            ));
        }
        if let Kind::Int { signed } = value.kind {
            if signed != context.signed {
                return Err(self.error(span, sign_mismatch_message(signed, context.signed)));
            }
        }
        Ok(lowered)
    }

    /// `x as int<W>` / `x as uint<W>`: an equal-width reinterpretation of the
    /// same bits. Widening, narrowing and `bool` targets are rejected; the
    /// sign-changing case is the explicit `$signed`/`$unsigned` the language does
    /// not express directly.
    fn lower_cast(
        &self,
        cast: &syn::ExprCast,
        context: Option<WidthContext>,
        span: Span,
    ) -> Result<Lowered> {
        let target = self.parse_local_type(&cast.ty)?;
        let (target_signed, target_width) = match target.bit_width().map(|width| (width, &target)) {
            Ok((width, ty)) => match ty {
                ValueType::UInt { .. } => (false, width),
                ValueType::SInt { .. } => (true, width),
                ValueType::Bool => return Err(self.error(
                    cast.ty.span(),
                    "a cast to `bool` is not supported; compare the value instead, for example \
                         `x != 0`",
                )),
                _ => {
                    return Err(self.error(
                        cast.ty.span(),
                        "a cast target must be `int<N>` or `uint<N>`",
                    ))
                }
            },
            Err(error) => return Err(error),
        };
        let inner = match unsuffixed_literal_operand(&cast.expr) {
            // A literal that names no type of its own takes the target's width and
            // signedness and is range checked there, so `1 as int<8>` is
            // `8'sd1` and `255 as int<8>` is refused instead of wrapping.
            Some(literal) => self.lower_expr(
                literal,
                Some(WidthContext {
                    width: target_width,
                    signed: target_signed,
                }),
                literal.span(),
            )?,
            // Anything else is a value with its own type: the cast reinterprets it
            // and never resizes it, so the source keeps its width and signedness.
            None => self.lower_expr(&cast.expr, None, cast.expr.span())?,
        };
        if inner.width != target_width {
            return Err(self.error(
                cast.span(),
                format!(
                    "a cast must keep the width: `{}` is {} bits and the target is {target_width}; \
                     use a slice to narrow or an explicit wider value instead",
                    super::lower::render_expr(&cast.expr),
                    inner.width
                ),
            ));
        }
        let inner_signed = int_sign(inner.kind).ok_or_else(|| {
            self.error(
                cast.expr.span(),
                "only an integer value can be reinterpreted with `as int<N>` or `as uint<N>`",
            )
        })?;
        let text = match (inner_signed, target_signed) {
            (false, true) => format!("$signed({})", inner.text),
            (true, false) => format!("$unsigned({})", inner.text),
            // Same width and same sign: nothing to reinterpret.
            _ => inner.text,
        };
        let lowered = Lowered::scalar(
            text,
            target_width,
            Kind::Int {
                signed: target_signed,
            },
        );
        match context {
            Some(context) => lowered.adapt(context, span, self),
            None => Ok(lowered),
        }
    }

    fn lower_literal(
        &self,
        lit: &Lit,
        context: Option<WidthContext>,
        span: Span,
    ) -> Result<Lowered> {
        match lit {
            Lit::Bool(value) => Ok(Lowered::scalar(
                if value.value { "1'b1" } else { "1'b0" },
                1,
                Kind::Bool,
            )),
            Lit::Int(value) => {
                let number: u128 = value.base10_parse().map_err(|_| {
                    self.error(
                        span,
                        "integer literals wider than 128 bits are not supported",
                    )
                })?;
                let suffix_text = value.suffix();
                let explicit = match suffix_text {
                    "" => None,
                    suffix => Some(literal_suffix_width(suffix).ok_or_else(|| {
                        self.error(
                            span,
                            format!(
                                "unknown integer literal suffix `{suffix}`; use u1..u128 to give \
                                 the literal an explicit width"
                            ),
                        )
                    })?),
                };
                match unsigned_suffix_width(suffix_text) {
                    // `8u8` names its own type: a known
                    // `uint<8>`, not an unsized literal that can be re-fitted. It is
                    // range checked at its own width (so `256u8` is refused) and then
                    // adapted as a typed value, which keeps the documented unsigned
                    // widening while refusing a narrowing or a sign change.
                    Some(width) => {
                        if !packed::literal_fits(false, number, width, false) {
                            return Err(self.error(
                                span,
                                format!(
                                    "the value {number} does not fit its declared width of {width} \
                                     bits"
                                ),
                            ));
                        }
                        let typed = Lowered::scalar(
                            literal_text(number, false, width, false),
                            width,
                            Kind::Int { signed: false },
                        );
                        match context {
                            Some(context) => typed.adapt(context, span, self),
                            None => Ok(typed),
                        }
                    }
                    // `iN` (legacy) and an unsized literal: the suffix, when the
                    // source wrote one, is the literal's own width and the context
                    // still fits it, exactly as before this fix.
                    None => match context {
                        Some(context) => {
                            let width = explicit.unwrap_or(context.width);
                            Lowered::integer(number, false, width, context.signed)
                                .adapt(context, span, self)
                        }
                        None => {
                            let width = explicit.unwrap_or_else(|| minimal_bits(number));
                            let lowered = Lowered::integer(number, false, width, false);
                            if !packed::literal_fits(false, number, width, false) {
                                return Err(self.error(
                                    span,
                                    format!(
                                        "the value {number} does not fit its declared width of \
                                         {width} bits"
                                    ),
                                ));
                            }
                            Ok(lowered)
                        }
                    },
                }
            }
            other => Err(self.error(
                other.span(),
                "only integer and boolean literals are supported in a testbench",
            )),
        }
    }

    /// Unary operators with Rust meaning: `!bool` is logical negation, `!int` is
    /// the bitwise complement, and `-` wraps a signed value at its own width.
    fn lower_unary(
        &self,
        unary: &syn::ExprUnary,
        context: Option<WidthContext>,
        span: Span,
    ) -> Result<Lowered> {
        match unary.op {
            UnOp::Not(_) => {
                let operand = self.lower_expr(&unary.expr, context, span)?;
                match operand.kind {
                    Kind::Bool => Ok(Lowered::scalar(
                        format!("(!({}))", operand.text),
                        1,
                        Kind::Bool,
                    )),
                    Kind::Int { signed } => {
                        let width = operand.width;
                        Ok(Lowered::scalar(
                            format!("(~({}))", operand.text),
                            width,
                            Kind::Int { signed },
                        ))
                    }
                    Kind::Struct => Err(self.error(
                        unary.span(),
                        "`!` needs a `bool` or an integer value, not a packed struct",
                    )),
                }
            }
            UnOp::Neg(_) => self.lower_negation(&unary.expr, unary.span(), context, span),
            ref other => Err(self.error(
                unary.span(),
                format!(
                    "unsupported unary operator in a testbench ({}); `!` is supported",
                    render_unop(other)
                ),
            )),
        }
    }

    fn lower_binary(
        &self,
        binary: &syn::ExprBinary,
        context: Option<WidthContext>,
        span: Span,
    ) -> Result<Lowered> {
        let (text, kind) = binary_operator(&binary.op).ok_or_else(|| {
            self.error(
                binary.span(),
                "unsupported operator in a testbench; supported operators are + - * / % & | ^ \
                 << >> == != < <= > >= && ||",
            )
        })?;
        match kind {
            OpKind::Arithmetic => {
                let left = self.lower_expr(&binary.left, context, span)?;
                let right = self.lower_expr(&binary.right, context, span)?;
                // Two `bool` operands are only defined for the bitwise operators.
                if left.kind == Kind::Bool && right.kind == Kind::Bool {
                    if !matches!(text, "&" | "|" | "^") {
                        return Err(self.error(
                            binary.span(),
                            format!(
                                "`{text}` needs two integer operands; boolean arithmetic is not \
                                 defined by ShyHDL"
                            ),
                        ));
                    }
                    return Ok(Lowered::scalar(
                        format!("(({}) {} ({}))", left.text, text, right.text),
                        1,
                        Kind::Bool,
                    ));
                }
                let (left, right, width, signed) =
                    self.align_operands(left, right, binary.span(), text)?;
                Ok(Lowered::scalar(
                    format!("(({}) {} ({}))", left.text, text, right.text),
                    width,
                    Kind::Int { signed },
                ))
            }
            OpKind::Shift => {
                // The shift amount is self-determined in Verilog; only the left
                // operand takes the context width.
                let left = self.lower_expr(&binary.left, context, span)?;
                let right = self.lower_expr(&binary.right, None, span)?;
                let signed = int_sign(left.kind).ok_or_else(|| {
                    self.error(
                        binary.span(),
                        format!("`{text}` needs an integer value on the left"),
                    )
                })?;
                let count_signed = int_sign(right.kind).ok_or_else(|| {
                    self.error(
                        binary.span(),
                        format!("`{text}` needs an integer shift count"),
                    )
                })?;
                if count_signed {
                    return Err(self.error(
                        binary.right.span(),
                        "the shift count must be unsigned; reinterpret it with `as uint<W>` or \
                         write a non-negative literal",
                    ));
                }
                // A signed left operand needs Verilog's arithmetic shift: `>>` is
                // logical even for signed values, `>>>` fills with the sign bit.
                let operator = if text == ">>" && signed { ">>>" } else { text };
                let width = left.width;
                Ok(Lowered::scalar(
                    format!("(({}) {} ({}))", left.text, operator, right.text),
                    width,
                    Kind::Int { signed },
                ))
            }
            OpKind::Compare => {
                // `==` / `!=` on an enum-shaped value is built part by part, so a
                // nested inactive payload is ignored and a tag carrying `x`/`z`
                // never compares equal.
                if matches!(text, "==" | "!=") {
                    if let Some(condition) =
                        self.typed_comparison(&binary.left, &binary.right, binary.span())?
                    {
                        let condition = if text == "!=" {
                            format!("!({condition})")
                        } else {
                            condition
                        };
                        return Ok(Lowered::scalar(format!("({condition})"), 1, Kind::Bool));
                    }
                }
                let left = self.lower_expr(&binary.left, None, span)?;
                let right = self.lower_expr(&binary.right, None, span)?;
                if left.kind == Kind::Struct || right.kind == Kind::Struct {
                    return Err(self.error(
                        binary.span(),
                        format!(
                            "`{text}` cannot compare a packed struct value; select the field to \
                             compare"
                        ),
                    ));
                }
                if left.kind == Kind::Bool || right.kind == Kind::Bool {
                    if left.kind != right.kind {
                        return Err(self.error(
                            binary.span(),
                            format!(
                                "`{text}` compares a `bool` with an integer; compare values of the \
                                 same kind"
                            ),
                        ));
                    }
                    return Ok(Lowered::scalar(
                        format!("(({}) {} ({}))", left.text, text, right.text),
                        1,
                        Kind::Bool,
                    ));
                }
                let (left, right, _, _) = self.align_operands(left, right, binary.span(), text)?;
                Ok(Lowered::scalar(
                    format!("(({}) {} ({}))", left.text, text, right.text),
                    1,
                    Kind::Bool,
                ))
            }
            OpKind::Logical => {
                let left = self.lower_expr(&binary.left, None, span)?;
                let right = self.lower_expr(&binary.right, None, span)?;
                if left.kind != Kind::Bool || right.kind != Kind::Bool {
                    return Err(self.error(
                        binary.span(),
                        format!(
                            "`{text}` needs two `bool` operands; write the comparisons explicitly, \
                             for example `a != 0 {text} b != 0`"
                        ),
                    ));
                }
                Ok(Lowered::scalar(
                    format!("(({}) {} ({}))", left.text, text, right.text),
                    1,
                    Kind::Bool,
                ))
            }
        }
    }

    /// `-x`: a negative literal where the source wrote one, otherwise a two's
    /// complement negation of a signed value at its own width.
    fn lower_negation(
        &self,
        operand: &Expr,
        op_span: Span,
        context: Option<WidthContext>,
        span: Span,
    ) -> Result<Lowered> {
        // Only an *unsized* literal, or the legacy `iN` spelling, becomes a negative
        // literal here. A `uN` literal is a value of its own type (`u8` is a
        // `uint<8>`), so negating it falls through to the typed path below, which
        // reports an unsigned negation instead of quietly turning it signed.
        let signed_literal = match operand {
            Expr::Lit(syn::ExprLit {
                lit: Lit::Int(value),
                ..
            }) if unsigned_suffix_width(value.suffix()).is_none() => Some(value),
            _ => None,
        };
        if let Some(value) = signed_literal {
            let magnitude: u128 = value.base10_parse().map_err(|_| {
                self.error(
                    operand.span(),
                    "integer literals wider than 128 bits are not supported",
                )
            })?;
            if magnitude == 0 {
                return self.lower_expr(operand, context, span);
            }
            return match context {
                Some(context) => Lowered::integer(magnitude, true, context.width, context.signed)
                    .adapt(context, span, self),
                None => {
                    // An unsized integer literal is 32 bits wide and signed in
                    // Verilog, so a negative one without a context is that too,
                    // unless the source named a width with a suffix.
                    let width = match value.suffix() {
                        "" => 32,
                        suffix => literal_suffix_width(suffix).ok_or_else(|| {
                            self.error(
                                operand.span(),
                                format!(
                                    "unknown integer literal suffix `{suffix}`; use u1..u128 to \
                                     give the literal an explicit width"
                                ),
                            )
                        })?,
                    };
                    let context = WidthContext {
                        width,
                        signed: true,
                    };
                    let lowered = Lowered::integer(magnitude, true, width, true);
                    check_literal_fits(
                        Literal {
                            value: magnitude,
                            negative: true,
                        },
                        context,
                        operand.span(),
                        self,
                    )?;
                    Ok(lowered)
                }
            };
        }
        let value = self.lower_expr(operand, context, span)?;
        match value.kind {
            Kind::Int { signed: true } => Ok(Lowered::scalar(
                format!("(-({}))", value.text),
                value.width,
                value.kind,
            )),
            Kind::Int { signed: false } => Err(self.error(
                op_span,
                "unary minus on an unsigned value is not supported: the wrap-around would be a \
                 guess; reinterpret it with `as int<W>` or write the constant explicitly (for \
                 example `0xffu8`)",
            )),
            Kind::Bool | Kind::Struct => {
                Err(self.error(op_span, "unary minus needs an integer value"))
            }
        }
    }

    /// Bring the two sides of an operator to the one width and signedness they
    /// are evaluated at.
    ///
    /// A literal adopts the other side; two values must already agree, because
    /// widening, narrowing and sign changes are never implicit.
    fn align_operands(
        &self,
        left: Lowered,
        right: Lowered,
        span: Span,
        operator: &str,
    ) -> Result<(Lowered, Lowered, u32, bool)> {
        let left_sign = int_sign(left.kind)
            .ok_or_else(|| self.error(span, format!("`{operator}` needs two integer operands")))?;
        let right_sign = int_sign(right.kind)
            .ok_or_else(|| self.error(span, format!("`{operator}` needs two integer operands")))?;
        let (width, signed) = match (left.literal.is_some(), right.literal.is_some()) {
            (true, false) => (right.width, right_sign),
            (false, true) => (left.width, left_sign),
            (true, true) => {
                if left_sign != right_sign {
                    return Err(self.error(span, sign_mismatch_message(left_sign, right_sign)));
                }
                (left.width.max(right.width), left_sign)
            }
            (false, false) => {
                let width = packed::align(
                    packed::Width {
                        bits: left.width,
                        signed: left_sign,
                    },
                    packed::Width {
                        bits: right.width,
                        signed: right_sign,
                    },
                )
                .map_err(|error| match error {
                    packed::AlignError::Widths { lhs, rhs } => {
                        let width = lhs.max(rhs);
                        self.error(
                            span,
                            format!(
                                "`{operator}` needs two operands of the same width, but these are \
                                 {lhs} and {rhs} bits; make one explicit, for example `x[0..{width}]` \
                                 or `as uint<{width}>`"
                            ),
                        )
                    }
                    packed::AlignError::Signs { lhs, rhs } => {
                        self.error(span, sign_mismatch_message(lhs, rhs))
                    }
                })?;
                (width.bits, width.signed)
            }
        };
        let context = WidthContext { width, signed };
        let left = left.adapt(context, span, self)?;
        let right = right.adapt(context, span, self)?;
        Ok((left, right, width, signed))
    }

    /// Align the two sides of `assert_eq!`, which may also compare two `bool` or
    /// two packed struct values (bit equality).
    pub(super) fn align_comparable(
        &self,
        left: Lowered,
        right: Lowered,
        span: Span,
        what: &str,
    ) -> Result<(Lowered, Lowered)> {
        match (left.kind, right.kind) {
            (Kind::Bool, Kind::Bool) => Ok((left, right)),
            (Kind::Struct, Kind::Struct) => {
                let context = WidthContext {
                    width: left.width.max(right.width),
                    signed: false,
                };
                Ok((
                    left.adapt(context, span, self)?,
                    right.adapt(context, span, self)?,
                ))
            }
            _ => {
                let (left, right, _, _) = self.align_operands(left, right, span, what)?;
                Ok((left, right))
            }
        }
    }

    /// Resolve an expression to a selected range of one net, if it is one.
    ///
    /// Returns `Ok(None)` when the expression is not a signal reference at all,
    /// so the caller can report the right error.
    pub(super) fn try_value_ref(&self, expr: &Expr) -> Result<Option<ValueRef>> {
        Ok(self.selection(expr)?.map(Selection::into_value_ref))
    }

    /// Resolve an expression to a [`Selection`]: one net, the bit offset of the
    /// selected range inside it, and the dynamic index layers that offset it.
    fn selection(&self, expr: &Expr) -> Result<Option<Selection>> {
        match expr {
            Expr::Paren(paren) => self.selection(&paren.expr),
            Expr::Group(group) => self.selection(&group.expr),
            Expr::Path(path) => self.selection_of_path(path),
            Expr::Field(field) => self.selection_of_field(field),
            Expr::Index(index) => {
                let Some(base) = self.selection(&index.expr)? else {
                    return Ok(None);
                };
                self.select(&base, &index.index, index.span())
            }
            Expr::MethodCall(call) => {
                let Some(base) = self.selection(&call.receiver)? else {
                    return Ok(None);
                };
                let method = call.method.to_string();
                let mut args = call.args.iter();
                match method.as_str() {
                    "get" if call.args.len() == 1 => {
                        let index = args.next().expect("checked length");
                        self.select(&base, index, call.span())
                    }
                    "slice" if call.args.len() == 2 => {
                        let lo = args.next().expect("checked length");
                        let hi = args.next().expect("checked length");
                        let lo = literal_int(lo).ok_or_else(|| {
                            self.error(lo.span(), "`slice` bounds must be integer literals")
                        })?;
                        let hi = literal_int(hi).ok_or_else(|| {
                            self.error(hi.span(), "`slice` bounds must be integer literals")
                        })?;
                        if hi <= lo {
                            return Err(self.error(
                                call.span(),
                                format!(
                                    "the slice `[{lo}..{hi}]` is empty; the upper bound is exclusive"
                                ),
                            ));
                        }
                        self.select_range(&base, lo, hi, call.span())
                    }
                    other => Err(self.error(
                        call.span(),
                        format!(
                            "method `{other}` is not supported on a signal; use `x[i]` or \
                             `x.get(i)` to index, `x[lo..hi]` for a bit range, or `x.slice(lo, hi)` \
                             for an array sub-array"
                        ),
                    )),
                }
            }
            _ => Ok(None),
        }
    }

    fn selection_of_path(&self, path: &syn::ExprPath) -> Result<Option<Selection>> {
        let Some(ident) = single_ident(path) else {
            return Ok(None);
        };
        let name = ident.to_string();
        match self.name(&name) {
            Some(Name::Local { net, ty }) => {
                let width = ty.bit_width()?;
                Ok(Some(Selection::whole(
                    net.clone(),
                    ty.clone(),
                    width,
                    Driver::Test,
                )))
            }
            Some(Name::LoopVar { net }) => Ok(Some(Selection::whole(
                net.clone(),
                ValueType::UInt { width: 32 },
                32,
                Driver::LoopVar,
            ))),
            // An instance alone is not a value; `dut.port` is.
            Some(Name::Instance { .. }) | None => Ok(None),
        }
    }

    fn selection_of_field(&self, field: &syn::ExprField) -> Result<Option<Selection>> {
        let member = match &field.member {
            syn::Member::Named(ident) => ident.to_string(),
            syn::Member::Unnamed(index) => {
                return Err(self.error(
                    index.span,
                    "tuple field access is not supported in a testbench",
                ))
            }
        };
        // `dut.port` where `dut` is a DUT instance.
        if let Expr::Path(path) = field.base.as_ref() {
            if let Some(ident) = single_ident(path) {
                if let Some(Name::Instance { index }) = self.name(&ident.to_string()) {
                    let instance = self.instance(*index);
                    let Some(binding) = instance
                        .ports
                        .iter()
                        .find(|binding| binding.port.source_name == member)
                    else {
                        return Err(self.error(
                            field.span(),
                            format!(
                                "`{}.{}` is not a port of module `{}`; known ports: {}",
                                instance.var,
                                member,
                                instance.module,
                                instance
                                    .ports
                                    .iter()
                                    .map(|binding| binding.port.source_name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        ));
                    };
                    let driver = if binding.port.direction == PortDirection::Out {
                        Driver::DutOutput
                    } else if self.is_clock_net(&binding.net) {
                        Driver::ClockNet
                    } else if self.is_reset_net(&binding.net) {
                        Driver::ResetNet
                    } else {
                        Driver::Test
                    };
                    let width = binding.port.width()?;
                    return Ok(Some(Selection::whole(
                        binding.net.clone(),
                        binding.port.ty.clone(),
                        width,
                        driver,
                    )));
                }
            }
        }
        // `dut.word.op`: a field of a packed struct value.
        let Some(base) = self.selection(&field.base)? else {
            return Ok(None);
        };
        Ok(Some(self.select_field(base, &member, field.span())?))
    }

    /// Select a named field of a packed struct value: the field's offset is added
    /// to the selection, so the emitted Verilog stays one part-select.
    fn select_field(&self, base: Selection, member: &str, span: Span) -> Result<Selection> {
        let ValueType::Struct { fields, .. } = &base.ty else {
            return Err(self.error(
                span,
                format!(
                    "`{member}` is selected from a value of a non-struct type; only packed struct \
                     ports and values have named fields"
                ),
            ));
        };
        let total = base.ty.bit_width()?;
        let mut prefix = 0u32;
        for candidate in fields {
            let width = candidate.ty.bit_width()?;
            if candidate.name == member {
                // Fields are laid out most significant first, so
                // the field's low bit is `total - prefix - width`.
                let lo = total - prefix - width;
                let constant = base
                    .constant
                    .checked_add(lo)
                    .ok_or_else(|| self.error(span, "this selection is too wide to index"))?;
                return Ok(Selection {
                    constant,
                    width,
                    ty: candidate.ty.clone(),
                    ..base
                });
            }
            prefix += width;
        }
        Err(self.error(
            span,
            format!(
                "struct field `{member}` does not exist; known fields: {}",
                fields
                    .iter()
                    .map(|field| field.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
    }

    /// Index one value: an array element by `ty`, or a bit of an integer.
    ///
    /// The dispatch is on `ty`, never on a shared "aggregate" kind, which is what
    /// keeps an array index and a bit index from being confused.
    ///
    /// A bracket range is always a *raw bit range* of the receiver's packed bits,
    /// including when the receiver is an array; an array sub-array is `.slice`.
    /// Its bounds are therefore bit positions, and its type is an unsigned bit
    /// vector (`bool` for one bit), whatever the receiver's element type was.
    fn select(&self, base: &Selection, index: &Expr, span: Span) -> Result<Option<Selection>> {
        if let Expr::Range(range) = index {
            let (lo, hi) = self.range_bounds(range, index.span())?;
            return Ok(Some(self.select_bit_range(base, lo, hi, span)?));
        }
        match &base.ty {
            ValueType::Array { element, len } => {
                Ok(Some(self.select_element(base, index, element, *len, span)?))
            }
            ValueType::UInt { .. } | ValueType::SInt { .. } => {
                Ok(Some(self.select_bit(base, index, span)?))
            }
            ValueType::Bool => Err(self.error(
                span,
                format!(
                    "`{}` is one bit, so it has no bits or elements to select",
                    base.root
                ),
            )),
            _ => Err(self.error(
                span,
                format!(
                    "`{}` has no elements to index; select a struct field by name, or index an \
                     array",
                    base.root
                ),
            )),
        }
    }

    /// Select one bit of an integer: a literal index is folded, a dynamic one keeps
    /// the plain `x[(i)]` bit-select form (a single bit has no neighbours to spill
    /// into, so it needs no guard and its text is unchanged).
    fn select_bit(&self, base: &Selection, index: &Expr, span: Span) -> Result<Selection> {
        let bits = base.width;
        if let Some(position) = literal_int(index) {
            if u64::from(position) >= u64::from(bits) {
                return Err(self.error(
                    index.span(),
                    format!(
                        "bit {position} is outside `{}`, which is {bits} bits wide (bit 0 is the \
                         least significant bit)",
                        base.root
                    ),
                ));
            }
            let constant = base
                .constant
                .checked_add(position)
                .ok_or_else(|| self.error(index.span(), "this selection is too wide to index"))?;
            return Ok(Selection {
                constant,
                width: 1,
                ty: ValueType::Bool,
                ..base.clone()
            });
        }
        let index_value = self.lower_expr(index, None, span)?;
        let mut selection = base.clone();
        selection.terms.push(Term {
            index: zero_extend_to(&index_value.text, index_value.width, 32),
            scale: 1,
        });
        // A dynamic bit index on part of a wider net (`arr[i].field`, a struct
        // field) would otherwise read or write its neighbour's bits, so it carries
        // its own in-range condition too.
        selection
            .guards
            .push(format!("({}) < ({})", index_value.text, bits));
        selection.width = 1;
        selection.ty = ValueType::Bool;
        Ok(selection)
    }

    /// Select a range of bits of the receiver's packed bits, in bit positions.
    ///
    /// A bracket range is untyped raw bits: the result is an unsigned bit vector
    /// (a `bool` for one bit) even when the receiver is an array or a packed
    /// struct, which is why it can be compared with a plain value but needs an
    /// equal-width cast to be read as a signed integer.
    fn select_bit_range(
        &self,
        base: &Selection,
        lo: u32,
        hi: u32,
        span: Span,
    ) -> Result<Selection> {
        let bits = base.width;
        if hi > bits {
            return Err(self.error(
                span,
                format!(
                    "bits {lo}:{} are outside `{}`, which is {bits} bits wide (bit 0 is the least \
                     significant bit)",
                    hi - 1,
                    base.root
                ),
            ));
        }
        let constant = base
            .constant
            .checked_add(lo)
            .ok_or_else(|| self.error(span, "this selection is too wide to index"))?;
        let width = hi - lo;
        Ok(Selection {
            constant,
            width,
            ty: if width == 1 {
                ValueType::Bool
            } else {
                ValueType::UInt { width }
            },
            ..base.clone()
        })
    }

    /// Select an array element.
    ///
    /// A literal index is folded here against the known length. A dynamic index
    /// contributes an offset term and its own in-range guard, and is *widened here*
    /// so the caller never has to write a wider index type: for three elements of
    /// four bits a `uint<2>` index is enough, and the multiplication cannot wrap.
    fn select_element(
        &self,
        base: &Selection,
        index: &Expr,
        element: &ValueType,
        len: usize,
        span: Span,
    ) -> Result<Selection> {
        let element_width = element.bit_width()?;
        if let Some(position) = literal_int(index) {
            if u64::from(position) >= len as u64 {
                return Err(self.error(
                    index.span(),
                    format!(
                        "element {position} is outside an array of {len} element(s); a literal \
                         index is checked while the test is generated"
                    ),
                ));
            }
            let offset = u64::from(position)
                .checked_mul(u64::from(element_width))
                .and_then(|offset| u32::try_from(offset).ok())
                .ok_or_else(|| {
                    self.error(
                        index.span(),
                        "this element is too far into the array to index",
                    )
                })?;
            let constant = base
                .constant
                .checked_add(offset)
                .ok_or_else(|| self.error(index.span(), "this selection is too wide to index"))?;
            return Ok(Selection {
                constant,
                width: element_width,
                ty: element.clone(),
                ..base.clone()
            });
        }
        let index_value = self.lower_expr(index, None, span)?;
        let signed = int_sign(index_value.kind)
            .ok_or_else(|| self.error(index.span(), "an array index must be an integer value"))?;
        if signed {
            return Err(self.error(
                index.span(),
                "an array index must be unsigned; reinterpret it with `as uint<W>`",
            ));
        }
        let mut selection = base.clone();
        selection.terms.push(Term {
            // Zero-extended to 32 bits, so neither this multiplication nor the sum
            // with another layer can wrap, whatever width the index was written at.
            index: zero_extend_to(&index_value.text, index_value.width, 32),
            scale: element_width,
        });
        selection
            .guards
            .push(format!("({}) < ({len})", index_value.text));
        selection.width = element_width;
        selection.ty = element.clone();
        Ok(selection)
    }

    /// `a[lo..hi]` on an array: a sub-array of the same element type, keeping every
    /// dynamic guard of the base selection.
    fn select_subarray_at(
        &self,
        base: &Selection,
        lo: u32,
        hi: u32,
        element: &ValueType,
        len: usize,
        span: Span,
    ) -> Result<Selection> {
        if u64::from(hi) > len as u64 {
            return Err(self.error(
                span,
                format!("the sub-array `[{lo}..{hi}]` is outside an array of {len} element(s)"),
            ));
        }
        let element_width = element.bit_width()?;
        let count = hi - lo;
        let offset = u64::from(lo)
            .checked_mul(u64::from(element_width))
            .and_then(|offset| u32::try_from(offset).ok())
            .ok_or_else(|| self.error(span, "this sub-array is too far into the array to index"))?;
        let constant = base
            .constant
            .checked_add(offset)
            .ok_or_else(|| self.error(span, "this selection is too wide to index"))?;
        let width = count
            .checked_mul(element_width)
            .ok_or_else(|| self.error(span, "this sub-array is too wide to select"))?;
        Ok(Selection {
            constant,
            width,
            ty: ValueType::Array {
                element: Box::new(element.clone()),
                len: count as usize,
            },
            ..base.clone()
        })
    }

    /// `x.slice(lo, hi)`: a sub-array of an array, counted in *elements*.
    ///
    /// This is deliberately not a bit range: bits are written with brackets
    /// (`x[lo..hi]`), so asking for `.slice` on anything that is not an array is
    /// refused with that hint rather than reinterpreted.
    fn select_range(
        &self,
        base: &Selection,
        lo: u32,
        hi: u32,
        span: Span,
    ) -> Result<Option<Selection>> {
        match &base.ty {
            ValueType::Array { element, len } => Ok(Some(
                self.select_subarray_at(base, lo, hi, element, *len, span)?,
            )),
            _ => Err(self.error(
                span,
                format!(
                    "`.slice` takes a sub-array and only applies to an array, but `{}` is a single \
                     value of {} bits; for a bit range use brackets, where the bounds are bit \
                     positions: `{}[{lo}..{hi}]`",
                    base.root, base.width, base.root
                ),
            )),
        }
    }

    fn range_bounds(&self, range: &syn::ExprRange, span: Span) -> Result<(u32, u32)> {
        let start = range
            .start
            .as_ref()
            .and_then(|expr| literal_int(expr))
            .ok_or_else(|| {
                self.error(
                    span,
                    "the lower bound of a slice must be an integer literal",
                )
            })?;
        let end = range
            .end
            .as_ref()
            .and_then(|expr| literal_int(expr))
            .ok_or_else(|| {
                self.error(
                    span,
                    "the upper bound of a slice must be an integer literal",
                )
            })?;
        let hi = match range.limits {
            syn::RangeLimits::HalfOpen(_) => end,
            syn::RangeLimits::Closed(_) => end.checked_add(1).ok_or_else(|| {
                self.error(span, "the upper bound of this inclusive slice is too large")
            })?,
        };
        if hi <= start {
            return Err(self.error(
                span,
                format!(
                    "the slice `[{start}..{end}]` selects no bits; slices are half-open and count \
                     up from bit 0"
                ),
            ));
        }
        Ok((start, hi))
    }
}

/// One operand of a typed comparison.
///
/// Equality of an aggregate is built part by part from a [`packed::Equality`]
/// plan, and each part must be a legal Verilog expression. A signal reference can
/// be folded (a part-select of its root, never a select of a select); a constant
/// is computed here and emitted as one sized literal, because Verilog cannot
/// select bits of a literal.
enum Operand {
    Signal(Selection),
    /// Any other expression whose text is a legal Verilog expression: its parts
    /// are taken with a shift and a mask, which needs no temporary.
    Expr(String),
    Const {
        value: u128,
    },
}

impl Operand {
    /// The text of `width` bits at `lo` inside this operand.
    fn part(&self, lo: u32, width: u32) -> String {
        match self {
            Operand::Signal(selection) => Selection {
                constant: selection.constant + lo,
                width,
                // Only the selected width matters here; the bits are compared as
                // bits, so the part is read as an unsigned range of that width.
                ty: ValueType::UInt { width },
                ..selection.clone()
            }
            .text(),
            Operand::Expr(text) => {
                // Shifting and masking is legal for any expression in Verilog-2005,
                // and the mask also clears the bits above the part.
                let mask = if width == 128 {
                    u128::MAX
                } else {
                    (1u128 << width) - 1
                };
                format!("((({text}) >> {lo}) & {width}'d{mask})")
            }
            Operand::Const { .. } => {
                let masked = self.part_value(lo, width).expect("a constant part");
                format!("{width}'d{masked}")
            }
        }
    }

    /// The value of `width` bits at `lo`, when this operand is a constant.
    fn part_value(&self, lo: u32, width: u32) -> Option<u128> {
        match self {
            Operand::Const { value, .. } => {
                let shifted = value >> lo;
                Some(if width == 128 {
                    shifted
                } else {
                    shifted & ((1u128 << width) - 1)
                })
            }
            _ => None,
        }
    }

    /// Whether a part can carry `x`/`z`: a constant cannot, anything else can.
    fn may_be_unknown(&self) -> bool {
        !matches!(self, Operand::Const { .. })
    }
}

impl<'a> Generator<'a> {
    /// Build an `enum` value from a variant constructor, from a value that already
    /// has the enum's type, or from an explicit bare literal.
    ///
    /// The layout is the metadata's: the tag in the low `tag_width` bits, the
    /// payload above it, and the unused payload bits zero.
    pub(super) fn lower_enum_value(
        &self,
        expr: &Expr,
        ty: &ValueType,
        span: Span,
    ) -> Result<Lowered> {
        let ValueType::Enum {
            name: enum_name,
            tag_width,
            variants,
        } = ty
        else {
            return Err(self.error(span, "an enum value needs an enum target"));
        };
        let tag_width = *tag_width;
        let widest = variants
            .iter()
            .filter_map(|variant| variant.payload.as_ref())
            .map(|payload| payload.bit_width())
            .collect::<Result<Vec<u32>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let total = ty.bit_width()?;
        // `None` / `Red`: a variant named in the source, as long as it is not a
        // local with that name.
        if let Expr::Path(path) = expr {
            if let Some(name) = variant_name(&path.path, enum_name) {
                // A qualified path cannot be shadowed by a local; a bare name can.
                let shadowed = path.path.segments.len() == 1 && self.name(&name).is_some();
                if !shadowed {
                    if let Some(variant) = variants.iter().find(|variant| variant.name == name) {
                        check_tag(variant.tag, tag_width, span, self)?;
                        if variant.payload.is_some() {
                            return Err(self.error(
                                span,
                                format!(
                                    "variant `{name}` carries a payload, so it is built as \
                                     `{name}(..)`"
                                ),
                            ));
                        }
                        // The whole enum's width, not just the tag: a payload-free
                        // variant still has the payload bits (zero) above its tag,
                        // and a nested construction concatenates this text directly.
                        return Ok(Lowered::typed(
                            format!("{total}'d{}", variant.tag),
                            ty.clone(),
                            total,
                            kind_of(ty),
                        ));
                    }
                }
            }
        }
        // `Some(x)` / `Ok(x)` / `Err(e)`: a variant constructor.
        if let Expr::Call(call) = expr {
            if let Expr::Path(callee) = call.func.as_ref() {
                if let Some(name) = variant_name(&callee.path, enum_name) {
                    let shadowed = callee.path.segments.len() == 1 && self.name(&name).is_some();
                    if !shadowed {
                        if let Some(variant) = variants.iter().find(|variant| variant.name == name)
                        {
                            check_tag(variant.tag, tag_width, span, self)?;
                            let Some(payload_ty) = &variant.payload else {
                                return Err(self
                                    .error(span, format!("variant `{name}` carries no payload")));
                            };
                            // A payload the frontend generated for this variant has two
                            // shapes: fields `0..n-1` are a multi-payload, written as
                            // `V(a, b)`, while named fields are written as `V { a, b }`.
                            // Any other payload type is one value of its whole type.
                            let (text, payload_width) = if let Some(fields) =
                                generated_aggregate(enum_name, &name, payload_ty)
                            {
                                if numeric_fields(fields) {
                                    if call.args.len() != fields.len() {
                                        return Err(self.error(
                                            span,
                                            format!(
                                                "variant `{name}` takes {} value(s), but {} were \
                                                     given",
                                                fields.len(),
                                                call.args.len()
                                            ),
                                        ));
                                    }
                                    // Most significant field first, as the metadata
                                    // lays a generated aggregate out.
                                    let mut parts = Vec::with_capacity(fields.len());
                                    let mut width = 0u32;
                                    for (argument, field) in call.args.iter().zip(fields) {
                                        let value = self.lower_stored_value(
                                            argument,
                                            &field.ty,
                                            argument.span(),
                                        )?;
                                        width += field.ty.bit_width()?;
                                        parts.push(value.text);
                                    }
                                    (format!("{{{}}}", parts.join(", ")), width)
                                } else {
                                    return Err(self.error(
                                        span,
                                        format!(
                                            "variant `{name}` takes named fields, as in \
                                                 `{name} {{ .. }}`"
                                        ),
                                    ));
                                }
                            } else {
                                if call.args.len() != 1 {
                                    return Err(self.error(
                                        span,
                                        format!(
                                            "variant `{name}` takes one payload value, but {} \
                                                 were given",
                                            call.args.len()
                                        ),
                                    ));
                                }
                                let argument = call.args.first().expect("checked length");
                                let value =
                                    self.lower_stored_value(argument, payload_ty, argument.span())?;
                                (value.text, payload_ty.bit_width()?)
                            };
                            // The payload sits above the tag; the bits above it are
                            // zero, as the metadata layout requires.
                            let padded = if payload_width == widest {
                                text
                            } else {
                                format!("{{{}'b0, {}}}", widest - payload_width, text)
                            };
                            return Ok(Lowered::typed(
                                format!("{{{padded}, {tag_width}'d{}}}", variant.tag),
                                ty.clone(),
                                total,
                                kind_of(ty),
                            ));
                        }
                    }
                }
            }
        }
        // `Msg::Pair { hi, lo }`: a variant whose payload is a struct, given by
        // field name. The payload is assembled in declaration order (most
        // significant field first), exactly as the metadata lays it out.
        if let Expr::Struct(structure) = expr {
            let Some(name) = variant_name(&structure.path, enum_name) else {
                return Err(self.error(
                    span,
                    format!(
                        "`{}` is not a variant of `{enum_name}`",
                        super::lower::render_expr(expr)
                    ),
                ));
            };
            if let Some(rest) = &structure.rest {
                return Err(self.error(
                    rest.span(),
                    "`..` in a variant construction is not supported; name every field, because a \
                     function update would need a value of the same type",
                ));
            }
            let variant = variants
                .iter()
                .find(|variant| variant.name == name)
                .ok_or_else(|| {
                    self.error(span, format!("`{enum_name}` has no variant `{name}`"))
                })?;
            check_tag(variant.tag, tag_width, span, self)?;
            let Some(payload_ty) = &variant.payload else {
                return Err(self.error(span, format!("variant `{name}` carries no payload")));
            };
            let Some(fields) = generated_aggregate(enum_name, &name, payload_ty) else {
                // A user struct (or any other type) is one payload value, so its own
                // fields must not be mixed into the variant's field list.
                return Err(self.error(
                    span,
                    format!(
                        "variant `{name}` takes one payload value of its own type, as in \
                         `{name}(..)`, not named fields"
                    ),
                ));
            };
            if numeric_fields(fields) {
                return Err(self.error(
                    span,
                    format!("variant `{name}` takes positional values, as in `{name}(..)`"),
                ));
            }
            let mut provided = Vec::new();
            for field in &structure.fields {
                let syn::Member::Named(ident) = &field.member else {
                    return Err(
                        self.error(field.span(), "a variant's fields are named, not positional")
                    );
                };
                provided.push((ident.to_string(), field.expr.clone()));
            }
            for (position, (field_name, value_expr)) in provided.iter().enumerate() {
                if provided[..position]
                    .iter()
                    .any(|(previous, _)| previous == field_name)
                {
                    return Err(self.error(
                        value_expr.span(),
                        format!("variant `{name}` field `{field_name}` is given more than once"),
                    ));
                }
            }
            let mut parts = Vec::new();
            let mut payload_width = 0u32;
            for field in fields {
                let Some((_, value_expr)) = provided
                    .iter()
                    .find(|(provided, _)| provided == &field.name)
                else {
                    return Err(self.error(
                        span,
                        format!("variant `{name}` needs a value for field `{}`", field.name),
                    ));
                };
                let value = self.lower_stored_value(value_expr, &field.ty, value_expr.span())?;
                payload_width += field.ty.bit_width()?;
                parts.push(value.text);
            }
            for (provided, value_expr) in &provided {
                if !fields.iter().any(|field| &field.name == provided) {
                    return Err(self.error(
                        value_expr.span(),
                        format!("variant `{name}` has no field `{provided}`"),
                    ));
                }
            }
            let packed = format!("{{{}}}", parts.join(", "));
            let padded = if payload_width == widest {
                packed
            } else {
                format!("{{{}'b0, {packed}}}", widest - payload_width)
            };
            return Ok(Lowered::typed(
                format!("{{{padded}, {tag_width}'d{}}}", variant.tag),
                ty.clone(),
                total,
                kind_of(ty),
            ));
        }

        // A bare literal is explicit raw bits (which is how a test pokes an invalid
        // tag); a computed integer is not an enum.
        if matches!(expr, Expr::Lit(_)) {
            // Explicit raw bits: the tag is whatever the literal says.
            return self.lower_expr(
                expr,
                Some(WidthContext {
                    width: total,
                    signed: false,
                }),
                span,
            );
        }
        Err(self.error(
            span,
            "an enum value is built from a variant (`None`, `Some(x)`, `Ok(x)`) or from an explicit \
             literal; a computed integer has no tag and is not an enum",
        ))
    }

    /// One operand of a typed comparison: a signal reference or a constant.
    ///
    /// `Ok(None)` means the expression is neither, so the caller keeps its
    /// existing behaviour.
    fn comparison_operand(
        &self,
        expr: &Expr,
        ty: &ValueType,
        span: Span,
    ) -> Result<Option<Operand>> {
        if let Some(selection) = self.selection(expr)? {
            return Ok(Some(Operand::Signal(selection)));
        }
        if let Some(value) = self.constant_operand(expr, ty, span)? {
            return Ok(Some(Operand::Const { value }));
        }
        // Anything else is built at the expected type: a variant construction is a
        // concatenation of literals and payload parts, and the shift and the mask
        // above take it apart without any temporary.
        let value = self.lower_stored_value(expr, ty, span)?;
        Ok(Some(Operand::Expr(value.text)))
    }

    /// The raw bits of a constant literal at this type's width.
    fn constant_operand(&self, expr: &Expr, ty: &ValueType, span: Span) -> Result<Option<u128>> {
        let (magnitude, negative) = match expr {
            Expr::Lit(syn::ExprLit {
                lit: Lit::Int(value),
                ..
            }) => (integer_literal(value, span, self)?, false),
            Expr::Lit(syn::ExprLit {
                lit: Lit::Bool(value),
                ..
            }) => (u128::from(value.value), false),
            Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
                let Expr::Lit(syn::ExprLit {
                    lit: Lit::Int(value),
                    ..
                }) = unary.expr.as_ref()
                else {
                    return Ok(None);
                };
                (integer_literal(value, span, self)?, true)
            }
            _ => return Ok(None),
        };
        let width = ty.bit_width()?;
        let bits = match ty {
            ValueType::Bool => {
                if negative || magnitude > 1 {
                    return Err(self.error(span, format!("`{magnitude}` is not a `bool` value")));
                }
                magnitude
            }
            ValueType::UInt { width } => {
                if !packed::literal_fits(negative, magnitude, *width, false) {
                    return Err(
                        self.error(span, format!("`{magnitude}` does not fit `uint<{width}>`"))
                    );
                }
                magnitude
            }
            ValueType::SInt { width } => {
                if *width >= 128 {
                    return Err(self.error(
                        span,
                        "an `int<128>` literal is not supported in a testbench",
                    ));
                }
                let value = if negative {
                    -(magnitude as i128)
                } else {
                    magnitude as i128
                };
                if !packed::literal_fits(negative, magnitude, *width, true) {
                    return Err(self.error(span, format!("`{value}` does not fit `int<{width}>`")));
                }
                // Two's complement at the declared width.
                if value < 0 {
                    (value + (1i128 << width)) as u128
                } else {
                    value as u128
                }
            }
            // An enum (or any other packed type) takes the literal's raw bits.
            _ => {
                if !packed::literal_fits(negative, magnitude, width, false) {
                    return Err(
                        self.error(span, format!("`{magnitude}` does not fit {width} bits"))
                    );
                }
                magnitude
            }
        };
        Ok(Some(bits))
    }

    /// The part-by-part equality of two operands whose type contains an enum.
    ///
    /// `Ok(None)` means the comparison is not enum-shaped, or a side cannot be
    /// described, so the caller keeps its ordinary comparison (which reports a
    /// shape mismatch itself).
    pub(super) fn typed_comparison(
        &self,
        left: &Expr,
        right: &Expr,
        span: Span,
    ) -> Result<Option<String>> {
        let left_selection = self.selection(left)?;
        let right_selection = self.selection(right)?;
        let ty = match (&left_selection, &right_selection) {
            (Some(left), _) if packed::contains_enum(&left.ty) => left.ty.clone(),
            (_, Some(right)) if packed::contains_enum(&right.ty) => right.ty.clone(),
            _ => return Ok(None),
        };
        for selection in [&left_selection, &right_selection].into_iter().flatten() {
            if selection.ty != ty {
                // Two enum-shaped values of different types are not comparable, so
                // this is a diagnostic rather than a fallback to the bit test that
                // would accept equal widths of a different meaning.
                return Err(self.error(
                    span,
                    format!(
                        "these have different shapes and cannot be compared: `{}` and `{}`",
                        shape_name(&selection.ty),
                        shape_name(&ty)
                    ),
                ));
            }
        }
        let Some(left) = self.comparison_operand(left, &ty, span)? else {
            return Ok(None);
        };
        let Some(right) = self.comparison_operand(right, &ty, span)? else {
            return Ok(None);
        };
        Ok(Some(self.typed_equality(&left, &right, &ty, span)?))
    }

    /// A boolean expression that is true exactly when two runtime values are
    /// equal as values of `ty`.
    ///
    /// The comparison follows [`packed::Equality`]: an enum compares its tag and
    /// the payload of the variant that is active, so nested inactive payloads are
    /// ignored, and a tag carrying `x`/`z` never compares equal.
    fn typed_equality(
        &self,
        left: &Operand,
        right: &Operand,
        ty: &ValueType,
        span: Span,
    ) -> Result<String> {
        let plan = packed::equality(ty).map_err(|error| self.error(span, error))?;
        self.equal_parts(left, right, &plan)
    }

    /// The comparison one [`packed::Equality`] plan describes.
    fn equal_parts(
        &self,
        left: &Operand,
        right: &Operand,
        plan: &packed::Equality,
    ) -> Result<String> {
        match plan {
            packed::Equality::Bits { lo, width } => {
                if let (Some(a), Some(b)) =
                    (left.part_value(*lo, *width), right.part_value(*lo, *width))
                {
                    // Two constants: decide this part here. It compares the selected
                    // bits, not the whole packed operand, because a payload lives at
                    // an offset inside its enum.
                    return Ok(bool_text(a == b));
                }
                Ok(format!(
                    "({} === {})",
                    left.part(*lo, *width),
                    right.part(*lo, *width)
                ))
            }
            packed::Equality::All(parts) => {
                let mut texts = Vec::with_capacity(parts.len());
                for part in parts {
                    texts.push(self.equal_parts(left, right, part)?);
                }
                Ok(conjoin(&texts))
            }
            packed::Equality::Enum {
                tag_lo,
                tag_width,
                variants,
            } => {
                let tag_left = left.part(*tag_lo, *tag_width);
                let tag_right = right.part(*tag_lo, *tag_width);
                // `x`/`z` in either tag is a failure, not an unknown variant that
                // happens to be equal. `^tag` is `x` whenever the tag has any x/z.
                let mut known = Vec::new();
                for (operand, tag) in [(left, &tag_left), (right, &tag_right)] {
                    if operand.may_be_unknown() {
                        known.push(format!("((^{tag}) !== 1'bx)"));
                    }
                }
                if known.is_empty() {
                    known.push("1'b1".to_string());
                }
                let tags_equal = format!("({tag_left} === {tag_right})");
                let mut payload = "1'b1".to_string();
                // A tag that names no variant falls through to `1'b1`: the tags are
                // already equal, and nothing else about the value is defined.
                for (tag, part) in variants.iter().rev() {
                    let branch = self.equal_parts(left, right, part)?;
                    payload =
                        format!("(({tag_left} === {tag_width}'d{tag}) ? {branch} : {payload})");
                }
                Ok(format!(
                    "({} && {tags_equal} && ({payload}))",
                    conjoin(&known)
                ))
            }
        }
    }
}

/// The variant a path names, when any qualifier names this enum.
///
/// `Msg::Pair` and `Pair` both name `Pair`, while `Other::Pair` is not a variant
/// of `Msg` and is left to the ordinary resolution order.
fn variant_name(path: &syn::Path, enum_name: &str) -> Option<String> {
    let segments: Vec<&syn::PathSegment> = path.segments.iter().collect();
    // `Data` or `Msg::Data`, never `Msg::Msg::Data`.
    let (last, qualifier) = match segments.split_last() {
        Some((last, [])) => (last, None),
        Some((last, [qualifier])) => (last, Some(qualifier)),
        _ => return None,
    };
    if let Some(qualifier) = qualifier {
        // A builtin enum's metadata name carries its arguments, as in
        // `Option<uint<4>>`, while the source writes the root: `Option::Some`.
        let root = enum_name.split('<').next().unwrap_or(enum_name);
        if qualifier.ident != root {
            return None;
        }
    }
    Some(last.ident.to_string())
}

/// A variant's encoded tag has to fit its declared tag width.
fn check_tag(tag: u64, tag_width: u32, span: Span, generator: &Generator<'_>) -> Result<()> {
    if tag_width < 64 && tag >= (1u64 << tag_width) {
        return Err(generator.error(
            span,
            format!("variant tag {tag} does not fit the enum's {tag_width}-bit tag"),
        ));
    }
    Ok(())
}

/// The value of an integer literal, as the testbench reads it.
fn integer_literal(value: &syn::LitInt, span: Span, generator: &Generator<'_>) -> Result<u128> {
    value.base10_parse().map_err(|_| {
        generator.error(
            span,
            "integer literals wider than 128 bits are not supported",
        )
    })
}

/// Whether a type is an enum, or contains one anywhere.
///
/// Only these comparisons need the part-by-part form: a plain struct or array of
/// integers keeps the ordinary bit comparison.
/// The fields of a payload the frontend generated for a variant.
///
/// A generated aggregate is named `enum::variant`; any other struct payload is a
/// user type that stands for one value.
fn generated_aggregate<'a>(
    enum_name: &str,
    variant_name: &str,
    payload: &'a ValueType,
) -> Option<&'a [TypeField]> {
    let ValueType::Struct { name, fields } = payload else {
        return None;
    };
    if *name != format!("{enum_name}::{variant_name}") {
        return None;
    }
    Some(fields)
}

/// True when the fields are `0..n-1`, the shape of a multi-payload variant.
fn numeric_fields(fields: &[TypeField]) -> bool {
    fields
        .iter()
        .enumerate()
        .all(|(position, field)| field.name == position.to_string())
}

/// A short spelling of a type, for the comparison diagnostic.
fn shape_name(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "bool".to_string(),
        ValueType::UInt { width } => format!("uint<{width}>"),
        ValueType::SInt { width } => format!("int<{width}>"),
        ValueType::Struct { name, .. } => format!("struct {name}"),
        ValueType::Enum { name, .. } => format!("enum {name}"),
        ValueType::Array { element, len } => format!("[{}; {len}]", shape_name(element)),
    }
}

/// `1'b1` or `1'b0`.
fn bool_text(value: bool) -> String {
    if value {
        "1'b1".to_string()
    } else {
        "1'b0".to_string()
    }
}

/// A conjunction of already parenthesised conditions.
fn conjoin(parts: &[String]) -> String {
    if parts.is_empty() {
        return "1'b1".to_string();
    }
    format!("({})", parts.join(" && "))
}

/// A bit range inside one net, with the dynamic index layers that offset it.
///
/// Folding every layer here is what keeps the emitted Verilog legal (Verilog has
/// no `x[a][b]`) and lets `arr[i].field`, `arr[i][j]` and a further bit select
/// share one selection instead of nesting part-selects.
#[derive(Clone)]
struct Selection {
    /// The net the range lives in; never itself a slice.
    root: String,
    /// Width of the whole net, which is the only thing a "whole value" test may
    /// compare against: the *selected* type's width says nothing about the root.
    root_width: u32,
    /// Constant bit offset from the net's least significant bit.
    constant: u32,
    /// Dynamic layers, in selection order.
    terms: Vec<Term>,
    /// In-range conditions of the dynamic layers, ANDed when used.
    guards: Vec<String>,
    /// Width of the selected range.
    width: u32,
    /// Type of the selected range: the shape authority.
    ty: ValueType,
    /// What drives the net.
    driver: Driver,
}

/// One dynamic index layer: it offsets the selection by `index * scale` bits.
#[derive(Clone)]
struct Term {
    /// Index expression, already zero-extended to 32 bits so that neither the
    /// multiplication nor the sum of several layers can wrap.
    index: String,
    /// Bits per index step.
    scale: u32,
}

impl Selection {
    /// A whole net. The caller passes the width it already computed from the
    /// type, so a layout error is never swallowed here.
    fn whole(root: String, ty: ValueType, width: u32, driver: Driver) -> Self {
        Self {
            root,
            root_width: width,
            constant: 0,
            terms: Vec::new(),
            guards: Vec::new(),
            width,
            ty,
            driver,
        }
    }

    /// The plain Verilog text of the selection; also a legal assignment target.
    fn text(&self) -> String {
        if self.terms.is_empty() {
            // "Whole value" compares against the *root*, never against the selected
            // type: `a[0]` of a wider array also has `constant == 0`.
            if self.constant == 0 && self.width == self.root_width {
                return self.root.clone();
            }
            if self.width == 1 {
                return format!("{}[{}]", self.root, self.constant);
            }
            return format!(
                "{}[{}:{}]",
                self.root,
                self.constant + self.width - 1,
                self.constant
            );
        }
        // A one bit root is declared as a scalar (`reg x;`), which has no bit to
        // select: the value is the root itself, and its guard makes an index other
        // than zero read as `x` and leave a write without effect.
        if self.root_width == 1 && self.width == 1 {
            return self.root.clone();
        }
        // Every index is zero-extended to 32 bits and every scale is a 32 bit
        // constant, so neither `index * scale` nor the sum of the layers can wrap
        // for any offset inside a root of the supported size.
        let mut offset = Vec::new();
        if self.constant != 0 {
            offset.push(format!("32'd{}", self.constant));
        }
        for term in &self.terms {
            if term.scale == 1 {
                offset.push(format!("({})", term.index));
            } else {
                offset.push(format!("({}) * 32'd{}", term.index, term.scale));
            }
        }
        format!("{}[({}) +: {}]", self.root, offset.join(" + "), self.width)
    }

    fn into_value_ref(self) -> ValueRef {
        let text = self.text();
        let guard = if self.guards.is_empty() {
            None
        } else {
            Some(self.guards.join(" && "))
        };
        let kind = kind_of(&self.ty);
        ValueRef {
            text,
            ty: self.ty,
            kind,
            width: self.width,
            driver: self.driver,
            guard,
        }
    }
}

/// Zero-extend `text` from `from` bits to `to` bits (Verilog concatenation).
fn zero_extend_to(text: &str, from: u32, to: u32) -> String {
    if from >= to {
        text.to_string()
    } else {
        format!("{{{}'b0, {}}}", to - from, text)
    }
}

/// The text of a value used as an operand (a read).
///
/// A dynamic selection is guarded here, so an out-of-range index reads as `x`
/// instead of a neighbouring element. A Verilog part-select also loses the
/// `signed` attribute, so reading a signed slice asks for it back with
/// `$signed(...)`; a whole signed signal is already `signed`. An assignment target
/// uses [`ValueRef::text`] directly, because neither a ternary nor a system
/// function call can be assigned to.
fn operand_text(value: &ValueRef) -> String {
    let inner = match &value.guard {
        Some(guard) => format!("(({guard}) ? {} : {}'bx)", value.text, value.width),
        None => value.text.clone(),
    };
    if matches!(value.kind, Kind::Int { signed: true }) && !is_plain_identifier(&value.text) {
        format!("$signed({inner})")
    } else {
        inner
    }
}

/// Whether `text` is a bare identifier (a whole signal), which keeps the
/// `signed` attribute a declaration gave it.
fn is_plain_identifier(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_digit())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn single_ident(path: &syn::ExprPath) -> Option<&syn::Ident> {
    if path.qself.is_some() || path.path.segments.len() != 1 {
        return None;
    }
    let segment = &path.path.segments[0];
    if !matches!(segment.arguments, syn::PathArguments::None) {
        return None;
    }
    Some(&segment.ident)
}

/// An unsuffixed integer literal used only as the operand of a cast, looking
/// through parentheses and a leading `-`.
///
/// A suffixed literal names its own type (`255u8` is a known `uint<8>`), so it
/// stays a value with its own width and the cast must not resize it.
fn unsuffixed_literal_operand(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Paren(paren) => unsuffixed_literal_operand(&paren.expr),
        Expr::Group(group) => unsuffixed_literal_operand(&group.expr),
        Expr::Unary(unary) if matches!(unary.op, UnOp::Neg(_)) => {
            unsuffixed_literal_operand(&unary.expr)?;
            Some(expr)
        }
        Expr::Lit(syn::ExprLit {
            lit: Lit::Int(value),
            ..
        }) if value.suffix().is_empty() => Some(expr),
        _ => None,
    }
}

fn literal_int(expr: &Expr) -> Option<u32> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Int(value),
            ..
        }) => value.base10_parse().ok(),
        Expr::Paren(paren) => literal_int(&paren.expr),
        Expr::Group(group) => literal_int(&group.expr),
        _ => None,
    }
}

/// The width of an explicit `uN` suffix, which names a `uint<N>` type.
///
/// `iN` and every other suffix are legacy spellings that keep fitting the
/// context, so they are deliberately not a known unsigned type.
fn unsigned_suffix_width(suffix: &str) -> Option<u32> {
    let digits = suffix.strip_prefix('u')?;
    let width: u32 = digits.parse().ok()?;
    if width == 0 || width > 128 {
        return None;
    }
    Some(width)
}

/// The type an integer literal's suffix names, if it names one.
///
/// A `uN` suffix is the documented "this literal is a `uint<N>`" spelling, so it
/// is what lets a value state its own type without a written annotation (for
/// example the arms of a `match` expression, or a `let` with no type). `iN` stays
/// a width-only legacy spelling and is left to the context, exactly as
/// [`Self::lower_literal`] treats it.
pub(super) fn suffixed_literal_type(suffix: &str) -> Option<ValueType> {
    unsigned_suffix_width(suffix).map(|width| ValueType::UInt { width })
}

fn literal_suffix_width(suffix: &str) -> Option<u32> {
    let digits = suffix
        .strip_prefix('u')
        .or_else(|| suffix.strip_prefix('i'))?;
    let width: u32 = digits.parse().ok()?;
    if width == 0 || width > 128 {
        return None;
    }
    Some(width)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Arithmetic,
    Shift,
    Compare,
    Logical,
}

fn binary_operator(op: &BinOp) -> Option<(&'static str, OpKind)> {
    let result = match op {
        BinOp::Add(_) => ("+", OpKind::Arithmetic),
        BinOp::Sub(_) => ("-", OpKind::Arithmetic),
        BinOp::Mul(_) => ("*", OpKind::Arithmetic),
        BinOp::Div(_) => ("/", OpKind::Arithmetic),
        BinOp::Rem(_) => ("%", OpKind::Arithmetic),
        BinOp::BitAnd(_) => ("&", OpKind::Arithmetic),
        BinOp::BitOr(_) => ("|", OpKind::Arithmetic),
        BinOp::BitXor(_) => ("^", OpKind::Arithmetic),
        BinOp::Shl(_) => ("<<", OpKind::Shift),
        BinOp::Shr(_) => (">>", OpKind::Shift),
        BinOp::Eq(_) => ("==", OpKind::Compare),
        BinOp::Ne(_) => ("!=", OpKind::Compare),
        BinOp::Lt(_) => ("<", OpKind::Compare),
        BinOp::Le(_) => ("<=", OpKind::Compare),
        BinOp::Gt(_) => (">", OpKind::Compare),
        BinOp::Ge(_) => (">=", OpKind::Compare),
        BinOp::And(_) => ("&&", OpKind::Logical),
        BinOp::Or(_) => ("||", OpKind::Logical),
        _ => return None,
    };
    Some(result)
}

fn render_unop(op: &UnOp) -> &'static str {
    match op {
        UnOp::Not(_) => "!",
        UnOp::Neg(_) => "-",
        _ => "?",
    }
}
