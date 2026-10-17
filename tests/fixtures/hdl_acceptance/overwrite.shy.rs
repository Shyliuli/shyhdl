// ShyHDL acceptance fixture for the statements a `run` body forms by itself.
// Compiled by `frontend::compile(source, name, "Overwrite")`; never compiled by
// rustc.
//
// `run` behaviour is one implicit combinational process, evaluated in source
// order, where a later write to the same bits wins:
//
// * `z` reads the value written by the statement before it (AC-MVP-012),
// * `y` is written twice with a read in between, so the snapshot keeps the
//   first value while `y` ends on the second,
// * `snap` is a run-local net, and structural wiring (a child input) reads it,
// * `ConstOnly` is a child with no input at all, so its process has no event to
//   start from: its three outputs have to come out as dataflow values,
// * `mixed` lives in `MixedOwner`, a child whose own `run` reads nothing: its
//   low nibble is a constant and its high nibble is a register written at the
//   edge, so the two owners really are a dataflow value and a process, not two
//   halves of one dynamic process. `Overwrite` only copies the child's outputs,
//   which is ordinary combinational behaviour.
//
// Writing the same bits twice inside one process is legal: the multi-driver rule
// is about two processes driving one bit. Child input connections are structural
// wiring and are not part of this process.
//
// This alias case pairs a no-input constant combinational process with a
// sequential one, and `seen`, `mirror` and `cross` are registers sampled at the
// sequential edge, so they hold the composed value from before that edge. Two
// process owners on one signal are a different situation, and the blocking
// read-after-write inside a single process is covered by `z` and `y` above; what
// this case is about is the continuous-plus-process split.
//
// How the frontend lowers the dynamic process is its own business: an
// `always @(*)` block with blocking assignments is a legitimate lowering, and
// nothing here asserts otherwise. What the oracle pins down is the values.

#[hardware_instance]
struct Bump {
    #[In] d: uint<8>,
    #[Out] q: uint<8>,
}

impl Hardware for Bump {
    fn run(&mut self) {
        self.q = self.d + 1;
    }
}

#[hardware_instance]
struct ConstOnly {
    #[Out] k: uint<8>,
    #[Out] bits: uint<2>,
    #[Out] merged: uint<8>,
}

impl Hardware for ConstOnly {
    fn run(&mut self) {
        self.k = 0;
        self.k = self.k + 1;
        self.k[0] = false;

        // Both bits are written, but neither the whole value nor either bit is
        // seeded first.
        self.bits[0] = false;
        self.bits[1] = true;

        // The condition is a build-time constant alias, and both arms assign.
        let flag: bool = false;
        if flag {
            self.merged = 0xA5;
        } else {
            self.merged = 0x5A;
        }
    }
}

/// One output with two owners, in a module whose `run` reads no input at all.
///
/// The low nibble is a constant, so the combinational process here has an empty
/// edge set and its value has to become dataflow rather than a process waiting
/// for an event. The high nibble is a register written at the edge. The three
/// registered reads sample the composed value as it was before the edge, which
/// is the previous high nibble with the constant low one.
#[hardware_instance]
struct MixedOwner {
    #[In] i: uint<8>,
    #[In] hi: uint<4>,
    #[In] clk: bool,
    #[In] rst_n: bool,
    #[Out] mixed: uint<8>,
    #[Out] seen: bool,
    #[Out] mirror: uint<8>,
    #[Out] cross: uint<4>,
}

impl Hardware for MixedOwner {
    fn run(&mut self) {
        self.mixed[0..4] = 0x6;

        always! {
            if negedge(self.rst_n) {
                self.mixed[4..8] = 0;
                self.seen = false;
                self.mirror = 0;
                self.cross = 0;
            } else if posedge(self.clk) {
                // Every assignment is non-blocking, so all three reads see the
                // state from before the edge.
                self.seen = self.mixed[self.i];
                self.mirror = self.mixed;
                self.cross = self.mixed[2..6];
                self.mixed[4..8] = self.hi;
            }
        }
    }
}

#[hardware_instance]
struct Overwrite {
    #[In] a: uint<8>,
    #[In] b: uint<8>,
    #[In] i: uint<8>,
    #[In] hi: uint<4>,
    #[In] clk: bool,
    #[In] rst_n: bool,
    #[Out] z: uint<8>,
    #[Out] y: uint<8>,
    #[Out] snap_out: uint<8>,
    #[Out] k: uint<8>,
    #[Out] bits: uint<2>,
    #[Out] merged: uint<8>,
    #[Out] bump_out: uint<8>,
    #[Out] mixed: uint<8>,
    #[Out] seen: bool,
    #[Out] mirror: uint<8>,
    #[Out] cross: uint<4>,
    #[instance] bump: Bump,
    #[instance] const_only: ConstOnly,
    #[instance] mixed_owner: MixedOwner,
}

impl Hardware for Overwrite {
    fn run(&mut self) {
        self.z = self.a;
        self.z = self.z + 1;

        self.y = self.a;
        let snap: uint<8> = self.y;
        // The nested `snap` shadows the outer one only inside this block: it
        // must not become the module-level signal the outer reads below.
        if self.a[0] {
            let snap: uint<8> = snap + 1;
            self.z = snap;
        }
        self.y = self.b;

        self.bump.d = snap;
        self.bump_out = self.bump.q;
        self.snap_out = snap;

        // The constant process lives in its own module, so its process really
        // has no input and therefore nothing to trigger it.
        self.k = self.const_only.k;
        self.bits = self.const_only.bits;
        self.merged = self.const_only.merged;

        // The mixed-owner output is produced inside the child, whose `run` has
        // no edges of its own; reading its outputs here is ordinary behaviour.
        self.mixed_owner.i = self.i;
        self.mixed_owner.hi = self.hi;
        self.mixed_owner.clk = self.clk;
        self.mixed_owner.rst_n = self.rst_n;
        self.mixed = self.mixed_owner.mixed;
        self.seen = self.mixed_owner.seen;
        self.mirror = self.mixed_owner.mirror;
        self.cross = self.mixed_owner.cross;
    }
}
