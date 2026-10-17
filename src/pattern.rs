//! Normalized match patterns, coverage checking and pattern tests.
//!
//! Crate-private; the consumers are the frontend's `match` lowering and the
//! testbench. No diagnostics live here: every error carries a
//! `proc_macro2::Span` that the caller maps into its own type.
//!
//! Coverage is over the **typed** domain, never over packed bits:
//!
//! * a payload-free variant covers the whole variant (slack bits are free),
//! * a variant payload is a typed sub-domain, not the overlay width,
//! * an enum has one constructor per variant plus an unnamed *invalid*
//!   constructor only when the tag width leaves unused encodings; only
//!   `Wild`/`Bind` specialize it,
//! * a scalar has one constructor per constant in the matrix plus a generic
//!   branch; its domain is never enumerated,
//! * `struct`/`array` have a single product constructor and recurse.
//!
//! Guards are not part of this module: callers add only unguarded
//! alternatives, so a guarded arm can neither contribute coverage nor shadow a
//! later arm.
//!
//! Every analysis is bounded and reports an explicit error instead of
//! pretending a match is not exhaustive: pattern nesting, alternatives, matrix
//! rows, matrix columns and the total coverage work all have explicit limits.

use std::collections::BTreeMap;

use proc_macro2::Span;
use syn::spanned::Spanned;

use crate::metadata::{EnumVariant, TypeField, ValueType};

/// Pattern *nesting* limit: how deeply sub-patterns may nest (lowering only).
const MAX_PATTERN_DEPTH: usize = 16;
const MAX_ALTERNATIVES: usize = 64;
const MAX_MATRIX_ROWS: usize = 4096;
/// Parallel columns one matrix may have. Also the recursion depth of the
/// coverage walk, so the stack stays bounded.
const MAX_MATRIX_COLUMNS: usize = 256;
const MAX_MATRIX_CELLS: usize = 65536;
/// Cumulative cells the coverage walk may visit before it reports that the
/// match is too large to prove. Bounds total work, which the per-call
/// [`MAX_MATRIX_CELLS`] cannot.
const MAX_COVERAGE_WORK: usize = 1_000_000;

/// A pattern that cannot be normalized, or an analysis that hit a bound.
#[derive(Clone, Debug)]
pub(crate) struct PatError {
    pub span: Span,
    pub kind: PatErrorKind,
}

impl PatError {
    pub(crate) fn new(span: Span, kind: PatErrorKind) -> Self {
        Self { span, kind }
    }

