// Hand-written Verilog testbench for `match` in value position.
//
// Every expected value is stated here by hand from the contract, in plain
// integer arithmetic and branches, so the generated design never supplies its
// own expectation:
//
//   rhs        whole right-hand side:  sel0 a+b, sel1 a|b, sel2 a&b, sel3 a^b  (8 bit)
//   inf        untyped let:            sel0 a,   sel1 b,   sel2 a&b, sel3 a|b  (4 bit)
//   sized      typed let:              sel0 a,   sel1 b,   sel2 a+b, sel3 a^b  (8 bit)
//   block_tail block arm's tail:       sel0 a&b, sel1 a|b, sel2 (a^b)^F, sel3 (a&b)^F
//   nested     two matches in one ^:   sel0 a^b, sel1 b^a, sel2 (a&b)^(a^b), sel3 (a|b)^a
//   guarded    enum guard:             sel0 None->F, sel1 Some(a), sel2 Some(b), sel3 Some(a^b),
//                                      with v>8 taking v and v<=8 taking v^F
//   picked     or-pattern:             Idle|Busy -> 0, Done -> 1
//   taken      side-effecting match:   sel0 a, sel1 b, sel2 a, sel3 b
//   side       which arm ran:          sel0 1, sel1 2, sel2 4, sel3 8
//   single     inferred let value:      sel0 a, sel1 b, sel2 a, sel3 b
//   one_shot   its arm's counter:       always 1 (an inferred let is lowered
//                                       twice, so a twice-run arm would show 2)
//   and_out    false && (...):          always 0, right operand never runs
//   or_out     true || (...):           always 1, right operand never runs
//   if_out     if a==F && b==F ...:     always a^b, the arm's branch is never taken
//   lazy       counter shared by those unselected arms: always 0
//
// and for the edge process, where the assignment is non-blocking:
//
//   seq        sel0 a, sel1 b, sel2 old seq, sel3 old seq ^ F
//   seq_snap   always the seq value from BEFORE the edge, which is what proves
//              the assignment did not take effect early
//
// The combinational sweep is every (sel, a, b) with sel 0..3, a 0..15 and b in
// {3, 9}: 4 x 16 x 2 = 128 vectors. Each vector counts as one check and compares
// all fifteen combinational outputs, so a wrong arm cannot hide behind a right
// one. `lazy` is the one output whose expectation depends on the vector: the
// arm behind the `if` is reached only when both operands are 0xF, so it stays 0
// across the sweep and is expected to be 1 for the extra vector below.
//
// One extra vector (a = 0xF, b = 0xF) then takes the `if` branch that no swept
// vector reaches, so the `match` inside an `if` expression is exercised in the
// direction where its arm does run. The edge process adds one check after reset
// and one per step for eight steps.
//
// Verilog-2005 only. Protocol: exactly one `SHYHDL-AC-PASS match_expr ...` line
// on success, otherwise `SHYHDL-AC-FAIL match_expr ...` lines.
//
// Expects the frontend-generated module: MatchExprTop.

