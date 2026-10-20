# ShyHDL 语言参考

ShyHDL 用 Rust 风格的源码描述硬件。编译器把它解释成硬件语义，导出 Verilog-2005；源码本身
不会被编译成机器码。

两种写法：

- **单文件**：一个 `.shy.rs` 文件就是一个设计，用库入口 `check::<Top>()` / `build::<Top>(out)`
  编译，或交给 `shyhdl` 二进制。
- **宏模式**：把同样的条目写进普通 Rust 程序，`main` 是真实可执行文件：宏只登记源码文本并
  生成占位定义，运行时再编译设计。两种写法共用同一条流水线。

## 模块

`#[hardware_instance] struct` 是一个模块；字段是端口或子实例：

```rust
#[hardware_instance]
pub struct Adder4 {
    #[In] pub a: U<4>,
    #[In] pub b: U<4>,
    #[Out] pub sum: U<4>,
    #[Out] pub carry: bool,
    #[instance] pub child: Child,
}
```

- `#[In]` / `#[Out]` 标记端口方向；`U<N>` 是 `uint<N>` 的库拼写，`I<N>` 是 `int<N>`。
- 带常量泛型的模块在构建期特化：`Adder<8>`、`Adder<16>` 是两个模块。
- 子实例用字段名连接：在 `run` 里写 `self.child.a = self.a;`，不需要调用 `child.run()`；
  连接的书写顺序不改变拓扑。
- 子实例也可以是数组：`#[instance] pub cells: [Child; 4]` 是 4 个实例，Verilog 里叫
  `cells__0` … `cells__3`，每个元素有自己的输出网 `cells__0_<port>`。用构建期常量下标选一个，
  连线和读端口都写 `self.cells[i].a`：

  ```rust
  // 4 位行波进位：上一级的 cout 直接接下一级的 cin
  self.cells[0].cin = self.cin;
  for i in 0..3 {
      self.cells[i + 1].cin = self.cells[i].cout;
  }
  for i in 0..4 {
      self.cells[i].a = self.a[i];
      self.cells[i].b = self.b[i];
      self.s[i] = self.cells[i].s;
  }
  ```

  下标必须是构建期常量（可以带参数和常量表达式，例如 `i + 1`）：没有哪根线能在运行期选一个
  子模块。`run` 顶层的构建期 `for` 会先展开再分类，所以循环体里的连线仍然是连线。

- `mod name;` 在同目录加载 `name.shy.rs` 或 `name/mod.shy.rs`，模块名就是 `name`。
- 端口按具名字段展开：一个 N 位字段对应一个 `[N-1:0]` 端口，不做位域重排。

## 组合过程

`run` 的函数体是默认组合过程，按源码顺序求值：

```rust
fn run(&mut self) {
    let wide: U<5> = self.a.zext::<5>() + self.b.zext::<5>();
    self.sum = wide.trunc::<4>();
    self.carry = wide.get(4);
    if self.enable {
        self.out = self.sum;
    } else {
        self.out = 0;
    }
}
```

- **每条路径都必须赋值**：组合过程里漏掉的位会让电路锁存，编译器在构建期报源码级错误。
- **后写覆盖前写**：同一个过程内先写后覆盖是合法的，不是多驱动；其它过程写到同一个位才是
  多驱动（每个位恰好一个驱动者）。
- **组合环路必须能被检测**：成环的读-写链在构建期报错。
- **裸字段名等价于 `self.字段名`**：在 `run` 中，名字按“过程局部量、构建期参数、模块字段”
  的顺序查找；前两者都没有时，才把裸名解释为模块字段或子实例字段。因此 `y = a;` 与
  `self.y = self.a;` 等价，`kid.a = a;` 与 `self.kid.a = self.a;` 等价。局部变量始终优先，
  所以块内的 `let mut a = ...` 会遮蔽同名字段。
- `let` 在块内是过程局部量；写在 `run` 顶层的 `let` 是模块信号。
- `for` 循环必须是构建期可展开的静态循环，次数在构建期确定；运行期条件的循环被拒绝。
- `if` 是值表达式时可以出现在赋值右侧；未选分支的语句不会执行。

## 时序过程

`always!` 列出事件臂；**位置**决定语义，而不是信号名或边沿极性：

```rust
always! {
    #[Nedge] self.rst_n => { self.q = 0; }        // 最后一个臂之前：异步复位
    #[Pedge] self.clk => {                          // 最后一个臂：时钟沿
        if self.enable { self.q = self.d; }
    }
}
```

- 最后一个臂之前的所有臂都是异步复位，进入敏感列表，电平有效期间执行；复位优先级按源码
  顺序，第一个最高。
- 最后一个臂是常规时钟分支，在复位电平都不活跃时运行。
- 状态赋值是非阻塞的：同一过程内读状态读到旧值。
- 过程内 `let` 是阻塞临时量，按顺序计算，不受非阻塞提交影响。
- 寄存器身份由时序过程定义：`let mut` 不等于寄存器，单独的时钟信号标签也不会让信号变成状态。

## 位宽与运算

- 宽度总是显式：`uint<N>` / `int<N>`；整数字面量可以从上下文推导宽度，推导不出时报错。
- 位序 LSB 为 0；切片是半开区间 `x[lo..hi]`，包含 `lo`、不包含 `hi`。
- 等宽加法在 N 位回绕；第 N 位进位用 `get(N)` 取。
- 扩展与截断必须显式：`zext::<W>()`、`sext::<W>()`、`trunc::<W>()`，或位切片。
- 符号性转换写 `as int<W>` / `as uint<W>`；等宽赋值不隐式改变符号性。
- `>>` 是有符号算术右移（有符号操作数），`<<` / `>>` 的位数可以是常量或运行期值。
- 中间值宽度不自动提升；先算出宽位再截断，是写出进位的标准做法。

