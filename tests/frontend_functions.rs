//! Contract tests for `#[hardware_instance] fn`: a combinational function is a
//! module, and every call site is an instance of it.

use shyhdl::frontend::compile;

/// A function with a tuple return: one output port per element.
const ADDER: &str = r#"
#[hardware_instance]
fn AddMul(a: uint<4>, b: uint<4>) -> (uint<4>, uint<8>) {
    let sum = a + b;
    let product = a.zext::<8>() * b.zext::<8>();
    (sum, product)
}

#[hardware_instance]
struct FnTop {
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    #[Out] sum: uint<4>,
    #[Out] product: uint<8>,
}

impl Hardware for FnTop {
    fn run(&mut self) {
        let (s, p) = AddMul(a, b);
        sum = s;
        product = p;
    }
}
"#;

#[test]
fn a_function_becomes_a_combinational_module() {
    let compiled = compile(ADDER, "functions.shy.rs", "FnTop").expect("the design compiles");
    let module = compiled.design().module("AddMul").expect("function module");
    let ports: Vec<(&str, u32)> = module
        .ports
        .iter()
        .map(|port| (port.verilog_name.as_str(), port.width))
        .collect();
    assert_eq!(ports, vec![("a", 4), ("b", 4), ("out0", 4), ("out1", 8)]);
    // The body is the ordinary default combinational process.
    assert!(compiled.verilog.contains("always @(*) begin : comb0"));
    assert!(compiled.verilog.contains("out0 = sum;"));
    assert!(compiled.verilog.contains("out1 = product;"));
}

#[test]
fn a_call_instantiates_the_function_module() {
    let compiled = compile(ADDER, "functions.shy.rs", "FnTop").expect("the design compiles");
    let instance = compiled
        .top()
        .instances
        .iter()
        .find(|instance| instance.field.starts_with("AddMul__"))
        .expect("one instance per call");
    assert_eq!(instance.module_source_name, "AddMul");
    let connected: Vec<&str> = instance
        .connections
        .iter()
        .map(|connection| connection.port.as_str())
        .collect();
    assert_eq!(connected, vec!["a", "b", "out0", "out1"]);
    assert!(compiled.verilog.contains(".out0(AddMul__1_out0)"));
    assert!(compiled.verilog.contains(".out1(AddMul__1_out1)"));
}

#[test]
fn two_calls_are_two_instances() {
    let source = r#"
#[hardware_instance]
fn Inc(x: uint<8>) -> uint<8> {
    x + 1
}

#[hardware_instance]
struct Two {
    #[In] a: uint<8>,
    #[Out] y: uint<8>,
    #[Out] z: uint<8>,
}

impl Hardware for Two {
    fn run(&mut self) {
        y = Inc(a);
        z = Inc(y);
    }
}
"#;
    let compiled = compile(source, "two_calls.shy.rs", "Two").expect("the design compiles");
    let calls = compiled
        .top()
        .instances
        .iter()
        .filter(|instance| instance.module_source_name == "Inc")
        .count();
    assert_eq!(calls, 2, "each call site gets its own instance");
    // A single-output function yields one value, so no tuple is involved.
    assert!(compiled.verilog.contains(".out0(Inc__1_out0)"));
    assert!(compiled.verilog.contains(".out0(Inc__2_out0)"));
}

#[test]
fn a_function_may_call_another_function() {
    let source = r#"
#[hardware_instance]
fn Inc(x: uint<8>) -> uint<8> {
    x + 1
}

#[hardware_instance]
fn Twice(x: uint<8>) -> uint<8> {
    let once = Inc(x);
    Inc(once)
}

#[hardware_instance]
struct ChainTop {
    #[In] a: uint<8>,
    #[Out] y: uint<8>,
}

impl Hardware for ChainTop {
    fn run(&mut self) {
        y = Twice(a);
    }
}
"#;
    let compiled = compile(source, "chain.shy.rs", "ChainTop").expect("the design compiles");
    assert!(compiled.design().module("Inc").is_some());
    assert!(compiled.design().module("Twice").is_some());
    // `Twice` calls `Inc` twice, so it instantiates it twice; the top
    // instantiates `Twice` once.
    let twice = compiled.design().module("Twice").expect("Twice module");
    assert_eq!(
        twice
            .instances
            .iter()
            .filter(|instance| instance.module_source_name == "Inc")
            .count(),
        2
    );
    assert_eq!(
        compiled
            .top()
            .instances
            .iter()
            .filter(|instance| instance.module_source_name == "Twice")
            .count(),
        1
    );
}

