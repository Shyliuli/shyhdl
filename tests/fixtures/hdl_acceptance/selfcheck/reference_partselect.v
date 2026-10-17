// Reference DUT for the testbench self-check, written by hand to express the
// intended semantics of `partselect.shy.rs` directly: two disjoint four-bit
// writes, so `y` is `{ a, a }`. The only job of this file is to prove that
// `tb_partselect.v` passes a correct design; the acceptance test itself always
// runs the *generated* Verilog.

module PartSelect (input [3:0] a, output [7:0] y);
    assign y = {a, a};
endmodule
