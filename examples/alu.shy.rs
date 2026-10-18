//! 32 位 ALU：按 `OP<1:0>` 选 AND / OR / ADD / SUB，并给出 N、V、C、Z。
//!
//! 构建：`shyhdl build examples/alu.shy.rs --top Alu32`
//! 测试：`shyhdl test examples/alu.shy.rs --top Alu32 --test selects_operation`
//!
//! 操作码不建 enum：`OP` 在硬件上就是两根线，图上每一级 mux 的 `S` 端接的正是 `OP<0>` /
//! `OP<1>`。`match self.op { 0b00 => ..., }` 会被前端直接降成一条 `case (op)`，四个分支就是
//! 图上那个选择器，没有编解码、临时量或一次多余比较。enum 留给带载荷的变体（例如指令），
//! 那种场合标签本身是要进寄存器的。
//!
//! 进位与溢出都从 33 位中间值上读，和图上加法器的 `c`/`v` 输出位置一致：
//! 加法 `a + b`、减法 `a + ~b + 1` 各算一次 33 位数，最高位是进位，结果再截回 32 位。

#[hardware_instance]
struct Alu32 {
    #[In] a: uint<32>,
    #[In] b: uint<32>,
    #[In] op: uint<2>,
    #[Out] y: uint<32>,
    #[Out] z: bool,
    #[Out] n: bool,
    #[Out] c: bool,
    #[Out] v: bool,
}

impl Hardware for Alu32 {
    fn run(&mut self) {
        // 加法和减法只差在第二个操作数取反、再加 1：两者都算成 33 位，
        // 第 33 位就是进位（减法时是"没有借位"）。
        let sum: uint<33> = self.a.zext::<33>() + self.b.zext::<33>();
        let not_b: uint<32> = !self.b;
        let diff: uint<33> = self.a.zext::<33>() + not_b.zext::<33>() + 1;

        // 图上的两级 mux：一条 case，四个分支。
        self.y = match self.op {
            0b00 => self.a & self.b,
            0b01 => self.a | self.b,
            0b10 => sum.trunc::<32>(),
            0b11 => diff.trunc::<32>(),
        };

        // C：加法取进位；减法按 FPGA 惯例取"无借位"（a >= b 时为 1）。
        self.c = match self.op {
            0b10 => sum.get(32),
            0b11 => diff.get(32),
            _ => false,
        };

        // V：只有加减会溢出，判据是操作数同号、结果与之异号。
        self.v = match self.op {
            0b10 => (self.a.get(31) == self.b.get(31)) && (self.y.get(31) != self.a.get(31)),
            0b11 => (self.a.get(31) != self.b.get(31)) && (self.y.get(31) != self.a.get(31)),
            _ => false,
        };

        // N 取结果的符号位，Z 是 32 位全零；与上面的 mux 共用同一个 y。
        self.n = self.y.get(31);
        self.z = self.y == 0;
    }
}

/// 四个操作码轮流选一遍，值都取小整数，先确认选择器和算术本身。
#[hardware_test]
fn selects_operation() {
    let mut dut = Alu32::new();
    initial! {
        dut.a = 0x0000_000F;
        dut.b = 0x0000_00F0;

        dut.op = 0b00;
        delay(ns(1));
        assert_eq!(dut.y, 0x0000_0000);
        assert_eq!(dut.z, true);

        dut.op = 0b01;
        delay(ns(1));
        assert_eq!(dut.y, 0x0000_00FF);
        assert_eq!(dut.z, false);

        dut.op = 0b10;
        delay(ns(1));
        assert_eq!(dut.y, 0x0000_00FF);
        assert_eq!(dut.c, false);

        dut.op = 0b11;
        delay(ns(1));
        assert_eq!(dut.y, 0xFFFF_FF1F);
        assert_eq!(dut.n, true);

        pass!();
        finish!();
    }
}

/// 进位、借位与有符号溢出的边界：0x7FFFFFFF+1 进到符号位、-1-1 借位、0x80000000-1 溢出。
#[hardware_test]
fn carries_and_flags() {
    let mut dut = Alu32::new();
    initial! {
        dut.a = 0x7FFF_FFFF;
        dut.b = 0x0000_0001;
        dut.op = 0b10;
        delay(ns(1));
        assert_eq!(dut.y, 0x8000_0000);
        assert_eq!(dut.v, true);
        assert_eq!(dut.c, false);
        assert_eq!(dut.n, true);
        assert_eq!(dut.z, false);

        dut.a = 0xFFFF_FFFF;
        dut.b = 0x0000_0001;
        dut.op = 0b11;
        delay(ns(1));
        assert_eq!(dut.y, 0xFFFF_FFFE);
        assert_eq!(dut.v, false);
        // 减法把进位定义成"没有借位"：0xFFFFFFFF >= 1，所以 C = 1。
        assert_eq!(dut.c, true);

        dut.a = 0x8000_0000;
        dut.b = 0x0000_0001;
        dut.op = 0b11;
        delay(ns(1));
        assert_eq!(dut.y, 0x7FFF_FFFF);
        assert_eq!(dut.v, true);

        dut.a = 0xFFFF_FFFF;
        dut.b = 0x0000_0001;
        dut.op = 0b10;
        delay(ns(1));
        assert_eq!(dut.y, 0x0000_0000);
        assert_eq!(dut.c, true);
        assert_eq!(dut.z, true);

        pass!();
        finish!();
    }
}
