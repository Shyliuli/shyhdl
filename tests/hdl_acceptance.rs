//! End-to-end acceptance of the M1 frontend: the AC-MVP-001..008 and
//! AC-MVP-015 acceptance cases.
//!
//! Every test here runs the real pipeline: ShyHDL source goes through
//! `shyhdl::frontend::compile`, the Verilog it produces is compiled by an
//! independent Verilog toolchain (`iverilog`) and executed (`vvp`) against a
//! hand-written testbench, and every output is compared with a reference model
//! written in that testbench. No test inspects frontend internals to reach its
//! verdict; the only exception is the debug-metadata assertion, which reads the
//! public `CompiledDesign::metadata` that the debugger consumes.
//!
//! The testbenches are hand-written Verilog on purpose. A failure in this file
//! must mean the generated DUT is wrong, never that ShyHDL's own testbench
//! generator is wrong; the generated testbench path is verified separately.
//!
//! # Toolchain
//!
//! `iverilog`, `vvp` and GNU `timeout` must be on `PATH` (the repository README
//! documents how to put them there). A missing tool fails the test with a
//! diagnostic message. It is deliberately not a skip: a skipped acceptance test
//! silently turns into a pass.
//!
//! Verilog is compiled as Verilog-2005 (`-g2005`), because that is what the
//! product declares it emits. A laxer mode would accept constructs a strict
//! 2005 tool rejects, so the gate would stop catching them.
//!
//! Every external call runs under a wall-clock limit. A combinational loop that
//! never settles does not fail in `vvp`, it spins forever, so a run that has to
//! be killed fails the test and names the likely cause instead of hanging the
//! whole test run.
//!
//! # Verdict protocol
//!
//! Each testbench prints exactly one `SHYHDL-AC-PASS <group> ...` line when the
//! design behaved, and `SHYHDL-AC-FAIL <group> ...` lines otherwise. A run that
//! prints no verdict fails here even when it exits successfully, so a testbench
//! that dies early can never look like a pass.
//!
//! # Opt-in diagnosis
//!
//! `selfcheck/` holds hand-written reference DUTs. One `#[ignore]`d test runs
//! the testbenches against them, to tell a broken testbench apart from a broken
//! design when an acceptance test fails. It is not part of the gate and never
//! runs by default.
//!
//! Build artefacts stay in `target/hdl_acceptance/<test>/` for inspection.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use shyhdl::frontend::CompiledDesign;
use shyhdl::metadata::{TypeField, ValueType, Variable};

/// ShyHDL fixtures are embedded, so a missing fixture is a compile error and
/// not a test that quietly does nothing.
macro_rules! shy_source {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hdl_acceptance/",
            $name
        ))
    };
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures_dir() -> PathBuf {
    manifest_dir().join("tests/fixtures/hdl_acceptance")
}

/// Path of a committed testbench or reference-DUT fixture.
fn fixture(relative: &str) -> PathBuf {
    let path = fixtures_dir().join(relative);
    assert!(
        path.is_file(),
        "missing Verilog fixture {}: the acceptance test cannot run without it",
        path.display()
    );
    path
}

// --------------------------------------------------------------------- tools

/// Locate an external tool on `PATH`.
///
/// A tool that is not installed is a failure whose message names the tool and
/// the `PATH` that was searched. It is deliberately not a skip: an acceptance
/// test that cannot run must never report success.
fn tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "`{name}` was not found on PATH, so the HDL acceptance tests cannot run.\n\
         Install iverilog/vvp, or source the ShyHDL toolchain environment so that both are on PATH.\n\
         PATH={}\n\
         This is a failure and not a skip: an acceptance test that cannot run must never report success.",
        path.to_string_lossy()
    );
}

/// Wall-clock limit for one external Verilog tool invocation.
///
/// A generated combinational loop does not fail in `vvp`: it spins forever. So
/// every external call is bounded, and a call that has to be killed fails the
/// test with the cause named instead of hanging the whole test run.
const VERILOG_TIMEOUT_SECS: &str = "120";

/// Run one external tool under GNU `timeout`.
///
/// `timeout` itself is required: a missing limiter is a failure for the same
/// reason a missing `iverilog` is, because an acceptance test must not silently
/// degrade into something weaker than what it claims to check.
fn run_limited(what: &str, program: &Path, args: &[OsString]) -> std::process::Output {
    let limiter = tool("timeout");
    let mut command = Command::new(&limiter);
    command
        .arg("-k")
        .arg("5")
        .arg(VERILOG_TIMEOUT_SECS)
        .arg(program)
        .args(args);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", limiter.display()));
    if matches!(output.status.code(), Some(124) | Some(137)) {
        panic!(
            "{what} did not finish within {VERILOG_TIMEOUT_SECS}s and was killed.\n\
             A combinational loop that never settles is the usual cause.\n\
             command: {} {}",
            program.display(),
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    output
}

// ---------------------------------------------------------------- workspaces

/// A per-test directory under `target/`, wiped before each run so a stale
/// artefact can never be mistaken for a fresh one.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = manifest_dir().join("target/hdl_acceptance").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", dir.display()));
        Self { dir }
    }

    fn write(&self, file: &str, contents: &str) -> PathBuf {
        let path = self.dir.join(file);
        fs::write(&path, contents)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
        path
    }
}

