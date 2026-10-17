//! Synthetic debug ELF for GDB replay (REQ-DBG-009, REQ-DBG-051, REQ-DBG-056).
//!
//! [`build_symbol_elf`] writes an ELF64 executable for `EM_X86_64` that GDB
//! loads with `file` while `target remote` answers RSP from the recording. The
//! file describes a virtual address layout that deliberately differs from the
//! hardware bit layout (REQ-DBG-009):
//!
//! * `.text` starts at [`CODE_BASE`] (4 KiB aligned). Every observation point
//!   owns one [`POINT_SLOT`]-byte slot of `nop` instructions and one `is_stmt`
//!   line in the synthetic listing, so a source-level `step` or `next` moves
//!   exactly one reference point.
//! * The last slot belongs to the `tick` subprogram at [`SymbolLayout::tick_pc`]
//!   (REQ-DBG-056). It is a fixed anchor address, never the PC of a recorded
//!   point: the replay target reports it while walking `break tick` so GDB
//!   evaluates the host-side condition at every observation point
//!   (REQ-DBG-035), while ordinary `step` keeps reporting point PCs.
//! * `.data` is uninitialized (`SHT_NOBITS`): the values live in the recording,
//!   never in this file. Each variable gets one byte-packed slot in a
//!   declaration-order layout and a `DW_TAG_variable` located at that address.
//!
//! # Virtual layout
//!
//! Every hardware value is expanded to whole bytes, so the target must slice
//! the recorded value and serialize it into these offsets; copying hardware
//! bytes verbatim is wrong.
//!
//! * `struct`: declaration order, first field at offset 0, no padding. The
//!   first field is the most significant bits of the recorded value.
//! * `array`: element 0 at offset 0, and element 0 is the least significant
//!   element of the recorded value.
//! * `enum`: tag in the lowest `ceil(tag_width / 8)` bytes. A payload enum adds
//!   one union of the variant payloads after the tag, sized by the largest of
//!   them, so a field after the enum starts at the same offset whatever the tag
//!   is. Only the active payload's bytes are written; GDB's C union shows every
//!   interpretation of those bytes, the tag says which one is active, and just
//!   the bytes past the active payload stay zero.
//! * `uint<W>` for `W <= 128`: little-endian, `ceil(W / 8)` bytes, zero
//!   extended above bit `W`. Wider values are a `struct` holding one
//!   `unsigned char[ceil(W / 8)]` so the width survives.
//! * `int<W>` for `W <= 128`: little-endian `ceil(W / 8)` bytes, sign
//!   extended above bit `W` (a signed DWARF integer of that byte size), so GDB
//!   prints the sign-extended value. Wider values use one
//!   `signed char[ceil(W / 8)]`.
//!
//! The file has no program interpreter, no dynamic section and no unwind
//! tables: nothing here is ever executed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gimli::write::{
    Address, AttributeValue, DwarfUnit, EndianVec, Expression, FileId, LineProgram, LineString,
    Sections, Unit, UnitEntryId,
};
use gimli::{
    DW_ATE_boolean, DW_ATE_signed, DW_ATE_signed_char, DW_ATE_unsigned, DW_ATE_unsigned_char,
    DW_AT_byte_size, DW_AT_comp_dir, DW_AT_const_value, DW_AT_count, DW_AT_data_member_location,
    DW_AT_decl_file, DW_AT_decl_line, DW_AT_encoding, DW_AT_external, DW_AT_high_pc,
    DW_AT_language, DW_AT_location, DW_AT_low_pc, DW_AT_name, DW_AT_producer, DW_AT_stmt_list,
    DW_AT_type, DW_TAG_array_type, DW_TAG_base_type, DW_TAG_enumeration_type, DW_TAG_enumerator,
    DW_TAG_member, DW_TAG_structure_type, DW_TAG_subprogram, DW_TAG_subrange_type,
    DW_TAG_union_type, DW_TAG_variable, DwAt, DwAte, Encoding, Format, LineEncoding, SectionId,
    DW_LANG_C99,
};
use object::elf;
use object::write::elf::{FileHeader, ProgramHeader, SectionHeader, Sym, Writer};
use object::Endianness;

use crate::error::{Error, Result};
use crate::metadata::{DesignMetadata, ValueType, Variable};

/// First virtual address of the generated `.text`, 4 KiB aligned.
pub const CODE_BASE: u64 = 0x0040_0000;

/// Bytes reserved for one observation point, and for the `tick` anchor.
pub const POINT_SLOT: u64 = 8;

/// Name of the synthetic subprogram used for conditional breakpoints.
pub const TICK: &str = "tick";

const PAGE: u64 = 4096;
const NOP: u8 = 0x90;

/// Where the ELF and the synthetic listing are, and what the debug layout is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLayout {
    /// The ELF written by [`build_symbol_elf`].
    pub elf_path: PathBuf,
    /// The synthetic source listing beside the ELF; one line per point, plus
    /// one line for `tick`.
    pub source_path: PathBuf,
    /// Virtual address of every observation point, in replay order.
    pub point_pc: Vec<u64>,
    /// Reverse of `point_pc`; the replay point index for a stopped PC.
    pub pc_point: HashMap<u64, usize>,
    /// Address of the `tick` subprogram, never one of `point_pc` (REQ-DBG-056).
    pub tick_pc: u64,
    /// Start of `.text`.
    pub code_base: u64,
    /// One past the last byte of `.text`, including the `tick` slot.
    pub text_end: u64,
    /// Start of `.data`.
    pub data_base: u64,
    /// One past the last byte of `.data`.
    pub data_end: u64,
    /// One slot per debug-visible variable, in metadata declaration order.
    pub vars: Vec<VarSlot>,
    /// Start address of a variable, to its index in `vars`.
    pub addr_vars: BTreeMap<u64, usize>,
}

impl SymbolLayout {
    /// The variable whose debug slot contains `addr`, if any.
    pub fn var_at(&self, addr: u64) -> Option<&VarSlot> {
        let (start, index) = self.addr_vars.range(..=addr).next_back()?;
        let slot = &self.vars[*index];
        let end = start.checked_add(slot.size)?;
        (addr < end).then_some(slot)
    }

