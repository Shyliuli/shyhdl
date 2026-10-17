//! The typed model a ShyHDL design lowers to.
//!
//! This is the exchange surface between the frontend and everything that comes
//! after it: Verilog generation, testbench generation and the debug metadata
//! builder. Every width, direction and bit range here is already resolved: the
//! generic parameters are gone, `N + 1` has become a number, and a struct field
//! access has become an explicit bit range.
//!
//! Sizes are one-dimensional bit vectors, so the model has no notion of a bus
//! wider than its own value. Structs are *not* a second storage form: a
//! struct-typed signal is a single `width()`-bit signal and `Ty::Struct` only
//! remembers how to cut it into fields (REQ-LANG-003).

use crate::metadata::{EnumVariant, SourceSpan, TypeField, ValueType};

/// Index of a signal inside [`Module::signals`].
pub type SignalId = usize;

/// A hardware type with a fixed bit width.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    /// One bit.
    Bool,
    /// `uint<W>`, `W >= 1`.
    UInt(u32),
    /// `int<W>`, `W >= 1`: one `W`-bit two's-complement value.
    SInt(u32),
    /// `[T; N]`, `N >= 1`: `N` values of `T`, element 0 in the least
    /// significant bits (REQ-LANG-047).
    ///
    /// The total width is checked against the maximum supported width where the
    /// type is built, so [`Ty::width`] cannot overflow here.
    Array { element: Box<Ty>, len: u32 },
    /// An `enum`, or a built-in `Option<T>` / `Result<T, E>`: a tag in the
    /// lowest bits with the widest variant's payload above it
    /// (REQ-LANG-040, REQ-LANG-041).
    ///
    /// Like an array, the total width is checked where the type is built.
    Enum(EnumTy),
    /// A plain data struct: one value of `total` bits, fields packed most
    /// significant first.
    Struct(StructTy),
}

impl Ty {
    /// Width in bits.
    pub fn width(&self) -> u32 {
        match self {
            Ty::Bool => 1,
            Ty::UInt(width) => *width,
            Ty::SInt(width) => *width,
            Ty::Array { element, len } => element.width() * len,
            Ty::Enum(enum_ty) => enum_ty.tag_width + enum_ty.payload_width(),
            Ty::Struct(strukt) => strukt.total,
        }
    }

    /// True for `bool`.
    pub fn is_bool(&self) -> bool {
        matches!(self, Ty::Bool)
    }

    /// True for a two's-complement value. `bool`, an array, an enum and a plain
    /// struct are not signed: their bits have no sign to read.
    pub fn is_signed(&self) -> bool {
        matches!(self, Ty::SInt(_))
    }

    /// The element type of an array.
    pub fn element(&self) -> Option<&Ty> {
        match self {
            Ty::Array { element, .. } => Some(element),
            _ => None,
        }
    }

    /// The field named `name`, for struct types only.
    pub fn field(&self, name: &str) -> Option<&TyField> {
        match self {
            Ty::Struct(strukt) => strukt.fields.iter().find(|field| field.name == name),
            _ => None,
        }
    }

    /// The shared metadata model for this type (REQ-DBG-004).
    pub fn to_value_type(&self) -> ValueType {
        match self {
            Ty::Bool => ValueType::Bool,
            Ty::UInt(width) => ValueType::UInt { width: *width },
            Ty::SInt(width) => ValueType::SInt { width: *width },
            Ty::Array { element, len } => ValueType::Array {
                element: Box::new(element.to_value_type()),
                len: *len as usize,
            },
            Ty::Enum(enum_ty) => enum_ty.to_value_type(),
            Ty::Struct(strukt) => ValueType::Struct {
                name: strukt.name.clone(),
                fields: strukt
                    .fields
                    .iter()
                    .map(|field| TypeField {
                        name: field.name.clone(),
                        ty: field.ty.to_value_type(),
                    })
                    .collect(),
            },
        }
    }

