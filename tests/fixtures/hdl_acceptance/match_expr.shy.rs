// ShyHDL acceptance fixture for `match` in value position: as the whole
// right-hand side of an assignment, as the value of a `let`, inside a larger
// arithmetic expression, and inside an `always!` arm where it must read the
// pre-edge value.
// Compiled by `frontend::compile(source, name, "MatchExprTop")`; never compiled
// by rustc.
//
// This is about the *value* an arm produces, which is a different concern from
// the statement-form `match` in `enums.shy.rs`: there the arms assign to ports,
// here every arm has to yield a value of the expected type. The contract:
//
// * an arm's last expression is the arm's value, and a block arm's tail
//   expression is that value,
// * the expected type comes from the context the `match` sits in, so a typed
//   `let` or an assignment gives the arms their type,
// * the arms of one `match` all have that one type,
// * only the selected arm is evaluated: a statement in an arm that is not taken
//   does not run, which is why `side` can witness which arm ran,
// * in an `always!` arm the assignment is non-blocking, so a `match` that reads
//   the register it assigns sees the value from before the edge.
//
// Two probes make "only the selected arm ran" and "non-blocking" observable as
// values rather than as claims about the lowering.

/// A payload-free enum with three variants, used for the or-pattern case. Three
/// variants put the tag at two bits, and both real tags are covered by an `or`
/// pattern rather than by a repeated arm.
enum Level {
    Idle,
    Busy,
    Done,
}

#[hardware_instance]
struct MatchExprTop {
    #[In] sel: uint<2>,
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    #[In] clk: bool,
    #[In] rst_n: bool,
    // The whole right-hand side is a `match`.
    #[Out] rhs: uint<8>,
    // A `let` without a type annotation takes its type from the arms.
    #[Out] inf: uint<4>,
    // A typed `let` gives the arms their type.
    #[Out] sized: uint<8>,
    // A block arm whose value is its tail expression.
    #[Out] block_tail: uint<4>,
    // Two `match` expressions as the operands of one arithmetic expression.
    #[Out] nested: uint<4>,
    // An enum scrutinee with a payload guard and a fallthrough arm.
    #[Out] guarded: uint<4>,
    // An enum scrutinee matched with an `or` pattern.
    #[Out] picked: uint<4>,
    // The value the side-effecting `match` produced.
    #[Out] taken: uint<4>,
    // Which arm actually ran, as a one-hot marker.
    #[Out] side: uint<4>,
    // A `match` inside an `always!` arm.
    #[Out] seq: uint<4>,
    // The same register read after a non-blocking assignment to it.
    #[Out] seq_snap: uint<4>,
    // The value the inferred `let` produced, and a counter its arm bumps. The
    // counter must be one: it guards against the initialiser being executed
    // twice during inference, when the type is resolved before the expected
    // type is known.
    #[Out] single: uint<4>,
    #[Out] one_shot: uint<4>,
    // Bumped by a `match` arm that sits in a position Rust never evaluates: the
    // right operand of `false && ...` and of `true || ...`, and the untaken
    // branch of an `if`. Every swept vector leaves it at zero; the one extra
    // vector whose `if` branch is taken leaves it at one.
    //
    // Known limitation: the three probes share this one counter, so a failure
    // here shows that some unselected arm ran but not which of the three it was.
    // Separating them would need one counter per probe; the single counter is
    // enough to catch any of the three, which is what the gate needs.
    #[Out] lazy: uint<4>,
    // `false && ...` and `true || ...`: the result is decided by the left
    // operand, so the arm on the right must not run.
    #[Out] and_out: bool,
    #[Out] or_out: bool,
    // The value of an `if` expression whose untaken branch holds the arm above.
    #[Out] if_out: uint<4>,
}

