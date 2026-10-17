//! Design metadata: the serializable mapping from source-level values to RTL
//! signals, consumed by the debug and replay layer (REQ-DBG-004).
//!
//! Layout follows REQ-LANG-003: struct fields go from the most significant bit
//! to the least significant bit in declaration order, with no padding.
//! `ValueType::bit_width` is the only place that computes widths.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Version of the [`DesignMetadata`] JSON wire format. Bump it when an older
/// build would misread a newer recording.
///
/// Version 2 removed `Variable.is_clock`: the debug clock comes from the testcase
/// manifest instead. A version 1 artifact is refused rather than migrated, and
/// [`compute_design_build_id`] changes, so recordings must be regenerated.
pub const SCHEMA_VERSION: u32 = 2;

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
    /// Payload type, `None` for a payload-free variant. Payload enums are M4
    /// work (REQ-LATER-001); the field exists so landing them does not change
    /// the wire format.
    pub payload: Option<Box<ValueType>>,
}

/// A hardware value type with a fixed, statically known bit width. Serialized
/// with a `kind` tag, for example `{ "kind": "uint", "width": 8 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueType {
    /// `bool`: exactly one bit.
    Bool,
    /// `uint<N>`; `width` is in bits and at least 1 (REQ-LANG-030).
    #[serde(rename = "uint")]
    UInt {
        /// Width in bits.
        width: u32,
    },
    /// `int<N>`; `width` is in bits and at least 1. Arithmetic semantics are
    /// M4 work (REQ-LATER-003). The wire `kind` stays `"sint"`: the schema name
    /// is not renamed with the language's type alias.
    #[serde(rename = "sint")]
    SInt {
        /// Width in bits.
        width: u32,
    },
    /// A value type, not a register and not a module (REQ-LANG-001).
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
    /// above it, unused payload bits zero. Not final until M4 (REQ-LANG-040,
    /// REQ-LANG-041).
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

    /// Recursively check what `bit_width` does not look at: names that address
    /// something are present and unique, and enum tags are unambiguous and fit
    /// in `tag_width`. `context` names the enclosing variable or field.
    fn validate_names_and_tags(&self, context: &str) -> Result<()> {
        match self {
            ValueType::Bool | ValueType::UInt { .. } | ValueType::SInt { .. } => Ok(()),
            ValueType::Struct { name, fields } => {
                if name.is_empty() {
                    return Err(invalid_metadata(format!(
                        "{context}: struct has an empty name"
                    )));
                }
                let mut seen = HashSet::new();
                for field in fields {
                    if field.name.is_empty() {
                        return Err(invalid_metadata(format!(
                            "{context}: struct `{name}` has an unnamed field"
                        )));
                    }
                    if !seen.insert(field.name.as_str()) {
                        return Err(invalid_metadata(format!(
                            "{context}: struct `{name}` has duplicate field `{}`",
                            field.name
                        )));
                    }
                    field
                        .ty
                        .validate_names_and_tags(&format!("{context}.{}", field.name))?;
                }
                Ok(())
            }
            ValueType::Array { element, .. } => element.validate_names_and_tags(context),
            ValueType::Enum {
                name,
                tag_width,
                variants,
            } => {
                if name.is_empty() {
                    return Err(invalid_metadata(format!(
                        "{context}: enum has an empty name"
                    )));
                }
                let mut seen = HashSet::new();
                for variant in variants {
                    if variant.name.is_empty() {
                        return Err(invalid_metadata(format!(
                            "{context}: enum `{name}` has an unnamed variant"
                        )));
                    }
                    if !seen.insert(variant.tag) {
                        return Err(invalid_metadata(format!(
                            "{context}: enum `{name}` has duplicate tag {}",
                            variant.tag
                        )));
                    }
                    // `tag_width >= 64` holds every `u64` tag, and shifting by
                    // 64 or more would panic, so it is excluded here.
                    if *tag_width < 64 && variant.tag >= (1u64 << tag_width) {
                        return Err(invalid_metadata(format!(
                            "{context}: enum `{name}` variant `{}` tag {} does not fit in tag_width {tag_width}",
                            variant.name, variant.tag
                        )));
                    }
                    if let Some(payload) = &variant.payload {
                        payload.validate_names_and_tags(&format!(
                            "{context}.{}.payload",
                            variant.name
                        ))?;
                    }
                }
                Ok(())
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
    /// Type and therefore the bit layout of the value (REQ-DBG-004).
    pub ty: ValueType,
    /// Where the variable is declared.
    pub source: SourceSpan,
}

/// Everything the debugger needs to know about one build's design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignMetadata {
    /// Wire format version, see [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Identifies the build this metadata belongs to; replay refuses a
    /// recording whose id differs (REQ-DBG-010). Compute it with
    /// [`compute_design_build_id`].
    pub build_id: String,
    /// Name of the top-level module.
    pub top: String,
    /// Debug-visible variables of the design.
    pub variables: Vec<Variable>,
}

impl DesignMetadata {
    /// New metadata for the current schema version.
    pub fn new(
        build_id: impl Into<String>,
        top: impl Into<String>,
        variables: Vec<Variable>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            build_id: build_id.into(),
            top: top.into(),
            variables,
        }
    }

    /// Check that the metadata is consistent enough to drive the debugger:
    /// non-empty `top` and `build_id`; per variable a non-empty unique `id`, a
    /// non-empty `instance_path` and `rtl_name`, and a `ty` whose width fits and
    /// whose nested names and enum tags are usable.
    ///
    /// [`DesignMetadata::from_json_str`] runs this, so malformed metadata is
    /// rejected at load instead of reaching GDB as wrong values.
    pub fn validate(&self) -> Result<()> {
        if self.top.is_empty() {
            return Err(invalid_metadata("`top` is empty"));
        }
        if self.build_id.is_empty() {
            return Err(invalid_metadata("`build_id` is empty"));
        }
        let mut ids = HashSet::new();
        for variable in &self.variables {
            if variable.id.is_empty() {
                return Err(invalid_metadata("a variable has an empty `id`"));
            }
            if !ids.insert(variable.id.as_str()) {
                return Err(invalid_metadata(format!(
                    "duplicate variable id `{}`",
                    variable.id
                )));
            }
            if variable.instance_path.is_empty() {
                return Err(invalid_metadata(format!(
                    "variable `{}` has an empty `instance_path`",
                    variable.id
                )));
            }
            if variable.rtl_name.is_empty() {
                return Err(invalid_metadata(format!(
                    "variable `{}` has an empty `rtl_name`",
                    variable.id
                )));
            }
            let context = format!("variable `{}`", variable.id);
            variable
                .ty
                .bit_width()
                .map_err(|error| invalid_metadata(format!("{context}: {error}")))?;
            variable.ty.validate_names_and_tags(&context)?;
        }
        Ok(())
    }

    /// Serialize to pretty JSON for a `*.meta.json` next to the generated RTL.
    /// Callers building metadata by hand should call
    /// [`DesignMetadata::validate`] first.
    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            action: "write",
            source,
        })
    }

    /// Parse JSON written by [`DesignMetadata::to_json_string`], rejecting a
    /// different schema version and inconsistent content.
    ///
    /// Nesting is bounded by `serde_json`'s 128-container recursion limit, so a
    /// hostile document cannot drive the recursive layout checks into a stack
    /// overflow; exceeding it is reported as [`Error::Json`].
    pub fn from_json_str(json: &str) -> Result<Self> {
        let metadata: Self = serde_json::from_str(json).map_err(|source| Error::Json {
            action: "read",
            source,
        })?;
        if metadata.schema_version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                expected: SCHEMA_VERSION,
                found: metadata.schema_version,
            });
        }
        metadata.validate()?;
        Ok(metadata)
    }
}