    /// Byte at a `.text` address, since nothing writes into that section.
    pub fn text_byte(&self, addr: u64) -> Option<u8> {
        (addr >= self.code_base && addr < self.text_end).then_some(NOP)
    }
}

/// One variable's slot in the virtual debug layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarSlot {
    /// `Variable::id`, so a slot maps back to the recording's signal binding.
    pub id: String,
    /// Unique GDB name of this slot, derived from the instance path.
    pub symbol_name: String,
    /// Name as written in the source.
    pub source_name: String,
    /// Hierarchical instance path, for example `top.u_adder`.
    pub instance_path: String,
    /// Virtual address of the slot.
    pub addr: u64,
    /// Size of the slot in bytes, always `kind.byte_size()`.
    pub size: u64,
    /// Type of the value stored in the slot.
    pub kind: VarKind,
}

/// The virtual debug layout of one value type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarKind {
    /// One byte, `DW_ATE_boolean`.
    Bool,
    /// Unsigned; at most 128 bits is one `DW_ATE_unsigned`, wider is a byte array.
    UInt {
        /// Width in bits.
        width: u32,
    },
    /// Signed; at most 128 bits is one `DW_ATE_signed`, wider is a byte array.
    SInt {
        /// Width in bits.
        width: u32,
    },
    /// Fields in declaration order, each expanded to whole bytes.
    Struct {
        /// Struct name as written in the source.
        name: String,
        /// Fields with their virtual byte offsets, in declaration order.
        fields: Vec<StructField>,
    },
    /// Contiguous elements, element 0 at the lowest offset.
    Array {
        /// Type of one element.
        element: Box<VarKind>,
        /// Number of elements.
        len: u64,
    },
    /// Tag in the lowest `ceil(tag_width / 8)` bytes, then the payload union.
    Enum {
        /// Enum name as written in the source.
        name: String,
        /// Width of the tag in bits.
        tag_width: u32,
        /// Variants with their encoded tags, in declaration order.
        variants: Vec<EnumVariantSlot>,
    },
}

impl VarKind {
    /// Size of the virtual debug slot in bytes.
    ///
    /// This is the one authoritative size for the target that serializes a
    /// recorded value: a struct is
    /// `max(byte_offset + field.kind.byte_size())` rather than
    /// `ceil(packed bits / 8)`, so `struct { uint<1>, uint<1> }` is two bytes
    /// and a nested struct is the sum of its field sizes.
    ///
    /// [`build_symbol_elf`] rejects a type whose size does not fit in `u64`, so
    /// a kind taken from a [`SymbolLayout`] is always exact; a kind built by
    /// hand that overflows saturates at `u64::MAX`.
    pub fn byte_size(&self) -> u64 {
        self.checked_byte_size().unwrap_or(u64::MAX)
    }

    fn checked_byte_size(&self) -> Option<u64> {
        Some(match self {
            VarKind::Bool => 1,
            VarKind::UInt { width } | VarKind::SInt { width } => width_bytes(*width)?,
            VarKind::Struct { fields, .. } => {
                let mut total: u64 = 0;
                for field in fields {
                    total = total.max(
                        field
                            .byte_offset
                            .checked_add(field.kind.checked_byte_size()?)?,
                    );
                }
                total
            }
            VarKind::Array { element, len } => element.checked_byte_size()?.checked_mul(*len)?,
            VarKind::Enum {
                tag_width,
                variants,
                ..
            } => {
                let mut payload: u64 = 0;
                for variant in variants {
                    if let Some(kind) = &variant.payload {
                        payload = payload.max(kind.checked_byte_size()?);
                    }
                }
                width_bytes(*tag_width)?.checked_add(payload)?
            }
        })
    }
}

/// One field of [`VarKind::Struct`], at its virtual byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    /// Field name as written in the source.
    pub name: String,
    /// Offset from the start of the struct in the virtual layout.
    pub byte_offset: u64,
    /// Type of the field.
    pub kind: VarKind,
}

/// One variant of [`VarKind::Enum`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumVariantSlot {
    /// Variant name as written in the source.
    pub name: String,
    /// Encoded tag value.
    pub tag: u64,
    /// Payload type, `None` for a payload-free variant. Every payload starts at
    /// the same debug byte offset, so the payload region is a union.
    pub payload: Option<Box<VarKind>>,
}

/// Write the debug ELF for `meta` and `points` to `out`, plus a synthetic
/// source listing beside it.
///
/// `points` has one opaque label per observation point in replay order (its
/// recorded time, for example). Point `i` becomes the PC at
/// `CODE_BASE + i * POINT_SLOT` and source line `i + 1`; the `tick` subprogram
/// sits one slot after the last point, at [`SymbolLayout::tick_pc`].
///
/// Variables the recording does not contain are filtered by the caller: this
/// function only knows the design metadata, not the recording.
pub fn build_symbol_elf(meta: &DesignMetadata, points: &[u64], out: &Path) -> Result<SymbolLayout> {
    meta.validate()?;
    if points.is_empty() {
        return Err(Error::InvalidMetadata {
            message: "cannot build debug symbols without observation points".into(),
        });
    }

    let text_len = (points.len() as u64)
        .checked_add(1)
        .and_then(|slots| slots.checked_mul(POINT_SLOT))
        .ok_or_else(|| layout_error("too many observation points"))?;
    let code_base = CODE_BASE;
    let text_end = code_base
        .checked_add(text_len)
        .ok_or_else(|| layout_error("observation points reach past the address space"))?;
    let data_base = align_up(text_end, PAGE)
        .ok_or_else(|| layout_error("the variable region reaches past the address space"))?;

    let (vars, data_len) = layout_variables(meta, data_base)?;
    let data_end = data_base
        .checked_add(data_len)
        .ok_or_else(|| layout_error("debug variables reach past the address space"))?;

    let point_pc: Vec<u64> = (0..points.len())
        .map(|i| code_base + i as u64 * POINT_SLOT)
        .collect();
    let tick_pc = code_base + points.len() as u64 * POINT_SLOT;
    let pc_point: HashMap<u64, usize> = point_pc.iter().copied().zip(0..).collect();
    debug_assert!(!pc_point.contains_key(&tick_pc));

    let elf_path = out.to_path_buf();
    let file_name = out
        .file_name()
        .ok_or_else(|| Error::Io {
            path: elf_path.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "output path has no file name to derive the symbol paths from",
            ),
        })?
        .to_string_lossy()
        .into_owned();
    let source_path = out.with_file_name(format!("{file_name}.points"));

    let dwarf = build_dwarf(meta, &vars, &point_pc, tick_pc, &source_path)?;
    let elf = build_elf(dwarf, &vars, text_len, tick_pc, data_len, data_base, out)?;
    fs::write(out, &elf).map_err(|source| Error::Io {
        path: elf_path.clone(),
        source,
    })?;
    fs::write(&source_path, source_listing(points)).map_err(|source| Error::Io {
        path: source_path.clone(),
        source,
    })?;

    Ok(SymbolLayout {
        elf_path,
        source_path,
        point_pc,
        pc_point,
        tick_pc,
        code_base,
        text_end,
        data_base,
        data_end,
        addr_vars: vars.iter().map(|slot| slot.addr).zip(0..).collect(),
        vars,
    })
}