    /// Human-readable name used in diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Ty::Bool => "bool".to_string(),
            Ty::UInt(width) => format!("uint<{width}>"),
            Ty::SInt(width) => format!("int<{width}>"),
            Ty::Array { element, len } => format!("[{}; {len}]", element.describe()),
            Ty::Enum(enum_ty) => enum_ty.name.clone(),
            Ty::Struct(strukt) => format!("struct {} ({} bits)", strukt.name, strukt.total),
        }
    }
}

/// A plain data struct's layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructTy {
    /// Struct name as written in the source.
    pub name: String,
    /// Fields in declaration order, most significant first.
    pub fields: Vec<TyField>,
    /// Total width: the sum of the field widths, with no padding.
    pub total: u32,
}

impl StructTy {
    /// A struct whose fields are packed most significant first, in the order
    /// they are given, with no padding (REQ-LANG-003).
    ///
    /// Fails, rather than inventing a layout, on an empty field list or a total
    /// that does not fit.
    pub(crate) fn from_fields(
        name: impl Into<String>,
        fields: Vec<(String, Ty)>,
    ) -> Result<StructTy, String> {
        let name = name.into();
        if fields.is_empty() {
            return Err(format!(
                "struct `{name}` has no fields and therefore no width"
            ));
        }
        let mut total: u32 = 0;
        for (_, ty) in &fields {
            total = total
                .checked_add(ty.width())
                .ok_or_else(|| format!("struct `{name}` is wider than {} bits", u32::MAX))?;
        }
        let mut resolved = Vec::new();
        let mut prefix = 0u32;
        for (field_name, ty) in fields {
            let width = ty.width();
            let offset = total - prefix - width;
            prefix += width;
            resolved.push(TyField {
                name: field_name,
                ty,
                offset,
                width,
            });
        }
        Ok(StructTy {
            name,
            fields: resolved,
            total,
        })
    }
}

/// An enum's layout, or a built-in `Option<T>` / `Result<T, E>`.
///
/// The value is one bit vector: the tag in the lowest `tag_width` bits, then
/// the **widest** variant's payload - every variant shares that room, its own
/// payload sits at the bottom of it, and the bits above it are zero
/// (REQ-LANG-040, REQ-LANG-041). Nothing here stores an offset or a width that
/// the tag width and the payload types do not already say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumTy {
    /// Name as written in the source (`Msg`), or the built-in's name.
    pub name: String,
    /// Width of the tag in bits, at least 1.
    pub tag_width: u32,
    /// Variants in declaration order.
    pub variants: Vec<EnumVariantTy>,
}

impl EnumTy {
    /// The room a payload has: the widest variant's payload width.
    pub fn payload_width(&self) -> u32 {
        self.variants
            .iter()
            .map(EnumVariantTy::payload_width)
            .max()
            .unwrap_or(0)
    }

    /// The variant with this name.
    pub fn variant(&self, name: &str) -> Option<&EnumVariantTy> {
        self.variants.iter().find(|variant| variant.name == name)
    }

    /// The shared metadata model (REQ-DBG-004).
    pub fn to_value_type(&self) -> ValueType {
        ValueType::Enum {
            name: self.name.clone(),
            tag_width: self.tag_width,
            variants: self
                .variants
                .iter()
                .map(|variant| EnumVariant {
                    name: variant.name.clone(),
                    tag: variant.tag,
                    payload: variant
                        .payload
                        .as_ref()
                        .map(|payload| Box::new(payload.to_value_type())),
                })
                .collect(),
        }
    }
}

/// One variant of an [`EnumTy`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariantTy {
    /// Variant name as written in the source.
    pub name: String,
    /// Encoded tag value.
    pub tag: u64,
    /// Payload type, `None` for a variant that carries nothing.
    pub payload: Option<Ty>,
}

impl EnumVariantTy {
    /// Width of this variant's own payload, not the room it shares.
    pub fn payload_width(&self) -> u32 {
        self.payload.as_ref().map(Ty::width).unwrap_or(0)
    }

    /// The bits this variant's payload occupies: its own width, not the room's.
    pub fn payload_range(&self, tag_width: u32) -> (u32, u32) {
        (tag_width, tag_width + self.payload_width())
    }
}