impl Hardware for MatchExprTop {
    fn run(&mut self) {
        // The `match` is the whole right-hand side, and an arm may be a call
        // chain: every arm here is an eight-bit value.
        self.rhs = match self.sel {
            0 => self.a.zext::<8>() + self.b.zext::<8>(),
            1 => self.a.zext::<8>() | self.b.zext::<8>(),
            2 => self.a.zext::<8>() & self.b.zext::<8>(),
            _ => self.a.zext::<8>() ^ self.b.zext::<8>(),
        };

        // An untyped `let`: the arms are four bits wide, so the binding is too.
        let inferred = match self.sel {
            0 => self.a,
            1 => self.b,
            2 => self.a & self.b,
            _ => self.a | self.b,
        };
        self.inf = inferred;

        // A typed `let`: the annotation is the expected type of every arm.
        let widened: uint<8> = match self.sel {
            0 => self.a.zext::<8>(),
            1 => self.b.zext::<8>(),
            2 => self.a.zext::<8>() + self.b.zext::<8>(),
            _ => self.a.zext::<8>() ^ self.b.zext::<8>(),
        };
        self.sized = widened;

        // A block arm: statements, then a tail expression that is the value.
        self.block_tail = match self.sel {
            0 => {
                let t: uint<4> = self.a & self.b;
                t
            }
            1 => {
                let t: uint<4> = self.a | self.b;
                t
            }
            2 => {
                let t: uint<4> = self.a ^ self.b;
                t ^ 0xF
            }
            _ => {
                let t: uint<4> = self.a & self.b;
                t ^ 0xF
            }
        };

        // Two `match` expressions used as the operands of one arithmetic
        // expression, so a value-position `match` nests like any other value.
        self.nested = (match self.sel {
            0 => self.a,
            1 => self.b,
            2 => self.a & self.b,
            _ => self.a | self.b,
        }) ^ (match self.sel {
            0 => self.b,
            1 => self.a,
            2 => self.a ^ self.b,
            _ => self.a,
        });

        // An enum scrutinee built by a `match`, then matched with a payload
        // guard in an arm placed before the unguarded arm for the same variant.
        let opt: Option<uint<4>> = match self.sel {
            0 => None,
            1 => Some(self.a),
            2 => Some(self.b),
            _ => Some(self.a ^ self.b),
        };
        self.guarded = match opt {
            Some(v) if v > 8 => v,
            Some(v) => v ^ 0xF,
            None => 0xF,
        };

        // An `or` pattern over the payload-free enum, so two variants share one
        // arm instead of repeating it.
        let level: Level = match self.sel {
            0 => Level::Idle,
            1 => Level::Busy,
            2 => Level::Done,
            _ => Level::Busy,
        };
        // Three variants put the tag at two bits, so one tag value has no
        // variant; the `_` arm is what makes the `match` exhaustive over the
        // representable domain, even though only declared variants are built.
        //
        // Known limitation: this `_` arm is unreachable from the ports of this
        // module, because `level` is only ever built from a declared variant.
        // The tag with no variant is covered by raw-bit stimulus instead, in
        // `enums.shy.rs`, where a testbench drives the invalid tag directly.
        self.picked = match level {
            Level::Idle | Level::Busy => 0x0,
            Level::Done => 0x1,
            _ => 0xF,
        };

        // Only the arm that is selected runs its statement, so `side` is the
        // one-hot marker of that arm and proves the others were not evaluated.
        self.taken = match self.sel {
            0 => {
                self.side = 0x1;
                self.a
            }
            1 => {
                self.side = 0x2;
                self.b
            }
            2 => {
                self.side = 0x4;
                self.a
            }
            _ => {
                self.side = 0x8;
                self.b
            }
        };

        // One evaluation of an inferred `let`. The arm bumps `counter`, so this
        // value being one is the evidence that the initialiser ran once even
        // though the type had to be inferred before the expected type was known.
        let mut counter: uint<4> = 0;
        let single_value = match self.sel {
            0 => {
                counter = counter + 1;
                self.a
            }
            1 => {
                counter = counter + 1;
                self.b
            }
            2 => {
                counter = counter + 1;
                self.a
            }
            _ => {
                counter = counter + 1;
                self.b
            }
        };
        self.single = single_value;
        self.one_shot = counter;

        // Rust does not evaluate the right operand of `false && ...` or of
        // `true || ...`, so neither arm below may run.
        let mut lazy_v: uint<4> = 0;
        self.and_out = false && (match self.sel {
            _ => {
                lazy_v = lazy_v + 1;
                true
            }
        });
        self.or_out = true || (match self.sel {
            _ => {
                lazy_v = lazy_v + 1;
                false
            }
        });

        // Only the taken `if` branch is evaluated. The condition is false for
        // every swept vector (`b` is never 0xF), so the branch holding the arm is
        // never entered and `lazy` has to stay at zero.
        self.if_out = if self.a == 0xF && self.b == 0xF {
            match self.sel {
                _ => {
                    lazy_v = lazy_v + 1;
                    self.a
                }
            }
        } else {
            self.a ^ self.b
        };
        self.lazy = lazy_v;

        // The value-position `match` inside an edge process. The assignment is
        // non-blocking, so the arm that reads `self.seq` reads the value from
        // before this edge, and the read that follows it sees the same old
        // value rather than the one being written.
        always! {
            #[Nedge] self.rst_n => {
                self.seq = 0;
                self.seq_snap = 0;
            }
            #[Pedge] self.clk => {
                self.seq = match self.sel {
                    0 => self.a,
                    1 => self.b,
                    2 => self.seq,
                    _ => self.seq ^ 0xF,
                };
                self.seq_snap = self.seq;
            }
        }
    }
}
