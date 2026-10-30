//! Places and types: assignment targets, field and bit ranges, layouts.

use super::*;
use crate::frontend::mir::{EnumTy, EnumVariantTy, StructTy};

pub(super) enum ReadPlace {
    /// Bits `[lo, hi)` of a signal, with the type of those bits: a struct field
    /// has its own type, not the type of the whole signal.
    Signal {
        signal: SignalId,
        range: Option<(u32, u32)>,
        ty: Ty,
    },
}

/// How a place is written.
pub(super) enum WritePlace {
    Signal {
        signal: SignalId,
        range: Option<(u32, u32)>,
        ty: Ty,
    },
    ChildInput {
        instance: usize,
        port: usize,
    },
    /// `arr[i] = ...` (or a field of an element, or a deeper index) with a
    /// run-time index: one element per arm, not a static range.
    DynamicWrite(DynamicWrite),
}

/// A write target with run-time element indices: `arr[i]`, `arr[i].field`,
/// `arr[i][j]`, ...
///
/// The run-time indices are not addresses. Each one is expanded into a `case`
/// over the elements it can select, and every arm writes a bit range the
/// analysis knows (see `Compiler::lower_dynamic_write`), so ownership, coverage
/// and the alias rewrite keep working on constant ranges and the IR needs no
/// dynamic-address form.
#[derive(Clone, Debug)]
pub(super) struct DynamicWrite {
    /// Signal holding the array.
    pub signal: SignalId,
    /// Place steps from the signal on, with the run-time indices still in place.
    pub components: Vec<Component>,
    /// The run-time indices, in the order they are expanded.
    pub layers: Vec<DynamicLayer>,
}

/// One run-time index inside a [`DynamicWrite`].
#[derive(Clone, Debug)]
pub(super) struct DynamicLayer {
    /// Position of the index step in [`DynamicWrite::components`].
    pub position: usize,
    /// Number of elements the array in front of it holds. An index narrower
    /// than that can only reach the first `2^width` of them; the expansion
    /// applies that bound.
    pub len: u32,
}

/// A resolved place, before deciding read versus write.
pub(super) enum Place {
    Signal {
        signal: SignalId,
        range: Option<(u32, u32)>,
        ty: Ty,
    },
    ChildInput {
        instance: usize,
        port: usize,
    },
    ChildOutput {
        instance: usize,
        port: usize,
        range: Option<(u32, u32)>,
        ty: Ty,
    },
    /// A run-time element target; only a write can use it.
    DynamicWrite(DynamicWrite),
}

/// One step of a place expression: a named field or a bit selection.
#[derive(Clone, Debug)]
pub(super) enum Component {
    /// `.field`
    Field(String),
    /// `[index]` or `[lo..hi]`, which select a bit range of the value
    /// (`[index]` selects an element when the value is an array).
    Bits(Box<syn::Expr>),
    /// `.slice(start, end)`: consecutive elements of an array value.
    Slice {
        /// First element.
        start: Box<syn::Expr>,
        /// One past the last element.
        end: Box<syn::Expr>,
    },
}

/// Split `self.a.b[0..4]` into (is_self, name, steps).
pub(super) fn decompose_place(
    expr: &syn::Expr,
) -> Result<(bool, Option<String>, Vec<Component>), String> {
    match expr {
        syn::Expr::Field(field) => {
            let (is_self, name, mut path) = decompose_place(&field.base)?;
            // A tuple field is `.0`, `.1`, ...; a tuple type names its fields
            // the same way, so one `Component::Field` covers both.
            let member = match &field.member {
                syn::Member::Named(member) => member.to_string(),
                syn::Member::Unnamed(index) => index.index.to_string(),
            };
            path.push(Component::Field(member));
            Ok((is_self, name, path))
        }
        syn::Expr::Index(index) => {
            let (is_self, name, mut path) = decompose_place(&index.expr)?;
            path.push(Component::Bits(Box::new((*index.index).clone())));
            Ok((is_self, name, path))
        }
        // `.slice(a, b)` is the one method that can be an assignment target: it
        // names consecutive elements of an array.
        syn::Expr::MethodCall(call) => {
            if call.method != "slice" || call.args.len() != 2 {
                return Err(format!(
                    "{} is not an assignment target",
                    parse::describe_expr(expr)
                ));
            }
            let (is_self, name, mut path) = decompose_place(&call.receiver)?;
            let mut args = call.args.iter();
            let start = Box::new(args.next().expect("checked: two arguments").clone());
            let end = Box::new(args.next().expect("checked: two arguments").clone());
            path.push(Component::Slice { start, end });
            Ok((is_self, name, path))
        }
        syn::Expr::Path(path) => {
            if path.qself.is_some() {
                return Err("qualified paths are not supported".to_string());
            }
            let Some(segment) = path.path.segments.last() else {
                return Err("empty path".to_string());
            };
            if path.path.segments.len() > 1 {
                return Err("paths with several segments are not supported".to_string());
            }
            let name = segment.ident.to_string();
            if name == "self" {
                Ok((true, None, Vec::new()))
            } else {
                Ok((false, Some(name), Vec::new()))
            }
        }
        syn::Expr::Paren(paren) => decompose_place(&paren.expr),
        syn::Expr::Group(group) => decompose_place(&group.expr),
        other => Err(format!(
            "{} is not an assignment target",
            parse::describe_expr(other)
        )),
    }
}

/// A half-open `[lo, hi)` range, checked against `limit` in `unit`s.
fn checked_range(
    lo: u32,
    hi: u32,
    limit: u32,
    unit: &str,
    span: &SourceSpan,
) -> Result<(u32, u32), Diagnostic> {
    if lo >= hi || hi > limit {
        return Err(Diagnostic::error(
            codes::OUT_OF_RANGE,
            Some(span.clone()),
            format!("{unit} range {lo}..{hi} is out of range for {limit} {unit}(s)"),
        ));
    }
    Ok((lo, hi))
}

