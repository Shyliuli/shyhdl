//! Verification of the synthetic debug ELF (REQ-DBG-009, REQ-DBG-051,
//! REQ-DBG-056).
//!
//! Nothing here calls the builder twice to compare it with itself: the ELF is
//! decoded by GNU `readelf` (header, program headers, section table, line
//! table, DIEs) and by GDB (symbol addresses, member offsets, sizes, and the
//! `break tick` anchor), so a wrong layout cannot pass by agreeing with the
//! code that produced it. The virtual addresses and byte offsets asserted below
//! are the layout the requirements prescribe, written out by hand.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use shyhdl::debugger::symbols::{self, SymbolLayout};
use shyhdl::error::Error;
use shyhdl::metadata::{DesignMetadata, EnumVariant, SourceSpan, TypeField, ValueType, Variable};

const POINTS: usize = 5;
/// Labels for the observation points, as a recording reports them.
const LABELS: [u64; POINTS] = [0, 10, 20, 35, 40];

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// A temporary directory that removes itself, so a test run leaves no output.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "shyhdl-symbols-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a temporary directory");
        Self(path)
    }

    fn elf(&self) -> PathBuf {
        self.0.join("shyhdl-debug.elf")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn uint(width: u32) -> ValueType {
    ValueType::UInt { width }
}

fn int(width: u32) -> ValueType {
    ValueType::SInt { width }
}

fn payload_variant(name: &str, tag: u64, payload: ValueType) -> EnumVariant {
    EnumVariant {
        name: name.into(),
        tag,
        payload: Some(Box::new(payload)),
    }
}

fn field(name: &str, ty: ValueType) -> TypeField {
    TypeField {
        name: name.into(),
        ty,
    }
}

fn variant(name: &str, tag: u64) -> EnumVariant {
    EnumVariant {
        name: name.into(),
        tag,
        payload: None,
    }
}

fn variable(
    id: &str,
    instance_path: &str,
    source_name: &str,
    line: usize,
    ty: ValueType,
) -> Variable {
    Variable {
        id: id.into(),
        source_name: source_name.into(),
        instance_path: instance_path.into(),
        rtl_name: format!("{instance_path}_{source_name}"),
        ty,
        source: SourceSpan::new("examples/packet.shy.rs", line, 9),
    }
}

fn fixture_variables() -> Vec<Variable> {
    Vec::from([
        variable("0", "top.u0", "count", 10, uint(8)),
        variable("1", "top.u1", "count", 11, uint(8)),
        variable("2", "top", "enable", 12, ValueType::Bool),
        variable(
            "3",
            "top",
            "word",
            13,
            ValueType::Struct {
                name: "Word".into(),
                fields: vec![
                    field("op", uint(4)),
                    field("addr", uint(32)),
                    field("flag", ValueType::Bool),
                    field("tag", uint(7)),
                ],
            },
        ),
        variable(
            "4",
            "top",
            "bits",
            14,
            ValueType::Struct {
                name: "Bits".into(),
                fields: vec![field("a", uint(1)), field("b", uint(1))],
            },
        ),
        variable(
            "5",
            "top",
            "lanes",
            15,
            ValueType::Array {
                element: Box::new(uint(6)),
                len: 5,
            },
        ),
        variable(
            "6",
            "top",
            "state",
            16,
            ValueType::Enum {
                name: "State".into(),
                tag_width: 2,
                variants: vec![variant("idle", 0), variant("run", 1), variant("halt", 2)],
            },
        ),
        variable("7", "top", "wide", 17, uint(129)),
        variable("8", "top", "signed", 18, int(12)),
        variable(
            "9",
            "top",
            "packet",
            19,
            ValueType::Struct {
                name: "Packet".into(),
                fields: vec![
                    field(
                        "kind",
                        ValueType::Enum {
                            name: "Config".into(),
                            tag_width: 2,
                            variants: vec![variant("short", 0), variant("long", 1)],
                        },
                    ),
                    field(
                        "payload",
                        ValueType::Array {
                            element: Box::new(uint(8)),
                            len: 3,
                        },
                    ),
                    field("valid", ValueType::Bool),
                ],
            },
        ),
        variable(
            "10",
            "top",
            "matrix",
            20,
            ValueType::Array {
                element: Box::new(ValueType::Struct {
                    name: "Pair".into(),
                    fields: vec![field("hi", uint(3)), field("lo", uint(5))],
                }),
                len: 2,
            },
        ),
        // Payload enums: each is a tag plus a union of the variant payloads.
        variable("11", "top", "option", 21, option_type()),
        variable(
            "12",
            "top",
            "message",
            22,
            ValueType::Enum {
                name: "Msg".into(),
                tag_width: 2,
                variants: vec![
                    variant("ping", 0),
                    payload_variant("data", 1, uint(32)),
                    payload_variant(
                        "pair",
                        2,
                        ValueType::Struct {
                            name: "Pair8".into(),
                            fields: vec![field("hi", uint(8)), field("lo", uint(8))],
                        },
                    ),
                ],
            },
        ),
        variable(
            "13",
            "top",
            "delta",
            23,
            ValueType::Enum {
                name: "Delta".into(),
                tag_width: 1,
                variants: vec![variant("idle", 0), payload_variant("move", 1, int(12))],
            },
        ),
        variable(
            "14",
            "top",
            "bag",
            24,
            ValueType::Enum {
                name: "Bag".into(),
                tag_width: 1,
                variants: vec![
                    variant("nothing", 0),
                    payload_variant(
                        "bits",
                        1,
                        ValueType::Struct {
                            name: "Bits2".into(),
                            fields: vec![field("a", uint(1)), field("b", uint(1))],
                        },
                    ),
                ],
            },
        ),
        variable(
            "15",
            "top",
            "holder",
            25,
            ValueType::Struct {
                name: "Holder".into(),
                fields: vec![
                    field("before", uint(8)),
                    field("choice", option_type()),
                    field("after", uint(16)),
                ],
            },
        ),
        variable(
            "16",
            "top",
            "queue",
            26,
            ValueType::Array {
                element: Box::new(option_type()),
                len: 3,
            },
        ),
    ])
}

/// `enum Opt { none, some(uint<8>) }`: one tag byte plus a one byte payload.
fn option_type() -> ValueType {
    ValueType::Enum {
        name: "Opt".into(),
        tag_width: 1,
        variants: vec![variant("none", 0), payload_variant("some", 1, uint(8))],
    }
}

/// Each variable's GDB name, byte offset and byte size in the virtual `.data`,
/// derived by hand from the layout rules:
///
/// * `uint<W>` and `int<W>` are `ceil(W / 8)` bytes,
/// * a struct is its fields in declaration order, each expanded to whole bytes
///   (`Bits` with two `uint<1>` fields is two bytes, not one),
/// * an array is `len` element sizes end to end,
/// * a payload-free enum is `ceil(tag_width / 8)` bytes; a payload enum adds one
///   union of the variant payloads, as wide as the widest of them in debug
///   bytes (`Bits2` with two `uint<1>` fields is two bytes, not two bits).
const EXPECTED_SLOTS: [(&str, u64, u64); 17] = [
    ("top__u0__count", 0, 1),
    ("top__u1__count", 1, 1),
    ("top__enable", 2, 1),
    ("top__word", 3, 7),
    ("top__bits", 10, 2),
    ("top__lanes", 12, 5),
    ("top__state", 17, 1),
    ("top__wide", 18, 17),
    ("top__signed", 35, 2),
    ("top__packet", 37, 5),
    ("top__matrix", 42, 4),
    ("top__option", 46, 2),
    ("top__message", 48, 5),
    ("top__delta", 53, 3),
    ("top__bag", 56, 3),
    ("top__holder", 59, 5),
    ("top__queue", 64, 6),
];

/// Sum of the sizes in `EXPECTED_SLOTS`.
const DATA_LEN: u64 = 70;

/// `Word` member offsets: `op` one byte, `addr` four, `flag` one, `tag` one.
const WORD_MEMBERS: [(&str, u64); 4] = [("op", 0), ("addr", 1), ("flag", 5), ("tag", 6)];

/// Short names that are unique in the fixture, so GDB must also answer to them.
const UNIQUE_SHORT_NAMES: [&str; 15] = [
    "enable", "word", "bits", "lanes", "state", "wide", "signed", "packet", "matrix", "option",
    "message", "delta", "bag", "holder", "queue",
];

fn expected_offset(symbol_name: &str) -> u64 {
    EXPECTED_SLOTS
        .iter()
        .find(|(name, _, _)| *name == symbol_name)
        .expect("a fixture variable")
        .1
}

fn fixture_meta() -> DesignMetadata {
    DesignMetadata::new("test-build-id", "top", fixture_variables())
}

fn build(tag: &str) -> (TempDir, SymbolLayout) {
    let dir = TempDir::new(tag);
    let elf = dir.elf();
    let layout =
        symbols::build_symbol_elf(&fixture_meta(), &LABELS, &elf).expect("the fixture builds");
    (dir, layout)
}

/// Run a tool with a fixed C locale, so parsing never depends on translations.
fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {program}: {error}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn readelf(elf: &Path, args: &[&str]) -> String {
    let mut full: Vec<&str> = args.to_vec();
    let path = elf.to_string_lossy().into_owned();
    full.push(&path);
    run("readelf", &full)
}

fn gdb(elf: &Path, commands: &[String]) -> String {
    let path = elf.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        "-batch".into(),
        "-q".into(),
        "-nx".into(),
        "-ex".into(),
        "set confirm off".into(),
        "-ex".into(),
        "set pagination off".into(),
    ];
    for command in commands {
        args.push("-ex".into());
        args.push(command.clone());
    }
    args.push(path);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run("gdb", &refs)
}