impl Ty {
    /// The typed layout a metadata value type describes.
    ///
    /// The inverse of [`Ty::to_value_type`], for consumers that work on the
    /// shared metadata model - the pattern analyzer, for one - and need a typed
    /// value back. It fails, rather than inventing a layout, when the value type
    /// is not one this frontend builds.
    ///
    /// The shared pattern analyzer hands bindings back as metadata types; the
    /// match lowering turns them into typed locals with this.
    pub(crate) fn from_value_type(ty: &ValueType) -> Result<Ty, String> {
        match ty {
            ValueType::Bool => Ok(Ty::Bool),
            ValueType::UInt { width } => {
                if *width == 0 {
                    return Err("`uint` has no bits".to_string());
                }
                Ok(Ty::UInt(*width))
            }
            ValueType::SInt { width } => {
                if *width == 0 {
                    return Err("`int` has no bits".to_string());
                }
                Ok(Ty::SInt(*width))
            }
            ValueType::Array { element, len } => {
                let element = Ty::from_value_type(element)?;
                let len = u32::try_from(*len)
                    .map_err(|_| format!("an array of {len} elements is too long"))?;
                if len == 0 {
                    return Err("an array must have at least one element".to_string());
                }
                Ok(Ty::Array {
                    element: Box::new(element),
                    len,
                })
            }
            ValueType::Enum {
                name,
                tag_width,
                variants,
            } => {
                if *tag_width == 0 || variants.is_empty() {
                    return Err(format!("enum `{name}` has no tag or no variants"));
                }
                let mut resolved = Vec::new();
                for variant in variants {
                    let payload = match &variant.payload {
                        Some(payload) => Some(Ty::from_value_type(payload)?),
                        None => None,
                    };
                    resolved.push(EnumVariantTy {
                        name: variant.name.clone(),
                        tag: variant.tag,
                        payload,
                    });
                }
                Ok(Ty::Enum(EnumTy {
                    name: name.clone(),
                    tag_width: *tag_width,
                    variants: resolved,
                }))
            }
            ValueType::Struct { name, fields } => {
                let mut resolved = Vec::new();
                for field in fields {
                    resolved.push((field.name.clone(), Ty::from_value_type(&field.ty)?));
                }
                Ok(Ty::Struct(StructTy::from_fields(name.clone(), resolved)?))
            }
        }
    }
}

/// One field of a [`StructTy`], with its place in the enclosing bit vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TyField {
    /// Field name as written in the source.
    pub name: String,
    /// Field type.
    pub ty: Ty,
    /// Least significant bit position of the field inside the struct.
    pub offset: u32,
    /// Field width in bits.
    pub width: u32,
}

impl TyField {
    /// Half-open bit range `[offset, offset + width)` of the field inside its
    /// enclosing struct.
    pub fn range(&self) -> (u32, u32) {
        (self.offset, self.offset + self.width)
    }
}

/// A resolved build-time parameter, for example `N = 8` of `Adder<8>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    /// Parameter name as written in the source.
    pub name: String,
    /// Value it was specialised with.
    pub value: u64,
}

/// Direction of a module port.
///
/// A clock is an ordinary input: the language has no clock marking, and which
/// signal is used as a reference clock is a property of a testbench, not of the
/// hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortDir {
    /// Driven by the parent; `#[In]`.
    In,
    /// Drives the parent; `#[Out]`.
    Out,
}

impl PortDir {
    /// `input` / `output` as used in Verilog.
    pub fn verilog_keyword(self) -> &'static str {
        match self {
            PortDir::In => "input",
            PortDir::Out => "output",
        }
    }

    /// True for input-like directions.
    pub fn is_input(self) -> bool {
        matches!(self, PortDir::In)
    }
}

/// Where a signal lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalScope {
    /// Declared at module scope.
    Module,
    /// Declared inside a process and therefore invisible outside it.
    Process(usize),
}

