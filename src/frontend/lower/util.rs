//! Helpers shared by the lowering modules: constants, name checks, width
//! checks, graph plumbing and build-time parameter binding.

use super::*;

use crate::packed;

pub(super) fn describe_literal(lit: &syn::Lit) -> &'static str {
    match lit {
        syn::Lit::Str(_) => "a string literal",
        syn::Lit::ByteStr(_) => "a byte string literal",
        syn::Lit::CStr(_) => "a C string literal",
        syn::Lit::Byte(_) => "a byte literal",
        syn::Lit::Char(_) => "a character literal",
        syn::Lit::Float(_) => "a floating point literal",
        syn::Lit::Bool(_) => "a boolean literal",
        syn::Lit::Int(_) => "an integer literal",
        _ => "an unsupported literal",
    }
}

pub(super) fn describe_pat(pat: &syn::Pat) -> &'static str {
    match pat {
        syn::Pat::Tuple(_) => "a tuple pattern",
        syn::Pat::Struct(_) => "a struct pattern",
        syn::Pat::Slice(_) => "a slice pattern",
        syn::Pat::Or(_) => "an or-pattern",
        syn::Pat::Reference(_) => "a reference pattern",
        syn::Pat::Lit(_) => "a literal pattern",
        _ => "an unsupported pattern",
    }
}

/// `posedge(x)` / `negedge(x)`.
pub(super) fn edge_condition(expr: &syn::Expr) -> Option<(Edge, &syn::Expr)> {
    let syn::Expr::Call(call) = expr else {
        return None;
    };
    let syn::Expr::Path(path) = &*call.func else {
        return None;
    };
    let name = path.path.segments.last()?.ident.to_string();
    let edge = match name.as_str() {
        "posedge" => Edge::Posedge,
        "negedge" => Edge::Negedge,
        _ => return None,
    };
    if call.args.len() != 1 {
        return None;
    }
    Some((edge, &call.args[0]))
}

/// Bit ranges read by a resolved expression.
pub(super) fn collect_read_refs(expr: &Expr, module: &ModuleBuild) -> Vec<ReadRef> {
    let mut reads = Vec::new();
    walk_reads(expr, module, &mut reads);
    reads
}

pub(super) fn walk_reads(expr: &Expr, module: &ModuleBuild, reads: &mut Vec<ReadRef>) {
    match &expr.kind {
        ExprKind::Const { .. } => {}
        ExprKind::Pattern { .. } => {}
        ExprKind::Signal(signal) => reads.push(ReadRef::whole(*signal, module)),
        ExprKind::Slice { signal, lo, hi } => reads.push(ReadRef {
            signal: *signal,
            lo: *lo,
            hi: *hi,
        }),
        ExprKind::BitSelect { signal, index } => {
            // A run-time index can select any bit, so the whole signal counts.
            reads.push(ReadRef::whole(*signal, module));
            walk_reads(index, module, reads);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_reads(lhs, module, reads);
            walk_reads(rhs, module, reads);
        }
        ExprKind::Unary { operand, .. } => walk_reads(operand, module, reads),
        ExprKind::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            walk_reads(cond, module, reads);
            walk_reads(then_expr, module, reads);
            walk_reads(else_expr, module, reads);
        }
        ExprKind::Concat(elements) => {
            for element in elements {
                walk_reads(element, module, reads);
            }
        }
        ExprKind::Replicate { value, .. } => walk_reads(value, module, reads),
        // The memory itself is not a signal: a read depends on its address, and
        // the write path is sequential, so its data is no combinational source.
        ExprKind::MemRead { addr, .. } => walk_reads(addr, module, reads),
    }
}

