// Hand-written Verilog testbench for the statements a `run` body forms by
// itself: source order, last write wins, read-after-write, a run-local snapshot
// that reaches a child instance, a child built only from constants, and one
// output with two owners.
//
// Every output is checked against its own independent expectation, so a DUT that
// resolved the read-after-write wrongly cannot hide behind another output that
// happens to agree. `ConstOnly` has no input at all, so it is also instantiated
// here directly: its output has to be a known value with no stimulus, which a
// process without a sensitivity list cannot produce.
//
// `mixed` is driven by two owners: the low nibble is a constant from the
// combinational process, the high nibble is a register written at the edge. The
// three registered reads (`seen` by dynamic index, `mirror` as a whole value,
// `cross` as a static slice across the boundary) all sample the composed value
// from *before* the edge, so they carry the previous high nibble with the
// constant low one. The whole-value read is what catches a nibble order that
// came out reversed.
//
// Verilog-2005 only: unknown bits are detected with a reduction XOR rather than
// with the SystemVerilog `$isunknown`. The `!==` comparisons catch `x` as well.
//
// Protocol: exactly one `SHYHDL-AC-PASS overwrite ...` line on success,
// otherwise `SHYHDL-AC-FAIL overwrite ...` lines. A run with no verdict is a
// failure.
//
// Expects the frontend-generated modules: Overwrite, Bump, ConstOnly.