/// How a signal is driven, which decides `wire` versus `reg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    /// A module port.
    Port(PortDir),
    /// Driven by continuous assignments or an instance output.
    Wire,
    /// Driven by a process.
    Reg,
}

/// One bit vector in a module: a port, an internal wire or register, or a
/// process-local temporary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signal {
    /// Name in the generated Verilog. For source signals this is the source
    /// name; for compiler-introduced temporaries it is derived from the source
    /// position, never from a random value (REQ-VER-005, REQ-VER-009).
    pub name: String,
    /// Source name, `None` for a compiler-introduced temporary.
    pub source_name: Option<String>,
    /// Type of the value.
    pub ty: Ty,
    /// Declaration order position, used to keep output stable.
    pub kind: SignalKind,
    /// Where the signal lives.
    pub scope: SignalScope,
    /// Where it was declared.
    pub source: Option<SourceSpan>,
}

impl Signal {
    /// Width in bits.
    pub fn width(&self) -> u32 {
        self.ty.width()
    }
}

/// A module port, projected out of [`Module::signals`] for consumers such as
/// testbench generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    /// Field name as written in the source.
    pub source_name: String,
    /// Name in the generated Verilog (identical today; kept separate so the
    /// naming policy can change without touching consumers).
    pub verilog_name: String,
    /// Direction.
    pub dir: PortDir,
    /// Width in bits (REQ-LANG-030: always explicit).
    pub width: u32,
    /// Type of the value, including the struct layout when applicable.
    pub ty: ValueType,
    /// Where it was declared.
    pub source: SourceSpan,
    /// The port signal in [`Module::signals`]; the single place the port's
    /// internal type and driver live.
    pub(crate) signal: SignalId,
}

/// A child module instantiated by a parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    /// Field name in the parent struct: also the instance name in Verilog.
    pub field: String,
    /// Instance name in the generated Verilog.
    pub verilog_name: String,
    /// Source struct name of the child.
    pub module_source_name: String,
    /// Specialised Verilog module name of the child, for example `Adder_N8`.
    pub module_verilog_name: String,
    /// Parameters the child was specialised with.
    pub params: Vec<Param>,
    /// One entry per child port, in child declaration order.
    pub connections: Vec<Connection>,
    /// Where the instance was declared.
    pub source: SourceSpan,
}

/// One connection between a parent expression and a child port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// Child port name.
    pub port: String,
    /// Parent-side expression driving the child input.
    pub expr: Expr,
    /// Where the connection was written.
    pub source: SourceSpan,
}

/// Which signal or child port an expression refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LValue {
    /// Bits `[lo, hi)` of a signal, or the whole signal when `range` is `None`.
    Signal {
        signal: SignalId,
        range: Option<(u32, u32)>,
        source: SourceSpan,
    },
}

impl LValue {
    /// The signal being written.
    pub fn signal(&self) -> SignalId {
        match self {
            LValue::Signal { signal, .. } => *signal,
        }
    }

    /// The bit range being written, `None` for the whole signal.
    pub fn range(&self) -> Option<(u32, u32)> {
        match self {
            LValue::Signal { range, .. } => *range,
        }
    }

    /// Where the assignment target was written.
    pub fn source(&self) -> &SourceSpan {
        match self {
            LValue::Signal { source, .. } => source,
        }
    }
}