/// The continuous assignments of a module, as range drivers.
pub(super) fn collect_wiring_drivers(build: &ModuleBuild) -> Vec<WiringDriver> {
    let mut drivers = Vec::new();
    for item in &build.body {
        if let BodyItem::Assign { target, value, .. } = item {
            let (lo, hi) = target
                .range()
                .unwrap_or_else(|| (0, build.signal(target.signal()).width()));
            drivers.push(WiringDriver {
                signal: target.signal(),
                lo,
                hi,
                reads: collect_read_refs(value, build),
            });
        }
    }
    drivers
}

/// Follow a wiring read to the values it comes from.
///
/// A range already on the stack means the chain returned to where it started:
/// that is a combinational loop, and it is reported as an edge so the graph
/// finds it.
pub(super) fn resolve_wiring_read(
    drivers: &[WiringDriver],
    read: ReadRef,
    target: SignalId,
    target_range: (u32, u32),
    stack: &mut Vec<ReadRef>,
    graph: &mut DependencyGraph,
) {
    let covering: Vec<&WiringDriver> = drivers
        .iter()
        .filter(|driver| driver.signal == read.signal && driver.lo < read.hi && read.lo < driver.hi)
        .collect();

    if covering.is_empty() {
        graph.add_edge(read.signal, target);
        graph.add_bit_edge(
            (read.signal, read.lo, read.hi),
            (target, target_range.0, target_range.1),
        );
        return;
    }

    let mut covered = 0u32;
    for driver in covering {
        let lo = driver.lo.max(read.lo);
        let hi = driver.hi.min(read.hi);
        covered += hi - lo;
        let driver_range = ReadRef {
            signal: driver.signal,
            lo: driver.lo,
            hi: driver.hi,
        };
        if stack.contains(&driver_range) {
            graph.add_edge(read.signal, target);
            graph.add_bit_edge(
                (read.signal, read.lo, read.hi),
                (target, target_range.0, target_range.1),
            );
            continue;
        }
        stack.push(driver_range);
        for nested in &driver.reads {
            resolve_wiring_read(drivers, *nested, target, target_range, stack, graph);
        }
        stack.pop();
    }

    if covered < read.width() {
        // Not every bit of the read is driven by the wiring, so it also reads
        // the signal's value from outside.
        graph.add_edge(read.signal, target);
        graph.add_bit_edge(
            (read.signal, read.lo, read.hi),
            (target, target_range.0, target_range.1),
        );
    }
}

/// `y[7:4], y[2]`: the bits of a signal that are missing a driver.
pub(super) fn format_bit_runs(name: &str, bits: &[u32]) -> String {
    let mut parts = Vec::new();
    let mut start = bits[0];
    let mut previous = bits[0];
    for bit in bits.iter().copied().skip(1) {
        if bit == previous + 1 {
            previous = bit;
            continue;
        }
        parts.push(run(name, start, previous));
        start = bit;
        previous = bit;
    }
    parts.push(run(name, start, previous));
    parts.join(", ")
}

pub(super) fn run(name: &str, start: u32, end: u32) -> String {
    if start == end {
        format!("{name}[{start}]")
    } else {
        format!("{name}[{end}:{start}]")
    }
}

/// Width of an assignment target must equal the width of the value, and its
/// signedness must match.
pub(super) fn check_width(
    target: &Ty,
    range: Option<(u32, u32)>,
    value: &Expr,
    span: SourceSpan,
) -> Result<(), Diagnostic> {
    let target_width = range
        .map(|(lo, hi)| hi - lo)
        .unwrap_or_else(|| target.width());
    if target_width != value.ty.width() {
        return Err(Diagnostic::error(
            codes::WIDTH_MISMATCH,
            Some(span),
            format!(
                "width mismatch: the target is {target_width} bit(s) but the value is {} bit(s) ({})",
                value.ty.width(),
                value.ty.describe()
            ),
        )
        .with_note(
            "widen explicitly with `zext::<W>()`/`sext::<W>()`, or narrow with `trunc::<W>()` or a bit range `[lo..hi]`",
            None,
        ));
    }
    // The same bits are a different value read as signed or as unsigned, so a
    // signedness change is written `as int<W>` / `as uint<W>` rather than
    // happening on assignment.
    if target.is_signed() != value.ty.is_signed() {
        return Err(signedness_mismatch(
            format!(
                "cannot assign {} to {}: the signedness differs",
                value.ty.describe(),
                target.describe()
            ),
            &span,
        ));
    }
    // An array or an enum is a shape, not only a width: `[uint<4>; 2]` and
    // `uint<8>` hold the same bits and mean different things, and an enum is not
    // its packed bits either. A copy that involves one of them - directly or
    // inside an aggregate - has to match shape by shape.
    if (mentions_shape(target) || mentions_shape(&value.ty)) && !same_shape(target, &value.ty) {
        return Err(Diagnostic::error(
            codes::WIDTH_MISMATCH,
            Some(span),
            format!(
                "cannot assign {} to {}: array values copy only between the same shape",
                value.ty.describe(),
                target.describe()
            ),
        ));
    }
    Ok(())
}