fn invalid_metadata(message: impl Into<String>) -> Error {
    Error::InvalidMetadata {
        message: message.into(),
    }
}

/// One source file that takes part in a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUnit {
    /// Path the file was read from; part of the build identity.
    pub path: String,
    /// Full text of the file.
    pub content: String,
}

/// One design parameter, for example `N=8` for `Adder<const N>`. Text, because
/// the frontend has the source spelling and the debugger only compares ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignParameter {
    /// Parameter name as written in the source.
    pub name: String,
    /// Value the frontend specialized the parameter to.
    pub value: String,
}

/// Digest of the source text alone.
///
/// Use it to detect that sources changed; to pair artifacts use
/// [`compute_design_build_id`], which also covers `top` and specialization.
pub fn compute_build_id(units: &[SourceUnit]) -> String {
    let mut hasher = BuildIdHasher::new();
    hasher.write(b"shyhdl-source-build-id\0");
    write_identity(&mut hasher);
    for unit in sorted_units(units) {
        hasher.write(unit.path.as_bytes());
        hasher.write(b"\0");
        hasher.write(&(unit.content.len() as u64).to_le_bytes());
        hasher.write(unit.content.as_bytes());
    }
    hasher.finish()
}

/// Build id of a design: its sources plus the `top` module and the
/// specialization parameters.
///
/// The same sources with a different `top` or different parameters are different
/// builds and get different ids. Backend and compile flags are not part of it;
/// pair them separately, for example in a manifest next to the artifacts.
///
/// Deterministic: sources are hashed in path order, parameters in name order, and
/// exactly repeated parameters collapse.
pub fn compute_design_build_id(
    units: &[SourceUnit],
    top: &str,
    parameters: &[DesignParameter],
) -> String {
    let mut hasher = BuildIdHasher::new();
    hasher.write(b"shyhdl-design-build-id\0");
    write_identity(&mut hasher);

    hasher.write(b"\0top\0");
    hasher.write(top.as_bytes());

    let mut ordered: Vec<&DesignParameter> = parameters.iter().collect();
    ordered.sort_by(|a, b| (&a.name, &a.value).cmp(&(&b.name, &b.value)));
    ordered.dedup_by(|a, b| a.name == b.name && a.value == b.value);
    for parameter in ordered {
        hasher.write(b"\0param\0");
        hasher.write(parameter.name.as_bytes());
        hasher.write(b"=");
        hasher.write(parameter.value.as_bytes());
    }

    for unit in sorted_units(units) {
        hasher.write(b"\0source\0");
        hasher.write(unit.path.as_bytes());
        hasher.write(b"\0");
        hasher.write(&(unit.content.len() as u64).to_le_bytes());
        hasher.write(unit.content.as_bytes());
    }
    hasher.finish()
}

