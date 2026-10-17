// Reference DUT for the testbench self-check, written by hand to express the
// intended semantics of `overwrite.shy.rs` directly: one combinational process,
// source order, last write wins, read-after-write resolved, a run-local value
// reaching a child input, a constant-only child whose outputs do not depend on
// any event, and one output with two owners.
//
// The only job of this file is to prove that `tb_overwrite.v` passes a correct
// design; the acceptance test itself always runs the *generated* Verilog.

module Bump (input [7:0] d, output [7:0] q);
    assign q = d + 8'h01;
endmodule

module ConstOnly (output [7:0] k, output [1:0] bits, output [7:0] merged);
    // `k = 0; k = k + 1; k[0] = false;`
    assign k = 8'h00;

    // `bits[0] = false; bits[1] = true;`, neither bit seeded first.
    assign bits = 2'b10;

    // `if flag { 0xA5 } else { 0x5A }`, with `flag` a constant false, so the
    // merged value is the taken arm.
    assign merged = 8'h5A;
endmodule

module Overwrite (
    input  [7:0] a,
    input  [7:0] b,
    input  [7:0] i,
    input  [3:0] hi,
    input        clk,
    input        rst_n,
    output [7:0] z,
    output [7:0] y,
    output [7:0] snap_out,
    output [7:0] k,
    output [1:0] bits,
    output [7:0] merged,
    output [7:0] bump_out,
    output [7:0] mixed,
    output reg       seen,
    output reg [7:0] mirror,
    output reg [3:0] cross
);
    // One owner: the constant low nibble from the combinational process. The
    // other: the high nibble, a register written at the edge. They are composed
    // on one wire, high nibble first.
    reg  [3:0] high;
    wire [7:0] composed = { high, 4'h6 };

    assign mixed = composed;

    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            high <= 4'h0;
            seen <= 1'b0;
            mirror <= 8'h00;
            cross <= 4'h0;
        end else begin
            // Every assignment is non-blocking, so all three read the composed
            // value from before the edge: the previous high nibble with the
            // constant low one.
            seen <= composed[i];
            mirror <= composed;
            cross <= composed[5:2];
            high <= hi;
        end
    end

    // `let snap = self.y` reads the value written by the statement before it,
    // and that net reaches the child input.
    Bump bump (.d(a), .q(bump_out));

    assign z = a + 8'h01;
    assign y = b;
    assign snap_out = a;

    ConstOnly const_only (.k(k), .bits(bits), .merged(merged));
endmodule