#[test]
fn a_function_without_a_tail_value_is_rejected() {
    let source = r#"
#[hardware_instance]
fn Broken(x: uint<8>) -> uint<8> {
    let y = x + 1;
}

#[hardware_instance]
struct Top {
    #[In] a: uint<8>,
    #[Out] y: uint<8>,
}

impl Hardware for Top {
    fn run(&mut self) {
        y = Broken(a);
    }
}
"#;
    let error = compile(source, "broken.shy.rs", "Top").expect_err("no tail value");
    assert_eq!(error.code, "SH-F004", "{error:?}");
    assert!(
        error.message.contains("must end with the value it returns"),
        "{error:?}"
    );
}

#[test]
fn a_call_with_the_wrong_number_of_arguments_is_rejected() {
    let source = r#"
#[hardware_instance]
fn Add(a: uint<4>, b: uint<4>) -> uint<4> {
    a + b
}

#[hardware_instance]
struct Top {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}

impl Hardware for Top {
    fn run(&mut self) {
        y = Add(a);
    }
}
"#;
    let error = compile(source, "arity.shy.rs", "Top").expect_err("wrong arity");
    assert_eq!(error.code, "SH-F004", "{error:?}");
    assert!(error.message.contains("takes 2 argument"), "{error:?}");
}

#[test]
fn a_sequential_body_is_rejected_at_parse_time() {
    let source = r#"
#[hardware_instance]
fn Seq(x: uint<8>, clk: bool) -> uint<8> {
    always! {
        #[Pedge] clk => { x = 0; }
    }
    x
}
"#;
    let error = compile(source, "seq.shy.rs", "Seq").expect_err("always is not combinational");
    assert_eq!(error.code, "SH-F002", "{error:?}");
    assert!(error.message.contains("combinational"), "{error:?}");
}

#[test]
fn the_committed_function_example_compiles_and_runs() {
    let source =
        std::fs::read_to_string("examples/functions.shy.rs").expect("the example must be readable");
    let compiled =
        compile(&source, "examples/functions.shy.rs", "TwoAdders").expect("the example compiles");
    // Two calls of one function: two instances, both named after the function.
    assert_eq!(
        compiled
            .top()
            .instances
            .iter()
            .filter(|instance| instance.module_source_name == "Adder4")
            .count(),
        2
    );
    // The generated Verilog keeps the function as a module and instantiates it.
    assert!(compiled.verilog.contains("module Adder4 ("));
    assert!(compiled.verilog.contains(".out0(Adder4__1_out0)"));
    // The second call gets its own instance and its own output nets.
    assert!(compiled.verilog.contains(".out1(Adder4__3_out1)"));
}

/// A carry chain through a loop: each iteration reads `cin[i]` and writes
/// `cin[i + 1]`. The bits are disjoint, so this is not a combinational loop —
/// a signal-level graph would call it one.
#[test]
fn a_carry_chain_through_a_loop_is_not_a_loop() {
    let source = r#"
#[hardware_instance]
fn Fadd(a: bool, b: bool, cin: bool) -> (bool, bool) {
    let t: bool = a ^ b;
    (t ^ cin, (a & b) | (t & cin))
}

#[hardware_instance]
fn Adder4(a: uint<4>, b: uint<4>, sub: bool) -> (uint<4>, bool, bool) {
    let mut cin: [bool; 5] = [false; 5];
    let mut sum: uint<4> = 0;
    cin[0] = sub;
    for i in 0..4 {
        let (sb, co) = Fadd(a[i], b[i] ^ sub, cin[i]);
        sum[i] = sb;
        cin[i + 1] = co;
    }
    (sum, cin[4], cin[4] ^ cin[3])
}

#[hardware_instance]
struct Top {
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    #[In] sub: bool,
    #[Out] s: uint<4>,
    #[Out] c: bool,
    #[Out] v: bool,
}

impl Hardware for Top {
    fn run(&mut self) {
        let (sum, cout, overflow) = Adder4(a, b, sub);
        s = sum;
        c = cout;
        v = overflow;
    }
}
"#;
    let compiled = compile(source, "carry.shy.rs", "Top").expect("a carry chain is not a loop");
    let verilog = &compiled.verilog;

    // Every stage is its own instance, chained through the carry bits.
    for stage in 0..4 {
        assert!(
            verilog.contains(&format!(".cin(cin[{stage}])")),
            "stage {stage} takes cin[{stage}]:\n{verilog}"
        );
    }
    assert!(verilog.contains("cin[0] = sub;"));
    assert!(verilog.contains("cin[4] = "));
}
