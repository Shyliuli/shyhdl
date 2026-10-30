//! The rules both lowerings share about packed values.
//!
//! ShyHDL source is lowered twice: by the frontend into synthesizable modules,
//! and by the testbench into Verilog text. The two produce different things, but
//! a packed value is the same in both - a width, a signedness and a layout - and
//! so is the rule that neither widening nor a sign change is implicit. Those
//! rules live here once, so a design and its test cannot disagree about them.

use crate::metadata::ValueType;

/// A value's width in bits and how its bits are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Width {
    /// Width in bits, at least 1.
    pub(crate) bits: u32,
    /// True for a two's-complement value.
    pub(crate) signed: bool,
}

/// Why two operands of one operation do not line up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AlignError {
    /// The widths differ: hardware arithmetic is equal width.
    Widths {
        /// Width of the left operand.
        lhs: u32,
        /// Width of the right operand.
        rhs: u32,
    },
    /// One value is signed and the other is not.
    Signs {
        /// Whether the left operand is signed.
        lhs: bool,
        /// Whether the right operand is signed.
        rhs: bool,
    },
}

/// The one width and signedness two operands are evaluated at.
///
/// Widening, narrowing and a change of sign are written in the source, never
/// applied here: two values that differ in either do not line up.
pub(crate) fn align(lhs: Width, rhs: Width) -> Result<Width, AlignError> {
    if lhs.bits != rhs.bits {
        return Err(AlignError::Widths {
            lhs: lhs.bits,
            rhs: rhs.bits,
        });
    }
    if lhs.signed != rhs.signed {
        return Err(AlignError::Signs {
            lhs: lhs.signed,
            rhs: rhs.signed,
        });
    }
    Ok(lhs)
}

/// `2^(width - 1)`: the magnitude of the most negative signed value, and the
/// exclusive upper bound of the most positive one.
pub(crate) fn signed_bound(width: u32) -> u128 {
    1u128 << (width - 1)
}

/// Does an integer literal fit a value of this width and signedness?
///
/// The sign is separate from the magnitude, so `-1` is `negative` and `1`. A
/// signed `width`-bit value holds `-2^(width-1) ..= 2^(width-1) - 1`, an unsigned
/// one holds `0 ..= 2^width - 1`, and a `u128` fits any width above 128 bits.
pub(crate) fn literal_fits(negative: bool, magnitude: u128, width: u32, signed: bool) -> bool {
    if !signed {
        // `1 << 128` does not fit a `u128`; every width that wide holds everything.
        return width >= 128 || magnitude < (1u128 << width);
    }
    if width > 128 {
        return true;
    }
    let bound = signed_bound(width);
    if negative {
        magnitude <= bound
    } else {
        magnitude < bound
    }
}

/// Does this type hold an enum anywhere? Equality has to read those by type.
pub(crate) fn contains_enum(ty: &ValueType) -> bool {
    match ty {
        ValueType::Enum { .. } => true,
        ValueType::Struct { fields, .. } => fields.iter().any(|field| contains_enum(&field.ty)),
        ValueType::Array { element, .. } => contains_enum(element),
        _ => false,
    }
}

/// How two values of one type are compared as values.
///
/// A packed aggregate is not always its bits: an enum carries a tag that says
/// which payload is the value, so only that payload is compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Equality {
    /// These bits of the two values must be equal.
    Bits {
        /// Low bit of the range, from the value's least significant bit.
        lo: u32,
        /// Number of bits in the range.
        width: u32,
    },
    /// Every part must be equal.
    All(Vec<Equality>),
    /// The tags must be equal, then the payload of the variant the tag names.
    /// Bits an inactive payload happens to hold are not part of the value, and a
    /// tag that names no variant says nothing about the payload.
    Enum {
        /// Low bit of the tag.
        tag_lo: u32,
        /// Width of the tag.
        tag_width: u32,
        /// Tagged payloads in declaration order; each tag value is unique.
        variants: Vec<(u64, Equality)>,
    },
}

/// How two values of `ty` are compared, part by part.
///
/// It fails, rather than inventing a layout, when `ty` has none.
pub(crate) fn equality(ty: &ValueType) -> Result<Equality, String> {
    build(ty, 0)
}

/// Width of `ty` in bits, as a message the caller can report.
fn bits_of(ty: &ValueType) -> Result<u32, String> {
    ty.bit_width().map_err(|error| error.to_string())
}

/// [`equality`], for a value that starts `lo` bits into the one being compared.
fn build(ty: &ValueType, lo: u32) -> Result<Equality, String> {
    // A value with no enum inside is one run of bits: only a tag says which bits
    // are part of the value, so a struct or an array without one is compared as
    // a whole.
    if !contains_enum(ty) {
        return Ok(Equality::Bits {
            lo,
            width: bits_of(ty)?,
        });
    }
    match ty {
        // The layout is the metadata's: fields packed most significant first,
        // element 0 of an array in the lowest bits.
        ValueType::Struct { fields, .. } => {
            let mut low = lo + bits_of(ty)?;
            let mut parts = Vec::with_capacity(fields.len());
            for field in fields {
                low -= bits_of(&field.ty)?;
                parts.push(build(&field.ty, low)?);
            }
            Ok(Equality::All(parts))
        }
        ValueType::Array { element, len } => {
            let width = bits_of(element)?;
            let mut parts = Vec::with_capacity(*len);
            for index in 0..*len {
                parts.push(build(element, lo + index as u32 * width)?);
            }
            Ok(Equality::All(parts))
        }
        ValueType::Enum {
            tag_width,
            variants,
            ..
        } => {
            let mut parts = Vec::new();
            for variant in variants {
                if let Some(payload) = &variant.payload {
                    parts.push((variant.tag, build(payload, lo + tag_width)?));
                }
            }
            Ok(Equality::Enum {
                tag_lo: lo,
                tag_width: *tag_width,
                variants: parts,
            })
        }
        // A scalar holds no enum, so it never takes the struct or array arm above.
        ValueType::Bool | ValueType::UInt { .. } | ValueType::SInt { .. } => Ok(Equality::Bits {
            lo,
            width: bits_of(ty)?,
        }),
    }
}
