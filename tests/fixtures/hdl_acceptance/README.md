# HDL acceptance fixtures

These files belong to `tests/hdl_acceptance.rs`, the end-to-end acceptance of
the M1/M4 frontend.

## How a test runs

1. A `.shy.rs` fixture is compiled by `shyhdl::frontend::compile`.
2. The generated Verilog is written to `target/hdl_acceptance/<test>/`.
3. `iverilog` compiles the generated DUT together with a hand-written `tb_*.v`
   testbench, and `vvp` runs it.
4. The testbench compares every output against a reference model written in the
   testbench itself, and prints a verdict.

The testbenches are hand-written on purpose. A failure here must mean the
generated DUT is wrong, never that ShyHDL's own testbench generator is wrong.
The generated-testbench paths are verified separately by `tests/testbench_gen.rs`
and `tests/testbench_enums.rs`.

## Verdict protocol

- Success is exactly one line `SHYHDL-AC-PASS <group> ...`.
- Failure is one or more `SHYHDL-AC-FAIL <group> ...` lines.
- A run that prints no verdict is a failure in the Rust test, even when `vvp`
  exited successfully, so a testbench that dies early cannot look like a pass.
- Each testbench also checks that it performed the expected number of
  comparisons, so a sweep that silently does nothing cannot pass.

## Files

