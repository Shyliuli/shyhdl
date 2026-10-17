//! Statement lowering and testbench assembly.
//!
//! Everything a test writes is lowered here into plain Verilog-2005: `initial`
//! and `always` processes, delays, event and condition waits, blocking and
//! non-blocking assignments, assertions and the clock/reset wrappers. Time
//! never advances on the Rust side (REQ-SIM-002): the generated testbench uses
//! `#delay` and edge waits, and the simulator owns the event queue.

use std::collections::{BTreeMap, HashMap, HashSet};

use proc_macro2::{Span, TokenStream};
use syn::parse::{Parse, ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprCall, ExprIf, ExprRange, Ident, Lit, Macro, Pat, Stmt, Token};

use crate::error::{Error, Result};
use crate::metadata::{SourceSpan, ValueType};
use crate::pattern::{self, Alternative, ConstResolver, Coverage, Missing, PatError, Test};

use super::expr::{contains_enum, Driver, Kind, Lowered, ValueRef, WidthContext};
use super::{DesignInfo, ModulePort, TimeScale};

/// Names of the generated result-protocol counters.
const CASE_COUNTER: &str = "__shyhdl_case";
const PASS_COUNTER: &str = "__shyhdl_pass";
const FAIL_COUNTER: &str = "__shyhdl_fail";
const FAILED_FLAG: &str = "__shyhdl_failed";
const BLOCKS_DONE: &str = "__shyhdl_blocks_done";

/// Where a lowered statement goes; statements outside a process are rejected.
#[derive(Debug, Clone, Copy)]
pub(super) struct Ctx {
    pub procedural: bool,
}

const TOP_LEVEL: Ctx = Ctx { procedural: false };
const PROCEDURAL: Ctx = Ctx { procedural: true };

/// A name a test can use, and what it is in the generated testbench.
pub(super) enum Name {
    /// `let mut x: uint<N>` testbench local.
    Local { net: String, ty: ValueType },
    /// `for i in 0..N` loop variable.
    LoopVar { net: String },
    /// A DUT instance created by `Module::new()`.
    Instance { index: usize },
}

/// One port net of one DUT instance.
pub(super) struct PortBinding {
    pub port: ModulePort,
    pub net: String,
}

/// One instantiated DUT.
pub(super) struct InstanceInfo {
    pub var: String,
    pub inst_name: String,
    pub module: String,
    pub module_source_name: String,
    pub ports: Vec<PortBinding>,
}

/// Indentation-aware line buffer.
pub(super) struct Emitter {
    lines: Vec<String>,
    indent: usize,
}

impl Emitter {
    fn new(indent: usize) -> Self {
        Self {
            lines: Vec::new(),
            indent,
        }
    }

    fn push(&mut self, text: impl AsRef<str>) {
        let pad = "  ".repeat(self.indent);
        self.lines.push(format!("{pad}{}", text.as_ref()));
    }

    fn open(&mut self, header: impl AsRef<str>) {
        self.push(header);
        self.indent += 1;
    }

    fn close(&mut self, tail: &str) {
        self.indent -= 1;
        self.push(tail);
    }

    fn text(&self) -> String {
        let mut out = self.lines.join("\n");
        out.push('\n');
        out
    }
}

/// The generator state for one test function.
pub(super) struct Generator<'a> {
    source_name: String,
    source_span: SourceSpan,
    design: &'a DesignInfo,
    test: String,
    top: String,
    timeunit: TimeScale,
    precision: TimeScale,
    /// Decimal digits needed to write a precision tick in time units.
    fraction_digits: u32,
    instances: Vec<InstanceInfo>,
    names: HashMap<String, Name>,
    /// Module-level net declarations, in source order.
    declarations: Vec<String>,
    loop_vars: Vec<String>,
    procs: Vec<String>,
    clock_nets: HashSet<String>,
    reset_nets: HashSet<String>,
    expected_passes: Option<u64>,
    has_explicit_finish: bool,
    initial_blocks: u64,
    /// Generated clocks, as `(test variable, full waveform path)`.
    clocks: Vec<(String, String)>,
    /// Number of `match` expressions lowered so far, which is what makes the
    /// hidden nets of one match distinct from another's.
    match_counter: u32,
    /// Nets the test itself assigns to, with the location of the first
    /// assignment, so a clock/reset wrapper declared later still conflicts.
    /// Ordered by net, so a test with several conflicts always reports the same
    /// one.
    explicit_drives: BTreeMap<String, Span>,
}

/// A resolved clock argument: the net, and the identity of its recording.
struct ClockTarget {
    value: ValueRef,
    var: String,
    instance: String,
    port: String,
}

/// The value a `match` tests: the net its pattern tests select bits of, the type
/// those bits mean, and the procedural copy into a hidden net when the source
/// expression was not a whole signal.
struct Scrutinee {
    text: String,
    ty: ValueType,
    width: u32,
    /// `Some("net = value;")` for a resolved expression, `None` for a whole signal.
    setup: Option<String>,
}

/// One binding of one `match` arm: the name the guard and the body use, and the
/// hidden net that holds the bits its pattern binds.
struct MatchBinding {
    name: String,
    net: String,
    ty: ValueType,
}

/// One arm of a `match`, ready to emit: the source arm with its guard and body,
/// its alternatives paired with the test each one names, and its binding nets.
struct MatchArm<'a> {
    arm: &'a syn::Arm,
    pairs: &'a [(Alternative, Test)],
    bindings: Vec<MatchBinding>,
}

/// A `match` resolved for emission: the value it tests, and its arms' alternatives
/// with the test each one names.
struct PreparedMatch<'e> {
    item: &'e syn::ExprMatch,
    index: u32,
    scrutinee: Scrutinee,
    arms: Vec<Vec<(Alternative, Test)>>,
}

/// Where a `match` arm's body goes.
enum MatchSink {
    /// Statement position: the arm runs statements, and a value it produces is
    /// thrown away.
    Statement,
    /// Value position: the arm's value is stored into this net, at this type.
    Value { net: String, ty: ValueType },
}

impl<'a> Generator<'a> {
    pub(super) fn new(
        source_name: &str,
        design: &'a DesignInfo,
        test: &str,
        timeunit: TimeScale,
        precision: TimeScale,
    ) -> Result<Self> {
        let unit = timeunit.femtoseconds();
        let precision_fs = precision.femtoseconds();
        if precision_fs == 0 || !unit.is_multiple_of(precision_fs) {
            return Err(Error::TypeLayout {
                message: format!(
                    "timeunit {timeunit} is not an exact multiple of precision {precision}"
                ),
                span: None,
            });
        }
        let mut remaining = unit / precision_fs;
        let mut fraction_digits = 0u32;
        while remaining > 1 && remaining.is_multiple_of(10) {
            remaining /= 10;
            fraction_digits += 1;
        }
        if remaining != 1 {
            return Err(Error::TypeLayout {
                message: format!(
                    "timeunit {timeunit} and precision {precision} differ by a factor that is not a \
                     power of ten, so decimal delays would not be exact"
                ),
                span: None,
            });
        }
        Ok(Self {
            source_name: source_name.to_string(),
            source_span: SourceSpan::new(source_name, 1, 1),
            design,
            test: test.to_string(),
            top: String::new(),
            timeunit,
            precision,
            fraction_digits,
            instances: Vec::new(),
            names: HashMap::new(),
            declarations: Vec::new(),
            loop_vars: Vec::new(),
            procs: Vec::new(),
            clock_nets: HashSet::new(),
            reset_nets: HashSet::new(),
            expected_passes: Some(0),
            has_explicit_finish: false,
            initial_blocks: 0,
            clocks: Vec::new(),
            match_counter: 0,
            explicit_drives: BTreeMap::new(),
        })
    }

    /// Verilog module name of the generated testbench.
    pub(super) fn top(&self) -> &str {
        &self.top
    }

    /// Statically reachable `pass!()` marker count, if it is definite.
    pub(super) fn expected_passes(&self) -> Option<u64> {
        self.expected_passes
    }

    /// The clocks the test generates, in declaration order.
    pub(super) fn clocks(&self) -> Vec<super::GeneratedClock> {
        self.clocks
            .iter()
            .map(|(var, signal)| super::GeneratedClock {
                var: var.clone(),
                signal: signal.clone(),
            })
            .collect()
    }

    /// The DUT instances the test creates, with the waveform scope of each.
    pub(super) fn dut_instances(&self) -> Vec<super::DutInstance> {
        self.instances
            .iter()
            .map(|instance| super::DutInstance {
                var: instance.var.clone(),
                instance: instance.inst_name.clone(),
                module: instance.module.clone(),
                module_source_name: instance.module_source_name.clone(),
                scope: format!("TOP.{}.{}", self.top, instance.inst_name),
                ports: instance
                    .ports
                    .iter()
                    .map(|binding| super::DutPortBinding {
                        source_name: binding.port.source_name.clone(),
                        verilog_name: binding.port.verilog_name.clone(),
                        net: binding.net.clone(),
                        direction: binding.port.direction,
                        width: binding.port.width().unwrap_or(0),
                    })
                    .collect(),
            })
            .collect()
    }

    pub(super) fn name(&self, name: &str) -> Option<&Name> {
        self.names.get(name)
    }

    pub(super) fn instance(&self, index: usize) -> &InstanceInfo {
        &self.instances[index]
    }

    pub(super) fn is_clock_net(&self, net: &str) -> bool {
        self.clock_nets.contains(net)
    }

    pub(super) fn is_reset_net(&self, net: &str) -> bool {
        self.reset_nets.contains(net)
    }

    /// A source-located "this is not supported" error (REQ-VER-007).
    pub(super) fn error(&self, span: Span, message: impl Into<String>) -> Error {
        let mut message = message.into();
        let span = match SourceSpan::from_proc_macro2(&self.source_name, span) {
            Ok(span) => span,
            Err(_) => {
                message.push_str(
                    " (source location unavailable: parse without proc-macro2 span-locations)",
                );
                SourceSpan::new(&self.source_name, 1, 1)
            }
        };
        Error::Unsupported { span, message }
    }

    /// Lower the whole test function and render the testbench module.
    pub(super) fn generate(&mut self, item: &syn::ItemFn) -> Result<String> {
        self.source_span = SourceSpan::from_proc_macro2(&self.source_name, item.sig.ident.span())
            .unwrap_or_else(|_| SourceSpan::new(&self.source_name, 1, 1));
        self.top = format!(
            "tb_{}_{}",
            sanitize_identifier(&self.design.top),
            sanitize_identifier(&self.test)
        );
        if item
            .sig
            .inputs
            .iter()
            .any(|arg| !matches!(arg, syn::FnArg::Receiver(_)))
        {
            return Err(self.error(
                item.sig.span(),
                "a `#[hardware_test]` function takes no arguments",
            ));
        }
        if !item.sig.generics.params.is_empty() {
            return Err(self.error(
                item.sig.generics.span(),
                "a `#[hardware_test]` function cannot be generic: the test is expanded as written",
            ));
        }
        if let syn::ReturnType::Type(_, ty) = &item.sig.output {
            return Err(self.error(ty.span(), "a `#[hardware_test]` function returns nothing"));
        }
        let mut analysis = Analysis {
            passes: Some(0),
            has_finish: false,
        };
        analyze_block(&self.source_name, &item.block.stmts, Some(1), &mut analysis)?;
        self.has_explicit_finish = analysis.has_finish;
        self.expected_passes = if analysis.has_finish {
            None
        } else {
            analysis.passes
        };

        let mut emitter = Emitter::new(0);
        self.lower_discarded_block(&item.block.stmts, TOP_LEVEL, &mut emitter)?;
        if self.initial_blocks == 0 && !self.has_explicit_finish {
            // Nothing can ever report completion: say so in the generated file
            // instead of letting the run look successful.
            self.procs.push(
                "// No `initial!` block and no `finish!()`: this test can never report a PASS, so\n\
                 // the run is reported as a timeout rather than inferred as success."
                    .to_string(),
            );
        } else if self.initial_blocks > 0 && !self.has_explicit_finish {
            self.emit_completion_watcher();
        }
        self.check_driver_conflicts()?;
        Ok(self.render())
    }

    // ---------------------------------------------------------------- statements

    fn lower_stmt(&mut self, stmt: &Stmt, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        match stmt {
            Stmt::Local(local) => self.lower_let(local, ctx, out),
            Stmt::Macro(item) => self.lower_macro(&item.mac, ctx, out),
            Stmt::Expr(expr, _) => self.lower_expr_stmt(expr, ctx, out),
            other => Err(self.error(
                other.span(),
                "unsupported statement in a testbench; supported statements are `let` bindings, \
                 assignments, `nb_assign!(..)`, `if`, `for`, `match`, `assert!`, `assert_eq!`, \
                 `pass!()`, `finish!()`, `timeout!(..)`, `delay(..)`, `wait(..)`, \
                 `wait_until(..)`, `sample_after(..)`, `gen_clk(..)`, `initial! { .. }`, \
                 `always! { .. }`, `clock!(..)` and `reset!(..)`",
            )),
        }
    }

    fn lower_expr_stmt(&mut self, expr: &Expr, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        match expr {
            Expr::Macro(item) => self.lower_macro(&item.mac, ctx, out),
            Expr::Assign(assign) => {
                self.lower_assign(&assign.left, &assign.right, assign.span(), ctx, out)
            }
            Expr::If(item) => self.lower_if(item, ctx, out),
            Expr::ForLoop(item) => self.lower_for(item, ctx, out),
            Expr::Match(item) => self.lower_match(item, ctx, out),
            Expr::Block(block) => self.lower_discarded_block(&block.block.stmts, ctx, out),
            Expr::Call(call) => self.lower_call_stmt(call, ctx, out),
            Expr::MethodCall(call) => Err(self.error(
                call.span(),
                "method calls are not supported in a testbench; drive DUT inputs with \
                 `dut.port = value` and read outputs with `dut.port`",
            )),
            other => Err(self.error(
                other.span(),
                format!(
                    "unsupported statement `{}` in a testbench; a statement must be an assignment \
                     or one of the supported wrappers",
                    render_expr(other)
                ),
            )),
        }
    }

    /// `let mut x = Module::new();` (an instance) or a testbench local.
    fn lower_let(&mut self, local: &syn::Local, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        // `let x: uint<8> = ..` is a typed pattern in Rust syntax, so the type
        // annotation is read from the pattern: `syn::Local` has no type field.
        let (ident, annotation) = match &local.pat {
            Pat::Ident(pattern) if pattern.subpat.is_none() => (pattern.ident.clone(), None),
            Pat::Type(typed) => match typed.pat.as_ref() {
                Pat::Ident(pattern) if pattern.subpat.is_none() => {
                    (pattern.ident.clone(), Some(typed.ty.as_ref()))
                }
                other => return Err(self.error(
                    other.span(),
                    "only `let mut name = ...` and `let mut name: uint<N>;` are supported in a \
                         testbench",
                )),
            },
            other => {
                return Err(self.error(
                    other.span(),
                    "only `let mut name = ...` and `let mut name: uint<N>;` are supported in a \
                     testbench",
                ))
            }
        };
        let var = ident.to_string();
        let init = match &local.init {
            Some(init) => {
                if init.diverge.is_some() {
                    return Err(self.error(
                        init.expr.span(),
                        "`let ... else` is not supported in a testbench",
                    ));
                }
                Some(init.expr.as_ref())
            }
            None => None,
        };

        if let Some(expr) = init {
            if let Some((module, span)) = module_new_call(expr) {
                if ctx.procedural {
                    return Err(self.error(
                        span,
                        "a DUT is declared at the top of the test, not inside `initial!`/`always!`",
                    ));
                }
                return self.declare_instance(&var, &module, span);
            }
        }

        // The type is written on the `let`, or stated by the initialiser itself: a
        // suffixed literal, a cast, a variant, a signal, or the values of a
        // `match`/`if` that agree. A value that names no type of its own is still
        // refused instead of guessed from a width, so `let mut x = 1;` keeps asking
        // for the annotation it asked for before.
        let ty = match annotation {
            Some(ty) => self.parse_local_type(ty)?,
            None => match init {
                Some(expr) => self
                    .value_type_of(expr)?
                    .ok_or_else(|| self.local_needs_type(&var, local.span()))?,
                None => return Err(self.local_needs_type(&var, local.span())),
            },
        };
        // The initialiser is a store into this local, so it follows the same rules
        // as any other store: an array literal is built from the local's shape, and
        // an aggregate keeps its exact shape. A `match`, an `if` or a block in it
        // becomes statements, so a local declared at the top of the test gets one
        // `initial` block that holds those statements and the assignment together.
        let value = match init {
            Some(expr) if ctx.procedural => Some(self.lower_value(expr, &ty, out)?),
            Some(expr) if needs_statements(expr) => {
                let mut block = Emitter::new(0);
                block.open("initial begin");
                let value = self.lower_value(expr, &ty, &mut block)?;
                block.push(format!("{var} = {};", value.text));
                block.close("end");
                self.procs.push(block.text().trim_end().to_string());
                None
            }
            Some(expr) => Some(self.lower_stored_value(expr, &ty, expr.span())?),
            None => None,
        };
        self.declare_local(&var, ty, local.span())?;
        if let Some(value) = value {
            if ctx.procedural {
                out.push(format!("{var} = {};", value.text));
            } else {
                // A local initialised at the top of the test gets its own
                // `initial` block so the assignment happens in a process.
                let mut block = Emitter::new(0);
                block.open("initial begin");
                block.push(format!("{var} = {};", value.text));
                block.close("end");
                self.procs.push(block.text().trim_end().to_string());
            }
        }
        Ok(())
    }

    pub(super) fn parse_local_type(&self, ty: &syn::Type) -> Result<ValueType> {
        // `[T; N]`: a fixed-length array of a fixed-width element type.
        if let syn::Type::Array(array) = ty {
            let element = self.parse_local_type(&array.elem)?;
            let len = match &array.len {
                Expr::Lit(syn::ExprLit {
                    lit: Lit::Int(value),
                    ..
                }) => value.base10_parse::<usize>().map_err(|_| {
                    self.error(
                        array.len.span(),
                        "the array length must be a positive integer literal",
                    )
                })?,
                other => {
                    return Err(
                        self.error(other.span(), "the array length must be an integer literal")
                    )
                }
            };
            if len == 0 {
                return Err(self.error(array.len.span(), "an array of zero elements has no bits"));
            }
            return Ok(ValueType::Array {
                element: Box::new(element),
                len,
            });
        }
        let syn::Type::Path(path) = ty else {
            return Err(self.error(
                ty.span(),
                "unsupported type in a testbench; use `bool`, `uint<N>`, `int<N>` or `[T; N]`",
            ));
        };
        let Some(segment) = path.path.segments.last() else {
            return Err(self.error(ty.span(), "unsupported type in a testbench"));
        };
        let base = segment.ident.to_string();
        match base.as_str() {
            "bool" => Ok(ValueType::Bool),
            "uint" | "int" => {
                let width = generic_width(segment).ok_or_else(|| {
                    self.error(
                        ty.span(),
                        format!(
                            "`{base}<N>` needs an integer literal width, for example `{base}<8>`"
                        ),
                    )
                })?;
                if width == 0 {
                    return Err(self.error(ty.span(), "a zero width signal is not valid"));
                }
                Ok(if base == "uint" {
                    ValueType::UInt { width }
                } else {
                    ValueType::SInt { width }
                })
            }
            // An enum of the design: its variants and its layout are the
            // metadata's, so a local can hold one without inventing a type.
            other => self.design_enum_type(other, segment, ty.span()),
        }
    }