`timescale 1ns/1ps

module tb_overwrite;
    localparam integer EXPECTED_COMBOS = 1024;
    localparam integer EXPECTED_MIXED = 128;

    reg  [7:0] a8, b8;
    reg  [7:0] i8;
    reg  [3:0] hi4;
    reg        clk, rst_n;
    reg  [7:0] expect_z, expect_bump;
    reg  [7:0] expect_mixed;
    reg  [3:0] expect_cross;
    reg  [3:0] old_high;

    wire [7:0] z, y, snap_out, k, bump_out, k_direct;
    wire [1:0] bits, bits_direct;
    wire [7:0] merged, merged_direct;
    wire [7:0] mixed;
    wire       seen;
    wire [7:0] mirror;
    wire [3:0] cross;

    integer i, j, combos, mixed_combos, failures;
    reg     saw_overwrite, saw_old_state;

    Overwrite dut_overwrite (
        .a(a8),
        .b(b8),
        .i(i8),
        .hi(hi4),
        .clk(clk),
        .rst_n(rst_n),
        .z(z),
        .y(y),
        .snap_out(snap_out),
        .k(k),
        .bits(bits),
        .merged(merged),
        .bump_out(bump_out),
        .mixed(mixed),
        .seen(seen),
        .mirror(mirror),
        .cross(cross)
    );

    ConstOnly dut_const_only (.k(k_direct), .bits(bits_direct), .merged(merged_direct));

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL overwrite %0s a8=%0d b8=%0d i=%0d hi=%0d got=%h want=%h",
                             what, a8, b8, i8, hi4, got, want);
            end
        end
    endtask

    // A constant-only process must not leave unknown bits behind. A reduction
    // XOR is `x` as soon as one bit is `x`, in plain Verilog-2005.
    task check_known;
        input [255:0] what;
        input [7:0]   got;
        begin
            if ((^got) === 1'bx) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL overwrite %0s is unknown at t=%0t", what, $time);
            end
        end
    endtask

    initial begin
        failures = 0;
        combos = 0;
        mixed_combos = 0;
        saw_overwrite = 1'b0;
        saw_old_state = 1'b0;
        a8 = 8'h00;
        b8 = 8'h00;
        i8 = 8'h00;
        hi4 = 4'h0;
        clk = 1'b0;
        rst_n = 1'b0;

        // The combinational outputs do not depend on the edge process, so the
        // sweep runs with the register side held in reset.
        for (i = 0; i < 256; i = i + 1) begin
            for (j = 0; j < 4; j = j + 1) begin
                a8 = i;
                // `b` differs from `a` in the first two cases, so "the last
                // write wins" and "the snapshot kept the earlier value" are two
                // distinguishable claims rather than one.
                case (j)
                    0: b8 = a8 + 8'h01;
                    1: b8 = ~a8;
                    2: b8 = 8'h00;
                    default: b8 = 8'hFF;
                endcase
                #1;
                combos = combos + 1;
                // Expected values are computed in 8-bit variables: in a wider
                // context Verilog would not wrap the addition.
                expect_z = a8 + 8'h01;
                expect_bump = a8 + 8'h01;
                check_value("z.read-after-write", z, expect_z);
                check_value("y.last-write-wins", y, b8);
                check_value("snap_out.source-order", snap_out, a8);
                check_value("bump_out.snapshot-through-child", bump_out, expect_bump);
                check_known("k.through-parent", k);
                check_value("k.through-parent", k, 8'h00);
                check_known("k.direct", k_direct);
                check_value("k.direct", k_direct, 8'h00);
                // Both bits written without seeding the value: bit 1 true and
                // bit 0 false is `2'b10`.
                check_known("bits.through-parent", bits);
                check_value("bits.through-parent", bits, 2'b10);
                check_known("bits.direct", bits_direct);
                check_value("bits.direct", bits_direct, 2'b10);
                // A branch on a build-time constant still has to merge: the
                // taken arm is the `else`, so the value is 0x5A either way.
                check_known("merged.through-parent", merged);
                check_value("merged.through-parent", merged, 8'h5A);
                check_known("merged.direct", merged_direct);
                check_value("merged.direct", merged_direct, 8'h5A);
                if (y !== snap_out)
                    saw_overwrite = 1'b1;
            end
        end

        // Without a case where the overwritten value differs from the snapshot,
        // the two checks above could both pass on one shared value.
        if (!saw_overwrite) begin
            failures = failures + 1;
            $display("SHYHDL-AC-FAIL overwrite y never differed from snap_out, so the overwrite \
                      was never observed");
        end

        // The reset gives the register side an explicit initial value.
        check_known("mixed.after-reset", mixed);
        check_value("mixed.low-is-the-constant", mixed[3:0], 4'h6);
        check_value("mixed.high-is-zero-after-reset", mixed[7:4], 4'h0);
        check_value("seen.is-zero-after-reset", seen, 1'b0);
        check_value("mirror.is-zero-after-reset", mirror, 8'h00);
        check_value("cross.is-zero-after-reset", cross, 4'h0);

        rst_n = 1'b1;
        old_high = 4'h0;
        for (j = 0; j < 16; j = j + 1) begin
            hi4 = j[3:0];
            for (i = 0; i < 8; i = i + 1) begin
                i8 = i;
                expect_mixed = { old_high, 4'h6 };
                expect_cross = { old_high[1:0], 2'b01 };

                clk = 1'b1;
                #1;
                clk = 1'b0;
                #1;
                mixed_combos = mixed_combos + 1;

                // All three registered reads carry the state from before the
                // edge: the previous high nibble with the constant low one.
                check_value("seen.dynamic-bit-old-state", seen, expect_mixed[i]);
                check_value("mirror.whole-value-old-state", mirror, expect_mixed);
                check_value("cross.slice-across-boundary", cross, expect_cross);
                // The edge loaded the new high nibble and left the constant low.
                check_value("mixed.high-loaded-at-edge", mixed[7:4], hi4);
                check_value("mixed.low-stays-constant", mixed[3:0], 4'h6);
                // `mirror` and `mixed` disagree exactly when the edge changed
                // the high nibble, which is what proves `mirror` is the old
                // state and not the new one.
                if (mirror !== { hi4, 4'h6 })
                    saw_old_state = 1'b1;
                old_high = hi4;
            end
        end

        if (!saw_old_state) begin
            failures = failures + 1;
            $display("SHYHDL-AC-FAIL overwrite mirror never differed from the newly loaded \
                      value, so the old state was never observed");
        end

        if (failures == 0 && combos == EXPECTED_COMBOS && mixed_combos == EXPECTED_MIXED)
            $display("SHYHDL-AC-PASS overwrite combos=%0d mixed=%0d", combos, mixed_combos);
        else
            $display("SHYHDL-AC-FAIL overwrite failures=%0d combos=%0d expected_combos=%0d mixed=%0d expected_mixed=%0d",
                     failures, combos, EXPECTED_COMBOS, mixed_combos, EXPECTED_MIXED);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL overwrite timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
