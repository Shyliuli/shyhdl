// ShyHDL acceptance fixture for AC-MVP-015 (packing half). Compiled by
// `frontend::compile(source, name, "PacketMake")`; never compiled by rustc.
//
// `Packet` is 4 + 32 + 1 + 7 = 44 bits with no padding, fields packed most
// significant first in declaration order (REQ-LANG-003). `tb_packet.v` wires
// the 44-bit output of this module straight into `PacketSplit` and checks every
// field against the bit-level layout `{ op, addr, flag, tag }`.

struct Packet {
    op: uint<4>,
    addr: uint<32>,
    flag: bool,
    tag: uint<7>,
}

#[hardware_instance]
struct PacketMake {
    #[In] op: uint<4>,
    #[In] addr: uint<32>,
    #[In] flag: bool,
    #[In] tag: uint<7>,
    #[Out] pkt: Packet,
}

impl Hardware for PacketMake {
    fn run(&mut self) {
        self.pkt = Packet { op: self.op, addr: self.addr, flag: self.flag, tag: self.tag };
    }
}