/// Build the typed variable slots, packed in declaration order from `base`.
fn layout_variables(meta: &DesignMetadata, base: u64) -> Result<(Vec<VarSlot>, u64)> {
    let mut vars = Vec::with_capacity(meta.variables.len());
    let mut offset: u64 = 0;
    let mut names = HashSet::new();
    for variable in &meta.variables {
        let symbol_name = symbol_name(variable)?;
        if !names.insert(symbol_name.clone()) {
            return Err(Error::InvalidMetadata {
                message: format!(
                    "variable `{}` and another variable both map to debug symbol `{symbol_name}`",
                    variable.id
                ),
            });
        }
        let kind = var_kind(&variable.ty, &variable.id)?;
        let size = kind.checked_byte_size().ok_or_else(|| {
            layout_error(format!(
                "variable `{}` is larger than a u64 byte count",
                variable.id
            ))
        })?;
        let addr = base
            .checked_add(offset)
            .ok_or_else(|| layout_error("debug variables reach past the address space"))?;
        vars.push(VarSlot {
            id: variable.id.clone(),
            symbol_name,
            source_name: variable.source_name.clone(),
            instance_path: variable.instance_path.clone(),
            addr,
            size,
            kind,
        });
        offset = offset
            .checked_add(size)
            .ok_or_else(|| layout_error("debug variables are larger than a u64 byte count"))?;
    }
    Ok((vars, offset))
}

/// GDB name of a variable: `<instance path>__<source name>`, for example
/// `top__child__reg`. Distinct instance paths cannot collide, and a collision
/// that survives sanitizing is reported instead of silently merging two slots.
fn symbol_name(variable: &Variable) -> Result<String> {
    if variable.source_name.is_empty() {
        return Err(Error::InvalidMetadata {
            message: format!("variable `{}` has an empty `source_name`", variable.id),
        });
    }
    let path = variable
        .instance_path
        .split('.')
        .map(sanitize_component)
        .collect::<Vec<_>>()
        .join("__");
    Ok(format!(
        "{path}__{}",
        sanitize_component(&variable.source_name)
    ))
}

