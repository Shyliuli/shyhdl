//! Enum types and the statement form of `match`.

use shyhdl::*;

#[hardware]
pub enum Op {
    Idle,
    Load(U<8>),
    Pair { hi: I<4>, lo: U<4> },
}

#[hardware]
pub enum Level {
    Idle,
    Busy,
    Done,
}

#[hardware_instance]
pub struct EnumRouter {
    #[In]
    pub op: Op,
    #[In]
    pub level: Level,
    #[Out]
    pub y: U<8>,
    #[Out]
    pub tag: U<2>,
    #[Out]
    pub route: U<4>,
}

#[hardware]
impl Hardware for EnumRouter {
    fn run(&mut self) {
        match self.op {
            Op::Idle => {
                self.y = 0;
                self.tag = 0;
            }
            Op::Load(v) if v > 0x80 => {
                self.y = v;
                self.tag = 1;
            }
            Op::Load(v) => {
                self.y = v + 1;
                self.tag = 1;
            }
            Op::Pair { hi, lo } => {
                self.y = lo.zext::<8>() + (hi as U<4>).zext::<8>();
                self.tag = 2;
            }
            _ => {
                self.y = 0xFF;
                self.tag = 3;
            }
        }

        match self.level {
            Level::Idle | Level::Busy => {
                self.route = 0;
            }
            Level::Done => {
                self.route = 1;
            }
            _ => {
                self.route = 2;
            }
        }
    }
}