// ------------------------------------------------------------------ verilog

/// The captured verdict of one `vvp` run.
struct Verdict {
    stdout: String,
    command: String,
}

impl Verdict {
    fn pass_marker(&self, group: &str) -> bool {
        self.stdout
            .lines()
            .any(|line| line.starts_with(&format!("SHYHDL-AC-PASS {group}")))
    }

    fn fail_lines(&self, group: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with(&format!("SHYHDL-AC-FAIL {group}")))
            .collect()
    }
}

fn run_tb(workspace: &Workspace, stem: &str, top: &str, sources: &[PathBuf]) -> Verdict {
    let iverilog = tool("iverilog");
    let vvp = tool("vvp");
    let output_file = workspace.dir.join(format!("{stem}.vvp"));

    // The declared product is Verilog-2005, so the gate compiles it as
    // Verilog-2005. A laxer language mode accepts constructs the product must
    // not emit, which would hide a real portability defect.
    let mut compile_args: Vec<OsString> = vec![
        "-g2005".into(),
        "-o".into(),
        output_file.clone().into_os_string(),
        "-s".into(),
        top.into(),
    ];
    for source in sources {
        compile_args.push(source.clone().into_os_string());
    }
    let compiled = run_limited("iverilog", &iverilog, &compile_args);
    assert!(
        compiled.status.success(),
        "iverilog failed to compile the DUT and the testbench:\n\
         command: {} {}\n\
         stderr:\n{}",
        iverilog.display(),
        sources
            .iter()
            .map(|s| s.display().to_string())
            .collect::<Vec<_>>()
            .join(" "),
        String::from_utf8_lossy(&compiled.stderr)
    );

    let simulated = run_limited("vvp", &vvp, &[output_file.clone().into_os_string()]);
    let stdout = String::from_utf8_lossy(&simulated.stdout).into_owned();
    assert!(
        simulated.status.success(),
        "vvp exited with {} for {}:\n{stdout}\nstderr:\n{}",
        simulated.status,
        output_file.display(),
        String::from_utf8_lossy(&simulated.stderr)
    );
    Verdict {
        stdout,
        command: format!("{} {}", vvp.display(), output_file.display()),
    }
}

/// Require the testbench to have behaved *and* to have said so.
fn assert_passed(verdict: &Verdict, group: &str) {
    let failures = verdict.fail_lines(group);
    assert!(
        failures.is_empty(),
        "the design under test disagreed with the reference model ({}):\n{}",
        verdict.command,
        verdict.stdout
    );
    assert!(
        verdict.pass_marker(group),
        "the run printed no `SHYHDL-AC-PASS {group}` marker, so it proves nothing ({}):\n{}",
        verdict.command,
        verdict.stdout
    );
}

/// Require the testbench to have *rejected* the design: a sensitivity check, so
/// that a vacuous pass cannot hide behind a testbench that never compares.
fn assert_failed(verdict: &Verdict, group: &str) {
    assert!(
        !verdict.pass_marker(group),
        "expected the run to reject the design, but it printed a PASS marker ({}):\n{}",
        verdict.command,
        verdict.stdout
    );
    assert!(
        !verdict.fail_lines(group).is_empty(),
        "expected a `SHYHDL-AC-FAIL {group}` marker ({}):\n{}",
        verdict.command,
        verdict.stdout
    );
}

// ------------------------------------------------------------------ frontend

fn compile(source: &str, source_name: &str, top: &str) -> CompiledDesign {
    shyhdl::frontend::compile(source, source_name, top).unwrap_or_else(|diagnostic| {
        panic!(
            "the frontend rejected `{source_name}` with top `{top}`:\n{}",
            diagnostic.to_report_string()
        )
    })
}

fn compile_fixture(file: &str, source: &str, top: &str) -> CompiledDesign {
    let name = format!("tests/fixtures/hdl_acceptance/{file}");
    compile(source, &name, top)
}

/// The text of one emitted module, from `module <name>` to its `endmodule`.
fn module_body<'a>(verilog: &'a str, name: &str) -> Option<&'a str> {
    let start = verilog.find(&format!("module {name} "))?;
    let rest = &verilog[start..];
    let end = rest.find("endmodule")?;
    Some(&rest[..end])
}

/// True when `body` contains an instantiation of `module` labelled `instance`.
fn instantiates(body: &str, module: &str, instance: &str) -> bool {
    body.lines()
        .any(|line| line.contains(module) && line.contains(&format!(" {instance} ")))
}

