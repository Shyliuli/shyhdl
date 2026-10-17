// Hand-written Verilog testbench for enums wrapped in a struct or an array, and
// for equality that ignores the payload it must ignore.
//
// Layout, written out by hand so the design never computes its own expectation:
//
//   Msg                       = { 8-bit payload, 2-bit tag }        = 10 bits
//     Idle = tag 0, Data = tag 1 (value zero-extended), Pair = tag 2
//   Envelope { msg, extra }   = { 10-bit msg, 2-bit extra }         = 12 bits
//   [Msg; 2]                  = { msg1, msg0 }                      = 20 bits
//
// The tag is always the lowest slice of its own value, a struct packs its fields
// most significant first, and an array puts element 0 lowest.
//
// The equality cases drive raw bits, so the bits that must not matter can be set
// to junk: a `Data` compared against a `Data` with the same value but different
// junk in the payload's unused high half must still be equal, and a value whose
// tag has no variant must compare on its tag alone.
//
// Verilog-2005 only. Protocol: exactly one `SHYHDL-AC-PASS wrapped ...` line on
// success, otherwise `SHYHDL-AC-FAIL wrapped ...` lines.
//
// Expects the frontend-generated modules: WrapMake, WrapEq, WrapTop.

`timescale 1ns/1ps

module tb_wrapped;
    localparam integer EXPECTED_CHECKS = 27;

    reg  [3:0]  v4;
    reg  [3:0]  s4;
    reg  [11:0] a12, b12;
    reg  [9:0]  ea10, eb10;
    reg  [19:0] aa20, ab20;

    wire [11:0] env_idle, env_pair;
    wire [19:0] arr;
    wire [9:0]  arr0, arr1, env_field;
    wire [1:0]  env_extra;
    wire        env_same, env_differ, msg_same, msg_differ, arr_same, arr_differ;

    integer i, checks, failures;

    WrapMake dutm (
        .v(v4),
        .s(s4),
        .env_idle(env_idle),
        .env_pair(env_pair),
        .arr(arr),
        .arr0(arr0),
        .arr1(arr1),
        .env_field(env_field),
        .env_extra(env_extra)
    );

    WrapEq dute (
        .a(a12),
        .b(b12),
        .ea(ea10),
        .eb(eb10),
        .aa(aa20),
        .ab(ab20),
        .env_same(env_same),
        .env_differ(env_differ),
        .msg_same(msg_same),
        .msg_differ(msg_differ),
        .arr_same(arr_same),
        .arr_differ(arr_differ)
    );

    WrapTop dut_top (
        .v(v4),
        .s(s4),
        .a(a12),
        .b(b12),
        .ea(ea10),
        .eb(eb10),
        .aa(aa20),
        .ab(ab20),
        .env_idle(),
        .env_pair(),
        .arr(),
        .arr0(),
        .arr1(),
        .env_field(),
        .env_extra(),
        .env_same(),
        .env_differ(),
        .msg_same(),
        .msg_differ(),
        .arr_same(),
        .arr_differ()
    );

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL wrapped %0s v=%h s=%h got=%h want=%h",
                             what, v4, s4, got, want);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        v4 = 4'h0;
        s4 = 4'h0;
        a12 = 12'h000;
        b12 = 12'h000;
        ea10 = 10'h000;
        eb10 = 10'h000;
        aa20 = 20'h00000;
        ab20 = 20'h00000;

        // The enum inside a struct: the tag is the low slice of the ten-bit
        // field, the payload above it, and the struct's own field is above that.
        for (i = 0; i < 16; i = i + 1) begin
            v4 = i[3:0];
            s4 = (4'd15 - i[3:0]);
            #1;
            checks = checks + 1;
            check_value("wrap.struct-idle", env_idle, { 8'h00, 2'd0, 2'h1 });
            check_value("wrap.struct-pair", env_pair, { s4, v4, 2'd2, 2'h2 });
            // An array of the same enum: element 0 is the low ten bits.
            check_value("wrap.array-element0", arr[9:0], { 4'h0, v4, 2'd1 });
            check_value("wrap.array-element1", arr[19:10], { 8'h00, 2'd0 });
            check_value("wrap.array-read-0", arr0, { 4'h0, v4, 2'd1 });
            check_value("wrap.array-read-1", arr1, { 8'h00, 2'd0 });
            // A field of a struct field, read back out.
            check_value("wrap.struct-field-read", env_field, { s4, v4, 2'd2 });
            check_value("wrap.struct-other-field", env_extra, 2'h2);
        end

        // Equality over a struct holding an enum: the same value is equal, and a
        // different tag or payload is not.
        a12 = { 4'h0, 4'h3, 2'd1, 2'h1 };
        b12 = { 4'h0, 4'h3, 2'd1, 2'h1 };
        #1;
        checks = checks + 1;
        check_value("eq.struct-same", env_same, 1'b1);
        b12 = { 4'h0, 4'h4, 2'd1, 2'h1 };
        #1;
        checks = checks + 1;
        check_value("eq.struct-different-payload", env_differ, 1'b1);
        // Only the `extra` field differs, which is still part of the struct.
        b12 = { 4'h0, 4'h3, 2'd1, 2'h3 };
        #1;
        checks = checks + 1;
        check_value("eq.struct-different-extra", env_differ, 1'b1);

        // `Data` carries its value in the LOW nibble of the wide payload, so the
        // four bits above it are the ones a `Data` does not use: junk there must
        // not change the verdict.
        ea10 = { 8'hF5, 2'd1 };
        eb10 = { 8'h05, 2'd1 };
        #1;
        checks = checks + 1;
        check_value("eq.inactive-payload-bits-ignored", msg_same, 1'b1);
        // The active nibble really is compared.
        eb10 = { 8'hF6, 2'd1 };
        #1;
        checks = checks + 1;
        check_value("eq.active-payload-bit-differs", msg_differ, 1'b1);
        // And the tag still has to agree.
        eb10 = { 8'hF5, 2'd2 };
        #1;
        checks = checks + 1;
        check_value("eq.different-tag-differs", msg_differ, 1'b1);

        // A tag with no variant: only the tag takes part, so junk in the payload
        // must not change the verdict either way.
        ea10 = { 8'hA5, 2'd3 };
        eb10 = { 8'h5A, 2'd3 };
        #1;
        checks = checks + 1;
        check_value("eq.invalid-tag-compares-tag-only", msg_same, 1'b1);
        eb10 = { 8'hA5, 2'd0 };
        #1;
        checks = checks + 1;
        check_value("eq.invalid-tag-vs-idle-differs", msg_differ, 1'b1);

        // An array of enums: element 0 lower, and any difference anywhere in the
        // array makes the two arrays differ.
        aa20 = { 10'h000, 4'h0, 4'h7, 2'd1 };
        ab20 = aa20;
        #1;
        checks = checks + 1;
        check_value("eq.array-same", arr_same, 1'b1);
        // Element 1 differs only by junk in the payload bits its `Idle` tag does
        // not use, so the arrays are still equal.
        ab20 = { 8'hFF, 2'd0, 4'h0, 4'h7, 2'd1 };
        #1;
        checks = checks + 1;
        check_value("eq.array-inactive-element-payload-ignored", arr_same, 1'b1);
        // Element 0 really differs.
        ab20 = { 10'h000, 4'h0, 4'h8, 2'd1 };
        #1;
        checks = checks + 1;
        check_value("eq.array-different-element0", arr_differ, 1'b1);

        if (failures == 0 && checks == EXPECTED_CHECKS)
            $display("SHYHDL-AC-PASS wrapped checks=%0d", checks);
        else
            $display("SHYHDL-AC-FAIL wrapped failures=%0d checks=%0d expected_checks=%0d",
                     failures, checks, EXPECTED_CHECKS);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL wrapped timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