/// Binary operators of the supported expression subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    /// `+`, equal widths, wrapping.
    Add,
    /// `-`, equal widths, wrapping.
    Sub,
    /// `*`, equal widths, wrapping.
    Mul,
    /// `&`, bitwise.
    BitAnd,
    /// `|`, bitwise.
    BitOr,
    /// `^`, bitwise.
    BitXor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`, unsigned.
    Lt,
    /// `<=`, unsigned.
    Le,
    /// `>`, unsigned.
    Gt,
    /// `>=`, unsigned.
    Ge,
    /// `&&`, boolean only.
    LogAnd,
    /// `||`, boolean only.
    LogOr,
}

impl BinOp {
    /// The operator as written in Verilog.
    pub fn verilog(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::LogAnd => "&&",
            BinOp::LogOr => "||",
        }
    }

    /// The operator as written in the source.
    pub fn source(self) -> &'static str {
        self.verilog()
    }
}

/// Unary operators of the supported expression subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// `!`, boolean only.
    Not,
    /// `~`, same width.
    BitNot,
    /// `-`, signed values only, wrapping at the value's own width.
    Neg,
}

impl UnOp {
    /// The operator as written in Verilog.
    pub fn verilog(self) -> &'static str {
        match self {
            UnOp::Not => "!",
            UnOp::BitNot => "~",
            UnOp::Neg => "-",
        }
    }
}

/// A resolved value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    /// Type and therefore width of the value.
    pub ty: Ty,
    /// Shape of the value.
    pub kind: ExprKind,
    /// Where it was written.
    pub source: SourceSpan,
}

/// Shape of a resolved value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprKind {
    /// A constant. `value` is the value itself, so a negative one is a negative
    /// `int<W>` value and never a bit pattern read as a magnitude; `width == 0`
    /// marks an unsized integer literal, which is only legal as a shift count
    /// and is emitted as a plain decimal.
    Const { value: i128, width: u32 },
    /// A constant with don't-care bits, the value of a `casez` arm: `care`
    /// selects the bits that are compared and `value` says what they must be.
    /// It only appears as the value of one [`CaseArm`]; nothing reads it.
    Pattern { value: u128, care: u128, width: u32 },
    /// A whole signal.
    Signal(SignalId),
    /// Bits `[lo, hi)` of a signal.
    Slice { signal: SignalId, lo: u32, hi: u32 },
    /// An operation.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// A unary operation.
    Unary { op: UnOp, operand: Box<Expr> },
    /// `cond ? then : else`, produced by an `if` expression.
    Ternary {
        cond: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    /// Concatenation, most significant element first.
    Concat(Vec<Expr>),
    /// Replication: `count` copies of `value`, as in `{4{bit}}`.
    Replicate { count: u32, value: Box<Expr> },
    /// Indexed bit select with a run-time index.
    BitSelect { signal: SignalId, index: Box<Expr> },
}

/// One statement inside a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// An assignment. `nonblocking` is true for state in a sequential process
    /// (REQ-LANG-016), false for combinational logic and process locals
    /// (REQ-LANG-017).
    Assign {
        target: LValue,
        value: Expr,
        nonblocking: bool,
        source: SourceSpan,
    },
    /// `if`/`else`.
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
        source: SourceSpan,
    },
    /// `case`/`casez`: the run-time element write dispatch (one arm per index)
    /// and a `match` whose arms are all plain bit patterns. A `match` that needs
    /// a guard or a range keeps its [`Stmt::If`] chain.
    Case {
        kind: CaseKind,
        scrutinee: Expr,
        arms: Vec<CaseArm>,
        default: Option<Vec<Stmt>>,
        source: SourceSpan,
    },
}

/// Which comparison a [`Stmt::Case`] performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseKind {
    /// `case`: every arm value is a constant with all bits compared.
    Case,
    /// `casez`: an arm value may contain don't-care bits, spelled `?`.
    Casez,
}

impl CaseKind {
    /// The keyword as written in Verilog.
    pub fn verilog(self) -> &'static str {
        match self {
            CaseKind::Case => "case",
            CaseKind::Casez => "casez",
        }
    }
}

/// One arm of a [`Stmt::Case`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseArm {
    /// Values this arm matches, all of the scrutinee's type and width.
    pub values: Vec<Expr>,
    /// Body of the arm.
    pub body: Vec<Stmt>,
    /// Where the arm was written.
    pub source: SourceSpan,
}

/// Clock edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// `posedge`.
    Posedge,
    /// `negedge`.
    Negedge,
}

impl Edge {
    /// The keyword as written in Verilog.
    pub fn verilog(self) -> &'static str {
        match self {
            Edge::Posedge => "posedge",
            Edge::Negedge => "negedge",
        }
    }
}

/// The regular edge branch of a sequential process.
///
/// It is the branch that runs when no reset level is active: the hardware clock
/// in the ordinary case. It is identified by the source's control flow (the last
/// arm of `always!`, or the last branch of an edge chain), never by a signal
/// name or an edge polarity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockSpec {
    /// Signal whose edge triggers the branch, always an input port of this module.
    pub signal: SignalId,
    /// Active edge.
    pub edge: Edge,
}

/// An asynchronous reset of a sequential process.
///
/// The reset is in the sensitivity list, so its arm runs as soon as the active
/// level appears, between clock edges (REQ-LANG-020, AC-MVP-005). `edge` says
/// which transition enters the arm; the body runs while the level is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetSpec {
    /// Reset signal, always an input port of this module.
    pub signal: SignalId,
    /// Edge that enters the reset arm.
    pub edge: Edge,
}

/// What kind of process a [`Process`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessKind {
    /// Combinational: `always @(*)`, blocking assignments, every path covered.
    Comb,
    /// Sequential: an edge-triggered `always` block.
    ///
    /// `resets` are asynchronous resets in source order; the first one has the
    /// highest priority (REQ-LANG-020).
    Seq {
        clock: ClockSpec,
        resets: Vec<ResetSpec>,
    },
}

/// A combinational or sequential process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    /// Stable label: `comb0`, `seq0`, ... in source order.
    pub label: String,
    /// Kind of process.
    pub kind: ProcessKind,
    /// Signals declared inside the process (named-block temporaries).
    pub locals: Vec<SignalId>,
    /// Statements in source order.
    pub body: Vec<Stmt>,
    /// Where the process was written.
    pub source: SourceSpan,
}

/// One item of a module body, in source order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyItem {
    /// Continuous wiring: `assign`.
    Assign {
        target: LValue,
        value: Expr,
        source: SourceSpan,
    },
    /// An instantiated child, indexing [`Module::instances`].
    Instance(usize),
    /// A process.
    Process(Process),
}

/// One module: one `#[hardware_instance]` struct at one parameter binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    /// Source struct name.
    pub source_name: String,
    /// Name in the generated Verilog: the source name, plus the parameter
    /// binding for a generic struct (`Adder` with `N = 8` becomes `Adder_N8`).
    pub verilog_name: String,
    /// Parameters this module was specialised with.
    pub params: Vec<Param>,
    /// Whether this is the requested top module.
    pub is_top: bool,
    /// Where the struct was declared.
    pub source: SourceSpan,
    /// Ports in declaration order.
    pub ports: Vec<Port>,
    /// Child instances, in declaration order.
    pub instances: Vec<Instance>,
    /// All signals of the module, ports included.
    pub signals: Vec<Signal>,
    /// Body items; instances are also reachable through [`Module::instances`].
    pub body: Vec<BodyItem>,
}