// ---------------------------------------------------------------------------
// readelf: the file itself
// ---------------------------------------------------------------------------

#[test]
fn elf_is_a_virtual_x86_64_executable_without_runtime_tables() {
    let (dir, layout) = build("header");
    let elf = dir.elf();

    let header = readelf(&elf, &["-h"]);
    for needle in [
        "ELF64",
        "2's complement, little endian",
        "EXEC (Executable file)",
        "Advanced Micro Devices X86-64",
    ] {
        assert!(
            header.contains(needle),
            "ELF header lacks {needle}:\n{header}"
        );
    }

    let program = readelf(&elf, &["-l"]);
    for forbidden in ["INTERP", "DYNAMIC", "TLS", "GNU_RELRO", "GNU_STACK", "NOTE"] {
        assert!(
            !program.contains(forbidden),
            "program headers must not contain {forbidden}:\n{program}"
        );
    }
    let segments = segments(&program);
    assert_eq!(
        segments.len(),
        2,
        "one text segment and one data segment:\n{program}"
    );
    assert_eq!(
        segments[0].0, layout.code_base,
        "text loads at the code base"
    );
    assert_eq!(segments[0].1, layout.text_end - layout.code_base);
    assert_eq!(segments[0].2, layout.text_end - layout.code_base);
    assert_eq!(
        segments[1].0, layout.data_base,
        "data loads at the data base"
    );
    assert_eq!(
        segments[1].1, 0,
        "the file has no value bytes; the recording answers reads"
    );
    assert_eq!(segments[1].2, DATA_LEN, "the variable window is in memory");

    let sections = readelf(&elf, &["-S"]);
    for forbidden in [".eh_frame", ".interp", ".dynamic", ".got", ".plt"] {
        assert!(
            !sections.contains(forbidden),
            "the symbol file must not carry {forbidden}:\n{sections}"
        );
    }
    let text = section(&sections, ".text");
    assert_eq!(text.0, "PROGBITS");
    assert_eq!(text.1, layout.code_base);
    assert_eq!(text.3, layout.text_end - layout.code_base);
    let data = section(&sections, ".data");
    assert_eq!(data.0, "NOBITS", "the debug image has no initial values");
    assert_eq!(data.1, layout.data_base);
    assert_eq!(data.3, DATA_LEN);

    let symbols = readelf(&elf, &["-s"]);
    for (name, offset, size) in EXPECTED_SLOTS {
        let address = layout.data_base + offset;
        let needle = format!("{size} OBJECT  GLOBAL DEFAULT    2 {name}");
        assert!(
            symbols.lines().any(|line| line.contains(&needle)),
            "no symbol line for {name} ({needle}) in:\n{symbols}"
        );
        assert!(
            symbols.contains(&format!("{address:016x}")),
            "symbol table lacks the address of {name} ({address:#x}):\n{symbols}"
        );
    }
    assert!(
        symbols.lines().any(|line| {
            line.split_whitespace().last() == Some("tick")
                && line.contains(&format!("{:016x}", layout.tick_pc))
        }),
        "the tick anchor must be a function symbol at {:#x}:\n{symbols}",
        layout.tick_pc
    );
}