/// Does this type hold an array or an enum anywhere? Those are the shapes a copy
/// has to match exactly; a plain struct keeps the lenient width comparison it
/// always had.
pub(super) fn mentions_shape(ty: &Ty) -> bool {
    match ty {
        Ty::Array { .. } | Ty::Enum(_) => true,
        Ty::Struct(strukt) => strukt.fields.iter().any(|field| mentions_shape(&field.ty)),
        _ => false,
    }
}

/// The same shape: the same arrays of the same elements, the same enums of the
/// same variants and payloads, the same structs of the same fields.
pub(super) fn same_shape(lhs: &Ty, rhs: &Ty) -> bool {
    match (lhs, rhs) {
        (
            Ty::Array {
                element: lhs_element,
                len: lhs_len,
            },
            Ty::Array {
                element: rhs_element,
                len: rhs_len,
            },
        ) => lhs_len == rhs_len && same_shape(lhs_element, rhs_element),
        (Ty::Enum(lhs), Ty::Enum(rhs)) => {
            lhs.name == rhs.name
                && lhs.tag_width == rhs.tag_width
                && lhs.variants.len() == rhs.variants.len()
                && lhs.variants.iter().zip(&rhs.variants).all(|(lhs, rhs)| {
                    lhs.name == rhs.name
                        && lhs.tag == rhs.tag
                        && match (&lhs.payload, &rhs.payload) {
                            (None, None) => true,
                            (Some(lhs), Some(rhs)) => same_shape(lhs, rhs),
                            _ => false,
                        }
                })
        }
        (Ty::Struct(lhs), Ty::Struct(rhs)) => {
            lhs.name == rhs.name
                && lhs.fields.len() == rhs.fields.len()
                && lhs
                    .fields
                    .iter()
                    .zip(&rhs.fields)
                    .all(|(lhs, rhs)| lhs.name == rhs.name && same_shape(&lhs.ty, &rhs.ty))
        }
        // Everything else is compared as a whole: an element can be a `uint`, an
        // `int`, a `bool` or a struct, and each of those has a meaning of its own.
        _ => lhs == rhs,
    }
}

/// The error for a signed value and an unsigned one meeting.
///
/// Verilog makes the whole expression unsigned as soon as one operand is, and a
/// two's-complement value read as a magnitude is a different value: the source
/// has to say which reading it means.
fn signedness_mismatch(message: String, span: &SourceSpan) -> Diagnostic {
    Diagnostic::error(codes::WIDTH_MISMATCH, Some(span.clone()), message).with_note(
        "reinterpret the bits explicitly with `as int<W>` or `as uint<W>` (the bit pattern is unchanged)",
        None,
    )
}

/// The width and the way the bits are read, for the shared operand rules.
fn width_of(ty: &Ty) -> packed::Width {
    packed::Width {
        bits: ty.width(),
        signed: ty.is_signed(),
    }
}

