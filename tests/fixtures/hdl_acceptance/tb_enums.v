// Hand-written Verilog testbench for user enums with payloads and for `match`
// over them.
//
// Every expected value is built here by concatenating the raw bits by hand, so
// the layout under test never computes its own expectation: the tag is written
// as the least significant slice, the payload above it, and the unused payload
// bits of a variant as zeroes.
//
// `Msg` has three variants, so the tag is two bits and `2'd3` has no variant.
// The match covers every real tag explicitly and ends in `_`, so the undefined
// tag takes the `_` arm; that arm is checked explicitly instead of being left to
// default behaviour.
//
// Verilog-2005 only. Protocol: exactly one `SHYHDL-AC-PASS enums ...` line on
// success, otherwise `SHYHDL-AC-FAIL enums ...` lines.
//
// Expects the frontend-generated modules: EnumMake, EnumMatch, EnumTop.
// `Msg` occupies two tag bits plus an eight-bit payload, so it is ten bits: the
// tag is the low slice and `{ hi, lo }` or the zero-extended `Data` payload is
// above it.

`timescale 1ns/1ps

module tb_enums;
    localparam integer EXPECTED_CHECKS = 49;

    reg  [3:0] v4;
    reg  [3:0] s4;
    reg  [9:0] msg10;

    wire [9:0] idle, data, pair;
    wire [3:0] y, made_y;
    wire [1:0] tag_seen, made_tag;
    wire [7:0] bound;
    wire [3:0] or_y, or_after;
    wire       or_hit;
    reg  [7:0] or_pair8;

    integer i, checks, failures;
    reg  [3:0] s_neg;

    EnumMake dutm (.v(v4), .s(s4), .idle(idle), .data(data), .pair(pair));

    EnumMatch dutx (.msg(msg10), .y(y), .tag_seen(tag_seen), .bound(bound));

    EnumTop dut_top (
        .v(v4),
        .s(s4),
        .msg(msg10),
        .idle(),
        .data(),
        .pair(),
        .y(),
        .tag_seen(),
        .bound(),
        .made_y(made_y),
        .made_tag(made_tag),
        .or_pair(or_pair8),
        .or_y(or_y),
        .or_hit(or_hit),
        .or_after(or_after)
    );

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL enums %0s v=%h s=%h msg=%h got=%h want=%h",
                             what, v4, s4, msg10, got, want);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        v4 = 4'h0;
        s4 = 4'h0;
        msg10 = 10'h000;
        or_pair8 = 8'h00;

        // Construction: the tag is the low slice and the unused payload bits of
        // a variant are zero. `Idle` is tag 0 with an all-zero payload, `Data` is
        // tag 1 with the value zero-extended above it, and `Pair` is tag 2 with
        // the signed field in the high nibble of the payload.
        for (i = 0; i < 16; i = i + 1) begin
            v4 = i[3:0];
            // A part-select cannot follow an expression, so the negative value is
            // materialised in a four-bit variable first.
            s_neg = 4'd15 - i[3:0];
            s4 = s_neg;
            #1;
            checks = checks + 1;
            check_value("make.idle-is-zero", idle, { 8'h00, 2'd0 });
            check_value("make.data-zero-extended", data, { 4'h0, v4, 2'd1 });
            check_value("make.pair-fields-msb-first", pair, { s4, v4, 2'd2 });
        end

        // The match over every representable value of the enum: each real tag,
        // both sides of the guard on `Data`, and the tag with no variant. The
        // expected `y` and `bound` follow the arm that source order selects.
        for (i = 0; i < 8; i = i + 1) begin
            case (i)
                // Idle: tag 0, payload ignored.
                0: begin msg10 = { 8'h00, 2'd0 }; end
                // Data below and at the guard boundary: the second Data arm.
                1: begin msg10 = { 4'h0, 4'h0, 2'd1 }; end
                2: begin msg10 = { 4'h0, 4'h2, 2'd1 }; end
                // Data above the guard: the guarded arm.
                3: begin msg10 = { 4'h0, 4'h3, 2'd1 }; end
                4: begin msg10 = { 4'h0, 4'hF, 2'd1 }; end
                // Pair: the named payload binds both fields.
                5: begin msg10 = { 4'h8, 4'h5, 2'd2 }; end
                6: begin msg10 = { 4'h7, 4'h9, 2'd2 }; end
                // Tag 3 has no variant: the `_` arm.
                default: begin msg10 = { 4'h6, 4'h6, 2'd3 }; end
            endcase
            #1;
            checks = checks + 1;
            case (i)
                0: begin
                    check_value("match.idle-y", y, 4'h0);
                    check_value("match.idle-tag", tag_seen, 2'd0);
                    check_value("match.idle-bound", bound, 8'h00);
                end
                1: begin
                    check_value("match.data-zero-y", y, 4'h1);
                    check_value("match.data-zero-bound", bound, 8'h00);
                end
                2: begin
                    check_value("match.data-at-guard-boundary", y, 4'h1);
                    check_value("match.data-at-guard-boundary-tag", tag_seen, 2'd1);
                end
                3: begin
                    check_value("match.data-above-guard", y, 4'h3);
                    check_value("match.data-above-guard-bound", bound, 8'h03);
                end
                4: begin
                    check_value("match.data-max-above-guard", y, 4'hF);
                    check_value("match.data-max-bound", bound, 8'h0F);
                end
                5: begin
                    check_value("match.pair-y-is-lo", y, 4'h5);
                    check_value("match.pair-bound-is-signed-hi", bound, 8'hF8);
                end
                6: begin
                    check_value("match.pair-y-is-lo-2", y, 4'h9);
                    check_value("match.pair-bound-is-positive-hi", bound, 8'h07);
                end
                default: begin
                    check_value("match.undefined-tag-takes-underscore", y, 4'hF);
                    check_value("match.undefined-tag-marker", tag_seen, 2'd3);
                    check_value("match.undefined-tag-bound", bound, 8'h00);
                end
            endcase
        end

        // The constructed value, matched through the hierarchy: `Data` only, so
        // the guard decides between its two arms.
        for (i = 0; i < 16; i = i + 1) begin
            v4 = i[3:0];
            #1;
            checks = checks + 1;
            if (i > 2)
                check_value("top.made-data-above-guard", made_y, v4);
            else
                check_value("top.made-data-at-or-below-guard", made_y, 4'h1);
            check_value("top.made-data-tag", made_tag, 2'd1);
        end

        // The general `or`-guard path: `[x, 5] | [0, x] if x == 5` over the
        // array, with `x` shadowed inside the arms and the `let` read after.
        // Element 0 is the low nibble, so 8'h50 is `[0, 5]`: the first
        // alternative matches but its guard fails and the second one is tried,
        // binding `x` to the high element, which is where `y` comes from. The
        // `after` output proves the arm did not leak that binding.
        for (i = 0; i < 3; i = i + 1) begin
            case (i)
                0: or_pair8 = 8'h50;
                1: or_pair8 = 8'h55;
                default: or_pair8 = 8'h00;
            endcase
            #1;
            // Three outputs, so this block adds nine checks to the total.
            checks = checks + 1;
            case (i)
                0: check_value("or-guard.fallback-to-second-alternative", or_y, 4'h5);
                1: check_value("or-guard.first-alternative-wins", or_y, 4'h5);
                default: check_value("or-guard.no-alternative", or_y, 4'h0);
            endcase
            checks = checks + 1;
            case (i)
                0: check_value("or-guard.hit-after-fallback", or_hit, 1'b1);
                1: check_value("or-guard.hit-from-first", or_hit, 1'b1);
                default: check_value("or-guard.no-hit", or_hit, 1'b0);
            endcase
            checks = checks + 1;
            case (i)
                // The arm's binding is local: `after` always reads `pair[0]`.
                0: check_value("or-guard.shadow-did-not-leak", or_after, 4'h0);
                1: check_value("or-guard.shadow-reads-outer-let", or_after, 4'h5);
                default: check_value("or-guard.shadow-zero", or_after, 4'h0);
            endcase
        end

        if (failures == 0 && checks == EXPECTED_CHECKS)
            $display("SHYHDL-AC-PASS enums checks=%0d", checks);
        else
            $display("SHYHDL-AC-FAIL enums failures=%0d checks=%0d expected_checks=%0d",
                     failures, checks, EXPECTED_CHECKS);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL enums timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