/// `(vaddr, file size, memory size)` of every `PT_LOAD` segment.
fn segments(program: &str) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    let mut lines = program.lines().peekable();
    while let Some(line) = lines.next() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.first() != Some(&"LOAD") {
            continue;
        }
        let vaddr = hex(tokens[2]);
        let sizes = lines.peek().expect("a LOAD segment has a size line");
        let sizes: Vec<&str> = sizes.split_whitespace().collect();
        out.push((vaddr, hex(sizes[0]), hex(sizes[1])));
    }
    out
}

/// `(type, address, file offset, size)` of a section header.
fn section(sections: &str, name: &str) -> (String, u64, u64, u64) {
    let line = sections
        .lines()
        .find(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            tokens.get(2) == Some(&name)
        })
        .unwrap_or_else(|| panic!("no section header for {name}:\n{sections}"));
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let size = sections
        .lines()
        .skip_while(|candidate| *candidate != line)
        .nth(1)
        .and_then(|next| next.split_whitespace().next())
        .unwrap_or_else(|| panic!("no size line for {name}:\n{sections}"));
    (
        tokens[3].to_string(),
        hex(tokens[4]),
        hex(tokens[5]),
        hex(size),
    )
}

fn hex(value: &str) -> u64 {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(digits, 16)
        .unwrap_or_else(|error| panic!("not hexadecimal: {value}: {error}"))
}