    pub(crate) fn message(&self) -> String {
        self.kind.message()
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PatErrorKind {
    UnknownName(String),
    NotAConstructor(String),
    LiteralOutOfRange {
        width: u32,
    },
    ArityMismatch {
        expected: usize,
        found: usize,
    },
    PayloadArity {
        variant: String,
    },
    BindingMismatch {
        name: String,
    },
    UnsupportedPattern(String),
    TooManyAlternatives {
        count: usize,
        limit: usize,
    },
    /// A pattern nested deeper than [`MAX_PATTERN_DEPTH`]; produced by pattern
    /// lowering only, never by the coverage walk.
    TooDeep {
        depth: usize,
        limit: usize,
    },
    CoverageLimit(String),
    Layout(String),
}

impl PatErrorKind {
    pub(crate) fn message(&self) -> String {
        match self {
            PatErrorKind::UnknownName(name) => format!(
                "`{name}` is not a constant and not a variant of the matched type; write `_` to bind"
            ),
            PatErrorKind::NotAConstructor(what) => {
                format!("{what} cannot be matched against this type")
            }
            PatErrorKind::LiteralOutOfRange { width } => format!(
                "this pattern value does not fit the matched type ({width} bit(s), two's complement \
                 for a signed type)"
            ),
            PatErrorKind::ArityMismatch { expected, found } => {
                format!("this pattern names {found} element(s) but the type has {expected}")
            }
            PatErrorKind::PayloadArity { variant } => {
                format!("variant `{variant}` payload does not match the pattern's shape")
            }
            PatErrorKind::BindingMismatch { name } => format!(
                "`{name}` is not bound with the same name and type by every `|` alternative"
            ),
            PatErrorKind::UnsupportedPattern(what) => {
                format!("{what} is not supported in a match pattern yet")
            }
            PatErrorKind::TooManyAlternatives { count, limit } => format!(
                "this pattern expands to {count} alternatives, more than the supported {limit}"
            ),
            PatErrorKind::TooDeep { depth, limit } => format!(
                "this pattern nests {depth} levels deep, more than the supported {limit}"
            ),
            PatErrorKind::CoverageLimit(detail) => {
                format!("the exhaustiveness check for this match is too large to prove: {detail}")
            }
            PatErrorKind::Layout(detail) => {
                format!("the matched type has no usable layout: {detail}")
            }
        }
    }
}

/// Resolves a bare pattern identifier to a build-time constant.
pub(crate) trait ConstResolver {
    fn resolve(&self, ident: &syn::Ident) -> Option<u128>;
}

/// A normalized pattern over a typed value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Pat {
    Wild,
    Bind(String),
    Bool(bool),
    /// An integer constant as the **bit pattern** of the matched type's width.
    Int(u128),
    /// An inclusive range of bit patterns, `lo..=hi`.
    Range {
        lo: u128,
        hi: u128,
    },
    /// A bit pattern with don't-care bits; `care` marks the bits that count.
    Mask {
        care: u128,
        value: u128,
        width: u32,
    },
    Variant {
        index: u32,
        payload: Option<Box<Pat>>,
    },
    Struct(Vec<Pat>),
    Array(Vec<Pat>),
}

/// One readable binding of a pattern. `Span` is not comparable, so no `PartialEq`.
#[derive(Clone, Debug)]
pub(crate) struct Binding {
    pub name: String,
    /// Bit offset inside the matched value (LSB is 0).
    pub offset: u32,
    pub ty: ValueType,
    pub span: Span,
}

/// One alternative of an arm; `a | b` produces two.
///
/// Bindings are per alternative: the same name may bind at a different offset.
#[derive(Clone, Debug)]
pub(crate) struct Alternative {
    pub pattern: Pat,
    pub bindings: Vec<Binding>,
}

#[derive(Clone, Debug)]
pub(crate) struct LoweredArm {
    pub alternatives: Vec<Alternative>,
}

/// Normalize one arm's pattern against the type it matches.
///
/// A bare identifier resolves to a constant, then to a payload-free variant of
/// the matched enum, then to a binding.
pub(crate) fn lower_arm(
    pat: &syn::Pat,
    ty: &ValueType,
    consts: &dyn ConstResolver,
) -> Result<LoweredArm, PatError> {
    let alternatives = lower_many(pat, ty, consts, 0, 0)?;
    check_binding_shape(pat.span(), &alternatives)?;
    Ok(LoweredArm { alternatives })
}

fn lower_many(
    pat: &syn::Pat,
    ty: &ValueType,
    consts: &dyn ConstResolver,
    base: u32,
    depth: usize,
) -> Result<Vec<Alternative>, PatError> {
    if depth > MAX_PATTERN_DEPTH {
        return Err(PatError::new(
            pat.span(),
            PatErrorKind::TooDeep {
                depth,
                limit: MAX_PATTERN_DEPTH,
            },
        ));
    }

    match pat {
        syn::Pat::Wild(_) => Ok(one(Pat::Wild)),

        syn::Pat::Ident(ident) => {
            if ident.subpat.is_some() {
                return Err(unsupported(pat, "`name @ pattern`"));
            }
            let name = ident.ident.to_string();
            if let Some(value) = consts.resolve(&ident.ident) {
                return Ok(one(const_pattern(value, ty, pat.span(), false)?));
            }
            if let ValueType::Enum { variants, .. } = ty {
                if let Some((index, variant)) = find_variant(variants, &name) {
                    if variant.payload.is_some() {
                        return Err(PatError::new(
                            pat.span(),
                            PatErrorKind::PayloadArity { variant: name },
                        ));
                    }
                    return Ok(one(Pat::Variant {
                        index: index as u32,
                        payload: None,
                    }));
                }
            }
            Ok(vec![Alternative {
                pattern: Pat::Bind(name.clone()),
                bindings: vec![Binding {
                    name,
                    offset: base,
                    ty: ty.clone(),
                    span: pat.span(),
                }],
            }])
        }

        syn::Pat::Path(path) => {
            if let ValueType::Enum { name, variants, .. } = ty {
                check_qualifier(name, &path.path, pat.span())?;
                if let Some(last) = path.path.segments.last() {
                    if let Some((index, variant)) = find_variant(variants, &last.ident.to_string())
                    {
                        if variant.payload.is_some() {
                            return Err(PatError::new(
                                pat.span(),
                                PatErrorKind::PayloadArity {
                                    variant: variant.name.clone(),
                                },
                            ));
                        }
                        return Ok(one(Pat::Variant {
                            index: index as u32,
                            payload: None,
                        }));
                    }
                }
            } else if path.path.segments.len() > 1 {
                return Err(PatError::new(
                    pat.span(),
                    PatErrorKind::NotAConstructor(format!(
                        "the path `{}`",
                        last_segment(&path.path).unwrap_or_default()
                    )),
                ));
            }
            if path.path.segments.len() == 1 {
                if let Some(value) = consts.resolve(&path.path.segments[0].ident) {
                    return Ok(one(const_pattern(value, ty, pat.span(), false)?));
                }
                return Err(PatError::new(
                    pat.span(),
                    PatErrorKind::UnknownName(path.path.segments[0].ident.to_string()),
                ));
            }
            Err(PatError::new(
                pat.span(),
                PatErrorKind::UnknownName(last_segment(&path.path).unwrap_or_default()),
            ))
        }

        syn::Pat::TupleStruct(tuple) => {
            let name = last_segment(&tuple.path).unwrap_or_default();
            let ValueType::Enum {
                name: enum_name,
                variants,
                ..
            } = ty
            else {
                return Err(PatError::new(
                    pat.span(),
                    PatErrorKind::NotAConstructor(format!("`{name}(…)`")),
                ));
            };
            check_qualifier(enum_name, &tuple.path, pat.span())?;
            let Some((index, variant)) = find_variant(variants, &name) else {
                return Err(PatError::new(pat.span(), PatErrorKind::UnknownName(name)));
            };
            let index = index as u32;
            match variant.payload.as_ref() {
                None => {
                    if !tuple.elems.is_empty() {
                        return Err(PatError::new(
                            pat.span(),
                            PatErrorKind::ArityMismatch {
                                expected: 0,
                                found: tuple.elems.len(),
                            },
                        ));
                    }
                    Ok(one(Pat::Variant {
                        index,
                        payload: None,
                    }))
                }
                Some(payload_ty) => {
                    let tag_width = tag_width_of(ty, pat.span())?;
                    match generated_aggregate(enum_name, &variant.name, payload_ty) {
                        // A generated multi-payload aggregate takes exactly its
                        // fields, `V(a, b)`, and is never one whole value.
                        Some(fields) if numeric_fields(fields) => {
                            if fields.len() != tuple.elems.len() {
                                return Err(PatError::new(
                                    pat.span(),
                                    PatErrorKind::ArityMismatch {
                                        expected: fields.len(),
                                        found: tuple.elems.len(),
                                    },
                                ));
                            }
                            let mut parts = Vec::with_capacity(tuple.elems.len());
                            for (position, elem) in tuple.elems.iter().enumerate() {
                                let (offset, field_ty) =
                                    struct_field_layout(payload_ty, position, pat.span())?;
                                parts.push(lower_many(
                                    elem,
                                    &field_ty,
                                    consts,
                                    base + tag_width + offset,
                                    depth + 1,
                                )?);
                            }
                            let parts = combine(pat.span(), parts, Pat::Struct)?;
                            Ok(wrap_variant(index, parts))
                        }
                        // A generated named aggregate is written `V { … }`, so
                        // the tuple form is refused whatever its arity.
                        Some(_) => Err(PatError::new(
                            pat.span(),
                            PatErrorKind::NotAConstructor(format!(
                                "variant `{}` names its payload fields; write `{} {{ … }}`",
                                variant.name, variant.name
                            )),
                        )),
                        // Any other payload is one value: a plain struct such as
                        // `Data(Packet)` or a builtin, bound whole by `V(value)`.
                        None => {
                            if tuple.elems.len() != 1 {
                                return Err(PatError::new(
                                    pat.span(),
                                    PatErrorKind::ArityMismatch {
                                        expected: 1,
                                        found: tuple.elems.len(),
                                    },
                                ));
                            }
                            let sub = tuple.elems.first().expect("length checked");
                            let payload =
                                lower_many(sub, payload_ty, consts, base + tag_width, depth + 1)?;
                            Ok(wrap_variant(index, payload))
                        }
                    }
                }
            }
        }

        syn::Pat::Struct(strukt) => {
            let name = last_segment(&strukt.path).unwrap_or_default();
            if let ValueType::Enum {
                name: enum_name,
                variants,
                ..
            } = ty
            {
                // `Msg::Pair { hi, lo }`: the variant's payload is the struct.
                check_qualifier(enum_name, &strukt.path, pat.span())?;
                let Some((index, variant)) = find_variant(variants, &name) else {
                    return Err(PatError::new(pat.span(), PatErrorKind::UnknownName(name)));
                };
                let Some(payload_ty) = variant.payload.as_ref() else {
                    return Err(PatError::new(
                        pat.span(),
                        PatErrorKind::PayloadArity { variant: name },
                    ));
                };
                let ValueType::Struct {
                    name: struct_name,
                    fields,
                    ..
                } = payload_ty.as_ref()
                else {
                    return Err(PatError::new(
                        pat.span(),
                        PatErrorKind::NotAConstructor(format!(
                            "a named-field pattern against `{name}`, whose payload is {}",
                            describe(payload_ty)
                        )),
                    ));
                };
                // The named form is only for a *generated* aggregate whose
                // fields have names. `Data(Packet)` keeps its own struct name
                // and is written `Data(value)` instead.
                match generated_aggregate(enum_name, &variant.name, payload_ty) {
                    Some(generated) if !numeric_fields(generated) => {}
                    Some(_) => {
                        return Err(PatError::new(
                            pat.span(),
                            PatErrorKind::NotAConstructor(format!(
                                "`{name} {{ … }}` names fields, but variant `{name}` carries \
                                 several payloads; write `{name}(a, b)`"
                            )),
                        ))
                    }
                    None => {
                        return Err(PatError::new(
                            pat.span(),
                            PatErrorKind::NotAConstructor(format!(
                                "`{name} {{ … }}` is only for a generated named payload; \
                                 `{name}` carries `{struct_name}`, so write `{name}(value)` and \
                                 select the fields of that value"
                            )),
                        ))
                    }
                }
                let tag_width = tag_width_of(ty, pat.span())?;
                let fields = fields.clone();
                let index = index as u32;
                let payload_ty = payload_ty.as_ref().clone();
                let parts = lower_struct_fields(
                    pat,
                    strukt,
                    &fields,
                    &payload_ty,
                    consts,
                    base + tag_width,
                    depth + 1,
                )?;
                Ok(wrap_variant(index, parts))
            } else {
                let ValueType::Struct {
                    name: struct_name,
                    fields,
                    ..
                } = ty
                else {
                    return Err(PatError::new(
                        pat.span(),
                        PatErrorKind::NotAConstructor("a struct pattern".to_string()),
                    ));
                };
                if !struct_name.is_empty() && name != *struct_name {
                    return Err(PatError::new(
                        pat.span(),
                        PatErrorKind::NotAConstructor(format!(
                            "`{name}` does not name struct `{struct_name}`"
                        )),
                    ));
                }
                let fields = fields.clone();
                lower_struct_fields(pat, strukt, &fields, ty, consts, base, depth + 1)
            }
        }

        syn::Pat::Slice(slice) => {
            let ValueType::Array { element, len } = ty else {
                return Err(PatError::new(
                    pat.span(),
                    PatErrorKind::NotAConstructor("an array pattern".to_string()),
                ));
            };
            let element_width = width_of(element, pat.span())?;
            let rest = slice
                .elems
                .iter()
                .position(|elem| matches!(elem, syn::Pat::Rest(_)));
            let rest_count = slice
                .elems
                .iter()
                .filter(|elem| matches!(elem, syn::Pat::Rest(_)))
                .count();
            if rest_count > 1 {
                return Err(unsupported(pat, "more than one `..` in an array pattern"));
            }
            // Checked before any slot is written: too many named elements would
            // otherwise move `position` backwards and silently overwrite a slot.
            let named = slice.elems.len() - rest_count;
            if named > *len {
                return Err(PatError::new(
                    pat.span(),
                    PatErrorKind::ArityMismatch {
                        expected: *len,
                        found: named,
                    },
                ));
            }
            let mut slots: Vec<Option<&syn::Pat>> = vec![None; *len];
            let mut position = 0usize;
            for (index, elem) in slice.elems.iter().enumerate() {
                if matches!(elem, syn::Pat::Rest(_)) {
                    // `named <= len`, so this cannot move `position` backwards.
                    position = len.saturating_sub(slice.elems.len() - index - 1);
                    continue;
                }
                if position >= *len {
                    return Err(PatError::new(
                        pat.span(),
                        PatErrorKind::ArityMismatch {
                            expected: *len,
                            found: position + 1,
                        },
                    ));
                }
                slots[position] = Some(elem);
                position += 1;
            }
            if rest.is_none() && position != *len {
                return Err(PatError::new(
                    pat.span(),
                    PatErrorKind::ArityMismatch {
                        expected: *len,
                        found: position,
                    },
                ));
            }
            let mut parts = Vec::with_capacity(*len);
            for (index, slot) in slots.iter().enumerate() {
                let offset = (index as u32).checked_mul(element_width).ok_or_else(|| {
                    PatError::new(
                        pat.span(),
                        PatErrorKind::CoverageLimit("an array offset overflows".to_string()),
                    )
                })?;
                parts.push(match slot {
                    Some(sub) => lower_many(sub, element, consts, base + offset, depth + 1)?,
                    None => one(Pat::Wild),
                });
            }
            combine(pat.span(), parts, Pat::Array)
        }

        syn::Pat::Or(or) => {
            let mut out = Vec::new();
            for case in &or.cases {
                out.extend(lower_many(case, ty, consts, base, depth)?);
                check_alternatives(pat.span(), out.len())?;
            }
            Ok(out)
        }

        syn::Pat::Lit(literal) => match &literal.lit {
            syn::Lit::Bool(value) => match ty {
                ValueType::Bool => Ok(one(Pat::Bool(value.value))),
                ValueType::UInt { width } if *width == 1 => {
                    Ok(one(Pat::Int(u128::from(value.value))))
                }
                _ => Err(PatError::new(
                    pat.span(),
                    PatErrorKind::LiteralOutOfRange {
                        width: width_of(ty, pat.span())?,
                    },
                )),
            },
            syn::Lit::Int(int) => {
                let (negative, magnitude) = parse_int_literal(int, pat.span())?;
                Ok(one(const_pattern(magnitude, ty, pat.span(), negative)?))
            }
            _ => Err(unsupported(pat, "this literal")),
        },

        syn::Pat::Paren(paren) => lower_many(&paren.pat, ty, consts, base, depth),
        syn::Pat::Type(typed) => lower_many(&typed.pat, ty, consts, base, depth),

        syn::Pat::Range(range) => lower_range(range, ty, base),
        syn::Pat::Reference(_) => Err(unsupported(pat, "a reference pattern")),
        syn::Pat::Tuple(_) => Err(unsupported(pat, "a tuple pattern")),
        syn::Pat::Rest(_) => Err(unsupported(pat, "`..` outside a struct or array pattern")),
        syn::Pat::Macro(mac) => lower_mask_macro(mac, ty, base),
        syn::Pat::Const(_) => Err(unsupported(pat, "a const-block pattern")),
        syn::Pat::Verbatim(_) => Err(unsupported(pat, "unparsed pattern text")),
        _ => Err(unsupported(pat, "this pattern")),
    }
}

/// An inclusive range over the bit pattern of an unsigned integer.
///
/// Ranges are the Rust-native way to say "these bits are this constant": the
/// bound is a bit pattern, and the endpoints are literals, so the check stays a
/// build-time constant and never depends on a run-time value.
fn lower_range(
    range: &syn::PatRange,
    ty: &ValueType,
    _base: u32,
) -> Result<Vec<Alternative>, PatError> {
    let span = range.span();
    let width = match ty {
        ValueType::UInt { width } => *width,
        _ => {
            return Err(unsupported(
                &syn::Pat::Range(range.clone()),
                "a range pattern on a non-integer value",
            ))
        }
    };
    if !matches!(range.limits, syn::RangeLimits::Closed(_)) {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(
                "an open range is ambiguous here; write `lo..=hi`".to_string(),
            ),
        ));
    }
    let Some(start) = range.start.as_deref() else {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(
                "an open range is ambiguous here; write `lo..=hi`".to_string(),
            ),
        ));
    };
    let Some(end) = range.end.as_deref() else {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(
                "an open range is ambiguous here; write `lo..=hi`".to_string(),
            ),
        ));
    };
    let lo = range_bound(start, width, span)?;
    let hi = range_bound(end, width, span)?;
    if lo > hi {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(format!("this range is empty: {lo} is above {hi}")),
        ));
    }
    Ok(one(Pat::Range { lo, hi }))
}

