// ShyHDL acceptance fixture for disjoint part-select writes. Compiled by
// `frontend::compile(source, name, "PartSelect")`; never compiled by rustc.
//
// `y[0..4]` is written from `a`, and `y[4..8]` is written from `y[0..4]`. The
// two ranges do not overlap, so this is legal hardware with no combinational
// loop, even though the signal-level dependency graph contains a `y -> y` edge.
// The result is `y = { a, a }`.
//
// NOTE: partial-width writes are the construct this fixture exists to pin down.
// If the frontend rejects the syntax, or reports a signal-level combinational
// loop, that is a frontend defect: the hardware below is legal and must not be
// worked around in the testbench.

#[hardware_instance]
struct PartSelect {
    #[In] a: uint<4>,
    #[Out] y: uint<8>,
}

impl Hardware for PartSelect {
    fn run(&mut self) {
        self.y[0..4] = self.a;
        self.y[4..8] = self.y[0..4];
    }
}