    /// The enum of the design a written type name means.
    ///
    /// The design's own port types are the only authority a testbench has: an enum
    /// is not a type the testbench could build. A written parameter list names the
    /// payload shape (`Option<uint<4>>`), because the metadata name of a builtin is
    /// rooted at `Option`/`Result` and the payload is what tells two of them apart;
    /// a bare name has to be unique among the design's enums.
    fn design_enum_type(
        &self,
        head: &str,
        segment: &syn::PathSegment,
        span: Span,
    ) -> Result<ValueType> {
        let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
            return self.design_enum_named(head, span);
        };
        let mut args = Vec::with_capacity(arguments.args.len());
        for argument in &arguments.args {
            let syn::GenericArgument::Type(parameter) = argument else {
                return Err(self.error(
                    span,
                    format!("the parameters of `{head}<...>` must be types"),
                ));
            };
            args.push(self.parse_local_type(parameter)?);
        }
        let written = format!(
            "{head}<{}>",
            args.iter()
                .map(describe_type)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut candidates = self.design_enums();
        candidates.retain(|ty| enum_head(ty) == head && enum_payloads(ty) == args);
        match candidates.as_slice() {
            [ty] => Ok(ty.clone()),
            [] => Err(self.error(
                span,
                format!(
                    "the design declares no enum `{written}`; match a port or a field whose type \
                     the design states"
                ),
            )),
            many => Err(self.error(
                span,
                format!(
                    "`{written}` matches more than one enum of the design ({})",
                    enum_names(many).join(", ")
                ),
            )),
        }
    }

    /// The enum of the design a bare type name means.
    ///
    /// An exact name wins over the parameterised spelling of a builtin, and several
    /// matches are reported instead of guessed.
    fn design_enum_named(&self, written: &str, span: Span) -> Result<ValueType> {
        let enums = self.design_enums();
        let exact: Vec<ValueType> = enums
            .iter()
            .filter(|ty| enum_name(ty) == written)
            .cloned()
            .collect();
        let head = format!("{written}<");
        let candidates: Vec<ValueType> = if exact.is_empty() {
            enums
                .into_iter()
                .filter(|ty| enum_name(ty).starts_with(&head))
                .collect()
        } else {
            exact
        };
        match candidates.as_slice() {
            [ty] => Ok(ty.clone()),
            [] => Err(self.error(
                span,
                format!(
                    "unsupported testbench type `{written}`; use `bool`, `uint<N>`, `int<N>`, \
                     `[T; N]`, or the name of an enum the design declares. A struct-typed value \
                     has to come from a DUT port or a struct field, because its layout is part of \
                     the design"
                ),
            )),
            many => Err(self.error(
                span,
                format!(
                    "the type name `{written}` matches more than one enum of the design ({}); \
                     write its parameters, for example `{written}<...>`",
                    enum_names(many).join(", ")
                ),
            )),
        }
    }

    /// Every enum type the design's ports contain, without duplicates.
    fn design_enums(&self) -> Vec<ValueType> {
        let mut enums = Vec::new();
        for port in &self.design.ports {
            collect_enums(&port.ty, &mut enums);
        }
        enums
    }

    /// An aggregate is assigned only from a value of exactly its shape, or from an
    /// explicit zero fill.
    ///
    /// A scalar literal or a same-width value of another shape is refused instead
    /// of being reinterpreted as bits, and a value with no shape of its own (a
    /// computed scalar) is refused too, so nothing slips through as `None`.
    fn check_aggregate_shape(&self, target: &ValueType, value: &Lowered, span: Span) -> Result<()> {
        let value_has_array = value.ty.as_ref().is_some_and(contains_array);
        if !contains_array(target) && !value_has_array {
            // A plain struct of scalars: unchanged from before arrays landed.
            return Ok(());
        }
        match &value.ty {
            Some(value_ty) if value_ty == target => Ok(()),
            Some(value_ty) => Err(self.error(
                span,
                format!(
                    "these have a different shape: `{}` and `{}`; an array-shaped copy must match \
                     exactly",
                    describe_type(value_ty),
                    describe_type(target)
                ),
            )),
            // A shaped literal (`[..]`, `[v; N]`) carries its type, so reaching here
            // means the source wrote something without a shape: a scalar literal or
            // a scalar expression. Neither may stand in for an array-shaped value.
            None => Err(self.error(
                span,
                format!(
                    "an array-shaped target `{}` takes a shaped value, not a scalar; write the \
                     elements, for example `[[0; 2]; 2]`, or assign a value of the same shape",
                    describe_type(target)
                ),
            )),
        }
    }

    /// Lower the value stored into a target of this type.
    ///
    /// An array literal is built from the target's shape, and every other value
    /// must have exactly that shape when the target is an aggregate; a scalar
    /// target keeps its existing width and signedness rules.
    pub(super) fn lower_stored_value(
        &self,
        expr: &Expr,
        ty: &ValueType,
        span: Span,
    ) -> Result<Lowered> {
        let value = match expr {
            Expr::Array(_) | Expr::Repeat(_) => {
                if !matches!(ty, ValueType::Array { .. }) {
                    return Err(self.error(
                        span,
                        "an array literal needs an array target; declare the value with its \
                         shape, for example `let mut a: [uint<4>; 3] = [1, 2, 3];`",
                    ));
                }
                self.lower_array_literal(expr, ty, span)?
            }
            // An enum is built from a variant (its tag is part of the type), so a
            // construction takes its own path rather than a width context. A value
            // that already has the enum's type (a port, a field, a local) is read
            // like any other value: it is not rebuilt.
            _ if matches!(ty, ValueType::Enum { .. }) => {
                if self.needs_target_type(expr)? {
                    self.lower_enum_value(expr, ty, span)?
                } else {
                    let value = self.lower_expr(expr, None, span)?;
                    self.check_aggregate_shape(ty, &value, span)?;
                    value
                }
            }
            _ => {
                let context = context_of(ty.bit_width()?, ty);
                self.lower_expr(expr, Some(context), span)?
            }
        };
        // Either side being an aggregate makes this a shape question, not a width
        // question: an array may not slide into an integer of the same width.
        self.check_aggregate_shape(ty, &value, span)?;
        Ok(value)
    }

    /// Build an array literal for a target of exactly this shape.
    ///
    /// Element 0 is the least significant, so a list is concatenated in reverse
    /// (`[1, 2, 3]` is `{3, 2, 1}`) and `[v; N]` is a Verilog replication.
    fn lower_array_literal(&self, expr: &Expr, ty: &ValueType, span: Span) -> Result<Lowered> {
        let ValueType::Array { element, len } = ty else {
            return Err(self.error(span, "an array literal needs an array target"));
        };
        let total = ty.bit_width()?;
        let text = match expr {
            Expr::Repeat(repeat) => {
                let count = match repeat.len.as_ref() {
                    Expr::Lit(syn::ExprLit {
                        lit: Lit::Int(value),
                        ..
                    }) => value.base10_parse::<usize>().ok(),
                    _ => None,
                };
                let Some(count) = count else {
                    return Err(self.error(
                        repeat.len.span(),
                        "the repeat count must be an integer literal",
                    ));
                };
                if count != *len {
                    return Err(self.error(
                        repeat.len.span(),
                        format!(
                            "this repeats {count} element(s) but the target holds {len}; a literal \
                             must have exactly the target's shape"
                        ),
                    ));
                }
                let value = self.lower_element(&repeat.expr, element, repeat.expr.span())?;
                format!("{{{} {{{}}}}}", len, value.text)
            }
            Expr::Array(list) => {
                if list.elems.len() != *len {
                    return Err(self.error(
                        list.span(),
                        format!(
                            "this list has {} element(s) but the target holds {len}; a literal must \
                             have exactly the target's shape",
                            list.elems.len()
                        ),
                    ));
                }
                let mut parts = Vec::with_capacity(list.elems.len());
                for item in list.elems.iter().rev() {
                    parts.push(self.lower_element(item, element, item.span())?.text);
                }
                format!("{{{}}}", parts.join(", "))
            }
            other => {
                return Err(self.error(other.span(), "expected an array literal or a repeat"));
            }
        };
        Ok(Lowered::typed(text, ty.clone(), total, Kind::Struct))
    }

    /// One element of an array literal, shaped by the element's type.
    fn lower_element(&self, expr: &Expr, element: &ValueType, span: Span) -> Result<Lowered> {
        let value = match expr {
            Expr::Array(_) | Expr::Repeat(_) => self.lower_array_literal(expr, element, span)?,
            _ => {
                let context = context_of(element.bit_width()?, element);
                self.lower_expr(expr, Some(context), span)?
            }
        };
        // A nested element is a copy too: a same-width value of another shape must
        // not slip in where the element's shape is known.
        self.check_aggregate_shape(element, &value, span)?;
        Ok(value)
    }

    fn declare_local(&mut self, name: &str, ty: ValueType, span: Span) -> Result<()> {
        self.check_free_name(name, span)?;
        let width = ty.bit_width()?;
        let signed = matches!(ty, ValueType::SInt { .. });
        self.declarations
            .push(signal_declaration("reg", width, signed, name));
        self.names.insert(
            name.to_string(),
            Name::Local {
                net: name.to_string(),
                ty,
            },
        );
        Ok(())
    }

    fn declare_instance(&mut self, var: &str, module: &Ident, span: Span) -> Result<()> {
        let module_name = module.to_string();
        if module_name != self.design.top_source_name && module_name != self.design.top {
            return Err(self.error(
                span,
                format!(
                    "this test instantiates `{module_name}` but the design under test is `{}`; a \
                     testbench drives the DUT top module",
                    self.design.top_source_name
                ),
            ));
        }
        self.check_free_name(var, span)?;
        let inst_name = format!("u_{var}");
        let mut bindings = Vec::with_capacity(self.design.ports.len());
        for port in &self.design.ports {
            let net = format!("{var}_{}", port.verilog_name);
            self.check_free_name(&net, span)?;
            let width = port.width()?;
            let signed = matches!(port.ty, ValueType::SInt { .. });
            let keyword = if port.direction.is_driven_by_test() {
                "reg"
            } else {
                "wire"
            };
            self.declarations
                .push(signal_declaration(keyword, width, signed, &net));
            bindings.push(PortBinding {
                port: port.clone(),
                net,
            });
        }
        let index = self.instances.len();
        self.instances.push(InstanceInfo {
            var: var.to_string(),
            inst_name,
            module: self.design.top.clone(),
            module_source_name: self.design.top_source_name.clone(),
            ports: bindings,
        });
        self.names.insert(var.to_string(), Name::Instance { index });
        Ok(())
    }

    fn check_free_name(&self, name: &str, span: Span) -> Result<()> {
        if !is_verilog_identifier(name) {
            return Err(self.error(
                span,
                format!("`{name}` is not a valid Verilog identifier (letters, digits, `_`)"),
            ));
        }
        if is_verilog_keyword(name) || name.starts_with("__shyhdl") {
            return Err(self.error(
                span,
                format!("`{name}` is a reserved word and cannot be used as a signal name"),
            ));
        }
        if self.names.contains_key(name)
            || self
                .declarations
                .iter()
                .any(|declaration| declaration.ends_with(&format!(" {name};")))
        {
            return Err(self.error(span, format!("`{name}` is declared more than once")));
        }
        Ok(())
    }

    fn lower_assign(
        &mut self,
        left: &Expr,
        right: &Expr,
        span: Span,
        ctx: Ctx,
        out: &mut Emitter,
    ) -> Result<()> {
        self.require_procedural(ctx, span, "an assignment")?;
        let target = self.write_target(left)?;
        // The value is lowered where it stands, so a `match`/`if` it contains runs
        // before the store and never as a ternary on the right of `=`.
        let value = self.lower_value(right, &target.ty, out)?;
        self.comment_at(out, span)?;
        let statement = format!("{} = {};", target.text, value.text);
        match &target.guard {
            // The guard belongs to the statement, never to the lvalue: a ternary
            // cannot be assigned to.
            Some(guard) => out.push(format!("if ({guard}) {statement}")),
            None => out.push(statement),
        }
        Ok(())
    }

    /// Resolve an assignment target, rejecting signals the test does not drive.
    fn write_target(&mut self, expr: &Expr) -> Result<ValueRef> {
        let target = self.try_value_ref(expr)?.ok_or_else(|| {
            self.error(
                expr.span(),
                format!(
                    "`{}` cannot be assigned; use a testbench local or a DUT input",
                    render_expr(expr)
                ),
            )
        })?;
        match target.driver {
            Driver::Test => {
                // Remembered so a clock/reset wrapper declared *later* in the test
                // still conflicts: one signal has exactly one driver. The key is the
                // net, so writing one element of an array still names its signal.
                let net = target
                    .text
                    .split('[')
                    .next()
                    .unwrap_or(&target.text)
                    .to_string();
                self.explicit_drives
                    .entry(net)
                    .or_insert_with(|| expr.span());
                Ok(target)
            }
            other => Err(self.error(
                expr.span(),
                format!(
                    "cannot assign to `{}`: it is {}",
                    target.text,
                    other.describe()
                ),
            )),
        }
    }