fn width_of(ty: &ValueType) -> u32 {
    match ty {
        ValueType::Bool => 1,
        ValueType::UInt { width } | ValueType::SInt { width } => *width,
        ValueType::Struct { fields, .. } => fields.iter().map(|field| width_of(&field.ty)).sum(),
        ValueType::Array { element, len } => width_of(element) * (*len as u32),
        other => panic!("no width is defined for {other:?}"),
    }
}

fn variable<'a>(variables: &'a [Variable], rtl_name: &str) -> &'a Variable {
    variables
        .iter()
        .find(|variable| variable.rtl_name == rtl_name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = variables.iter().map(|v| v.rtl_name.as_str()).collect();
            panic!("no metadata variable is named `{rtl_name}`; the design has {names:?}")
        })
}

// ------------------------------------------------------------------- AC-001..004

/// AC-MVP-001, AC-MVP-002, AC-MVP-003 and AC-MVP-004: all 65536 input pairs of
/// the 8-bit adder, the 1-bit adder, the wrapper that instantiates the adder
/// and connects it by assignment, and the two-level
/// hierarchy, each compared with a bit-level ripple-carry reference model.
#[test]
fn ac001_ac002_ac003_ac004_adders_match_an_independent_reference_model() {
    let compiled = compile_fixture("adder.shy.rs", shy_source!("adder.shy.rs"), "AdderTop");
    let workspace = Workspace::new("adders");
    let dut = workspace.write("adder.v", &compiled.verilog);

    let verdict = run_tb(
        &workspace,
        "tb_adder",
        "tb_adder",
        &[fixture("tb_adder.v"), dut],
    );
    assert_passed(&verdict, "adder");

    // AC-MVP-001 asks for all 65536 pairs; a smaller sweep that happened to
    // agree must not pass.
    assert!(
        verdict.stdout.contains("checks8=65536"),
        "the adder testbench must report the full 65536 exhaustive comparisons:\n{}",
        verdict.stdout
    );

    // AC-MVP-003: the wrapper is structural wiring, not a call into `run()`.
    let wrapper = module_body(&compiled.verilog, "Adder8")
        .unwrap_or_else(|| panic!("Adder8 was not emitted:\n{}", compiled.verilog));
    assert!(
        instantiates(wrapper, "Add_N8", "add"),
        "Adder8 must instantiate the specialised child Add_N8:\n{wrapper}"
    );
    // Every child port has to be connected, outputs included. A missing output
    // connection leaves the parent's net undriven, which is exactly the defect
    // this assertion caught once; the wrapper's own output copies may be wiring
    // or the implicit combinational process, and that is not what is at stake
    // here.
    for port in ["a", "b", "sum", "carry"] {
        assert!(
            wrapper.contains(&format!(".{port}(")),
            "the child instantiation must connect `{port}`:\n{wrapper}"
        );
    }

    // AC-MVP-004: the same source struct specialised twice, and one
    // specialisation instantiated twice, all reachable from the top.
    let top = module_body(&compiled.verilog, "AdderTop")
        .unwrap_or_else(|| panic!("AdderTop was not emitted:\n{}", compiled.verilog));
    for instance in ["one", "eight", "other8", "wrapper"] {
        assert!(
            top.lines()
                .any(|line| line.contains(&format!(" {instance} ("))),
            "AdderTop must instantiate `{instance}`:\n{top}"
        );
    }
}