/// Tool version and schema version, so an id never matches across formats.
fn write_identity(hasher: &mut BuildIdHasher) {
    hasher.write(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.write(&SCHEMA_VERSION.to_le_bytes());
}

fn sorted_units(units: &[SourceUnit]) -> Vec<&SourceUnit> {
    let mut ordered: Vec<&SourceUnit> = units.iter().collect();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    ordered
}

/// FNV-1a 64 over the build identity. Deterministic across runs and platforms;
/// it only has to distinguish builds, it is not a security primitive.
struct BuildIdHasher(u64);

impl BuildIdHasher {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(self) -> String {
        format!("{}-{:016x}", env!("CARGO_PKG_VERSION"), self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uint(width: u32) -> ValueType {
        ValueType::UInt { width }
    }

    fn metadata(variables: Vec<Variable>) -> DesignMetadata {
        DesignMetadata::new(compute_design_build_id(&[], "top", &[]), "top", variables)
    }

    fn variable(id: &str, ty: ValueType) -> Variable {
        Variable {
            id: id.into(),
            source_name: "value".into(),
            instance_path: "top.u0".into(),
            rtl_name: format!("u0_{id}"),
            ty,
            source: SourceSpan::new("src/top.rs", 4, 9),
        }
    }

    #[test]
    fn struct_width_is_sum_of_fields() {
        // Field-to-bit mapping (declaration order, no padding) is verified
        // against generated RTL, not here.
        let ty = ValueType::Struct {
            name: "Word".into(),
            fields: vec![
                TypeField {
                    name: "op".into(),
                    ty: uint(4),
                },
                TypeField {
                    name: "addr".into(),
                    ty: uint(32),
                },
                TypeField {
                    name: "flag".into(),
                    ty: ValueType::Bool,
                },
                TypeField {
                    name: "tag".into(),
                    ty: uint(7),
                },
            ],
        };
        assert_eq!(ty.bit_width().unwrap(), 44);
    }

    #[test]
    fn array_width_multiplies_element_width() {
        let ty = ValueType::Array {
            element: Box::new(uint(6)),
            len: 5,
        };
        assert_eq!(ty.bit_width().unwrap(), 30);
    }

    #[test]
    fn zero_width_types_are_rejected() {
        assert!(uint(0).bit_width().is_err());
        assert!(ValueType::Array {
            element: Box::new(uint(8)),
            len: 0
        }
        .bit_width()
        .is_err());
        assert!(ValueType::Struct {
            name: "Empty".into(),
            fields: vec![]
        }
        .bit_width()
        .is_err());
    }

    #[test]
    fn width_overflow_is_reported_not_truncated() {
        let ty = ValueType::Array {
            element: Box::new(uint(u32::MAX)),
            len: 2,
        };
        assert!(matches!(
            ty.bit_width().unwrap_err(),
            Error::TypeLayout { .. }
        ));

        let ty = ValueType::Struct {
            name: "TooWide".into(),
            fields: vec![
                TypeField {
                    name: "a".into(),
                    ty: uint(u32::MAX),
                },
                TypeField {
                    name: "b".into(),
                    ty: uint(1),
                },
            ],
        };
        assert!(matches!(
            ty.bit_width().unwrap_err(),
            Error::TypeLayout { .. }
        ));
    }

    #[test]
    fn metadata_json_round_trips() {
        let original = metadata(vec![variable("0", uint(8))]);
        let json = original.to_json_string().unwrap();
        assert!(
            json.contains("\"kind\": \"uint\""),
            "unexpected json: {json}"
        );
        assert_eq!(DesignMetadata::from_json_str(&json).unwrap(), original);
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let json = r#"{"schema_version":99,"build_id":"x","top":"t","variables":[]}"#;
        assert!(matches!(
            DesignMetadata::from_json_str(json).unwrap_err(),
            Error::SchemaVersion {
                expected: SCHEMA_VERSION,
                found: 99
            }
        ));

        // A version 1 artifact (the one that still carried `is_clock`) is
        // refused with a version diagnostic, not migrated.
        let stale = r#"{"schema_version":1,"build_id":"x","top":"t","variables":[{"id":"0","source_name":"v","instance_path":"t.u","rtl_name":"u_v","ty":{"kind":"uint","width":8},"source":{"file":"f","line":1,"column":1},"is_clock":true}]}"#;
        assert!(matches!(
            DesignMetadata::from_json_str(stale).unwrap_err(),
            Error::SchemaVersion { found: 1, .. }
        ));
    }

    #[test]
    fn inconsistent_metadata_is_rejected_at_load() {
        let duplicate_ids = metadata(vec![variable("0", uint(8)), variable("0", uint(4))]);
        let json = duplicate_ids.to_json_string().unwrap();
        assert!(matches!(
            DesignMetadata::from_json_str(&json).unwrap_err(),
            Error::InvalidMetadata { .. }
        ));

        let zero_width = metadata(vec![variable("0", uint(0))]);
        assert!(matches!(
            zero_width.validate().unwrap_err(),
            Error::InvalidMetadata { .. }
        ));

        let mut empty_top = metadata(vec![variable("0", uint(8))]);
        empty_top.top = String::new();
        assert!(matches!(
            empty_top.validate().unwrap_err(),
            Error::InvalidMetadata { .. }
        ));

        let mut empty_build_id = metadata(vec![variable("0", uint(8))]);
        empty_build_id.build_id = String::new();
        assert!(matches!(
            empty_build_id.validate().unwrap_err(),
            Error::InvalidMetadata { .. }
        ));
    }

    fn load_type(ty: ValueType) -> Result<DesignMetadata> {
        DesignMetadata::from_json_str(&metadata(vec![variable("0", ty)]).to_json_string().unwrap())
    }

    #[test]
    fn unusable_nested_types_are_rejected_at_load() {
        let field = |name: &str, ty: ValueType| TypeField {
            name: name.into(),
            ty,
        };
        let variant = |name: &str, tag: u64| EnumVariant {
            name: name.into(),
            tag,
            payload: None,
        };
        let cases: Vec<(&str, ValueType)> = vec![
            (
                "empty struct name",
                ValueType::Struct {
                    name: String::new(),
                    fields: vec![field("a", uint(1))],
                },
            ),
            (
                "unnamed field",
                ValueType::Struct {
                    name: "S".into(),
                    fields: vec![field("", uint(1))],
                },
            ),
            (
                "duplicate field",
                ValueType::Struct {
                    name: "S".into(),
                    fields: vec![field("a", uint(1)), field("a", uint(2))],
                },
            ),
            (
                "empty enum name",
                ValueType::Enum {
                    name: String::new(),
                    tag_width: 1,
                    variants: vec![variant("A", 0)],
                },
            ),
            (
                "unnamed variant",
                ValueType::Enum {
                    name: "E".into(),
                    tag_width: 1,
                    variants: vec![variant("", 0)],
                },
            ),
            (
                "duplicate tag",
                ValueType::Enum {
                    name: "E".into(),
                    tag_width: 2,
                    variants: vec![variant("A", 1), variant("B", 1)],
                },
            ),
            (
                "tag wider than tag_width",
                ValueType::Enum {
                    name: "E".into(),
                    tag_width: 2,
                    variants: vec![variant("A", 4)],
                },
            ),
            (
                "no variants",
                ValueType::Enum {
                    name: "E".into(),
                    tag_width: 2,
                    variants: vec![],
                },
            ),
            (
                "nested width overflow",
                ValueType::Struct {
                    name: "S".into(),
                    fields: vec![field(
                        "a",
                        ValueType::Array {
                            element: Box::new(uint(u32::MAX)),
                            len: 2,
                        },
                    )],
                },
            ),
            (
                "nested zero width",
                ValueType::Struct {
                    name: "S".into(),
                    fields: vec![field("a", uint(0))],
                },
            ),
            (
                "array length beyond any width",
                ValueType::Array {
                    element: Box::new(uint(8)),
                    len: usize::MAX,
                },
            ),
        ];

        for (what, ty) in cases {
            let error = load_type(ty).unwrap_err();
            assert!(
                matches!(error, Error::InvalidMetadata { .. }),
                "{what}: unexpected error {error}"
            );
        }
    }

    #[test]
    fn tag_width_limits_are_inclusive() {
        // 2 bits hold tags 0..=3; 64 bits hold every u64 tag.
        for tag in 0..=3 {
            let variant = EnumVariant {
                name: format!("V{tag}"),
                tag,
                payload: None,
            };
            let ty = ValueType::Enum {
                name: "E".into(),
                tag_width: 2,
                variants: vec![variant],
            };
            assert!(load_type(ty).is_ok(), "tag {tag} should fit in 2 bits");
        }
        let ty = ValueType::Enum {
            name: "E".into(),
            tag_width: 64,
            variants: vec![EnumVariant {
                name: "Max".into(),
                tag: u64::MAX,
                payload: None,
            }],
        };
        assert!(load_type(ty).is_ok(), "64-bit tag_width holds every tag");
    }

    #[test]
    fn deeply_nested_external_json_is_rejected_without_overflowing() {
        // `serde_json` stops nested containers at 128 levels, so the recursive
        // checks below never see a stack-deep document.
        let mut ty = String::from(r#"{"kind":"uint","width":8}"#);
        for _ in 0..500 {
            ty = format!(r#"{{"kind":"array","element":{ty},"len":1}}"#);
        }
        let json = format!(
            r#"{{"schema_version":{SCHEMA_VERSION},"build_id":"b","top":"t","variables":[{{"id":"0","source_name":"v","instance_path":"t.u","rtl_name":"u_v","ty":{ty},"source":{{"file":"f","line":1,"column":1}}}}]}}"#
        );
        let error = DesignMetadata::from_json_str(&json).unwrap_err();
        assert!(
            matches!(error, Error::Json { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn source_build_id_is_stable_and_content_sensitive() {
        let a = SourceUnit {
            path: "a.rs".into(),
            content: "fn a() {}".into(),
        };
        let b = SourceUnit {
            path: "b.rs".into(),
            content: "fn b() {}".into(),
        };
        let base = compute_build_id(&[a.clone(), b.clone()]);
        assert_eq!(base, compute_build_id(&[a.clone(), b.clone()]));
        assert_eq!(base, compute_build_id(&[b.clone(), a.clone()]));

        let changed = SourceUnit {
            path: "b.rs".into(),
            content: "fn b() { }".into(),
        };
        assert_ne!(base, compute_build_id(&[a, changed]));
    }

    #[test]
    fn design_build_id_covers_top_and_parameters() {
        let units = [SourceUnit {
            path: "a.rs".into(),
            content: "fn a() {}".into(),
        }];
        let params = |value: &str| {
            [DesignParameter {
                name: "N".into(),
                value: value.into(),
            }]
        };

        let base = compute_design_build_id(&units, "top", &params("8"));
        assert_eq!(base, compute_design_build_id(&units, "top", &params("8")));
        assert_ne!(base, compute_design_build_id(&units, "other", &params("8")));
        assert_ne!(base, compute_design_build_id(&units, "top", &params("1")));
        assert_ne!(base, compute_build_id(&units));

        // Two parameters in either order, and an exactly repeated parameter,
        // give one id; a different parameter value does not.
        let n8 = DesignParameter {
            name: "N".into(),
            value: "8".into(),
        };
        let p2 = DesignParameter {
            name: "P".into(),
            value: "2".into(),
        };
        let ordered = compute_design_build_id(&units, "top", &[n8.clone(), p2.clone()]);
        assert_eq!(
            ordered,
            compute_design_build_id(&units, "top", &[p2.clone(), n8.clone()])
        );
        assert_eq!(
            ordered,
            compute_design_build_id(&units, "top", &[n8.clone(), n8, p2.clone()])
        );
        let n1 = DesignParameter {
            name: "N".into(),
            value: "1".into(),
        };
        assert_ne!(
            ordered,
            compute_design_build_id(&units, "top", &[n1, p2.clone()])
        );
        // A parameter renamed to another name is a different build too.
        let q2 = DesignParameter {
            name: "Q".into(),
            value: "2".into(),
        };
        assert_ne!(ordered, compute_design_build_id(&units, "top", &[q2, p2]));
    }

    #[test]
    fn proc_macro_spans_carry_source_locations() {
        let file = syn::parse_file("fn first() {}\nfn second() {}\n").unwrap();
        let span =
            SourceSpan::from_proc_macro2("input.rs", syn::spanned::Spanned::span(&file.items[1]))
                .unwrap();
        assert_eq!(span, SourceSpan::new("input.rs", 2, 1));
    }

    #[test]
    fn hand_built_spans_map_to_the_file_start() {
        // proc-macro2 1.0 reports `call_site()` as line 1, column 0 and the
        // fallback cannot tell that apart from the start of a file, so this is
        // the documented outcome rather than an error.
        let span =
            SourceSpan::from_proc_macro2("input.rs", proc_macro2::Span::call_site()).unwrap();
        assert_eq!(span, SourceSpan::new("input.rs", 1, 1));
    }
}
