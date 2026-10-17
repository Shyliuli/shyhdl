// Reference DUT for the testbench self-check of `match_expr.shy.rs`, written by
// hand to express the intended semantics directly.
//
// This is the opt-in diagnosis helper, never the evidence that the generated
// design is right: the acceptance test always runs the frontend's Verilog.
//
// The semantics it states, which the testbench checks independently:
//
//   rhs        sel0 a+b, sel1 a|b, sel2 a&b, sel3 a^b          (8 bit)
//   inf        sel0 a,   sel1 b,   sel2 a&b, sel3 a|b          (4 bit)
//   sized      sel0 a,   sel1 b,   sel2 a+b, sel3 a^b          (8 bit)
//   block_tail sel0 a&b, sel1 a|b, sel2 (a^b)^F, sel3 (a&b)^F
//   nested     sel0 a^b, sel1 b^a, sel2 (a&b)^(a^b), sel3 (a|b)^a
//   guarded    None->F, else the payload, with v>8 taking v and v<=8 v^F
//   picked     Idle|Busy -> 0, Done -> 1, and the tag with no variant -> F
//   taken      sel0 a, sel1 b, sel2 a, sel3 b
//   side       the one-hot marker of the arm that ran
//   single     the inferred `let` value, and one_shot its arm's counter, which
//              is one because the initialiser executes once
//   and_out    false && (...) is 0, or_out true || (...) is 1, and the counter
//              they share with the untaken `if` branch stays 0
//   seq        sel0 a, sel1 b, sel2 old seq, sel3 old seq ^ F, with seq_snap
//              always the value from before the edge

module MatchExprTop (
    input  [1:0] sel,
    input  [3:0] a,
    input  [3:0] b,
    input        clk,
    input        rst_n,
    output reg [7:0] rhs,
    output reg [3:0] inf,
    output reg [7:0] sized,
    output reg [3:0] block_tail,
    output reg [3:0] nested,
    output reg [3:0] guarded,
    output reg [3:0] picked,
    output reg [3:0] taken,
    output reg [3:0] side,
    output reg [3:0] seq,
    output reg [3:0] seq_snap,
    output reg [3:0] single,
    output reg [3:0] one_shot,
    output reg [3:0] lazy,
    output reg       and_out,
    output reg       or_out,
    output reg [3:0] if_out
);
    reg [3:0] payload;

    always @(*) begin
        case (sel)
            2'd0: rhs = { 4'h0, a } + { 4'h0, b };
            2'd1: rhs = { 4'h0, a } | { 4'h0, b };
            2'd2: rhs = { 4'h0, a } & { 4'h0, b };
            default: rhs = { 4'h0, a } ^ { 4'h0, b };
        endcase

        case (sel)
            2'd0: inf = a;
            2'd1: inf = b;
            2'd2: inf = a & b;
            default: inf = a | b;
        endcase

        case (sel)
            2'd0: sized = { 4'h0, a };
            2'd1: sized = { 4'h0, b };
            2'd2: sized = { 4'h0, a } + { 4'h0, b };
            default: sized = { 4'h0, a } ^ { 4'h0, b };
        endcase

        case (sel)
            2'd0: block_tail = a & b;
            2'd1: block_tail = a | b;
            2'd2: block_tail = (a ^ b) ^ 4'hF;
            default: block_tail = (a & b) ^ 4'hF;
        endcase

        case (sel)
            2'd0: nested = a ^ b;
            2'd1: nested = b ^ a;
            2'd2: nested = (a & b) ^ (a ^ b);
            default: nested = (a | b) ^ a;
        endcase

        // `None` for tag 0, `Some(payload)` for tag 1. The guard takes the
        // payload when it is above eight and the complemented value otherwise.
        case (sel)
            2'd0: begin
                // None
                guarded = 4'hF;
            end
            2'd1: begin
                payload = a;
                if (payload > 4'd8) guarded = payload;
                else guarded = payload ^ 4'hF;
            end
            2'd2: begin
                payload = b;
                if (payload > 4'd8) guarded = payload;
                else guarded = payload ^ 4'hF;
            end
            default: begin
                payload = a ^ b;
                if (payload > 4'd8) guarded = payload;
                else guarded = payload ^ 4'hF;
            end
        endcase

        case (sel)
            2'd0: picked = 4'h0;   // Idle
            2'd1: picked = 4'h0;   // Busy
            2'd2: picked = 4'h1;   // Done
            default: picked = 4'h0; // Busy
        endcase

        case (sel)
            2'd0: begin
                taken = a;
                side = 4'h1;
            end
            2'd1: begin
                taken = b;
                side = 4'h2;
            end
            2'd2: begin
                taken = a;
                side = 4'h4;
            end
            default: begin
                taken = b;
                side = 4'h8;
            end
        endcase

        case (sel)
            2'd0: single = a;
            2'd1: single = b;
            2'd2: single = a;
            default: single = b;
        endcase
        // The initialiser runs once, so its counter is one.
        one_shot = 4'h1;

        // The right operand of `false &&` and of `true ||` is never evaluated,
        // so those two arms never bump the shared counter. The `if` branch
        // holding the other arm is taken only when both operands are 0xF, and
        // then that arm runs once and yields `a`.
        and_out = 1'b0;
        or_out = 1'b1;
        if (a == 4'hF && b == 4'hF) begin
            lazy = 4'h1;
            if_out = a;
        end else begin
            lazy = 4'h0;
            if_out = a ^ b;
        end
    end

    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            seq <= 4'h0;
            seq_snap <= 4'h0;
        end else begin
            case (sel)
                2'd0: seq <= a;
                2'd1: seq <= b;
                2'd2: seq <= seq;
                default: seq <= seq ^ 4'hF;
            endcase
            // Non-blocking: this still reads the value from before the edge.
            seq_snap <= seq;
        end
    end
endmodule