/// AC-MVP-003 and AC-MVP-004 at the metadata level: two instances of the *same*
/// specialisation must keep two independent instance paths, and every variable
/// id must stay unique. Collapsing both instances onto the module name would
/// also duplicate the ids, which the debugger cannot survive.
///
/// The whole instance tree is checked, not just the first level: the nested
/// instance inside a child module must get its own path too, so a metadata
/// builder that stops after one level is caught here.
#[test]
fn ac003_ac004_two_instances_of_one_specialisation_keep_independent_debug_paths() {
    let compiled = compile_fixture("adder.shy.rs", shy_source!("adder.shy.rs"), "AdderTop");
    let variables = &compiled.metadata.variables;
    assert_eq!(compiled.metadata.top, "AdderTop");

    let mut ids: Vec<&str> = variables.iter().map(|v| v.id.as_str()).collect();
    ids.sort_unstable();
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(
        ids.len(),
        unique.len(),
        "variable ids must be unique, but {} of {} collide: {:?}",
        ids.len() - unique.len(),
        ids.len(),
        variables.iter().map(|v| &v.id).collect::<Vec<_>>()
    );

    let paths_for = |field: &str| -> Vec<String> {
        let mut paths: Vec<String> = variables
            .iter()
            .filter(|v| v.instance_path.contains(field))
            .map(|v| v.instance_path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    };
    let eight = paths_for("eight");
    let other8 = paths_for("other8");
    assert!(
        !eight.is_empty(),
        "no metadata variable is attributed to instance `eight`: {:?}",
        variables
            .iter()
            .map(|v| &v.instance_path)
            .collect::<Vec<_>>()
    );
    assert!(
        !other8.is_empty(),
        "no metadata variable is attributed to instance `other8`"
    );
    assert!(
        eight.iter().all(|path| !other8.contains(path)),
        "the two instances of `Add_N8` share an instance path: eight={eight:?} other8={other8:?}"
    );

    // The whole instance tree, not just the first level: `wrapper` is an
    // `Adder8`, and the `Add<8>` inside it is a third instance of `Add_N8`.
    let deep = paths_for("wrapper.add");
    assert!(
        !deep.is_empty(),
        "no metadata variable is attributed to the nested instance `wrapper.add`, so the \
         instance tree stops after one level: {:?}",
        variables
            .iter()
            .map(|v| &v.instance_path)
            .collect::<Vec<_>>()
    );
    assert!(
        deep.iter()
            .all(|path| !eight.contains(path) && !other8.contains(path)),
        "the nested instance shares a path with a top-level one: \
         deep={deep:?} eight={eight:?} other8={other8:?}"
    );
    assert!(
        variables.iter().all(|v| v.instance_path != "Add_N8"),
        "a child variable kept the flat module name as its instance path, which cannot \
         distinguish `eight` from `other8`: {:?}",
        variables
            .iter()
            .map(|v| &v.instance_path)
            .collect::<Vec<_>>()
    );
}

// ------------------------------------------------------------------- AC-005..008

/// AC-MVP-005, AC-MVP-006, AC-MVP-007 and AC-MVP-008, driven by a testbench
/// that owns the clock and counts its edges.
#[test]
fn ac005_ac006_ac007_ac008_sequential_reset_enable_and_pipeline() {
    let compiled = compile_fixture(
        "sequential.shy.rs",
        shy_source!("sequential.shy.rs"),
        "SeqTop",
    );
    let workspace = Workspace::new("sequential");
    let dut = workspace.write("sequential.v", &compiled.verilog);

    let verdict = run_tb(
        &workspace,
        "tb_sequential",
        "tb_sequential",
        &[fixture("tb_sequential.v"), dut],
    );
    assert_passed(&verdict, "sequential");
}

// --------------------------------------------------------------------- AC-015

/// AC-MVP-015: the 44-bit struct is packed most significant field first with no
/// padding, and every field survives a write/read round trip through two
/// separate modules.
#[test]
fn ac015_44_bit_struct_round_trips_field_by_field() {
    let workspace = Workspace::new("packet");
    let make = compile_fixture(
        "packet_make.shy.rs",
        shy_source!("packet_make.shy.rs"),
        "PacketMake",
    );
    let split = compile_fixture(
        "packet_split.shy.rs",
        shy_source!("packet_split.shy.rs"),
        "PacketSplit",
    );

    // The debugger reads the same layout from the metadata.
    let pkt = variable(&make.metadata.variables, "pkt");
    match &pkt.ty {
        ValueType::Struct { name, fields } => {
            assert_eq!(name, "Packet");
            let shape: Vec<(&str, u32)> = fields
                .iter()
                .map(|field| (field.name.as_str(), width_of(&field.ty)))
                .collect();
            assert_eq!(
                shape,
                vec![("op", 4), ("addr", 32), ("flag", 1), ("tag", 7)],
                "Packet must be 4 + 32 + 1 + 7 bits in declaration order"
            );
            assert_eq!(
                width_of(&pkt.ty),
                44,
                "Packet must be exactly 44 bits, with no padding"
            );
        }
        other => panic!("`pkt` should be a struct-typed port, got {other:?}"),
    }

    let dut_make = workspace.write("packet_make.v", &make.verilog);
    let dut_split = workspace.write("packet_split.v", &split.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_packet",
        "tb_packet",
        &[fixture("tb_packet.v"), dut_make, dut_split],
    );
    assert_passed(&verdict, "packet");
}

// --------------------------------------------------------------- M4 signed

/// `int<W>` semantics, checked against Verilog integer arithmetic and its own
/// truncation: same-width arithmetic wraps, `>>` is arithmetic on a signed value
/// and logical on an unsigned one, the six comparisons are signed, an equal-width
/// `as uint<W>` compares the same bits as unsigned, `sext`/`trunc` keep the
/// signed type, a signed struct field stays signed when read whole while its
/// part-select is raw bits, and a negative literal keeps its sign through a
/// constant branch and a local.
///
/// The testbench also insists on witnesses: each signed-versus-unsigned
/// disagreement, and the signed-versus-logical shift, has to occur somewhere, so
/// a design whose "signed" operators are really the unsigned ones cannot pass.
#[test]
fn m4_signed_semantics_match_an_independent_integer_reference() {
    let compiled = compile_fixture("signed.shy.rs", shy_source!("signed.shy.rs"), "SignedTop");

    // A struct field is where signedness could be lost silently, so the metadata
    // has to describe `s` as signed next to the unsigned `u`. The variable id is
    // the instance path of the field in the source (`SignedTop.pair`) joined with
    // the port name, not the Verilog module name of the child.
    let pkt = compiled
        .metadata
        .variables
        .iter()
        .find(|variable| variable.id == "SignedTop.pair/p")
        .unwrap_or_else(|| {
            let ids: Vec<&str> = compiled
                .metadata
                .variables
                .iter()
                .map(|variable| variable.id.as_str())
                .collect();
            panic!("no variable `SignedTop.pair/p` in the metadata; it has: {ids:?}")
        });
    match &pkt.ty {
        ValueType::Struct { name, fields } => {
            assert_eq!(name, "Pair");
            let expected_s = ValueType::SInt { width: 4 };
            let expected_u = ValueType::UInt { width: 4 };
            let shape: Vec<(&str, &ValueType)> = fields
                .iter()
                .map(|field| (field.name.as_str(), &field.ty))
                .collect();
            assert_eq!(
                shape,
                vec![("s", &expected_s), ("u", &expected_u)],
                "Pair must keep `s` signed and `u` unsigned, four bits each"
            );
        }
        other => panic!("`p` should have a struct type, got {other:?}"),
    }

    let workspace = Workspace::new("signed");
    let dut = workspace.write("signed.v", &compiled.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_signed",
        "tb_signed",
        &[fixture("tb_signed.v"), dut],
    );
    assert_passed(&verdict, "signed");
}

// --------------------------------------------------------------- M4 arrays

/// The metadata variable with this exact id.
///
/// An id is the instance path of the source field joined with the signal name,
/// for example `ArrTop.konst/repeated`: the *child's* field name in the parent,
/// never the Verilog module name of the child. The failure names every id the
/// design does have, so a lookup that can never match is visible immediately.
fn variable_by_id<'a>(compiled: &'a CompiledDesign, id: &str) -> &'a Variable {
    compiled
        .metadata
        .variables
        .iter()
        .find(|variable| variable.id == id)
        .unwrap_or_else(|| {
            let ids: Vec<&str> = compiled
                .metadata
                .variables
                .iter()
                .map(|variable| variable.id.as_str())
                .collect();
            panic!("no metadata variable `{id}`; it has: {ids:?}")
        })
}

