//! Diagnostics tests: an illegal design must fail with the right code and a
//! source location, never with a silent fallback.
//!
//! Each case is a small source that should not compile. The expected code is
//! part of the observable contract (see `frontend::codes`).

use shyhdl::frontend::{codes, compile, parse_design, Diagnostic};

/// Compile `body` (a complete source file) and expect a failure with `code`.
fn expect(body: &str, top: &str, code: &str) -> Diagnostic {
    match compile(body, "test.shy.rs", top) {
        Ok(compiled) => panic!(
            "expected {code}, but compilation succeeded:\n{}",
            compiled.verilog
        ),
        Err(diagnostic) => {
            assert_eq!(diagnostic.code, code, "unexpected diagnostic: {diagnostic}");
            // Every diagnostic either points into the source or names the file
            // it is about (the requested top module comes from the caller, so it
            // has no source position of its own).
            assert!(
                diagnostic.span.is_some() || diagnostic.message.contains("test.shy.rs"),
                "diagnostics must be locatable: {diagnostic}"
            );
            diagnostic
        }
    }
}

#[test]
fn the_default_combinational_process_must_drive_every_bit_on_every_path() {
    let source = r#"
#[hardware_instance]
struct Latch {
    #[In] a: uint<4>,
    #[In] sel: bool,
    #[Out] y: uint<4>,
}
impl Hardware for Latch {
    fn run(&mut self) {
        if self.sel {
            self.y = self.a;
        }
    }
}
"#;
    let diagnostic = expect(source, "Latch", codes::INCOMPLETE_PATH);
    assert!(diagnostic.message.contains("y"), "{diagnostic}");
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.message.contains("every path")
                || note.message.contains("assign it unconditionally")),
        "{diagnostic}"
    );
}

#[test]
fn reading_bits_this_process_writes_later_is_rejected() {
    // `mixed[0..4]` is written by the same process *after* `mixed[i]` reads the
    // whole signal, so a bit the read can select is still undriven when it is
    // read. In an `always @(*)` that reads the previous value of those bits and
    // re-triggers on its own output: the process would have to remember state,
    // which the default combinational process must never do (REQ-LANG-015).
    let dynamic_read = r#"
#[hardware_instance]
struct FutureWrite {
    #[In] a: uint<8>,
    #[In] i: uint<8>,
    #[Out] seen: bool,
    #[Out] y: uint<8>,
}
impl Hardware for FutureWrite {
    fn run(&mut self) {
        let mut mixed: uint<8>;
        mixed[4..8] = self.a[4..8];
        self.seen = mixed[i];
        mixed[0..4] = 3;
        self.y = mixed;
    }
}
"#;
    let diagnostic = expect(dynamic_read, "FutureWrite", codes::INCOMPLETE_PATH);
    assert!(diagnostic.to_string().contains("mixed"), "{diagnostic}");

    // The same partial write is fine when the read only selects bits that are
    // already assigned: `mixed[5]` is in the high half written first.
    let static_read = dynamic_read.replace("self.seen = mixed[i];", "self.seen = mixed[5];");
    assert!(
        compile(&static_read, "test.shy.rs", "FutureWrite").is_ok(),
        "a read of bits this process already assigned is not a future write"
    );

    // Bits another process owns are not this process's unfinished business: a
    // register bit read by the default combinational process is legal.
    let seq_driven = r#"
#[hardware_instance]
struct SeqRead {
    #[In] a: uint<4>,
    #[In] clk: bool,
    #[Out] q: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for SeqRead {
    fn run(&mut self) {
        always! {
            #[Pedge] self.clk => { self.q = self.a; }
        }
        self.y = self.q + 1;
    }
}
"#;
    assert!(
        compile(seq_driven, "test.shy.rs", "SeqRead").is_ok(),
        "reading a signal a sequential process drives is legal"
    );
}

#[test]
fn a_half_driven_signal_is_an_error() {
    let source = r#"
#[hardware_instance]
struct Half {
    #[In] a: uint<4>,
    #[Out] y: uint<8>,
}
impl Hardware for Half {
    fn run(&mut self) {
        self.y[0..4] = self.a;
    }
}
"#;
    let diagnostic = expect(source, "Half", codes::UNDRIVEN);
    assert!(diagnostic.message.contains("y[7:4]"), "{diagnostic}");
}