    /// A signal has exactly one driver: a net that a wrapper generates a clock or
    /// a reset on cannot also be assigned by the test, no matter which of the two
    /// comes first in the source.
    fn check_driver_conflicts(&self) -> Result<()> {
        for (net, span) in &self.explicit_drives {
            let wrapper = if self.clock_nets.contains(net) {
                Some("a clock wrapper")
            } else if self.reset_nets.contains(net) {
                Some("a reset wrapper")
            } else {
                None
            };
            if let Some(wrapper) = wrapper {
                return Err(self.error(
                    *span,
                    format!(
                        "`{net}` is already driven by {wrapper}, so the test cannot assign to it; \
                         remove this assignment and let the wrapper drive the signal"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `if` / `else if` / `else`, lowered as one clause chain.
    ///
    /// A condition that needs statements of its own (a `match`/`if` value inside it)
    /// cannot be written between `end` and `else if`, so the rest of the chain goes
    /// into an `else begin .. end` block instead: those statements then run only
    /// when the branch before them was not taken.
    fn lower_if(&mut self, item: &ExprIf, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        self.require_procedural(ctx, item.span(), "`if`")?;
        let mut clauses: Vec<(Option<Expr>, Vec<Stmt>)> = Vec::new();
        let mut current = item.clone();
        loop {
            clauses.push((
                Some((*current.cond).clone()),
                current.then_branch.stmts.clone(),
            ));
            match current
                .else_branch
                .as_ref()
                .map(|(_, branch)| branch.as_ref().clone())
            {
                None => break,
                Some(Expr::If(nested)) => current = nested,
                Some(Expr::Block(block)) => {
                    clauses.push((None, block.block.stmts.clone()));
                    break;
                }
                Some(other) => {
                    clauses.push((None, vec![Stmt::Expr(other, None)]));
                    break;
                }
            }
        }
        self.comment_at(out, item.span())?;
        let conditions_need_statements = clauses
            .iter()
            .any(|(condition, _)| condition.as_ref().is_some_and(needs_statements));
        if conditions_need_statements {
            return self.emit_if_chain(&clauses, 0, ctx, out);
        }
        let mut lowered: Vec<(Option<Lowered>, Vec<Stmt>)> = Vec::with_capacity(clauses.len());
        for (condition, body) in clauses {
            let condition = match condition {
                Some(condition) => Some(self.lower_condition(&condition, "an `if` condition")?),
                None => None,
            };
            lowered.push((condition, body));
        }
        for (index, (condition, body)) in lowered.iter().enumerate() {
            let header = match (index, condition) {
                (0, Some(condition)) => format!("if ({}) begin", condition.text),
                (_, Some(condition)) => format!("end else if ({}) begin", condition.text),
                (_, None) => "end else begin".to_string(),
            };
            if index == 0 {
                out.open(header);
            } else {
                out.indent -= 1;
                out.open(header);
            }
            self.lower_discarded_block(body, ctx, out)?;
        }
        out.close("end");
        Ok(())
    }

    /// Emit `clauses[index..]` as a chain where every condition is lowered at the
    /// point the source evaluates it.
    fn emit_if_chain(
        &mut self,
        clauses: &[(Option<Expr>, Vec<Stmt>)],
        index: usize,
        ctx: Ctx,
        out: &mut Emitter,
    ) -> Result<()> {
        let (condition, body) = &clauses[index];
        let Some(condition) = condition else {
            return self.lower_discarded_block(body, ctx, out);
        };
        let condition = self.lower_bool_condition(condition, "an `if` condition", out)?;
        out.open(format!("if ({}) begin", condition.text));
        self.lower_discarded_block(body, ctx, out)?;
        out.indent -= 1;
        match clauses.get(index + 1) {
            // The last clause closes the chain.
            None => out.push("end"),
            Some((None, else_body)) => {
                out.open("end else begin");
                self.lower_discarded_block(else_body, ctx, out)?;
                out.close("end");
            }
            Some(_) => {
                out.open("end else begin");
                self.emit_if_chain(clauses, index + 1, ctx, out)?;
                out.close("end");
            }
        }
        Ok(())
    }

    fn lower_for(&mut self, item: &syn::ExprForLoop, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        self.require_procedural(ctx, item.span(), "`for`")?;
        let Pat::Ident(pattern) = item.pat.as_ref() else {
            return Err(self.error(
                item.pat.span(),
                "only `for i in 0..N` and `for i in 0..=N` with integer literal bounds are supported",
            ));
        };
        let var = pattern.ident.to_string();
        let Expr::Range(range) = item.expr.as_ref() else {
            return Err(self.error(
                item.expr.span(),
                "`for` needs a fixed number of iterations written as `0..N` or `0..=N`",
            ));
        };
        let Some((start, end, limits)) = range_parts(range) else {
            return Err(self.error(
                item.expr.span(),
                "`for` bounds must be integer literals so the loop has a fixed trip count \
                 (REQ-LANG-046)",
            ));
        };
        if !self.loop_vars.iter().any(|declared| declared == &var) {
            if self.names.contains_key(&var) {
                return Err(self.error(
                    item.pat.span(),
                    format!(
                        "`{var}` is already in use; a Verilog testbench has one flat name space"
                    ),
                ));
            }
            self.loop_vars.push(var.clone());
            self.names
                .insert(var.clone(), Name::LoopVar { net: var.clone() });
        }
        let comparison = match limits {
            syn::RangeLimits::HalfOpen(_) => "<",
            syn::RangeLimits::Closed(_) => "<=",
        };
        self.comment_at(out, item.span())?;
        out.open(format!(
            "for ({var} = {start}; {var} {comparison} {end}; {var} = {var} + 1) begin"
        ));
        self.lower_discarded_block(&item.body.stmts, ctx, out)?;
        out.close("end");
        Ok(())
    }

    // -------------------------------------------------------------------- match

    /// `match scrutinee { pattern [if guard] => body, ... }` over a typed value.
    ///
    /// The scrutinee is resolved **once**: a whole signal is tested where it
    /// stands, and anything else is copied into a hidden net at the match's entry
    /// with a procedural assignment - never a continuous `assign`, which would
    /// keep following a value that changed after the match. Arms keep their source
    /// order and every arm is emitted. The first pattern in source order that
    /// matches **and** whose guard holds wins; the `taken` flag is what makes a
    /// failed guard leave the match open, first for the other alternatives of the
    /// same arm and then for the arms after it. Exhaustiveness is the shared
    /// pattern analysis over the **unguarded** arms, and a match that is not proven
    /// total is rejected with a source location instead of falling back at run
    /// time.
    fn lower_match(&mut self, item: &syn::ExprMatch, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        self.require_procedural(ctx, item.span(), "`match`")?;
        self.match_counter += 1;
        let index = self.match_counter;
        let plan = self.prepare_match(item, index, out)?;
        self.emit_match_body(&plan, &MatchSink::Statement, out)
    }

    /// Resolve everything a `match` needs before emission: the scrutinee read once,
    /// the normalized patterns with their tests, the bindings and the exhaustiveness
    /// proof, all from the shared analysis.
    ///
    /// The scrutinee is lowered here, so a `match`/`if` inside it becomes statements
    /// that run before the snapshot: the value the arms test is still read once, and
    /// read before the arms run.
    fn prepare_match<'e>(
        &mut self,
        item: &'e syn::ExprMatch,
        index: u32,
        out: &mut Emitter,
    ) -> Result<PreparedMatch<'e>> {
        let (scrutinee_expr, nets) = if needs_statements(&item.expr) {
            self.materialize(&item.expr, None, None, out)?
        } else {
            ((*item.expr).clone(), Vec::new())
        };
        let scrutinee = self.match_scrutinee(&scrutinee_expr, index);
        self.forget_nets(nets);
        let scrutinee = scrutinee?;

        // Every arm is normalized and checked before anything is emitted: the tests,
        // the bindings and the exhaustiveness proof all come from the shared analysis.
        // Each arm keeps its alternatives paired with the test each one names, in
        // source order.
        let mut arms: Vec<Vec<(Alternative, Test)>> = Vec::with_capacity(item.arms.len());
        let mut coverage = Coverage::new(&scrutinee.ty);
        for arm in &item.arms {
            if let Some(attribute) = arm.attrs.first() {
                return Err(self.error(
                    attribute.span(),
                    "attributes on a `match` arm are not supported in a testbench",
                ));
            }
            let lowered = pattern::lower_arm(&arm.pat, &scrutinee.ty, &NoConstants)
                .map_err(|error| self.pattern_error(error.span, error))?;
            let tests = lowered
                .alternatives
                .iter()
                .map(|alternative| pattern::pattern_test(alternative, &scrutinee.ty))
                .collect::<std::result::Result<Vec<Test>, PatError>>()
                .map_err(|error| self.pattern_error(error.span, error))?;
            // A guard can be false for a value its pattern matches, so a guarded
            // arm covers nothing: it can neither make the match total nor shadow a
            // later arm.
            if arm.guard.is_none() {
                coverage
                    .add(&lowered.alternatives)
                    .map_err(|error| self.pattern_error(item.span(), error))?;
            }
            arms.push(lowered.alternatives.into_iter().zip(tests).collect());
        }
        if !coverage
            .is_exhaustive()
            .map_err(|error| self.pattern_error(item.span(), error))?
        {
            let missing = coverage
                .missing(3)
                .map_err(|error| self.pattern_error(item.span(), error))?;
            return Err(self.error(item.span(), describe_missing(&scrutinee.ty, &missing)));
        }
        Ok(PreparedMatch {
            item,
            index,
            scrutinee,
            arms,
        })
    }

    /// Emit the `taken` flag, the scrutinee snapshot and every arm in source order.
    fn emit_match_body(
        &mut self,
        plan: &PreparedMatch<'_>,
        sink: &MatchSink,
        out: &mut Emitter,
    ) -> Result<()> {
        let flag = format!("__shyhdl_match{}_taken", plan.index);
        self.declarations
            .push(signal_declaration("reg", 1, false, &flag));
        self.comment_at(out, plan.item.span())?;
        out.push(format!("{flag} = 1'b0;"));
        if let Some(setup) = &plan.scrutinee.setup {
            out.push(setup.clone());
        }
        for (arm_index, arm) in plan.item.arms.iter().enumerate() {
            let pairs = &plan.arms[arm_index];
            let arm_plan = MatchArm {
                arm,
                pairs,
                bindings: self.declare_arm_bindings(plan.index, arm_index, pairs)?,
            };
            self.comment_at(out, arm_plan.arm.pat.span())?;
            // The bindings are visible to the guard and the body of this arm only.
            let saved = self.push_bindings(&arm_plan.bindings);
            let result = self.emit_match_arm(&arm_plan, &plan.scrutinee, &flag, sink, out);
            self.pop_bindings(saved);
            result?;
        }
        Ok(())
    }

    /// Resolve the value a `match` tests, once, into a net a pattern test can
    /// select bits of.
    fn match_scrutinee(&mut self, expr: &Expr, index: u32) -> Result<Scrutinee> {
        if let Some(value) = self.try_value_ref(expr)? {
            let ty = value.ty.clone();
            let width = value.width;
            // A whole signal is already a plain net: test it where it stands.
            if value.guard.is_none() && is_verilog_identifier(&value.text) {
                return Ok(Scrutinee {
                    text: value.text,
                    ty,
                    width,
                    setup: None,
                });
            }
            // Otherwise read it once. The read keeps its in-range guard (an
            // out-of-range dynamic index reads `x`, never a neighbour), and a plain
            // net is what the later bit selects are legal on.
            let read = self.lower_expr(expr, None, expr.span())?;
            return self.hidden_scrutinee(index, ty, width, read.text);
        }
        // Not a signal: the type has to be nameable, and the value is built once.
        let ty = self.match_expression_type(expr)?;
        let value = self.lower_stored_value(expr, &ty, expr.span())?;
        let width = ty.bit_width()?;
        self.hidden_scrutinee(index, ty, width, value.text)
    }

    /// Copy a resolved scrutinee into a hidden net at the match's entry.
    fn hidden_scrutinee(
        &mut self,
        index: u32,
        ty: ValueType,
        width: u32,
        value: String,
    ) -> Result<Scrutinee> {
        let text = self.declare_hidden_net(index, "scrutinee", &ty)?;
        Ok(Scrutinee {
            setup: Some(format!("{text} = {value};")),
            text,
            ty,
            width,
        })
    }

    /// The type of a scrutinee that is not a signal.
    ///
    /// A variant constructor is resolved among the design's own enum types (the
    /// only authority a testbench has), a cast and a boolean literal name their
    /// type directly. Anything else is refused instead of guessed: a wrong type
    /// would silently test the wrong bits.
    fn match_expression_type(&self, expr: &Expr) -> Result<ValueType> {
        match expr {
            Expr::Paren(paren) => self.match_expression_type(&paren.expr),
            Expr::Group(group) => self.match_expression_type(&group.expr),
            Expr::Cast(cast) => self.parse_local_type(&cast.ty),
            Expr::Lit(literal) => match &literal.lit {
                Lit::Bool(_) => Ok(ValueType::Bool),
                _ => Err(self.unknown_match_type(expr.span())),
            },
            Expr::Path(path) => self.variant_enum_type(&path.path, false, expr.span()),
            Expr::Struct(structure) => self.variant_enum_type(&structure.path, true, expr.span()),
            Expr::Call(call) => match call.func.as_ref() {
                Expr::Path(path) => self.variant_enum_type(&path.path, true, expr.span()),
                _ => Err(self.unknown_match_type(expr.span())),
            },
            other => Err(self.unknown_match_type(other.span())),
        }
    }

    fn unknown_match_type(&self, span: Span) -> Error {
        self.error(
            span,
            "the type of this `match` value cannot be told; match a signal, or write a variant \
             the design declares (for example `Some(..)`)",
        )
    }

    /// The enum that declares `name` as a variant with (or without) a payload.
    ///
    /// A qualified path picks one enum explicitly; an unqualified name has to be
    /// unique among the design's enums, or the source says which one it means.
    fn variant_enum_type(
        &self,
        path: &syn::Path,
        with_payload: bool,
        span: Span,
    ) -> Result<ValueType> {
        let name = path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_default();
        let qualifier = match path.segments.len() {
            1 => None,
            2 => Some(path.segments[0].ident.to_string()),
            count => {
                return Err(self.error(
                    span,
                    format!("a path with {count} segments is not supported here"),
                ))
            }
        };
        let mut candidates = Vec::new();
        for port in &self.design.ports {
            collect_enums(&port.ty, &mut candidates);
        }
        if let Some(qualifier) = &qualifier {
            candidates.retain(|ty| match ty {
                ValueType::Enum { name, .. } => name == qualifier,
                _ => false,
            });
        }
        candidates.retain(|ty| match ty {
            ValueType::Enum { variants, .. } => variants
                .iter()
                .any(|variant| variant.name == name && variant.payload.is_some() == with_payload),
            _ => false,
        });
        match candidates.len() {
            0 => Err(self.error(
                span,
                format!(
                    "no enum in the design declares the variant `{name}`; match a signal whose \
                     type the design states instead"
                ),
            )),
            1 => Ok(candidates.pop().expect("one candidate")),
            _ => {
                let names = enum_names(&candidates);
                let first = names.first().copied().unwrap_or("<enum>");
                Err(self.error(
                    span,
                    format!(
                        "the variant `{name}` belongs to more than one enum of the design \
                         ({}); write the enum's name, for example `{first}::{name}(..)`",
                        names.join(", ")
                    ),
                ))
            }
        }
    }

    /// Declare a hidden net for a resolved `match` value or one of its bindings.
    ///
    /// The `__shyhdl` prefix is refused for a source name, so a hidden net can
    /// never collide with a signal the test declares.
    fn declare_hidden_net(&mut self, index: u32, what: &str, ty: &ValueType) -> Result<String> {
        let name = format!("__shyhdl_match{index}_{what}");
        let width = ty.bit_width()?;
        let signed = matches!(ty, ValueType::SInt { .. });
        self.declarations
            .push(signal_declaration("reg", width, signed, &name));
        Ok(name)
    }

    /// The hidden nets of one arm's bindings.
    ///
    /// The pattern analysis guarantees every `|` alternative of an arm binds the
    /// same names with the same types, so one net per name serves the whole arm;
    /// only the bit offsets differ per alternative, and those are applied where
    /// the alternative assigns.
    fn declare_arm_bindings(
        &mut self,
        index: u32,
        arm: usize,
        pairs: &[(Alternative, Test)],
    ) -> Result<Vec<MatchBinding>> {
        let Some((first, _)) = pairs.first() else {
            return Ok(Vec::new());
        };
        let mut bindings = Vec::with_capacity(first.bindings.len());
        for binding in &first.bindings {
            let what = format!("a{arm}_{}", sanitize_identifier(&binding.name));
            let net = self.declare_hidden_net(index, &what, &binding.ty)?;
            bindings.push(MatchBinding {
                name: binding.name.clone(),
                net,
                ty: binding.ty.clone(),
            });
        }
        Ok(bindings)
    }

    /// The alternatives of one arm in source order, each assigning the bits its own
    /// pattern binds, then the arm's guard and body.
    ///
    /// Every alternative tests the `taken` flag again. A pattern that matches but
    /// whose guard is false leaves the match open: the alternatives of a `|` may
    /// bind the same name to different bits, so the guard is a different question
    /// for each of them, and the arms after this one follow only once no
    /// alternative of this arm has taken the match.
    fn emit_match_arm(
        &mut self,
        plan: &MatchArm<'_>,
        scrutinee: &Scrutinee,
        flag: &str,
        sink: &MatchSink,
        out: &mut Emitter,
    ) -> Result<()> {
        for (alternative, test) in plan.pairs {
            let header = match (plan.pairs.len(), test) {
                // A wildcard or a bare binding on its own matches whatever the value
                // is, so there is nothing to test.
                (1, Test::AlwaysTrue) => format!("if (!{flag}) begin"),
                _ => format!(
                    "if (!{flag} && ({})) begin",
                    test_text(test, &scrutinee.text, scrutinee.width)
                ),
            };
            out.open(header);
            self.emit_bindings(alternative, &plan.bindings, scrutinee, out)?;
            self.emit_guard_and_body(plan.arm, flag, sink, out)?;
            out.close("end");
        }
        Ok(())
    }

    /// Assign one alternative's bindings from the matched value's bits.
    ///
    /// The offsets are the alternative's own, so `a | b` that binds the same name
    /// at two different offsets keeps both mappings.
    fn emit_bindings(
        &self,
        alternative: &Alternative,
        bindings: &[MatchBinding],
        scrutinee: &Scrutinee,
        out: &mut Emitter,
    ) -> Result<()> {
        for binding in &alternative.bindings {
            let Some(bound) = bindings.iter().find(|bound| bound.name == binding.name) else {
                return Err(self.error(
                    binding.span,
                    format!(
                        "internal: the match arm binds `{}` but no net was declared for it",
                        binding.name
                    ),
                ));
            };
            let width = bound.ty.bit_width()?;
            out.push(format!(
                "{} = {};",
                bound.net,
                bits_text(&scrutinee.text, scrutinee.width, binding.offset, width)
            ));
        }
        Ok(())
    }

    /// One arm's guard, when it has one, and its body.
    ///
    /// `flag` is set only once the guard holds, so a guard that is false leaves the
    /// match open for the other alternatives of this arm and then for the arms
    /// after it.
    fn emit_guard_and_body(
        &mut self,
        arm: &syn::Arm,
        flag: &str,
        sink: &MatchSink,
        out: &mut Emitter,
    ) -> Result<()> {
        match &arm.guard {
            Some((_, guard)) => {
                let condition = self.lower_bool_condition(guard, "a `match` guard", out)?;
                out.open(format!("if ({}) begin", condition.text));
                out.push(format!("{flag} = 1'b1;"));
                self.emit_arm_body(arm, sink, out)?;
                out.close("end");
            }
            None => {
                out.push(format!("{flag} = 1'b1;"));
                self.emit_arm_body(arm, sink, out)?;
            }
        }
        Ok(())
    }

    /// What one arm does once it has won: run its statements, or store its value.
    ///
    /// In statement position a value is lowered and thrown away, exactly as Rust
    /// discards it: `match s { 0 => 1u8, _ => 2u8 };` still runs the arms.
    fn emit_arm_body(&mut self, arm: &syn::Arm, sink: &MatchSink, out: &mut Emitter) -> Result<()> {
        match sink {
            MatchSink::Statement => self.lower_discarded(&arm.body, PROCEDURAL, out),
            MatchSink::Value { net, ty } => {
                let value = self.branch_value(&arm.body, ty, out)?;
                out.push(format!("{net} = {};", value.text));
                Ok(())
            }
        }
    }

    /// The value of one branch of a `match`/`if`: a block's trailing expression, or
    /// the expression itself.
    fn branch_value(&mut self, body: &Expr, ty: &ValueType, out: &mut Emitter) -> Result<Lowered> {
        match body {
            Expr::Block(block) => self.lower_block_value(&block.block, ty, out),
            other => self.lower_value(other, ty, out),
        }
    }

    /// A block in a value position: its statements run in order, and its trailing
    /// expression is the value.
    fn lower_block_value(
        &mut self,
        block: &syn::Block,
        ty: &ValueType,
        out: &mut Emitter,
    ) -> Result<Lowered> {
        let Some((last, statements)) = block.stmts.split_last() else {
            return Err(self.error(
                block.span(),
                "a block used as a value needs a trailing expression",
            ));
        };
        for statement in statements {
            self.lower_stmt(statement, PROCEDURAL, out)?;
        }
        let Stmt::Expr(expr, None) = last else {
            return Err(self.error(
                last.span(),
                "a block used as a value needs a trailing expression without `;`",
            ));
        };
        self.lower_value(expr, ty, out)
    }

    /// A statement position that may also carry a value the test throws away.
    ///
    /// Rust allows `match s { 0 => 1u8, _ => 2u8 };` and `{ ..; value }` as
    /// statements: the value is lowered (a signal it reads is still read, and a
    /// statement it needs still runs) and then discarded.
    fn lower_discarded(&mut self, expr: &Expr, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        match expr {
            Expr::Block(block) => self.lower_discarded_block(&block.block.stmts, ctx, out),
            Expr::Match(_)
            | Expr::If(_)
            | Expr::ForLoop(_)
            | Expr::Assign(_)
            | Expr::Call(_)
            | Expr::Macro(_) => self.lower_expr_stmt(expr, ctx, out),
            other => {
                let _ = self.lower_any_value(other, None, None, out)?;
                Ok(())
            }
        }
    }

    /// A block of statements whose trailing expression, if any, is discarded.
    fn lower_discarded_block(&mut self, stmts: &[Stmt], ctx: Ctx, out: &mut Emitter) -> Result<()> {
        let Some((last, statements)) = stmts.split_last() else {
            return Ok(());
        };
        for statement in statements {
            self.lower_stmt(statement, ctx, out)?;
        }
        match last {
            Stmt::Expr(expr, None) => self.lower_discarded(expr, ctx, out),
            other => self.lower_stmt(other, ctx, out),
        }
    }

    /// Make an arm's bindings visible, remembering what each name meant before.
    ///
    /// An enclosing match's arm may bind the same name, and a binding is scoped to
    /// its own arm, so the previous meaning is restored when the arm is done.
    fn push_bindings(&mut self, bindings: &[MatchBinding]) -> Vec<(String, Option<Name>)> {
        let mut saved = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let previous = self.names.insert(
                binding.name.clone(),
                Name::Local {
                    net: binding.net.clone(),
                    ty: binding.ty.clone(),
                },
            );
            saved.push((binding.name.clone(), previous));
        }
        saved
    }

    fn pop_bindings(&mut self, saved: Vec<(String, Option<Name>)>) {
        for (name, previous) in saved {
            match previous {
                Some(previous) => {
                    self.names.insert(name, previous);
                }
                None => {
                    self.names.remove(&name);
                }
            }
        }
    }

    /// A shared pattern or coverage failure as a source-located error.
    fn pattern_error(&self, span: Span, error: PatError) -> Error {
        self.error(span, error.kind.message())
    }

    // ------------------------------------------------ match / if as expressions

    /// Lower an expression that stands where a value of `ty` is stored: the
    /// initialiser of a `let`, the right of an assignment, `nb_assign!(..)` or the
    /// value of a `match` arm.
    ///
    /// A `match`, an `if` or a block in it is a *statement*, not an expression, so it
    /// is materialized first (see [`Generator::materialize`]) and then lowered by the
    /// plain rules, unchanged.
    fn lower_value(&mut self, expr: &Expr, ty: &ValueType, out: &mut Emitter) -> Result<Lowered> {
        if !needs_statements(expr) {
            return self.lower_stored_value(expr, ty, expr.span());
        }
        let (rewritten, nets) = self.materialize(expr, Some(ty), None, out)?;
        let value = self.lower_materialized(&rewritten, Some(ty), expr.span());
        self.forget_nets(nets);
        value
    }

    /// Lower an expression that has no target type of its own: an assertion operand,
    /// a comparison side, or the initialiser of a `let` with no annotation.
    fn lower_any_value(
        &mut self,
        expr: &Expr,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        out: &mut Emitter,
    ) -> Result<Lowered> {
        if !needs_statements(expr) {
            return self.lower_expr(expr, None, expr.span());
        }
        let (rewritten, nets) = self.materialize(expr, expected, fallback, out)?;
        let value = self.lower_materialized(&rewritten, expected, expr.span());
        self.forget_nets(nets);
        value
    }

    /// A `bool` condition lowered where it stands, so a `match` or an `if` inside it
    /// is evaluated exactly here, once.
    fn lower_bool_condition(
        &mut self,
        expr: &Expr,
        what: &str,
        out: &mut Emitter,
    ) -> Result<Lowered> {
        if !needs_statements(expr) {
            return self.lower_condition(expr, what);
        }
        let bool_ty = ValueType::Bool;
        let (rewritten, nets) = self.materialize(expr, Some(&bool_ty), None, out)?;
        let condition = self.lower_condition(&rewritten, what);
        self.forget_nets(nets);
        condition
    }

    /// Materialize one expression that is evaluated in source order with a later
    /// sibling (`assert_eq!` operands), capturing its value when that sibling emits
    /// statements.
    fn lower_operand_expr(
        &mut self,
        expr: &Expr,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        later_has_statements: bool,
        out: &mut Emitter,
    ) -> Result<(Expr, Vec<String>)> {
        let mut nets = Vec::new();
        let rewritten = self.materialize_operand(
            expr,
            expected,
            fallback,
            None,
            later_has_statements,
            None,
            out,
            &mut nets,
        )?;
        Ok((rewritten, nets))
    }

    /// Lower the expression a materialization produced.
    ///
    /// A materialized value is a hidden net that already has its exact type, so it is
    /// read like any other signal: no width is guessed and no literal is refitted. A
    /// value built in place (an array literal, a variant) keeps the path that gives
    /// it its shape.
    fn lower_materialized(
        &self,
        rewritten: &Expr,
        expected: Option<&ValueType>,
        span: Span,
    ) -> Result<Lowered> {
        match expected {
            Some(ty) if matches!(ty, ValueType::Enum { .. } | ValueType::Array { .. }) => {
                if self.try_value_ref(rewritten)?.is_some() {
                    let value = self.lower_expr(rewritten, None, span)?;
                    self.check_aggregate_shape(ty, &value, span)?;
                    Ok(value)
                } else {
                    self.lower_stored_value(rewritten, ty, span)
                }
            }
            Some(ty) => {
                let context = context_of(ty.bit_width()?, ty);
                self.lower_expr(rewritten, Some(context), span)
            }
            None => self.lower_expr(rewritten, None, span),
        }
    }

    /// Forget the hidden nets one materialized expression named.
    ///
    /// The generated Verilog still reads them; this only restores the source name
    /// space, so a `match` inside an expression cannot leak a name into the rest of
    /// the test.
    fn forget_nets(&mut self, nets: Vec<String>) {
        for net in nets {
            self.names.remove(&net);
        }
    }

    /// Make a hidden net readable by name, so its value is lowered with the same
    /// width, signedness and type rules as any other signal.
    fn register_net(&mut self, net: &str, ty: &ValueType) {
        self.names.insert(
            net.to_string(),
            Name::Local {
                net: net.to_string(),
                ty: ty.clone(),
            },
        );
    }

    /// The diagnostic for a testbench local whose type the source does not state.
    fn local_needs_type(&self, var: &str, span: Span) -> Error {
        self.error(
            span,
            format!(
                "testbench local `{var}` needs an explicit type, for example \
                 `let mut {var}: uint<8>;` or `let mut {var}: bool = false;`"
            ),
        )
    }

    /// Rewrite an expression so every sub-expression that needs statements becomes a
    /// read of a hidden net, emitting those statements where the source evaluates
    /// them.
    ///
    /// `expected` is the type the value is stored into and `fallback` the type of the
    /// value it is compared with; a `match`/`if` uses them only when its own values
    /// name no type. Returns the rewritten expression and the nets it named.
    fn materialize(
        &mut self,
        expr: &Expr,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        out: &mut Emitter,
    ) -> Result<(Expr, Vec<String>)> {
        let mut nets = Vec::new();
        let rewritten = self.materialize_in(expr, expected, fallback, None, out, &mut nets)?;
        Ok((rewritten, nets))
    }

    /// One step of [`Generator::materialize`]; `guard` is the Verilog condition the
    /// value is only evaluated under, `None` when it is always evaluated.
    fn materialize_in(
        &mut self,
        expr: &Expr,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        guard: Option<&str>,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<Expr> {
        Ok(match expr {
            Expr::Match(item) => {
                self.materialize_match(item, expected, fallback, guard, out, nets)?
            }
            Expr::If(item) => self.materialize_if(item, expected, fallback, guard, out, nets)?,
            Expr::Block(block) => {
                self.materialize_block(&block.block, expected, fallback, guard, out, nets)?
            }
            Expr::Binary(binary) => {
                return self.materialize_binary(binary, expected, fallback, guard, out, nets)
            }
            Expr::Paren(paren) => Expr::Paren(syn::ExprParen {
                attrs: paren.attrs.clone(),
                paren_token: paren.paren_token,
                expr: Box::new(self.materialize_in(
                    &paren.expr,
                    expected,
                    fallback,
                    guard,
                    out,
                    nets,
                )?),
            }),
            Expr::Group(group) => Expr::Group(syn::ExprGroup {
                attrs: group.attrs.clone(),
                group_token: group.group_token,
                expr: Box::new(self.materialize_in(
                    &group.expr,
                    expected,
                    fallback,
                    guard,
                    out,
                    nets,
                )?),
            }),
            Expr::Unary(unary) => Expr::Unary(syn::ExprUnary {
                attrs: unary.attrs.clone(),
                op: unary.op,
                expr: Box::new(self.materialize_in(
                    &unary.expr,
                    expected,
                    fallback,
                    guard,
                    out,
                    nets,
                )?),
            }),
            // A cast reinterprets a value that has its own type, so its operand has no
            // context of its own.
            Expr::Cast(cast) => Expr::Cast(syn::ExprCast {
                attrs: cast.attrs.clone(),
                expr: Box::new(self.materialize_in(&cast.expr, None, None, guard, out, nets)?),
                as_token: cast.as_token,
                ty: cast.ty.clone(),
            }),
            Expr::Field(field) => Expr::Field(syn::ExprField {
                attrs: field.attrs.clone(),
                base: Box::new(self.materialize_in(&field.base, None, None, guard, out, nets)?),
                dot_token: field.dot_token,
                member: field.member.clone(),
            }),
            Expr::Index(index) => {
                // The receiver is read before the index expression, as Rust writes
                // them.
                let base = self.materialize_operand(
                    &index.expr,
                    None,
                    None,
                    None,
                    needs_statements(&index.index),
                    guard,
                    out,
                    nets,
                )?;
                let subscript = self.materialize_operand(
                    &index.index,
                    None,
                    None,
                    None,
                    false,
                    guard,
                    out,
                    nets,
                )?;
                Expr::Index(syn::ExprIndex {
                    attrs: index.attrs.clone(),
                    expr: Box::new(base),
                    bracket_token: index.bracket_token,
                    index: Box::new(subscript),
                })
            }
            Expr::MethodCall(call) => {
                let receiver_has_later = call.args.iter().any(needs_statements);
                let receiver = self.materialize_operand(
                    &call.receiver,
                    None,
                    None,
                    None,
                    receiver_has_later,
                    guard,
                    out,
                    nets,
                )?;
                let mut args = Punctuated::new();
                for (position, argument) in call.args.iter().enumerate() {
                    let later = call.args.iter().skip(position + 1).any(needs_statements);
                    let argument = self
                        .materialize_operand(argument, None, None, None, later, guard, out, nets)?;
                    args.push(argument);
                }
                Expr::MethodCall(syn::ExprMethodCall {
                    attrs: call.attrs.clone(),
                    receiver: Box::new(receiver),
                    dot_token: call.dot_token,
                    method: call.method.clone(),
                    turbofish: call.turbofish.clone(),
                    paren_token: call.paren_token,
                    args,
                })
            }
            Expr::Call(call) => {
                let func = self.materialize_in(&call.func, None, None, guard, out, nets)?;
                let mut args = Punctuated::new();
                for (position, argument) in call.args.iter().enumerate() {
                    let later = call.args.iter().skip(position + 1).any(needs_statements);
                    let argument = self
                        .materialize_operand(argument, None, None, None, later, guard, out, nets)?;
                    args.push(argument);
                }
                Expr::Call(syn::ExprCall {
                    attrs: call.attrs.clone(),
                    func: Box::new(func),
                    paren_token: call.paren_token,
                    args,
                })
            }
            Expr::Array(array) => {
                let element = match expected {
                    Some(ValueType::Array { element, .. }) => Some(element.as_ref()),
                    _ => None,
                };
                let context = match element {
                    Some(element) => Some(context_of(element.bit_width()?, element)),
                    None => None,
                };
                let mut elems = Punctuated::new();
                for (position, item) in array.elems.iter().enumerate() {
                    let later = array.elems.iter().skip(position + 1).any(needs_statements);
                    let item = self.materialize_operand(
                        item, element, None, context, later, guard, out, nets,
                    )?;
                    elems.push(item);
                }
                Expr::Array(syn::ExprArray {
                    attrs: array.attrs.clone(),
                    bracket_token: array.bracket_token,
                    elems,
                })
            }
            Expr::Repeat(repeat) => Expr::Repeat(syn::ExprRepeat {
                attrs: repeat.attrs.clone(),
                bracket_token: repeat.bracket_token,
                expr: Box::new(self.materialize_in(&repeat.expr, None, None, guard, out, nets)?),
                semi_token: repeat.semi_token,
                len: repeat.len.clone(),
            }),
            other => {
                // Anything else has to be a plain expression: a construct this
                // lowering does not understand must never be passed through with a
                // statement-needing value still inside it.
                if needs_statements(other) {
                    return Err(self.error(
                        other.span(),
                        format!(
                            "a value produced by `match`/`if` cannot be used inside `{}`; write it \
                             into a local first",
                            render_expr(other)
                        ),
                    ));
                }
                other.clone()
            }
        })
    }

    /// A binary operator: its operands are read left to right, and the right side of
    /// `&&`/`||` runs only when the left side selects it.
    fn materialize_binary(
        &mut self,
        binary: &syn::ExprBinary,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        guard: Option<&str>,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<Expr> {
        let bool_ty = ValueType::Bool;
        // The context the plain lowering gives each operand, so a value read before a
        // later operand's statements is read at the width the final expression uses.
        let context = match expected {
            Some(ty) => Some(context_of(ty.bit_width()?, ty)),
            None => None,
        };
        if matches!(binary.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) {
            let left = self.materialize_operand(
                &binary.left,
                Some(&bool_ty),
                None,
                None,
                false,
                guard,
                out,
                nets,
            )?;
            // `&&`/`||` tests the left side once. When the right side emits
            // statements, they run between the test and the final expression, so the
            // left side is captured first: otherwise a statement that writes a signal
            // the left reads would change the value the operator tests. The guard is a
            // Verilog `if`, so an unknown (`x`) left operand takes the not-selected
            // branch, exactly as `if` and `wait_until` already do.
            let is_and = matches!(binary.op, syn::BinOp::And(_));
            let (left, only_when) = if needs_statements(&binary.right) {
                let value = self.lower_expr(&left, None, binary.left.span())?;
                let (left, left_text) = if self.operand_changes(&left) {
                    self.snapshot(&left, value, out, nets)?
                } else {
                    let text = value.text.clone();
                    (left, text)
                };
                let selects_right = if is_and {
                    left_text
                } else {
                    format!("!({left_text})")
                };
                let only_when = match guard {
                    Some(outer) => format!("({outer}) && ({selects_right})"),
                    None => selects_right,
                };
                (left, Some(only_when))
            } else {
                (left, None)
            };
            let right = self.materialize_in(
                &binary.right,
                Some(&bool_ty),
                fallback,
                only_when.as_deref(),
                out,
                nets,
            )?;
            return Ok(Expr::Binary(syn::ExprBinary {
                attrs: binary.attrs.clone(),
                left: Box::new(left),
                op: binary.op,
                right: Box::new(right),
            }));
        }
        let compares = !matches!(
            binary.op,
            syn::BinOp::Add(_)
                | syn::BinOp::Sub(_)
                | syn::BinOp::Mul(_)
                | syn::BinOp::Div(_)
                | syn::BinOp::Rem(_)
                | syn::BinOp::BitXor(_)
                | syn::BinOp::BitAnd(_)
                | syn::BinOp::BitOr(_)
                | syn::BinOp::Shl(_)
                | syn::BinOp::Shr(_)
        );
        let shifts = matches!(binary.op, syn::BinOp::Shl(_) | syn::BinOp::Shr(_));
        let (left_expected, left_context) = if compares {
            (None, None)
        } else {
            (expected, context)
        };
        let left = self.materialize_operand(
            &binary.left,
            left_expected,
            fallback,
            left_context,
            needs_statements(&binary.right),
            guard,
            out,
            nets,
        )?;
        // A comparison has no type of its own, so the type the left side already has
        // is the only thing that can tell a `match` on the right side what it
        // produces.
        let right_fallback = if compares {
            self.peek_type(&left, binary.left.span())
        } else {
            None
        };
        let right = self.materialize_operand(
            &binary.right,
            if compares || shifts { None } else { expected },
            right_fallback.as_ref().or(fallback),
            if compares || shifts { None } else { context },
            false,
            guard,
            out,
            nets,
        )?;
        Ok(Expr::Binary(syn::ExprBinary {
            attrs: binary.attrs.clone(),
            left: Box::new(left),
            op: binary.op,
            right: Box::new(right),
        }))
    }

    /// Materialize one operand of an ordered list, reading it into a hidden net when
    /// a later operand emits statements.
    #[allow(clippy::too_many_arguments)]
    fn materialize_operand(
        &mut self,
        operand: &Expr,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        context: Option<WidthContext>,
        later_has_statements: bool,
        guard: Option<&str>,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<Expr> {
        let rewritten = self.materialize_in(operand, expected, fallback, guard, out, nets)?;
        if !later_has_statements || !self.operand_changes(&rewritten) {
            return Ok(rewritten);
        }
        // Rust reads this operand before the later one runs its statements, so the
        // value has to be captured now, or a later assignment would change what this
        // expression already read.
        let value = self.lower_expr(&rewritten, context, operand.span())?;
        let (snapshot, _net) = self.snapshot(&rewritten, value, out, nets)?;
        Ok(snapshot)
    }

    /// Read a value into a hidden net and return the read and the net's name.
    fn snapshot(
        &mut self,
        expr: &Expr,
        value: Lowered,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<(Expr, String)> {
        let ty = match &value.ty {
            Some(ty) => ty.clone(),
            None => match value.kind {
                Kind::Bool => ValueType::Bool,
                Kind::Int { signed: true } => ValueType::SInt { width: value.width },
                Kind::Int { signed: false } => ValueType::UInt { width: value.width },
                Kind::Struct => {
                    return Err(self.error(
                        expr.span(),
                        "this value has no shape a hidden net could hold",
                    ))
                }
            },
        };
        self.match_counter += 1;
        let index = self.match_counter;
        let net = self.declare_hidden_net(index, "operand", &ty)?;
        out.push(format!("{net} = {};", value.text));
        self.register_net(&net, &ty);
        nets.push(net.clone());
        Ok((net_expr(&net), net))
    }

    /// Whether re-reading this value after a later statement could give a different
    /// answer. A constant cannot change, and a generated net is never assigned by the
    /// source, so neither needs a snapshot.
    fn operand_changes(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Lit(_) => false,
            // A variant constructor and an unknown name read no signal; the plain
            // lowering reports the unknown name.
            Expr::Path(_) => match self.try_value_ref(expr).ok().flatten() {
                Some(value) => !value.text.starts_with("__shyhdl"),
                None => false,
            },
            Expr::Paren(paren) => self.operand_changes(&paren.expr),
            Expr::Group(group) => self.operand_changes(&group.expr),
            Expr::Unary(unary) => self.operand_changes(&unary.expr),
            _ => true,
        }
    }

    /// The type an already-materialized operand has, as a hint for the other side of
    /// a comparison. Pure: nothing is emitted, and an error here is reported when the
    /// operand is lowered for real.
    fn peek_type(&self, expr: &Expr, span: Span) -> Option<ValueType> {
        self.lower_expr(expr, None, span)
            .ok()
            .and_then(|value| value.ty)
    }

    /// Turn a `match` in value position into statements plus a hidden net that holds
    /// the value of the arm that wins.
    fn materialize_match(
        &mut self,
        item: &syn::ExprMatch,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        guard: Option<&str>,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<Expr> {
        let ty = match expected {
            Some(ty) => ty.clone(),
            None => self.match_result_type(item, fallback, &HashMap::new())?,
        };
        if let Some(guard) = guard {
            out.open(format!("if ({guard}) begin"));
        }
        self.match_counter += 1;
        let index = self.match_counter;
        let net = self.declare_hidden_net(index, "result", &ty)?;
        let width = ty.bit_width()?;
        out.push(format!("{net} = {width}'d0;"));
        let plan = self.prepare_match(item, index, out)?;
        let sink = MatchSink::Value {
            net: net.clone(),
            ty: ty.clone(),
        };
        self.emit_match_body(&plan, &sink, out)?;
        if guard.is_some() {
            out.close("end");
        }
        nets.push(net.clone());
        self.register_net(&net, &ty);
        Ok(net_expr(&net))
    }

    /// Turn an `if` used as a value into statements: only the selected branch runs,
    /// and its value is left in a hidden net.
    fn materialize_if(
        &mut self,
        item: &syn::ExprIf,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        guard: Option<&str>,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<Expr> {
        let ty = match expected {
            Some(ty) => ty.clone(),
            None => self.if_result_type(item, fallback, &HashMap::new())?,
        };
        let Some((_, else_branch)) = &item.else_branch else {
            return Err(self.error(
                item.span(),
                "an `if` used as a value needs an `else` branch",
            ));
        };
        if let Some(guard) = guard {
            out.open(format!("if ({guard}) begin"));
        }
        self.match_counter += 1;
        let index = self.match_counter;
        let net = self.declare_hidden_net(index, "result", &ty)?;
        let width = ty.bit_width()?;
        out.push(format!("{net} = {width}'d0;"));
        let condition = self.lower_bool_condition(&item.cond, "an `if` condition", out)?;
        out.open(format!("if ({}) begin", condition.text));
        let then_value = self.lower_block_value(&item.then_branch, &ty, out)?;
        out.push(format!("{net} = {};", then_value.text));
        out.indent -= 1;
        out.open("end else begin");
        let else_value = self.branch_value(else_branch, &ty, out)?;
        out.push(format!("{net} = {};", else_value.text));
        out.close("end");
        if guard.is_some() {
            out.close("end");
        }
        nets.push(net.clone());
        self.register_net(&net, &ty);
        Ok(net_expr(&net))
    }

    /// Turn a block used as a value into its statements, lowered where they stand.
    ///
    /// The trailing expression is returned as the rewritten expression, so the
    /// caller keeps its own width and type rules.
    fn materialize_block(
        &mut self,
        block: &syn::Block,
        expected: Option<&ValueType>,
        fallback: Option<&ValueType>,
        guard: Option<&str>,
        out: &mut Emitter,
        nets: &mut Vec<String>,
    ) -> Result<Expr> {
        let Some((last, statements)) = block.stmts.split_last() else {
            return Err(self.error(
                block.span(),
                "a block used as a value needs a trailing expression",
            ));
        };
        let Stmt::Expr(tail, None) = last else {
            return Err(self.error(
                last.span(),
                "a block used as a value needs a trailing expression without `;`",
            ));
        };
        if let Some(guard) = guard {
            out.open(format!("if ({guard}) begin"));
        }
        for statement in statements {
            self.lower_stmt(statement, PROCEDURAL, out)?;
        }
        // The tail is already inside the guarded block when there is one.
        let rewritten = self.materialize_in(tail, expected, fallback, None, out, nets)?;
        if guard.is_some() {
            out.close("end");
        }
        Ok(rewritten)
    }

    /// The value type of a `match` expression: every arm that states a type has to
    /// state the same one, and an arm that states none (an unsuffixed literal) takes
    /// it from the context, exactly as any other literal does.
    fn match_result_type(
        &self,
        item: &syn::ExprMatch,
        fallback: Option<&ValueType>,
        outer: &HashMap<String, ValueType>,
    ) -> Result<ValueType> {
        // A value may name what its own pattern binds, and only the pattern states
        // that type, so each arm is typed with its bindings in scope.
        let scrutinee_ty = self.value_type_of_in(&item.expr, outer)?;
        let mut agreed: Option<(ValueType, Span)> = None;
        for arm in &item.arms {
            // A statement arm states no value type.
            let Some(tail) = arm_tail(&arm.body) else {
                continue;
            };
            let mut env = outer.clone();
            if let Some(ty) = &scrutinee_ty {
                if let Ok(lowered) = pattern::lower_arm(&arm.pat, ty, &NoConstants) {
                    if let Some(alternative) = lowered.alternatives.first() {
                        for binding in &alternative.bindings {
                            env.insert(binding.name.clone(), binding.ty.clone());
                        }
                    }
                }
            }
            let Some(ty) = self.value_type_of_in(tail, &env)? else {
                continue;
            };
            match &agreed {
                None => agreed = Some((ty, tail.span())),
                Some((first, _)) if *first != ty => {
                    return Err(self.error(
                        tail.span(),
                        format!(
                            "this arm's value is {} but another arm's is {}; every arm of a \
                             `match` expression must have the same type",
                            describe_type(&ty),
                            describe_type(first)
                        ),
                    ))
                }
                Some(_) => {}
            }
        }
        match agreed {
            Some((ty, _)) => Ok(ty),
            None => fallback.cloned().ok_or_else(|| {
                self.error(
                    item.span(),
                    "cannot tell the type of this `match` expression; write it on the `let`, or \
                     give a value a suffix (`1u8`), a cast (`as uint<8>`) or a variant",
                )
            }),
        }
    }

    /// Whether this expression needs the target's type to be built: a variant
    /// construction, a struct or array literal. A value that already has a type
    /// (a port, a field, a local) does not, so it is read as before.
    fn needs_target_type(&self, expr: &Expr) -> Result<bool> {
        Ok(match expr {
            Expr::Call(_) | Expr::Struct(_) | Expr::Array(_) | Expr::Repeat(_) => true,
            Expr::Path(_) => self.try_value_ref(expr)?.is_none(),
            Expr::Paren(paren) => self.needs_target_type(&paren.expr)?,
            Expr::Group(group) => self.needs_target_type(&group.expr)?,
            _ => false,
        })
    }

    /// The value type of an `if` expression: both branches state the same type, or
    /// one states it and the other is a literal that takes it.
    fn if_result_type(
        &self,
        item: &ExprIf,
        fallback: Option<&ValueType>,
        env: &HashMap<String, ValueType>,
    ) -> Result<ValueType> {
        let Some((_, else_branch)) = &item.else_branch else {
            return Err(self.error(
                item.span(),
                "an `if` used as a value needs an `else` branch",
            ));
        };
        let then_ty = match block_tail(&item.then_branch) {
            Some(tail) => self.value_type_of_in(tail, env)?,
            None => None,
        };
        let else_ty = self.value_type_of_in(else_branch, env)?;
        match (then_ty, else_ty) {
            (Some(then_ty), Some(else_ty)) if then_ty == else_ty => Ok(then_ty),
            (Some(_), Some(_)) => Err(self.error(
                item.span(),
                "both branches of an `if` used as a value must have the same type",
            )),
            (Some(ty), None) | (None, Some(ty)) => Ok(ty),
            (None, None) => fallback.cloned().ok_or_else(|| {
                self.error(
                    item.span(),
                    "cannot tell the type of this `if` expression; write it on the `let`, or keep \
                     one branch's value typed with a suffix, a cast or a variant",
                )
            }),
        }
    }

    /// The `ValueType` an expression states by itself, if it states one.
    ///
    /// A pure structural probe: it emits nothing and evaluates nothing, so a caller
    /// can decide a type before the value is lowered, and the value is still lowered
    /// exactly once. A plain integer literal states no type, exactly as it never has
    /// one; it takes the context when it is lowered.
    /// The type an expression states by itself, if it states one.
    fn value_type_of(&self, expr: &Expr) -> Result<Option<ValueType>> {
        self.value_type_of_in(expr, &HashMap::new())
    }

    fn value_type_of_in(
        &self,
        expr: &Expr,
        env: &HashMap<String, ValueType>,
    ) -> Result<Option<ValueType>> {
        Ok(match expr {
            Expr::Paren(paren) => self.value_type_of_in(&paren.expr, env)?,
            Expr::Group(group) => self.value_type_of_in(&group.expr, env)?,
            Expr::Unary(unary) => self.value_type_of_in(&unary.expr, env)?,
            Expr::Block(block) => match block_tail(&block.block) {
                Some(tail) => self.value_type_of_in(tail, env)?,
                None => None,
            },
            Expr::Match(item) => Some(self.match_result_type(item, None, env)?),
            Expr::If(item) => Some(self.if_result_type(item, None, env)?),
            Expr::Lit(literal) => match &literal.lit {
                Lit::Bool(_) => Some(ValueType::Bool),
                Lit::Int(value) => super::expr::suffixed_literal_type(value.suffix()),
                _ => None,
            },
            Expr::Cast(cast) => Some(self.parse_local_type(&cast.ty)?),
            Expr::Path(path) => {
                // A name this `match` binds is not a signal: its type comes from
                // the pattern, which is what lets an unannotated `let` take the
                // type of a payload it binds.
                let bound = path
                    .path
                    .get_ident()
                    .and_then(|ident| env.get(&ident.to_string()).cloned());
                match bound {
                    Some(ty) => Some(ty),
                    None => match self.try_value_ref(expr)? {
                        Some(value) => Some(value.ty),
                        // A variant names one of the design's enums; a name that is
                        // neither states no type, and the lowering reports it where
                        // the value is used.
                        None => self.variant_enum_type(&path.path, false, expr.span()).ok(),
                    },
                }
            }
            Expr::Struct(structure) => self
                .variant_enum_type(&structure.path, true, expr.span())
                .ok(),
            Expr::Call(call) => match call.func.as_ref() {
                Expr::Path(path) => self.variant_enum_type(&path.path, true, expr.span()).ok(),
                _ => None,
            },
            Expr::Field(_) | Expr::Index(_) | Expr::MethodCall(_) => {
                self.try_value_ref(expr)?.map(|value| value.ty)
            }
            // A comparison and a logical operator always produce a `bool`, which is
            // what the plain lowering gives them; arithmetic states no type.
            Expr::Binary(binary) => match binary.op {
                syn::BinOp::Eq(_)
                | syn::BinOp::Ne(_)
                | syn::BinOp::Lt(_)
                | syn::BinOp::Le(_)
                | syn::BinOp::Gt(_)
                | syn::BinOp::Ge(_)
                | syn::BinOp::And(_)
                | syn::BinOp::Or(_) => Some(ValueType::Bool),
                _ => None,
            },
            Expr::Array(array) => self.array_literal_type(array)?,
            Expr::Repeat(repeat) => match repeat.len.as_ref() {
                Expr::Lit(syn::ExprLit {
                    lit: Lit::Int(value),
                    ..
                }) => match (
                    value.base10_parse::<usize>(),
                    self.value_type_of_in(&repeat.expr, env)?,
                ) {
                    (Ok(len), Some(element)) => Some(ValueType::Array {
                        element: Box::new(element),
                        len,
                    }),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
    }

    /// The type an array literal states, when every element states the same one.
    fn array_literal_type(&self, array: &syn::ExprArray) -> Result<Option<ValueType>> {
        let Some(first) = array.elems.first() else {
            return Ok(None);
        };
        let Some(element) = self.value_type_of(first)? else {
            return Ok(None);
        };
        for item in array.elems.iter().skip(1) {
            if self.value_type_of(item)? != Some(element.clone()) {
                return Ok(None);
            }
        }
        Ok(Some(ValueType::Array {
            element: Box::new(element),
            len: array.elems.len(),
        }))
    }

    fn require_procedural(&self, ctx: Ctx, span: Span, what: &str) -> Result<()> {
        if ctx.procedural {
            return Ok(());
        }
        Err(self.error(
            span,
            format!("{what} must be inside `initial! {{ ... }}` or `always! {{ ... }}`"),
        ))
    }

    /// A `// file:line` comment, so the generated testbench keeps the
    /// source-to-generated mapping (REQ-VER-006).
    fn comment_at(&self, out: &mut Emitter, span: Span) -> Result<()> {
        let location = self.location(span)?;
        out.push(format!(
            "// {}:{}",
            sanitize_comment(&location.file),
            location.line
        ));
        Ok(())
    }

    // ------------------------------------------------------------ call statements

    fn lower_call_stmt(&mut self, call: &ExprCall, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        let Some(func) = call_path_ident(&call.func) else {
            return Err(self.error(call.span(), "unsupported call in a testbench"));
        };
        match func.to_string().as_str() {
            "delay" => {
                self.require_procedural(ctx, call.span(), "`delay(..)`")?;
                let duration = self.one_time_argument(call, "delay")?;
                let ticks = self.delay_text(duration, call.span())?;
                self.comment_at(out, call.span())?;
                out.push(format!("#{ticks};"));
                Ok(())
            }
            "wait" => {
                self.require_procedural(ctx, call.span(), "`wait(..)`")?;
                let (net, edge) = self.edge_argument(call, "wait")?;
                self.comment_at(out, call.span())?;
                out.push(format!("@({edge} {net});"));
                Ok(())
            }
            "wait_until" => {
                self.require_procedural(ctx, call.span(), "`wait_until(..)`")?;
                if call.args.len() != 1 {
                    return Err(self.error(
                        call.span(),
                        "`wait_until(condition)` takes exactly one condition",
                    ));
                }
                if needs_statements(&call.args[0]) {
                    return Err(self.error(
                        call.args[0].span(),
                        "`wait_until(..)` re-reads its condition every time a signal it names \
                         changes, but a `match`/`if` value in it would be evaluated only once, so \
                         the wait would never see a new value; compute the value into a local \
                         before the wait, or write the condition from signals",
                    ));
                }
                let condition = self.lower_condition(&call.args[0], "`wait_until`")?;
                self.comment_at(out, call.span())?;
                out.push(format!("wait ({});", condition.text));
                Ok(())
            }
            "gen_clk" => {
                if ctx.procedural {
                    return Err(self.error(
                        call.span(),
                        "`gen_clk` starts a process and is written at the test level, not inside \
                         `initial!`/`always!`",
                    ));
                }
                if call.args.len() != 2 {
                    return Err(self.error(
                        call.span(),
                        "`gen_clk(signal, frequency)` takes a DUT clock port and a frequency, for \
                         example `gen_clk(dut.clk, mhz(100))`",
                    ));
                }
                // Reuse the `clock!` lowering and its driver checks: `gen_clk`
                // only turns a frequency into the period.
                let period = self.frequency_period(&call.args[1])?;
                let args = ClockArgs {
                    signal: call.args[0].clone(),
                    period: Some(period),
                    high: None,
                    low: None,
                    initial: false,
                };
                self.emit_clock(args, call.span(), "gen_clk")
            }
            "sample_after" => {
                self.require_procedural(ctx, call.span(), "`sample_after(..)`")?;
                if call.args.len() != 2 {
                    return Err(self.error(
                        call.span(),
                        "`sample_after(edge, offset)` takes an edge and a positive settle offset",
                    ));
                }
                let (net, edge) = self.edge_expression(&call.args[0])?;
                let offset =
                    match time_quantity(&call.args[1]) {
                        TimeValue::Value(offset) => offset,
                        TimeValue::Overflow => return Err(self.error(
                            call.args[1].span(),
                            "the settle offset overflows 64 bits of femtoseconds; use a smaller \
                             value",
                        )),
                        TimeValue::NotATime => {
                            return Err(self.error(
                                call.args[1].span(),
                                "the settle offset must be a time value such as `ps(1)`",
                            ))
                        }
                    };
                if offset == 0 {
                    return Err(self.error(
                        call.args[1].span(),
                        "`sample_after` needs a positive settle offset: `#0` is not a stability \
                         barrier for non-blocking assignments (REQ-TEST-006); write \
                         `sample_after(posedge(dut.clk), ps(1))`",
                    ));
                }
                let ticks = self.delay_text(offset, call.args[1].span())?;
                self.comment_at(out, call.span())?;
                out.push(format!("@({edge} {net});"));
                out.push(format!("#{ticks};"));
                Ok(())
            }
            other => Err(self.error(
                call.span(),
                format!(
                    "unknown testbench call `{other}`; supported calls are `delay(ns(..))`, \
                     `wait(posedge(..))`, `wait(negedge(..))`, `wait_until(..)`, \
                     `sample_after(edge, offset)` and `gen_clk(signal, frequency)`"
                ),
            )),
        }
    }

    fn one_time_argument(&self, call: &ExprCall, what: &str) -> Result<u64> {
        if call.args.len() != 1 {
            return Err(self.error(
                call.span(),
                format!("`{what}()` takes exactly one time value"),
            ));
        }
        match time_quantity(&call.args[0]) {
            TimeValue::Value(femtoseconds) => Ok(femtoseconds),
            TimeValue::Overflow => Err(self.error(
                call.args[0].span(),
                format!(
                    "the time value of `{what}` overflows 64 bits of femtoseconds; use a smaller \
                     value"
                ),
            )),
            TimeValue::NotATime => Err(self.error(
                call.args[0].span(),
                format!(
                    "`{what}` needs a time value with an explicit unit, for example \
                     `{what}(ns(10))`; the unit is converted to the test's timeunit"
                ),
            )),
        }
    }

    fn edge_argument(&self, call: &ExprCall, what: &str) -> Result<(String, &'static str)> {
        if call.args.len() != 1 {
            return Err(self.error(
                call.span(),
                format!("`{what}(edge)` takes exactly one `posedge(..)` or `negedge(..)`"),
            ));
        }
        self.edge_expression(&call.args[0])
    }

    /// `posedge(dut.clk)` / `negedge(dut.clk)` to `("dut_clk", "posedge")`.
    fn edge_expression(&self, expr: &Expr) -> Result<(String, &'static str)> {
        let Expr::Call(call) = expr else {
            return Err(self.error(
                expr.span(),
                "an edge wait needs `posedge(signal)` or `negedge(signal)`",
            ));
        };
        let Some(ident) = call_path_ident(&call.func) else {
            return Err(self.error(
                expr.span(),
                "an edge wait needs `posedge(..)` or `negedge(..)`",
            ));
        };
        let edge = match ident.to_string().as_str() {
            "posedge" => "posedge",
            "negedge" => "negedge",
            other => {
                return Err(self.error(
                    call.func.span(),
                    format!("`{other}` is not an edge; use `posedge(..)` or `negedge(..)`"),
                ))
            }
        };
        if call.args.len() != 1 {
            return Err(self.error(
                call.span(),
                format!("`{edge}(..)` takes exactly one signal"),
            ));
        }
        let signal = self.try_value_ref(&call.args[0])?.ok_or_else(|| {
            self.error(
                call.args[0].span(),
                "an edge wait needs a signal such as `dut.clk`",
            )
        })?;
        if signal.width != 1 {
            return Err(self.error(
                call.args[0].span(),
                format!(
                    "`{edge}` needs a one bit signal, but `{}` is {} bits",
                    signal.text, signal.width
                ),
            ));
        }
        Ok((signal.text, edge))
    }

    /// Turn a frequency written as `mhz(100)` into an exact half-period-capable
    /// period in femtoseconds.
    ///
    /// Nothing is rounded: a frequency that does not divide one second into whole
    /// femtoseconds, a zero frequency, and an overflowing value are all rejected
    /// with the source location of the offending argument (REQ-TEST-004).
    fn frequency_period(&self, expr: &Expr) -> Result<u64> {
        let Expr::Call(call) = expr else {
            return Err(self.error(
                expr.span(),
                "a frequency needs an explicit unit, for example `mhz(100)`; supported units are \
                 hz, khz, mhz and ghz (also Hz, kHz, MHz, GHz)",
            ));
        };
        let Some(unit) = call_path_ident(&call.func) else {
            return Err(self.error(call.span(), "unsupported frequency value"));
        };
        let Some(multiplier) = frequency_multiplier(&unit.to_string()) else {
            return Err(self.error(
                call.func.span(),
                format!(
                    "`{unit}` is not a frequency unit; use hz, khz, mhz or ghz (also Hz, kHz, \
                     MHz, GHz)"
                ),
            ));
        };
        if call.args.len() != 1 {
            return Err(self.error(
                call.span(),
                format!("`{unit}(n)` takes one integer frequency"),
            ));
        }
        let magnitude = match &call.args[0] {
            Expr::Lit(syn::ExprLit {
                lit: Lit::Int(value),
                ..
            }) => value.base10_parse::<u64>().ok(),
            _ => None,
        };
        let Some(magnitude) = magnitude else {
            return Err(self.error(
                call.args[0].span(),
                "the frequency must be a non-negative integer literal",
            ));
        };
        let Some(hertz) = magnitude.checked_mul(multiplier) else {
            return Err(self.error(
                call.args[0].span(),
                "this frequency does not fit in 64 bits",
            ));
        };
        if hertz == 0 {
            return Err(self.error(expr.span(), "a frequency of zero is not a clock"));
        }
        if !FEMTOSECONDS_PER_SECOND.is_multiple_of(hertz) {
            return Err(self.error(
                expr.span(),
                format!(
                    "{hertz} Hz has no exact whole-femtosecond period, so it cannot be written as \
                     a frequency without rounding; no precision can fix that for this value, so \
                     write the clock explicitly with `clock!(<signal>, period = ns(..))` (or \
                     `high`/`low`) and pick a period your timeunit and precision represent exactly"
                ),
            ));
        }
        Ok(FEMTOSECONDS_PER_SECOND / hertz)
    }

    /// Lower a time value to a Verilog delay, exact in the test's time scale.
    ///
    /// A zero delay is legal Verilog and is emitted as `#0`; `sample_after`
    /// refuses a zero settle offset separately (REQ-TEST-006).
    fn delay_text(&self, femtoseconds: u64, span: Span) -> Result<String> {
        if femtoseconds == 0 {
            return Ok("0".to_string());
        }
        let precision = self.precision.femtoseconds();
        if !femtoseconds.is_multiple_of(precision) {
            return Err(self.error(
                span,
                format!(
                    "a delay of {femtoseconds} fs cannot be represented with timeunit {} and \
                     precision {}; declare `#[hardware_test(timeunit = \"..\", precision = \"..\")]` \
                     with a finer precision",
                    self.timeunit, self.precision
                ),
            ));
        }
        let ticks = femtoseconds / precision;
        if self.fraction_digits == 0 {
            return Ok(ticks.to_string());
        }
        let scale = 10u64.pow(self.fraction_digits);
        let whole = ticks / scale;
        let fraction = ticks % scale;
        if fraction == 0 {
            return Ok(whole.to_string());
        }
        Ok(format!(
            "{whole}.{fraction:0width$}",
            width = self.fraction_digits as usize
        ))
    }

    // -------------------------------------------------------------------- macros

    fn lower_macro(&mut self, mac: &Macro, ctx: Ctx, out: &mut Emitter) -> Result<()> {
        let Some(ident) = mac.path.get_ident() else {
            return Err(self.error(mac.path.span(), "unsupported macro in a testbench"));
        };
        match ident.to_string().as_str() {
            "initial" => {
                if ctx.procedural {
                    return Err(self.error(
                        mac.span(),
                        "`initial!` declares a process, so it is written at the test level, not \
                         inside another process",
                    ));
                }
                let stmts = parse_statements(mac.tokens.clone())
                    .map_err(|source| self.error(source.span(), source.to_string()))?;
                self.initial_blocks += 1;
                let completion = !self.has_explicit_finish;
                let mut emitter = Emitter::new(0);
                emitter.open("initial begin");
                let mut inner = Emitter::new(2);
                self.lower_discarded_block(&stmts, PROCEDURAL, &mut inner)?;
                if completion {
                    inner.push(format!("{BLOCKS_DONE} = {BLOCKS_DONE} + 1;"));
                }
                emitter.lines.extend(inner.lines);
                emitter.close("end");
                self.procs.push(emitter.text().trim_end().to_string());
                Ok(())
            }
            "always" => {
                if ctx.procedural {
                    return Err(self.error(
                        mac.span(),
                        "`always!` declares a process, so it is written at the test level, not \
                         inside another process",
                    ));
                }
                let stmts = parse_statements(mac.tokens.clone())
                    .map_err(|source| self.error(source.span(), source.to_string()))?;
                let mut emitter = Emitter::new(0);
                emitter.open("always begin");
                let mut inner = Emitter::new(2);
                self.lower_discarded_block(&stmts, PROCEDURAL, &mut inner)?;
                emitter.lines.extend(inner.lines);
                emitter.close("end");
                self.procs.push(emitter.text().trim_end().to_string());
                Ok(())
            }
            "clock" => {
                if ctx.procedural {
                    return Err(self.error(
                        mac.span(),
                        "`clock!` starts a process and is written at the test level",
                    ));
                }
                let args = parse_macro_args::<ClockArgs>(mac, self)?;
                self.emit_clock(args, mac.span(), "clock!")
            }
            "reset" => {
                if ctx.procedural {
                    return Err(self.error(
                        mac.span(),
                        "`reset!` starts a process and is written at the test level",
                    ));
                }
                let args = parse_macro_args::<ResetArgs>(mac, self)?;
                self.emit_reset(args, mac.span())
            }
            "timeout" => {
                if ctx.procedural {
                    return Err(self.error(
                        mac.span(),
                        "`timeout!` starts a process and is written at the test level",
                    ));
                }
                let args = parse_macro_args::<TimeoutArgs>(mac, self)?;
                self.emit_timeout(args.duration, mac.span())
            }
            "pass" => {
                self.require_procedural(ctx, mac.span(), "`pass!()`")?;
                self.comment_at(out, mac.span())?;
                out.push(format!("{CASE_COUNTER} = {CASE_COUNTER} + 1;"));
                out.open(format!("if ({FAILED_FLAG} == 1'b0) begin"));
                out.push(format!("{PASS_COUNTER} = {PASS_COUNTER} + 1;"));
                out.push(format!(
                    "$display(\"SHYHDL_PASS test={} case=%0d\", {CASE_COUNTER});",
                    self.test
                ));
                out.push("$fflush();");
                out.close("end");
                Ok(())
            }
            "finish" => {
                self.require_procedural(ctx, mac.span(), "`finish!()`")?;
                self.comment_at(out, mac.span())?;
                self.emit_completion(out, "finish");
                Ok(())
            }
            "nb_assign" => {
                self.require_procedural(ctx, mac.span(), "`nb_assign!(..)`")?;
                let args = parse_expr_list(mac, self)?;
                if args.len() != 2 {
                    return Err(self.error(
                        mac.span(),
                        "`nb_assign!(target, value)` takes exactly two arguments",
                    ));
                }
                let mut args = args.into_iter();
                let target_expr = args.next().expect("checked length");
                let value_expr = args.next().expect("checked length");
                let target = self.write_target(&target_expr)?;
                let value = self.lower_value(&value_expr, &target.ty, out)?;
                self.comment_at(out, mac.span())?;
                let statement = format!("{} <= {};", target.text, value.text);
                match &target.guard {
                    Some(guard) => out.push(format!("if ({guard}) {statement}")),
                    None => out.push(statement),
                }
                Ok(())
            }
            "assert" => {
                self.require_procedural(ctx, mac.span(), "`assert!(..)`")?;
                let (condition, message) = self.one_condition(mac, "assert!")?;
                let condition = self.lower_bool_condition(&condition, "`assert!`", out)?;
                self.emit_assert(out, mac.span(), &condition, message.as_deref())
            }
            "assert_eq" => {
                self.require_procedural(ctx, mac.span(), "`assert_eq!(..)`")?;
                let (left_expr, right_expr, message) = self.two_conditions(mac, "assert_eq!")?;
                // Both operands are lowered where they stand, in source order: a
                // `match` on the right runs after the left side was read, and the read
                // of the left side is captured when the right one has statements.
                let (left_expr, left_nets) = self.lower_operand_expr(
                    &left_expr,
                    None,
                    None,
                    needs_statements(&right_expr),
                    out,
                )?;
                let left = self.lower_expr(&left_expr, None, left_expr.span())?;
                let left_ty = left.ty.clone();
                // The left side's type is the only thing that can tell a `match` on
                // the right side what it produces.
                let (right_expr, right_nets) =
                    self.lower_operand_expr(&right_expr, None, left_ty.as_ref(), false, out)?;
                // A constructor on the right (`Msg::Data(3)`, `Some(Ok(9))`, `[1, 2]`)
                // is built at the left side's type; a value that already has its
                // type is read as before, without imposing the left side's context.
                let right = if self.needs_target_type(&right_expr)? {
                    match left_ty.as_ref() {
                        Some(ty) => self.lower_value(&right_expr, ty, out)?,
                        None => self.lower_expr(&right_expr, None, right_expr.span())?,
                    }
                } else {
                    self.lower_expr(&right_expr, None, right_expr.span())?
                };
                if let Some(left_ty) = &left.ty {
                    self.check_aggregate_shape(left_ty, &right, mac.span())?;
                } else if let Some(right_ty) = &right.ty {
                    self.check_aggregate_shape(right_ty, &left, mac.span())?;
                }
                // An enum-shaped comparison is built part by part; the failure line
                // still prints both operands as whole values.
                let condition = match left.ty.as_ref().or(right.ty.as_ref()) {
                    Some(ty) if contains_enum(ty) => {
                        self.typed_comparison(&left_expr, &right_expr, mac.span())?
                    }
                    _ => None,
                };
                let (left, right) = self.align_comparable(left, right, mac.span(), "assert_eq!")?;
                let result = self.emit_assert_eq(
                    out,
                    mac.span(),
                    &left,
                    &right,
                    message.as_deref(),
                    condition.as_deref(),
                );
                self.forget_nets(left_nets);
                self.forget_nets(right_nets);
                result
            }
            other => Err(self.error(
                mac.path.span(),
                format!(
                    "unknown testbench macro `{other}!`; supported macros are `initial!`, \
                     `always!`, `clock!`, `reset!`, `timeout!`, `pass!`, `finish!`, `nb_assign!`, \
                     `assert!` and `assert_eq!`"
                ),
            )),
        }
    }

    fn one_condition(&self, mac: &Macro, what: &str) -> Result<(Expr, Option<String>)> {
        let expressions = parse_expr_list(mac, self)?;
        match expressions.len() {
            1 => Ok((
                expressions.into_iter().next().expect("checked length"),
                None,
            )),
            2 => {
                let mut expressions = expressions.into_iter();
                let condition = expressions.next().expect("checked length");
                let message = expressions.next().expect("checked length");
                Ok((condition, Some(self.message_literal(&message)?)))
            }
            _ => Err(self.error(
                mac.span(),
                format!("`{what}` takes a condition and an optional message"),
            )),
        }
    }

    fn two_conditions(&self, mac: &Macro, what: &str) -> Result<(Expr, Expr, Option<String>)> {
        let expressions = parse_expr_list(mac, self)?;
        match expressions.len() {
            2 | 3 => {
                let mut expressions = expressions.into_iter();
                let left = expressions.next().expect("checked length");
                let right = expressions.next().expect("checked length");
                let message = match expressions.next() {
                    Some(message) => Some(self.message_literal(&message)?),
                    None => None,
                };
                Ok((left, right, message))
            }
            _ => Err(self.error(
                mac.span(),
                format!("`{what}` takes two values and an optional message"),
            )),
        }
    }

    fn message_literal(&self, expr: &Expr) -> Result<String> {
        match expr {
            Expr::Lit(syn::ExprLit {
                lit: Lit::Str(text),
                ..
            }) => Ok(text.value()),
            other => Err(self.error(
                other.span(),
                "the assertion message must be a string literal",
            )),
        }
    }

    fn emit_assert(
        &self,
        out: &mut Emitter,
        span: Span,
        condition: &Lowered,
        message: Option<&str>,
    ) -> Result<()> {
        let located = self.location(span)?;
        let (file, line) = (escape_verilog_string(&located.file), located.line);
        let mut format = format!(
            "SHYHDL_FAIL test={} case=%0d check=assert file={file} line={line}",
            self.test
        );
        if let Some(message) = message {
            format.push_str(&format!(" msg=\"{}\"", escape_verilog_string(message)));
        }
        out.open(format!("if (({}) !== 1'b1) begin", condition.text));
        out.push(format!("{FAILED_FLAG} = 1'b1;"));
        out.push(format!("{FAIL_COUNTER} = {FAIL_COUNTER} + 1;"));
        out.push(format!("$display(\"{format}\", {CASE_COUNTER} + 1);"));
        out.push("$fflush();");
        out.close("end");
        Ok(())
    }

    fn emit_assert_eq(
        &self,
        out: &mut Emitter,
        span: Span,
        left: &Lowered,
        right: &Lowered,
        message: Option<&str>,
        condition: Option<&str>,
    ) -> Result<()> {
        let located = self.location(span)?;
        let (file, line) = (escape_verilog_string(&located.file), located.line);
        let format_spec = value_format(left.width.max(right.width));
        let mut format = format!(
            "SHYHDL_FAIL test={} case=%0d check=assert_eq file={file} line={line} expected={} \
             actual={}",
            self.test, format_spec, format_spec
        );
        if let Some(message) = message {
            format.push_str(&format!(" msg=\"{}\"", escape_verilog_string(message)));
        }
        match condition {
            // A typed comparison is built from parts, so the whole-value check is
            // not the one to run.
            Some(condition) => out.open(format!("if (!({condition})) begin")),
            None => out.open(format!("if (({}) !== ({})) begin", left.text, right.text)),
        }
        out.push(format!("{FAILED_FLAG} = 1'b1;"));
        out.push(format!("{FAIL_COUNTER} = {FAIL_COUNTER} + 1;"));
        out.push(format!(
            "$display(\"{format}\", {CASE_COUNTER} + 1, {}, {});",
            right.text, left.text
        ));
        out.push("$fflush();");
        out.close("end");
        Ok(())
    }

    /// Print the result summary and end the simulation.
    fn emit_completion(&self, out: &mut Emitter, reason: &str) {
        out.open(format!("if ({FAILED_FLAG} == 1'b0) begin"));
        out.push(format!("{PASS_COUNTER} = {PASS_COUNTER} + 1;"));
        out.push(format!(
            "$display(\"SHYHDL_PASS test={} case={reason}\");",
            self.test
        ));
        out.close("end");
        out.push(self.summary_display());
        out.push("$fflush();");
        out.push("$finish;");
    }

    fn summary_display(&self) -> String {
        let expected = match self.expected_passes {
            Some(count) => count.to_string(),
            None => "unknown".to_string(),
        };
        format!(
            "$display(\"SHYHDL_SUMMARY test={} pass=%0d fail=%0d markers=%0d \
             expected_markers={expected}\", {PASS_COUNTER}, {FAIL_COUNTER}, {CASE_COUNTER});",
            self.test
        )
    }

    /// Source location of a construct. A span without a location is reported
    /// instead of being guessed: a wrong `file:line` in the generated testbench
    /// and in the assertion output is worse than a failed generation
    /// (`Error::MissingSourceLocation`, REQ-VER-007).
    /// `file:line` for a generated comment, safe to paste after `//`.
    fn location_text(&self, span: Span) -> Result<String> {
        let location = self.location(span)?;
        Ok(format!(
            "{}:{}",
            sanitize_comment(&location.file),
            location.line
        ))
    }

    fn location(&self, span: Span) -> Result<SourceSpan> {
        SourceSpan::from_proc_macro2(&self.source_name, span)
    }

    /// When the test has no explicit `finish!()`, the run ends when every
    /// `initial!` block has returned. A test that never returns (a clock that
    /// runs forever, a wait that never matches) reports no PASS at all.
    fn emit_completion_watcher(&mut self) {
        let blocks = self.initial_blocks;
        let mut emitter = Emitter::new(0);
        emitter
            .push("// Auto-completion: this test has no `finish!()`, so it is complete when every");
        emitter.push(format!(
            "// `initial!` block has returned ({} block(s)). A test that never returns is a",
            blocks
        ));
        emitter.push("// timeout, never a PASS.");
        emitter.open("initial begin");
        emitter.push(format!("wait ({BLOCKS_DONE} == {blocks});"));
        let mut inner = Emitter::new(1);
        self.emit_completion(&mut inner, "complete");
        emitter.lines.extend(inner.lines);
        emitter.close("end");
        self.procs.push(emitter.text().trim_end().to_string());
    }

    fn emit_timeout(&mut self, femtoseconds: u64, span: Span) -> Result<()> {
        let ticks = self.delay_text(femtoseconds, span)?;
        let mut emitter = Emitter::new(0);
        emitter.push(format!(
            "// timeout!() at {}: the run is a timeout, never a pass",
            self.location_text(span)?
        ));
        emitter.open("initial begin");
        emitter.push(format!("#{ticks};"));
        emitter.push(format!(
            "$display(\"SHYHDL_TIMEOUT test={} at=%0t\", $time);",
            self.test
        ));
        emitter.push(self.summary_display());
        emitter.push("$fflush();");
        emitter.push("$finish;");
        emitter.close("end");
        self.procs.push(emitter.text().trim_end().to_string());
        Ok(())
    }

    fn emit_clock(&mut self, args: ClockArgs, span: Span, what: &str) -> Result<()> {
        let target = self.clock_target(&args.signal, what)?;
        let signal = target.value;
        if signal.width != 1 {
            return Err(self.error(
                args.signal.span(),
                format!(
                    "a clock must be one bit wide, but `{}` is {} bits",
                    signal.text, signal.width
                ),
            ));
        }
        match signal.driver {
            Driver::DutOutput => {
                return Err(self.error(
                    args.signal.span(),
                    "a clock must be a DUT input; this port is an output",
                ))
            }
            Driver::ClockNet => {
                return Err(self.error(
                    args.signal.span(),
                    format!("`{}` already has a clock", signal.text),
                ))
            }
            Driver::ResetNet => {
                return Err(self.error(
                    args.signal.span(),
                    format!("`{}` is driven by `reset!`, not by `{what}`", signal.text),
                ))
            }
            _ => {}
        }
        let (high, low) = match (args.period, args.high, args.low) {
            (Some(period), None, None) => {
                if period == 0 {
                    return Err(self.error(
                        span,
                        "a clock period of zero would repeat without time passing; give a positive \
                         period",
                    ));
                }
                if !period.is_multiple_of(2) {
                    // `gen_clk` only ever generates a 50% duty clock, so an odd
                    // period has to be written out with the explicit wrapper.
                    let hint = if what == "gen_clk" {
                        "`gen_clk` generates a 50% duty clock, so its period has to be even; \
                         write the clock explicitly instead: `clock!(<signal>, high = fs(..), \
                         low = fs(..))`, with durations your precision can represent exactly"
                    } else {
                        "use `high = ns(..), low = ns(..)`"
                    };
                    return Err(self.error(
                        span,
                        format!(
                            "the clock period of {period} fs is odd, so it cannot be split into \
                             two equal phases; {hint}"
                        ),
                    ));
                }
                (period / 2, period / 2)
            }
            (None, Some(high), Some(low)) => {
                if high == 0 || low == 0 {
                    return Err(self.error(
                        span,
                        "a clock phase of zero would make the clock spin without time passing; \
                         give both phases a positive duration",
                    ));
                }
                (high, low)
            }
            (None, _, _) => {
                return Err(self.error(
                    span,
                    "a clock needs either `period = ns(..)` or both `high = ns(..)` and \
                     `low = ns(..)`",
                ))
            }
            (Some(_), _, _) => {
                return Err(self.error(span, "give either `period` or `high`/`low`, not both"))
            }
        };
        let high = self.delay_text(high, span)?;
        let low = self.delay_text(low, span)?;
        let net = signal.text.clone();
        let initial = if args.initial { "1'b1" } else { "1'b0" };
        // The clock's identity for the recording: the exact path the simulator
        // dumps it under, so a debugger selects clock mode from the manifest and
        // never has to guess a suffix (or rely on a port marking).
        let signal_path = format!("TOP.{}.{}.{}", self.top, target.instance, target.port);
        self.clocks.push((target.var, signal_path.clone()));
        let mut emitter = Emitter::new(0);
        emitter.push(format!(
            "// {what} at {}: {net} starts at {initial}, {high} high / {low} low, recorded as \
             {signal_path}",
            self.location_text(span)?
        ));
        emitter.open("initial begin");
        emitter.push(format!("{net} = {initial};"));
        emitter.open("forever begin");
        if args.initial {
            // Starts high: the level is high for `high`, then low for `low`.
            emitter.push(format!("#{high} {net} = 1'b0;"));
            emitter.push(format!("#{low} {net} = 1'b1;"));
        } else {
            // Starts low: the level is low for `low`, then high for `high`.
            emitter.push(format!("#{low} {net} = 1'b1;"));
            emitter.push(format!("#{high} {net} = 1'b0;"));
        }
        emitter.close("end");
        emitter.close("end");
        self.procs.push(emitter.text().trim_end().to_string());
        self.clock_nets.insert(net);
        Ok(())
    }

    /// Resolve the clock argument, which must be `instance.port`, and report both
    /// the net and the identity the recording will use.
    fn clock_target(&self, expr: &Expr, what: &str) -> Result<ClockTarget> {
        let needs = |generator: &Self, span: Span| {
            generator.error(
                span,
                format!("`{what}` needs a DUT clock port, for example `dut.clk`"),
            )
        };
        let Expr::Field(field) = expr else {
            return Err(self.error(
                expr.span(),
                format!("`{what}` needs a DUT clock port, for example `dut.clk`"),
            ));
        };
        let syn::Member::Named(port_name) = &field.member else {
            return Err(needs(self, field.span()));
        };
        let Expr::Path(path) = field.base.as_ref() else {
            return Err(needs(self, field.span()));
        };
        let segment = match (path.qself.is_none(), path.path.segments.len()) {
            (true, 1) if matches!(path.path.segments[0].arguments, syn::PathArguments::None) => {
                &path.path.segments[0]
            }
            _ => return Err(needs(self, field.span())),
        };
        let var = segment.ident.to_string();
        let Some(Name::Instance { index }) = self.name(&var) else {
            return Err(needs(self, field.span()));
        };
        let value = self
            .try_value_ref(expr)?
            .ok_or_else(|| needs(self, expr.span()))?;
        let instance = self.instance(*index);
        let port = instance
            .ports
            .iter()
            .find(|binding| *port_name == binding.port.source_name)
            .map(|binding| binding.port.verilog_name.clone())
            .ok_or_else(|| {
                self.error(
                    field.span(),
                    format!(
                        "`{var}.{port_name}` is not a port of module `{}`; known ports: {}",
                        instance.module,
                        instance
                            .ports
                            .iter()
                            .map(|binding| binding.port.source_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;
        Ok(ClockTarget {
            value,
            var,
            instance: instance.inst_name.clone(),
            port,
        })
    }

    fn emit_reset(&mut self, args: ResetArgs, span: Span) -> Result<()> {
        let signal = self.try_value_ref(&args.signal)?.ok_or_else(|| {
            self.error(
                args.signal.span(),
                "`reset!` needs a DUT reset port, for example `dut.rst_n`",
            )
        })?;
        if signal.width != 1 {
            return Err(self.error(
                args.signal.span(),
                format!(
                    "a reset must be one bit wide, but `{}` is {} bits",
                    signal.text, signal.width
                ),
            ));
        }
        match signal.driver {
            Driver::DutOutput => {
                return Err(self.error(
                    args.signal.span(),
                    "a reset must be a DUT input; this port is an output",
                ))
            }
            Driver::ResetNet => {
                return Err(self.error(
                    args.signal.span(),
                    format!("`{}` already has a reset process", signal.text),
                ))
            }
            Driver::ClockNet => {
                return Err(self.error(
                    args.signal.span(),
                    format!("`{}` is driven by `clock!`, not by `reset!`", signal.text),
                ))
            }
            _ => {}
        }
        if args.hold == 0 {
            return Err(self.error(span, "`reset!` needs a positive `hold = ns(..)`"));
        }
        // The inactive level is driven for `setup` so the following 1 -> 0
        // transition is real and observable (REQ-TEST-011), never inferred from
        // an X initialisation.
        let setup = args.setup.unwrap_or_else(|| self.timeunit.femtoseconds());
        let setup_text = self.delay_text(setup, span)?;
        let hold_text = self.delay_text(args.hold, span)?;
        let (inactive, active, direction) = if args.active_low {
            ("1'b1", "1'b0", "asserts low (1 -> 0)")
        } else {
            ("1'b0", "1'b1", "asserts high (0 -> 1)")
        };
        let net = signal.text.clone();
        let mut emitter = Emitter::new(0);
        emitter.push(format!(
            "// reset!() at {}: {net} {direction}, held for {hold_text}",
            self.location_text(span)?
        ));
        emitter.open("initial begin");
        emitter.push(format!("{net} = {inactive};"));
        emitter.push(format!("#{setup_text} {net} = {active};"));
        emitter.push(format!("#{hold_text} {net} = {inactive};"));
        emitter.close("end");
        self.procs.push(emitter.text().trim_end().to_string());
        self.reset_nets.insert(net);
        Ok(())
    }

    // -------------------------------------------------------------------- render

    fn render(&self) -> String {
        let mut emitter = Emitter::new(0);
        emitter.push(format!(
            "// Generated by ShyHDL {} from {}:{}:{} -- testbench for `{}`. Do not edit.",
            env!("CARGO_PKG_VERSION"),
            self.source_span.file,
            self.source_span.line,
            self.source_span.column,
            self.test
        ));
        emitter.push(format!(
            "// DUT: module `{}` (instances: {})",
            self.design.top,
            self.instances
                .iter()
                .map(|instance| format!("{} = {}", instance.var, instance.inst_name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        emitter.push(format!(
            "// Timing: `timescale {}/{}; delays are exact multiples of the precision.",
            self.timeunit, self.precision
        ));
        emitter.push("// Results: SHYHDL_PASS / SHYHDL_FAIL / SHYHDL_SUMMARY lines; after a failed assertion");
        emitter.push("// no further SHYHDL_PASS is printed.");
        emitter.push(String::new());
        emitter.push(format!("`timescale {}/{}", self.timeunit, self.precision));
        emitter.push(String::new());
        emitter.open(format!("module {};", self.top));
        for declaration in &self.declarations {
            emitter.push(declaration.clone());
        }
        if !self.declarations.is_empty() {
            emitter.push(String::new());
        }
        for name in &self.loop_vars {
            emitter.push(format!("integer {name};"));
        }
        if !self.loop_vars.is_empty() {
            emitter.push(String::new());
        }
        emitter.push("// Result protocol counters.");
        emitter.push(format!("integer {CASE_COUNTER} = 0;"));
        emitter.push(format!("integer {PASS_COUNTER} = 0;"));
        emitter.push(format!("integer {FAIL_COUNTER} = 0;"));
        emitter.push(format!("reg     {FAILED_FLAG} = 1'b0;"));
        if !self.has_explicit_finish && self.initial_blocks > 0 {
            emitter.push(format!("integer {BLOCKS_DONE} = 0;"));
        }
        emitter.push(String::new());
        for instance in &self.instances {
            emitter.push(format!("// {} = {}", instance.var, instance.module));
            emitter.open(format!("{} {} (", instance.module, instance.inst_name));
            let last = instance.ports.len().saturating_sub(1);
            for (position, binding) in instance.ports.iter().enumerate() {
                let comma = if position == last { "" } else { "," };
                emitter.push(format!(
                    ".{}({}){comma}",
                    binding.port.verilog_name, binding.net
                ));
            }
            emitter.close(");");
            emitter.push(String::new());
        }
        for procedure in &self.procs {
            for line in procedure.lines() {
                if line.is_empty() {
                    emitter.push(String::new());
                } else {
                    emitter.push(format!("  {line}"));
                }
            }
            emitter.push(String::new());
        }
        emitter.close("endmodule");
        emitter.text()
    }
}

// ---------------------------------------------------------------------- wrappers

/// `clock!(dut.clk, period = ns(10), initial = false)`.
struct ClockArgs {
    signal: Expr,
    period: Option<u64>,
    high: Option<u64>,
    low: Option<u64>,
    initial: bool,
}

impl Parse for ClockArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let signal: Expr = input.parse()?;
        let mut args = ClockArgs {
            signal,
            period: None,
            high: None,
            low: None,
            initial: false,
        };
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "period" => args.period = Some(parse_time_stream(input, &key)?),
                "high" => args.high = Some(parse_time_stream(input, &key)?),
                "low" => args.low = Some(parse_time_stream(input, &key)?),
                "initial" => {
                    let value: Expr = input.parse()?;
                    args.initial = match value {
                        Expr::Lit(syn::ExprLit {
                            lit: Lit::Bool(value),
                            ..
                        }) => value.value,
                        other => {
                            return Err(syn::Error::new(
                                other.span(),
                                "`initial` must be `true` or `false`",
                            ))
                        }
                    };
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `clock!` option `{other}`; expected `period`, `high`, `low` \
                             or `initial`"
                        ),
                    ))
                }
            }
        }
        Ok(args)
    }
}

/// `reset!(dut.rst_n, active_low, hold = ns(20))`.
struct ResetArgs {
    signal: Expr,
    active_low: bool,
    hold: u64,
    /// Time the inactive level is held before the active level is driven.
    /// `None` means one time unit.
    setup: Option<u64>,
}

impl Parse for ResetArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let signal: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let polarity: Ident = input.parse()?;
        let active_low = match polarity.to_string().as_str() {
            "active_low" => true,
            "active_high" => false,
            other => {
                return Err(syn::Error::new(
                    polarity.span(),
                    format!("expected `active_low` or `active_high`, found `{other}`"),
                ))
            }
        };
        let mut hold = None;
        let mut setup = None;
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "hold" => hold = Some(parse_time_stream(input, &key)?),
                "setup" => setup = Some(parse_time_stream(input, &key)?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown `reset!` option `{other}`; expected `hold` or `setup`"),
                    ))
                }
            }
        }
        let hold = hold.ok_or_else(|| {
            syn::Error::new(
                signal.span(),
                "`reset!` needs `hold = ns(..)`: the reset level must stay active across at least \
                 one clock edge (REQ-TEST-011)",
            )
        })?;
        Ok(ResetArgs {
            signal,
            active_low,
            hold,
            setup,
        })
    }
}

/// `timeout!(ns(1000))`.
struct TimeoutArgs {
    duration: u64,
}

impl Parse for TimeoutArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let duration = parse_time_stream(input, &Ident::new("timeout", Span::call_site()))?;
        Ok(TimeoutArgs { duration })
    }
}

/// Parse a time value inside macro arguments, for example `period = ns(10)`.
fn parse_time_stream(input: ParseStream<'_>, key: &Ident) -> syn::Result<u64> {
    match time_value_stream(input)? {
        Some(femtoseconds) => Ok(femtoseconds),
        None => Err(syn::Error::new(
            key.span(),
            format!("`{key}` needs a time value with a unit, for example `{key} = ns(10)`"),
        )),
    }
}

/// A time value written as `ns(10)`, `ps(1)`, ... in a token stream.
fn time_value_stream(input: ParseStream<'_>) -> syn::Result<Option<u64>> {
    let expr: Expr = input.parse()?;
    match time_quantity(&expr) {
        TimeValue::Value(femtoseconds) => Ok(Some(femtoseconds)),
        TimeValue::Overflow => Err(syn::Error::new(
            expr.span(),
            "this time value overflows 64 bits of femtoseconds; use a smaller value",
        )),
        TimeValue::NotATime => Ok(None),
    }
}

/// A time value written as `ns(10)` in an expression position.
/// What an expression turned out to be when read as a time value.
enum TimeValue {
    /// Not a time value at all.
    NotATime,
    /// A time value in femtoseconds.
    Value(u64),
    /// A time value that does not fit in 64 bits of femtoseconds. Reported instead
    /// of wrapping (a silently wrong delay) or panicking.
    Overflow,
}

/// A time value written as `ns(10)` in an expression position.
fn time_quantity(expr: &Expr) -> TimeValue {
    let Expr::Call(call) = expr else {
        return TimeValue::NotATime;
    };
    let Some(ident) = call_path_ident(&call.func) else {
        return TimeValue::NotATime;
    };
    let Some(unit) = time_femtoseconds(&ident.to_string()) else {
        return TimeValue::NotATime;
    };
    if call.args.len() != 1 {
        return TimeValue::NotATime;
    }
    let magnitude = match &call.args[0] {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Int(value),
            ..
        }) => match value.base10_parse::<u64>() {
            Ok(magnitude) => magnitude,
            Err(_) => return TimeValue::Overflow,
        },
        _ => return TimeValue::NotATime,
    };
    match unit.checked_mul(magnitude) {
        Some(femtoseconds) => TimeValue::Value(femtoseconds),
        None => TimeValue::Overflow,
    }
}

/// One second in femtoseconds: the exact reference a frequency is converted with.
const FEMTOSECONDS_PER_SECOND: u64 = 1_000_000_000_000_000;

/// Multiplier of a frequency unit; the common case aliases are accepted because
/// `MHz(100)` reads like the unit it names.
fn frequency_multiplier(unit: &str) -> Option<u64> {
    match unit {
        "hz" | "Hz" => Some(1),
        "khz" | "kHz" => Some(1_000),
        "mhz" | "MHz" => Some(1_000_000),
        "ghz" | "GHz" => Some(1_000_000_000),
        _ => None,
    }
}

fn time_femtoseconds(unit: &str) -> Option<u64> {
    match unit {
        "s" => Some(1_000_000_000_000_000),
        "ms" => Some(1_000_000_000_000),
        "us" => Some(1_000_000_000),
        "ns" => Some(1_000_000),
        "ps" => Some(1_000),
        "fs" => Some(1),
        _ => None,
    }
}

fn parse_macro_args<T: Parse>(mac: &Macro, generator: &Generator<'_>) -> Result<T> {
    syn::parse2(mac.tokens.clone())
        .map_err(|source| generator.error(source.span(), source.to_string()))
}

/// Comma separated expressions inside a macro: `Punctuated` itself is not a
/// `Parse` implementation, so it is parsed through `parse_terminated`.
fn parse_expr_list(mac: &Macro, generator: &Generator<'_>) -> Result<Punctuated<Expr, Token![,]>> {
    Punctuated::<Expr, Token![,]>::parse_terminated
        .parse2(mac.tokens.clone())
        .map_err(|source| generator.error(source.span(), source.to_string()))
}

fn parse_statements(tokens: TokenStream) -> syn::Result<Vec<Stmt>> {
    let parser = |input: ParseStream<'_>| syn::Block::parse_within(input);
    parser.parse2(tokens)
}

fn call_path_ident(expr: &Expr) -> Option<&Ident> {
    let Expr::Path(path) = expr else { return None };
    let segment = path.path.segments.last()?;
    if path.path.segments.len() != 1 || !matches!(segment.arguments, syn::PathArguments::None) {
        return None;
    }
    Some(&segment.ident)
}

fn module_new_call(expr: &Expr) -> Option<(Ident, Span)> {
    let Expr::Call(call) = expr else { return None };
    let Expr::Path(path) = call.func.as_ref() else {
        return None;
    };
    if path.path.segments.len() != 2 || !path.path.segments[1].ident.eq("new") {
        return None;
    }
    if !call.args.is_empty() {
        return None;
    }
    Some((path.path.segments[0].ident.clone(), expr.span()))
}

/// Whether a type is an array, or a struct that contains one anywhere.
///
/// The exact-shape rule is deliberately narrowed to these: a plain struct of
/// scalars keeps its previous behaviour (a literal fill), so landing arrays does
/// not change any existing testbench's meaning.
fn contains_array(ty: &ValueType) -> bool {
    match ty {
        ValueType::Array { .. } => true,
        ValueType::Struct { fields, .. } => fields.iter().any(|field| contains_array(&field.ty)),
        ValueType::Enum { variants, .. } => variants.iter().any(|variant| {
            variant
                .payload
                .as_ref()
                .is_some_and(|payload| contains_array(payload))
        }),
        _ => false,
    }
}

/// Whether lowering this expression needs statements of its own.
///
/// `match`, a value `if` and a block are statements; an operator needs them only
/// because one of its operands does. Anything else is a plain expression and keeps
/// its existing lowering, so a test without these constructs generates exactly the
/// same Verilog as before.
fn needs_statements(expr: &Expr) -> bool {
    match expr {
        Expr::Paren(paren) => needs_statements(&paren.expr),
        Expr::Group(group) => needs_statements(&group.expr),
        Expr::Unary(unary) => needs_statements(&unary.expr),
        Expr::Binary(binary) => needs_statements(&binary.left) || needs_statements(&binary.right),
        Expr::Cast(cast) => needs_statements(&cast.expr),
        Expr::Index(index) => needs_statements(&index.expr) || needs_statements(&index.index),
        Expr::Field(field) => needs_statements(&field.base),
        Expr::MethodCall(call) => {
            needs_statements(&call.receiver) || call.args.iter().any(needs_statements)
        }
        Expr::Call(call) => needs_statements(&call.func) || call.args.iter().any(needs_statements),
        Expr::Array(array) => array.elems.iter().any(needs_statements),
        Expr::Repeat(repeat) => needs_statements(&repeat.expr) || needs_statements(&repeat.len),
        Expr::Match(_) | Expr::If(_) | Expr::Block(_) => true,
        _ => false,
    }
}

/// The trailing expression of a block used as a value.
fn block_tail(block: &syn::Block) -> Option<&Expr> {
    match block.stmts.last() {
        Some(Stmt::Expr(expr, None)) => Some(expr),
        _ => None,
    }
}

/// The expression an arm body or an `if` branch produces as a value, if any.
fn arm_tail(body: &Expr) -> Option<&Expr> {
    match body {
        Expr::Block(block) => block_tail(&block.block),
        _ => Some(body),
    }
}

/// A read of a generated net, so a materialized value goes through the same width
/// and type rules as any other signal.
fn net_expr(name: &str) -> Expr {
    Expr::Path(syn::ExprPath {
        attrs: Vec::new(),
        qself: None,
        path: syn::Path::from(Ident::new(name, Span::call_site())),
    })
}

/// A short, readable spelling of a type for diagnostics.
fn describe_type(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "bool".to_string(),
        ValueType::UInt { width } => format!("uint<{width}>"),
        ValueType::SInt { width } => format!("int<{width}>"),
        ValueType::Struct { name, .. } => format!("struct {name}"),
        ValueType::Array { element, len } => format!("[{}; {len}]", describe_type(element)),
        ValueType::Enum { name, .. } => format!("enum {name}"),
    }
}

/// Every enum type reachable in a type: itself, or inside a struct field, an array
/// element or a variant payload. The design's port types are the only enums a
/// testbench can name.
fn collect_enums(ty: &ValueType, out: &mut Vec<ValueType>) {
    match ty {
        ValueType::Enum { variants, .. } => {
            if !out.contains(ty) {
                out.push(ty.clone());
            }
            for variant in variants {
                if let Some(payload) = &variant.payload {
                    collect_enums(payload, out);
                }
            }
        }
        ValueType::Struct { fields, .. } => {
            for field in fields {
                collect_enums(&field.ty, out);
            }
        }
        ValueType::Array { element, .. } => collect_enums(element, out),
        _ => {}
    }
}

/// The names of enum types, for a diagnostic that has to name them.
fn enum_names(types: &[ValueType]) -> Vec<&str> {
    types.iter().map(enum_name).collect()
}

/// The name the metadata gives an enum, or the empty string for another type.
fn enum_name(ty: &ValueType) -> &str {
    match ty {
        ValueType::Enum { name, .. } => name,
        _ => "",
    }
}

/// The head of an enum's metadata name: the part before any `<`, so a builtin
/// spelled `Option<uint<4>>` is still rooted at `Option`.
fn enum_head(ty: &ValueType) -> &str {
    enum_name(ty).split('<').next().unwrap_or_default()
}

/// The payload types of an enum's payload-carrying variants, in declaration
/// order: the shape a written parameter list such as `Result<T, E>` has to name.
fn enum_payloads(ty: &ValueType) -> Vec<ValueType> {
    let ValueType::Enum { variants, .. } = ty else {
        return Vec::new();
    };
    variants
        .iter()
        .filter_map(|variant| variant.payload.as_ref().map(|payload| (**payload).clone()))
        .collect()
}

/// Why the shared coverage analysis did not prove a `match` total.
fn describe_missing(ty: &ValueType, missing: &[Missing]) -> String {
    if missing.is_empty() {
        return format!(
            "this `match` on {} is not exhaustive; add a `_` arm to cover the rest",
            describe_type(ty)
        );
    }
    let names = missing
        .iter()
        .map(|missing| match missing {
            Missing::Bool(value) => format!("`{value}`"),
            Missing::Int(value) => format!("`{value}`"),
            Missing::Variant { name, .. } => format!("variant `{name}`"),
            Missing::Invalid => "the tag values that name no variant".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "this `match` on {} is not exhaustive: nothing covers {names}; cover them or add a `_` arm",
        describe_type(ty)
    )
}

/// The text of `width` bits at `offset` inside a `match` scrutinee.
///
/// The scrutinee is always one plain net, so this is a legal Verilog-2005
/// part-select; no pattern ever produces a select of a select.
fn bits_text(scrutinee: &str, scrutinee_width: u32, offset: u32, width: u32) -> String {
    if offset == 0 && width == scrutinee_width {
        return scrutinee.to_string();
    }
    if width == 1 {
        return format!("{scrutinee}[{offset}]");
    }
    format!("{scrutinee}[{}:{}]", offset + width - 1, offset)
}

/// A pattern test as a Verilog condition over the scrutinee's own bits.
fn test_text(test: &Test, scrutinee: &str, scrutinee_width: u32) -> String {
    match test {
        Test::AlwaysTrue => "1'b1".to_string(),
        Test::Bits {
            offset,
            width,
            value,
        } => format!(
            "{} == {width}'d{value}",
            bits_text(scrutinee, scrutinee_width, *offset, *width)
        ),
        Test::Range {
            offset,
            width,
            lo,
            hi,
        } => format!(
            "(({} >= {width}'d{lo}) && ({} <= {width}'d{hi}))",
            bits_text(scrutinee, scrutinee_width, *offset, *width),
            bits_text(scrutinee, scrutinee_width, *offset, *width)
        ),
        Test::Mask {
            offset,
            width,
            care,
            value,
        } => {
            let segments = crate::pattern::care_segments(*care, *width);
            if segments.is_empty() {
                return "1'b1".to_string();
            }
            segments
                .iter()
                .map(|(lo, hi)| {
                    let segment_width = hi - lo;
                    let segment_value = (value >> lo) & crate::pattern::bit_mask(segment_width);
                    format!(
                        "{} == {segment_width}'d{segment_value}",
                        bits_text(scrutinee, scrutinee_width, offset + lo, segment_width)
                    )
                })
                .collect::<Vec<_>>()
                .join(" && ")
        }
        Test::All(parts) => parts
            .iter()
            .map(|part| test_text(part, scrutinee, scrutinee_width))
            .collect::<Vec<_>>()
            .join(" && "),
    }
}

/// A testbench has no build-time constants, so a bare identifier in a pattern is
/// never a constant: the shared analysis resolves it to a variant of the matched
/// enum when one has that name, and binds it otherwise.
struct NoConstants;

impl ConstResolver for NoConstants {
    fn resolve(&self, _ident: &syn::Ident) -> Option<u128> {
        None
    }
}

/// A signal declaration: `<kind>[ signed][ [w-1:0]] <name>;`.
///
/// A `int<N>` net is declared `signed`, which is what makes its comparisons,
/// printing and `>>>` use two's complement semantics in Verilog.
fn signal_declaration(keyword: &str, width: u32, signed: bool, name: &str) -> String {
    let signed = if signed { " signed" } else { "" };
    let range = packed_range(width);
    if range.is_empty() {
        format!("{keyword}{signed} {name};")
    } else {
        format!("{keyword}{signed} {range} {name};")
    }
}

/// The width and signedness a value of this type is used at.
fn context_of(width: u32, ty: &ValueType) -> WidthContext {
    WidthContext {
        width,
        signed: matches!(ty, ValueType::SInt { .. }),
    }
}

/// A packed range for a declaration: empty for one bit, `[w-1:0]` otherwise.
fn packed_range(width: u32) -> String {
    if width <= 1 {
        String::new()
    } else {
        format!("[{}:0]", width - 1)
    }
}

fn value_format(width: u32) -> &'static str {
    if width > 64 {
        "%0h"
    } else {
        "%0d"
    }
}

/// `for` bounds: start, end and the comparison to use.
fn range_parts(range: &ExprRange) -> Option<(u32, u32, syn::RangeLimits)> {
    let start = match &range.start {
        Some(expr) => literal_u32(expr)?,
        None => 0,
    };
    let end = range.end.as_ref().and_then(|expr| literal_u32(expr))?;
    Some((start, end, range.limits))
}

fn literal_u32(expr: &Expr) -> Option<u32> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Int(value),
            ..
        }) => value.base10_parse().ok(),
        Expr::Paren(paren) => literal_u32(&paren.expr),
        _ => None,
    }
}

