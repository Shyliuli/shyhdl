// Mutated reference DUT for the testbench self-check: the same interface as
// `reference_adder.v`, but `Add_N8` computes a bitwise OR and never carries.
// The adder testbench must report FAIL and must not print a PASS marker, which
// proves the exhaustive comparison is not vacuous. See `README.md`.

module Add_N8 (input [7:0] a, input [7:0] b, output [7:0] sum, output carry);
    assign sum = a | b;
    assign carry = 1'b0;
endmodule

module Add_N1 (input a, input b, output sum, output carry);
    wire [1:0] wide = {1'b0, a} + {1'b0, b};
    assign sum = wide[0];
    assign carry = wide[1];
endmodule

module Adder8 (input [7:0] a, input [7:0] b, output [7:0] sum, output carry);
    Add_N8 add (.a(a), .b(b), .sum(sum), .carry(carry));
endmodule

module AdderTop (
    input  [7:0] a8,
    input  [7:0] b8,
    input        a1,
    input        b1,
    output [7:0] sum8,
    output       carry8,
    output [7:0] osum8,
    output       ocarry8,
    output       sum1,
    output       carry1,
    output [7:0] wsum,
    output       wcarry
);
    Add_N1 one (.a(a1), .b(b1), .sum(sum1), .carry(carry1));
    Add_N8 eight (.a(a8), .b(b8), .sum(sum8), .carry(carry8));
    Add_N8 other8 (.a(a8), .b(b8), .sum(osum8), .carry(ocarry8));
    Adder8 wrapper (.a(a8), .b(b8), .sum(wsum), .carry(wcarry));
endmodule