/// One endpoint of a range pattern: a non-negative integer literal that fits.
fn range_bound(expr: &syn::Expr, width: u32, span: Span) -> Result<u128, PatError> {
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Int(int),
        ..
    }) = expr
    else {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(
                "a range bound must be an integer literal".to_string(),
            ),
        ));
    };
    let (negative, value) = parse_int_literal(int, span)?;
    if negative {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(
                "a range bound on an unsigned value cannot be negative".to_string(),
            ),
        ));
    }
    if width > 128 || !fits_unsigned(value, width) {
        return Err(PatError::new(
            span,
            PatErrorKind::LiteralOutOfRange { width },
        ));
    }
    Ok(value)
}

/// A `mask!` pattern: a bit string in which `?` marks a bit that is not compared.
///
/// The literal text is read as a **bit string**, not as a number: `0b11??1111`
/// and `"11??1111"` both mean eight bits, the middle two ignored.
fn lower_mask_macro(
    mac: &syn::PatMacro,
    ty: &ValueType,
    _base: u32,
) -> Result<Vec<Alternative>, PatError> {
    let span = mac.span();
    let is_mask = mac
        .mac
        .path
        .segments
        .last()
        .map(|segment| segment.ident == "mask")
        .unwrap_or(false);
    if !is_mask {
        return Err(unsupported(
            &syn::Pat::Macro(mac.clone()),
            "a macro pattern other than `mask!(..)`",
        ));
    }
    let width = match ty {
        ValueType::UInt { width } => *width,
        _ => {
            return Err(unsupported(
                &syn::Pat::Macro(mac.clone()),
                "a mask pattern on a non-integer value",
            ))
        }
    };
    let bits = mask_bits(&mac.mac.tokens, span)?;
    if bits.len() as u32 != width {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(format!(
                "this mask has {} bits, but the value has {width}",
                bits.len()
            )),
        ));
    }
    let mut care = 0u128;
    let mut value = 0u128;
    for (index, bit) in bits.iter().enumerate() {
        let shift = bits.len() - 1 - index;
        match bit {
            '0' => care |= 1u128 << shift,
            '1' => {
                care |= 1u128 << shift;
                value |= 1u128 << shift;
            }
            '?' => {}
            _ => {
                return Err(PatError::new(
                    span,
                    PatErrorKind::UnsupportedPattern(
                        "a mask pattern may only contain 0, 1 and ?".to_string(),
                    ),
                ))
            }
        }
    }
    Ok(one(Pat::Mask { care, value, width }))
}

/// The characters of a mask literal, most significant bit first.
fn mask_bits(tokens: &proc_macro2::TokenStream, span: Span) -> Result<Vec<char>, PatError> {
    let mut text = String::new();
    for token in tokens.clone() {
        match token {
            proc_macro2::TokenTree::Literal(literal) => {
                let printed = literal.to_string();
                let stripped = if let Some(rest) = printed
                    .strip_prefix("0b")
                    .or_else(|| printed.strip_prefix("0B"))
                {
                    rest.to_string()
                } else if printed.starts_with('"') && printed.ends_with('"') && printed.len() >= 2 {
                    printed[1..printed.len() - 1].to_string()
                } else {
                    return Err(PatError::new(
                        span,
                        PatErrorKind::UnsupportedPattern(
                            "a mask literal is a binary bit string or a string".to_string(),
                        ),
                    ));
                };
                text.extend(stripped.chars().filter(|c| *c != '_'));
            }
            proc_macro2::TokenTree::Punct(punct) if punct.as_char() == '?' => text.push('?'),
            _ => {
                return Err(PatError::new(
                    span,
                    PatErrorKind::UnsupportedPattern(
                        "a mask pattern is `mask!(0b1?0????)` or `mask!(\"1?0????\")`".to_string(),
                    ),
                ))
            }
        }
    }
    if text.is_empty() || text.len() > 128 {
        return Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(
                "a mask pattern needs between 1 and 128 bits".to_string(),
            ),
        ));
    }
    Ok(text.chars().collect())
}

/// The runs of compared bits in a mask, least significant first.
///
/// `care` has a 1 for every bit that is compared; each maximal run becomes one
/// bit-select comparison, so the generated Verilog never touches a bit the
/// pattern does not name.
pub(crate) fn care_segments(care: u128, width: u32) -> Vec<(u32, u32)> {
    let mut segments = Vec::new();
    let mut index = 0;
    while index < width {
        if care & (1u128 << index) != 0 {
            let lo = index;
            while index < width && care & (1u128 << index) != 0 {
                index += 1;
            }
            segments.push((lo, index));
        } else {
            index += 1;
        }
    }
    segments
}

/// `width` one bits, for taking a field out of a mask's value.
pub(crate) fn bit_mask(width: u32) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

fn one(pattern: Pat) -> Vec<Alternative> {
    vec![Alternative {
        pattern,
        bindings: Vec::new(),
    }]
}

/// Wrap every payload alternative in the variant that carries it.
fn wrap_variant(index: u32, payload: Vec<Alternative>) -> Vec<Alternative> {
    payload
        .into_iter()
        .map(|alternative| Alternative {
            pattern: Pat::Variant {
                index,
                payload: Some(Box::new(alternative.pattern)),
            },
            bindings: alternative.bindings,
        })
        .collect()
}

/// Named-field patterns of a struct payload, shared by structs and struct
/// variants. `base` is the payload's offset inside the matched value.
fn lower_struct_fields(
    pat: &syn::Pat,
    strukt: &syn::PatStruct,
    fields: &[TypeField],
    struct_ty: &ValueType,
    consts: &dyn ConstResolver,
    base: u32,
    depth: usize,
) -> Result<Vec<Alternative>, PatError> {
    let mut slots: Vec<Option<&syn::Pat>> = vec![None; fields.len()];
    for field in &strukt.fields {
        let syn::Member::Named(member) = &field.member else {
            return Err(unsupported(pat, "a tuple struct pattern"));
        };
        let field_name = member.to_string();
        let Some(index) = fields.iter().position(|f| f.name == field_name) else {
            return Err(PatError::new(
                pat.span(),
                PatErrorKind::UnknownName(field_name),
            ));
        };
        if slots[index].is_some() {
            return Err(PatError::new(
                field.pat.span(),
                PatErrorKind::UnsupportedPattern(format!("the field `{field_name}` written twice")),
            ));
        }
        slots[index] = Some(&field.pat);
    }
    if strukt.rest.is_none() && slots.iter().any(|slot| slot.is_none()) {
        return Err(PatError::new(
            pat.span(),
            PatErrorKind::ArityMismatch {
                expected: fields.len(),
                found: slots.iter().filter(|slot| slot.is_some()).count(),
            },
        ));
    }
    let mut parts = Vec::with_capacity(fields.len());
    for (index, slot) in slots.iter().enumerate() {
        let (offset, field_ty) = struct_field_layout(struct_ty, index, pat.span())?;
        parts.push(match slot {
            Some(sub) => lower_many(sub, &field_ty, consts, base + offset, depth)?,
            None => one(Pat::Wild),
        });
    }
    combine(pat.span(), parts, Pat::Struct)
}

/// Cartesian product of the alternatives of every sub-position.
fn combine<F>(
    span: Span,
    parts: Vec<Vec<Alternative>>,
    build: F,
) -> Result<Vec<Alternative>, PatError>
where
    F: Fn(Vec<Pat>) -> Pat,
{
    let mut acc: Vec<(Vec<Pat>, Vec<Binding>)> = vec![(Vec::new(), Vec::new())];
    for part in parts {
        let mut next = Vec::new();
        for (patterns, bindings) in &acc {
            for alternative in &part {
                let mut patterns = patterns.clone();
                patterns.push(alternative.pattern.clone());
                let mut bindings = bindings.clone();
                bindings.extend(alternative.bindings.iter().cloned());
                next.push((patterns, bindings));
            }
        }
        check_alternatives(span, next.len())?;
        acc = next;
    }
    Ok(acc
        .into_iter()
        .map(|(patterns, bindings)| Alternative {
            pattern: build(patterns),
            bindings,
        })
        .collect())
}

fn check_alternatives(span: Span, count: usize) -> Result<(), PatError> {
    if count > MAX_ALTERNATIVES {
        return Err(PatError::new(
            span,
            PatErrorKind::TooManyAlternatives {
                count,
                limit: MAX_ALTERNATIVES,
            },
        ));
    }
    Ok(())
}

