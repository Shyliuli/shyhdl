// Reference DUT for the testbench self-check, written by hand in the shape the
// frontend is contracted to emit for `sequential.shy.rs`. The only job of this
// file is to prove that `tb_sequential.v` passes a correct design; the
// acceptance test itself always runs the *generated* Verilog instead.

module Register_W8 (input [7:0] d, input enable, input rst_n, input clk, output reg [7:0] q);
    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) q <= 8'h00;
        else if (enable) q <= d;
    end
endmodule

module Pipe2_W8 (
    input  [7:0] d,
    input        enable,
    input        rst_n,
    input        clk,
    output reg [7:0] q,
    output [7:0] stage1_out
);
    reg [7:0] stage1;

    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            stage1 <= 8'h00;
            q <= 8'h00;
        end else if (enable) begin
            stage1 <= d;
            q <= stage1;
        end
    end

    assign stage1_out = stage1;
endmodule

module Pipe8 (
    input  [7:0] d,
    input        enable,
    input        rst_n,
    input        clk,
    output [7:0] q,
    output [7:0] stage1_out
);
    Pipe2_W8 body (.d(d), .enable(enable), .rst_n(rst_n), .clk(clk), .q(q), .stage1_out(stage1_out));
endmodule

module SeqTop (
    input  [7:0] d,
    input        enable,
    input        rst_n,
    input        clk,
    output [7:0] q,
    output [7:0] pq,
    output [7:0] pstage
);
    Register_W8 store (.d(d), .enable(enable), .rst_n(rst_n), .clk(clk), .q(q));
    Pipe8 pipe (.d(d), .enable(enable), .rst_n(rst_n), .clk(clk), .q(pq), .stage1_out(pstage));
endmodule