/// The old `.slice(lo, hi)` spelling on a value that is not an array.
///
/// It used to be a bit range; a bit range is written `[lo..hi]` now, and
/// `.slice` names consecutive elements of an array.
pub(super) fn slice_removed(ty: &Ty, span: &SourceSpan) -> Diagnostic {
    Diagnostic::error(
        codes::UNSUPPORTED,
        Some(span.clone()),
        format!(
            "`.slice(lo, hi)` is not a bit range of {}: write the bit range `[lo..hi]`; \
             `.slice(lo, hi)` selects a sub-array of an array value",
            ty.describe()
        ),
    )
}

/// The integer literal `element`, written at `span`.
fn element_literal(element: u32, span: proc_macro2::Span) -> syn::Expr {
    syn::Expr::Lit(syn::ExprLit {
        attrs: Vec::new(),
        lit: syn::Lit::Int(syn::LitInt::new(&element.to_string(), span)),
    })
}

/// The span of an index step, for the literals a probe or an arm writes there.
fn index_span(path: &[Component], position: usize) -> proc_macro2::Span {
    match &path[position] {
        Component::Bits(index) => index.span(),
        Component::Field(_) | Component::Slice { .. } => {
            unreachable!("called for an index step")
        }
    }
}

/// The place steps with every run-time index replaced by the element the arm
/// picked, so the ordinary place resolver can name the constant bit range.
pub(super) fn concrete_path(
    components: &[Component],
    layers: &[DynamicLayer],
    chosen: &[u32],
) -> Vec<Component> {
    let mut path = components.to_vec();
    for (layer, element) in layers.iter().zip(chosen) {
        let span = index_span(components, layer.position);
        path[layer.position] = Component::Bits(Box::new(element_literal(*element, span)));
    }
    path
}

/// Name and optional annotation of a `let` pattern.
pub(super) fn pattern_name(pat: &syn::Pat) -> Result<(String, Option<&syn::Type>), String> {
    match pat {
        syn::Pat::Ident(ident) => Ok((ident.ident.to_string(), None)),
        syn::Pat::Type(typed) => match &*typed.pat {
            syn::Pat::Ident(ident) => Ok((ident.ident.to_string(), Some(&typed.ty))),
            _ => Err("only a plain name can be declared here".to_string()),
        },
        syn::Pat::Wild(_) => Err("a hardware signal needs a name".to_string()),
        syn::Pat::Tuple(tuple) if tuple.elems.is_empty() => {
            Err("the unit pattern `()` has no bits; bind a name instead".to_string())
        }
        other => Err(format!("unsupported pattern: {}", describe_pat(other))),
    }
}

/// Split a `let` pattern into its pattern and its type annotation.
///
/// `let (a, b): (uint<4>, uint<8>) = ...` writes the annotation around the
/// whole pattern, so a destructuring pattern has to peel it off first.
pub(super) fn split_let_pattern(pat: &syn::Pat) -> (&syn::Pat, Option<&syn::Type>) {
    match pat {
        syn::Pat::Type(typed) => (&typed.pat, Some(&typed.ty)),
        other => (other, None),
    }
}

/// The type as it is spelled in the source, for a tuple's descriptive name.
///
/// A struct or an enum contributes its own name (a nested tuple's name is
/// already `(...)`), so a tuple type reads `((uint<4>, bool), uint<8>)` rather
/// than repeating a width in the middle of a type name.
fn type_spelling(ty: &Ty) -> String {
    match ty {
        Ty::Struct(strukt) => strukt.name.clone(),
        Ty::Array { element, len } => format!("[{}; {len}]", type_spelling(element)),
        other => other.describe(),
    }
}