// ---------------------------------------------------------------------------
// readelf: line table and DIEs
// ---------------------------------------------------------------------------

/// One row of `readelf --debug-dump=decodedline`.
#[derive(Debug)]
struct Row {
    line: u64,
    address: u64,
    is_stmt: bool,
}

fn decoded_lines(elf: &Path) -> Vec<Row> {
    let dump = readelf(elf, &["--debug-dump=decodedline"]);
    let mut rows = Vec::new();
    for line in dump.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(position) = tokens.iter().position(|token| token.starts_with("0x")) else {
            continue;
        };
        // The end-of-sequence row has no line number.
        let Some(Ok(line)) = tokens.get(position - 1).map(|token| token.parse()) else {
            continue;
        };
        rows.push(Row {
            line,
            address: hex(tokens[position]),
            is_stmt: tokens.last() == Some(&"x"),
        });
    }
    rows
}

/// One DIE from `readelf --debug-dump=info`.
struct Die {
    tag: String,
    depth: usize,
    attrs: HashMap<String, String>,
}

impl Die {
    fn attr(&self, name: &str) -> String {
        self.attrs
            .get(name)
            .unwrap_or_else(|| panic!("a {} has no {name}", self.tag))
            .clone()
    }

    fn address(&self, name: &str) -> u64 {
        hex(self.attr(name).as_str())
    }

    /// The `DW_OP_addr` of a `DW_AT_location`, which readelf prints decoded.
    fn location(&self) -> u64 {
        let value = self.attr("DW_AT_location");
        let start = value
            .find("DW_OP_addr: ")
            .unwrap_or_else(|| panic!("no DW_OP_addr in {value}"))
            + "DW_OP_addr: ".len();
        let digits: String = value[start..]
            .chars()
            .take_while(|ch| ch.is_ascii_hexdigit())
            .collect();
        hex(&digits)
    }
}

fn dies(elf: &Path) -> Vec<Die> {
    parse_dies(&readelf(elf, &["--debug-dump=info"]))
}

fn parse_dies(dump: &str) -> Vec<Die> {
    let mut dies: Vec<Die> = Vec::new();
    for line in dump.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix('<') else {
            continue;
        };
        let Some(gt) = rest.find('>') else {
            continue;
        };
        if rest.as_bytes().get(gt + 1) == Some(&b'<') {
            // `<depth><offset>: Abbrev Number: n (DW_TAG_...)`
            let depth = rest[..gt].parse().expect("a DIE depth");
            let after = &rest[gt + 1..];
            let tag = after
                .find("(DW_TAG_")
                .map(|start| {
                    let tail = &after[start + 1..];
                    let end = tail.find(')').expect("a closed tag");
                    tail[..end].to_string()
                })
                .unwrap_or_default();
            dies.push(Die {
                tag,
                depth,
                attrs: HashMap::new(),
            });
        } else if let Some((name, value)) = rest[gt + 1..].split_once(':') {
            let die = dies.last_mut().expect("an attribute belongs to a DIE");
            die.attrs
                .insert(name.trim().to_string(), value.trim().to_string());
        }
    }
    dies
}

/// DIEs with `tag` whose `DW_AT_name` is `name`.
fn named<'a>(dies: &'a [Die], tag: &str, name: &str) -> Vec<&'a Die> {
    dies.iter()
        .filter(|die| {
            die.tag == tag && die.attrs.get("DW_AT_name").map(String::as_str) == Some(name)
        })
        .collect()
}

/// `(name, offset)` of the direct `DW_TAG_member` children of a type DIE, in the
/// order readelf walks them.
fn members(dies: &[Die], parent: &Die) -> Vec<(String, u64)> {
    dies.iter()
        .skip_while(|die| !std::ptr::eq(*die, parent))
        .skip(1)
        .take_while(|die| die.depth > parent.depth)
        .filter(|die| die.tag == "DW_TAG_member")
        .map(|die| {
            (
                die.attr("DW_AT_name"),
                die.attrs["DW_AT_data_member_location"]
                    .parse()
                    .expect("a member offset"),
            )
        })
        .collect()
}

