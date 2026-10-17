//! The top module: only wiring between the two children.

use shyhdl::*;

#[hardware_instance]
pub struct TourTop {
    #[In]
    pub a: U<8>,
    #[In]
    pub op: Op,
    #[In]
    pub level: Level,
    #[Out]
    pub sum: U<8>,
    #[Out]
    pub carry: bool,
    #[Out]
    pub y: U<8>,
    #[Out]
    pub tag: U<2>,
    #[Out]
    pub route: U<4>,
    #[instance]
    dp: Datapath<8>,
    #[instance]
    router: EnumRouter,
}

#[hardware]
impl Hardware for TourTop {
    fn run(&mut self) {
        self.dp.a = self.a;
        self.dp.b = 0x3;
        self.sum = self.dp.sum;
        self.carry = self.dp.carry;

        self.router.op = self.op;
        self.router.level = self.level;
        self.y = self.router.y;
        self.tag = self.router.tag;
        self.route = self.router.route;
    }
}

/// 0x12 + 0x3 = 0x15 without a carry; `Load(0x40)` is below the guard.
#[hardware_test]
pub fn tour_smoke() {
    let mut dut = TourTop::new();
    initial! {
        dut.a = 0x12;
        dut.op = Op::Load(0x40);
        dut.level = Level::Busy;
        delay(ns(1));
        assert_eq!(dut.sum, 0x15);
        assert!(!dut.carry);
        assert_eq!(dut.y, 0x41);
        assert_eq!(dut.tag, 1);
        assert_eq!(dut.route, 0);
        pass!();
        finish!();
    }
}
