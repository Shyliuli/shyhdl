// Hand-written Verilog testbench for `int<W>` semantics.
//
// The reference is an independent integer-domain computation — Verilog `integer`
// arithmetic — followed by explicit truncation to the width under test, and the
// expected value is compared as a bit pattern. The bit shifts are defined
// explicitly rather than by an operator: an arithmetic right shift replicates
// the sign bit, a logical one shifts a zero in, and a part-select is raw bits.
//
// Each group also insists on a witness, because a testbench full of zeroes and
// ones can pass while the DUT has no signed semantics at all:
//
// * a signed comparison that disagrees with the unsigned one (4- and 8-bit),
// * an arithmetic right shift that disagrees with the logical one (4- and 8-bit),
// * a part-select whose unsigned comparison disagrees with reading those bits as
//   signed, and
// * a constant-only negative literal whose signed value differs from the same
//   bits read as unsigned.
//
// Verilog-2005 only. Protocol: exactly one `SHYHDL-AC-PASS signed ...` line on
// success, otherwise `SHYHDL-AC-FAIL signed ...` lines.
//
// Expects the frontend-generated modules: Signed4, Signed8, PairExt,
// SignedConst, SignedTop.

`timescale 1ns/1ps

module tb_signed;
    localparam integer EXPECTED_COMBOS = 256;
    localparam integer EXPECTED_BOUNDS = 6;
    localparam integer EXPECTED_STRUCT = 16;

    reg  [3:0] a4, b4;
    wire [3:0] sum4, diff4, prod4, band4, bor4, bxor4, shl4, shr4, neg4;
    wire       lt4, le4, gt4, ge4, eq4, ne4, ult4;

    reg  [7:0]  a8r, b8r;
    wire [7:0]  sum8, shr8, neg8;
    wire        lt8, ult8;
    wire [15:0] ext8;

    reg  [7:0] pkt;
    reg  [3:0] t4;
    wire [3:0] ps, pu, pback, pshr;
    wire [7:0] pext;
    wire       plt, plowlt;
    wire [1:0] plow;

    wire [3:0] cv, cvshr, cu;

    wire [3:0]  t_sum4, t_shr4, t_pshr;
    wire [15:0] t_ext8;

    integer i, checks, bound_checks, struct_checks, failures;
    integer ai, bi, si, ti;
    reg saw_div4, saw_div8, saw_shift4, saw_shift8, saw_slice, saw_const;

    Signed4 dut4 (
        .a(a4),
        .b(b4),
        .sum(sum4),
        .diff(diff4),
        .prod(prod4),
        .band(band4),
        .bor(bor4),
        .bxor(bxor4),
        .shl(shl4),
        .shr(shr4),
        .neg(neg4),
        .lt(lt4),
        .le(le4),
        .gt(gt4),
        .ge(ge4),
        .eq(eq4),
        .ne(ne4),
        .ult(ult4)
    );

    Signed8 dut8 (
        .a(a8r),
        .b(b8r),
        .sum(sum8),
        .shr(shr8),
        .neg(neg8),
        .lt(lt8),
        .ult(ult8),
        .ext(ext8)
    );

    PairExt dutp (
        .p(pkt),
        .t(t4),
        .s(ps),
        .u(pu),
        .ext(pext),
        .back(pback),
        .shr(pshr),
        .lt(plt),
        .low(plow),
        .low_lt(plowlt)
    );

    SignedConst dutc (.v(cv), .v_shr(cvshr), .u(cu));

    SignedTop dut_top (
        .a4(a4),
        .b4(b4),
        .a8(a8r),
        .b8(b8r),
        .p(pkt),
        .t(t4),
        .sum4(t_sum4),
        .shr4(t_shr4),
        .ext8(t_ext8),
        .pshr(t_pshr)
    );

    /// Sign-extend a 4-bit pattern into an integer.
    function integer s4;
        input [3:0] x;
        begin
            s4 = x[3] ? (x - 16) : x;
        end
    endfunction

    /// Sign-extend an 8-bit pattern into an integer.
    function integer s8f;
        input [7:0] x;
        begin
            s8f = x[7] ? (x - 256) : x;
        end
    endfunction

    /// Sign-extend a 2-bit pattern into an integer.
    function integer s2;
        input [1:0] x;
        begin
            s2 = x[1] ? (x - 4) : x;
        end
    endfunction

    function [3:0] wrap4;
        input integer x;
        begin
            wrap4 = x & 15;
        end
    endfunction

    function [7:0] wrap8;
        input integer x;
        begin
            wrap8 = x & 255;
        end
    endfunction

    /// Arithmetic right shift by one, defined bitwise: the sign bit is copied.
    function [3:0] ashr4;
        input [3:0] x;
        begin
            ashr4 = { x[3], x[3:1] };
        end
    endfunction

    function [7:0] ashr8;
        input [7:0] x;
        begin
            ashr8 = { x[7], x[7:1] };
        end
    endfunction

    /// Logical right shift by one, for the witness.
    function [3:0] lshr4;
        input [3:0] x;
        begin
            lshr4 = { 1'b0, x[3:1] };
        end
    endfunction

    function [7:0] lshr8;
        input [7:0] x;
        begin
            lshr8 = { 1'b0, x[7:1] };
        end
    endfunction

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL signed %0s a4=%h b4=%h a8=%h b8=%h p=%h t=%h got=%h want=%h",
                             what, a4, b4, a8r, b8r, pkt, t4, got, want);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        bound_checks = 0;
        struct_checks = 0;
        saw_div4 = 1'b0;
        saw_div8 = 1'b0;
        saw_shift4 = 1'b0;
        saw_shift8 = 1'b0;
        saw_slice = 1'b0;
        saw_const = 1'b0;
        a4 = 4'h0;
        b4 = 4'h0;
        a8r = 8'h00;
        b8r = 8'h00;
        pkt = 8'h00;
        t4 = 4'h0;

        // All 256 four-bit pairs: arithmetic, bitwise, shifts, negation and the
        // comparisons.
        for (i = 0; i < EXPECTED_COMBOS; i = i + 1) begin
            a4 = i[3:0];
            b4 = i[7:4];
            ai = s4(a4);
            bi = s4(b4);
            #1;
            checks = checks + 1;
            check_value("sum.wrap4", sum4, wrap4(ai + bi));
            check_value("diff.wrap4", diff4, wrap4(ai - bi));
            check_value("prod.wrap4", prod4, wrap4(ai * bi));
            check_value("band.bits4", band4, a4 & b4);
            check_value("bor.bits4", bor4, a4 | b4);
            check_value("bxor.bits4", bxor4, a4 ^ b4);
            check_value("shl.bits4", shl4, { a4[2:0], 1'b0 });
            check_value("shr.arithmetic4", shr4, ashr4(a4));
            check_value("neg.wrap4", neg4, wrap4(-ai));
            check_value("cmp.lt4", lt4, (ai < bi));
            check_value("cmp.le4", le4, (ai <= bi));
            check_value("cmp.gt4", gt4, (ai > bi));
            check_value("cmp.ge4", ge4, (ai >= bi));
            check_value("cmp.eq4", eq4, (ai == bi));
            check_value("cmp.ne4", ne4, (ai != bi));
            check_value("cast.uint-lt4", ult4, (a4 < b4));
            check_value("top.sum4", t_sum4, wrap4(ai + bi));
            check_value("top.shr4", t_shr4, ashr4(a4));
            if ((ai < bi) !== (a4 < b4))
                saw_div4 = 1'b1;
            if (ashr4(a4) !== lshr4(a4))
                saw_shift4 = 1'b1;
        end

        // The boundaries that decide signed semantics: -128 and 127.
        for (i = 0; i < EXPECTED_BOUNDS; i = i + 1) begin
            case (i)
                0: begin a8r = 8'h80; b8r = 8'h01; end
                1: begin a8r = 8'h7F; b8r = 8'h01; end
                2: begin a8r = 8'h80; b8r = 8'hFF; end
                3: begin a8r = 8'h7F; b8r = 8'h7F; end
                4: begin a8r = 8'h80; b8r = 8'h80; end
                default: begin a8r = 8'h00; b8r = 8'hFF; end
            endcase
            ai = s8f(a8r);
            bi = s8f(b8r);
            #1;
            bound_checks = bound_checks + 1;
            check_value("sum8.wrap", sum8, wrap8(ai + bi));
            check_value("shr8.arithmetic", shr8, ashr8(a8r));
            check_value("neg8.wrap", neg8, wrap8(-ai));
            check_value("cmp8.lt", lt8, (ai < bi));
            check_value("cast8.uint-lt", ult8, (a8r < b8r));
            check_value("ext8.sext", ext8, { { 8{a8r[7]} }, a8r });
            check_value("top.ext8", t_ext8, { { 8{a8r[7]} }, a8r });
            if ((ai < bi) !== (a8r < b8r))
                saw_div8 = 1'b1;
            if (ashr8(a8r) !== lshr8(a8r))
                saw_shift8 = 1'b1;
        end

        // Every nibble of the signed struct field, with the unsigned field next
        // to it and a signed value to compare against.
        for (i = 0; i < EXPECTED_STRUCT; i = i + 1) begin
            pkt = { i[3:0], 4'h9 };
            t4 = i[3:0] + 4'h5;
            si = s4(i[3:0]);
            ti = s4(t4);
            #1;
            struct_checks = struct_checks + 1;
            check_value("pair.s-field-whole", ps, i[3:0]);
            check_value("pair.u-field-whole", pu, 4'h9);
            check_value("pair.s-sext", pext, { { 4{i[3]} }, i[3:0] });
            check_value("pair.s-trunc-back", pback, i[3:0]);
            check_value("pair.s-shr-arithmetic", pshr, ashr4(i[3:0]));
            check_value("pair.s-signed-compare", plt, (si < ti));
            check_value("pair.s-part-select", plow, i[1:0]);
            check_value("pair.s-part-select-unsigned", plowlt, (i[1:0] < 2'b01));
            check_value("top.pshr", t_pshr, ashr4(i[3:0]));
            if ((i[1:0] < 2'b01) !== (s2(i[1:0]) < 1))
                saw_slice = 1'b1;
        end

        // A module with no input at all: the negative literal has to be live and
        // negative, and the shift on the local keeps its signedness.
        check_value("const.negative-literal", cv, 4'hD);
        check_value("const.negative-literal-shr", cvshr, 4'hE);
        check_value("const.unsigned-view", cu, 4'hF);
        if (s4(cv) !== cu)
            saw_const = 1'b1;

        if (!saw_div4 || !saw_div8 || !saw_shift4 || !saw_shift8 || !saw_slice || !saw_const) begin
            failures = failures + 1;
            $display("SHYHDL-AC-FAIL signed witnesses missing: div4=%b div8=%b shift4=%b shift8=%b \
                      slice=%b const=%b",
                     saw_div4, saw_div8, saw_shift4, saw_shift8, saw_slice, saw_const);
        end

        if (failures == 0 && checks == EXPECTED_COMBOS && bound_checks == EXPECTED_BOUNDS
            && struct_checks == EXPECTED_STRUCT)
            $display("SHYHDL-AC-PASS signed combos=%0d bounds=%0d struct=%0d",
                     checks, bound_checks, struct_checks);
        else
            $display("SHYHDL-AC-FAIL signed failures=%0d combos=%0d/%0d bounds=%0d/%0d struct=%0d/%0d",
                     failures, checks, EXPECTED_COMBOS, bound_checks, EXPECTED_BOUNDS,
                     struct_checks, EXPECTED_STRUCT);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL signed timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