/// `(name, tag)` of the `DW_TAG_enumerator` children of an enumeration type.
fn enumerators(dies: &[Die], parent: &Die) -> Vec<(String, u64)> {
    dies.iter()
        .skip_while(|die| !std::ptr::eq(*die, parent))
        .skip(1)
        .take_while(|die| die.depth > parent.depth)
        .filter(|die| die.tag == "DW_TAG_enumerator")
        .map(|die| {
            (
                die.attr("DW_AT_name"),
                die.attr("DW_AT_const_value")
                    .parse()
                    .expect("an enumerator value"),
            )
        })
        .collect()
}

/// Every `DW_TAG_variable` name mapped to its location address.
fn variable_locations(dies: &[Die]) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for die in dies.iter().filter(|die| die.tag == "DW_TAG_variable") {
        let name = die.attr("DW_AT_name");
        assert!(
            out.insert(name.clone(), die.location()).is_none(),
            "duplicate variable DIE {name}"
        );
    }
    out
}

#[test]
fn line_table_has_one_statement_line_per_point_plus_the_tick_anchor() {
    let (dir, layout) = build("lines");
    let rows = decoded_lines(&dir.elf());

    assert_eq!(
        rows.len(),
        POINTS + 1,
        "one line per point plus the tick anchor: {rows:?}"
    );
    let mut seen = HashSet::new();
    for (index, row) in rows.iter().take(POINTS).enumerate() {
        assert_eq!(row.address, layout.point_pc[index]);
        assert_eq!(row.line, index as u64 + 1, "a distinct line per point");
        assert!(row.is_stmt, "point {index} must be a statement");
        assert!(seen.insert(row.line), "line {} repeats", row.line);
    }
    let tick = rows.last().expect("the tick row");
    assert_eq!(tick.address, layout.tick_pc);
    assert_eq!(tick.line, POINTS as u64 + 1);
    assert!(tick.is_stmt);
    assert!(!layout.pc_point.contains_key(&layout.tick_pc));
}

#[test]
fn dwarf_defines_one_tick_anchor_and_every_variable_at_its_layout_address() {
    let (dir, layout) = build("dwarf");
    let dies = dies(&dir.elf());

    let ticks = named(&dies, "DW_TAG_subprogram", symbols::TICK);
    assert_eq!(ticks.len(), 1, "`break tick` must not be ambiguous");
    assert_eq!(ticks[0].address("DW_AT_low_pc"), layout.tick_pc);
    assert_eq!(ticks[0].address("DW_AT_high_pc"), symbols::POINT_SLOT);
    assert_eq!(
        ticks[0].attrs["DW_AT_decl_line"],
        (POINTS + 1).to_string(),
        "tick sits on the listing line after the last point"
    );

    let addresses = variable_locations(&dies);
    for (name, offset, _) in EXPECTED_SLOTS {
        assert_eq!(
            addresses.get(name).copied(),
            Some(layout.data_base + offset),
            "DW_AT_location of {name}"
        );
        for alias in UNIQUE_SHORT_NAMES {
            if name.ends_with(&format!("__{alias}")) {
                assert_eq!(
                    addresses.get(alias).copied(),
                    Some(layout.data_base + offset),
                    "the unique short name {alias} must resolve too"
                );
            }
        }
    }
    assert!(
        !addresses.contains_key("count"),
        "`count` names two variables, so it must not alias either one"
    );
    assert_eq!(
        addresses.len(),
        EXPECTED_SLOTS.len() + UNIQUE_SHORT_NAMES.len(),
        "every slot plus one alias per unique short name"
    );

    // Struct members carry the expanded byte offsets, not hardware bit offsets.
    let word = named(&dies, "DW_TAG_structure_type", "Word");
    assert_eq!(word.len(), 1);
    assert_eq!(word[0].attrs["DW_AT_byte_size"], "7");
    assert_eq!(
        members(&dies, word[0]),
        WORD_MEMBERS
            .iter()
            .map(|(name, offset)| (name.to_string(), *offset))
            .collect::<Vec<_>>()
    );

    // A payload enum is a struct: the tag, then a union of the variant payloads
    // starting where the tag ends.
    let message = named(&dies, "DW_TAG_structure_type", "Msg");
    assert_eq!(message.len(), 1);
    assert_eq!(
        message[0].attrs["DW_AT_byte_size"], "5",
        "one tag byte plus the widest payload (uint<32>)"
    );
    assert_eq!(
        members(&dies, message[0]),
        vec![("tag".to_string(), 0), ("payload".to_string(), 1)]
    );
    let tags = named(&dies, "DW_TAG_enumeration_type", "Msg_tag");
    assert_eq!(tags.len(), 1);
    assert_eq!(
        enumerators(&dies, tags[0]),
        vec![
            ("ping".to_string(), 0),
            ("data".to_string(), 1),
            ("pair".to_string(), 2)
        ],
        "the tag type keeps every variant's encoded tag"
    );
    let unions = named(&dies, "DW_TAG_union_type", "payload");
    let widest = unions
        .iter()
        .find(|union| union.attrs["DW_AT_byte_size"] == "4")
        .expect("the uint<32> payload sizes the union");
    assert_eq!(
        members(&dies, widest),
        vec![("data".to_string(), 0), ("pair".to_string(), 0)],
        "every payload member starts at the same union offset"
    );
    // Two hardware bits, two debug bytes: the union of `Bits2` is two bytes.
    assert!(
        unions
            .iter()
            .any(|union| union.attrs["DW_AT_byte_size"] == "2"),
        "an expanded payload keeps its whole debug bytes"
    );
    let bag = named(&dies, "DW_TAG_structure_type", "Bag");
    assert_eq!(bag.len(), 1);
    assert_eq!(bag[0].attrs["DW_AT_byte_size"], "3");
    // The payload-free enum keeps its shape: an enumeration type, not a struct.
    assert_eq!(named(&dies, "DW_TAG_enumeration_type", "State").len(), 1);
    assert!(named(&dies, "DW_TAG_structure_type", "State").is_empty());
    // A field after a payload enum starts after the whole union.
    let holder = named(&dies, "DW_TAG_structure_type", "Holder");
    assert_eq!(holder.len(), 1);
    assert_eq!(
        members(&dies, holder[0]),
        vec![
            ("before".to_string(), 0),
            ("choice".to_string(), 1),
            ("after".to_string(), 3)
        ],
        "the byte after the enum is start + 1 + the two byte Opt slot"
    );
}

