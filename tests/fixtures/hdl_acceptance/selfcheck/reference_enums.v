// Reference DUT for the testbench self-check of `enums.shy.rs`, written by hand
// to express the intended semantics directly.
//
// Layout, applied by hand here: the tag is the low `max(1, ceil(log2 n))` bits,
// the payload sits above it and is as wide as the widest variant's, payload bits
// a variant does not use are zero, and a struct payload packs its fields most
// significant first. `Msg` has three variants, so the tag is two bits and the
// widest payload is the eight-bit `Pair`, giving a ten-bit value.
//
// The acceptance test itself always runs the *generated* Verilog; this file only
// proves that `tb_enums.v` passes a design whose semantics are right.

module EnumMake (
    input  [3:0] v,
    input  [3:0] s,
    output [9:0] idle,
    output [9:0] data,
    output [9:0] pair
);
    // Tag 0, no payload: every payload bit is zero.
    assign idle = { 8'h00, 2'd0 };
    // Tag 1, the four-bit value zero-extended to the payload width.
    assign data = { 4'h0, v, 2'd1 };
    // Tag 2, the named payload with its fields most significant first.
    assign pair = { s, v, 2'd2 };
endmodule

/// The `match`, arm by arm, in source order. The guard on the first `Data` arm
/// is checked before the arm is taken, and the final `_` catches the tag that
/// has no variant.
module EnumMatch (
    input  [9:0] msg,
    output reg [3:0] y,
    output reg [1:0] tag_seen,
    output reg [7:0] bound
);
    wire [1:0] tag = msg[1:0];
    wire [7:0] payload = msg[9:2];
    wire signed [3:0] hi = payload[7:4];
    wire [3:0] lo = payload[3:0];

    always @(*) begin
        case (tag)
            2'd0: begin
                y = 4'h0;
                tag_seen = 2'd0;
                bound = 8'h00;
            end
            2'd1: begin
                // The guard, then the unguarded arm for the same variant.
                if (lo > 4'd2) begin
                    y = lo;
                    tag_seen = 2'd1;
                    bound = { 4'h0, lo };
                end else begin
                    y = 4'h1;
                    tag_seen = 2'd1;
                    bound = { 4'h0, lo };
                end
            end
            2'd2: begin
                y = lo;
                tag_seen = 2'd2;
                // The signed field, sign-extended by the binding.
                bound = { { 4{hi[3]} }, hi };
            end
            // The tag with no variant.
            default: begin
                y = 4'hF;
                tag_seen = 2'd3;
                bound = 8'h00;
            end
        endcase
    end
endmodule

/// The general `or` pattern with a guard, over a two-element array.
///
/// `[x, 5] | [0, x] if x == 5`: the alternatives are tried in order and each
/// binds `x` for its own guard. A first alternative that matches structurally
/// but whose guard fails therefore gives way to the second one rather than
/// falling straight through to `_`. `after` reads the `let` above the match, so
/// it always reports `pair[0]` and shows the arm's binding did not leak.
module OrMatch (
    input  [7:0] pair,
    output reg [3:0] y,
    output reg       hit,
    output reg [3:0] after
);
    wire [3:0] first = pair[3:0];
    wire [3:0] second = pair[7:4];

    always @(*) begin
        y = 4'h0;
        hit = 1'b0;
        // `[x, 5]`: the second element is five, binding x to the first. Its
        // guard is `x == 5`, so it only holds when the first element is five.
        if (second == 4'd5) begin
            if (first == 4'd5) begin
                y = first;
                hit = 1'b1;
            end else if (first == 4'd0) begin
                // The first alternative matched but its guard failed, so the
                // second alternative `[0, x]` is tried and binds x to the
                // second element; its guard is then satisfied.
                y = second;
                hit = 1'b1;
            end
        end
        // Nothing else can match: when the second element is not five the first
        // alternative cannot match, and the second one binds x to that same
        // element, so its guard cannot hold either.
        after = first;
    end
endmodule

module EnumTop (
    input  [3:0] v,
    input  [3:0] s,
    input  [9:0] msg,
    input  [7:0] or_pair,
    output [9:0] idle,
    output [9:0] data,
    output [9:0] pair,
    output [3:0] y,
    output [1:0] tag_seen,
    output [7:0] bound,
    output [3:0] made_y,
    output [1:0] made_tag,
    output [3:0] or_y,
    output       or_hit,
    output [3:0] or_after
);
    wire [9:0] made_data;

    EnumMake make (.v(v), .s(s), .idle(idle), .data(data), .pair(pair));
    EnumMatch matched (.msg(msg), .y(y), .tag_seen(tag_seen), .bound(bound));
    EnumMatch matched_made (.msg(made_data), .y(made_y), .tag_seen(made_tag), .bound());
    OrMatch ormatch (.pair(or_pair), .y(or_y), .hit(or_hit), .after(or_after));

    assign made_data = data;
endmodule
