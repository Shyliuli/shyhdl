//! Design metadata: the serializable mapping from source-level values to RTL
//! signals, consumed by the debug and replay layer.
//!
//! Struct fields go from the most significant bit to the least significant bit
//! in declaration order, with no padding. `ValueType::bit_width` is the only
//! place that computes widths.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A location in a ShyHDL source file. `line` and `column` are 1-based, so the
/// span prints as `file:line:col`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpan {
    /// Path of the source file as given to the frontend.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number, counted in UTF-8 characters.
    pub column: usize,
}

impl SourceSpan {
    /// Build a span from already 1-based coordinates.
    pub fn new(file: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            file: file.into(),
            line,
            column,
        }
    }

    /// Convert a `proc-macro2` span into a 1-based source span (`proc-macro2`
    /// columns are 0-based). `file` is the path the source was read from.
    ///
    /// Pass spans taken from parsed syntax, never hand-built ones: a span that
    /// cannot be placed at all (line 0) is reported as
    /// [`Error::MissingSourceLocation`], but `proc-macro2` 1.0's fallback
    /// reports `Span::call_site()` as 1:1, which is indistinguishable from a
    /// genuine location at the start of a file.
    pub fn from_proc_macro2(file: impl Into<String>, span: proc_macro2::Span) -> Result<Self> {
        let file = file.into();
        let start = span.start();
        if start.line == 0 {
            return Err(Error::MissingSourceLocation { file });
        }
        Ok(Self {
            file,
            line: start.line,
            column: start.column + 1,
        })
    }
}

impl std::fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// One field of a [`ValueType::Struct`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeField {
    /// Field name as written in the source.
    pub name: String,
    /// Type of the field.
    pub ty: ValueType,
}

/// One variant of a [`ValueType::Enum`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumVariant {
    /// Variant name as written in the source.
    pub name: String,
    /// Encoded tag value.
    pub tag: u64,
    /// Payload type, `None` for a payload-free variant.
    pub payload: Option<Box<ValueType>>,
}

/// A hardware value type with a fixed, statically known bit width. Serialized
/// with a `kind` tag, for example `{ "kind": "uint", "width": 8 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueType {
    /// `bool`: exactly one bit.
    Bool,
    /// `uint<N>`; `width` is in bits and at least 1.
    #[serde(rename = "uint")]
    UInt {
        /// Width in bits.
        width: u32,
    },
    /// `int<N>`; `width` is in bits and at least 1. The wire `kind` stays
    /// `"sint"`: the schema name is not renamed with the language's type alias.
    #[serde(rename = "sint")]
    SInt {
        /// Width in bits.
        width: u32,
    },
    /// A value type, not a register and not a module.
    Struct {
        /// Struct name as written in the source.
        name: String,
        /// Fields in declaration order, most significant bit first.
        fields: Vec<TypeField>,
    },
    /// `len` copies of `element`, laid out end to end.
    Array {
        /// Type of one element.
        element: Box<ValueType>,
        /// Number of elements, at least 1.
        len: usize,
    },
    /// Tag in the lowest `tag_width` bits, widest variant payload in the bits
    /// above it, unused payload bits zero.
    Enum {
        /// Enum name as written in the source.
        name: String,
        /// Width of the tag in bits, at least 1.
        tag_width: u32,
        /// Variants in declaration order.
        variants: Vec<EnumVariant>,
    },
}

impl ValueType {
    /// Total width in bits.
    ///
    /// Rejects zero-width types (`uint<0>`, an empty struct, an array of length
    /// 0) and width overflow, so a caller never gets a silently truncated width.
    pub fn bit_width(&self) -> Result<u32> {
        match self {
            ValueType::Bool => Ok(1),
            ValueType::UInt { width } | ValueType::SInt { width } => {
                if *width == 0 {
                    return Err(layout_error(format!("{self:?} has no bits")));
                }
                Ok(*width)
            }
            ValueType::Struct { name, fields } => {
                if fields.is_empty() {
                    return Err(layout_error(format!("struct `{name}` has no fields")));
                }
                let mut total: u32 = 0;
                for field in fields {
                    total = total.checked_add(field.ty.bit_width()?).ok_or_else(|| {
                        layout_error(format!("struct `{name}` is wider than {} bits", u32::MAX))
                    })?;
                }
                Ok(total)
            }
            ValueType::Array { element, len } => {
                if *len == 0 {
                    return Err(layout_error("array of length 0 has no bits"));
                }
                let len = u32::try_from(*len).map_err(|_| {
                    layout_error(format!(
                        "array of length {len} is wider than {} bits",
                        u32::MAX
                    ))
                })?;
                element.bit_width()?.checked_mul(len).ok_or_else(|| {
                    layout_error(format!(
                        "array of {len} elements is wider than {} bits",
                        u32::MAX
                    ))
                })
            }
            ValueType::Enum {
                name,
                tag_width,
                variants,
            } => {
                if *tag_width == 0 || variants.is_empty() {
                    return Err(layout_error(format!(
                        "enum `{name}` has no tag or no variants"
                    )));
                }
                let mut widest: u32 = 0;
                for variant in variants {
                    if let Some(payload) = &variant.payload {
                        widest = widest.max(payload.bit_width()?);
                    }
                }
                tag_width.checked_add(widest).ok_or_else(|| {
                    layout_error(format!("enum `{name}` is wider than {} bits", u32::MAX))
                })
            }
        }
    }
}

fn layout_error(message: impl Into<String>) -> Error {
    Error::TypeLayout {
        message: message.into(),
        span: None,
    }
}

/// A debug-visible variable: one source-level value bound to one RTL signal.
///
/// There is no clock marker here: the debug clock comes from the testcase
/// manifest (`gen_clk`), so no source-level marker survives into metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    /// Stable id the debugger addresses this variable by; unique per design.
    pub id: String,
    /// Name as written in the source.
    pub source_name: String,
    /// Hierarchical instance path, for example `top.u_adder`.
    pub instance_path: String,
    /// Signal name in the generated RTL.
    pub rtl_name: String,
    /// Type and therefore the bit layout of the value.
    pub ty: ValueType,
    /// Where the variable is declared.
    pub source: SourceSpan,
}

/// Everything the debugger needs to know about one build's design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignMetadata {
    /// Name of the top-level module.
    pub top: String,
    /// Debug-visible variables of the design.
    pub variables: Vec<Variable>,
}

impl DesignMetadata {
    /// New metadata for a design.
    pub fn new(top: impl Into<String>, variables: Vec<Variable>) -> Self {
        Self {
            top: top.into(),
            variables,
        }
    }

    /// Serialize to pretty JSON for a `*.meta.json` next to the generated RTL.
    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            action: "write",
            source,
        })
    }

    /// Parse JSON written by [`DesignMetadata::to_json_string`].
    pub fn from_json_str(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|source| Error::Json {
            action: "read",
            source,
        })
    }
}
