// ShyHDL acceptance fixture for AC-MVP-015 (unpacking half). Compiled by
// `frontend::compile(source, name, "PacketSplit")`; never compiled by rustc.
//
// The field layout is the same as in `packet_make.shy.rs`, so the testbench can
// drive `PacketSplit.pkt` from `PacketMake.pkt` and check the round trip field
// by field.

struct Packet {
    op: uint<4>,
    addr: uint<32>,
    flag: bool,
    tag: uint<7>,
}

#[hardware_instance]
struct PacketSplit {
    #[In] pkt: Packet,
    #[Out] op: uint<4>,
    #[Out] addr: uint<32>,
    #[Out] flag: bool,
    #[Out] tag: uint<7>,
}

impl Hardware for PacketSplit {
    fn run(&mut self) {
        self.op = self.pkt.op;
        self.addr = self.pkt.addr;
        self.flag = self.pkt.flag;
        self.tag = self.pkt.tag;
    }
}