/// Both operands of an operation must have the same width and read their bits
/// the same way.
pub(super) fn require_aligned(
    lhs: &Ty,
    rhs: &Ty,
    op: crate::frontend::mir::BinOp,
    span: &SourceSpan,
) -> Result<(), Diagnostic> {
    match packed::align(width_of(lhs), width_of(rhs)) {
        Ok(_) => Ok(()),
        Err(packed::AlignError::Widths { .. }) => Err(Diagnostic::error(
            codes::WIDTH_MISMATCH,
            Some(span.clone()),
            format!(
                "`{}` mixes widths: {} and {}",
                op.source(),
                lhs.describe(),
                rhs.describe()
            ),
        )
        .with_note(
            "hardware arithmetic is equal width here; widen one side explicitly",
            None,
        )),
        Err(packed::AlignError::Signs { .. }) => Err(signedness_mismatch(
            format!(
                "`{}` mixes a signed and an unsigned value: {} and {}",
                op.source(),
                lhs.describe(),
                rhs.describe()
            ),
            span,
        )),
    }
}

/// A shift count is an unsigned value or a build-time constant.
///
/// A signed count is rejected: a negative one has no shift meaning, and the
/// value of a shift is the same whichever way the count is declared, so nothing
/// is lost by requiring the unsigned spelling.
pub(super) fn check_shift_count(value: &Expr, span: &SourceSpan) -> Result<(), Diagnostic> {
    let constant = matches!(value.kind, ExprKind::Const { .. });
    if matches!(value.ty, Ty::UInt(_)) || (constant && !value.ty.is_signed()) {
        return Ok(());
    }
    Err(Diagnostic::error(
        codes::BAD_TYPE,
        Some(span.clone()),
        format!(
            "a shift count must be an unsigned value or a build-time constant, found {}",
            value.ty.describe()
        ),
    ))
}

/// Reject unsized literals that survived lowering: their width is unknowable.
pub(super) fn check_unsized(expr: &Expr) -> Result<(), Diagnostic> {
    match &expr.kind {
        ExprKind::Const { width: 0, .. } => Err(Diagnostic::error(
            codes::WIDTH_UNKNOWN,
            Some(expr.source.clone()),
            "cannot infer the width of this integer literal",
        )
        .with_note(
            "annotate the value, for example `let x: uint<8> = 0;`",
            None,
        )),
        // A shift count may stay unsized: it is a repetition count, not a value.
        ExprKind::Binary {
            op: crate::frontend::mir::BinOp::Shl | crate::frontend::mir::BinOp::Shr,
            lhs,
            rhs,
        } => {
            check_unsized(lhs)?;
            let _ = rhs;
            Ok(())
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            check_unsized(lhs)?;
            check_unsized(rhs)
        }
        ExprKind::Unary { operand, .. } => check_unsized(operand),
        ExprKind::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            check_unsized(cond)?;
            check_unsized(then_expr)?;
            check_unsized(else_expr)
        }
        ExprKind::Concat(elements) => {
            for element in elements {
                check_unsized(element)?;
            }
            Ok(())
        }
        ExprKind::Replicate { value, .. } => check_unsized(value),
        ExprKind::BitSelect { index, .. } => check_unsized(index),
        ExprKind::MemRead { addr, .. } => check_unsized(addr),
        ExprKind::Const { .. } | ExprKind::Signal(_) | ExprKind::Slice { .. } => Ok(()),
        ExprKind::Pattern { .. } => Ok(()),
    }
}

pub(super) fn check_bound(value: u64, span: &SourceSpan) -> Result<u32, Diagnostic> {
    if value > u64::from(u32::MAX) {
        return Err(Diagnostic::error(
            codes::OUT_OF_RANGE,
            Some(span.clone()),
            format!("bit index {value} is out of range"),
        ));
    }
    Ok(value as u32)
}