/// Every alternative must bind the same name/type; offsets may differ.
fn check_binding_shape(span: Span, alternatives: &[Alternative]) -> Result<(), PatError> {
    let mut shape: Option<BTreeMap<String, ValueType>> = None;
    for alternative in alternatives {
        let mut names: BTreeMap<String, ValueType> = BTreeMap::new();
        for binding in &alternative.bindings {
            if names
                .insert(binding.name.clone(), binding.ty.clone())
                .is_some()
            {
                return Err(PatError::new(
                    binding.span,
                    PatErrorKind::BindingMismatch {
                        name: binding.name.clone(),
                    },
                ));
            }
        }
        match &shape {
            None => shape = Some(names),
            Some(expected) if *expected != names => {
                let name = expected
                    .keys()
                    .chain(names.keys())
                    .find(|name| expected.get(*name) != names.get(*name))
                    .cloned()
                    .unwrap_or_else(|| "<binding>".to_string());
                return Err(PatError::new(span, PatErrorKind::BindingMismatch { name }));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn unsupported(pat: &syn::Pat, what: &str) -> PatError {
    PatError::new(
        pat.span(),
        PatErrorKind::UnsupportedPattern(what.to_string()),
    )
}

fn last_segment(path: &syn::Path) -> Option<String> {
    path.segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn find_variant<'a>(variants: &'a [EnumVariant], name: &str) -> Option<(usize, &'a EnumVariant)> {
    variants.iter().enumerate().find(|(_, v)| v.name == name)
}

/// `None` and `Option::None` both name a variant of enum `Option`; `Other::None`
/// never does, so a path from another enum cannot be mistaken for this one.
/// A predeclared enum carries its arguments in the metadata name
/// (`Option<uint<4>>`), so only the part before `<` is the root to compare.
fn check_qualifier(enum_name: &str, path: &syn::Path, span: Span) -> Result<(), PatError> {
    match path.segments.len() {
        1 => Ok(()),
        2 => {
            let qualifier = path.segments[0].ident.to_string();
            let root = enum_root(enum_name);
            if qualifier == root {
                Ok(())
            } else {
                Err(PatError::new(
                    span,
                    PatErrorKind::NotAConstructor(format!(
                        "`{qualifier}::…` does not belong to enum `{root}`"
                    )),
                ))
            }
        }
        count => Err(PatError::new(
            span,
            PatErrorKind::UnsupportedPattern(format!("a path with {count} segments")),
        )),
    }
}

/// The enum name without its arguments: `Option<uint<4>>` → `Option`.
fn enum_root(enum_name: &str) -> &str {
    match enum_name.split_once('<') {
        Some((root, _)) => root.trim(),
        None => enum_name,
    }
}

/// The fields of a *generated* payload aggregate, if this is one.
///
/// A variant written with several payloads (or with named payloads) is lowered
/// to a struct named `Enum::Variant`. A plain struct payload such as
/// `Data(Packet)` keeps its own name and is never treated as generated.
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

/// Sign and magnitude of an integer literal, up to `u128`.
///
/// A pattern's `-` is expected in the literal's own text; a `syn` version that
/// represents it otherwise reaches the caller's catch-all instead of guessing.
fn parse_int_literal(lit: &syn::LitInt, span: Span) -> Result<(bool, u128), PatError> {
    let text = lit.to_string();
    let suffix = lit.suffix();
    let digits = text.strip_suffix(suffix).unwrap_or(text.as_str());
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    let (negative, body) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, cleaned),
    };
    let (radix, body) =
        if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (16u32, rest.to_string())
        } else if let Some(rest) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
            (2, rest.to_string())
        } else if let Some(rest) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
            (8, rest.to_string())
        } else {
            (10, body)
        };
    let magnitude = u128::from_str_radix(&body, radix)
        .map_err(|_| PatError::new(span, PatErrorKind::LiteralOutOfRange { width: u128::BITS }))?;
    Ok((negative, magnitude))
}

/// An integer pattern is a *number*: it must fit the matched type's range, and a
/// negative one is encoded in two's complement at that width. A type wider than
/// 128 bits takes no integer pattern at all, only `_` or a binding.
fn const_pattern(value: u128, ty: &ValueType, span: Span, negative: bool) -> Result<Pat, PatError> {
    match ty {
        ValueType::Bool => {
            if negative || value > 1 {
                return Err(out_of_range(span, 1));
            }
            Ok(Pat::Bool(value == 1))
        }
        ValueType::UInt { width } => {
            if *width > 128 {
                return Err(wide_integer_pattern(span));
            }
            if negative || !fits_unsigned(value, *width) {
                return Err(out_of_range(span, *width));
            }
            Ok(Pat::Int(value))
        }
        ValueType::SInt { width } => {
            if *width > 128 {
                return Err(wide_integer_pattern(span));
            }
            let limit = signed_limit(*width);
            if negative {
                if value == 0 {
                    return Ok(Pat::Int(0));
                }
                if value > limit {
                    return Err(out_of_range(span, *width));
                }
                Ok(Pat::Int(if *width == 128 {
                    value.wrapping_neg()
                } else {
                    (1u128 << *width) - value
                }))
            } else if value < limit {
                Ok(Pat::Int(value))
            } else {
                Err(out_of_range(span, *width))
            }
        }
        other => Err(PatError::new(
            span,
            PatErrorKind::NotAConstructor(format!(
                "an integer pattern against {}",
                describe(other)
            )),
        )),
    }
}

fn out_of_range(span: Span, width: u32) -> PatError {
    PatError::new(span, PatErrorKind::LiteralOutOfRange { width })
}

fn wide_integer_pattern(span: Span) -> PatError {
    PatError::new(
        span,
        PatErrorKind::UnsupportedPattern(
            "an integer pattern against a type wider than 128 bits; use `_` or a binding"
                .to_string(),
        ),
    )
}

fn fits_unsigned(value: u128, width: u32) -> bool {
    width >= 128 || value < (1u128 << width)
}

/// `2^(width - 1)`: the exclusive upper bound of a signed positive value, and
/// the inclusive magnitude of a negative one.
fn signed_limit(width: u32) -> u128 {
    let shift = width.saturating_sub(1).min(127);
    1u128 << shift
}

fn describe(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "bool".to_string(),
        ValueType::UInt { width } => format!("uint<{width}>"),
        ValueType::SInt { width } => format!("sint<{width}>"),
        ValueType::Struct { name, .. } => format!("struct {name}"),
        ValueType::Array { len, .. } => format!("an array of {len}"),
        ValueType::Enum { name, .. } => format!("enum {name}"),
    }
}

fn width_of(ty: &ValueType, span: Span) -> Result<u32, PatError> {
    ty.bit_width()
        .map_err(|error| PatError::new(span, PatErrorKind::Layout(error.to_string())))
}

fn tag_width_of(ty: &ValueType, span: Span) -> Result<u32, PatError> {
    match ty {
        ValueType::Enum { tag_width, .. } => Ok(*tag_width),
        other => Err(PatError::new(
            span,
            PatErrorKind::NotAConstructor(format!("a variant pattern against {}", describe(other))),
        )),
    }
}

/// Bit offset and type of one struct field: declaration order, most significant
/// field first, no padding.
fn struct_field_layout(
    ty: &ValueType,
    index: usize,
    span: Span,
) -> Result<(u32, ValueType), PatError> {
    let ValueType::Struct { fields, .. } = ty else {
        return Err(PatError::new(
            span,
            PatErrorKind::NotAConstructor("a field pattern".to_string()),
        ));
    };
    let total = width_of(ty, span)?;
    let mut prefix = 0u32;
    for (position, field) in fields.iter().enumerate() {
        let width = width_of(&field.ty, span)?;
        if position == index {
            let offset = total
                .checked_sub(prefix)
                .and_then(|rest| rest.checked_sub(width));
            let Some(offset) = offset else {
                return Err(PatError::new(
                    span,
                    PatErrorKind::Layout("struct field offsets do not fit".to_string()),
                ));
            };
            return Ok((offset, field.ty.clone()));
        }
        prefix = prefix.checked_add(width).ok_or_else(|| {
            PatError::new(
                span,
                PatErrorKind::Layout("struct field offsets overflow".to_string()),
            )
        })?;
    }
    Err(PatError::new(
        span,
        PatErrorKind::ArityMismatch {
            expected: fields.len(),
            found: index + 1,
        },
    ))
}

/// True when the tag width leaves unused encodings. A tag of 64 bits or more
/// always does, because a variant list cannot enumerate that many tags.
fn has_tag_holes(tag_width: u32, variants: usize) -> bool {
    if tag_width >= 64 {
        return true;
    }
    (variants as u64) != (1u64 << tag_width)
}

/// Whether the values an arm covers make the match total.
#[derive(Clone, Debug)]
pub(crate) struct Coverage {
    ty: ValueType,
    rows: Vec<Vec<Pat>>,
}

impl Coverage {
    pub(crate) fn new(ty: &ValueType) -> Self {
        Self {
            ty: ty.clone(),
            rows: Vec::new(),
        }
    }

    /// Add one arm's unguarded alternatives.
    pub(crate) fn add(&mut self, alternatives: &[Alternative]) -> Result<(), PatError> {
        let span = Span::call_site();
        if self.rows.len() + alternatives.len() > MAX_MATRIX_ROWS {
            return Err(PatError::new(
                span,
                PatErrorKind::CoverageLimit(format!("more than {MAX_MATRIX_ROWS} rows")),
            ));
        }
        for alternative in alternatives {
            self.rows.push(vec![alternative.pattern.clone()]);
        }
        Ok(())
    }

    /// True when every value of the type is covered.
    ///
    /// A match without `_` can be exhaustive, for example
    /// `None | Some(true) | Some(false)` for `Option<bool>`.
    pub(crate) fn is_exhaustive(&self) -> Result<bool, PatError> {
        // Integer columns are decided as an interval union: literals are points,
        // ranges are intervals, and a wildcard covers the rest. That is exact
        // and cheap, and it is what makes `0x00..=0x0F | 0x10..=0x1F` total.
        if let Some(total) = self.integer_domain_is_exhaustive()? {
            return Ok(total);
        }
        let span = Span::call_site();
        let candidate = vec![Pat::Wild];
        let mut work = 0usize;
        Ok(!useful(
            &self.rows,
            &candidate,
            std::slice::from_ref(&self.ty),
            span,
            &mut work,
        )?)
    }

    /// Whether an integer column is covered, as an exact interval union.
    ///
    /// `None` means the question is not an integer-column question, so the
    /// general matrix decides. `Some(false)` is returned when a `mask!` pattern is
    /// present: a bit-level cover is not proven here, so such a match needs a
    /// wildcard arm instead of a claim this code cannot justify.
    fn integer_domain_is_exhaustive(&self) -> Result<Option<bool>, PatError> {
        let width = match &self.ty {
            ValueType::UInt { width } | ValueType::SInt { width } => *width,
            _ => return Ok(None),
        };
        let mut intervals: Vec<(u128, u128)> = Vec::new();
        for row in &self.rows {
            if row.len() != 1 {
                return Ok(None);
            }
            match &row[0] {
                Pat::Wild | Pat::Bind(_) => return Ok(Some(true)),
                Pat::Int(value) => intervals.push((*value, *value)),
                Pat::Range { lo, hi } => intervals.push((*lo, *hi)),
                Pat::Mask { .. } => return Ok(Some(false)),
                _ => return Ok(None),
            }
        }
        intervals.sort_unstable();
        let limit = if width >= 128 {
            u128::MAX
        } else {
            (1u128 << width) - 1
        };
        let mut covered: Option<u128> = None;
        for (lo, hi) in intervals {
            match covered {
                None => {
                    if lo > 0 {
                        return Ok(Some(false));
                    }
                    covered = Some(hi);
                }
                Some(end) => {
                    if lo > end.saturating_add(1) {
                        return Ok(Some(false));
                    }
                    if hi > end {
                        covered = Some(hi);
                    }
                }
            }
            if covered == Some(limit) {
                return Ok(Some(true));
            }
        }
        Ok(Some(covered == Some(limit)))
    }