#[test]
fn bit_index_and_slice_bounds_are_checked() {
    // The draft's `d.get(N + 1)` on an (N+1)-bit value is exactly this error.
    let out_of_range = r#"
#[hardware_instance]
struct Adder<const N: usize> {
    #[In] a: uint<N>,
    #[Out] carry: bool,
}
impl<const N: usize> Hardware for Adder<N> {
    fn run(&mut self) {
        let sum: uint<{ N + 1 }> = self.a.zext::<{ N + 1 }>();
        self.carry = sum.get(N + 1);
    }
}
"#;
    let diagnostic = expect(out_of_range, "Adder<8>", codes::OUT_OF_RANGE);
    assert!(diagnostic.message.contains("out of range"), "{diagnostic}");

    let bad_slice = r#"
#[hardware_instance]
struct Slicer {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for Slicer {
    fn run(&mut self) {
        self.y = self.a[1..5];
    }
}
"#;
    expect(bad_slice, "Slicer", codes::OUT_OF_RANGE);

    let wide_literal = r#"
#[hardware_instance]
struct Wide {
    #[In] a: bool,
    #[Out] y: uint<4>,
}
impl Hardware for Wide {
    fn run(&mut self) {
        self.y = 100;
    }
}
"#;
    let diagnostic = expect(wide_literal, "Wide", codes::OUT_OF_RANGE);
    assert!(diagnostic.message.contains("100"), "{diagnostic}");
}

#[test]
fn mixed_widths_are_rejected_instead_of_widened() {
    let source = r#"
#[hardware_instance]
struct Mix {
    #[In] a: uint<4>,
    #[In] b: uint<8>,
    #[Out] y: uint<8>,
}
impl Hardware for Mix {
    fn run(&mut self) {
        self.y = self.a + self.b;
    }
}
"#;
    let diagnostic = expect(source, "Mix", codes::WIDTH_MISMATCH);
    assert!(diagnostic.message.contains("mixes widths"), "{diagnostic}");
}

#[test]
fn assignment_width_must_match_exactly() {
    let source = r#"
#[hardware_instance]
struct Narrow {
    #[In] a: uint<8>,
    #[Out] y: uint<4>,
}
impl Hardware for Narrow {
    fn run(&mut self) {
        self.y = self.a;
    }
}
"#;
    expect(source, "Narrow", codes::WIDTH_MISMATCH);
}

#[test]
fn overlapping_drivers_across_processes_are_rejected() {
    // The default combinational process and an `always!` process both driving
    // `y` is the conflict: each bit may have exactly one driver process.
    // (Writing the same bits twice *inside* one process is not this: there the
    // later statement simply wins, see
    // `frontend_compile::later_assignment_inside_one_process_is_legal`.)
    let source = r#"
#[hardware_instance]
struct TwoDrivers {
    #[In] a: bool,
    #[In] clk: bool,
    #[Out] y: bool,
}
impl Hardware for TwoDrivers {
    fn run(&mut self) {
        self.y = self.a;
        always! {
            #[Pedge] self.clk => { self.y = !self.a; }
        }
    }
}
"#;
    let diagnostic = expect(source, "TwoDrivers", codes::MULTI_DRIVER);
    assert!(diagnostic.message.contains("one driver"), "{diagnostic}");

    // Structure has the same rule: a child input connected twice has two
    // drivers, and the connection is not a statement that could overwrite.
    let child = r#"
#[hardware_instance]
struct Kid {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Kid {
    fn run(&mut self) {
        self.y = self.a;
    }
}

#[hardware_instance]
struct Parent {
    #[In] a: bool,
    #[In] b: bool,
    #[Out] y: bool,
    #[instance] kid: Kid,
}
impl Hardware for Parent {
    fn run(&mut self) {
        self.kid.a = self.a;
        self.kid.a = self.b;
        self.y = self.kid.y;
    }
}
"#;
    let diagnostic = expect(child, "Parent", codes::MULTI_DRIVER);
    assert!(
        diagnostic.message.contains("connected twice"),
        "{diagnostic}"
    );
}

#[test]
fn writing_an_input_port_is_rejected() {
    let source = r#"
#[hardware_instance]
struct Writer {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Writer {
    fn run(&mut self) {
        self.a = self.y;
    }
}
"#;
    expect(source, "Writer", codes::INPUT_WRITTEN);
}

#[test]
fn combinational_feedback_is_detected_at_range_granularity() {
    // Reading a bit this same process drives, before it has been assigned,
    // would use the process's own previous output: a self-loop, reported as the
    // read that comes too early.
    let direct = r#"
#[hardware_instance]
struct Loop {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Loop {
    fn run(&mut self) {
        self.y = self.y;
    }
}
"#;
    let diagnostic = expect(direct, "Loop", codes::INCOMPLETE_PATH);
    assert!(diagnostic.message.contains("y["), "{diagnostic}");

    // A loop through structure is the dependency graph's job, and it is still
    // reported as a loop: `y` drives the child, the child drives `y` back.
    let through_child = r#"
#[hardware_instance]
struct Inverter {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Inverter {
    fn run(&mut self) {
        self.y = !self.a;
    }
}

#[hardware_instance]
struct Feedback {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] kid: Inverter,
}
impl Hardware for Feedback {
    fn run(&mut self) {
        self.kid.a = self.y;
        self.y = self.kid.y;
    }
}
"#;
    let diagnostic = expect(through_child, "Feedback", codes::COMB_LOOP);
    assert!(
        diagnostic.message.contains("combinational loop"),
        "{diagnostic}"
    );

    // The same loop routed through a branch: only one arm reads the child, so
    // the sources of `w` have to be the union over the arms. Taking just one
    // arm would drop `p_y -> w` and the loop would go unreported.
    let through_branch = r#"
#[hardware_instance]
struct Pass {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Pass {
    fn run(&mut self) {
        self.y = self.a;
    }
}

#[hardware_instance]
struct LoopViaChild {
    #[In] sel: bool,
    #[In] d: bool,
    #[Out] q: bool,
    #[Out] w: bool,
    #[instance] p: Pass,
}
impl Hardware for LoopViaChild {
    fn run(&mut self) {
        self.q = self.d;
        self.p.a = self.q;
        if self.sel {
            self.w = self.p.y;
        } else {
            self.w = self.q;
        }
        self.q = self.w;
    }
}
"#;
    let diagnostic = expect(through_branch, "LoopViaChild", codes::COMB_LOOP);
    assert!(
        diagnostic.message.contains("combinational loop"),
        "{diagnostic}"
    );

    // Both paths write a constant, and the condition is a constant too, so the
    // merged entry has no sources at all. Keeping that (empty) entry is what
    // stops the later read of `y` from looking like a read of `y` itself.
    let constant_branch = r#"
#[hardware_instance]
struct ConstantBranch {
    #[Out] y: bool,
    #[Out] z: bool,
}
impl Hardware for ConstantBranch {
    fn run(&mut self) {
        if true {
            self.y = false;
        } else {
            self.y = true;
        }
        self.z = self.y;
    }
}
"#;
    compile(constant_branch, "branch.shy.rs", "ConstantBranch")
        .expect("a constant write on every path is not feedback");

    // A run-time element write replaces only the element it names. The read of
    // `cells[0]` is a *process* read, so it resolves through provenance: the loop
    // through the child is visible only when the leaf unions the sources of the
    // range it covers with the arm's own. A splice that replaced the leaf range
    // with the arm's sources alone would drop `p_y -> q` and the loop would go
    // unreported.
    let leaf_union = r#"
#[hardware_instance]
struct Src {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Src {
    fn run(&mut self) {
        self.y = self.a;
    }
}

#[hardware_instance]
struct LeafUnion {
    #[In] i: uint<1>,
    #[In] x: bool,
    #[Out] q: bool,
    #[instance] p: Src,
}
impl Hardware for LeafUnion {
    fn run(&mut self) {
        let mut cells: [bool; 2] = [false, false];
        cells[0] = self.p.y;
        cells[self.i] = self.x;
        self.q = cells[0];
        self.p.a = self.q;
    }
}
"#;
    let diagnostic = expect(leaf_union, "LeafUnion", codes::COMB_LOOP);
    assert!(
        diagnostic.message.contains("combinational loop"),
        "{diagnostic}"
    );

    // The same shape with a plain input in the neighbouring field has no loop.
    let gap_ok = r#"
#[hardware_instance]
struct GapOk {
    #[In] i: uint<1>,
    #[In] x: bool,
    #[In] a: bool,
    #[Out] q: bool,
}
impl Hardware for GapOk {
    fn run(&mut self) {
        let mut cells: [bool; 2] = [false, false];
        cells[0] = self.a;
        cells[self.i] = self.x;
        self.q = cells[0];
    }
}
"#;
    compile(gap_ok, "gap.shy.rs", "GapOk").expect("a plain input is not a loop");

    // A real gap: the leaf writes `cells[i].val`, so the neighbouring field
    // `cells[0].tag` is *not* covered by any leaf. Its source comes from the
    // child, so the loop is visible only when the before-sources of that range
    // survive the splice; widening the leaf over the gap would replace them with
    // the arm's own sources and hide the loop.
    let field_gap = r#"
#[hardware_instance]
struct Src2 {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Src2 {
    fn run(&mut self) {
        self.y = self.a;
    }
}

struct Cell {
    tag: bool,
    val: bool,
}

#[hardware_instance]
struct FieldGap {
    #[In] i: uint<1>,
    #[In] x: bool,
    #[Out] q: bool,
    #[Out] tags: bool,
    #[instance] p: Src2,
}
impl Hardware for FieldGap {
    fn run(&mut self) {
        let mut cells: [Cell; 2] = [Cell { tag: false, val: false }; 2];
        cells[0].tag = self.p.y;
        cells[self.i].val = self.x;
        self.q = cells[0].tag;
        self.p.a = self.q;
        self.tags = cells[0].tag;
    }
}
"#;
    let diagnostic = expect(field_gap, "FieldGap", codes::COMB_LOOP);
    assert!(
        diagnostic.message.contains("combinational loop"),
        "{diagnostic}"
    );

    // The other direction: the range between two leaves must stay tracked. Here
    // `cells[1].tag` is written from `cells[0].tag`, a bit no leaf covers, and
    // neither bit is read through the child. A splice that dropped the gap would
    // report the read of `cells[0].tag` as a read of `cells` itself and invent a
    // self loop the design does not have. The whole array is initialized and the
    // gap bit is written, so every read here is assigned and the only diagnostic
    // this design could produce is that invented loop.
    let gap_control = r#"
struct Cell {
    tag: bool,
    val: bool,
}

#[hardware_instance]
struct GapControl {
    #[In] i: uint<1>,
    #[In] a: bool,
    #[In] x: bool,
    #[Out] q: bool,
}
impl Hardware for GapControl {
    fn run(&mut self) {
        let mut cells: [Cell; 2] = [Cell { tag: false, val: false }; 2];
        cells[0].tag = self.a;
        cells[self.i].val = self.x;
        cells[1].tag = cells[0].tag;
        self.q = cells[1].tag;
    }
}
"#;
    compile(gap_control, "gap_control.shy.rs", "GapControl")
        .expect("the range between two leaves is not a self loop");

    // Reading bits that were assigned earlier in the same process is not a loop.
    let split = r#"
#[hardware_instance]
struct Split {
    #[In] a: uint<4>,
    #[Out] y: uint<8>,
}
impl Hardware for Split {
    fn run(&mut self) {
        self.y[0..4] = self.a;
        self.y[4..8] = self.y[0..4];
    }
}
"#;
    assert!(
        compile(split, "test.shy.rs", "Split").is_ok(),
        "a split write is not a loop"
    );

    // ... but reading the very bits that are being assigned is: `y[4..8]` reads
    // its own previous value, exactly like the whole-signal case above.
    let same_bits = r#"
#[hardware_instance]
struct SameBits {
    #[In] a: uint<4>,
    #[Out] y: uint<8>,
}
impl Hardware for SameBits {
    fn run(&mut self) {
        self.y[0..4] = self.a;
        self.y[4..8] = self.y[4..8];
    }
}
"#;
    expect(same_bits, "SameBits", codes::INCOMPLETE_PATH);
}

#[test]
fn unconnected_child_inputs_are_rejected() {
    let source = r#"
#[hardware_instance]
struct Child {
    #[In] a: bool,
    #[In] b: bool,
    #[Out] y: bool,
}
impl Hardware for Child {
    fn run(&mut self) {
        self.y = self.a & self.b;
    }
}

#[hardware_instance]
struct Parent {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] kid: Child,
}
impl Hardware for Parent {
    fn run(&mut self) {
        self.kid.a = self.a;
        self.y = self.kid.y;
    }
}
"#;
    let diagnostic = expect(source, "Parent", codes::UNCONNECTED_INPUT);
    assert!(diagnostic.message.contains("b"), "{diagnostic}");
}

#[test]
fn recursion_between_modules_is_rejected() {
    let source = r#"
#[hardware_instance]
struct A {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] next: B,
}
impl Hardware for A {
    fn run(&mut self) {
        self.next.a = self.a;
        self.y = self.next.y;
    }
}

#[hardware_instance]
struct B {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] next: A,
}
impl Hardware for B {
    fn run(&mut self) {
        self.next.a = self.a;
        self.y = self.next.y;
    }
}
"#;
    let diagnostic = expect(source, "A", codes::MODULE_RECURSION);
    assert!(diagnostic.message.contains("recursion"), "{diagnostic}");
}

#[test]
fn unknown_names_and_types_are_reported() {
    let unknown_name = r#"
#[hardware_instance]
struct Unknown {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Unknown {
    fn run(&mut self) {
        self.y = missing;
    }
}
"#;
    expect(unknown_name, "Unknown", codes::UNKNOWN_NAME);

    let unknown_type = r#"
#[hardware_instance]
struct BadType {
    #[In] a: uint<4>,
    #[Out] y: tuple,
}
impl Hardware for BadType {
    fn run(&mut self) {
        self.y = self.a;
    }
}
"#;
    expect(unknown_type, "BadType", codes::BAD_TYPE);

    let zero_width = r#"
#[hardware_instance]
struct Zero {
    #[In] a: uint<0>,
    #[Out] y: bool,
}
impl Hardware for Zero {
    fn run(&mut self) {
        self.y = false;
    }
}
"#;
    expect(zero_width, "Zero", codes::BAD_TYPE);
}

#[test]
fn a_hardware_instance_needs_an_impl_and_a_real_top() {
    let no_impl = r#"
#[hardware_instance]
struct Lonely {
    #[In] a: bool,
    #[Out] y: bool,
}
"#;
    expect(no_impl, "Lonely", codes::IMPL);

    let source = r#"
#[hardware_instance]
struct Real {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Real {
    fn run(&mut self) {
        self.y = self.a;
    }
}
"#;
    let diagnostic = expect(source, "Missing", codes::TOP);
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.message.contains("Real")),
        "{diagnostic}"
    );
}

#[test]
fn nested_and_recursive_data_structs_are_rejected() {
    let recursive = r#"
struct Node {
    value: uint<4>,
    next: Node,
}

#[hardware_instance]
struct Use {
    #[In] a: Node,
    #[Out] y: uint<4>,
}
impl Hardware for Use {
    fn run(&mut self) {
        self.y = self.a.value;
    }
}
"#;
    expect(recursive, "Use", codes::LAYOUT);

    let missing_field = r#"
struct Pair {
    a: uint<4>,
    b: uint<4>,
}

#[hardware_instance]
struct Compose {
    #[In] a: uint<4>,
    #[Out] p: Pair,
}
impl Hardware for Compose {
    fn run(&mut self) {
        self.p = Pair { a: self.a };
    }
}
"#;
    let diagnostic = expect(missing_field, "Compose", codes::DECLARATION);
    assert!(diagnostic.message.contains('b'), "{diagnostic}");
}

#[test]
fn match_must_be_exhaustive_with_a_default_or_every_value() {
    let not_exhaustive = r#"
#[hardware_instance]
struct Exhaustive {
    #[In] sel: uint<2>,
    #[Out] y: uint<4>,
}
impl Hardware for Exhaustive {
    fn run(&mut self) {
        match self.sel {
            0 => { self.y = 1; }
            1 => { self.y = 2; }
        }
    }
}
"#;
    let diagnostic = expect(not_exhaustive, "Exhaustive", codes::UNSUPPORTED);
    assert!(
        diagnostic.message.contains("not exhaustive"),
        "{diagnostic}"
    );

    // A default arm plus coverage of the remaining paths is accepted.
    let with_default = r#"
#[hardware_instance]
struct Defaulted {
    #[In] sel: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for Defaulted {
    fn run(&mut self) {
        match self.sel {
            0 => { self.y = 1; }
            _ => { self.y = 0; }
        }
    }
}
"#;
    // A plain bit-pattern `match` is one `case`: the arm is a `case` value and
    // the `_` arm is `default`, compared against the matched signal itself (no
    // snapshot temporary).
    let compiled = compile(with_default, "test.shy.rs", "Defaulted").expect("match with default");
    let verilog = &compiled.verilog;
    assert!(verilog.contains("case (sel)"), "{}", verilog);
    assert!(verilog.contains("4'd0: begin"), "{}", verilog);
    assert!(verilog.contains("y = 4'd1;"), "{}", verilog);
    assert!(verilog.contains("default: begin"), "{}", verilog);
    assert!(verilog.contains("y = 4'd0;"), "{}", verilog);
    assert!(
        !verilog.contains("if ("),
        "a plain bit `match` must not fall back to an if chain:\n{verilog}"
    );
}

#[test]
fn runtime_loop_bounds_are_rejected() {
    let source = r#"
#[hardware_instance]
struct Dynamic {
    #[In] n: uint<4>,
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for Dynamic {
    fn run(&mut self) {
        self.y = 0;
        for i in 0..self.n {
            self.y = self.a;
        }
    }
}
"#;
    expect(source, "Dynamic", codes::NOT_CONSTANT);

    let while_loop = r#"
#[hardware_instance]
struct WhileLoop {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for WhileLoop {
    fn run(&mut self) {
        self.y = false;
        while self.a {
            self.y = true;
        }
    }
}
"#;
    expect(while_loop, "WhileLoop", codes::NOT_CONSTANT);
}

#[test]
fn static_loop_expansion_is_bounded_across_the_whole_nesting() {
    // 100 x 700 bodies is more than the supported expansion, even though each
    // loop on its own would fit.
    let nested = r#"
#[hardware_instance]
struct Nested {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for Nested {
    fn run(&mut self) {
        self.y = 0;
        for i in 0..100 {
            for j in 0..700 {
                self.y = self.a;
            }
        }
    }
}
"#;
    let diagnostic = expect(nested, "Nested", codes::NOT_CONSTANT);
    assert!(diagnostic.message.contains("70000"), "{diagnostic}");

    // Two loops that run one after the other are not a nesting and each fits.
    let sequential = r#"
#[hardware_instance]
struct Sequential {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for Sequential {
    fn run(&mut self) {
        self.y = 0;
        for i in 0..100 {
            self.y = self.a;
        }
        for j in 0..200 {
            self.y = self.a;
        }
    }
}
"#;
    assert!(compile(sequential, "test.shy.rs", "Sequential").is_ok());
}

#[test]
fn an_empty_static_range_produces_nothing_and_is_not_an_error() {
    let driven_elsewhere = r#"
#[hardware_instance]
struct EmptyOk {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for EmptyOk {
    fn run(&mut self) {
        self.y = self.a;
        for i in 0..0 { self.y = 0; }
    }
}
"#;
    assert!(compile(driven_elsewhere, "test.shy.rs", "EmptyOk").is_ok());

    // With nothing else driving it, the usual driver check reports it - not an
    // "internal" error without a location.
    let nothing = r#"
#[hardware_instance]
struct EmptyDriven {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for EmptyDriven {
    fn run(&mut self) {
        for i in 0..0 { self.y = self.a; }
    }
}
"#;
    let diagnostic = expect(nothing, "EmptyDriven", codes::UNDRIVEN);
    assert!(diagnostic.span.is_some(), "{diagnostic}");
}

#[test]
fn inclusive_ranges_that_overflow_are_reported_not_wrapped() {
    // `0..=u64::MAX` cannot be turned into a half-open range; it must not panic
    // and must not silently wrap.
    let huge_loop = r#"
#[hardware_instance]
struct Huge {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for Huge {
    fn run(&mut self) {
        self.y = self.a;
        for i in 0..=18446744073709551615 { self.y = 0; }
    }
}
"#;
    let diagnostic = expect(huge_loop, "Huge", codes::NOT_CONSTANT);
    assert!(diagnostic.message.contains("overflows"), "{diagnostic}");

    let huge_slice = r#"
#[hardware_instance]
struct HugeSlice {
    #[In] a: uint<4>,
    #[Out] y: uint<4>,
}
impl Hardware for HugeSlice {
    fn run(&mut self) {
        self.y = self.a[0..=18446744073709551615];
    }
}
"#;
    expect(huge_slice, "HugeSlice", codes::OUT_OF_RANGE);

    let huge_index = r#"
#[hardware_instance]
struct HugeIndex {
    #[In] a: uint<4>,
    #[Out] y: uint<1>,
}
impl Hardware for HugeIndex {
    fn run(&mut self) {
        self.y = self.a[0..=18446744073709551615];
    }
}
"#;
    expect(huge_index, "HugeIndex", codes::OUT_OF_RANGE);
}

#[test]
fn unsupported_constructs_are_named_not_ignored() {
    // A width-changing `as` is no longer unsupported: it is a range error, and
    // `frontend_signed::a_cast_reinterprets_bits_at_one_width_only` pins it.
    let child_run = r#"
#[hardware_instance]
struct Child {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Child {
    fn run(&mut self) {
        self.y = self.a;
    }
}

#[hardware_instance]
struct Parent {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] kid: Child,
}
impl Hardware for Parent {
    fn run(&mut self) {
        self.kid.run();
        self.kid.a = self.a;
        self.y = self.kid.y;
    }
}
"#;
    let diagnostic = expect(child_run, "Parent", codes::UNSUPPORTED);
    assert!(diagnostic.message.contains("run()"), "{diagnostic}");

    let tuple_struct = r#"
#[hardware_instance]
struct Tuple(#[In] bool);
"#;
    let diagnostic = expect(tuple_struct, "Tuple", codes::DECLARATION);
    assert!(diagnostic.message.contains("named fields"), "{diagnostic}");

    let float_literal = r#"
#[hardware_instance]
struct Floaty {
    #[In] a: bool,
    #[Out] y: uint<4>,
}
impl Hardware for Floaty {
    fn run(&mut self) {
        self.y = 1.5;
    }
}
"#;
    let diagnostic = expect(float_literal, "Floaty", codes::UNSUPPORTED);
    assert!(
        diagnostic.message.contains("floating point"),
        "{diagnostic}"
    );

    let struct_update = r#"
struct Packet {
    op: uint<4>,
    tag: uint<4>,
}

#[hardware_instance]
struct Update {
    #[In] p: Packet,
    #[In] tag: uint<4>,
    #[Out] y: Packet,
}
impl Hardware for Update {
    fn run(&mut self) {
        self.y = Packet { tag: self.tag, ..self.p };
    }
}
"#;
    expect(struct_update, "Update", codes::UNSUPPORTED);

    let seq_macro = r#"
#[hardware_instance]
struct Seq {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Seq {
    fn run(&mut self) {
        seq! { self.y = self.a; }
    }
}
"#;
    expect(seq_macro, "Seq", codes::UNSUPPORTED);

    // `[lo..hi]` is the one way to select bits, for arrays too, so an integer
    // `.slice` is gone and the message names the bracket form.
    let integer_slice = r#"
#[hardware_instance]
struct Slicey {
    #[In] a: uint<8>,
    #[Out] y: uint<4>,
}
impl Hardware for Slicey {
    fn run(&mut self) {
        self.y = self.a.slice(0, 4);
    }
}
"#;
    let diagnostic = expect(integer_slice, "Slicey", codes::UNSUPPORTED);
    assert!(diagnostic.message.contains('['), "{diagnostic}");
}

#[test]
fn the_removed_comb_macro_is_reported_with_its_replacement() {
    // `comb!` is neither accepted nor silently ignored: the diagnosis names the
    // removed construct and says what to write instead.
    let source = r#"
#[hardware_instance]
struct OldComb {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for OldComb {
    fn run(&mut self) {
        comb! {
            self.y = self.a;
        }
    }
}
"#;
    let diagnostic = expect(source, "OldComb", codes::UNSUPPORTED);
    assert!(
        diagnostic.message.contains("`comb!` was removed"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.message.contains("directly in `run`"),
        "{diagnostic}"
    );
}

#[test]
fn always_arms_take_their_roles_from_source_order() {
    // Every arm before the last is an asynchronous reset and the last is the
    // clock, so this order is legal: `rst_n` is the reset, `clk` the clock.
    let ordered = r#"
#[hardware_instance]
struct Ordered {
    #[In] d: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}
impl Hardware for Ordered {
    fn run(&mut self) {
        always! {
            #[Nedge] self.rst_n => { self.q = false; }
            #[Pedge] self.clk => { self.q = self.d; }
        }
    }
}
"#;
    let compiled = compile(ordered, "arms.shy.rs", "Ordered").expect("resets come first");
    assert!(
        compiled
            .verilog
            .contains("always @(posedge clk or negedge rst_n)"),
        "{}",
        compiled.verilog
    );
    let reset = compiled
        .verilog
        .find("if ((!rst_n))")
        .expect("reset branch");
    let clock = compiled.verilog.find("q <= d;").expect("clock body");
    assert!(reset < clock, "the reset branch is the outer one");

    let no_event = r#"
#[hardware_instance]
struct NoEvent {
    #[In] d: bool,
    #[Out] q: bool,
}
impl Hardware for NoEvent {
    fn run(&mut self) {
        always! {
            self.d => { self.q = false; }
        }
    }
}
"#;
    expect(no_event, "NoEvent", codes::SEQ_SHAPE);

    let not_bool = r#"
#[hardware_instance]
struct NotBool {
    #[In] d: uint<4>,
    #[Out] q: bool,
}
impl Hardware for NotBool {
    fn run(&mut self) {
        always! {
            #[Pedge] self.d => { self.q = true; }
        }
    }
}
"#;
    let diagnostic = expect(not_bool, "NotBool", codes::SEQ_SHAPE);
    assert!(diagnostic.message.contains("bool"), "{diagnostic}");

    // A single edge on a plain #[In] needs no marking at all: it is the regular
    // edge branch.
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
            #[Pedge] self.clk => { self.q = self.d; }
        }
    }
}
"#;
    let compiled = compile(single, "test.shy.rs", "Single").expect("a single edge is a clock");
    assert!(
        compiled
            .verilog
            .contains("always @(posedge clk) begin : seq0"),
        "{}",
        compiled.verilog
    );

    // The old `#[Reset]` marker is gone, and every spelling of it says so
    // instead of being ignored: roles are written by source order now.
    for (name, attribute) in [
        ("reset_lower", "#[Reset]"),
        ("reset_snake", "#[reset]"),
        ("async_reset_pascal", "#[AsyncReset]"),
        ("async_reset_snake", "#[async_reset]"),
    ] {
        let source = format!(
            r#"
#[hardware_instance]
struct OldReset {{
    #[In] d: bool,
    #[In] rst_n: bool,
    #[In] clk: bool,
    #[Out] q: bool,
}}
impl Hardware for OldReset {{
    fn run(&mut self) {{
        always! {{
            #[Nedge] {attribute} self.rst_n => {{ self.q = false; }}
            #[Pedge] self.clk => {{ self.q = self.d; }}
        }}
    }}
}}
"#
        );
        let diagnostic = expect(&source, "OldReset", codes::SEQ_SHAPE);
        assert!(
            diagnostic.message.contains("was removed"),
            "{name}: {diagnostic}"
        );
    }

    // An event must come from an input: an output port is rejected.
    let from_output = r#"
#[hardware_instance]
struct FromOutput {
    #[In] d: bool,
    #[Out] clk: bool,
    #[Out] q: bool,
}
impl Hardware for FromOutput {
    fn run(&mut self) {
        always! {
            #[Pedge] self.clk => { self.q = self.d; }
        }
    }
}
"#;
    let diagnostic = expect(from_output, "FromOutput", codes::SEQ_SHAPE);
    assert!(diagnostic.message.contains("output port"), "{diagnostic}");
}

#[test]
fn parse_errors_come_with_a_usable_hint() {
    // The draft's `uint<N+1>` cannot be parsed as Rust; the fix is a brace.
    let unbraced = r#"
#[hardware_instance]
struct Adder<const N: usize> {
    #[In] a: uint<N>,
    #[Out] c: uint<N+1>,
}
impl<const N: usize> Hardware for Adder<N> {
    fn run(&mut self) {
        self.c = self.a;
    }
}
"#;
    let diagnostic = expect(unbraced, "Adder<8>", codes::PARSE);
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.message.contains("uint<{ N + 1 }>")),
        "expected a bracing hint: {diagnostic}"
    );

    // The draft's `let self.c = ...` is not a binding.
    let bad_let = r#"
#[hardware_instance]
struct BadLet {
    #[In] a: bool,
    #[Out] c: bool,
}
impl Hardware for BadLet {
    fn run(&mut self) {
        let self.c = self.a;
    }
}
"#;
    let diagnostic = expect(bad_let, "BadLet", codes::PARSE);
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.message.contains("drop `let`")),
        "expected a binding hint: {diagnostic}"
    );

    // A syntax error inside a macro body points at the token, not at the macro.
    let broken_macro = r#"
#[hardware_instance]
struct BrokenMacro {
    #[In] a: bool,
    #[Out] c: bool,
}
impl Hardware for BrokenMacro {
    fn run(&mut self) {
        always! { self.c = ; }
    }
}
"#;
    let diagnostic = expect(broken_macro, "BrokenMacro", codes::PARSE);
    assert!(diagnostic.message.contains("always!"), "{diagnostic}");
}

#[test]
fn names_that_cannot_be_verilog_identifiers_are_rejected() {
    // `reg` is a Verilog keyword, `expect` and `logic` are SystemVerilog
    // keywords a Verilator run rejects. The frontend never renames a source name
    // silently, so these are reported where they are declared.
    for name in ["reg", "logic", "expect", "always"] {
        let source = format!(
            r#"
#[hardware_instance]
struct Naming {{
    #[In] {name}: bool,
    #[Out] y: bool,
}}
impl Hardware for Naming {{
    fn run(&mut self) {{
        self.y = {name};
    }}
}}
"#
        );
        let diagnostic = expect(&source, "Naming", codes::RESERVED_NAME);
        assert!(diagnostic.message.contains(name), "{diagnostic}");
    }

    let non_ascii = r#"
#[hardware_instance]
struct Numeric {
    #[In] 数据: bool,
    #[Out] y: bool,
}
impl Hardware for Numeric {
    fn run(&mut self) {
        self.y = 数据;
    }
}
"#;
    let diagnostic = expect(non_ascii, "Numeric", codes::RESERVED_NAME);
    assert!(diagnostic.message.contains("ASCII"), "{diagnostic}");

    let raw_identifier = r#"
#[hardware_instance]
struct Raw {
    #[In] r#type: bool,
    #[Out] y: bool,
}
impl Hardware for Raw {
    fn run(&mut self) {
        self.y = r#type;
    }
}
"#;
    let diagnostic = expect(raw_identifier, "Raw", codes::RESERVED_NAME);
    assert!(
        diagnostic.message.contains("raw identifier"),
        "{diagnostic}"
    );
}

#[test]
fn warnings_do_not_stop_compilation() {
    // An unreachable hardware instance and an unread child output are warnings.
    let source = r#"
#[hardware_instance]
struct Child {
    #[In] a: bool,
    #[Out] y: bool,
    #[Out] unused: bool,
}
impl Hardware for Child {
    fn run(&mut self) {
        self.y = self.a;
        self.unused = self.a;
    }
}

#[hardware_instance]
struct Parent {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] kid: Child,
}
impl Hardware for Parent {
    fn run(&mut self) {
        self.kid.a = self.a;
        self.y = self.kid.y;
    }
}

#[hardware_instance]
struct Unused {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for Unused {
    fn run(&mut self) {
        self.y = self.a;
    }
}
"#;
    let compiled = compile(source, "test.shy.rs", "Parent").expect("warnings are not errors");
    let codes: Vec<&str> = compiled
        .warnings
        .iter()
        .map(|warning| warning.code)
        .collect();
    assert!(codes.contains(&codes::UNREAD_OUTPUT), "{codes:?}");
    assert!(codes.contains(&codes::UNREACHABLE), "{codes:?}");
    assert!(compiled.warnings.iter().all(|warning| !warning.is_error()));

    // parse_design carries the same warnings.
    let design = parse_design(source, "test.shy.rs", "Parent").expect("model");
    assert_eq!(design.warnings().len(), compiled.warnings.len());
}
