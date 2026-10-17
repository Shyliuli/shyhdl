//! Contract tests for the ShyHDL frontend: real sources in, Verilog and typed
//! modules out.
//!
//! These tests check what other layers depend on - module names, port
//! directions and widths, instantiations, connection completeness, the shape of
//! the generated processes and the metadata - rather than the implementation
//! behind them.

use shyhdl::frontend::{compile, compile_all, parse_design, PortDir};

/// The Verilog without comments and blank lines, for comparing structure.
fn code_lines(verilog: &str) -> Vec<&str> {
    verilog
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("//"))
        .collect()
}

fn example(name: &str) -> String {
    std::fs::read_to_string(format!("examples/{name}"))
        .unwrap_or_else(|error| panic!("example examples/{name} must be readable: {error}"))
}

/// The expression that drives `target` in the generated Verilog, following the
/// compiler's version wires to the first expression that is not another single
/// name: `assign y = y__v3;` and `assign y__v3 = 4'd1;` resolve to `4'd1`.
fn driven_value(verilog: &str, target: &str) -> String {
    let mut assignments = std::collections::HashMap::new();
    for line in verilog.lines() {
        let Some((lhs, rhs)) = line
            .trim()
            .trim_end_matches(';')
            .strip_prefix("assign ")
            .and_then(|statement| statement.split_once(" = "))
        else {
            continue;
        };
        assignments.insert(lhs.trim().to_string(), rhs.trim().to_string());
    }

    let is_name = |text: &str| {
        !text.is_empty()
            && !text.starts_with(|c: char| c.is_ascii_digit())
            && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };

    let mut current = target.to_string();
    for _ in 0..64 {
        let Some(rhs) = assignments.get(&current).cloned() else {
            break;
        };
        if !is_name(&rhs) {
            return rhs;
        }
        current = rhs;
    }
    panic!("`{target}` is not driven by a resolvable `assign` chain");
}