impl Module {
    /// The signal with the given Verilog name, module scope only.
    pub fn signal_by_name(&self, name: &str) -> Option<&Signal> {
        self.signals.iter().find(|signal| signal.name == name)
    }

    /// The port with the given Verilog name.
    pub fn port(&self, verilog_name: &str) -> Option<&Port> {
        self.ports
            .iter()
            .find(|port| port.verilog_name == verilog_name)
    }

    /// Processes in source order.
    pub fn processes(&self) -> impl Iterator<Item = &Process> {
        self.body.iter().filter_map(|item| match item {
            BodyItem::Process(process) => Some(process),
            _ => None,
        })
    }

    /// Continuous assignments in source order.
    pub fn assignments(&self) -> impl Iterator<Item = (&LValue, &Expr)> {
        self.body.iter().filter_map(|item| match item {
            BodyItem::Assign { target, value, .. } => Some((target, value)),
            _ => None,
        })
    }
}

/// A fully lowered design: every module reachable from the top, specialised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Design {
    /// Path of the source file this design came from.
    pub(crate) source_name: String,
    /// Modules, the top module first.
    pub(crate) modules: Vec<Module>,
    /// Index of the top module.
    pub(crate) top: usize,
    /// Non-fatal diagnostics found while lowering.
    pub(crate) warnings: Vec<crate::frontend::Diagnostic>,
}

