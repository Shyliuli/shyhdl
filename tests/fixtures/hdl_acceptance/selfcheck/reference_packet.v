// Reference DUT for the testbench self-check, written by hand in the shape the
// frontend is contracted to emit for `packet_make.shy.rs` and
// `packet_split.shy.rs`. The only job of this file is to prove that
// `tb_packet.v` passes a correct design; the acceptance test itself always runs
// the *generated* Verilog instead.

module PacketMake (
    input  [3:0]  op,
    input  [31:0] addr,
    input         flag,
    input  [6:0]  tag,
    output [43:0] pkt
);
    assign pkt = {op, addr, flag, tag};
endmodule

module PacketSplit (
    input  [43:0] pkt,
    output [3:0]  op,
    output [31:0] addr,
    output        flag,
    output [6:0]  tag
);
    assign op = pkt[43:40];
    assign addr = pkt[39:8];
    assign flag = pkt[7];
    assign tag = pkt[6:0];
endmodule