pub(super) fn params_of(hw: &HwStruct, env: &ConstEnv) -> Vec<Param> {
    hw.params
        .iter()
        .filter_map(|param| {
            env.get(&param.name).map(|value| Param {
                name: param.name.clone(),
                value: *value,
            })
        })
        .collect()
}

/// Verilog module name of a specialised hardware struct.
pub(super) fn mangle(name: &str, params: &[parse::HwParam], env: &ConstEnv) -> String {
    if params.is_empty() {
        return name.to_string();
    }
    let mut result = name.to_string();
    for param in params {
        match env.get(&param.name) {
            Some(value) => {
                result.push('_');
                result.push_str(&param.name);
                result.push_str(&value.to_string());
            }
            None => result.push_str("_unbound"),
        }
    }
    result
}

/// Bind a hardware struct's parameters to the arguments written at a use site.
pub(super) fn bind_params(
    hw: &HwStruct,
    args: &[syn::Expr],
    outer: &ConstEnv,
    source_name: &str,
    span: SourceSpan,
) -> Result<ConstEnv, Diagnostic> {
    let mut env: ConstEnv = ConstEnv::new();
    if args.len() > hw.params.len() {
        return Err(Diagnostic::error(
            codes::BAD_TYPE,
            Some(span),
            format!(
                "`{}` takes {} parameter(s), {} given",
                hw.name,
                hw.params.len(),
                args.len()
            ),
        ));
    }
    for (index, arg) in args.iter().enumerate() {
        let param = &hw.params[index];
        let value = consteval::eval(arg, outer).map_err(|message| {
            Diagnostic::error(
                codes::NOT_CONSTANT,
                span_in(source_name, arg.span()),
                message,
            )
        })?;
        env.insert(param.name.clone(), value);
    }
    for param in hw.params.iter().skip(args.len()) {
        let Some(default) = param.default else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!(
                    "`{}` parameter `{}` is not given and has no default; write `{}<{}>`",
                    hw.name, param.name, hw.name, param.name
                ),
            ));
        };
        env.insert(param.name.clone(), default);
    }
    Ok(env)
}

/// Check that the impl's parameters line up with the struct's.
pub(super) fn impl_matches_params(
    hw: &HwStruct,
    implementation: &HwImpl,
    env: &ConstEnv,
    source_name: &str,
) -> Option<Diagnostic> {
    if implementation.params.len() != hw.params.len() {
        return Some(Diagnostic::error(
            codes::IMPL,
            Some(implementation.span.clone()),
            format!(
                "`{}` is declared with {} const parameter(s) but its impl declares {}",
                hw.name,
                hw.params.len(),
                implementation.params.len()
            ),
        ));
    }
    for (index, param) in implementation.params.iter().enumerate() {
        if param.name != hw.params[index].name {
            return Some(Diagnostic::error(
                codes::IMPL,
                Some(implementation.span.clone()),
                format!(
                    "impl parameter `{}` does not match struct parameter `{}` of `{}`",
                    param.name, hw.params[index].name, hw.name
                ),
            ));
        }
        let expected = env.get(&param.name).copied().unwrap_or(0);
        let actual = match consteval::eval(&implementation.args[index], env) {
            Ok(value) => value,
            Err(message) => {
                return Some(Diagnostic::error(
                    codes::IMPL,
                    span_in(source_name, implementation.args[index].span()),
                    message,
                ))
            }
        };
        if expected != actual {
            return Some(Diagnostic::error(
                codes::IMPL,
                Some(implementation.span.clone()),
                format!(
                    "impl `{}<{}>` does not match the specialisation `{}={}`",
                    hw.name, param.name, param.name, expected
                ),
            ));
        }
    }
    None
}