// ---------------------------------------------------------------------------
// GDB: symbols, offsets, sizes and the tick anchor
// ---------------------------------------------------------------------------

#[test]
fn gdb_reads_the_virtual_layout() {
    let (dir, layout) = build("gdb-layout");
    let base = layout.data_base;
    let address = |name: &str, add: u64| base + expected_offset(name) + add;
    let commands: Vec<String> = vec![
        "p/x (unsigned long)&top__u0__count".into(),
        "p/x (unsigned long)&'top__u1__count'".into(),
        "p/x (unsigned long)&top__word.addr".into(),
        "p/x (unsigned long)&top__word.flag".into(),
        "p/x (unsigned long)&top__word.tag".into(),
        "p/x (unsigned long)&top__lanes[4]".into(),
        "p/x (unsigned long)&top__packet.payload[2]".into(),
        "p/x (unsigned long)&top__matrix[1].lo".into(),
        "p/x (unsigned long)&top__message.payload.data".into(),
        "p/x (unsigned long)&top__message.payload.pair".into(),
        "p/x (unsigned long)&top__delta.payload.move".into(),
        "p/x (unsigned long)&top__bag.payload.bits.b".into(),
        "p/x (unsigned long)&top__holder.after".into(),
        "p/x (unsigned long)&top__holder.choice.payload.some".into(),
        "p/x (unsigned long)&top__queue[2]".into(),
        "p/x (unsigned long)&top__queue[1].payload.some".into(),
        "p sizeof(top__word)".into(),
        "p sizeof(top__option)".into(),
        "p sizeof(top__message)".into(),
        "p sizeof(top__delta)".into(),
        "p sizeof(top__bag)".into(),
        "p sizeof(top__holder)".into(),
        "p sizeof(top__queue)".into(),
        "p sizeof(top__bits)".into(),
        "p sizeof(top__lanes)".into(),
        "p sizeof(top__wide)".into(),
        "p sizeof(top__signed)".into(),
        "p sizeof(top__packet)".into(),
        "p sizeof(top__matrix)".into(),
        "info address top__u0__count".into(),
        "ptype top__state".into(),
        "ptype top__wide".into(),
        "ptype top__message".into(),
        "ptype top__message.payload".into(),
        "ptype top__bag".into(),
        "ptype top__bag.payload".into(),
        "ptype top__holder".into(),
    ];
    let output = gdb(&dir.elf(), &commands);

    for (what, expected) in [
        ("&top__u0__count", address("top__u0__count", 0)),
        ("&'top__u1__count'", address("top__u1__count", 0)),
        ("&top__word.addr", address("top__word", 1)),
        ("&top__word.flag", address("top__word", 5)),
        ("&top__word.tag", address("top__word", 6)),
        ("&top__lanes[4]", address("top__lanes", 4)),
        ("&top__packet.payload[2]", address("top__packet", 3)),
        ("&top__matrix[1].lo", address("top__matrix", 3)),
        ("&top__message.payload.data", address("top__message", 1)),
        (
            "&top__message.payload.pair",
            address("top__message", 1),
            // The union is one region: the inactive interpretation aliases it.
        ),
        ("&top__delta.payload.move", address("top__delta", 1)),
        ("&top__bag.payload.bits.b", address("top__bag", 2)),
        ("&top__holder.after", address("top__holder", 3)),
        (
            "&top__holder.choice.payload.some",
            address("top__holder", 2),
        ),
        ("&top__queue[2]", address("top__queue", 4)),
        ("&top__queue[1].payload.some", address("top__queue", 3)),
    ] {
        assert!(
            output.contains(&format!("= 0x{expected:x}\n")),
            "GDB must place {what} at {expected:#x}:\n{output}"
        );
    }
    for (what, size) in [
        ("top__word", 7),
        ("top__bits", 2),
        ("top__lanes", 5),
        ("top__wide", 17),
        ("top__signed", 2),
        ("top__packet", 5),
        ("top__matrix", 4),
        ("top__option", 2),
        ("top__message", 5),
        ("top__delta", 3),
        ("top__bag", 3),
        ("top__holder", 5),
        ("top__queue", 6),
    ] {
        assert!(
            output.contains(&format!("= {size}\n")),
            "sizeof({what}) must be {size}:\n{output}"
        );
    }
    assert!(
        output.contains(&format!(
            "\"top__u0__count\" is static storage at address 0x{:x}.",
            address("top__u0__count", 0)
        )),
        "the symbol table must agree with the DWARF location:\n{output}"
    );
    assert!(
        output.contains("idle") && output.contains("run") && output.contains("halt"),
        "the enum tag must stay printable by variant name:\n{output}"
    );
    assert!(
        output.contains("uint<129>") && output.contains("[17]"),
        "a 129 bit value keeps its width as a byte array:\n{output}"
    );
    assert!(
        output.contains("enum Msg_tag tag") && output.contains("union payload payload"),
        "a payload enum is a tag plus a union of the variant payloads:\n{output}"
    );
    // The union names every variant's payload; the widest one sizes it, so the
    // uint<32> payload and the expanded-field payload both have to appear.
    assert!(
        output.contains("uint<32> data") && output.contains("struct Pair8 pair"),
        "every variant payload stays a named union member:\n{output}"
    );
    assert!(
        output.contains("Bits2"),
        "a payload of expanded fields keeps its own struct:\n{output}"
    );
    assert!(
        output.contains("Holder") && output.contains("after"),
        "the field after a payload enum stays addressable:\n{output}"
    );
}