#[test]
fn adder8_wires_a_comb_child_into_a_registered_parent() {
    let source = example("adder.shy.rs");
    let compiled =
        compile(&source, "examples/adder.shy.rs", "Adder8").expect("adder example compiles");

    assert_eq!(compiled.top().verilog_name, "Adder8");
    assert!(
        compiled.design().module("Adder_N8").is_some(),
        "the generic child is specialised"
    );

    // Ports, in declaration order, with explicit widths.
    let ports: Vec<(&str, PortDir, u32)> = compiled
        .top()
        .ports
        .iter()
        .map(|port| (port.verilog_name.as_str(), port.dir, port.width))
        .collect();
    assert_eq!(
        ports,
        vec![
            ("a", PortDir::In, 8),
            ("b", PortDir::In, 8),
            ("enable", PortDir::In, 1),
            ("rst_n", PortDir::In, 1),
            ("clk", PortDir::In, 1),
            ("sum", PortDir::Out, 8),
            ("carry", PortDir::Out, 1),
        ]
    );

    // The combinational child gets its own module with a build-time width, and
    // its body is the default combinational process: blocking assignments in
    // source order, driven by `always @(*)` because it reads its inputs.
    let child = compiled
        .design()
        .module("Adder_N8")
        .expect("specialised child");
    assert_eq!(child.params.len(), 1);
    assert_eq!(child.params[0].name, "N");
    assert_eq!(child.params[0].value, 8);
    assert!(
        compiled.verilog.contains("reg [8:0] wide;"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("always @(*) begin : comb0"),
        "{}",
        compiled.verilog
    );
    assert!(compiled.verilog.contains("wide = ({1'b0, a} + {1'b0, b});"));
    assert!(compiled.verilog.contains("sum = wide[7:0];"));
    assert!(compiled.verilog.contains("carry = wide[8];"));

    // The instance lists every child port: an output left out would leave its
    // net undriven.
    let instance = compiled
        .top()
        .instances
        .iter()
        .find(|instance| instance.field == "adder")
        .expect("adder instance");
    let connected: Vec<&str> = instance
        .connections
        .iter()
        .map(|connection| connection.port.as_str())
        .collect();
    assert_eq!(connected, vec!["a", "b", "sum", "carry"]);
    assert!(
        compiled.verilog.contains(".sum(adder_sum)"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains(".carry(adder_carry)"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("wire [7:0] adder_sum;"),
        "{}",
        compiled.verilog
    );
    assert!(
        !compiled.verilog.contains("adder.run()"),
        "a child never needs run()"
    );

    // The registered parent: reset in the sensitivity list, state non-blocking.
    assert!(compiled
        .verilog
        .contains("always @(posedge clk or negedge rst_n) begin : seq0"));
    assert!(compiled.verilog.contains("if ((!rst_n)) begin"));
    assert!(compiled.verilog.contains("sum <= adder_sum;"));
    assert!(compiled.verilog.contains("carry <= adder_carry;"));
}

#[test]
fn generic_parameters_are_specialised_at_build_time() {
    let source = example("adder.shy.rs");
    let wide = compile(&source, "examples/adder.shy.rs", "Adder<8>").expect("Adder<8>");
    let narrow = compile(&source, "examples/adder.shy.rs", "Adder<1>").expect("Adder<1>");

    assert!(wide.design().module("Adder_N8").is_some());
    assert!(narrow.design().module("Adder_N1").is_some());

    // A one-bit signal is a scalar: no range, no bit select on it. The body is
    // the default combinational process, so `wide` is a `reg` it drives.
    assert!(
        narrow.verilog.contains("output reg carry"),
        "{}",
        narrow.verilog
    );
    assert!(
        narrow.verilog.contains("reg [1:0] wide;"),
        "{}",
        narrow.verilog
    );
    assert!(
        narrow.verilog.contains("carry = wide[1];"),
        "{}",
        narrow.verilog
    );

    // A generic top without parameters is an error, not a guess.
    let error = compile(&source, "examples/adder.shy.rs", "Adder").expect_err("needs parameters");
    assert_eq!(error.code, shyhdl::frontend::codes::BAD_TYPE);
    assert!(error.message.contains("N"), "{}", error.message);
}

#[test]
fn connection_order_does_not_change_the_topology() {
    let first = r#"
#[hardware_instance]
struct Child { #[In] a: bool, #[Out] y: bool }
impl Hardware for Child { fn run(&mut self) { self.y = self.a; } }

#[hardware_instance]
struct Parent { #[In] a: bool, #[In] b: bool, #[Out] y: bool, #[Out] z: bool, #[instance] kid: Child }
impl Hardware for Parent {
    fn run(&mut self) {
        self.kid.a = self.a;
        self.y = self.kid.y;
        self.z = self.b;
    }
}
"#;
    let swapped = first.replace(
        "        self.kid.a = self.a;\n        self.y = self.kid.y;\n        self.z = self.b;",
        "        self.z = self.b;\n        self.y = self.kid.y;\n        self.kid.a = self.a;",
    );
    let a = compile(first, "order.shy.rs", "Parent").expect("first order");
    let b = compile(&swapped, "order.shy.rs", "Parent").expect("swapped order");
    // Source-location comments move with the source, and independent
    // statements keep their source order in readable output; neither may change
    // which signals are connected to what.
    let mut left = code_lines(&a.verilog);
    let mut right = code_lines(&b.verilog);
    left.sort_unstable();
    right.sort_unstable();
    assert_eq!(
        left, right,
        "connection order must not change the emitted structure"
    );

    // The instance's connections are listed in the child's port order, which the
    // parent's writing order cannot change (REQ-LANG-013).
    let connections = |compiled: &shyhdl::frontend::CompiledDesign| -> Vec<(String, String)> {
        compiled
            .top()
            .instances
            .iter()
            .flat_map(|instance| {
                instance
                    .connections
                    .iter()
                    .map(|connection| {
                        (connection.port.clone(), format!("{:?}", connection.expr.ty))
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    assert_eq!(connections(&a), connections(&b));
}

#[test]
fn register_pair_instantiates_two_widths_with_an_async_reset() {
    let source = example("register.shy.rs");
    let compiled =
        compile(&source, "examples/register.shy.rs", "RegisterPair").expect("register example");

    assert!(compiled.design().module("Register_W8").is_some());
    assert!(compiled.design().module("Register_W4").is_some());

    let wide = compiled
        .design()
        .module("Register_W8")
        .expect("wide register");
    assert_eq!(wide.ports.len(), 5);
    let clk = wide.port("clk").expect("clock port");
    assert_eq!(clk.dir, PortDir::In, "a clock is an ordinary input port");
    assert_eq!(clk.width, 1);
    assert!(compiled
        .verilog
        .contains("always @(posedge clk or negedge rst_n)"));
    assert!(compiled.verilog.contains("output reg [7:0] q"));
    // Both instances are connected, including their clocks.
    let pair = compiled.design().top();
    for field in ["wide", "narrow"] {
        let instance = pair
            .instances
            .iter()
            .find(|i| i.field == field)
            .expect(field);
        assert_eq!(instance.connections.len(), 5, "{field} connects every port");
    }
    assert!(compiled.verilog.contains(".clk(clk)"));
}

#[test]
fn pipeline_commits_state_non_blocking() {
    let source = example("pipeline.shy.rs");
    let compiled = compile(&source, "examples/pipeline.shy.rs", "Pipe8").expect("pipeline example");
    let body = &compiled.verilog;

    assert!(body.contains("always @(posedge clk or negedge rst_n)"));
    // `stage1 = self.d;` then `self.q = stage1;`: both are non-blocking, so the
    // second one samples the previous stage1.
    let stage = body.find("stage1 <= d;").expect("stage1 assignment");
    let commit = body.find("q <= stage1;").expect("q assignment");
    assert!(stage < commit, "the pipeline commits in source order");
    assert!(body.contains("reg [7:0] stage1;"), "{}", body);
    // `stage1` is a run-level `let`, so `stage1_out` reads it through the
    // module signal rather than through the process.
    assert!(body.contains("stage1_out = stage1;"), "{}", body);
}

#[test]
fn packet_struct_packs_high_to_low_without_padding() {
    let source = example("packet.shy.rs");
    let compiled =
        compile(&source, "examples/packet.shy.rs", "PacketMake").expect("packet example");

    let pkt = compiled.top().port("pkt").expect("packed port");
    assert_eq!(pkt.width, 44, "4 + 32 + 1 + 7 bits, no padding");
    match &pkt.ty {
        shyhdl::metadata::ValueType::Struct { name, fields } => {
            assert_eq!(name, "Packet");
            let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
            assert_eq!(names, vec!["op", "addr", "flag", "tag"]);
        }
        other => panic!("expected a struct port, got {other:?}"),
    }
    // Most significant field first.
    assert!(
        compiled.verilog.contains("{op, addr, flag, tag}"),
        "{}",
        compiled.verilog
    );

    // Field reads keep the field's own width, not the struct's.
    let split = compile(&source, "examples/packet.shy.rs", "PacketSplit").expect("split example");
    assert!(
        split.verilog.contains("op = pkt[43:40];"),
        "{}",
        split.verilog
    );
    assert!(
        split.verilog.contains("addr = pkt[39:8];"),
        "{}",
        split.verilog
    );
    assert!(
        split.verilog.contains("flag = pkt[7];"),
        "{}",
        split.verilog
    );
    assert!(
        split.verilog.contains("tag = pkt[6:0];"),
        "{}",
        split.verilog
    );
}

#[test]
fn later_assignment_inside_one_process_is_legal() {
    // AC-MVP-012: overwriting inside one combinational process is fine, and
    // every bit is still driven on every path.
    let source = example("packet.shy.rs");
    let compiled =
        compile(&source, "examples/packet.shy.rs", "PacketTagReplace").expect("tag replace");
    assert!(
        compiled.verilog.contains("out = pkt;"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("out[6:0] = new_tag;"),
        "{}",
        compiled.verilog
    );
    assert_eq!(
        compiled.warnings.len(),
        3,
        "the other structs are unreachable: {:?}",
        compiled.warnings
    );
}

#[test]
fn constant_logic_is_wired_and_a_read_after_an_overwrite_keeps_its_snapshot() {
    // A `run` body that reads no signal at all has an empty sensitivity list:
    // `always @(*)` would never fire and the outputs would stay `x`. It is
    // emitted as continuous assignments instead, one version wire per write, so
    // blocking order survives: `z` keeps the value `y` had when the `let` was
    // bound, while `y` ends on the value written last.
    let source = r#"
#[hardware_instance]
struct ConstVersions {
    #[Out] y: uint<4>,
    #[Out] z: uint<4>,
}
impl Hardware for ConstVersions {
    fn run(&mut self) {
        self.y = 0;
        let x: uint<4> = self.y;
        self.y = 1;
        self.z = x;
    }
}
"#;
    let compiled = compile(source, "const.shy.rs", "ConstVersions").expect("constant logic");

    assert!(
        !compiled.verilog.contains("always"),
        "no signal is read, so no process may be emitted:\n{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("output wire [3:0] y"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("output wire [3:0] z"),
        "{}",
        compiled.verilog
    );
    assert_eq!(
        driven_value(&compiled.verilog, "y"),
        "4'd1",
        "the last write wins:\n{}",
        compiled.verilog
    );
    assert_eq!(
        driven_value(&compiled.verilog, "z"),
        "4'd0",
        "a read after an overwrite sees the overwritten value:\n{}",
        compiled.verilog
    );

    // Version wires and other compiler-introduced nets have no source name, so
    // the debug metadata exposes the source signals only (REQ-DBG-008).
    let names: Vec<&str> = compiled
        .metadata
        .variables
        .iter()
        .map(|variable| variable.source_name.as_str())
        .collect();
    assert_eq!(names, vec!["y", "z", "x"], "{names:?}");
}

#[test]
fn nested_lets_keep_block_scope_and_the_outer_binding_returns() {
    // A `let` inside control flow is a block-scoped temporary: two blocks may
    // use the same name with different types, and the bindings stay distinct.
    let branches = r#"
#[hardware_instance]
struct ShadowBranches {
    #[In] a: uint<4>,
    #[In] b: uint<8>,
    #[In] sel: bool,
    #[Out] y: uint<4>,
    #[Out] w: uint<8>,
}
impl Hardware for ShadowBranches {
    fn run(&mut self) {
        self.y = 0;
        self.w = 0;
        if self.sel {
            let t: uint<4> = self.a;
            self.y = t;
        } else {
            let t: uint<8> = self.b;
            self.w = t;
        }
    }
}
"#;
    let compiled = compile(branches, "scope.shy.rs", "ShadowBranches")
        .expect("a `let` is scoped to its block");
    let module = compiled.design().module("ShadowBranches").expect("module");
    let mut widths: Vec<u32> = module
        .signals
        .iter()
        .filter(|signal| signal.source_name.as_deref() == Some("t"))
        .map(|signal| signal.width())
        .collect();
    widths.sort_unstable();
    assert_eq!(
        widths,
        vec![4, 8],
        "each block declares its own `t`, with the type it was written with"
    );

    // A nested `let t` shadows the module signal `t` instead of aliasing it: the
    // read after the block is still the outer binding, and the initialiser of
    // the nested one reads that outer binding, not itself.
    let shadow = r#"
#[hardware_instance]
struct ShadowOuter {
    #[In] a: uint<4>,
    #[In] sel: bool,
    #[Out] y: uint<4>,
    #[Out] w: uint<4>,
}
impl Hardware for ShadowOuter {
    fn run(&mut self) {
        let t: uint<4> = self.a;
        self.w = 0;
        if self.sel {
            let t: uint<4> = t + 1;
            self.w = t;
        }
        self.y = t;
    }
}
"#;
    let compiled = compile(shadow, "scope.shy.rs", "ShadowOuter").expect("shadowing `let`");
    assert!(
        compiled.verilog.contains("y = t;"),
        "the read after the block is the outer binding:\n{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("= (t + 4'd1);"),
        "the nested initialiser reads the outer binding:\n{}",
        compiled.verilog
    );
    assert!(
        !compiled.verilog.contains("t = (t + 4'd1);"),
        "a `let` must not read the binding it declares:\n{}",
        compiled.verilog
    );
}

#[test]
fn a_process_may_read_bits_a_sequential_process_drives() {
    // `y` has two owners: the register process owns `y[0]`, the default
    // combinational process owns `y[1]` and reads the register bit. That is
    // feedback through a register - legal hardware and no combinational loop.
    let source = r#"
#[hardware_instance]
struct SplitOwner {
    #[In] a: bool,
    #[In] clk: bool,
    #[Out] y: uint<2>,
}
impl Hardware for SplitOwner {
    fn run(&mut self) {
        self.y[1] = self.y[0] ^ self.a;
        always! {
            #[Pedge] self.clk => { self.y[0] = self.a; }
        }
    }
}
"#;
    let compiled = compile(source, "split_owner.shy.rs", "SplitOwner").expect("split bit owners");
    assert!(
        compiled.verilog.contains("always @(*) begin : comb0"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled
            .verilog
            .contains("always @(posedge clk) begin : seq0"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("y[1] = "),
        "the combinational bit is driven blocking:\n{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("y[0] <= a;"),
        "the register bit is driven non-blocking:\n{}",
        compiled.verilog
    );
}

#[test]
fn an_unrolled_self_doubling_body_emits_one_wire_per_write() {
    // A static `for` unrolls; a body like `y = y + y` must stay linear. Each
    // write becomes its own version wire, read at every later use site, so the
    // emitted text grows with the number of writes - inlining the doubling
    // expression would nest one copy per iteration instead.
    let source = |iterations: usize| {
        format!(
            r#"
#[hardware_instance]
struct Expanded {{
    #[Out] y: uint<8>,
}}
impl Hardware for Expanded {{
    fn run(&mut self) {{
        self.y = 1;
        for i in 0..{iterations} {{
            self.y = self.y + self.y;
        }}
    }}
}}
"#
        )
    };

    for iterations in [4, 16] {
        let compiled =
            compile(&source(iterations), "expanded.shy.rs", "Expanded").expect("unrolled doubling");
        assert_eq!(
            compiled.verilog.matches("assign ").count(),
            iterations + 2,
            "one assignment for the initial value, one per iteration and one for the output:\n{}",
            compiled.verilog
        );
        assert!(
            compiled.verilog.lines().all(|line| line.len() < 120),
            "no generated line may hold a nested doubling expression:\n{}",
            compiled.verilog
        );
    }
}

#[test]
fn metadata_covers_every_instance_path() {
    let source = example("register.shy.rs");
    let compiled =
        compile(&source, "examples/register.shy.rs", "RegisterPair").expect("register example");

    let paths: Vec<&str> = compiled
        .metadata
        .variables
        .iter()
        .map(|variable| variable.instance_path.as_str())
        .collect();
    assert!(paths.contains(&"RegisterPair"), "{paths:?}");
    assert!(paths.contains(&"RegisterPair.wide"), "{paths:?}");
    assert!(paths.contains(&"RegisterPair.narrow"), "{paths:?}");

    // Two instances of one module never share an id.
    let ids: std::collections::HashSet<&str> = compiled
        .metadata
        .variables
        .iter()
        .map(|variable| variable.id.as_str())
        .collect();
    assert_eq!(ids.len(), compiled.metadata.variables.len());
    assert!(ids.contains("RegisterPair.wide/q"));
    assert!(ids.contains("RegisterPair.narrow/q"));
    compiled
        .metadata
        .validate()
        .expect("metadata is consistent");

    // A clock is an ordinary input signal; the design carries no clock marking.
    let clock = compiled
        .metadata
        .variables
        .iter()
        .find(|variable| variable.source_name == "clk")
        .expect("clk variable");
    assert_eq!(clock.ty, shyhdl::metadata::ValueType::Bool);
    assert_eq!(clock.rtl_name, "clk");
}

#[test]
fn build_id_covers_sources_top_and_parameters() {
    let source = example("adder.shy.rs");
    let eight = compile(&source, "examples/adder.shy.rs", "Adder<8>").expect("Adder<8>");
    let one = compile(&source, "examples/adder.shy.rs", "Adder<1>").expect("Adder<1>");
    let again = compile(&source, "examples/adder.shy.rs", "Adder<8>").expect("Adder<8> again");

    assert_eq!(
        eight.metadata.build_id, again.metadata.build_id,
        "the same build has the same id"
    );
    assert_ne!(
        eight.metadata.build_id, one.metadata.build_id,
        "different parameters are different builds"
    );
    assert_eq!(eight.metadata.top, "Adder_N8");
}

#[test]
fn compilation_is_deterministic() {
    for (path, top) in [
        ("adder.shy.rs", "Adder8"),
        ("register.shy.rs", "RegisterPair"),
        ("pipeline.shy.rs", "Pipe8"),
        ("packet.shy.rs", "PacketSelect"),
    ] {
        let source = example(path);
        let first = compile(&source, path, top).expect("first compile");
        let second = compile(&source, path, top).expect("second compile");
        assert_eq!(
            first.verilog, second.verilog,
            "{path} must generate identical Verilog"
        );
        assert_eq!(
            first.metadata, second.metadata,
            "{path} must generate identical metadata"
        );
    }
}

#[test]
fn parse_design_returns_the_model_without_generating_verilog() {
    let source = example("adder.shy.rs");
    let design = parse_design(&source, "examples/adder.shy.rs", "Adder8").expect("model");
    assert_eq!(design.top().verilog_name, "Adder8");
    assert_eq!(design.modules().len(), 2);
    assert!(design.resolve("Adder", &[("N", 8)]).is_some());
    assert!(design.resolve("Adder", &[("N", 4)]).is_none());
    assert!(design.module_by_source_name("Adder").is_some());
    assert_eq!(design.instantiators("Adder_N8").count(), 1);
    assert_eq!(design.source_name(), "examples/adder.shy.rs");
    assert!(design.warnings().is_empty());
}

#[test]
fn compile_all_reports_every_independent_error() {
    let source = r#"
#[hardware_instance]
struct TwoBad {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
    #[Out] z: uint<4>,
}
impl Hardware for TwoBad {
    fn run(&mut self) {
        self.y = self.a.zext::<8>();
        self.z = self.a.get(9);
    }
}
"#;
    let diagnostics = compile_all(source, "two_bad.shy.rs", "TwoBad").expect_err("two errors");
    assert_eq!(diagnostics.errors().count(), 2, "{diagnostics}");
    let codes: Vec<&str> = diagnostics.errors().map(|d| d.code).collect();
    assert!(
        codes.contains(&shyhdl::frontend::codes::WIDTH_MISMATCH),
        "{codes:?}"
    );
    assert!(
        codes.contains(&shyhdl::frontend::codes::OUT_OF_RANGE),
        "{codes:?}"
    );
    // Every diagnostic points somewhere in the source.
    assert!(
        diagnostics.errors().all(|d| d.span.is_some()),
        "{diagnostics}"
    );
}

#[test]
fn the_event_arm_spellings_all_produce_the_same_process() {
    // Roles come from source order, as they do in the edge-chain spelling: every
    // arm before the last is an asynchronous reset whose active level is the edge
    // written on it, and the last arm is the clock. Nothing is marked.
    let explicit = r#"
#[hardware_instance]
struct Explicit {
    #[In] d: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}
impl Hardware for Explicit {
    fn run(&mut self) {
        always! {
            #[Nedge] self.rst_n => { self.q = false; }
            #[Pedge] self.clk => { self.q = self.d; }
        }
    }
}
"#;
    let compiled = compile(explicit, "arms.shy.rs", "Explicit").expect("reset first");
    assert!(compiled
        .verilog
        .contains("always @(posedge clk or negedge rst_n)"));
    assert!(compiled.verilog.contains("if ((!rst_n))"));
    let reset = compiled.verilog.find("q <= 1'b0;").expect("reset body");
    let clock = compiled.verilog.find("q <= d;").expect("clock body");
    assert!(reset < clock, "the reset branch comes first");

    // More than one asynchronous reset is expressible; the first one written has
    // the highest priority, and each takes its active level from its own edge.
    let two = r#"
#[hardware_instance]
struct TwoResets {
    #[In] d: bool,
    #[In] rst_a: bool,
    #[In] rst_b: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}
impl Hardware for TwoResets {
    fn run(&mut self) {
        always! {
            #[Nedge] self.rst_a => { self.q = false; }
            #[Pedge] self.rst_b => { self.q = true; }
            #[Pedge] self.clk => { self.q = self.d; }
        }
    }
}
"#;
    let compiled = compile(two, "arms.shy.rs", "TwoResets").expect("two resets");
    assert!(
        compiled
            .verilog
            .contains("always @(posedge clk or negedge rst_a or posedge rst_b)"),
        "{}",
        compiled.verilog
    );
    let first = compiled.verilog.find("if ((!rst_a))").expect("first reset");
    let second = compiled.verilog.find("if (rst_b)").expect("second reset");
    let clock_body = compiled.verilog.find("q <= d;").expect("clock body");
    assert!(first < second, "resets keep their written priority");
    assert!(second < clock_body, "the clock branch is the last one");
}

#[test]
fn the_edge_chain_spelling_uses_control_flow_for_its_roles() {
    // One branch is the regular edge branch; nothing has to be marked.
    let single = r#"
#[hardware_instance]
struct Single {
    #[In] d: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}
impl Hardware for Single {
    fn run(&mut self) {
        always! {
            if posedge(self.clk) { self.q = self.d; }
        }
    }
}
"#;
    let compiled = compile(single, "chain.shy.rs", "Single").expect("single edge chain");
    assert!(
        compiled
            .verilog
            .contains("always @(posedge clk) begin : seq0"),
        "{}",
        compiled.verilog
    );

    // Every earlier branch is an asynchronous reset, the last is the regular
    // branch - read from the control flow, not from a signal name or polarity.
    let chain = r#"
#[hardware_instance]
struct Marked {
    #[In] d: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}
impl Hardware for Marked {
    fn run(&mut self) {
        always! {
            if negedge(self.rst_n) { self.q = false; }
            else if posedge(self.clk) { self.q = self.d; }
        }
    }
}
"#;
    let compiled =
        compile(chain, "chain.shy.rs", "Marked").expect("chain with a plain input clock");
    assert!(
        compiled
            .verilog
            .contains("always @(posedge clk or negedge rst_n)"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("if ((!rst_n))"),
        "{}",
        compiled.verilog
    );

    // A bare `else` after the last edge branch has no event and is rejected.
    let bare_else = chain.replace(
        "else if posedge(self.clk) { self.q = self.d; }",
        "else if posedge(self.clk) { self.q = self.d; } else { self.q = false; }",
    );
    let error = compile(&bare_else, "chain.shy.rs", "Marked").expect_err("bare else");
    assert_eq!(error.code, shyhdl::frontend::codes::SEQ_SHAPE);
}

#[test]
fn a_removed_clock_marking_is_explained() {
    // `#[Clk]` is gone: the message says what to write instead, on a port and
    // on an arm alike.
    let port = r#"
#[hardware_instance]
struct Old {
    #[In] d: bool,
    #[Clk] clk: bool,
    #[Out] q: bool,
}
impl Hardware for Old {
    fn run(&mut self) {
        self.q = self.d;
    }
}
"#;
    let error = compile(port, "old.shy.rs", "Old").expect_err("#[Clk] port");
    assert_eq!(error.code, shyhdl::frontend::codes::DECLARATION);
    assert!(
        error
            .notes
            .iter()
            .any(|note| note.message.contains("#[In]")),
        "{error}"
    );

    let arm = r#"
#[hardware_instance]
struct OldArm {
    #[In] d: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}
impl Hardware for OldArm {
    fn run(&mut self) {
        always! {
            #[Nedge] self.rst_n => { self.q = false; }
            #[Pedge] #[Clk] self.clk => { self.q = self.d; }
        }
    }
}
"#;
    let error = compile(arm, "old.shy.rs", "OldArm").expect_err("#[Clk] arm");
    assert_eq!(error.code, shyhdl::frontend::codes::SEQ_SHAPE);
    assert!(error.message.contains("removed"), "{error}");
}

#[test]
fn one_bit_signals_stay_scalars_in_every_position() {
    let source = r#"
#[hardware_instance]
struct Bits {
    #[In] a: bool,
    #[Out] hi: bool,
    #[Out] lo: bool,
}
impl Hardware for Bits {
    fn run(&mut self) {
        let pair: uint<2> = self.a.zext::<2>();
        self.hi = pair.get(1);
        self.lo = pair.get(0);
    }
}
"#;
    let compiled = compile(source, "bits.shy.rs", "Bits").expect("bits");
    assert!(
        compiled.verilog.contains("reg [1:0] pair;"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("pair = {1'b0, a};"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("hi = pair[1];"),
        "{}",
        compiled.verilog
    );
    assert!(
        compiled.verilog.contains("lo = pair[0];"),
        "{}",
        compiled.verilog
    );
}