/// Fixed-length arrays: the metadata has to describe the element type and the
/// length at every nesting level, and the generated RTL has to behave like the
/// hand-written reference over the vectors in `tb_arrays.v` (element 0 in the
/// least significant bits, an out-of-range dynamic index reading the standard
/// unknown, a combinational dynamic write defaulted over the whole array, and a
/// sequential one leaving the other elements on their old value).
#[test]
fn m4_arrays_pack_element_zero_lowest_and_survive_a_hierarchy() {
    let compiled = compile_fixture("arrays.shy.rs", shy_source!("arrays.shy.rs"), "ArrTop");

    // `[uint<4>; 3]` is one value: three unsigned nibbles.
    let nibble = ValueType::UInt { width: 4 };
    let repeated = variable_by_id(&compiled, "ArrTop.konst/repeated");
    assert_eq!(
        repeated.ty,
        ValueType::Array {
            element: Box::new(nibble.clone()),
            len: 3
        },
        "an array needs its element type and its length"
    );

    // An element type that is itself an array nests the same way.
    let inner = ValueType::Array {
        element: Box::new(nibble.clone()),
        len: 2,
    };
    let nested = variable_by_id(&compiled, "ArrTop.konst/nested");
    assert_eq!(
        nested.ty,
        ValueType::Array {
            element: Box::new(inner),
            len: 2
        },
        "a nested array needs both lengths"
    );

    // A struct holding an array keeps each field's own type.
    let items = ValueType::Array {
        element: Box::new(nibble.clone()),
        len: 2,
    };
    let bundle = ValueType::Struct {
        name: "Bundle".to_string(),
        fields: vec![
            TypeField {
                name: "items".to_string(),
                ty: items,
            },
            TypeField {
                name: "flag".to_string(),
                ty: ValueType::Bool,
            },
        ],
    };
    assert_eq!(
        variable_by_id(&compiled, "ArrTop.nest/b").ty,
        bundle,
        "a struct field that is an array must keep its type"
    );

    // An array of structs, whose `tag` field is the signed `int<4>`.
    let cell = ValueType::Struct {
        name: "Cell".to_string(),
        fields: vec![
            TypeField {
                name: "tag".to_string(),
                ty: ValueType::SInt { width: 4 },
            },
            TypeField {
                name: "val".to_string(),
                ty: ValueType::UInt { width: 4 },
            },
        ],
    };
    assert_eq!(
        variable_by_id(&compiled, "ArrTop.nest/cells").ty,
        ValueType::Array {
            element: Box::new(cell),
            len: 2
        },
        "an array of structs must keep every field type, including the signed one"
    );

    let workspace = Workspace::new("arrays");
    let dut = workspace.write("arrays.v", &compiled.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_arrays",
        "tb_arrays",
        &[fixture("tb_arrays.v"), dut],
    );
    assert_passed(&verdict, "arrays");
}