#[test]
fn gdb_resolves_break_tick_to_the_anchor() {
    let (dir, layout) = build("gdb-break");
    // The target shadows the PC into the anchor slot while GDB finishes its
    // internal step-over, so the whole 8 byte slot must be one statement range.
    let mut commands = vec!["break tick".into(), "info break".into()];
    commands.push(format!("info line *{:#x}", layout.tick_pc));
    commands.push(format!("info line *{:#x}", layout.tick_pc + 1));
    // A front end lists the same static block `info variables` reports, so the
    // two views have to agree on which symbols exist.
    commands.push("info variables".into());
    let output = gdb(&dir.elf(), &commands);

    assert!(
        output.contains(&format!("Breakpoint 1 at {:#x}", layout.tick_pc)),
        "`break tick` must resolve to the anchor {:#x}:\n{output}",
        layout.tick_pc
    );
    assert_eq!(
        output.matches(" in tick at ").count(),
        1,
        "the anchor is one location; per-point timing is the target's job:\n{output}"
    );
    assert!(
        !output.contains("<MULTIPLE>"),
        "`break tick` must not fan out to several locations:\n{output}"
    );
    let file = layout
        .source_path
        .file_name()
        .expect("a listing name")
        .to_string_lossy()
        .into_owned();
    let anchor = format!(
        "Line {} of \"{file}\" starts at address {:#x}",
        POINTS + 1,
        layout.tick_pc
    );
    assert_eq!(
        output.matches(&anchor).count(),
        2,
        "the anchor and the anchor's second byte are one statement range:\n{output}"
    );
    for (name, _, _) in EXPECTED_SLOTS {
        assert!(
            output.contains(name),
            "`info variables` must list {name}, the same list a front end shows:\n{output}"
        );
    }
}