## 组合函数

`#[hardware_instance]` 也可以标在函数上：一个函数就是一段组合逻辑，参数是输入，返回类型列出
输出。函数不能包含 `always!`，调用点会实例化它。

```rust
#[hardware_instance]
fn AddMul(a: uint<4>, b: uint<4>) -> (uint<4>, uint<8>) {
    let sum = a + b;
    let product = a.zext::<8>() * b.zext::<8>();
    (sum, product)
}

fn run(&mut self) {
    let (s, p) = AddMul(self.a, self.b);
    self.sum = s;
    self.product = p;
}
```

- 函数体是普通的组合语句，最后必须给出返回值；多个输出写元组，单个输出直接写表达式。
- 每次调用都是一个独立实例：同一个函数调用两次就是两个实例，各自有独立的输出线。
- 函数的输出端口在生成的 Verilog 里命名为 `out0`、`out1`，按返回元组的顺序排列。
- 函数可以调用其它函数；调用关系在构建期展开成模块实例，不会递归执行。
- 函数是组合逻辑，出现 `always!` 会在解析期报错；时序逻辑只能写在模块的 `run` 里。

## 数据类型

- `struct` 是固定尺寸值类型：字段按声明顺序从高位排到低位，不插入 padding，布局不依赖 Rust
  的 ABI。普通 `struct` 不会自动成为寄存器或模块。
- `enum` 编码为标签加最大 payload 尺寸；未使用的 payload 位填零。类型相等按 variant 与有效
  字段比较。
- `Option` / `Result` 是普通 enum，不会自动映射成 ready/valid 握手。
- 数组 `[T; N]` 是固定布局的值类型，元素 0 在最低位；不会被推断成 RAM。
- 元组 `(A, B)` 是匿名字段的结构体：元素按位置从高位排到低位，用 `.0`、`.1` 读取，也可以
  赋值。它主要用于函数返回多个值。
- `let` 支持解构：元组 `let (a, b) = v;`、结构体 `let Point { x, y } = p;`、嵌套模式、
  重命名 `Point { x: px, .. }` 和通配 `_`。写在 `run` 顶层时，每个绑定都是模块信号；写在
  控制流内则是过程局部量。
- `enum` 的 payload 解构请用 `match`：`let Some(v) = o;` 会报错并提示改用 `match`，因为
  `let` 没有失败分支可以处理。
- 端口和信号可以是这些类型的组合。

## 匹配

`match` 出现在语句位置或值位置，语义相同：按源码顺序，第一个模式匹配且守卫成立的臂执行。

```rust
// sig: uint<8>, code: uint<8>
self.code = match self.sig {
    0 | 1 => 0b0000_0001,
    2 => 0b0000_0010,
    mask!(0b0000_00??) => 0b1000_0000,
    0x00..=0x7F => 0b0100_0000,
    _ => 0,
};
```

- **必须穷尽**：所有取值都要被某个臂接住，否则构建期报错并列出未覆盖的取值。
- **模式**：常量、`or` 模式、范围 `0x10..=0x1F`、位通配 `mask!(0b11??1111)` /
  `mask!("11??1111")`、变体与 payload 模式、结构体字段模式、绑定。
- **守卫**：`x if x > 2 => ...`；守卫可以读绑定。
- **范围模式**的区间并集覆盖整个类型域时算穷尽，不需要 `_`。**位通配**只比较 `0`/`1` 指定的
  位，需要 `_` 兜底，因为这里不声称能证明位级覆盖。
- **代码生成**：无守卫且模式全是位模式（常量、`mask!`、变体标签与 payload）时，整个 `match`
  生成一条 `case`；含 `mask!` 时生成 `casez`，`?` 位就是通配，被匹配的信号直接作为 `case`
  的表达式，不引入快照临时量。范围模式（`case` 的一臂只能是一个值）与带守卫的 `match` 保留
  源码顺序的 `if`/`else` 链。
- **值位置**的 `match` 遵循 Rust 语法：分支的尾表达式就是整个表达式的值，分支块可以包含
  语句。作为整条赋值或 `let` 的右值时，每个分支直接写目标，不经过结果临时量；被匹配的是整
  信号时也直接读它，只有表达式才需要一份快照。
- 规模上限：嵌套 16 层、单个臂最多 64 个备选、覆盖检查最多 4096 行、256 个并行列、65536 个
  单元，总计 100 万次访问。

## 测试

`#[hardware_test]` 声明一个 ShyHDL 测试；它生成独立的标准 Verilog-2005 testbench，不依赖
Rust DPI：

```rust
#[hardware_test]
fn adds() {
    let mut dut = Adder4::new();
    initial! {
        dut.a = 1;
        dut.b = 2;
        delay(ns(1));
        assert_eq!(dut.sum, 3);
        pass!();
        finish!();
    }
}
```

- `gen_clk` / `clock!` 生成时钟，`reset!` 驱动复位，`always!` 写周期行为。
- `delay`、`wait`、`wait_until`、`sample_after` 控制时间；`assert!` / `assert_eq!` 失败即
  终止该测试。
- `pass!` / `finish!` 结束测试；没有 `finish!` 的测试靠超时判定失败。
- 测试可以声明时间精度与时钟频率；具体写法见示例。

## 语言边界

- 没有动态堆、递归、trait 对象与运行期循环。
- 赋值与运算按硬件语义解释，不等同于 Rust 求值模型；`#[hardware] impl` 的函数体只被登记，
  不参与 Rust 编译。
- 位宽不会隐式提升或截断；越界与索引错误在构建期报告。
- Verilator 后端是二态语义，不声称完整四态仿真。
