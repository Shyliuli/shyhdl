# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-17

### Added

- 语言：`#[hardware_instance]` 模块、`#[In]` / `#[Out]` / `#[instance]` 端口、构建期常量泛型、
  跨文件模块，以及组合过程（`run` 中的 `let`、赋值、`if`、`match`、构建期 `for`）和时序过程
  （`always!` 的事件臂、异步复位、非阻塞状态）。
- 数据类型：`uint<N>` / `int<N>`、`struct`、带 payload 的 `enum`、`Option`、`Result`、
  固定长度数组；字段布局按声明顺序高位到低位、无 padding。
- 位操作：等宽算术与回绕、显式 `zext` / `sext` / `trunc`、半开区间切片、`get`、运行期位选、
  有符号算术右移。
- 模式匹配：变体与 payload 模式、`or` 模式、守卫、穷尽性检查、范围模式 `0x10..=0x1F`、位通配
  `mask!(0b11??1111)` / `mask!("11??1111")`；纯位模式生成一条 `case`/`casez`，守卫与范围
  模式保留源码顺序的 `if` 链。
- Verilog-2005 输出：保留模块边界、实例名与源码位置注释；赋值与 `let` 的值位置 `match` 直接
  写目标，整信号作为被匹配值时直接读取，不引入快照临时量。
- Testbench：`#[hardware_test]` 生成独立的标准 Verilog testbench，不依赖 Rust DPI；支持
  `gen_clk` / `clock!` / `reset!` / `initial!` / `always!` / `delay` / `wait` / `wait_until` /
  `sample_after` / `assert!` / `assert_eq!` / `pass!` / `finish!`。
- 仿真：Verilator 后端，记录 VCD 或 FST，写出可复现的 run manifest；外部工具缺失、超时、
  编译错误与仿真失败分别分类。
- 回放与调试：只读波形游标、固定网格或时钟沿的观察点、`Debugger` 会话、合成调试 ELF 与
  GDB RSP 服务；`step` / `next` / `reverse-step` / `continue` / `watch` / 条件 `break` /
  `print` 都作用在记录上，写操作被拒绝。
- 库入口：`check`、`build`、`verilog`、`module_verilog`、`test_verilog`、`list_tests`、
  `test`、`simulate`、`source_for`、`run`。
- 文档：语言参考、库指南与 API 文档。