/// `Adder` or `Adder<8>` (also `Adder<8, 2>`).
pub(super) fn parse_top_spec(spec: &str) -> Result<(String, Vec<syn::Expr>), String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("the top module name is empty".to_string());
    }
    let ty: syn::Type = syn::parse_str(trimmed)
        .map_err(|_| format!("`{spec}` is not a module name; write `Adder8` or `Adder<8>`"))?;
    let (name, args) = parse::type_path_args(&ty)
        .map_err(|message| format!("`{spec}` is not a module name: {message}"))?;
    for arg in &args {
        if consteval::eval(arg, &ConstEnv::new()).is_err() {
            return Err(format!(
                "`{spec}`: top module parameters must be literal values, for example `{name}<8>`"
            ));
        }
    }
    Ok((name, args))
}

/// Intersect two definite-assignment maps: a bit survives only when it was
/// assigned on both paths.
pub(super) fn merge_definite(
    lhs: &HashMap<SignalId, Bits>,
    rhs: &HashMap<SignalId, Bits>,
    width_of: impl Fn(SignalId) -> u32,
) -> HashMap<SignalId, Bits> {
    let mut signals: Vec<SignalId> = lhs.keys().chain(rhs.keys()).copied().collect();
    signals.sort_unstable();
    signals.dedup();
    let mut merged = HashMap::new();
    for signal in signals {
        let width = width_of(signal);
        let mut bits = lhs
            .get(&signal)
            .cloned()
            .unwrap_or_else(|| Bits::new(width));
        let other = rhs
            .get(&signal)
            .cloned()
            .unwrap_or_else(|| Bits::new(width));
        bits.intersect_with(&other);
        merged.insert(signal, bits);
    }
    merged
}

/// Intersect the definite-assignment maps of every branch.
pub(super) fn merge_all_definite(
    states: &[HashMap<SignalId, Bits>],
    width_of: impl Fn(SignalId) -> u32,
) -> HashMap<SignalId, Bits> {
    let mut iter = states.iter();
    let Some(first) = iter.next() else {
        return HashMap::new();
    };
    let mut merged = first.clone();
    for state in iter {
        merged = merge_definite(&merged, state, &width_of);
    }
    merged
}

