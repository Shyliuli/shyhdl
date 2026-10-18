# ShyHDL 库

ShyHDL 是一个 Rust 库：把源码里的硬件描述编译成 Verilog-2005，导出 testbench，交给 Verilator
仿真，并把一次运行录成可回放的记录。命令行二进制只是这层 API 的一个调用者。

## 依赖

```toml
[dependencies]
shyhdl = "0.1"
```

运行仿真需要 Verilator 在 `PATH` 上（也可以设置 `SHYHDL_VERILATOR` 指向它）；只调试已有记录时
需要 GDB。

## 两种写法

**宏模式**：硬件写在普通 Rust 程序里，`main` 是真实可执行文件。宏只登记源码文本并生成占位
定义，所以这些文件正常参与 `cargo build`：

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

**单文件**：把同样的条目写进 `.shy.rs`，由库入口或 `shyhdl` 二进制编译。两种写法共用同一条
流水线；`source_for::<T>()` 返回宏模式登记的源码文本。

## 入口

| 函数 | 作用 |
| --- | --- |
| `check::<T>()` | 编译并报告诊断，不写产物；成功返回 `Ok(())` |
| `build::<T>(out)` | 编译并写出 `<top>.v` 与 `<top>.meta.json`，返回写出的路径 |
| `verilog::<T>()` | 整份设计的 Verilog 文本 |
| `module_verilog::<T>(module)` | 单个模块的 Verilog 文本 |
| `test_verilog::<T>(test)` | 生成一个测试的独立 testbench 文本 |
| `list_tests::<T>()` | 列出 `#[hardware_test]` 的名字 |
| `test::<T>(name, out)` | 编译并运行一个测试，返回 `Outcome` |
| `simulate::<T>(out)` | 编译一次，返回可多次运行的 `Simulator` |
| `Simulator::run(test)` | 运行一个测试，返回 `RunResult` |
| `source_for::<T>()` | 取回宏模式登记的源码 |
| `run()` | 把进程参数转交命令行（二进制入口用） |

## 类型

- `U<N>` / `I<N>`：源码里的 `uint<N>` / `int<N>`。它们是库内标记类型，前端按硬件宽度解释，
  不参与 Rust 的真实算术。
- `TestId`：`#[hardware_test]` 占位函数返回的测试标识。
- `Simulator`：一次性编译出的设计，可以反复 `run`。
- `RunResult`：一次运行的结果，含 `outcome`、`reason`、`test_name`、`wave_path`、
  `manifest_path`、`out`。
- `Outcome`：`Pass` / `Fail` / `Timeout` / `Environment` / `CompileError` / `SimulationError`
  等分类。
- `Debugger`：把 `RunResult` 的录制接到真实 GDB 的会话。
- `Error` / `Result`：库错误类型，带源码位置或路径。

## 导出与仿真

```rust
use shyhdl::*;

fn main() -> shyhdl::Result<()> {
    // 只编译，不写文件
    check::<Adder4>()?;

    // 写出 Verilog 与元数据
    let written = build::<Adder4>("build")?;
    println!("{written:?}");

    // 编译一次，跑测试
    let mut sim = simulate::<Adder4>("build/run")?;
    let result = sim.run(wraps)?;
    println!("{:?}", result.outcome);
    Ok(())
}
```

## 上板（`nvboard` feature）

```rust
#[cfg(feature = "nvboard")]
{
    // 打开虚拟板：开关 -> 输入，LED <- 输出，关窗即退出
    let run = shyhdl::board::<Adder4>("build")?;
    for binding in &run.bindings {
        println!("{binding}");
    }
}
```

`board` 与 `simulate` 用同一份 Verilog，只是把 `main()` 换成 NVBoard 的帧循环；构建、引脚
分配（`vendor/nvboard` 里的 `libnvboard.a`、`auto_bind.cpp`、`board_runner.cpp`）都写在
`build/` 下，可以像其它产物一样检查。不启用 feature 时这个函数不存在。

## 调试

`Debugger` 复用一次运行的录制只读回放，不重新仿真：

```rust
let mut sim = simulate::<Adder4>("build/run")?;
let result = sim.run(wraps)?;

let dbg = Debugger::new(&result, 10)?;
println!("elf={}", dbg.elf_path().display());
println!("port={}", dbg.port);
println!("gdb={}", dbg.gdb_argv().join(" "));
dbg.run(result)?;          // 阻塞，等一个 GDB 会话
```

- `elf_path()`：本次会话的合成调试 ELF（每个信号一个变量，每个观察点一行）。
- `gdb_argv()`：可直接执行的 GDB 命令行，已包含只读开关与 `target remote`。
- `run(result)`：只服务一个 GDB 会话，会话结束后返回。

符号名规则：`<instance_path>__<source_name>`（如 `Encoder38__sig`），源名在设计里唯一时还会
有短名别名（`sig`）。

## 宏模式的边界

- 宏把程序里登记的所有条目拼成一份源码编译，多个设计若声明同名类型会冲突。
- 占位定义只保证 Rust 侧能编译；硬件语义在运行时由前端解释。
- `#[hardware] impl` 的函数体不参与 Rust 编译，编译器不会检查其中的 Rust 类型错误。

## 错误与工具

- 源码编译错误带文件、行、列与稳定错误码；用法错误与外部工具缺失是分开的错误分类。
- Verilator 通过 `PATH` 查找，可用 `SHYHDL_VERILATOR` 覆盖；超时可用 `SHYHDL_TIMEOUT` 设置。
- 仿真产物（Verilog、元数据、波形）是构建结果，不应提交到版本库。
