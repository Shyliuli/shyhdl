# ShyHDL

用 Rust 风格的源码描述硬件，生成可读的 Verilog-2005。语言可以写在单文件 `.shy.rs` 里，也可以
直接写进普通 Rust 程序：宏登记设计，库在运行时把它编译成 Verilog、导出 testbench、驱动仿真，
并把一次运行录成可回放的记录。

> 状态：0.1.0。语言子集已经可用并有端到端测试覆盖；它不是把任意 Rust 编译成硬件的工具，
> 边界见[已知限制](#已知限制)。

本项目是一个小玩具，由LLM生成，不建议直接用于生产。
## 特性

- **可读的 RTL**：生成的 Verilog 保留模块边界、实例名与源码位置注释；纯位模式的 `match`
  生成一条 `case`/`casez`，赋值与 `let` 直接写目标，不堆临时寄存器。
- **显式的硬件语义**：位宽、扩展、截断、符号性都写在源码里；组合过程全路径赋值、单驱动、
  异步复位优先级都在构建期检查。
- **独立 testbench**：`#[hardware_test]` 变成标准 Verilog-2005 测试平台，不依赖 Rust DPI。
- **真实仿真**：用 Verilator 驱动，记录 VCD 或 FST，并写出可复现的 run manifest。
- **只读回放**：一次运行可以交给真实 GDB，`step`、`reverse-step`、`watch`、条件 `break` 与
  `print` 都作用在记录上，不回跑仿真。
- **两种写法**：单文件与宏模式共用同一条流水线；`mod name;` 支持跨文件模块。

## 安装

```toml
[dependencies]
shyhdl = "0.1"
```

运行时只需要 Verilator；GDB 只在回放记录时需要。两者都通过 `PATH` 查找，也可以分别用
`SHYHDL_VERILATOR`、`SHYHDL_TIMEOUT` 覆盖。

在仓库里构建：

```console
$ cargo build --release
```

## 快速开始

```rust
use shyhdl::*;

#[hardware_instance]
struct Adder4 {
    #[In] a: U<4>,
    #[In] b: U<4>,
    #[Out] sum: U<4>,
    #[Out] carry: bool,
}

#[hardware]
impl Hardware for Adder4 {
    fn run(&mut self) {
        let wide: U<5> = self.a.zext::<5>() + self.b.zext::<5>();
        self.sum = wide.trunc::<4>();
        self.carry = wide.get(4);
    }
}

fn main() -> shyhdl::Result<()> {
    check::<Adder4>()?;                    // 编译并报告诊断
    println!("{}", verilog::<Adder4>()?);  // 整份设计的 Verilog
    Ok(())
}
```

测试也是源码的一部分：

```rust
#[hardware_test]
fn wraps() {
    let mut dut = Adder4::new();
    initial! {
        dut.a = 15;
        dut.b = 1;
        delay(ns(1));
        assert_eq!(dut.sum, 0);
        assert_eq!(dut.carry, true);
        pass!();
        finish!();
    }
}
```

`U<W>` / `I<W>` 是库内标记类型，前端把它们当作 `uint<W>` / `int<W>`，不参与 Rust 的真实算术。
宏只登记源码文本并生成占位定义，因此这些文件正常参与 `cargo build`。

## 语言概览

| 类别 | 支持内容 |
| --- | --- |
| 模块 | `#[hardware_instance] struct`、`#[instance]` 子实例、赋值连线、构建期常量泛型 |
| 数据类型 | `bool`、`U<N>` / `uint<N>`、`I<N>` / `int<N>`、`struct`、带 payload 的 `enum`、`Option`、`Result`、`[T; N]` |
| 组合过程 | `run` 中的 `let`、赋值、`if`、`match`、构建期 `for`，按源码顺序求值 |
| 时序过程 | 事件臂 `#[Pedge]` / `#[Nedge]`，最后一个臂是时钟，状态赋值非阻塞 |
| 运算 | 等宽算术与回绕、位运算、`>>`（有符号算术右移）、切片 `x[lo..hi]`、`get(i)`、`zext` / `sext` / `trunc`、等宽 `as` |
| 匹配 | 语句位置与值位置的 `match`、变体与 payload 模式、`or` 模式、守卫、穷尽性检查、范围 `0x10..=0x1F`、位通配 `mask!(0b11??1111)` 与 `mask!("11??1111")`；纯位模式生成 `case`/`casez` |
| 测试 | `gen_clk` / `clock!` / `reset!` / `initial!` / `always!` / `delay` / `wait` / `wait_until` / `sample_after` / `assert!` / `assert_eq!` / `pass!` / `finish!` |

完整规则（字段布局、复位优先级、位宽与符号性、模式匹配与 `case`/`casez` 的取舍）见
[docs/language.md](./docs/language.md)。

## 示例

工作区里有三个可直接运行的项目：

```console
$ cargo run -p shyhdl-example-adder    # 最小完整示例
$ cargo run -p shyhdl-example-select   # if / match 值、守卫、or 模式、Option
$ cargo run -p shyhdl-example-tour     # Rust 模块拆分：datapath / select / top
```

单文件示例在 `examples/*.shy.rs`：`adder`、`signed`、`arrays`、`pipeline`、`register`、`packet`。

## 文档

- [docs/language.md](./docs/language.md)：语言参考（模块、过程、位宽、匹配、测试）
- [docs/library.md](./docs/library.md)：库入口、类型、导出、仿真与回放
- `cargo doc --open`：按模块组织的 API 文档

## 已知限制

- 语言是有限的、可静态展开的硬件描述子集：没有动态堆、递归、trait 对象与运行期循环。
- 赋值与运算按硬件语义解释，不等同于 Rust 求值模型；`#[hardware] impl` 的函数体只被登记，
  不参与 Rust 编译。
- 数组是固定布局的值类型，不会被推断成 RAM。
- 模式匹配的规模上限：嵌套 16 层、单个臂最多 64 个备选、覆盖检查最多 4096 行、256 个并行
  列、65536 个单元，以及总计 100 万次访问。
- Verilator 后端是二态语义，不声称完整四态仿真。
- Verilator 的 verilation kit 路径必须是 ASCII；ShyHDL 自身的源码与生成物支持中文路径。
- 宏模式把程序中登记的所有条目拼成一份源码编译，多个设计若声明同名类型会冲突。
- `Debugger::new` 需要 `&RunResult`，因为端口与录制配对来自该次运行的 manifest。

## 项目结构

```text
src/                库入口、编译驱动、仿真、回放与调试
  frontend/         源码 → Verilog + 元数据 + 诊断
  testbench/        #[hardware_test] → 独立 Verilog-2005 testbench
  simulation/       Verilator 后端与 run manifest
  waveform.rs       VCD / FST 读取
  replay.rs         观测点游标、步进与观察点
  debugger/         GDB RSP 服务端与符号 ELF
  interop.rs        Rust 入口：标记类型、注册、simulate、Debugger、字符串导出
macros/             proc-macro：hardware_instance、hardware、hardware_test
examples/           单文件 DSL 示例与三个可运行工作区
tests/              端到端与契约测试
docs/               语言与库文档
```

## 许可

MIT，见 [LICENSE](./LICENSE)。