| File | Pins down |
| --- | --- |
| `adder.shy.rs` | AC-MVP-001..004: `Add<const N>` at N=1 and N=8, a wrapper that instantiates the adder and connects it by assignment (no `run()` call), two instances of the same specialisation, and a two-level hierarchy. Top module `AdderTop`. |
| `tb_adder.v` | All 65536 input pairs plus all four 1-bit pairs, compared against a bit-level ripple-carry reference model (never the `+` operator). |
| `sequential.shy.rs` | AC-MVP-005..008: asynchronous active-low reset, held reset, enable hold, two-stage pipeline old value. Top module `SeqTop`. |
| `tb_sequential.v` | Owns the clock and counts its own edges, so "no clock edge happened" is evidence and not an assumption. |
| `packet_make.shy.rs`, `packet_split.shy.rs` | AC-MVP-015: the 44-bit `Packet` struct, packed most significant field first in declaration order. |
| `tb_packet.v` | States the layout independently as `{ op, addr, flag, tag }`, wires `PacketMake.pkt` into `PacketSplit.pkt`, and sweeps every `op` and `tag`. |
| `overwrite.shy.rs` | The statements a `run` body forms by itself, as one implicit combinational process in source order: `z` reads the value written before it (AC-MVP-012), `y` is overwritten after a snapshot read, and a run-local `let` reaches a child instance input. The child `ConstOnly` has no input at all, so its three outputs have to come out as dataflow rather than as a process waiting for an event: `k` (read-after-write, then a partial write), `bits` (both bits of a `uint<2>` written without seeding the value) and `merged` (a branch on a build-time constant whose two arms both assign). `mixed` is the one output with two owners: a constant low nibble from the combinational process and a register high nibble from the edge process, with `seen`, `mirror` and `cross` registered from the composed value *before* each edge. |
| `tb_overwrite.v` | Every output against its own independent expectation over 1024 input pairs: `z = a + 1`, `y = b`, `snap_out = a`, `bump_out = a + 1`, `k = 0`, `bits = 2'b10`, `merged = 0x5A`, each also checked for unknown bits with a Verilog-2005 reduction XOR instead of `$isunknown`. `ConstOnly` is instantiated directly as well, so its values are checked with no stimulus at all. A case where `y` differs from `snap_out` is required, so the two claims cannot collapse into one. Then 128 mixed-owner steps (16 high nibbles × 8 indices) check the dynamic bit, the whole value (which catches a nibble order that came out reversed) and a static slice across the boundary, all against the pre-edge state, after a reset that gives the register an explicit initial value. |
| `partselect.shy.rs` | Disjoint partial-width writes: `y[0..4]` from `a` and `y[4..8]` from `y[0..4]`. Legal hardware, no combinational loop, even though a signal-level dependency graph contains a `y -> y` edge. |
| `tb_partselect.v` | `y` is always `{ a, a }`, over the whole 4-bit input space. Kept separate from `tb_overwrite.v` so neither verdict can hide the other. |
| `signed.shy.rs` | `int<W>` semantics: `Signed4` sweeps all 256 four-bit pairs through same-width `+ - *`, bitwise operators, shifts, negation, the six comparisons and the same comparison after `as uint<4>`; `Signed8` covers the -128 and 127 boundaries with `sext` to 16 bits; `PairExt` reads a struct field that is signed next to an unsigned one, sign-extends it, truncates it back, shifts the field expression itself and part-selects it into raw bits; `SignedConst` has no input at all and keeps a negative literal signed through a constant branch and a local. `SignedTop` wires every leaf through one level of hierarchy. |
| `tb_signed.v` | Each expected value comes from Verilog integer arithmetic with the result truncated to the width under test, and the shifts are defined bitwise (a sign bit copied for arithmetic, a zero for logical). Per the whole 4-bit space, the six boundary pairs and the 16 field nibbles. It also insists on witnesses: a signed comparison that disagrees with the unsigned one, an arithmetic shift that disagrees with the logical one, a part-select that is not the signed reading of its bits, and a negative literal whose signed value differs from the same bits read as unsigned. |
| `arrays.shy.rs` | Fixed-length arrays: `ArrConst` builds a repeat literal, a list literal, a constant element write, a whole-array copy of the same shape, a half-open sub-array and a nested literal, and reads elements by constant, dynamic and nested-dynamic index; `ArrDynRead` reads a three-element array and a one-element `[bool; 1]` by dynamic index, each with an index that has no element; `ArrDynWrite` defaults the whole array before a combinational dynamic write and reads it back in the same process, and its sequential dynamic write leaves the other elements on their old value while a snapshot reads the element being written; `ArrNest` has an array of structs whose `tag` is signed, and a struct holding an array. `ArrTop` wires every leaf through one level of hierarchy. |
| `tb_arrays.v` | Expectations are built here by slicing and concatenating the raw bits by hand, with element 0 as the least significant slice, so the layout under test never computes its own expectation. An out-of-range dynamic read, including a nested inner index, has to be the standard unknown, detected with a reduction XOR rather than a SystemVerilog system function. A nested dynamic write and a dynamic field write have to change only the element or field they select, leaving its neighbour at the default, and a bit select of one element must apply to that element rather than to the whole array. 79 checks: 16 constant, 4 dynamic read, 4 combinational write, 9 nested write, 3 field write, 5 sequential steps, 16 nesting, 4 one-element, 6 bit-select, 4 cast-index and 8 nested-dynamic. |
| `enums.shy.rs` | A user enum with payloads: `Msg` has `Idle`, `Data(uint<4>)` and a named `Pair { hi: int<4>, lo: uint<4> }`, so the tag is two bits (`max(1, ceil(log2 3))`) with one tag value that has no variant, the payload field is as wide as the widest variant, the payload bits a variant does not use are zero, and a named payload packs its fields most significant first. `EnumMatch` destructures with a payload guard in an arm placed before the unguarded arm for the same variant, and ends in `_` for the tag without a variant. `OrMatch` matches an `or` pattern with a guard where both alternatives bind the same name. `EnumTop` wires the construction and both matches through one level of hierarchy. |
| `tb_enums.v` | The layout is written out here as the lowest tag slice plus the payload above it, so the design never supplies its own expectation. Construction covers the zeroed unused payload bits and the field order of the named payload. The `match` cases cover every real tag, both sides of the payload guard, and the tag with no variant reaching `_`. The `or`-guard cases pin that a first alternative whose guard fails falls back to the second alternative with its own binding, and that the binding stays inside the arm instead of shadowing the `let` above it. 49 checks: 16 construction, 8 match arms, 16 hierarchy, 9 `or`-guard (3 vectors x 3 outputs). |
| `options.shy.rs` | The built-in `Option<T>` and `Result<T, E>`: `None` is tag 0 and `Some` tag 1, `Ok` is tag 0 and `Err` tag 1, and the payload is as wide as the widest case, so `Option<uint<4>>` is a one-bit tag under four payload bits and `Option<Result<uint<4>, int<8>>>` widens twice. A nested construction takes its value from the expected type, so `Some(Ok(x))` builds the nested value with no user enum. The `Option<bool>` match is exhaustive in three arms with no `_`, and the nested `Result` is destructured through `Some(Ok(x))` and `Some(Err(e))`. Equality is driven from raw bits. |
| `tb_options.v` | Every width is stated here by hand, so the tag-and-payload layout is never read back from the design. Construction checks the zeroed high half of the `Ok` payload and the signed `Err` payload. The equality cases drive raw bits: a `Some` with a different payload must differ, junk in a `None` payload must not, and an `Ok` against an `Err` differ even where the shared payload bits agree. 32 checks: 16 construction, 4 exhaustive nested arms, 4 deep nested construction, 8 equality. |
| `wrapped.shy.rs` | An enum inside another container: `Envelope { msg: Msg, extra: uint<2> }` and `[Msg; 2]`, so the enum's own layout has to compose with struct field order (most significant first) and array element order (element 0 in the least significant bits). Equality compares the tag and, for a variant that carries one, its payload; the payload bits a variant does not use are not part of the comparison. |
| `tb_wrapped.v` | The struct, the array and the enum inside each are laid out by hand. The equality cases drive raw bits: junk in the payload bits a variant does not use cannot change the verdict, a value whose tag has no variant compares on its tag alone, and one differing element makes two arrays differ. 27 checks: 16 wrapped construction, 11 equality. |
| `match_expr.shy.rs` | `match` in value position, the concern `enums.shy.rs` does not cover: an arm's last expression is the arm's value and a block arm's tail expression is that value, with the expected type coming from the context (the whole right-hand side, a typed `let`, or an untyped `let` that infers it from the arms). Covers two `match` expressions as the operands of one arithmetic expression, an enum scrutinee with a payload guard and a fallthrough arm, and an `or` pattern over a payload-free enum that ends in `_` for the tag with no variant. Three probes make evaluation observable: `side` is a one-hot marker of the arm that actually ran, so an arm that is not selected must not run its statement; `one_shot` counts the initialiser of an inferred `let`, which must execute once even though its type is resolved before the expected type is known; and `lazy` counts arms in positions Rust never evaluates, namely the right operand of `false && ...` and of `true || ...`, and the untaken branch of an `if`. `seq`/`seq_snap` sit in an `always!` arm, where the non-blocking assignment means a `match` reading the register it assigns sees the value from before the edge. |
| `tb_match_expr.v` | Expectations are written out here in plain integer arithmetic and branches, never taken from the design. The sweep is every (sel, a, b) with sel 0..3, a 0..15 and b in {3, 9}, 128 vectors, each comparing all fifteen combinational outputs so a wrong arm cannot hide behind a right one; `lazy` is the one output whose expectation depends on the vector, since only the extra vector reaches the `if` branch. That extra vector (a=0xF, b=0xF) exercises the positive direction of the `if` probe: its arm runs once, `if_out` becomes `a` and `lazy` becomes one, while the short-circuit results stay false and true. Eight clocked steps then check the registered value after each edge and the snapshot taken at that edge. 138 checks: 128 sweep, 1 extra `if` vector, 1 after reset and 8 clocked steps. |
| `selfcheck/reference_*.v` | Hand-written reference DUTs with the interface the frontend is contracted to emit. Used only by the opt-in diagnosis test, never by the acceptance gate. |
| `selfcheck/mutated_adder.v` | The same interface with a wrong `Add_N8`. This is the one negative control in the gate: the adder testbench must reject it, otherwise it would be passing vacuously. |

## Reference DUTs are not part of the gate

The gate runs the *generated* Verilog. Passing a correct hand-written DUT through
the same testbench would only repeat a verdict the testbench already reached, so
the reference DUTs are opt-in:

```text
cargo test --test hdl_acceptance -- --ignored
```

Use that when an acceptance failure needs a diagnosis: if the reference DUT
passes and the generated one fails, the testbench is fine and the frontend is
not.

## Toolchain

`iverilog`, `vvp` and GNU `timeout` must be on `PATH` as system-installed tools;
this checkout does not provide or source an environment script for them. A
missing tool fails the test with a message that names the tool and the `PATH` it
searched. Nothing here skips silently: a skipped acceptance test turns into a
pass without anyone noticing.
Every external call is bounded by `timeout`, because a combinational loop that
never settles makes `vvp` spin forever instead of failing.