// ----------------------------------------------------------- M4 enums and types

/// A user enum with payloads: the tag is the lowest `max(1, ceil(log2 n))` bits
/// with variants numbered from zero, the payload above it is as wide as the
/// widest variant's, the payload bits a variant does not use are zero, and a
/// named payload packs its fields most significant first. The `match` picks the
/// first arm whose pattern and guard hold, so an arm guarded on the payload
/// takes precedence over a later arm for the same variant, and the tag that has
/// no variant takes the final `_`.
#[test]
fn m4_user_enum_payload_layout_and_match_arm_priority() {
    let compiled = compile_fixture("enums.shy.rs", shy_source!("enums.shy.rs"), "EnumTop");

    // `Msg` is one ten-bit value: a two-bit tag with an eight-bit payload above
    // it, and the three variants numbered from zero in declaration order.
    let msg = variable_by_id(&compiled, "EnumTop.matched/msg");
    match &msg.ty {
        ValueType::Enum {
            name,
            tag_width,
            variants,
        } => {
            assert_eq!(name, "Msg");
            assert_eq!(*tag_width, 2, "three variants need a two-bit tag");
            assert_eq!(variants.len(), 3);
            let shape: Vec<(&str, u64, Option<&ValueType>)> = variants
                .iter()
                .map(|variant| {
                    (
                        variant.name.as_str(),
                        variant.tag,
                        variant.payload.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                shape,
                vec![
                    ("Idle", 0, None),
                    ("Data", 1, Some(&ValueType::UInt { width: 4 })),
                    (
                        "Pair",
                        2,
                        Some(&ValueType::Struct {
                            // A user enum's named payload is named after its
                            // variant, qualified by the enum it belongs to.
                            name: "Msg::Pair".to_string(),
                            fields: vec![
                                TypeField {
                                    name: "hi".to_string(),
                                    ty: ValueType::SInt { width: 4 },
                                },
                                TypeField {
                                    name: "lo".to_string(),
                                    ty: ValueType::UInt { width: 4 },
                                },
                            ],
                        })
                    ),
                ],
                "variants are numbered from zero in declaration order, and the named payload keeps both field types"
            );
        }
        other => panic!("`msg` should have an enum type, got {other:?}"),
    }

    let workspace = Workspace::new("enums");
    let dut = workspace.write("enums.v", &compiled.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_enums",
        "tb_enums",
        &[fixture("tb_enums.v"), dut],
    );
    assert_passed(&verdict, "enums");
}

/// The built-in `Option<T>` and `Result<T, E>`: `None` is tag 0 and `Some` tag 1,
/// `Ok` is tag 0 and `Err` tag 1, and each payload is as wide as the widest case,
/// so a nested `Option<Result<..>>` widens twice. The patterns nest, and the
/// `Option<bool>` case is exhaustive in three arms with no `_` because both
/// payload values are spelled out.
#[test]
fn m4_option_and_result_encoding_with_nested_patterns() {
    let compiled = compile_fixture("options.shy.rs", shy_source!("options.shy.rs"), "OptTop");

    // `Option<uint<4>>`: one tag bit under a four-bit payload.
    let some = variable_by_id(&compiled, "OptTop.make/some");
    match &some.ty {
        ValueType::Enum {
            name,
            tag_width,
            variants,
        } => {
            assert_eq!((name.as_str(), *tag_width), ("Option", 1));
            let shape: Vec<(&str, u64, Option<&ValueType>)> = variants
                .iter()
                .map(|variant| {
                    (
                        variant.name.as_str(),
                        variant.tag,
                        variant.payload.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                shape,
                vec![
                    ("None", 0, None),
                    ("Some", 1, Some(&ValueType::UInt { width: 4 })),
                ],
                "None is tag 0 and Some is tag 1"
            );
        }
        other => panic!("`some` should have an `Option` type, got {other:?}"),
    }

    // `Result<uint<4>, int<8>>`: one tag bit under the widest payload, signed.
    let err = variable_by_id(&compiled, "OptTop.make/err");
    match &err.ty {
        ValueType::Enum {
            name,
            tag_width,
            variants,
        } => {
            assert_eq!((name.as_str(), *tag_width), ("Result", 1));
            let shape: Vec<(&str, u64, Option<&ValueType>)> = variants
                .iter()
                .map(|variant| {
                    (
                        variant.name.as_str(),
                        variant.tag,
                        variant.payload.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                shape,
                vec![
                    ("Ok", 0, Some(&ValueType::UInt { width: 4 })),
                    ("Err", 1, Some(&ValueType::SInt { width: 8 })),
                ],
                "Ok is tag 0 with the unsigned payload, Err is tag 1 with the signed one"
            );
        }
        other => panic!("`err` should have a `Result` type, got {other:?}"),
    }

    // The nested value widens twice: the inner `Result` under the outer tag.
    let deep = variable_by_id(&compiled, "OptTop.make/deep_err");
    match &deep.ty {
        ValueType::Enum {
            name,
            tag_width,
            variants,
        } => {
            assert_eq!((name.as_str(), *tag_width), ("Option", 1));
            assert_eq!(variants.len(), 2);
            match variants[1].payload.as_deref() {
                Some(ValueType::Enum {
                    name,
                    tag_width,
                    variants,
                }) => {
                    assert_eq!((name.as_str(), *tag_width), ("Result", 1));
                    assert_eq!(variants.len(), 2);
                }
                other => panic!("the `Some` payload should be a nested `Result`, got {other:?}"),
            }
        }
        other => panic!("`deep_err` should have an `Option` type, got {other:?}"),
    }

    let workspace = Workspace::new("options");
    let dut = workspace.write("options.v", &compiled.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_options",
        "tb_options",
        &[fixture("tb_options.v"), dut],
    );
    assert_passed(&verdict, "options");
}

/// An enum inside another container: a struct field and an array element both
/// keep the enum's own layout, an array puts element 0 in the least significant
/// bits, and a struct packs its fields most significant first. Equality compares
/// the tag and, for a variant that carries one, its payload: the payload bits a
/// variant does not use never take part, so junk in them cannot change the
/// verdict, while a differing tag always can.
#[test]
fn m4_enum_wrapped_in_struct_and_array_with_payload_aware_equality() {
    let compiled = compile_fixture("wrapped.shy.rs", shy_source!("wrapped.shy.rs"), "WrapTop");

    // `Envelope { msg, extra }`: the enum field first, so it is the high ten bits.
    let env = variable_by_id(&compiled, "WrapTop.make/env_pair");
    match &env.ty {
        ValueType::Struct { name, fields } => {
            assert_eq!(name, "Envelope");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "msg");
            assert_eq!(fields[1].name, "extra");
            assert!(
                matches!(&fields[0].ty, ValueType::Enum { name, .. } if name == "Msg"),
                "the first field must be the enum, got {:?}",
                fields[0].ty
            );
            assert_eq!(fields[1].ty, ValueType::UInt { width: 2 });
        }
        other => panic!("`env_pair` should have a struct type, got {other:?}"),
    }

    // `[Msg; 2]`: two elements, each with the enum's own type.
    let arr = variable_by_id(&compiled, "WrapTop.make/arr");
    match &arr.ty {
        ValueType::Array { element, len } => {
            assert_eq!(*len, 2);
            assert!(
                matches!(element.as_ref(), ValueType::Enum { name, .. } if name == "Msg"),
                "the element type must be the enum, got {element:?}"
            );
        }
        other => panic!("`arr` should have an array type, got {other:?}"),
    }

    let workspace = Workspace::new("wrapped");
    let dut = workspace.write("wrapped.v", &compiled.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_wrapped",
        "tb_wrapped",
        &[fixture("tb_wrapped.v"), dut],
    );
    assert_passed(&verdict, "wrapped");
}

/// `match` in value position: the arm's last expression is the arm's value, a
/// block arm's tail expression is that value, and the expected type comes from
/// the context the `match` sits in. Only the selected arm is evaluated, so a
/// statement in an arm that is not taken does not run, and inside an `always!`
/// arm the non-blocking assignment means a `match` that reads the register it
/// assigns sees the value from before the edge. The testbench also proves the
/// initialiser of an inferred `let` runs once, and that the right operand of a
/// short-circuit `&&`/`||` and the untaken branch of an `if` never run.
#[test]
fn m4_match_in_value_position_evaluates_only_the_selected_arm() {
    let compiled = compile_fixture(
        "match_expr.shy.rs",
        shy_source!("match_expr.shy.rs"),
        "MatchExprTop",
    );

    // The values the match produces keep the width of the arm that is selected.
    assert_eq!(
        variable_by_id(&compiled, "MatchExprTop/rhs").ty,
        ValueType::UInt { width: 8 },
        "the whole right-hand side is eight bits wide"
    );
    assert_eq!(
        variable_by_id(&compiled, "MatchExprTop/inf").ty,
        ValueType::UInt { width: 4 },
        "an untyped `let` takes the width of its arms"
    );
    assert_eq!(
        variable_by_id(&compiled, "MatchExprTop/one_shot").ty,
        ValueType::UInt { width: 4 },
        "the single-evaluation counter is a four-bit value"
    );
    assert_eq!(
        variable_by_id(&compiled, "MatchExprTop/and_out").ty,
        ValueType::Bool,
        "`false && ...` produces a bool"
    );

    let workspace = Workspace::new("match_expr");
    let dut = workspace.write("match_expr.v", &compiled.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_match_expr",
        "tb_match_expr",
        &[fixture("tb_match_expr.v"), dut],
    );
    assert_passed(&verdict, "match_expr");
}

// -------------------------------------------------- AC-MVP-012 and part selects

/// The behaviour statements of a `run` body form one implicit combinational
/// process, evaluated in source order: a later write wins, a statement reads
/// what the statement before it wrote, a run-local `let` keeps the value of the
/// moment it was bound and reaches a child instance input, and a child with no
/// input at all still produces known values instead of waiting for an event.
/// AC-MVP-012 is the overwrite case of this; the assertions are about values,
/// not about which lowering produced them.
#[test]
fn ac012_run_statements_form_one_combinational_process_in_source_order() {
    let overwrite = compile_fixture(
        "overwrite.shy.rs",
        shy_source!("overwrite.shy.rs"),
        "Overwrite",
    );

    let workspace = Workspace::new("overwrite");
    let dut = workspace.write("overwrite.v", &overwrite.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_overwrite",
        "tb_overwrite",
        &[fixture("tb_overwrite.v"), dut],
    );
    assert_passed(&verdict, "overwrite");
}

/// Disjoint partial-width writes into one signal are legal hardware with no
/// combinational loop, even though a signal-level dependency graph contains a
/// `y -> y` edge. Kept in its own test so that a verdict on this construct can
/// never hide, or be hidden by, the AC-MVP-012 verdict.
#[test]
fn disjoint_part_select_writes_are_legal_hardware() {
    let part_select = compile_fixture(
        "partselect.shy.rs",
        shy_source!("partselect.shy.rs"),
        "PartSelect",
    );

    let workspace = Workspace::new("partselect");
    let dut = workspace.write("partselect.v", &part_select.verilog);
    let verdict = run_tb(
        &workspace,
        "tb_partselect",
        "tb_partselect",
        &[fixture("tb_partselect.v"), dut],
    );
    assert_passed(&verdict, "partselect");
}

// ------------------------------------------------------ testbench sensitivity

/// One negative control: the adder testbench must reject a design that has the
/// same interface but a wrong `Add_N8`. Without it, a testbench that compared
/// nothing would pass vacuously. The positive half of the comparison is the
/// real acceptance test above, which requires a PASS against the generated
/// design, so no hand-written correct DUT is needed here as well.
#[test]
fn adder_testbench_rejects_a_design_that_disagrees_with_the_reference_model() {
    let workspace = Workspace::new("adder_negative_control");
    let verdict = run_tb(
        &workspace,
        "tb_adder_mutated",
        "tb_adder",
        &[fixture("tb_adder.v"), fixture("selfcheck/mutated_adder.v")],
    );
    assert_failed(&verdict, "adder");
}

// ------------------------------------------------------------ opt-in diagnosis

/// The hand-written reference DUTs under `selfcheck/` exist to tell a broken
/// testbench apart from a broken design when an acceptance test fails. They are
/// deliberately **not** part of the gate: the gate runs the generated Verilog,
/// and passing a correct hand-written DUT through the same testbench would only
/// repeat a verdict it already reached.
///
/// Run it explicitly when an acceptance failure needs a diagnosis:
///
/// ```text
/// cargo test --test hdl_acceptance -- --ignored
/// ```
///
/// It needs `iverilog`, `vvp` and GNU `timeout` on `PATH`, like every other
/// test in this file.
#[test]
#[ignore = "opt-in diagnosis, not part of the acceptance gate"]
fn reference_duts_are_accepted_by_their_testbenches() {
    let pairs = [
        ("adder", "tb_adder", "reference_adder.v"),
        ("sequential", "tb_sequential", "reference_sequential.v"),
        ("packet", "tb_packet", "reference_packet.v"),
        ("overwrite", "tb_overwrite", "reference_overwrite.v"),
        ("partselect", "tb_partselect", "reference_partselect.v"),
        ("signed", "tb_signed", "reference_signed.v"),
        ("arrays", "tb_arrays", "reference_arrays.v"),
        ("enums", "tb_enums", "reference_enums.v"),
        ("options", "tb_options", "reference_options.v"),
        ("wrapped", "tb_wrapped", "reference_wrapped.v"),
        ("match_expr", "tb_match_expr", "reference_match_expr.v"),
    ];
    let workspace = Workspace::new("reference_duts");
    for (group, bench, reference) in pairs {
        let verdict = run_tb(
            &workspace,
            bench,
            bench,
            &[
                fixture(&format!("{bench}.v")),
                fixture(&format!("selfcheck/{reference}")),
            ],
        );
        assert_passed(&verdict, group);
    }
}
