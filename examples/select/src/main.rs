//! Selection and matching in one design: `if` as a value, `match` as a
//! statement, `match` as a value, guards, `or` patterns and the built-in
//! `Option`.
//!
//! Run it with `cargo run -p shyhdl-example-select`.

use shyhdl::*;

#[hardware]
enum Op {
    Idle,
    Load(U<8>),
    Pair { hi: I<4>, lo: U<4> },
}

#[hardware]
enum Level {
    Idle,
    Busy,
    Done,
}

#[hardware_instance]
struct SelectTop {
    #[In]
    sel: U<2>,
    #[In]
    a: U<8>,
    #[In]
    pair: [U<4>; 2],
    #[In]
    op: Op,
    #[In]
    level: Level,
    #[In]
    ob: Option<U<4>>,
    #[Out]
    if_value: U<8>,
    #[Out]
    stmt: U<8>,
    #[Out]
    picked: U<8>,
    #[Out]
    built: Op,
    #[Out]
    opt: U<4>,
    #[Out]
    or_out: U<4>,
    #[Out]
    route: U<4>,
}

#[hardware]
impl Hardware for SelectTop {
    fn run(&mut self) {
        self.if_value = if self.sel == 0 {
            self.a
        } else if self.sel == 1 {
            self.a + 1
        } else {
            self.a ^ 0xFF
        };

        match self.op {
            Op::Idle => {
                self.stmt = 0;
            }
            Op::Load(v) if v > 0x80 => {
                self.stmt = v;
            }
            Op::Load(v) => {
                self.stmt = v + 1;
            }
            Op::Pair { hi, lo } => {
                self.stmt = lo.zext::<8>() + (hi as U<4>).zext::<8>();
            }
            _ => {
                self.stmt = 0xFF;
            }
        }

        self.picked = match self.sel {
            0 => self.a,
            1 => {
                let bumped: U<8> = self.a + 1;
                bumped
            }
            2 => self.a ^ 0xFF,
            _ => {
                (match self.a {
                    0 => 1,
                    _ => 2,
                }) + 3
            }
        };

        self.built = match self.sel {
            0 => Op::Idle,
            1 => Op::Load(self.a),
            2 => Op::Pair {
                hi: 0,
                lo: self.a[0..4],
            },
            _ => Op::Idle,
        };

        self.opt = match self.ob {
            None => 0xF,
            Some(v) => v,
        };

        self.or_out = match self.pair {
            [x, 5] | [0, x] if x == 5 => x,
            [x, _] => x,
            _ => 0,
        };

        self.route = match self.level {
            Level::Idle | Level::Busy => 0,
            Level::Done => 1,
            _ => 2,
        };
    }
}

#[hardware_test]
fn select_smoke() {
    let mut dut = SelectTop::new();
    initial! {
        dut.sel = 1;
        dut.a = 0x12;
        dut.pair = [1, 2];
        dut.op = Op::Load(0x40);
        dut.level = Level::Busy;
        dut.ob = Some(0x5);
        delay(ns(1));
        assert_eq!(dut.if_value, 0x13);
        assert_eq!(dut.stmt, 0x41);
        assert_eq!(dut.picked, 0x13);
        assert_eq!(dut.built, Op::Load(0x12));
        assert_eq!(dut.opt, 0x5);
        assert_eq!(dut.or_out, 0x1);
        assert_eq!(dut.route, 0x0);

        dut.sel = 2;
        dut.a = 0xF0;
        dut.pair = [0, 5];
        dut.op = Op::Pair { hi: 1, lo: 2 };
        dut.level = Level::Done;
        dut.ob = None;
        delay(ns(1));
        assert_eq!(dut.if_value, 0x0F);
        assert_eq!(dut.stmt, 0x3);
        assert_eq!(dut.picked, 0x0F);
        assert_eq!(dut.built, Op::Pair { hi: 0, lo: 0 });
        assert_eq!(dut.opt, 0xF);
        assert_eq!(dut.or_out, 0x5);
        assert_eq!(dut.route, 0x1);
        pass!();
        finish!();
    }
}

fn main() -> shyhdl::Result<()> {
    let text = verilog::<SelectTop>()?;
    println!("SelectTop: {} lines of Verilog", text.lines().count());

    let mut sim = simulate::<SelectTop>("build/select")?;
    let result = sim.run(select_smoke)?;
    println!("select_smoke: {:?}", result.outcome);
    if result.outcome != Outcome::Pass {
        return Err(Error::NotImplemented {
            feature: "the select test did not pass",
        });
    }

    let dbg = Debugger::new(&result)?;
    println!("debug start on port :{}", dbg.port);
    if std::env::var_os("SHYHDL_DEBUG").is_some() {
        // Blocks while one GDB session replays this recording.
        dbg.run(result)?;
    }
    Ok(())
}
