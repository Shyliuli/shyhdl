//! Arithmetic and bit operations, generic in the build-time width `W`.

use shyhdl::*;

#[hardware_instance]
pub struct Datapath<const W: usize> {
    #[In]
    pub a: U<W>,
    #[In]
    pub b: U<W>,
    #[Out]
    pub sum: U<W>,
    #[Out]
    pub carry: bool,
    #[Out]
    pub shifted: U<W>,
    #[Out]
    pub masked: U<W>,
}

#[hardware]
impl<const W: usize> Hardware for Datapath<W> {
    fn run(&mut self) {
        let wide: U<{ W + 1 }> = self.a.zext::<{ W + 1 }>() + self.b.zext::<{ W + 1 }>();
        self.sum = wide[0..W];
        self.carry = wide.get(W);
        self.shifted = self.a >> 1;
        self.masked = self.a & 0xF0;
    }
}