// ---------------------------------------------------------------------------
// The layout contract itself, and its error paths
// ---------------------------------------------------------------------------

#[test]
fn layout_is_deterministic_and_the_addresses_do_not_overlap() {
    let (dir, layout) = build("layout");

    assert_eq!(layout.code_base, symbols::CODE_BASE);
    assert_eq!(layout.point_pc.len(), POINTS);
    for (index, pc) in layout.point_pc.iter().enumerate() {
        assert_eq!(*pc, layout.code_base + index as u64 * symbols::POINT_SLOT);
        assert_eq!(layout.pc_point[pc], index);
    }
    assert_eq!(
        layout.tick_pc,
        layout.code_base + POINTS as u64 * symbols::POINT_SLOT
    );
    assert_eq!(layout.text_end, layout.tick_pc + symbols::POINT_SLOT);
    assert_eq!(layout.data_base, layout.text_end.div_ceil(4096) * 4096);
    assert_eq!(layout.data_end, layout.data_base + DATA_LEN);
    assert!(!layout.pc_point.contains_key(&layout.tick_pc));
    assert_eq!(layout.text_byte(layout.code_base), Some(0x90));
    assert_eq!(layout.text_byte(layout.text_end), None);

    assert_eq!(layout.vars.len(), EXPECTED_SLOTS.len());
    let mut addresses = BTreeMap::new();
    for (index, ((name, offset, size), slot)) in EXPECTED_SLOTS.iter().zip(&layout.vars).enumerate()
    {
        assert_eq!(slot.symbol_name, *name);
        assert_eq!(slot.addr, layout.data_base + offset);
        assert_eq!(slot.size, *size, "{name} size");
        assert_eq!(slot.kind.byte_size(), *size, "{name} kind agrees");
        assert_eq!(layout.addr_vars[&slot.addr], index);
        assert_eq!(
            layout
                .var_at(slot.addr + size - 1)
                .map(|found| found.id.as_str()),
            Some(slot.id.as_str())
        );
        addresses.insert(slot.addr, index);
    }
    assert_eq!(layout.addr_vars, addresses);
    assert_eq!(layout.var_at(layout.data_end), None);

    // The same input must produce the same bytes and the same listing.
    let first = std::fs::read(dir.elf()).unwrap();
    let listing = std::fs::read(&layout.source_path).unwrap();
    let rebuilt = symbols::build_symbol_elf(&fixture_meta(), &LABELS, &dir.elf()).unwrap();
    assert_eq!(rebuilt.point_pc, layout.point_pc);
    assert_eq!(rebuilt.tick_pc, layout.tick_pc);
    assert_eq!(rebuilt.source_path, layout.source_path);
    assert_eq!(std::fs::read(dir.elf()).unwrap(), first);
    assert_eq!(std::fs::read(&rebuilt.source_path).unwrap(), listing);
    let listing = String::from_utf8(listing).unwrap();
    assert_eq!(
        listing.lines().count(),
        POINTS + 1,
        "one listing line per point, plus the anchor"
    );
    for (index, label) in LABELS.iter().enumerate() {
        assert!(
            listing.contains(&format!("observation point {index}; label {label:#018x}")),
            "the listing must name every point:\n{listing}"
        );
    }
}

#[test]
fn unrepresentable_or_ambiguous_input_is_refused() {
    let dir = TempDir::new("errors");
    let elf = dir.elf();

    // No observation points: there would be no timeline to debug.
    let error = symbols::build_symbol_elf(&fixture_meta(), &[], &elf).unwrap_err();
    assert!(
        matches!(error, Error::InvalidMetadata { .. }),
        "unexpected error: {error}"
    );

    // A payload enum is a supported shape now, so the fixture itself is the
    // positive case; what is still refused is a symbol name that collides.

    // Two variables that sanitize to one GDB name must not share a slot.
    let mut meta = fixture_meta();
    meta.variables
        .push(variable("12", "top.u0", "count", 22, uint(8)));
    let error = symbols::build_symbol_elf(&meta, &LABELS, &elf).unwrap_err();
    assert!(
        matches!(error, Error::InvalidMetadata { .. }),
        "unexpected error: {error}"
    );

    // A missing directory is an I/O error that keeps the path.
    let missing = dir.0.join("nope").join("debug.elf");
    let error = symbols::build_symbol_elf(&fixture_meta(), &LABELS, &missing).unwrap_err();
    match error {
        Error::Io { path, .. } => assert_eq!(path, missing),
        other => panic!("unexpected error: {other}"),
    }
}