impl Design {
    /// Path of the source file the design was built from.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// The requested top module.
    pub fn top(&self) -> &Module {
        &self.modules[self.top]
    }

    /// All modules: the top module first, then the rest in discovery order.
    pub fn modules(&self) -> &[Module] {
        &self.modules
    }

    /// The module with this generated Verilog name, for example `Adder_N8`.
    pub fn module(&self, verilog_name: &str) -> Option<&Module> {
        self.modules
            .iter()
            .find(|module| module.verilog_name == verilog_name)
    }

    /// The module whose source struct has this name, regardless of parameters.
    ///
    /// When the struct is generic and reachable at several parameter bindings
    /// this returns the first one in discovery order; use [`Design::resolve`] to
    /// pick an exact binding.
    pub fn module_by_source_name(&self, source_name: &str) -> Option<&Module> {
        self.modules
            .iter()
            .find(|module| module.source_name == source_name)
    }

    /// The module for a source struct name at an exact parameter binding.
    pub fn resolve(&self, source_name: &str, params: &[(&str, u64)]) -> Option<&Module> {
        self.modules.iter().find(|module| {
            module.source_name == source_name
                && module.params.len() == params.len()
                && params.iter().all(|(name, value)| {
                    module
                        .params
                        .iter()
                        .any(|p| p.name == *name && p.value == *value)
                })
        })
    }

    /// Every module that instantiates `verilog_name`.
    pub fn instantiators(&self, verilog_name: &str) -> impl Iterator<Item = &Module> {
        let wanted = verilog_name.to_string();
        self.modules.iter().filter(move |module| {
            module
                .instances
                .iter()
                .any(|instance| instance.module_verilog_name == wanted)
        })
    }

    /// Non-fatal diagnostics found while lowering.
    pub fn warnings(&self) -> &[crate::frontend::Diagnostic] {
        &self.warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> Ty {
        let fields = vec![
            TyField {
                name: "op".into(),
                ty: Ty::UInt(4),
                offset: 40,
                width: 4,
            },
            TyField {
                name: "addr".into(),
                ty: Ty::UInt(32),
                offset: 8,
                width: 32,
            },
            TyField {
                name: "flag".into(),
                ty: Ty::Bool,
                offset: 7,
                width: 1,
            },
            TyField {
                name: "tag".into(),
                ty: Ty::UInt(7),
                offset: 0,
                width: 7,
            },
        ];
        Ty::Struct(StructTy {
            name: "Packet".into(),
            total: 44,
            fields,
        })
    }

    #[test]
    fn struct_fields_pack_high_to_low_without_padding() {
        // AC-MVP-015: 4 + 32 + 1 + 7 = 44 bits, no padding.
        assert_eq!(packet().width(), 44);
        let op = packet().field("op").unwrap().clone();
        assert_eq!(op.range(), (40, 44));
        let tag = packet().field("tag").unwrap().clone();
        assert_eq!(tag.range(), (0, 7));
    }

    #[test]
    fn value_type_conversion_keeps_field_order() {
        let value = packet().to_value_type();
        match value {
            ValueType::Struct { name, fields } => {
                assert_eq!(name, "Packet");
                assert_eq!(fields.len(), 4);
                assert_eq!(fields[0].name, "op");
                assert_eq!(value_type_width(&fields[0].ty), 4);
            }
            other => panic!("unexpected value type: {other:?}"),
        }
    }

    fn value_type_width(ty: &ValueType) -> u32 {
        match ty {
            ValueType::UInt { width } => *width,
            other => other.bit_width().unwrap(),
        }
    }

    #[test]
    fn binary_operators_have_verilog_spelling() {
        assert_eq!(BinOp::Le.verilog(), "<=");
        assert_eq!(UnOp::BitNot.verilog(), "~");
        assert_eq!(Edge::Negedge.verilog(), "negedge");
    }
}
