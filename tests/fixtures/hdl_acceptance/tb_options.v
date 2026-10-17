// Hand-written Verilog testbench for `Option<T>` and `Result<T, E>`.
//
// Layout, written out by hand so the design never computes its own expectation:
//
//   Option<uint<4>>                      = { 4-bit payload, 1-bit tag }
//     None = tag 0, Some = tag 1
//   Result<uint<4>, int<8>>              = { 8-bit payload, 1-bit tag }
//     Ok   = tag 0, Err  = tag 1
//   Option<Result<uint<4>, int<8>>>      = { inner tag + 8-bit payload, outer tag }
//     None = outer 0, Some = outer 1 wrapping an inner Result
//
// The tag is the lowest slice; the payload above it is as wide as the widest
// case, so `Ok`'s four-bit value sits in the low half of the eight-bit payload
// and its unused high half is zero.
//
// Equality is driven from raw bits here, which is what lets the inactive payload
// bits be set to junk: a `Some` compared against a `Some` with a different
// payload must differ, while the junk in the bits an inactive variant does not
// use must not affect the result.
//
// Verilog-2005 only. Protocol: exactly one `SHYHDL-AC-PASS options ...` line on
// success, otherwise `SHYHDL-AC-FAIL options ...` lines.
//
// Expects the frontend-generated modules: OptMake, OptMatch, NestedMatch,
// DeepNest, EnumEq, OptTop.