    /// Best-effort hint for a diagnostic; the verdict is [`Self::is_exhaustive`].
    pub(crate) fn missing(&self, limit: usize) -> Result<Vec<Missing>, PatError> {
        let span = Span::call_site();
        let tys = std::slice::from_ref(&self.ty);
        let mut work = 0usize;
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        match &self.ty {
            ValueType::Bool => {
                for value in [false, true] {
                    if out.len() >= limit {
                        break;
                    }
                    if useful(&self.rows, &[Pat::Bool(value)], tys, span, &mut work)? {
                        out.push(Missing::Bool(value));
                    }
                }
            }
            ValueType::UInt { .. } | ValueType::SInt { .. } => {
                // With `k` constants covered, some value in `0..=k` is uncovered,
                // so the domain is never enumerated.
                let constants: Vec<u128> = self
                    .rows
                    .iter()
                    .filter_map(|row| match row.first() {
                        Some(Pat::Int(value)) => Some(*value),
                        _ => None,
                    })
                    .collect();
                let mut choice = None;
                for value in 0..=(constants.len() as u128) {
                    if !constants.contains(&value) {
                        choice = Some(value);
                        break;
                    }
                }
                if let Some(value) = choice {
                    if useful(&self.rows, &[Pat::Int(value)], tys, span, &mut work)? {
                        out.push(Missing::Int(value));
                    }
                }
            }
            ValueType::Enum {
                tag_width,
                variants,
                ..
            } => {
                for (index, variant) in variants.iter().enumerate() {
                    if out.len() >= limit {
                        break;
                    }
                    let candidate = vec![Pat::Variant {
                        index: index as u32,
                        payload: variant.payload.as_ref().map(|_| Box::new(Pat::Wild)),
                    }];
                    if useful(&self.rows, &candidate, tys, span, &mut work)? {
                        out.push(Missing::Variant {
                            index: index as u32,
                            name: variant.name.clone(),
                        });
                    }
                }
                if out.is_empty() && has_tag_holes(*tag_width, variants.len()) {
                    out.push(Missing::Invalid);
                }
            }
            ValueType::Struct { .. } | ValueType::Array { .. } => {}
        }
        Ok(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Missing {
    Bool(bool),
    Int(u128),
    Variant { index: u32, name: String },
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ctor {
    Bool(bool),
    Int(u128),
    Variant(u32),
    /// An unused tag encoding; only `Wild`/`Bind` specialize it.
    Invalid,
    Struct,
    Array,
}

fn check_size(rows: usize, columns: usize, span: Span) -> Result<(), PatError> {
    if columns > MAX_MATRIX_COLUMNS {
        return Err(PatError::new(
            span,
            PatErrorKind::CoverageLimit(format!(
                "more than {MAX_MATRIX_COLUMNS} parallel columns to check"
            )),
        ));
    }
    if rows > MAX_MATRIX_ROWS || rows.saturating_mul(columns) > MAX_MATRIX_CELLS {
        return Err(PatError::new(
            span,
            PatErrorKind::CoverageLimit(format!("more than {MAX_MATRIX_CELLS} matrix cells")),
        ));
    }
    Ok(())
}

/// Whether `candidate` matches a value no row of `rows` matches.
///
/// `work` accumulates the cells visited, so a pathological match fails with an
/// explicit [`PatErrorKind::CoverageLimit`] instead of running away. Columns are
/// work, not nesting: there is no per-column depth count here.
fn useful(
    rows: &[Vec<Pat>],
    candidate: &[Pat],
    tys: &[ValueType],
    span: Span,
    work: &mut usize,
) -> Result<bool, PatError> {
    check_size(rows.len(), tys.len(), span)?;
    // A range or a mask is decided by the integer interval path, not here.
    if rows.iter().any(|row| {
        row.iter()
            .any(|p| matches!(p, Pat::Range { .. } | Pat::Mask { .. }))
    }) {
        return Ok(true);
    }
    // A row that is `Wild`/`Bind` in every remaining column matches every value
    // of this product, so nothing below it can be new. This is what keeps a
    // flat, wide type cheap.
    if rows
        .iter()
        .any(|row| row.iter().all(|p| matches!(p, Pat::Wild | Pat::Bind(_))))
    {
        return Ok(false);
    }
    *work = work.saturating_add(rows.len().max(1).saturating_mul(tys.len().max(1)));
    if *work > MAX_COVERAGE_WORK {
        return Err(PatError::new(
            span,
            PatErrorKind::CoverageLimit(format!(
                "more than {MAX_COVERAGE_WORK} matrix cells visited"
            )),
        ));
    }
    if tys.is_empty() || candidate.is_empty() {
        return Ok(rows.is_empty());
    }
    let ty = &tys[0];
    let first = &candidate[0];
    let scalar = matches!(ty, ValueType::UInt { .. } | ValueType::SInt { .. });

    if matches!(first, Pat::Wild | Pat::Bind(_)) {
        if !column_is_complete(rows, ty) {
            return Ok(true);
        }
        // Every typed constructor, not just the ones the rows name: a wildcard
        // row also spans the product structure that lives below this column.
        for ctor in complete_ctors(rows, ty) {
            let (sub_rows, sub_types) = specialize(rows, tys, &ctor, span)?;
            let sub_candidate = specialize_candidate(candidate, &ctor, ty, span)?;
            if useful(&sub_rows, &sub_candidate, &sub_types, span, work)? {
                return Ok(true);
            }
        }
        // A scalar's constants leave values no row names; the wildcard rows are
        // the only thing covering them.
        if scalar && column_has_wild(rows) {
            let (sub_rows, sub_types) = default_matrix(rows, tys);
            let sub_candidate = candidate[1..].to_vec();
            return useful(&sub_rows, &sub_candidate, &sub_types, span, work);
        }
        return Ok(false);
    }

    let Some(ctor) = ctor_of(first) else {
        return Err(PatError::new(
            span,
            PatErrorKind::CoverageLimit("a wildcard reached the constructor path".to_string()),
        ));
    };
    let (sub_rows, sub_types) = specialize(rows, tys, &ctor, span)?;
    let sub_candidate = specialize_candidate(candidate, &ctor, ty, span)?;
    useful(&sub_rows, &sub_candidate, &sub_types, span, work)
}

fn ctor_of(pat: &Pat) -> Option<Ctor> {
    match pat {
        Pat::Bool(value) => Some(Ctor::Bool(*value)),
        Pat::Int(value) => Some(Ctor::Int(*value)),
        Pat::Variant { index, .. } => Some(Ctor::Variant(*index)),
        Pat::Struct(_) => Some(Ctor::Struct),
        Pat::Array(_) => Some(Ctor::Array),
        // Ranges and masks are decided by the integer interval path; the matrix
        // never sees them, so they have no constructor of their own.
        Pat::Range { .. } | Pat::Mask { .. } => None,
        Pat::Wild | Pat::Bind(_) => None,
    }
}

fn column_has_wild(rows: &[Vec<Pat>]) -> bool {
    rows.iter()
        .any(|row| matches!(row.first(), Some(Pat::Wild) | Some(Pat::Bind(_))))
}

fn constants_in_column(rows: &[Vec<Pat>]) -> Vec<Ctor> {
    let mut out = Vec::new();
    for row in rows {
        if let Some(Pat::Int(value)) = row.first() {
            let ctor = Ctor::Int(*value);
            if !out.contains(&ctor) {
                out.push(ctor);
            }
        }
    }
    out
}

/// The full constructor set of a column's type, used once the column is
/// complete so the algorithm descends into every sub-domain.
fn complete_ctors(rows: &[Vec<Pat>], ty: &ValueType) -> Vec<Ctor> {
    match ty {
        ValueType::Bool => vec![Ctor::Bool(false), Ctor::Bool(true)],
        ValueType::Enum {
            tag_width,
            variants,
            ..
        } => {
            let mut out: Vec<Ctor> = (0..variants.len() as u32).map(Ctor::Variant).collect();
            if has_tag_holes(*tag_width, variants.len()) {
                out.push(Ctor::Invalid);
            }
            out
        }
        ValueType::Struct { .. } => vec![Ctor::Struct],
        ValueType::Array { .. } => vec![Ctor::Array],
        ValueType::UInt { .. } | ValueType::SInt { .. } => constants_in_column(rows),
    }
}

/// The number of values a scalar domain holds, when constants could cover it.
fn small_domain_size(ty: &ValueType) -> Option<u128> {
    let width = match ty {
        ValueType::UInt { width } | ValueType::SInt { width } => *width,
        _ => return None,
    };
    if width >= 128 {
        None
    } else {
        Some(1u128 << width)
    }
}

/// Whether the column covers every constructor of its type.
fn column_is_complete(rows: &[Vec<Pat>], ty: &ValueType) -> bool {
    let wild = column_has_wild(rows);
    match ty {
        ValueType::Bool => {
            let mut seen = [false, false];
            for row in rows {
                if let Some(Pat::Bool(value)) = row.first() {
                    seen[usize::from(*value)] = true;
                }
            }
            wild || (seen[0] && seen[1])
        }
        ValueType::Enum {
            tag_width,
            variants,
            ..
        } => {
            let mut seen = vec![false; variants.len()];
            for row in rows {
                if let Some(Pat::Variant { index, .. }) = row.first() {
                    if let Some(slot) = seen.get_mut(*index as usize) {
                        *slot = true;
                    }
                }
            }
            // A wildcard row covers the named variants *and* the tag holes.
            wild || (seen.iter().all(|slot| *slot) && !has_tag_holes(*tag_width, variants.len()))
        }
        ValueType::Struct { .. } | ValueType::Array { .. } => {
            wild || rows
                .iter()
                .any(|row| matches!(row.first(), Some(Pat::Struct(_)) | Some(Pat::Array(_))))
        }
        ValueType::UInt { .. } | ValueType::SInt { .. } => {
            if wild {
                return true;
            }
            match small_domain_size(ty) {
                Some(size) => constants_in_column(rows).len() as u128 == size,
                None => false,
            }
        }
    }
}

fn default_matrix(rows: &[Vec<Pat>], tys: &[ValueType]) -> (Vec<Vec<Pat>>, Vec<ValueType>) {
    let keep: Vec<Vec<Pat>> = rows
        .iter()
        .filter(|row| matches!(row.first(), Some(Pat::Wild) | Some(Pat::Bind(_))))
        .map(|row| row[1..].to_vec())
        .collect();
    (keep, tys[1..].to_vec())
}

fn specialize(
    rows: &[Vec<Pat>],
    tys: &[ValueType],
    ctor: &Ctor,
    span: Span,
) -> Result<(Vec<Vec<Pat>>, Vec<ValueType>), PatError> {
    let ty = &tys[0];
    let mut out = Vec::new();
    for row in rows {
        if let Some(mut sub) = specialize_pattern(&row[0], ctor, ty, span)? {
            sub.extend_from_slice(&row[1..]);
            out.push(sub);
        }
    }
    let mut types = ctor_types(ctor, ty, span)?;
    types.extend_from_slice(&tys[1..]);
    check_size(out.len(), types.len(), span)?;
    Ok((out, types))
}

fn specialize_candidate(
    candidate: &[Pat],
    ctor: &Ctor,
    ty: &ValueType,
    span: Span,
) -> Result<Vec<Pat>, PatError> {
    let mut sub = specialize_pattern(&candidate[0], ctor, ty, span)?.unwrap_or_default();
    sub.extend_from_slice(&candidate[1..]);
    Ok(sub)
}

fn specialize_pattern(
    pat: &Pat,
    ctor: &Ctor,
    ty: &ValueType,
    span: Span,
) -> Result<Option<Vec<Pat>>, PatError> {
    match pat {
        Pat::Wild | Pat::Bind(_) => Ok(Some(vec![Pat::Wild; arity_of(ctor, ty, span)?])),
        Pat::Bool(value) => Ok(match ctor {
            Ctor::Bool(other) if other == value => Some(Vec::new()),
            _ => None,
        }),
        Pat::Int(value) => Ok(match ctor {
            Ctor::Int(other) if other == value => Some(Vec::new()),
            _ => None,
        }),
        // Ranges and masks never reach the matrix; the integer interval path
        // decides them before it runs.
        Pat::Range { .. } | Pat::Mask { .. } => Ok(None),
        Pat::Variant { index, payload } => match ctor {
            Ctor::Variant(other) if other == index => {
                let sub = match payload {
                    Some(payload) => vec![(**payload).clone()],
                    None => vec![Pat::Wild; variant_arity(ty, *index, span)?],
                };
                Ok(Some(sub))
            }
            _ => Ok(None),
        },
        Pat::Struct(parts) => Ok(match ctor {
            Ctor::Struct => Some(parts.clone()),
            _ => None,
        }),
        Pat::Array(parts) => Ok(match ctor {
            Ctor::Array => Some(parts.clone()),
            _ => None,
        }),
    }
}

fn arity_of(ctor: &Ctor, ty: &ValueType, span: Span) -> Result<usize, PatError> {
    match ctor {
        Ctor::Bool(_) | Ctor::Int(_) | Ctor::Invalid => Ok(0),
        Ctor::Variant(index) => variant_arity(ty, *index, span),
        Ctor::Struct => match ty {
            ValueType::Struct { fields, .. } => Ok(fields.len()),
            other => Err(coverage_mismatch("a struct constructor", other, span)),
        },
        Ctor::Array => match ty {
            ValueType::Array { len, .. } => Ok(*len),
            other => Err(coverage_mismatch("an array constructor", other, span)),
        },
    }
}

fn variant_arity(ty: &ValueType, index: u32, span: Span) -> Result<usize, PatError> {
    let ValueType::Enum { variants, .. } = ty else {
        return Err(coverage_mismatch("a variant constructor", ty, span));
    };
    let Some(variant) = variants.get(index as usize) else {
        return Err(PatError::new(
            span,
            PatErrorKind::CoverageLimit(format!("variant index {index} is out of range")),
        ));
    };
    Ok(usize::from(variant.payload.is_some()))
}

fn ctor_types(ctor: &Ctor, ty: &ValueType, span: Span) -> Result<Vec<ValueType>, PatError> {
    match ctor {
        Ctor::Bool(_) | Ctor::Int(_) | Ctor::Invalid => Ok(Vec::new()),
        Ctor::Variant(index) => {
            let ValueType::Enum { variants, .. } = ty else {
                return Err(coverage_mismatch("a variant constructor", ty, span));
            };
            let Some(variant) = variants.get(*index as usize) else {
                return Err(PatError::new(
                    span,
                    PatErrorKind::CoverageLimit(format!("variant index {index} is out of range")),
                ));
            };
            Ok(match &variant.payload {
                Some(payload) => vec![(**payload).clone()],
                None => Vec::new(),
            })
        }
        Ctor::Struct => match ty {
            ValueType::Struct { fields, .. } => {
                Ok(fields.iter().map(|field| field.ty.clone()).collect())
            }
            other => Err(coverage_mismatch("a struct constructor", other, span)),
        },
        Ctor::Array => match ty {
            ValueType::Array { element, len } => Ok(vec![(**element).clone(); *len]),
            other => Err(coverage_mismatch("an array constructor", other, span)),
        },
    }
}

fn coverage_mismatch(what: &str, ty: &ValueType, span: Span) -> PatError {
    PatError::new(
        span,
        PatErrorKind::CoverageLimit(format!("{what} was specialized against {}", describe(ty))),
    )
}

/// The test one alternative names, for a caller to render in its own dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Test {
    AlwaysTrue,
    /// `value[offset +: width] == value`.
    Bits {
        offset: u32,
        width: u32,
        value: u128,
    },
    /// `lo <= value[offset +: width] <= hi`.
    Range {
        offset: u32,
        width: u32,
        lo: u128,
        hi: u128,
    },
    /// The bits `care` selects must equal `value`; the others are ignored.
    Mask {
        offset: u32,
        width: u32,
        care: u128,
        value: u128,
    },
    All(Vec<Test>),
}

pub(crate) fn pattern_test(alternative: &Alternative, ty: &ValueType) -> Result<Test, PatError> {
    test_of(&alternative.pattern, ty, 0)
}

/// The whole-value bit pattern one test requires, as `(value, care)`.
///
/// A `case`/`casez` compares the whole scrutinee, so a test that names only some
/// bits says the rest do not matter, and every test of one arm is folded into one
/// pattern: `care` says which bits are compared, `value` what they must be.
///
/// `None` means no `case` arm can spell the test - a range, or a pattern wider
/// than the 128-bit constants the MIR carries - and the `match` keeps its `if`
/// chain.
pub(crate) fn case_pattern(test: &Test) -> Option<(u128, u128)> {
    match test {
        Test::AlwaysTrue => Some((0, 0)),
        Test::Bits {
            offset,
            width,
            value,
        } => {
            let care = placed_bits(bit_mask(*width), *offset, *width)?;
            Some(((bit_mask(*width) & *value) << offset, care))
        }
        Test::Mask {
            offset,
            width,
            care,
            value,
        } => {
            let placed = placed_bits(*care, *offset, *width)?;
            Some(((bit_mask(*width) & *value) << offset, placed))
        }
        Test::Range { .. } => None,
        Test::All(tests) => {
            let mut value = 0u128;
            let mut care = 0u128;
            for test in tests {
                let (bits, wanted) = case_pattern(test)?;
                // Two parts of one pattern may not disagree about a bit they both
                // name; one that does is not a pattern at all.
                if (value ^ bits) & (care & wanted) != 0 {
                    return None;
                }
                value = (value & !wanted) | bits;
                care |= wanted;
            }
            Some((value, care))
        }
    }
}

/// `bits` moved to `offset`, when the result still fits the 128-bit constants.
fn placed_bits(bits: u128, offset: u32, width: u32) -> Option<u128> {
    if width > 128 || offset.checked_add(width)? > 128 {
        return None;
    }
    Some(bits << offset)
}

fn test_of(pat: &Pat, ty: &ValueType, offset: u32) -> Result<Test, PatError> {
    let span = Span::call_site();
    match pat {
        Pat::Wild | Pat::Bind(_) => Ok(Test::AlwaysTrue),
        Pat::Bool(value) => Ok(Test::Bits {
            offset,
            width: 1,
            value: u128::from(*value),
        }),
        Pat::Int(value) => Ok(Test::Bits {
            offset,
            width: width_of(ty, span)?,
            value: *value,
        }),
        Pat::Range { lo, hi } => Ok(Test::Range {
            offset,
            width: width_of(ty, span)?,
            lo: *lo,
            hi: *hi,
        }),
        Pat::Mask { care, value, width } => {
            let ty_width = width_of(ty, span)?;
            if *width != ty_width {
                return Err(PatError::new(
                    span,
                    PatErrorKind::UnsupportedPattern(format!(
                        "a mask pattern needs {ty_width} bits to match this type, not {width}"
                    )),
                ));
            }
            Ok(Test::Mask {
                offset,
                width: *width,
                care: *care,
                value: *value,
            })
        }
        Pat::Variant { index, payload } => {
            let ValueType::Enum {
                tag_width,
                variants,
                ..
            } = ty
            else {
                return Err(PatError::new(
                    span,
                    PatErrorKind::NotAConstructor("a variant pattern".to_string()),
                ));
            };
            let Some(variant) = variants.get(*index as usize) else {
                return Err(PatError::new(
                    span,
                    PatErrorKind::CoverageLimit(format!("variant index {index} is out of range")),
                ));
            };
            let mut tests = vec![Test::Bits {
                offset,
                width: *tag_width,
                value: u128::from(variant.tag),
            }];
            if let (Some(payload), Some(payload_ty)) = (payload, variant.payload.as_ref()) {
                let sub = test_of(payload, payload_ty, offset + *tag_width)?;
                if sub != Test::AlwaysTrue {
                    tests.push(sub);
                }
            }
            Ok(if tests.len() == 1 {
                tests.pop().expect("one test")
            } else {
                Test::All(tests)
            })
        }
        Pat::Struct(parts) => {
            let mut tests = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let (field_offset, field_ty) = struct_field_layout(ty, index, span)?;
                let test = test_of(part, &field_ty, offset + field_offset)?;
                if test != Test::AlwaysTrue {
                    tests.push(test);
                }
            }
            Ok(if tests.is_empty() {
                Test::AlwaysTrue
            } else {
                Test::All(tests)
            })
        }
        Pat::Array(parts) => {
            let ValueType::Array { element, len } = ty else {
                return Err(PatError::new(
                    span,
                    PatErrorKind::NotAConstructor("an array pattern".to_string()),
                ));
            };
            if parts.len() != *len {
                return Err(PatError::new(
                    span,
                    PatErrorKind::ArityMismatch {
                        expected: *len,
                        found: parts.len(),
                    },
                ));
            }
            let element_width = width_of(element, span)?;
            let mut tests = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let element_offset =
                    (index as u32).checked_mul(element_width).ok_or_else(|| {
                        PatError::new(
                            span,
                            PatErrorKind::CoverageLimit("an array offset overflows".to_string()),
                        )
                    })?;
                let test = test_of(part, element, offset + element_offset)?;
                if test != Test::AlwaysTrue {
                    tests.push(test);
                }
            }
            Ok(if tests.is_empty() {
                Test::AlwaysTrue
            } else {
                Test::All(tests)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoConsts;

    impl ConstResolver for NoConsts {
        fn resolve(&self, _ident: &syn::Ident) -> Option<u128> {
            None
        }
    }

    fn uint(width: u32) -> ValueType {
        ValueType::UInt { width }
    }

    fn sint(width: u32) -> ValueType {
        ValueType::SInt { width }
    }

    fn variant(name: &str, tag: u64, payload: Option<ValueType>) -> EnumVariant {
        EnumVariant {
            name: name.to_string(),
            tag,
            payload: payload.map(Box::new),
        }
    }

    fn option(inner: ValueType) -> ValueType {
        ValueType::Enum {
            name: "Option".to_string(),
            tag_width: 1,
            variants: vec![variant("None", 0, None), variant("Some", 1, Some(inner))],
        }
    }

    fn pair_struct() -> ValueType {
        ValueType::Struct {
            name: "Pair".to_string(),
            fields: vec![
                TypeField {
                    name: "hi".to_string(),
                    ty: ValueType::Bool,
                },
                TypeField {
                    name: "lo".to_string(),
                    ty: ValueType::Bool,
                },
            ],
        }
    }

    /// A struct of `fields` booleans named `f0..n-1`.
    fn struct_of_bools(fields: usize) -> ValueType {
        ValueType::Struct {
            name: "Bools".to_string(),
            fields: (0..fields)
                .map(|index| TypeField {
                    name: format!("f{index}"),
                    ty: ValueType::Bool,
                })
                .collect(),
        }
    }

    /// `Msg` has a payload-free variant and a variant whose payload is the
    /// *generated* named aggregate `Msg::Pair`, written `Msg::Pair { … }`.
    fn msg() -> ValueType {
        ValueType::Enum {
            name: "Msg".to_string(),
            tag_width: 1,
            variants: vec![
                variant("Idle", 0, None),
                variant(
                    "Pair",
                    1,
                    Some(ValueType::Struct {
                        name: "Msg::Pair".to_string(),
                        fields: vec![
                            TypeField {
                                name: "hi".to_string(),
                                ty: ValueType::Bool,
                            },
                            TypeField {
                                name: "lo".to_string(),
                                ty: ValueType::Bool,
                            },
                        ],
                    }),
                ),
            ],
        }
    }

    /// `syn` has no `Parse` impl for `Pat` (a pattern is ambiguous alone), so
    /// the tests parse through `Pat::parse_multi`, as the frontend does.
    fn parse_pattern(text: &str) -> syn::Pat {
        use syn::parse::Parser;
        syn::Pat::parse_multi
            .parse_str(text)
            .expect("parse pattern")
    }

    fn lower(text: &str, ty: &ValueType) -> LoweredArm {
        lower_arm(&parse_pattern(text), ty, &NoConsts).expect("lower pattern")
    }

    fn lower_err(text: &str, ty: &ValueType) -> PatErrorKind {
        lower_arm(&parse_pattern(text), ty, &NoConsts)
            .expect_err("expected a lowering error")
            .kind
    }

    fn cover(ty: &ValueType, patterns: &[&str]) -> Coverage {
        let mut coverage = Coverage::new(ty);
        for text in patterns {
            coverage
                .add(&lower(text, ty).alternatives)
                .expect("add alternatives");
        }
        coverage
    }

    #[test]
    fn option_bool_three_arms_are_exhaustive_without_a_wildcard() {
        let ty = option(ValueType::Bool);
        let coverage = cover(&ty, &["None", "Some(true)", "Some(false)"]);
        assert!(coverage.is_exhaustive().unwrap());
    }

    #[test]
    fn option_bool_missing_a_payload_value_is_not_exhaustive() {
        let ty = option(ValueType::Bool);
        let coverage = cover(&ty, &["None", "Some(true)"]);
        assert!(!coverage.is_exhaustive().unwrap());
        assert_eq!(
            coverage.missing(2).unwrap(),
            vec![Missing::Variant {
                index: 1,
                name: "Some".to_string()
            }]
        );
    }

    #[test]
    fn option_with_two_variants_has_no_invalid_tag() {
        let ty = option(uint(8));
        let coverage = cover(&ty, &["None", "Some(_)"]);
        assert!(coverage.is_exhaustive().unwrap());
    }

    #[test]
    fn an_enum_with_a_tag_hole_needs_a_wildcard() {
        let ty = ValueType::Enum {
            name: "State".to_string(),
            tag_width: 2,
            variants: vec![
                variant("Idle", 0, None),
                variant("Run", 1, None),
                variant("Halt", 2, None),
            ],
        };
        let coverage = cover(&ty, &["State::Idle", "State::Run", "State::Halt"]);
        assert!(!coverage.is_exhaustive().unwrap());
        assert_eq!(coverage.missing(1).unwrap(), vec![Missing::Invalid]);
        let with_wild = cover(&ty, &["State::Idle", "State::Run", "State::Halt", "_"]);
        assert!(with_wild.is_exhaustive().unwrap());
    }

    #[test]
    fn a_small_unsigned_domain_is_covered_by_its_constants() {
        let one = uint(1);
        assert!(cover(&one, &["0", "1"]).is_exhaustive().unwrap());
        assert!(!cover(&one, &["0"]).is_exhaustive().unwrap());

        let two = uint(2);
        assert!(cover(&two, &["0", "1", "2", "3"]).is_exhaustive().unwrap());
        assert!(!cover(&two, &["0", "1", "2"]).is_exhaustive().unwrap());
    }

    #[test]
    fn a_large_unsigned_domain_is_not_exhausted_by_constants() {
        let wide = uint(8);
        assert!(!cover(&wide, &["0", "1", "2", "3"]).is_exhaustive().unwrap());
        assert!(cover(&wide, &["0", "1", "_"]).is_exhaustive().unwrap());
    }

    #[test]
    fn struct_fields_are_a_product() {
        let ty = pair_struct();
        let coverage = cover(
            &ty,
            &[
                "Pair { hi: false, lo: false }",
                "Pair { hi: false, lo: true }",
                "Pair { hi: true, lo: false }",
                "Pair { hi: true, lo: true }",
            ],
        );
        assert!(coverage.is_exhaustive().unwrap());
    }

    #[test]
    fn a_wildcard_field_still_leaves_its_sibling_uncovered() {
        let ty = pair_struct();
        let coverage = cover(&ty, &["Pair { hi: _, lo: true }"]);
        assert!(!coverage.is_exhaustive().unwrap());
        // `hi` free and `lo` covered both ways is total again.
        let total = cover(
            &ty,
            &["Pair { hi: _, lo: true }", "Pair { hi: _, lo: false }"],
        );
        assert!(total.is_exhaustive().unwrap());
    }

    #[test]
    fn an_enum_struct_variant_matches_its_payload_fields() {
        let ty = msg();
        let coverage = cover(
            &ty,
            &[
                "Msg::Idle",
                "Msg::Pair { hi: false, lo: false }",
                "Msg::Pair { hi: false, lo: true }",
                "Msg::Pair { hi: true, lo: false }",
                "Msg::Pair { hi: true, lo: true }",
            ],
        );
        assert!(coverage.is_exhaustive().unwrap());
    }

    #[test]
    fn an_enum_struct_variant_binds_payload_fields() {
        let ty = msg();
        let arm = lower("Msg::Pair { hi: x, lo: _ }", &ty);
        let bindings = &arm.alternatives[0].bindings;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].name, "x");
        assert_eq!(bindings[0].ty, ValueType::Bool);
        // tag (1 bit) + `hi` at struct offset 1.
        assert_eq!(bindings[0].offset, 2);
    }

    #[test]
    fn a_path_from_another_enum_is_refused() {
        let ty = msg();
        match lower_err("Other::Idle", &ty) {
            PatErrorKind::NotAConstructor(message) => {
                assert!(message.contains("Other"), "{message}");
            }
            other => panic!("expected NotAConstructor, got {other:?}"),
        }
    }

    #[test]
    fn a_tuple_variant_needs_the_payload_arity() {
        let ty = msg();
        match lower_err("Msg::Idle(x)", &ty) {
            PatErrorKind::ArityMismatch { expected, found } => {
                assert_eq!((expected, found), (0, 1));
            }
            other => panic!("expected ArityMismatch, got {other:?}"),
        }
        // `Msg::Pair` is a generated *named* aggregate, so the tuple form is a
        // shape error whatever its arity, not an arity error.
        match lower_err("Msg::Pair(a, b)", &ty) {
            PatErrorKind::NotAConstructor(message) => {
                assert!(message.contains("Pair"), "{message}");
            }
            other => panic!("expected NotAConstructor, got {other:?}"),
        }
    }

    #[test]
    fn bindings_record_their_offset_and_type() {
        let ty = option(uint(8));
        let arm = lower("Some(x)", &ty);
        let bindings = &arm.alternatives[0].bindings;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].name, "x");
        assert_eq!(bindings[0].offset, 1);
        assert_eq!(bindings[0].ty, uint(8));
    }

    #[test]
    fn pattern_test_compares_tag_then_payload() {
        let ty = option(ValueType::Bool);
        let arm = lower("Some(true)", &ty);
        assert_eq!(
            pattern_test(&arm.alternatives[0], &ty).unwrap(),
            Test::All(vec![
                Test::Bits {
                    offset: 0,
                    width: 1,
                    value: 1
                },
                Test::Bits {
                    offset: 1,
                    width: 1,
                    value: 1
                },
            ])
        );
        let idle = lower("None", &ty);
        assert_eq!(
            pattern_test(&idle.alternatives[0], &ty).unwrap(),
            Test::Bits {
                offset: 0,
                width: 1,
                value: 0
            }
        );
    }

    #[test]
    fn a_negative_literal_is_encoded_in_twos_complement() {
        let span = Span::call_site();
        assert_eq!(
            const_pattern(1, &sint(8), span, true).unwrap(),
            Pat::Int(0xff)
        );
        assert_eq!(
            const_pattern(128, &sint(8), span, true).unwrap(),
            Pat::Int(0x80)
        );
        assert!(const_pattern(129, &sint(8), span, true).is_err());
        assert!(const_pattern(256, &uint(8), span, false).is_err());
        // A positive literal must fit the *signed* range: `-1` is written as
        // `-1`, not as the bit pattern 255.
        assert_eq!(
            const_pattern(127, &sint(8), span, false).unwrap(),
            Pat::Int(127)
        );
        assert!(const_pattern(128, &sint(8), span, false).is_err());
        assert!(const_pattern(255, &sint(8), span, false).is_err());
    }

    #[test]
    fn a_negative_pattern_literal_reaches_lowering() {
        let ty = sint(8);
        assert_eq!(lower("-1", &ty).alternatives[0].pattern, Pat::Int(0xff));
    }

    #[test]
    fn an_integer_pattern_wider_than_128_bits_is_unsupported() {
        let span = Span::call_site();
        assert!(matches!(
            const_pattern(1, &uint(129), span, false),
            Err(PatError {
                kind: PatErrorKind::UnsupportedPattern(_),
                ..
            })
        ));
        assert!(matches!(
            const_pattern(1, &sint(129), span, true),
            Err(PatError {
                kind: PatErrorKind::UnsupportedPattern(_),
                ..
            })
        ));
    }

    #[test]
    fn a_predeclared_enum_qualifier_uses_its_root() {
        let ty = ValueType::Enum {
            name: "Option<uint<4>>".to_string(),
            tag_width: 1,
            variants: vec![variant("None", 0, None), variant("Some", 1, Some(uint(4)))],
        };
        assert_eq!(lower("Option::None", &ty).alternatives.len(), 1);
        assert_eq!(lower("Option::Some(x)", &ty).alternatives.len(), 1);
        assert!(cover(&ty, &["Option::None", "Option::Some(_)"])
            .is_exhaustive()
            .unwrap());

        let result = ValueType::Enum {
            name: "Result<uint<4>, bool>".to_string(),
            tag_width: 1,
            variants: vec![
                variant("Err", 0, Some(ValueType::Bool)),
                variant("Ok", 1, Some(uint(4))),
            ],
        };
        assert_eq!(lower("Result::Ok(x)", &result).alternatives.len(), 1);
        assert_eq!(lower("Result::Err(e)", &result).alternatives.len(), 1);

        match lower_err("Other::Some(x)", &ty) {
            PatErrorKind::NotAConstructor(message) => {
                assert!(message.contains("Option"), "{message}");
            }
            other => panic!("expected NotAConstructor, got {other:?}"),
        }
        match lower_err("core::option::Option::None", &ty) {
            PatErrorKind::UnsupportedPattern(message) => {
                assert!(message.contains("segments"), "{message}");
            }
            other => panic!("expected UnsupportedPattern, got {other:?}"),
        }
    }

    #[test]
    fn a_wild_only_enum_match_is_exhaustive() {
        let ty = ValueType::Enum {
            name: "State".to_string(),
            tag_width: 2,
            variants: vec![
                variant("Idle", 0, None),
                variant("Run", 1, None),
                variant("Halt", 2, None),
            ],
        };
        assert!(cover(&ty, &["_"]).is_exhaustive().unwrap());
    }

    #[test]
    fn an_array_rest_pattern_cannot_name_more_elements_than_the_array() {
        let one = ValueType::Array {
            element: Box::new(ValueType::Bool),
            len: 1,
        };
        // `[a, .., b]` on a one-element array used to move `position` backwards
        // and silently overwrite `a`; it is now refused before any slot is set.
        match lower_err("[a, .., b]", &one) {
            PatErrorKind::ArityMismatch { expected, found } => {
                assert_eq!((expected, found), (1, 2));
            }
            other => panic!("expected ArityMismatch, got {other:?}"),
        }

        // The same shape fits two elements, binding both ends at their own
        // offsets: element 0 is the least significant bit.
        let two = ValueType::Array {
            element: Box::new(ValueType::Bool),
            len: 2,
        };
        let arm = lower("[a, .., b]", &two);
        let bindings = &arm.alternatives[0].bindings;
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].name, "a");
        assert_eq!(bindings[0].offset, 0);
        assert_eq!(bindings[1].name, "b");
        assert_eq!(bindings[1].offset, 1);
    }

    #[test]
    fn a_struct_pattern_from_another_type_is_refused() {
        let ty = pair_struct();
        match lower_err("Other { hi: _, lo: _ }", &ty) {
            PatErrorKind::NotAConstructor(message) => {
                assert!(message.contains("Other"), "{message}");
                assert!(message.contains("Pair"), "{message}");
            }
            other => panic!("expected NotAConstructor, got {other:?}"),
        }
        assert_eq!(lower("Pair { hi: _, lo: _ }", &ty).alternatives.len(), 1);
    }

    /// A plain struct payload is bound whole; a generated numeric aggregate is
    /// split by `V(a, b)`; the named form is only for a generated named payload.
    #[test]
    fn a_plain_struct_payload_is_bound_whole_and_a_multi_payload_splits() {
        let packet = ValueType::Struct {
            name: "Packet".to_string(),
            fields: vec![
                TypeField {
                    name: "hi".to_string(),
                    ty: ValueType::Bool,
                },
                TypeField {
                    name: "lo".to_string(),
                    ty: ValueType::Bool,
                },
            ],
        };
        let generated = ValueType::Struct {
            name: "E::Multi".to_string(),
            fields: vec![
                TypeField {
                    name: "0".to_string(),
                    ty: ValueType::Bool,
                },
                TypeField {
                    name: "1".to_string(),
                    ty: uint(4),
                },
            ],
        };
        let ty = ValueType::Enum {
            name: "E".to_string(),
            tag_width: 1,
            variants: vec![
                variant("Data", 0, Some(packet.clone())),
                variant("Multi", 1, Some(generated)),
            ],
        };

        // `E::Data(p)` binds the whole `Packet`, not its fields.
        let arm = lower("E::Data(p)", &ty);
        let bindings = &arm.alternatives[0].bindings;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].name, "p");
        assert_eq!(bindings[0].offset, 1);
        assert_eq!(bindings[0].ty, packet);

        // `E::Data { … }` is not a generated named payload.
        match lower_err("E::Data { hi: _, lo: _ }", &ty) {
            PatErrorKind::NotAConstructor(message) => {
                assert!(message.contains("Data(value)"), "{message}");
            }
            other => panic!("expected NotAConstructor, got {other:?}"),
        }

        // `E::Multi(a, b)` splits the generated numeric aggregate. Field 0 is
        // the most significant of the 5-bit aggregate, so it sits at 1 + 4.
        let arm = lower("E::Multi(a, b)", &ty);
        let bindings = &arm.alternatives[0].bindings;
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].name, "a");
        assert_eq!(bindings[0].ty, ValueType::Bool);
        assert_eq!(bindings[0].offset, 5);
        assert_eq!(bindings[1].name, "b");
        assert_eq!(bindings[1].ty, uint(4));
        assert_eq!(bindings[1].offset, 1);

        // Two elements never fit a single plain struct payload.
        match lower_err("E::Data(a, b)", &ty) {
            PatErrorKind::ArityMismatch { expected, found } => {
                assert_eq!((expected, found), (1, 2));
            }
            other => panic!("expected ArityMismatch, got {other:?}"),
        }

        // A generated named aggregate is never one whole value: `V(p)` is a
        // shape error, the named form is the only one.
        match lower_err("Msg::Pair(p)", &msg()) {
            PatErrorKind::NotAConstructor(message) => {
                assert!(message.contains("Pair"), "{message}");
            }
            other => panic!("expected NotAConstructor, got {other:?}"),
        }

        // A multi-payload aggregate takes exactly its fields, so `V(p)` cannot
        // steal a single whole-payload binding.
        match lower_err("E::Multi(p)", &ty) {
            PatErrorKind::ArityMismatch { expected, found } => {
                assert_eq!((expected, found), (2, 1));
            }
            other => panic!("expected ArityMismatch, got {other:?}"),
        }
    }

    /// A wildcard row spans every column of a wide flat product, so the walk
    /// must answer immediately instead of branching per element.
    #[test]
    fn a_wide_bool_array_covered_by_a_wildcard_is_exhaustive() {
        let ty = ValueType::Array {
            element: Box::new(ValueType::Bool),
            len: 32,
        };
        assert!(cover(&ty, &["_"]).is_exhaustive().unwrap());
    }

    /// The same for a struct of sixteen fields, written with `_` and with
    /// bindings: bindings cover exactly as much as `_`, they only record
    /// offsets.
    #[test]
    fn a_struct_with_sixteen_fields_covered_by_wildcards_is_exhaustive() {
        let ty = struct_of_bools(16);
        let wild = format!(
            "Bools {{ {} }}",
            (0..16)
                .map(|index| format!("f{index}: _"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(cover(&ty, &[&wild]).is_exhaustive().unwrap());

        let bound = format!(
            "Bools {{ {} }}",
            (0..16)
                .map(|index| format!("f{index}: b{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let arm = lower(&bound, &ty);
        assert_eq!(arm.alternatives[0].bindings.len(), 16);
        assert!(cover(&ty, &[&bound]).is_exhaustive().unwrap());
    }

    /// A match that is exhaustive but exponential to prove for this algorithm
    /// reports a work limit: never a claim about nesting, and never a wrong
    /// verdict.
    #[test]
    fn a_hard_but_covered_match_reports_work_not_nesting() {
        let ty = ValueType::Array {
            element: Box::new(ValueType::Bool),
            len: 32,
        };
        let mut elements = vec!["_"; 32];
        elements[31] = "false";
        let first = format!("[{}]", elements.join(", "));
        elements[31] = "true";
        let second = format!("[{}]", elements.join(", "));
        let patterns = [first.as_str(), second.as_str()];

        match cover(&ty, &patterns).is_exhaustive() {
            Err(error) => {
                // Assert on the *complete* diagnostic a caller reports, not on
                // the inner detail: the "too large to prove" prefix belongs to
                // `PatErrorKind::message`.
                let message = error.message();
                assert!(
                    matches!(&error.kind, PatErrorKind::CoverageLimit(_)),
                    "expected a coverage limit, got {message}"
                );
                assert!(message.contains("too large to prove"), "{message}");
                assert!(!message.contains("nest"), "{message}");
            }
            Ok(verdict) => panic!("expected a work limit, got {verdict:?}"),
        }
    }
}