/// Keep identifier characters; anything else becomes `_`, and a leading digit
/// gets an underscore so the result is usable unquoted in a GDB expression.
fn sanitize_component(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for (index, ch) in component.chars().enumerate() {
        match ch {
            'a'..='z' | 'A'..='Z' | '_' => out.push(ch),
            '0'..='9' if index > 0 => out.push(ch),
            '0'..='9' => out.push('_'),
            _ => out.push('_'),
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// The virtual debug layout of a hardware type.
fn var_kind(ty: &ValueType, context: &str) -> Result<VarKind> {
    Ok(match ty {
        ValueType::Bool => VarKind::Bool,
        ValueType::UInt { width } => {
            width_bytes_checked(*width, context)?;
            VarKind::UInt { width: *width }
        }
        ValueType::SInt { width } => {
            width_bytes_checked(*width, context)?;
            VarKind::SInt { width: *width }
        }
        ValueType::Struct { name, fields } => {
            let mut out = Vec::with_capacity(fields.len());
            let mut offset: u64 = 0;
            for field in fields {
                let kind = var_kind(&field.ty, &format!("{context}.{}", field.name))?;
                let size = kind.checked_byte_size().ok_or_else(|| {
                    layout_error(format!(
                        "{context}: struct `{name}` is larger than a u64 byte count"
                    ))
                })?;
                out.push(StructField {
                    name: field.name.clone(),
                    byte_offset: offset,
                    kind,
                });
                offset = offset.checked_add(size).ok_or_else(|| {
                    layout_error(format!("{context}: struct `{name}` is too large"))
                })?;
            }
            VarKind::Struct {
                name: name.clone(),
                fields: out,
            }
        }
        ValueType::Array { element, len } => VarKind::Array {
            element: Box::new(var_kind(element, context)?),
            len: u64::try_from(*len).map_err(|_| {
                layout_error(format!("{context}: array length does not fit in u64"))
            })?,
        },
        ValueType::Enum {
            name,
            tag_width,
            variants,
        } => {
            width_bytes_checked(*tag_width, context)?;
            let mut slots = Vec::with_capacity(variants.len());
            for variant in variants {
                let payload = match &variant.payload {
                    Some(payload) => Some(Box::new(var_kind(
                        payload,
                        &format!("{context}.{}", variant.name),
                    )?)),
                    None => None,
                };
                slots.push(EnumVariantSlot {
                    name: variant.name.clone(),
                    tag: variant.tag,
                    payload,
                });
            }
            VarKind::Enum {
                name: name.clone(),
                tag_width: *tag_width,
                variants: slots,
            }
        }
    })
}

/// Bytes needed to hold `width` bits, expanded to whole bytes.
fn width_bytes(width: u32) -> Option<u64> {
    (width != 0).then(|| u64::from(width).div_ceil(8))
}

fn width_bytes_checked(width: u32, context: &str) -> Result<u64> {
    width_bytes(width).ok_or_else(|| {
        layout_error(format!(
            "{context}: a value of zero width has no debug layout"
        ))
    })
}

/// Round `value` up to `align`, or `None` when that does not fit in `u64`.
fn align_up(value: u64, align: u64) -> Option<u64> {
    value.div_ceil(align).checked_mul(align)
}

fn layout_error(message: impl Into<String>) -> Error {
    Error::TypeLayout {
        message: message.into(),
        span: None,
    }
}

/// The synthetic listing: one line per point at line `index + 1`, then the
/// `tick` anchor.
fn source_listing(points: &[u64]) -> String {
    let mut out = String::with_capacity((points.len() + 1) * 72);
    for (index, label) in points.iter().enumerate() {
        out.push_str(&format!(
            "/* observation point {index}; label {label:#018x} (synthetic listing) */\n"
        ));
    }
    out.push_str(&format!(
        "/* {TICK}: virtual anchor for `break {TICK} if <expr>`; not an observation point */\n"
    ));
    out
}

/// The DWARF for the virtual program: one compilation unit, one `tick`
/// subprogram, one variable per debug slot.
fn build_dwarf(
    meta: &DesignMetadata,
    vars: &[VarSlot],
    point_pc: &[u64],
    tick_pc: u64,
    source_path: &Path,
) -> Result<DwarfUnit> {
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };
    let mut dwarf = DwarfUnit::new(encoding);

    let working_dir = source_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = source_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shyhdl.points".to_string());
    let mut line_program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        line_string(&working_dir.to_string_lossy()),
        None,
        line_string(&file_name),
        None,
    );
    let listing_file = line_program.add_file(
        line_string(&file_name),
        line_program.default_directory(),
        None,
    );
    // Declaration locations map back to the source the user wrote; the listing
    // lines the debugger stops on are only a synthetic scaffold.
    let mut source_files: HashMap<String, FileId> = HashMap::new();
    for variable in &meta.variables {
        if variable.source.file.is_empty() || source_files.contains_key(&variable.source.file) {
            continue;
        }
        let id = line_program.add_file(
            line_string(&variable.source.file),
            line_program.default_directory(),
            None,
        );
        source_files.insert(variable.source.file.clone(), id);
    }
    emit_lines(&mut line_program, point_pc, tick_pc, listing_file);
    dwarf.unit.line_program = line_program;

    let root = dwarf.unit.root();
    let unit = &mut dwarf.unit;
    set(unit, root, DW_AT_producer, string("shyhdl debug symbols"));
    set(unit, root, DW_AT_name, string(&file_name));
    set(
        unit,
        root,
        DW_AT_comp_dir,
        string(&working_dir.to_string_lossy()),
    );
    // C-like framing keeps GDB's value formatting predictable for these
    // synthetic types; the names and offsets are still the design's.
    set(
        unit,
        root,
        DW_AT_language,
        AttributeValue::Language(DW_LANG_C99),
    );
    set(
        unit,
        root,
        DW_AT_low_pc,
        AttributeValue::Address(Address::Constant(point_pc[0])),
    );
    set(
        unit,
        root,
        DW_AT_high_pc,
        AttributeValue::Udata(tick_pc + POINT_SLOT - point_pc[0]),
    );
    set(unit, root, DW_AT_stmt_list, AttributeValue::LineProgramRef);

    // GDB finds a block by looking up the function symbol that contains the PC,
    // and a DAP front end hangs its variable list on that block's static
    // block -- the same scope `info variables` reads. A line-table row alone
    // names no function, so the observation points get a subprogram of their
    // own; `tick` stays the separate anchor `break tick if <expr>` uses.
    let replay = unit.add(root, DW_TAG_subprogram);
    set(unit, replay, DW_AT_name, string("replay"));
    set(
        unit,
        replay,
        DW_AT_low_pc,
        AttributeValue::Address(Address::Constant(point_pc[0])),
    );
    set(
        unit,
        replay,
        DW_AT_high_pc,
        AttributeValue::Udata(tick_pc - point_pc[0]),
    );
    set(unit, replay, DW_AT_external, AttributeValue::Flag(true));
    set(
        unit,
        replay,
        DW_AT_decl_file,
        AttributeValue::FileIndex(Some(listing_file)),
    );
    set(unit, replay, DW_AT_decl_line, AttributeValue::Udata(1));

    let tick = unit.add(root, DW_TAG_subprogram);
    set(unit, tick, DW_AT_name, string(TICK));
    set(
        unit,
        tick,
        DW_AT_low_pc,
        AttributeValue::Address(Address::Constant(tick_pc)),
    );
    set(unit, tick, DW_AT_high_pc, AttributeValue::Udata(POINT_SLOT));
    set(unit, tick, DW_AT_external, AttributeValue::Flag(true));
    set(
        unit,
        tick,
        DW_AT_decl_file,
        AttributeValue::FileIndex(Some(listing_file)),
    );
    set(
        unit,
        tick,
        DW_AT_decl_line,
        AttributeValue::Udata(point_pc.len() as u64 + 1),
    );

    // A variable with a unique short name stays reachable by that name; the
    // qualified name is always available. Two variables that share a short name
    // get no alias, so no lookup silently becomes ambiguous.
    let mut short_counts: HashMap<&str, usize> = HashMap::new();
    for variable in &meta.variables {
        *short_counts
            .entry(variable.source_name.as_str())
            .or_default() += 1;
    }
    for (variable, slot) in meta.variables.iter().zip(vars) {
        debug_assert_eq!(variable.id, slot.id, "slots follow the metadata order");
        let ty = emit_type(unit, root, &variable.ty)?;
        let location = expression(slot.addr);
        let decl_file = source_files
            .get(&variable.source.file)
            .map(|id| AttributeValue::FileIndex(Some(*id)))
            .unwrap_or(AttributeValue::FileIndex(Some(listing_file)));
        let names: &[&str] = if short_counts.get(variable.source_name.as_str()) == Some(&1) {
            &[slot.symbol_name.as_str(), variable.source_name.as_str()]
        } else {
            &[slot.symbol_name.as_str()]
        };
        for name in names {
            let die = unit.add(root, DW_TAG_variable);
            set(unit, die, DW_AT_name, string(name));
            set(unit, die, DW_AT_type, AttributeValue::UnitRef(ty));
            set(
                unit,
                die,
                DW_AT_location,
                AttributeValue::Exprloc(location.clone()),
            );
            // File scope, not `external`: GDB files non-external variables in
            // the block `info variables` and a DAP front end's `Globals` scope
            // both read, and `p <name>` still resolves them from any frame of
            // this unit, including a conditional `break tick if <expr>`.
            set(unit, die, DW_AT_decl_file, decl_file.clone());
            set(
                unit,
                die,
                DW_AT_decl_line,
                AttributeValue::Udata(variable.source.line as u64),
            );
        }
    }
    Ok(dwarf)
}

/// One `is_stmt` row per point plus one for `tick`, in a single sequence based
/// at the first point.
fn emit_lines(program: &mut LineProgram, point_pc: &[u64], tick_pc: u64, file: FileId) {
    let Some(&first) = point_pc.first() else {
        return;
    };
    program.begin_sequence(Some(Address::Constant(first)));
    for (index, _) in point_pc.iter().enumerate() {
        let row = program.row();
        row.address_offset = index as u64 * POINT_SLOT;
        row.file = file;
        row.line = index as u64 + 1;
        row.column = 1;
        row.is_statement = true;
        program.generate_row();
    }
    {
        let row = program.row();
        row.address_offset = point_pc.len() as u64 * POINT_SLOT;
        row.file = file;
        row.line = point_pc.len() as u64 + 1;
        row.column = 1;
        row.is_statement = true;
        program.generate_row();
    }
    program.end_sequence(tick_pc + POINT_SLOT - first);
}

fn expression(addr: u64) -> Expression {
    let mut expression = Expression::new();
    expression.op_addr(Address::Constant(addr));
    expression
}

fn line_string(value: &str) -> LineString {
    LineString::String(value.as_bytes().to_vec())
}

fn string(value: &str) -> AttributeValue {
    AttributeValue::String(value.as_bytes().to_vec())
}

fn set(unit: &mut Unit, die: UnitEntryId, name: DwAt, value: AttributeValue) {
    unit.get_mut(die).set(name, value);
}

/// A `DW_TAG_enumerator` child of an enumeration type.
fn enumerator(unit: &mut Unit, parent: UnitEntryId, name: &str, tag: u64) {
    let die = unit.add(parent, DW_TAG_enumerator);
    set(unit, die, DW_AT_name, string(name));
    set(unit, die, DW_AT_const_value, AttributeValue::Udata(tag));
}

/// The DWARF type of a hardware type, using the same byte sizes as
/// [`var_kind`] so member offsets and sizes cannot disagree.
///
/// Every type DIE is a child of the compilation unit and referenced by offset.
/// GDB only expects subrange children under an array type, so nesting a struct
/// inside one would hide its members.
fn emit_type(unit: &mut Unit, root: UnitEntryId, ty: &ValueType) -> Result<UnitEntryId> {
    Ok(match ty {
        ValueType::Bool => base_type(unit, root, "bool", DW_ATE_boolean, 1),
        ValueType::UInt { width } => {
            let size = width_bytes_checked(*width, "uint")?;
            if size <= 16 {
                base_type(unit, root, &format!("uint<{width}>"), DW_ATE_unsigned, size)
            } else {
                wide_type(
                    unit,
                    root,
                    &format!("uint<{width}>"),
                    size,
                    DW_ATE_unsigned_char,
                )
            }
        }
        ValueType::SInt { width } => {
            // The stored bytes hold the sign-extended value, so the DWARF type
            // is a signed integer of exactly those bytes.
            let size = width_bytes_checked(*width, "int")?;
            if size <= 16 {
                base_type(unit, root, &format!("int<{width}>"), DW_ATE_signed, size)
            } else {
                wide_type(
                    unit,
                    root,
                    &format!("int<{width}>"),
                    size,
                    DW_ATE_signed_char,
                )
            }
        }
        ValueType::Struct { name, fields } => {
            let die = unit.add(root, DW_TAG_structure_type);
            set(unit, die, DW_AT_name, string(name));
            let mut offset: u64 = 0;
            for field in fields {
                let field_type = emit_type(unit, root, &field.ty)?;
                let field_size =
                    var_kind(&field.ty, name)?
                        .checked_byte_size()
                        .ok_or_else(|| {
                            layout_error(format!("struct `{name}` is larger than a u64 byte count"))
                        })?;
                let member = unit.add(die, DW_TAG_member);
                set(unit, member, DW_AT_name, string(&field.name));
                set(
                    unit,
                    member,
                    DW_AT_data_member_location,
                    AttributeValue::Udata(offset),
                );
                set(
                    unit,
                    member,
                    DW_AT_type,
                    AttributeValue::UnitRef(field_type),
                );
                offset = offset
                    .checked_add(field_size)
                    .ok_or_else(|| layout_error(format!("struct `{name}` is too large")))?;
            }
            set(unit, die, DW_AT_byte_size, AttributeValue::Udata(offset));
            die
        }
        ValueType::Array { element, len } => {
            let element_size = var_kind(element, "array element")?
                .checked_byte_size()
                .ok_or_else(|| layout_error("array element is larger than a u64 byte count"))?;
            let count = u64::try_from(*len)
                .map_err(|_| layout_error("array length does not fit in u64"))?;
            let total = element_size
                .checked_mul(count)
                .ok_or_else(|| layout_error("array is too large"))?;
            let die = unit.add(root, DW_TAG_array_type);
            set(unit, die, DW_AT_byte_size, AttributeValue::Udata(total));
            let element_type = emit_type(unit, root, element)?;
            set(unit, die, DW_AT_type, AttributeValue::UnitRef(element_type));
            let subrange = unit.add(die, DW_TAG_subrange_type);
            set(unit, subrange, DW_AT_count, AttributeValue::Udata(count));
            die
        }
        ValueType::Enum {
            name,
            tag_width,
            variants,
        } => {
            let tag_bytes = width_bytes_checked(*tag_width, name)?;
            if variants.iter().all(|variant| variant.payload.is_none()) {
                // Payload-free enum: one enumeration type, unchanged.
                let die = unit.add(root, DW_TAG_enumeration_type);
                set(unit, die, DW_AT_name, string(name));
                set(unit, die, DW_AT_byte_size, AttributeValue::Udata(tag_bytes));
                for variant in variants {
                    enumerator(unit, die, &variant.name, variant.tag);
                }
                die
            } else {
                // With payloads the readable C shape is a tag plus a union of the
                // variant payloads, starting where the tag ends. The union is as
                // large as the widest payload, so the offsets of whatever follows
                // the enum never depend on the active tag.
                let mut union_bytes: u64 = 0;
                for variant in variants {
                    if let Some(payload) = &variant.payload {
                        let size =
                            var_kind(payload, name)?
                                .checked_byte_size()
                                .ok_or_else(|| {
                                    layout_error(format!(
                                        "enum `{name}` has a payload larger than a u64 byte count"
                                    ))
                                })?;
                        union_bytes = union_bytes.max(size);
                    }
                }
                let total = tag_bytes.checked_add(union_bytes).ok_or_else(|| {
                    layout_error(format!("enum `{name}` is larger than a u64 byte count"))
                })?;
                let die = unit.add(root, DW_TAG_structure_type);
                set(unit, die, DW_AT_name, string(name));
                set(unit, die, DW_AT_byte_size, AttributeValue::Udata(total));

                let tag_type = unit.add(root, DW_TAG_enumeration_type);
                set(unit, tag_type, DW_AT_name, string(&format!("{name}_tag")));
                set(
                    unit,
                    tag_type,
                    DW_AT_byte_size,
                    AttributeValue::Udata(tag_bytes),
                );
                for variant in variants {
                    enumerator(unit, tag_type, &variant.name, variant.tag);
                }
                let tag_member = unit.add(die, DW_TAG_member);
                set(unit, tag_member, DW_AT_name, string("tag"));
                set(
                    unit,
                    tag_member,
                    DW_AT_data_member_location,
                    AttributeValue::Udata(0),
                );
                set(
                    unit,
                    tag_member,
                    DW_AT_type,
                    AttributeValue::UnitRef(tag_type),
                );

                let union = unit.add(root, DW_TAG_union_type);
                set(unit, union, DW_AT_name, string("payload"));
                set(
                    unit,
                    union,
                    DW_AT_byte_size,
                    AttributeValue::Udata(union_bytes),
                );
                for variant in variants {
                    let Some(payload) = &variant.payload else {
                        continue;
                    };
                    let member = unit.add(union, DW_TAG_member);
                    set(unit, member, DW_AT_name, string(&variant.name));
                    set(
                        unit,
                        member,
                        DW_AT_data_member_location,
                        AttributeValue::Udata(0),
                    );
                    let payload_type = emit_type(unit, root, payload)?;
                    set(
                        unit,
                        member,
                        DW_AT_type,
                        AttributeValue::UnitRef(payload_type),
                    );
                }
                let payload_member = unit.add(die, DW_TAG_member);
                set(unit, payload_member, DW_AT_name, string("payload"));
                set(
                    unit,
                    payload_member,
                    DW_AT_data_member_location,
                    AttributeValue::Udata(tag_bytes),
                );
                set(
                    unit,
                    payload_member,
                    DW_AT_type,
                    AttributeValue::UnitRef(union),
                );
                die
            }
        }
    })
}

fn base_type(
    unit: &mut Unit,
    root: UnitEntryId,
    name: &str,
    encoding: DwAte,
    size: u64,
) -> UnitEntryId {
    let die = unit.add(root, DW_TAG_base_type);
    set(unit, die, DW_AT_name, string(name));
    set(
        unit,
        die,
        DW_AT_encoding,
        AttributeValue::Encoding(encoding),
    );
    set(unit, die, DW_AT_byte_size, AttributeValue::Udata(size));
    die
}

/// A value wider than 128 bits keeps its width as a struct holding one byte
/// array: one base type of that size would misreport every byte.
fn wide_type(
    unit: &mut Unit,
    root: UnitEntryId,
    name: &str,
    size: u64,
    encoding: DwAte,
) -> UnitEntryId {
    let die = unit.add(root, DW_TAG_structure_type);
    set(unit, die, DW_AT_name, string(name));
    set(unit, die, DW_AT_byte_size, AttributeValue::Udata(size));
    let member = unit.add(die, DW_TAG_member);
    set(unit, member, DW_AT_name, string("bytes"));
    set(
        unit,
        member,
        DW_AT_data_member_location,
        AttributeValue::Udata(0),
    );
    let element = base_type(unit, root, "unsigned char", encoding, 1);
    let array = unit.add(root, DW_TAG_array_type);
    set(unit, array, DW_AT_byte_size, AttributeValue::Udata(size));
    set(unit, array, DW_AT_type, AttributeValue::UnitRef(element));
    let subrange = unit.add(array, DW_TAG_subrange_type);
    set(unit, subrange, DW_AT_count, AttributeValue::Udata(size));
    set(unit, member, DW_AT_type, AttributeValue::UnitRef(array));
    die
}

/// Write the ELF64 executable that carries the sections and the symbol table.
fn build_elf(
    mut dwarf: DwarfUnit,
    vars: &[VarSlot],
    text_len: u64,
    tick_pc: u64,
    data_len: u64,
    data_base: u64,
    elf_path: &Path,
) -> Result<Vec<u8>> {
    let mut sections = Sections::new(EndianVec::new(gimli::LittleEndian));
    dwarf.write(&mut sections).map_err(|error| Error::Io {
        path: elf_path.to_path_buf(),
        source: io::Error::other(format!("failed to encode DWARF: {error}")),
    })?;
    let mut debug_sections: Vec<(&'static str, Vec<u8>)> = Vec::new();
    sections
        .for_each(|id, data| {
            let bytes = data.clone().into_vec();
            if !bytes.is_empty() {
                debug_sections.push((section_name(id), bytes));
            }
            Ok::<(), std::convert::Infallible>(())
        })
        .expect("for_each is infallible");
    debug_sections.sort_by_key(|(name, _)| *name);

    let text: Vec<u8> = vec![NOP; text_len as usize];

    let mut buffer: Vec<u8> = Vec::new();
    let mut writer = Writer::new(Endianness::Little, true, &mut buffer);
    writer.reserve_file_header();
    writer.reserve_program_headers(2);

    let text_name = writer.add_section_name(b".text");
    let text_index = writer.reserve_section_index();
    let data_name = writer.add_section_name(b".data");
    let data_index = writer.reserve_section_index();
    let mut debug_indices = Vec::with_capacity(debug_sections.len());
    for (name, _) in &debug_sections {
        let name_id = writer.add_section_name(name.as_bytes());
        debug_indices.push((writer.reserve_section_index(), name_id));
    }
    writer.reserve_symtab_section_index();
    writer.reserve_strtab_section_index();
    writer.reserve_shstrtab_section_index();

    let file_symbol = writer.add_string(b"shyhdl.points");
    writer.reserve_symbol_index(None);
    let tick_symbol = writer.add_string(TICK.as_bytes());
    writer.reserve_symbol_index(Some(text_index));
    let var_symbols: Vec<_> = vars
        .iter()
        .map(|slot| (writer.add_string(slot.symbol_name.as_bytes()), slot))
        .collect();
    for _ in vars {
        writer.reserve_symbol_index(Some(data_index));
    }

    let text_offset = writer.reserve(text.len(), PAGE as usize);
    // Uninitialized storage: the replay target answers reads from the
    // recording, so the file has no value bytes at all.
    let data_offset = writer.reserve(0, PAGE as usize);
    // Reserved in the order the sections are written below.
    writer.reserve_symtab();
    writer.reserve_strtab();
    writer.reserve_shstrtab();
    let debug_offsets: Vec<usize> = debug_sections
        .iter()
        .map(|(_, bytes)| writer.reserve(bytes.len(), 1))
        .collect();
    writer.reserve_section_headers();

    writer
        .write_file_header(&FileHeader {
            os_abi: 0,
            abi_version: 0,
            e_type: elf::ET_EXEC,
            e_machine: elf::EM_X86_64,
            e_entry: CODE_BASE,
            e_flags: 0,
        })
        .map_err(|error| Error::Io {
            path: elf_path.to_path_buf(),
            source: io::Error::other(format!("failed to write the ELF header: {error}")),
        })?;
    writer.write_align_program_headers();
    writer.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_X,
        p_offset: text_offset as u64,
        p_vaddr: CODE_BASE,
        p_paddr: CODE_BASE,
        p_filesz: text.len() as u64,
        p_memsz: text.len() as u64,
        p_align: PAGE,
    });
    writer.write_program_header(&ProgramHeader {
        p_type: elf::PT_LOAD,
        p_flags: elf::PF_R | elf::PF_W,
        p_offset: data_offset as u64,
        p_vaddr: data_base,
        p_paddr: data_base,
        p_filesz: 0,
        p_memsz: data_len,
        p_align: PAGE,
    });

    writer.pad_until(text_offset);
    writer.write(&text);
    writer.pad_until(data_offset);

    writer.write_null_symbol();
    writer.write_symbol(&Sym {
        name: Some(file_symbol),
        section: None,
        st_info: sym_info(elf::STB_LOCAL, elf::STT_FILE),
        st_other: elf::STV_DEFAULT,
        st_shndx: elf::SHN_ABS,
        st_value: 0,
        st_size: 0,
    });
    for (name, slot) in &var_symbols {
        writer.write_symbol(&Sym {
            name: Some(*name),
            section: Some(data_index),
            st_info: sym_info(elf::STB_GLOBAL, elf::STT_OBJECT),
            st_other: elf::STV_DEFAULT,
            st_shndx: 0,
            st_value: slot.addr,
            st_size: slot.size,
        });
    }
    writer.write_symbol(&Sym {
        name: Some(tick_symbol),
        section: Some(text_index),
        st_info: sym_info(elf::STB_GLOBAL, elf::STT_FUNC),
        st_other: elf::STV_DEFAULT,
        st_shndx: 0,
        st_value: tick_pc,
        st_size: POINT_SLOT,
    });
    writer.write_strtab();
    writer.write_shstrtab();
    for (offset, (_, bytes)) in debug_offsets.iter().zip(&debug_sections) {
        writer.pad_until(*offset);
        writer.write(bytes);
    }

    writer.write_null_section_header();
    writer.write_section_header(&SectionHeader {
        name: Some(text_name),
        sh_type: elf::SHT_PROGBITS,
        sh_flags: u64::from(elf::SHF_ALLOC | elf::SHF_EXECINSTR),
        sh_addr: CODE_BASE,
        sh_offset: text_offset as u64,
        sh_size: text.len() as u64,
        sh_link: 0,
        sh_info: 0,
        sh_addralign: 16,
        sh_entsize: 0,
    });
    writer.write_section_header(&SectionHeader {
        name: Some(data_name),
        sh_type: elf::SHT_NOBITS,
        sh_flags: u64::from(elf::SHF_ALLOC | elf::SHF_WRITE),
        sh_addr: data_base,
        sh_offset: data_offset as u64,
        sh_size: data_len,
        sh_link: 0,
        sh_info: 0,
        sh_addralign: 1,
        sh_entsize: 0,
    });
    for ((index, name_id), (offset, (_, bytes))) in debug_indices
        .iter()
        .zip(debug_offsets.iter().zip(&debug_sections))
    {
        let _ = index;
        writer.write_section_header(&SectionHeader {
            name: Some(*name_id),
            sh_type: elf::SHT_PROGBITS,
            sh_flags: 0,
            sh_addr: 0,
            sh_offset: *offset as u64,
            sh_size: bytes.len() as u64,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: 1,
            sh_entsize: 0,
        });
    }
    writer.write_symtab_section_header(2);
    writer.write_strtab_section_header();
    writer.write_shstrtab_section_header();
    Ok(buffer)
}

