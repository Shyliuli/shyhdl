// ShyHDL source (Rust-style DSL). Compiled by `frontend::compile`, not by rustc.
//
// A plain data struct is a fixed-size value with no padding: fields are packed
// most significant first, in declaration order (REQ-LANG-003). `Packet` here is
// 4 + 32 + 1 + 7 = 44 bits. Used as a port it expands to one 44-bit vector.

struct Packet {
    op: uint<4>,
    addr: uint<32>,
    flag: bool,
    tag: uint<7>,
}

// Pack four inputs into one struct value.
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

// Unpack a struct value into its fields.
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

// Field-level rewrite inside the default combinational logic: the whole value
// is assigned first, then one field is overwritten on one path. Overwriting
// inside a single process is legal and last-write-wins (REQ-LANG-019); every
// bit is still assigned on every path, so no latch is inferred (REQ-LANG-022).
#[hardware_instance]
struct PacketTagReplace {
    #[In] pkt: Packet,
    #[In] new_tag: uint<7>,
    #[In] replace: bool,
    #[Out] out: Packet,
}

impl Hardware for PacketTagReplace {
    fn run(&mut self) {
        // Statements written directly in `run` are the default combinational
        // logic: they are one process, so the later field write wins.
        self.out = self.pkt;
        if self.replace {
            self.out.tag = self.new_tag;
        }
    }
}

// Default combinational logic that muxes whole values, one branch per path.
#[hardware_instance]
struct PacketSelect {
    #[In] a: Packet,
    #[In] b: Packet,
    #[In] sel: bool,
    #[Out] out: Packet,
}

impl Hardware for PacketSelect {
    fn run(&mut self) {
        if self.sel {
            self.out = self.a;
        } else {
            self.out = self.b;
        }
    }
}