/// The runs of `read` that `definite` does not cover, in ascending order.
///
/// A read of one of those bits uses the process's own previous output, because
/// the process drives the signal and has not assigned this bit yet on this path
/// A bit that *is* definite is a blocking read of the value the
/// process itself computed, which is not feedback; keeping that distinction per
/// bit range is what stops a run-time bit select - which reads the whole signal
/// - from looking like a loop over the bits it already assigned.
pub(super) fn unassigned_runs(
    definite: Option<&Bits>,
    read: ReadRef,
    width: u32,
) -> Vec<(u32, u32)> {
    let none = Bits::new(width);
    let definite = definite.unwrap_or(&none);
    let mut runs = Vec::new();
    let mut bit = read.lo;
    while bit < read.hi {
        if definite.get(bit) {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit < read.hi && !definite.get(bit) {
            bit += 1;
        }
        runs.push((start, bit));
    }
    runs
}

/// Verilog-2005 keywords plus the SystemVerilog ones a Verilator run may
/// reject. A source name that is one of these cannot be emitted as an
/// identifier, so it is reported instead of producing Verilog that will not
/// compile.
pub(super) const VERILOG_KEYWORDS: &[&str] = &[
    // Verilog-2005 (IEEE 1364-2005)
    "always",
    "and",
    "assign",
    "automatic",
    "begin",
    "buf",
    "bufif0",
    "bufif1",
    "case",
    "casex",
    "casez",
    "cell",
    "cmos",
    "config",
    "deassign",
    "default",
    "defparam",
    "design",
    "disable",
    "edge",
    "else",
    "end",
    "endcase",
    "endconfig",
    "endfunction",
    "endgenerate",
    "endmodule",
    "endprimitive",
    "endspecify",
    "endtable",
    "endtask",
    "event",
    "for",
    "force",
    "forever",
    "fork",
    "function",
    "generate",
    "genvar",
    "highz0",
    "highz1",
    "if",
    "ifnone",
    "incdir",
    "include",
    "initial",
    "inout",
    "input",
    "instance",
    "integer",
    "join",
    "large",
    "liblist",
    "library",
    "localparam",
    "macromodule",
    "medium",
    "module",
    "nand",
    "negedge",
    "nmos",
    "nor",
    "noshowcancelled",
    "not",
    "notif0",
    "notif1",
    "or",
    "output",
    "parameter",
    "pmos",
    "posedge",
    "primitive",
    "pull0",
    "pull1",
    "pulldown",
    "pullup",
    "pulsestyle_ondetect",
    "pulsestyle_onevent",
    "rcmos",
    "real",
    "realtime",
    "reg",
    "release",
    "repeat",
    "rnmos",
    "rpmos",
    "rtran",
    "rtranif0",
    "rtranif1",
    "scalared",
    "showcancelled",
    "signed",
    "small",
    "specify",
    "specparam",
    "strong0",
    "strong1",
    "supply0",
    "supply1",
    "table",
    "task",
    "time",
    "tran",
    "tranif0",
    "tranif1",
    "tri",
    "tri0",
    "tri1",
    "triand",
    "trior",
    "trireg",
    "unsigned",
    "use",
    "vectored",
    "wait",
    "wand",
    "weak0",
    "weak1",
    "while",
    "wire",
    "wor",
    "xnor",
    "xor",
    // SystemVerilog keywords a Verilator run is likely to see
    "always_comb",
    "always_ff",
    "always_latch",
    "assert",
    "assume",
    "bit",
    "break",
    "byte",
    "chandle",
    "checker",
    "class",
    "clocking",
    "const",
    "constraint",
    "continue",
    "cover",
    "covergroup",
    "do",
    "endclass",
    "endclocking",
    "endinterface",
    "endpackage",
    "endprogram",
    "endproperty",
    "endsequence",
    "enum",
    "expect",
    "extends",
    "extern",
    "final",
    "foreach",
    "import",
    "int",
    "interface",
    "join_any",
    "join_none",
    "logic",
    "longint",
    "modport",
    "new",
    "null",
    "package",
    "packed",
    "priority",
    "program",
    "property",
    "protected",
    "pure",
    "rand",
    "randc",
    "ref",
    "return",
    "sequence",
    "shortint",
    "shortreal",
    "static",
    "string",
    "struct",
    "super",
    "this",
    "throw",
    "typedef",
    "union",
    "unique",
    "var",
    "virtual",
    "void",
];

/// Check that a source name can be a Verilog identifier.
///
/// The policy is to *reject* rather than rename: names in the generated RTL are
/// the source names, so a symbol layer never has to guess a mapping. A name that
/// cannot be emitted is reported at its declaration.
pub(super) fn check_verilog_name(
    name: &str,
    what: &str,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    if let Some(raw) = name.strip_prefix("r#") {
        return Err(Diagnostic::error(
            codes::RESERVED_NAME,
            span,
            format!("{what} `{name}` is a raw identifier; rename it to `{raw}`"),
        ));
    }
    if !name.is_ascii() {
        return Err(Diagnostic::error(
            codes::RESERVED_NAME,
            span,
            format!("{what} `{name}` is not ASCII; Verilog-2005 identifiers are ASCII only"),
        ));
    }
    if !name
        .chars()
        .next()
        .map(|first| first.is_ascii_alphabetic() || first == '_')
        .unwrap_or(false)
    {
        return Err(Diagnostic::error(
            codes::RESERVED_NAME,
            span,
            format!("{what} `{name}` must start with a letter or `_` to be a Verilog identifier"),
        ));
    }
    if VERILOG_KEYWORDS.contains(&name) {
        return Err(Diagnostic::error(
            codes::RESERVED_NAME,
            span,
            format!("{what} `{name}` is a Verilog keyword and cannot be used as a signal name"),
        )
        .with_note(
            "rename it (the frontend never renames a source name silently)",
            None,
        ));
    }
    Ok(())
}