/// Static trip count of a `for` bound, used to count reachable `pass!()`.
fn loop_trip(range: &ExprRange) -> Option<u64> {
    let (start, end, limits) = range_parts(range)?;
    let start = u64::from(start);
    let end = u64::from(end);
    let count = match limits {
        syn::RangeLimits::HalfOpen(_) => end.checked_sub(start)?,
        syn::RangeLimits::Closed(_) => end.checked_sub(start)?.checked_add(1)?,
    };
    Some(count)
}

/// What a static walk of the test body can know about completions.
struct Analysis {
    passes: Option<u64>,
    has_finish: bool,
}

fn analyze_block(
    source_name: &str,
    stmts: &[Stmt],
    multiplier: Option<u64>,
    analysis: &mut Analysis,
) -> Result<()> {
    for stmt in stmts {
        analyze_stmt(source_name, stmt, multiplier, analysis)?;
    }
    Ok(())
}

fn analyze_stmt(
    source_name: &str,
    stmt: &Stmt,
    multiplier: Option<u64>,
    analysis: &mut Analysis,
) -> Result<()> {
    match stmt {
        Stmt::Macro(item) => analyze_macro(source_name, &item.mac, multiplier, analysis),
        Stmt::Expr(expr, _) => analyze_expr(source_name, expr, multiplier, analysis),
        Stmt::Local(local) => {
            if let Some(init) = &local.init {
                analyze_expr(source_name, &init.expr, multiplier, analysis)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn analyze_expr(
    source_name: &str,
    expr: &Expr,
    multiplier: Option<u64>,
    analysis: &mut Analysis,
) -> Result<()> {
    match expr {
        Expr::Macro(item) => analyze_macro(source_name, &item.mac, multiplier, analysis),
        Expr::If(item) => {
            analyze_block(source_name, &item.then_branch.stmts, None, analysis)?;
            if let Some((_, else_branch)) = &item.else_branch {
                analyze_expr(source_name, else_branch, None, analysis)?;
            }
            Ok(())
        }
        Expr::Block(block) => analyze_block(source_name, &block.block.stmts, multiplier, analysis),
        // An assignment's value is always evaluated, so a completion marker inside a
        // `match`/`if` value there stays reachable exactly as the statement's is.
        Expr::Assign(item) => analyze_expr(source_name, &item.right, multiplier, analysis),
        Expr::Match(item) => {
            // Which arm runs is a run-time decision, so a `pass!()` under a
            // `match` makes the reachable-marker count unknown, exactly as under
            // an `if`.
            for arm in &item.arms {
                analyze_expr(source_name, &arm.body, None, analysis)?;
            }
            Ok(())
        }
        Expr::ForLoop(item) => {
            let trip = match item.expr.as_ref() {
                Expr::Range(range) => loop_trip(range),
                _ => None,
            };
            let nested = match (multiplier, trip) {
                (Some(multiplier), Some(trip)) => multiplier.checked_mul(trip),
                _ => None,
            };
            analyze_block(source_name, &item.body.stmts, nested, analysis)
        }
        _ => Ok(()),
    }
}

fn analyze_macro(
    source_name: &str,
    mac: &Macro,
    multiplier: Option<u64>,
    analysis: &mut Analysis,
) -> Result<()> {
    let Some(ident) = mac.path.get_ident() else {
        return Ok(());
    };
    match ident.to_string().as_str() {
        "pass" => {
            analysis.passes = match (analysis.passes, multiplier) {
                (Some(total), Some(count)) => Some(total + count),
                _ => None,
            };
            Ok(())
        }
        "initial" => {
            let stmts =
                parse_statements(mac.tokens.clone()).map_err(|source| Error::Unsupported {
                    span: SourceSpan::from_proc_macro2(source_name, source.span())
                        .unwrap_or_else(|_| SourceSpan::new(source_name, 1, 1)),
                    message: source.to_string(),
                })?;
            analyze_block(source_name, &stmts, multiplier, analysis)
        }
        "always" => {
            let stmts =
                parse_statements(mac.tokens.clone()).map_err(|source| Error::Unsupported {
                    span: SourceSpan::from_proc_macro2(source_name, source.span())
                        .unwrap_or_else(|_| SourceSpan::new(source_name, 1, 1)),
                    message: source.to_string(),
                })?;
            analyze_block(source_name, &stmts, None, analysis)
        }
        // The value macros take expressions, and a `match`/`if` value in one can run
        // completion markers of its own: they are counted like any other branch.
        "assert" | "assert_eq" | "nb_assign" => {
            let expressions = Punctuated::<Expr, Token![,]>::parse_terminated
                .parse2(mac.tokens.clone())
                .map_err(|source| Error::Unsupported {
                    span: SourceSpan::from_proc_macro2(source_name, source.span())
                        .unwrap_or_else(|_| SourceSpan::new(source_name, 1, 1)),
                    message: source.to_string(),
                })?;
            for expression in expressions {
                analyze_expr(source_name, &expression, multiplier, analysis)?;
            }
            Ok(())
        }
        "finish" | "timeout" => {
            analysis.has_finish = true;
            Ok(())
        }
        _ => Ok(()),
    }
}

// ------------------------------------------------------------------ small helpers

fn generic_width(segment: &syn::PathSegment) -> Option<u32> {
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    if arguments.args.len() != 1 {
        return None;
    }
    match arguments.args.first()? {
        syn::GenericArgument::Const(Expr::Lit(syn::ExprLit {
            lit: Lit::Int(value),
            ..
        })) => value.base10_parse().ok(),
        _ => None,
    }
}

fn sanitize_identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty()
        || out
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_digit())
    {
        out.insert(0, '_');
    }
    out
}

fn is_verilog_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Words a backend may treat as reserved.
///
/// Verilator parses a SystemVerilog superset and rejects identifiers that are
/// legal Verilog-2005 keywords only in name, so the full IEEE 1800-2017 keyword
/// list is refused: a generated testbench that a real tool will not compile is a
/// bug in the generator (REQ-SIM-010 keeps language semantics with the spec, and
/// the generated names must still compile there).
fn is_verilog_keyword(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "accept_on",
        "alias",
        "always",
        "always_comb",
        "always_ff",
        "always_latch",
        "and",
        "assert",
        "assign",
        "assume",
        "automatic",
        "before",
        "begin",
        "bind",
        "bins",
        "binsof",
        "bit",
        "break",
        "buf",
        "bufif0",
        "bufif1",
        "byte",
        "case",
        "casex",
        "casez",
        "cell",
        "chandle",
        "checker",
        "class",
        "clocking",
        "cmos",
        "config",
        "const",
        "constraint",
        "context",
        "continue",
        "cover",
        "covergroup",
        "coverpoint",
        "cross",
        "deassign",
        "default",
        "defparam",
        "design",
        "disable",
        "dist",
        "do",
        "edge",
        "else",
        "end",
        "endcase",
        "endchecker",
        "endclass",
        "endclocking",
        "endconfig",
        "endfunction",
        "endgenerate",
        "endgroup",
        "endinterface",
        "endmodule",
        "endpackage",
        "endprimitive",
        "endprogram",
        "endproperty",
        "endspecify",
        "endsequence",
        "endtable",
        "endtask",
        "enum",
        "event",
        "eventually",
        "expect",
        "export",
        "extends",
        "extern",
        "final",
        "first_match",
        "for",
        "force",
        "foreach",
        "forever",
        "fork",
        "forkjoin",
        "function",
        "generate",
        "genvar",
        "global",
        "highz0",
        "highz1",
        "if",
        "iff",
        "ifnone",
        "ignore_bins",
        "illegal_bins",
        "implements",
        "implies",
        "import",
        "incdir",
        "include",
        "initial",
        "inout",
        "input",
        "inside",
        "instance",
        "int",
        "integer",
        "interconnect",
        "interface",
        "intersect",
        "join",
        "join_any",
        "join_none",
        "large",
        "let",
        "liblist",
        "library",
        "local",
        "localparam",
        "logic",
        "longint",
        "macromodule",
        "matches",
        "medium",
        "modport",
        "module",
        "nand",
        "negedge",
        "nettype",
        "new",
        "nexttime",
        "nmos",
        "nor",
        "noshowcancelled",
        "not",
        "notif0",
        "notif1",
        "null",
        "or",
        "output",
        "package",
        "packed",
        "parameter",
        "pmos",
        "posedge",
        "primitive",
        "priority",
        "program",
        "property",
        "protected",
        "pull0",
        "pull1",
        "pulldown",
        "pullup",
        "pulsestyle_ondetect",
        "pulsestyle_onevent",
        "pure",
        "rand",
        "randc",
        "randcase",
        "randsequence",
        "rcmos",
        "real",
        "realtime",
        "ref",
        "reg",
        "reject_on",
        "release",
        "repeat",
        "restrict",
        "return",
        "rnmos",
        "rpmos",
        "rtran",
        "rtranif0",
        "rtranif1",
        "s_always",
        "s_eventually",
        "s_nexttime",
        "s_until",
        "s_until_with",
        "scalared",
        "sequence",
        "shortint",
        "shortreal",
        "showcancelled",
        "signed",
        "small",
        "soft",
        "solve",
        "specify",
        "specparam",
        "static",
        "string",
        "strong",
        "strong0",
        "strong1",
        "struct",
        "super",
        "supply0",
        "supply1",
        "sync_accept_on",
        "sync_reject_on",
        "table",
        "tagged",
        "task",
        "this",
        "throughout",
        "time",
        "timeprecision",
        "timeunit",
        "tran",
        "tranif0",
        "tranif1",
        "tri",
        "tri0",
        "tri1",
        "triand",
        "trior",
        "trireg",
        "type",
        "typedef",
        "union",
        "unique",
        "unique0",
        "unsigned",
        "until",
        "until_with",
        "untyped",
        "use",
        "uwire",
        "var",
        "vectored",
        "virtual",
        "void",
        "wait",
        "wait_order",
        "wand",
        "weak",
        "weak0",
        "weak1",
        "while",
        "wildcard",
        "wire",
        "with",
        "within",
        "wor",
        "xnor",
        "xor",
    ];
    KEYWORDS.contains(&name)
}

/// Keep a file name safe inside a `//` comment: a control character would
/// otherwise split or corrupt the generated line.
fn sanitize_comment(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn escape_verilog_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// Render an expression for a diagnostic message.
pub(super) fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Path(path) => path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            Lit::Int(value) => value.to_string(),
            Lit::Str(value) => format!("\"{}\"", value.value()),
            Lit::Bool(value) => value.value.to_string(),
            other => format!("{other:?}"),
        },
        Expr::Field(field) => {
            let member = match &field.member {
                syn::Member::Named(ident) => ident.to_string(),
                syn::Member::Unnamed(index) => index.index.to_string(),
            };
            format!("{}.{}", render_expr(&field.base), member)
        }
        Expr::Index(index) => format!(
            "{}[{}]",
            render_expr(&index.expr),
            render_expr(&index.index)
        ),
        Expr::MethodCall(call) => format!(
            "{}.{}({})",
            render_expr(&call.receiver),
            call.method,
            call.args
                .iter()
                .map(render_expr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Call(call) => format!(
            "{}({})",
            render_expr(&call.func),
            call.args
                .iter()
                .map(render_expr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Paren(paren) => format!("({})", render_expr(&paren.expr)),
        Expr::Group(group) => render_expr(&group.expr),
        Expr::Unary(unary) => format!("{}{}", render_unop(&unary.op), render_expr(&unary.expr)),
        Expr::Binary(binary) => format!(
            "{} {} {}",
            render_expr(&binary.left),
            binary_operator_text(&binary.op),
            render_expr(&binary.right)
        ),
        Expr::Range(range) => {
            let start = range
                .start
                .as_ref()
                .map(|expr| render_expr(expr))
                .unwrap_or_default();
            let end = range
                .end
                .as_ref()
                .map(|expr| render_expr(expr))
                .unwrap_or_default();
            let limits = match range.limits {
                syn::RangeLimits::HalfOpen(_) => "..",
                syn::RangeLimits::Closed(_) => "..=",
            };
            format!("{start}{limits}{end}")
        }
        Expr::Macro(item) => format!(
            "{}!(..)",
            item.mac
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
                .unwrap_or_default()
        ),
        Expr::Assign(assign) => format!(
            "{} = {}",
            render_expr(&assign.left),
            render_expr(&assign.right)
        ),
        Expr::If(_) => "if ...".to_string(),
        Expr::ForLoop(_) => "for ...".to_string(),
        Expr::Match(_) => "match ...".to_string(),
        Expr::Block(_) => "block".to_string(),
        Expr::Reference(reference) => format!("&{}", render_expr(&reference.expr)),
        Expr::Array(_) => "array".to_string(),
        Expr::Tuple(_) => "tuple".to_string(),
        other => {
            let text = format!("{other:?}");
            text.split_whitespace()
                .take(6)
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

fn render_unop(op: &syn::UnOp) -> &'static str {
    match op {
        syn::UnOp::Not(_) => "!",
        syn::UnOp::Neg(_) => "-",
        _ => "?",
    }
}

fn binary_operator_text(op: &syn::BinOp) -> &'static str {
    use syn::BinOp;
    match op {
        BinOp::Add(_) => "+",
        BinOp::Sub(_) => "-",
        BinOp::Mul(_) => "*",
        BinOp::Div(_) => "/",
        BinOp::Rem(_) => "%",
        BinOp::And(_) => "&&",
        BinOp::Or(_) => "||",
        BinOp::BitXor(_) => "^",
        BinOp::BitAnd(_) => "&",
        BinOp::BitOr(_) => "|",
        BinOp::Shl(_) => "<<",
        BinOp::Shr(_) => ">>",
        BinOp::Eq(_) => "==",
        BinOp::Lt(_) => "<",
        BinOp::Le(_) => "<=",
        BinOp::Ne(_) => "!=",
        BinOp::Ge(_) => ">=",
        BinOp::Gt(_) => ">",
        _ => "?",
    }
}
