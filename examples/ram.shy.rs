//! A single-port RAM: a memory resource, not a packed array.
//!
//! Build it with `shyhdl build --top Ram examples/ram.shy.rs`.
//!
//! `Mem<T, N>` is its own resource and never enters a value's width, so a memory
//! cannot be copied, assigned as a whole, put in a port or used in arithmetic.
//! `read(addr)` is an expression and does not register anything; `write(addr,
//! data)` is a statement in the clock arm of `always!` and becomes a non-blocking
//! assignment. The element type is an ordinary fixed-layout value.

#[hardware_instance]
struct Ram {
    #[In] clk: bool,
    #[In] we: bool,
    #[In] addr: uint<8>,
    #[In] wdata: uint<32>,
    #[Out] rdata: uint<32>,
    mem: Mem<uint<32>, 256>,
}

impl Hardware for Ram {
    fn run(&mut self) {
        // An asynchronous read port: the assignment is combinational, the read
        // itself is not registered.
        self.rdata = self.mem.read(self.addr);

        always! {
            #[Pedge] self.clk => {
                if self.we {
                    self.mem.write(self.addr, self.wdata);
                }
            }
        }
    }
}