`timescale 1ns/1ps

module tb_options;
    localparam integer EXPECTED_CHECKS = 32;

    reg  [3:0] v4;
    reg  [7:0] s8;
    reg        ob_tag, ob_pay;
    reg  [4:0] a5, b5;
    reg  [8:0] ra9, rb9;
    reg  [9:0] na10, nb10;

    wire [4:0] none5, some5;
    wire [8:0] ok9, err9;
    wire [9:0] deep_ok10, deep_err10;
    wire [3:0] n_y, ok_y, err_y;
    wire [7:0] ok_e, err_e;
    wire       same, differ, rsame, rdiffer, nsame, ndiffer;

    integer i, checks, failures;

    OptMake dutm (
        .v(v4),
        .s(s8),
        .none(none5),
        .some(some5),
        .ok(ok9),
        .err(err9),
        .deep_ok(deep_ok10),
        .deep_err(deep_err10)
    );

    NestedMatch dutn (.ob({ ob_pay, ob_tag }), .y(n_y));

    EnumEq dute (
        .a(a5),
        .b(b5),
        .ra(ra9),
        .rb(rb9),
        .na(na10),
        .nb(nb10),
        .same(same),
        .differ(differ),
        .rsame(rsame),
        .rdiffer(rdiffer),
        .nsame(nsame),
        .ndiffer(ndiffer)
    );

    OptTop dut_top (
        .v(v4),
        .s(s8),
        .ob({ ob_pay, ob_tag }),
        .a(a5),
        .b(b5),
        .ra(ra9),
        .rb(rb9),
        .na(na10),
        .nb(nb10),
        .none(),
        .some(),
        .ok(),
        .err(),
        .deep_ok(),
        .deep_err(),
        .n_y(),
        .same(),
        .differ(),
        .rsame(),
        .rdiffer(),
        .nsame(),
        .ndiffer(),
        .ok_y(),
        .ok_e(),
        .err_y(),
        .err_e()
    );

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL options %0s v=%h s=%h got=%h want=%h",
                             what, v4, s8, got, want);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        v4 = 4'h0;
        s8 = 8'h00;
        ob_tag = 1'b0;
        ob_pay = 1'b0;
        a5 = 5'h00;
        b5 = 5'h00;
        ra9 = 9'h000;
        rb9 = 9'h000;
        na10 = 10'h000;
        nb10 = 10'h000;

        // Construction: the tag is the low slice, the payload above it, and the
        // unused high half of `Ok`'s payload is zero.
        for (i = 0; i < 16; i = i + 1) begin
            v4 = i[3:0];
            s8 = 8'h80 | { 4'h0, i[3:0] };
            #1;
            checks = checks + 1;
            check_value("make.none-is-zero", none5, { 4'h0, 1'b0 });
            check_value("make.some-zero-extended", some5, { v4, 1'b1 });
            check_value("make.ok-payload-low-half", ok9, { 4'h0, v4, 1'b0 });
            check_value("make.err-signed-payload", err9, { s8, 1'b1 });
        end

        // The nested case with a complete pattern and no `_`: all three arms are
        // reachable, including both payload values of the inner type.
        for (i = 0; i < 4; i = i + 1) begin
            case (i)
                0: begin ob_tag = 1'b0; ob_pay = 1'b0; end
                1: begin ob_tag = 1'b1; ob_pay = 1'b0; end
                2: begin ob_tag = 1'b1; ob_pay = 1'b1; end
                // `Option<bool>` has no second payload value, so this case only
                // repeats index 2 with the tag bit set.
                default: begin ob_tag = 1'b1; ob_pay = 1'b1; end
            endcase
            #1;
            checks = checks + 1;
            case (i)
                0: check_value("nested.none-arm", n_y, 4'h0);
                1: check_value("nested.some-false-arm", n_y, 4'h1);
                default: check_value("nested.some-true-arm", n_y, 4'h2);
            endcase
        end

        // The deep construction: `Some(Ok(v))` and `Some(Err(s))`, destructured
        // two levels deep.
        for (i = 0; i < 4; i = i + 1) begin
            v4 = i[3:0];
            s8 = 8'hFC | { 4'h0, i[1:0] };
            #1;
            checks = checks + 1;
            check_value("deep.some-ok-tag", deep_ok10[0], 1'b1);
            check_value("deep.some-ok-inner-tag", deep_ok10[1], 1'b0);
            check_value("deep.some-err-inner-tag", deep_err10[1], 1'b1);
            check_value("deep.some-err-payload", deep_err10[9:2], s8);
        end

        // Equality over an `Option`: equal payloads agree, different payloads
        // differ, and `None` differs from `Some`. The junk in the inactive bits
        // of `None` must not make it equal to a `Some`.
        a5 = { 4'h5, 1'b1 };
        b5 = { 4'h5, 1'b1 };
        #1;
        checks = checks + 1;
        check_value("eq.some-equal", same, 1'b1);
        b5 = { 4'h6, 1'b1 };
        #1;
        checks = checks + 1;
        check_value("eq.some-different-payload", differ, 1'b1);
        // `None` with junk in the payload bits: the inactive bits must be
        // ignored, so this still differs from the `Some` above.
        a5 = { 4'hF, 1'b0 };
        b5 = { 4'h5, 1'b1 };
        #1;
        checks = checks + 1;
        check_value("eq.none-ignores-its-payload", differ, 1'b1);
        // Two `None`s with different junk payloads are still equal.
        b5 = { 4'h0, 1'b0 };
        #1;
        checks = checks + 1;
        check_value("eq.two-none-are-equal-despite-junk", same, 1'b1);

        // The same over a `Result`: an `Ok` and an `Err` differ even when the
        // shared payload bits would compare equal.
        ra9 = { 8'h5A, 1'b0 };
        rb9 = { 8'h5A, 1'b1 };
        #1;
        checks = checks + 1;
        check_value("eq.ok-and-err-differ", rdiffer, 1'b1);
        rb9 = { 8'h5A, 1'b0 };
        #1;
        checks = checks + 1;
        check_value("eq.two-ok-equal", rsame, 1'b1);

        // And over the nested type: `Some(Ok(x))` against `Some(Err(y))`.
        na10 = { 8'h00, 1'b0, 1'b1 };
        nb10 = { 8'hFF, 1'b1, 1'b1 };
        #1;
        checks = checks + 1;
        check_value("eq.nested-ok-vs-err-differ", ndiffer, 1'b1);
        nb10 = { 8'h00, 1'b0, 1'b1 };
        #1;
        checks = checks + 1;
        check_value("eq.nested-equal", nsame, 1'b1);

        if (failures == 0 && checks == EXPECTED_CHECKS)
            $display("SHYHDL-AC-PASS options checks=%0d", checks);
        else
            $display("SHYHDL-AC-FAIL options failures=%0d checks=%0d expected_checks=%0d",
                     failures, checks, EXPECTED_CHECKS);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL options timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