impl Compiler {
    pub(super) fn resolve_read_place(&mut self, expr: &syn::Expr) -> Result<ReadPlace, Diagnostic> {
        let span = self
            .span(expr.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        match self.resolve_place(expr)? {
            Place::Signal { signal, range, ty } => Ok(ReadPlace::Signal { signal, range, ty }),
            Place::ChildOutput {
                instance,
                port,
                range,
                ty,
            } => {
                let (signal, port_name) = {
                    let child_port = &self.cur.instances[instance].child_ports[port];
                    (
                        child_port
                            .parent_signal
                            .expect("output ports have a parent net"),
                        child_port.name.clone(),
                    )
                };
                self.cur.instances[instance].read_outputs.insert(port_name);
                Ok(ReadPlace::Signal { signal, range, ty })
            }
            Place::ChildInput { instance, port } => {
                let field = self.cur.instances[instance].field.clone();
                let port_name = self.cur.instances[instance].child_ports[port].name.clone();
                Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    format!("`{field}.{port_name}` is a child input; read the signal it is connected from instead"),
                ))
            }
            // A run-time element read is a value (`arr[i]`), not a place;
            // reaching here means the index is inside a longer place chain,
            // which the value path handles by naming the element first.
            Place::DynamicWrite(dynamic) => {
                let name = self.cur.signal(dynamic.signal).name.clone();
                Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    format!(
                        "`{name}[...]` with a run-time index cannot be read as a place; \
                         read the element as a value first"
                    ),
                ))
            }
        }
    }

    pub(super) fn resolve_write_place(
        &mut self,
        expr: &syn::Expr,
    ) -> Result<WritePlace, Diagnostic> {
        let span = self
            .span(expr.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        match self.resolve_place(expr)? {
            Place::Signal { signal, range, ty } => {
                self.check_writable(signal, &span)?;
                Ok(WritePlace::Signal { signal, range, ty })
            }
            Place::ChildInput { instance, port } => Ok(WritePlace::ChildInput { instance, port }),
            Place::ChildOutput { instance, port, .. } => {
                let field = self.cur.instances[instance].field.clone();
                let port_name = self.cur.instances[instance].child_ports[port].name.clone();
                Err(Diagnostic::error(
                    codes::INPUT_WRITTEN,
                    Some(span),
                    format!("output `{port_name}` of instance `{field}` is driven by the instance, not by the parent"),
                ))
            }
            Place::DynamicWrite(dynamic) => {
                self.check_writable(dynamic.signal, &span)?;
                Ok(WritePlace::DynamicWrite(dynamic))
            }
        }
    }

    /// An input port is driven by the parent, never by its own module.
    fn check_writable(&self, signal: SignalId, span: &SourceSpan) -> Result<(), Diagnostic> {
        let signal_info = self.cur.signal(signal);
        if let SignalKind::Port(dir) = signal_info.kind {
            if dir.is_input() {
                let name = signal_info.name.clone();
                return Err(Diagnostic::error(
                    codes::INPUT_WRITTEN,
                    Some(span.clone()),
                    format!("input port `{name}` cannot be driven by this module"),
                ));
            }
        }
        Ok(())
    }

    /// Shared place resolution for reads and writes.
    pub(super) fn resolve_place(&mut self, expr: &syn::Expr) -> Result<Place, Diagnostic> {
        let span = self
            .span(expr.span())
            .unwrap_or_else(|| self.cur.struct_source.clone());
        let (base_is_self, base_name, mut path) = match decompose_place(expr) {
            Ok(parts) => parts,
            Err(message) => return Err(Diagnostic::error(codes::UNSUPPORTED, Some(span), message)),
        };

        if base_is_self {
            let Some(first) = path.first().cloned() else {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    "`self` alone is not a value",
                ));
            };
            path.remove(0);
            let Component::Field(first) = first else {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span),
                    "`self` must be followed by a field name",
                ));
            };
            return self.resolve_field_place(&first, &mut path, &span, true);
        }

        let Some(name) = base_name else {
            return Err(Diagnostic::error(
                codes::UNSUPPORTED,
                Some(span),
                "an assignment target must be a field (`self.x`) or a local signal",
            ));
        };

        if let Some(context) = &self.proc {
            if let Some(signal) = context.locals.get(&name).copied() {
                let signal_ty = self.cur.signal(signal).ty.clone();
                if let Some(dynamic) = self.dynamic_write_place(signal, &path, &signal_ty, &span)? {
                    return Ok(dynamic);
                }
                let (range, ty) = self.place_range(&signal_ty, &path, &span)?;
                return Ok(Place::Signal { signal, range, ty });
            }
        }
        // A bare field name is the implicit `self.<name>`: a module field or a
        // child instance is reachable without `self.`, after locals and
        // parameters have had their chance.
        self.resolve_field_place(&name, &mut path, &span, false)
    }

    /// Resolve `self.<first>` (or the implicit bare form) plus the remaining
    /// place steps.
    ///
    /// `explicit_self` only decides how a failure is worded: the resolution
    /// itself is identical for `self.x` and the implicit `x`.
    fn resolve_field_place(
        &mut self,
        first: &str,
        path: &mut Vec<Component>,
        span: &SourceSpan,
        explicit_self: bool,
    ) -> Result<Place, Diagnostic> {
        let prefix = if explicit_self {
            format!("self.{first}")
        } else {
            first.to_string()
        };
        if let Some(signal) = self.cur.signal_by_name.get(first).copied() {
            let signal_ty = self.cur.signal(signal).ty.clone();
            if let Some(dynamic) = self.dynamic_write_place(signal, path, &signal_ty, span)? {
                return Ok(dynamic);
            }
            let (range, ty) = self.place_range(&signal_ty, path, span)?;
            return Ok(Place::Signal { signal, range, ty });
        }
        // A memory has no value of its own: only `read(addr)` and
        // `write(addr, data)` name its contents, and neither is a place.
        if self.cur.memory_by_field.contains_key(first) {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("`{prefix}` is a memory, not a value"),
            )
            .with_note(
                format!(
                    "read one element with `{prefix}.read(addr)`, write one with `{prefix}.write(addr, data)`"
                ),
                None,
            ));
        }
        if let Some(entry) = self.cur.instance_by_field.get(first).copied() {
            // An array of instances is indexed by a build-time constant:
            // there is no hardware that could select an instance at run time.
            let instance = match entry {
                FieldInstances::Single(index) => index,
                FieldInstances::Array { first: base, len } => {
                    let Some(Component::Bits(index)) = path.first().cloned() else {
                        return Err(Diagnostic::error(
                            codes::UNSUPPORTED,
                            Some(span.clone()),
                            format!(
                                "`{prefix}` is an array of {len} instances: use a build-time constant index, for example `{prefix}[0].<port>`"
                            ),
                        ));
                    };
                    path.remove(0);
                    let value = consteval::eval(&index, &self.cur.env).map_err(|message| {
                        Diagnostic::error(codes::NOT_CONSTANT, Some(span.clone()), message)
                    })?;
                    if value >= u64::from(len) {
                        return Err(Diagnostic::error(
                            codes::OUT_OF_RANGE,
                            Some(span.clone()),
                            format!(
                                "instance {value} is out of range for `{first}`, which has {len} instances (valid: 0..{})",
                                len.saturating_sub(1)
                            ),
                        ));
                    }
                    base + value as usize
                }
            };
            let Some(Component::Field(port_name)) = path.first().cloned() else {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span.clone()),
                    format!("`{prefix}` is a child instance: refer to one of its ports"),
                ));
            };
            path.remove(0);
            let Some(port) = self.cur.instances[instance]
                .child_ports
                .iter()
                .position(|candidate| candidate.name == port_name)
            else {
                let available: Vec<String> = self.cur.instances[instance]
                    .child_ports
                    .iter()
                    .map(|port| port.name.clone())
                    .collect();
                let field = self.cur.instances[instance].field.clone();
                return Err(Diagnostic::error(
                    codes::UNKNOWN_NAME,
                    Some(span.clone()),
                    format!("instance `{field}` has no port `{port_name}`"),
                )
                .with_note(
                    format!("ports of `{field}`: {}", available.join(", ")),
                    None,
                ));
            };
            let dir = self.cur.instances[instance].child_ports[port].dir;
            if dir.is_input() {
                if !path.is_empty() {
                    return Err(Diagnostic::error(
                        codes::UNSUPPORTED,
                        Some(span.clone()),
                        format!("`{port_name}` is a child input: connect the whole value"),
                    ));
                }
                return Ok(Place::ChildInput { instance, port });
            }
            let port_ty = self.cur.instances[instance].child_ports[port].ty.clone();
            // A child output is driven by the instance, so a run-time
            // element write to it is reported as what it is: the port is not
            // the parent's to write. The element itself is not resolved.
            if !self.run_time_indices(path).is_empty() {
                return Ok(Place::ChildOutput {
                    instance,
                    port,
                    range: None,
                    ty: port_ty,
                });
            }
            let (range, ty) = self.place_range(&port_ty, path, span)?;
            return Ok(Place::ChildOutput {
                instance,
                port,
                range,
                ty,
            });
        }
        let mut fields: Vec<String> = self.cur.signal_by_name.keys().cloned().collect();
        fields.extend(self.cur.instance_by_field.keys().cloned());
        fields.sort();
        let message = if explicit_self {
            format!("`self.{first}` is not a field of this module")
        } else {
            format!("unknown name `{first}`")
        };
        let mut diagnostic = Diagnostic::error(codes::UNKNOWN_NAME, Some(span.clone()), message);
        if !fields.is_empty() {
            diagnostic = diagnostic.with_note(format!("fields: {}", fields.join(", ")), None);
        }
        Err(diagnostic)
    }

    /// Bit range and type of a place, following its field, bit-range and
    /// sub-array steps.
    ///
    /// The type matters: `self.pkt.op` is 4 bits wide even though `pkt` is 44
    /// bits, and the expression it lowers to has to say so. `[lo..hi]` is always
    /// a bit range of the value, and a single `[k]` is an element when the value
    /// is an array and a bit otherwise; `.slice(lo, hi)` names consecutive
    /// elements of an array.
    pub(super) fn place_range(
        &self,
        ty: &Ty,
        components: &[Component],
        span: &SourceSpan,
    ) -> Result<(Option<(u32, u32)>, Ty), Diagnostic> {
        let mut lo = 0u32;
        let mut hi = ty.width();
        let mut current = ty.clone();
        for component in components {
            match component {
                Component::Field(name) => {
                    let Some(field) = current.field(name).cloned() else {
                        return Err(Diagnostic::error(
                            codes::UNKNOWN_NAME,
                            Some(span.clone()),
                            format!("{} has no field `{name}`", current.describe()),
                        ));
                    };
                    lo += field.offset;
                    hi = lo + field.width;
                    current = field.ty.clone();
                }
                Component::Bits(index) => {
                    let index_span = self.span(index.span()).unwrap_or_else(|| span.clone());
                    // A single index into an array names an element. A range is a
                    // bit range like any other, and falls through below.
                    if let Ty::Array { element, len } = &current {
                        if !matches!(index.as_ref(), syn::Expr::Range(_)) {
                            // Every run-time index is resolved before the place is
                            // used (`dynamic_write_place`), so only build-time
                            // indices are left here.
                            if self.is_run_time_index(index) {
                                return Err(Diagnostic::error(
                                    codes::INTERNAL,
                                    Some(index_span),
                                    "a run-time element index reached the place resolver",
                                ));
                            }
                            let (element_index, _) =
                                self.bits_range(index, *len, "element", &index_span)?;
                            let element = element.as_ref().clone();
                            let width = element.width();
                            lo += element_index * width;
                            hi = lo + width;
                            current = element;
                            continue;
                        }
                    }
                    let width = hi - lo;
                    // A range is a bit range of the packed value, a struct
                    // included; the bits of a struct are not addressed one at a
                    // time, its fields are selected by name.
                    if !matches!(index.as_ref(), syn::Expr::Range(_)) {
                        match current {
                            Ty::Struct(_) => {
                                return Err(Diagnostic::error(
                                    codes::UNSUPPORTED,
                                    Some(index_span),
                                    "a struct value cannot be indexed; select a field by name",
                                ))
                            }
                            Ty::Enum(_) => {
                                return Err(Diagnostic::error(
                                    codes::UNSUPPORTED,
                                    Some(index_span),
                                    "an enum value cannot be indexed; compare it, or match on its variants",
                                ))
                            }
                            _ => {}
                        }
                    }
                    let (bit_lo, bit_hi) = self.bits_range(index, width, "bit", &index_span)?;
                    lo += bit_lo;
                    hi = lo + (bit_hi - bit_lo);
                    current = if bit_hi - bit_lo == 1 {
                        Ty::Bool
                    } else {
                        Ty::UInt(bit_hi - bit_lo)
                    };
                }
                Component::Slice { start, end } => {
                    let slice_span = self.span(start.span()).unwrap_or_else(|| span.clone());
                    let Ty::Array { element, len } = &current else {
                        return Err(slice_removed(&current, &slice_span));
                    };
                    let (start, end) = self.two_bounds(start, end, *len, "element", &slice_span)?;
                    let element = element.as_ref().clone();
                    let width = element.width();
                    lo += start * width;
                    hi = lo + (end - start) * width;
                    // A sub-array stays an array, a one-element one included.
                    current = Ty::Array {
                        element: Box::new(element),
                        len: end - start,
                    };
                }
            }
        }
        if lo == 0 && hi == ty.width() {
            Ok((None, current))
        } else {
            Ok((Some((lo, hi)), current))
        }
    }

    /// The two build-time bounds `.slice(start, end)` writes, checked against
    /// `limit` in `unit`s.
    pub(super) fn two_bounds(
        &self,
        start: &syn::Expr,
        end: &syn::Expr,
        limit: u32,
        unit: &str,
        span: &SourceSpan,
    ) -> Result<(u32, u32), Diagnostic> {
        let start = self.const_bits(start, span)?;
        let end = self.const_bits(end, span)?;
        checked_range(start, end, limit, unit, span)
    }

    /// Is this index step decided by the hardware?
    ///
    /// A range is a build-time bit range, so it never is, whatever its bounds
    /// turn out to be; a single index is decided by the hardware when the
    /// frontend cannot evaluate it.
    pub(super) fn is_run_time_index(&self, index: &syn::Expr) -> bool {
        !matches!(index, syn::Expr::Range(_)) && consteval::eval(index, &self.cur.env).is_err()
    }

    /// The positions of the run-time element indices in a place path.
    fn run_time_indices(&self, path: &[Component]) -> Vec<usize> {
        path.iter()
            .enumerate()
            .filter_map(|(position, component)| match component {
                Component::Bits(index) if self.is_run_time_index(index) => Some(position),
                _ => None,
            })
            .collect()
    }

    /// The run-time element write a place names, when it has a run-time index.
    ///
    /// `None` when every index is a build-time constant - the ordinary place
    /// path handles that - and when a run-time index selects bits rather than
    /// elements: that is not a place at all, and the ordinary path reports the
    /// missing build-time index exactly as before.
    ///
    /// The type in front of each run-time index is probed with a zero index:
    /// every element of an array has the same type, so the probe is exact, even
    /// for a second or later run-time index.
    fn dynamic_write_place(
        &self,
        signal: SignalId,
        path: &[Component],
        base_ty: &Ty,
        span: &SourceSpan,
    ) -> Result<Option<Place>, Diagnostic> {
        let positions = self.run_time_indices(path);
        if positions.is_empty() {
            return Ok(None);
        }
        // Every element of an array has the same type, so probing every run-time
        // index as element 0 gives the type in front of each of them exactly.
        let mut probe = path.to_vec();
        for position in &positions {
            let probe_span = index_span(path, *position);
            probe[*position] = Component::Bits(Box::new(element_literal(0, probe_span)));
        }
        let mut layers = Vec::new();
        for position in positions {
            let (_, ty) = self.place_range(base_ty, &probe[..position], span)?;
            let Ty::Array { len, .. } = ty else {
                return Ok(None);
            };
            layers.push(DynamicLayer { position, len });
        }
        // The whole concrete path has to resolve, so a field or range behind a
        // run-time index is checked here, once, for every element.
        self.place_range(base_ty, &probe, span)?;
        Ok(Some(Place::DynamicWrite(DynamicWrite {
            signal,
            components: path.to_vec(),
            layers,
        })))
    }

    /// The type one concrete combination of a run-time write writes.
    ///
    /// Every element of an array has the same type, so probing with element 0
    /// gives the type every arm writes.
    pub(super) fn dynamic_write_target(
        &self,
        write: &DynamicWrite,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        let base_ty = self.cur.signal(write.signal).ty.clone();
        let probe = concrete_path(
            &write.components,
            &write.layers,
            &vec![0; write.layers.len()],
        );
        let (_, ty) = self.place_range(&base_ty, &probe, span)?;
        Ok(ty)
    }

    /// `[lo, hi)` selected by an index or a range, in `unit`s.
    ///
    /// `unit` is `"bit"` for a `uint`/`int` value and `"element"` for an array,
    /// so the diagnostic names what the bound actually counts.
    pub(super) fn bits_range(
        &self,
        index: &syn::Expr,
        width: u32,
        unit: &str,
        span: &SourceSpan,
    ) -> Result<(u32, u32), Diagnostic> {
        if let syn::Expr::Range(range) = index {
            let lo = match &range.start {
                Some(start) => self.const_bits(start, span)?,
                None => 0,
            };
            let hi = match &range.end {
                Some(end) => self.const_bits(end, span)?,
                None => {
                    return Err(Diagnostic::error(
                        codes::NOT_CONSTANT,
                        Some(span.clone()),
                        format!("a {unit} range needs an explicit end, for example `[0..4]`"),
                    ))
                }
            };
            let hi = match range.limits {
                syn::RangeLimits::Closed(_) => hi.checked_add(1).ok_or_else(|| {
                    Diagnostic::error(
                        codes::OUT_OF_RANGE,
                        Some(span.clone()),
                        "the end of this inclusive range overflows a 64-bit constant",
                    )
                })?,
                syn::RangeLimits::HalfOpen(_) => hi,
            };
            return checked_range(lo, hi, width, unit, span);
        }
        let bit = self.const_bits(index, span)?;
        if bit >= width {
            return Err(Diagnostic::error(
                codes::OUT_OF_RANGE,
                Some(span.clone()),
                format!("{unit} {bit} is out of range for {width} {unit}(s)"),
            ));
        }
        Ok((bit, bit + 1))
    }

    /// A build-time bit index.
    pub(super) fn const_bits(
        &self,
        expr: &syn::Expr,
        span: &SourceSpan,
    ) -> Result<u32, Diagnostic> {
        let value = consteval::eval(expr, &self.cur.env).map_err(|_| {
            Diagnostic::error(
                codes::NOT_CONSTANT,
                self.span(expr.span()),
                format!(
                    "a bit selection used as an assignment target needs a build-time index, but this is {}",
                    parse::describe_expr(expr)
                ),
            )
            .with_note("assign the whole value, or use a build-time index", None)
        })?;
        check_bound(value, span)
    }

    // -------------------------------------------------------- temporaries

    /// Materialise an expression into a named signal so it can be sliced.
    ///
    /// A bit-select on a complex expression is not legal Verilog-2005, so the
    /// value gets a name first. Inside a process that name is a process-local
    /// assigned with a blocking assignment, which keeps the sequential meaning
    /// of the surrounding statements; in the wiring it is a wire with its own
    pub(super) fn resolve_type(
        &mut self,
        ty: &syn::Type,
        span: SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        if let syn::Type::Array(array) = ty {
            return self.resolve_array_type(array, span);
        }
        if let syn::Type::Tuple(tuple) = ty {
            return self.resolve_tuple_type(tuple, span);
        }
        let syn::Type::Path(path) = ty else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                format!(
                    "unsupported type ({}); hardware types are `bool`, `uint<N>`, `int<N>` and `[T; N]`",
                    parse::describe_type(ty)
                ),
            ));
        };
        if path.qself.is_some() {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "qualified types are not supported",
            ));
        }
        let Some(segment) = path.path.segments.last() else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "empty type path",
            ));
        };
        let name = segment.ident.to_string();
        if path.path.segments.len() > 1 {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                format!("`{name}` is not a hardware type: use `bool`, `uint<N>`, `int<N>`, or a plain data struct"),
            ));
        }

        match name.as_str() {
            "bool" => match &segment.arguments {
                syn::PathArguments::None => Ok(Ty::Bool),
                _ => Err(Diagnostic::error(
                    codes::BAD_TYPE,
                    Some(span),
                    "`bool` takes no width",
                )),
            },
            // `Option<T>` and `Result<T, E>` are part of the language: the
            // source never declares them.
            "Option" => {
                let arguments = self.builtin_arguments(segment, "Option", 1, &span)?;
                let payload = self.resolve_type(arguments[0], span.clone())?;
                self.option_type(payload, &span)
            }
            "Result" => {
                let arguments = self.builtin_arguments(segment, "Result", 2, &span)?;
                let ok = self.resolve_type(arguments[0], span.clone())?;
                let err = self.resolve_type(arguments[1], span.clone())?;
                self.result_type(ok, err, &span)
            }
            // `U<N>` / `I<N>` are the spellings the Rust interop layer exposes:
            // the same fixed-width types, named without a lowercase identifier in
            // ordinary Rust code.
            "uint" | "int" | "U" | "I" => {
                let signed = name == "int" || name == "I";
                let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("`{name}` needs an explicit width: write `{name}<8>`"),
                    ));
                };
                let mut args = arguments.args.iter();
                let Some(value) = args.next().and_then(parse::const_argument) else {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("`{name}<W>` needs a build-time width"),
                    ));
                };
                if args.next().is_some() {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("`{name}` takes exactly one width"),
                    ));
                }
                let width = consteval::eval_width(&value, &self.cur.env).map_err(|message| {
                    Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message)
                })?;
                Ok(if signed {
                    Ty::SInt(width)
                } else {
                    Ty::UInt(width)
                })
            }
            "Mem" => Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "`Mem<T, N>` is a module resource, not a value type",
            )
            .with_note(
                "declare it as a field of a `#[hardware_instance]` struct; it cannot be a port, a struct field, an array element or a signal",
                None,
            )),
            other => {
                // `uint8` / `int8`: the width in the name, for the widths that
                // were spelled that way before `uint<N>` existed.
                let shorthand = other
                    .strip_prefix("uint")
                    .map(|digits| (false, digits))
                    .or_else(|| other.strip_prefix("int").map(|digits| (true, digits)));
                if let Some((signed, digits)) = shorthand {
                    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                        let width = digits.parse::<u64>().unwrap_or(0);
                        match consteval::check_width(width) {
                            Ok(width) => {
                                return Ok(if signed {
                                    Ty::SInt(width)
                                } else {
                                    Ty::UInt(width)
                                })
                            }
                            Err(message) => {
                                return Err(Diagnostic::error(codes::BAD_TYPE, Some(span), message))
                            }
                        }
                    }
                }
                if other == "usize"
                    || other == "isize"
                    || other == "u8"
                    || other == "u16"
                    || other == "u32"
                    || other == "u64"
                    || other == "i8"
                    || other == "i16"
                    || other == "i32"
                    || other == "i64"
                {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("`{other}` is not a hardware type: hardware widths are always explicit, write `uint<N>` or `int<N>`"),
                    ));
                }
                if !self.parsed.data.contains_key(other) && !self.parsed.enums.contains_key(other) {
                    return Err(Diagnostic::error(
                        codes::BAD_TYPE,
                        Some(span),
                        format!("unknown type `{other}`"),
                    )
                .with_note(
                        "hardware types are `bool`, `uint<N>`, `int<N>`, `[T; N]`, enums and plain data structs",
                        None,
                    ));
                }
                self.layout(other, &span)
            }
        }
    }

    /// `[T; N]`: a fixed-length array, element 0 in the least significant bits.
    ///
    /// The length is a build-time constant, and the total width is checked as a
    /// product, so a wide element type times a long length is refused instead of
    /// wrapping.
    fn resolve_array_type(
        &mut self,
        array: &syn::TypeArray,
        span: SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        let element = self.resolve_type(&array.elem, span.clone())?;
        let len = consteval::eval(&array.len, &self.cur.env)
            .map_err(|message| Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message))?;
        self.array_type(element, len, &span)
    }

    /// `(T, U, ...)`: a tuple type, laid out as a struct whose fields are named
    /// `0`, `1`, ... and packed most significant first.
    ///
    /// The unit type `()` is refused: a value with no bits has no hardware
    /// meaning here, and every signal must have a width.
    fn resolve_tuple_type(
        &mut self,
        tuple: &syn::TypeTuple,
        span: SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        if tuple.elems.is_empty() {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span),
                "the unit type `()` has no bits and is not a hardware value",
            ));
        }
        let mut elements = Vec::new();
        for element in &tuple.elems {
            elements.push(self.resolve_type(element, span.clone())?);
        }
        self.tuple_type(elements, &span)
    }

    /// The tuple type `elements` make.
    ///
    /// A tuple reuses the struct layout: the fields are named after their
    /// position and the type's name is descriptive, so widths, metadata, field
    /// access and the debugger already know what to do with it.
    pub(super) fn tuple_type(
        &self,
        elements: Vec<Ty>,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        let name = format!(
            "({})",
            elements
                .iter()
                .map(type_spelling)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let fields: Vec<(String, Ty)> = elements
            .into_iter()
            .enumerate()
            .map(|(index, ty)| (index.to_string(), ty))
            .collect();
        let strukt = StructTy::from_fields(name, fields)
            .map_err(|message| Diagnostic::error(codes::BAD_TYPE, Some(span.clone()), message))?;
        let total = u64::from(strukt.total);
        if total > consteval::MAX_WIDTH {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!(
                    "`{}` is {total} bits, more than the maximum supported width of {} bits",
                    Ty::Struct(strukt).describe(),
                    consteval::MAX_WIDTH
                ),
            ));
        }
        Ok(Ty::Struct(strukt))
    }

    /// The array type `len` elements of `element` make.
    ///
    /// The length and the total width are checked here, so every array type the
    /// frontend builds obeys the same limits - whether it was declared, written
    /// as a literal, or repeated.
    pub(super) fn array_type(
        &self,
        element: Ty,
        len: u64,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        if len == 0 {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                "an array must have at least one element",
            ));
        }
        if len > u64::from(u32::MAX) {
            return Err(Diagnostic::error(
                codes::OUT_OF_RANGE,
                Some(span.clone()),
                format!("an array of {len} elements is out of range"),
            ));
        }
        let len = len as u32;
        let total = element.width().saturating_mul(len);
        if u64::from(total) > consteval::MAX_WIDTH {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!(
                    "`[{}; {len}]` is {} bits, more than the maximum supported width of {} bits",
                    element.describe(),
                    u64::from(element.width()) * u64::from(len),
                    consteval::MAX_WIDTH
                ),
            ));
        }
        Ok(Ty::Array {
            element: Box::new(element),
            len,
        })
    }

    /// The type arguments of a built-in `Option<..>` / `Result<..>`.
    fn builtin_arguments<'a>(
        &self,
        segment: &'a syn::PathSegment,
        name: &str,
        arity: usize,
        span: &SourceSpan,
    ) -> Result<Vec<&'a syn::Type>, Diagnostic> {
        let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("`{name}` needs its type argument(s): write `{name}<..>`"),
            ));
        };
        let mut types = Vec::new();
        for argument in &arguments.args {
            let syn::GenericArgument::Type(ty) = argument else {
                return Err(Diagnostic::error(
                    codes::BAD_TYPE,
                    Some(span.clone()),
                    format!("`{name}` takes types, not constants or lifetimes"),
                ));
            };
            types.push(ty);
        }
        if types.len() != arity {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("`{name}` takes exactly {arity} type argument(s)"),
            ));
        }
        Ok(types)
    }

    /// `Option<T>`: `None` (tag 0) or `Some` carrying a `T` (tag 1).
    fn option_type(&self, payload: Ty, span: &SourceSpan) -> Result<Ty, Diagnostic> {
        let variants = vec![
            EnumVariantTy {
                name: "None".to_string(),
                tag: 0,
                payload: None,
            },
            EnumVariantTy {
                name: "Some".to_string(),
                tag: 1,
                payload: Some(payload),
            },
        ];
        self.enum_type("Option".to_string(), variants, span)
    }

    /// `Result<T, E>`: `Ok` carrying a `T` (tag 0) or `Err` carrying an `E`.
    fn result_type(&self, ok: Ty, err: Ty, span: &SourceSpan) -> Result<Ty, Diagnostic> {
        let variants = vec![
            EnumVariantTy {
                name: "Ok".to_string(),
                tag: 0,
                payload: Some(ok),
            },
            EnumVariantTy {
                name: "Err".to_string(),
                tag: 1,
                payload: Some(err),
            },
        ];
        self.enum_type("Result".to_string(), variants, span)
    }

    /// An enum type from its variants: the tag width the variant count needs,
    /// and a total width checked like an array's.
    fn enum_type(
        &self,
        name: String,
        variants: Vec<EnumVariantTy>,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        let tag_width = tag_width_for(variants.len());
        let ty = Ty::Enum(EnumTy {
            name,
            tag_width,
            variants,
        });
        let total = u64::from(ty.width());
        if total > consteval::MAX_WIDTH {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!(
                    "`{}` is {total} bits, more than the maximum supported width of {} bits",
                    ty.describe(),
                    consteval::MAX_WIDTH
                ),
            ));
        }
        Ok(ty)
    }

    /// The layout of a plain data type - a struct or an enum - built on demand.
    pub(super) fn layout(&mut self, name: &str, span: &SourceSpan) -> Result<Ty, Diagnostic> {
        if let Some(cached) = self.layouts.get(name) {
            return cached.clone();
        }
        if self.layout_stack.iter().any(|entry| entry == name) {
            let mut chain = self.layout_stack.clone();
            chain.push(name.to_string());
            return Err(Diagnostic::error(
                codes::LAYOUT,
                Some(span.clone()),
                format!("data type `{name}` is recursive: {}", chain.join(" -> ")),
            ));
        }
        let struct_item = self.parsed.data.get(name).cloned();
        let enum_item = self.parsed.enums.get(name).cloned();
        if struct_item.is_none() && enum_item.is_none() {
            return Err(Diagnostic::error(
                codes::BAD_TYPE,
                Some(span.clone()),
                format!("unknown data type `{name}`"),
            ));
        }
        let generics = [
            struct_item.as_ref().map(|item| &item.generics.params),
            enum_item.as_ref().map(|item| &item.generics.params),
        ];
        for params in generics.into_iter().flatten() {
            if !params.is_empty() {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    Some(span.clone()),
                    format!(
                        "data type `{name}` has generic parameters, which fixed-layout values cannot have"
                    ),
                ));
            }
        }

        self.layout_stack.push(name.to_string());
        let result = if let Some(item) = &struct_item {
            self.struct_layout(name, item, span)
        } else {
            self.enum_layout(name, enum_item.as_ref().expect("checked above"), span)
        };
        self.layout_stack.pop();

        self.layouts.insert(name.to_string(), result.clone());
        result
    }

    /// A declared `struct`: named fields, most significant first.
    fn struct_layout(
        &mut self,
        name: &str,
        item: &syn::ItemStruct,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        let syn::Fields::Named(named) = &item.fields else {
            return Err(Diagnostic::error(
                codes::LAYOUT,
                Some(span.clone()),
                format!("data struct `{name}` must declare named fields"),
            ));
        };
        if named.named.is_empty() {
            return Err(Diagnostic::error(
                codes::LAYOUT,
                Some(span.clone()),
                format!("data struct `{name}` has no fields and therefore no width"),
            ));
        }
        let mut resolved: Vec<(String, Ty)> = Vec::new();
        for field in &named.named {
            let Some(ident) = field.ident.as_ref() else {
                continue;
            };
            let field_span = self.span(field.span()).unwrap_or_else(|| span.clone());
            let ty = self.resolve_type(&field.ty, field_span)?;
            resolved.push((ident.to_string(), ty));
        }
        let layout = StructTy::from_fields(name.to_string(), resolved)
            .map_err(|message| Diagnostic::error(codes::LAYOUT, Some(span.clone()), message))?;
        Ok(Ty::Struct(layout))
    }

    /// A declared `enum`: tags in declaration order, payloads above them.
    fn enum_layout(
        &mut self,
        name: &str,
        item: &syn::ItemEnum,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        if item.variants.is_empty() {
            return Err(Diagnostic::error(
                codes::LAYOUT,
                Some(span.clone()),
                format!("enum `{name}` has no variants and therefore no width"),
            ));
        }
        let mut variants: Vec<EnumVariantTy> = Vec::new();
        for (tag, variant) in item.variants.iter().enumerate() {
            let variant_name = variant.ident.to_string();
            // An explicit discriminant is an encoding of its own: tags are the
            // declaration order here, and a custom encoding is not a feature
            // Say so instead of silently renumbering it.
            if let Some((_, value)) = &variant.discriminant {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    self.span(value.span()).or_else(|| Some(span.clone())),
                    format!(
                        "`{name}::{variant_name}` writes an explicit discriminant: enum tags are \
                         the declaration order, and a custom encoding is not supported"
                    ),
                ));
            }
            // An attribute would decide something this layout does not carry, so
            // it is reported rather than quietly ignored.
            for attribute in &variant.attrs {
                if attribute.path().is_ident("doc") {
                    continue;
                }
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    self.span(attribute.span()).or_else(|| Some(span.clone())),
                    format!(
                        "an attribute on `{name}::{variant_name}` is not supported: it would \
                         decide behaviour the enum layout does not carry"
                    ),
                ));
            }
            if variants
                .iter()
                .any(|existing| existing.name == variant_name)
            {
                return Err(Diagnostic::error(
                    codes::DECLARATION,
                    self.span(variant.ident.span())
                        .or_else(|| Some(span.clone())),
                    format!("enum `{name}` declares the variant `{variant_name}` more than once"),
                ));
            }
            let variant_span = self.span(variant.span()).unwrap_or_else(|| span.clone());
            let payload =
                self.variant_payload(name, &variant_name, &variant.fields, &variant_span)?;
            variants.push(EnumVariantTy {
                name: variant_name,
                tag: tag as u64,
                payload,
            });
        }
        self.enum_type(name.to_string(), variants, span)
    }

    /// The payload type of one enum variant.
    ///
    /// A single unnamed field *is* the payload; several unnamed fields and a
    /// named payload both become a struct, so a payload is always one value with
    /// one type - which is what the shared metadata model stores.
    fn variant_payload(
        &mut self,
        enum_name: &str,
        variant_name: &str,
        fields: &syn::Fields,
        span: &SourceSpan,
    ) -> Result<Option<Ty>, Diagnostic> {
        match fields {
            syn::Fields::Unit => Ok(None),
            syn::Fields::Unnamed(unnamed) => {
                if unnamed.unnamed.is_empty() {
                    return Ok(None);
                }
                if unnamed.unnamed.len() == 1 {
                    let field = unnamed.unnamed.first().expect("one field");
                    let field_span = self.span(field.span()).unwrap_or_else(|| span.clone());
                    return Ok(Some(self.resolve_type(&field.ty, field_span)?));
                }
                let mut resolved = Vec::new();
                for (index, field) in unnamed.unnamed.iter().enumerate() {
                    let field_span = self.span(field.span()).unwrap_or_else(|| span.clone());
                    resolved.push((index.to_string(), self.resolve_type(&field.ty, field_span)?));
                }
                Ok(Some(self.payload_struct(
                    enum_name,
                    variant_name,
                    resolved,
                    span,
                )?))
            }
            syn::Fields::Named(named) => {
                if named.named.is_empty() {
                    return Ok(None);
                }
                let mut resolved = Vec::new();
                for field in &named.named {
                    let Some(ident) = field.ident.as_ref() else {
                        continue;
                    };
                    let field_span = self.span(field.span()).unwrap_or_else(|| span.clone());
                    let ty = self.resolve_type(&field.ty, field_span)?;
                    resolved.push((ident.to_string(), ty));
                }
                Ok(Some(self.payload_struct(
                    enum_name,
                    variant_name,
                    resolved,
                    span,
                )?))
            }
        }
    }

    /// The struct several payload values are laid out as, named `Enum::Variant`.
    fn payload_struct(
        &self,
        enum_name: &str,
        variant_name: &str,
        fields: Vec<(String, Ty)>,
        span: &SourceSpan,
    ) -> Result<Ty, Diagnostic> {
        let name = format!("{enum_name}::{variant_name}");
        let layout = StructTy::from_fields(name, fields)
            .map_err(|message| Diagnostic::error(codes::LAYOUT, Some(span.clone()), message))?;
        Ok(Ty::Struct(layout))
    }
}

/// The tag width a number of variants needs: at least one bit, and enough bits
/// to name every variant.
fn tag_width_for(variants: usize) -> u32 {
    let mut width = 1u32;
    while width < 64 && (1u64 << width) < variants as u64 {
        width += 1;
    }
    width
}