/// `st_info`: the binding in the high nibble, the symbol type in the low one.
fn sym_info(bind: u8, kind: u8) -> u8 {
    (bind << 4) | kind
}

/// ELF section name of a DWARF section.
fn section_name(id: SectionId) -> &'static str {
    id.name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{SourceSpan, TypeField};

    fn variable(instance_path: &str, source_name: &str, ty: ValueType) -> Variable {
        Variable {
            id: source_name.into(),
            source_name: source_name.into(),
            instance_path: instance_path.into(),
            rtl_name: "x".into(),
            ty,
            source: SourceSpan::new("t.shy", 1, 1),
        }
    }

    fn layout(ty: ValueType) -> VarKind {
        var_kind(&ty, "v").unwrap()
    }

    #[test]
    fn widths_expand_to_whole_bytes() {
        assert_eq!(width_bytes(1), Some(1));
        assert_eq!(width_bytes(8), Some(1));
        assert_eq!(width_bytes(9), Some(2));
        assert_eq!(width_bytes(129), Some(17));
        assert_eq!(width_bytes(0), None);
    }

    #[test]
    fn debug_layout_expands_struct_fields_in_declaration_order() {
        let kind = layout(ValueType::Struct {
            name: "Word".into(),
            fields: vec![
                TypeField {
                    name: "op".into(),
                    ty: ValueType::UInt { width: 4 },
                },
                TypeField {
                    name: "addr".into(),
                    ty: ValueType::UInt { width: 32 },
                },
                TypeField {
                    name: "flag".into(),
                    ty: ValueType::Bool,
                },
                TypeField {
                    name: "tag".into(),
                    ty: ValueType::UInt { width: 7 },
                },
            ],
        });
        let VarKind::Struct { fields, .. } = &kind else {
            panic!("expected a struct");
        };
        let offsets: Vec<u64> = fields.iter().map(|field| field.byte_offset).collect();
        assert_eq!(offsets, vec![0, 1, 5, 6]);
        assert_eq!(kind.byte_size(), 7, "no padding and no bit rounding");
    }

    #[test]
    fn nested_structs_sum_their_field_sizes() {
        // `struct { uint<1>, uint<1> }` is two bytes, not one: the packed width
        // is two bits, but each field owns a byte (REQ-DBG-009).
        let kind = layout(ValueType::Struct {
            name: "Bits".into(),
            fields: vec![
                TypeField {
                    name: "a".into(),
                    ty: ValueType::UInt { width: 1 },
                },
                TypeField {
                    name: "b".into(),
                    ty: ValueType::UInt { width: 1 },
                },
            ],
        });
        assert_eq!(kind.byte_size(), 2);

        let outer = layout(ValueType::Struct {
            name: "Outer".into(),
            fields: vec![
                TypeField {
                    name: "inner".into(),
                    ty: ValueType::Struct {
                        name: "Bits".into(),
                        fields: vec![
                            TypeField {
                                name: "a".into(),
                                ty: ValueType::UInt { width: 1 },
                            },
                            TypeField {
                                name: "b".into(),
                                ty: ValueType::UInt { width: 1 },
                            },
                        ],
                    },
                },
                TypeField {
                    name: "tail".into(),
                    ty: ValueType::UInt { width: 12 },
                },
            ],
        });
        assert_eq!(outer.byte_size(), 4, "2 bytes of inner plus 2 of tail");
    }

    #[test]
    fn arrays_and_enums_size_from_their_elements() {
        let array = layout(ValueType::Array {
            element: Box::new(ValueType::UInt { width: 6 }),
            len: 5,
        });
        assert_eq!(array.byte_size(), 5, "one byte per element, not 30 bits");

        let enumeration = layout(ValueType::Enum {
            name: "state".into(),
            tag_width: 2,
            variants: vec![crate::metadata::EnumVariant {
                name: "idle".into(),
                tag: 0,
                payload: None,
            }],
        });
        assert_eq!(enumeration.byte_size(), 1);
        let VarKind::Enum { variants, .. } = &enumeration else {
            panic!("expected an enum");
        };
        assert_eq!(variants[0].tag, 0);
    }

    #[test]
    fn payload_enums_are_a_tag_plus_the_widest_payload() {
        // `enum Msg { Ping, Data(uint<32>), Pair(Bits) }` in two tag bits: one
        // tag byte plus the widest payload. `Bits` is two `uint<1>` fields, so
        // its debug size is two bytes even though its hardware width is two
        // bits: the union region is measured in debug bytes, not raw bits.
        let message = || ValueType::Enum {
            name: "Msg".into(),
            tag_width: 2,
            variants: vec![
                crate::metadata::EnumVariant {
                    name: "Ping".into(),
                    tag: 0,
                    payload: None,
                },
                crate::metadata::EnumVariant {
                    name: "Data".into(),
                    tag: 1,
                    payload: Some(Box::new(ValueType::UInt { width: 32 })),
                },
                crate::metadata::EnumVariant {
                    name: "Pair".into(),
                    tag: 2,
                    payload: Some(Box::new(ValueType::Struct {
                        name: "Bits".into(),
                        fields: vec![
                            TypeField {
                                name: "a".into(),
                                ty: ValueType::UInt { width: 1 },
                            },
                            TypeField {
                                name: "b".into(),
                                ty: ValueType::UInt { width: 1 },
                            },
                        ],
                    })),
                },
            ],
        };
        let kind = layout(message());
        assert_eq!(kind.byte_size(), 5, "one tag byte plus the widest payload");
        let VarKind::Enum { variants, .. } = &kind else {
            panic!("expected an enum");
        };
        assert!(variants[0].payload.is_none());
        assert_eq!(
            variants[1].payload.as_deref().map(VarKind::byte_size),
            Some(4)
        );
        assert_eq!(
            variants[2].payload.as_deref().map(VarKind::byte_size),
            Some(2),
            "two expanded fields, not two bits"
        );

        // A field after the enum starts after the whole union, so its offset
        // does not move with the active variant.
        let holder = layout(ValueType::Struct {
            name: "Holder".into(),
            fields: vec![
                TypeField {
                    name: "before".into(),
                    ty: ValueType::UInt { width: 8 },
                },
                TypeField {
                    name: "msg".into(),
                    ty: message(),
                },
                TypeField {
                    name: "after".into(),
                    ty: ValueType::UInt { width: 16 },
                },
            ],
        });
        let VarKind::Struct { fields, .. } = &holder else {
            panic!("expected a struct");
        };
        assert_eq!(fields[1].byte_offset, 1);
        assert_eq!(fields[2].byte_offset, 6, "1 + tag byte + 4 payload bytes");
        assert_eq!(holder.byte_size(), 8);
    }

    #[test]
    fn symbol_names_come_from_the_instance_path() {
        assert_eq!(
            symbol_name(&variable("top.u_adder", "reg", ValueType::Bool)).unwrap(),
            "top__u_adder__reg"
        );
        assert_eq!(
            symbol_name(&variable("top", "reg", ValueType::Bool)).unwrap(),
            "top__reg"
        );
        assert_eq!(
            symbol_name(&variable("top", "weird name", ValueType::Bool)).unwrap(),
            "top__weird_name"
        );
        assert_eq!(
            symbol_name(&variable("1bad.path", "x", ValueType::Bool)).unwrap(),
            "_bad__path__x"
        );
    }
}