`timescale 1ns/1ps

module tb_match_expr;
    localparam integer EXPECTED_CHECKS = 138;

    reg  [1:0] sel;
    reg  [3:0] a, b;
    reg        clk, rst_n;

    wire [7:0] rhs, sized;
    wire [3:0] inf, block_tail, nested, guarded, picked, taken, side, seq, seq_snap;
    wire [3:0] single, one_shot, lazy, if_out;
    wire       and_out, or_out;

    integer s, av, bi, step, checks, failures;
    integer e_rhs, e_inf, e_sized, e_block, e_nested, e_guarded, e_picked, e_taken, e_side;
    integer e_single;
    integer v;

    MatchExprTop dut (
        .sel(sel),
        .a(a),
        .b(b),
        .clk(clk),
        .rst_n(rst_n),
        .rhs(rhs),
        .inf(inf),
        .sized(sized),
        .block_tail(block_tail),
        .nested(nested),
        .guarded(guarded),
        .picked(picked),
        .taken(taken),
        .side(side),
        .seq(seq),
        .seq_snap(seq_snap),
        .single(single),
        .one_shot(one_shot),
        .lazy(lazy),
        .and_out(and_out),
        .or_out(or_out),
        .if_out(if_out)
    );

    task check_value;
        input [255:0] what;
        input [63:0]  got;
        input [63:0]  want;
        begin
            if (got !== want) begin
                failures = failures + 1;
                if (failures <= 12)
                    $display("SHYHDL-AC-FAIL match_expr %0s sel=%0d a=%h b=%h got=%h want=%h",
                             what, sel, a, b, got, want);
            end
        end
    endtask

    initial begin
        failures = 0;
        checks = 0;
        sel = 2'd0;
        a = 4'h0;
        b = 4'h0;
        clk = 1'b0;
        rst_n = 1'b1;

        // The combinational sweep: only this arm runs, so every output is the
        // value of the selected arm and nothing else.
        for (s = 0; s < 4; s = s + 1) begin
            sel = s[1:0];
            for (av = 0; av < 16; av = av + 1) begin
                a = av[3:0];
                for (bi = 0; bi < 2; bi = bi + 1) begin
                    if (bi == 0) b = 4'd3;
                    else b = 4'd9;
                    #1;
                    checks = checks + 1;

                    // rhs: the whole right-hand side, eight bits wide.
                    case (sel)
                        2'd0: e_rhs = a + b;
                        2'd1: e_rhs = a | b;
                        2'd2: e_rhs = a & b;
                        default: e_rhs = a ^ b;
                    endcase

                    // inf: the untyped let, four bits wide.
                    case (sel)
                        2'd0: e_inf = a;
                        2'd1: e_inf = b;
                        2'd2: e_inf = a & b;
                        default: e_inf = a | b;
                    endcase

                    // sized: the typed let, eight bits wide.
                    case (sel)
                        2'd0: e_sized = a;
                        2'd1: e_sized = b;
                        2'd2: e_sized = a + b;
                        default: e_sized = a ^ b;
                    endcase

                    // block_tail: the block arm's tail expression.
                    case (sel)
                        2'd0: e_block = a & b;
                        2'd1: e_block = a | b;
                        2'd2: e_block = (a ^ b) ^ 15;
                        default: e_block = (a & b) ^ 15;
                    endcase

                    // nested: two match values combined by one arithmetic op.
                    case (sel)
                        2'd0: e_nested = a ^ b;
                        2'd1: e_nested = b ^ a;
                        2'd2: e_nested = (a & b) ^ (a ^ b);
                        default: e_nested = (a | b) ^ a;
                    endcase

                    // guarded: the enum payload, with the guard deciding.
                    case (sel)
                        2'd0: e_guarded = 15;
                        2'd1: begin
                            v = a;
                            if (v > 8) e_guarded = v;
                            else e_guarded = v ^ 15;
                        end
                        2'd2: begin
                            v = b;
                            if (v > 8) e_guarded = v;
                            else e_guarded = v ^ 15;
                        end
                        default: begin
                            v = a ^ b;
                            if (v > 8) e_guarded = v;
                            else e_guarded = v ^ 15;
                        end
                    endcase

                    // picked: the or-pattern shares one arm between two variants.
                    if (sel == 2'd2) e_picked = 1;
                    else e_picked = 0;

                    // taken and side: the value of the side-effecting arm and
                    // the one-hot marker of the arm that produced it.
                    if (sel == 2'd0) begin
                        e_taken = a;
                        e_side = 1;
                    end else if (sel == 2'd1) begin
                        e_taken = b;
                        e_side = 2;
                    end else if (sel == 2'd2) begin
                        e_taken = a;
                        e_side = 4;
                    end else begin
                        e_taken = b;
                        e_side = 8;
                    end

                    check_value("rhs.whole-right-hand-side", rhs, e_rhs);
                    check_value("inf.untyped-let", inf, e_inf);
                    check_value("sized.typed-let", sized, e_sized);
                    check_value("block_tail.tail-expression", block_tail, e_block);
                    check_value("nested.two-matches-in-arithmetic", nested, e_nested);
                    check_value("guarded.payload-guard", guarded, e_guarded);
                    check_value("picked.or-pattern", picked, e_picked);
                    check_value("taken.arm-value", taken, e_taken);
                    check_value("side.only-the-selected-arm-ran", side, e_side);

                    // The inferred `let` produced the same value as the arms
                    // say, and its arm ran exactly once: a lowering that
                    // evaluates the initialiser twice leaves two here.
                    if (sel == 2'd0) e_single = a;
                    else if (sel == 2'd1) e_single = b;
                    else if (sel == 2'd2) e_single = a;
                    else e_single = b;
                    check_value("single.inferred-let-value", single, e_single);
                    check_value("one_shot.initialiser-ran-once", one_shot, 1);

                    // Short-circuit operands and the untaken `if` branch never
                    // run their arm, so the counter they share stays at zero.
                    check_value("and_out.short-circuit-decides", and_out, 1'b0);
                    check_value("or_out.short-circuit-decides", or_out, 1'b1);
                    check_value("if_out.taken-branch-value", if_out, a ^ b);
                    // Every swept vector misses the `if` branch, so the arm it
                    // holds never runs and the counter it shares with the two
                    // short-circuit operands stays at zero.
                    check_value("lazy.unselected-arms-did-not-run", lazy, 0);
                end
            end
        end

        // The one vector that reaches the `if` branch: both operands are 0xF, so
        // the branch holding the arm is taken and its value becomes if_out.
        // This is the positive direction of "the arm inside an `if` runs only
        // when that branch is selected".
        sel = 2'd0;
        a = 4'hF;
        b = 4'hF;
        #1;
        checks = checks + 1;
        check_value("if_out.then-branch-match-value", if_out, 4'hF);
        check_value("lazy.selected-if-arm-ran-once", lazy, 1);
        check_value("and_out.still-short-circuited", and_out, 1'b0);
        check_value("or_out.still-short-circuited", or_out, 1'b1);

        // The edge process. After reset the register is zero; each step drives
        // the inputs, takes one rising edge, and only then checks the registered
        // value and the snapshot taken at that same edge.
        rst_n = 1'b0;
        #1;
        rst_n = 1'b1;
        #1;
        checks = checks + 1;
        check_value("seq.reset-value", seq, 0);
        check_value("seq_snap.reset-value", seq_snap, 0);

        for (step = 0; step < 8; step = step + 1) begin
            case (step)
                0: begin sel = 2'd0; a = 4'hA; b = 4'h0; end
                1: begin sel = 2'd1; a = 4'h0; b = 4'h5; end
                2: begin sel = 2'd2; a = 4'h0; b = 4'h0; end
                3: begin sel = 2'd3; a = 4'h0; b = 4'h0; end
                4: begin sel = 2'd0; a = 4'h3; b = 4'h0; end
                5: begin sel = 2'd3; a = 4'h0; b = 4'h0; end
                6: begin sel = 2'd2; a = 4'h0; b = 4'h0; end
                default: begin sel = 2'd1; a = 4'h0; b = 4'h7; end
            endcase
            #1;
            // One rising edge; the non-blocking assignments land after it.
            clk = 1'b1;
            #1;
            clk = 1'b0;
            #1;
            checks = checks + 1;
            // Each expectation below is written out by hand for this step, with
            // the pre-edge value of the register spelled out in the comment.
            case (step)
                // pre-edge 0: sel0 takes a.
                0: begin
                    check_value("seq.sel0-takes-a", seq, 4'hA);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'h0);
                end
                // pre-edge A: sel1 takes b.
                1: begin
                    check_value("seq.sel1-takes-b", seq, 4'h5);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'hA);
                end
                // pre-edge 5: sel2 holds the value the register had before this
                // edge, which is what proves the arm did not read the new one.
                2: begin
                    check_value("seq.sel2-holds-old-value", seq, 4'h5);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'h5);
                end
                // pre-edge 5: sel3 transforms the old value.
                3: begin
                    check_value("seq.sel3-transforms-old-value", seq, 4'hA);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'h5);
                end
                // pre-edge A.
                4: begin
                    check_value("seq.sel0-takes-a-again", seq, 4'h3);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'hA);
                end
                // pre-edge 3.
                5: begin
                    check_value("seq.sel3-transforms-again", seq, 4'hC);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'h3);
                end
                // pre-edge C: sel2 holds it.
                6: begin
                    check_value("seq.sel2-holds-again", seq, 4'hC);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'hC);
                end
                // pre-edge C: sel1 takes b.
                default: begin
                    check_value("seq.sel1-takes-b-again", seq, 4'h7);
                    check_value("seq_snap.pre-edge-value", seq_snap, 4'hC);
                end
            endcase
        end

        if (failures == 0 && checks == EXPECTED_CHECKS)
            $display("SHYHDL-AC-PASS match_expr checks=%0d", checks);
        else
            $display("SHYHDL-AC-FAIL match_expr failures=%0d checks=%0d expected_checks=%0d",
                     failures, checks, EXPECTED_CHECKS);
        $finish;
    end

    initial begin
        #100000;
        $display("SHYHDL-AC-FAIL match_expr timeout at %0t: no verdict was printed", $time);
        $finish;
    end
endmodule
