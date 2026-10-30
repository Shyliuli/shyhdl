//! Reading ShyHDL source as Rust syntax.
//!
//! The source is parsed with `syn`; hardware meaning comes from the annotations
//! (`#[hardware_instance]`, `#[In]`, `#[Out]`, `#[instance]`) and from
//! which macro a block is written with (`always!`). Nothing is inferred
//! from a plain `struct`.
//!
//! `#[hardware_instance]` marks a `struct` (one module) or a `fn` (one
//! combinational function). A function's parameters are its inputs and its
//! return type lists its outputs; its body is ordinary combinational DSL code
//! and may not contain `always!`.
//!
//! A parse failure is reported exactly where `syn` found it. Because the DSL is
//! Rust syntax, a few draft spellings cannot be parsed at all; those get a
//! targeted hint instead of a bare "expected `,`" (see [`parse_error`]).

use std::collections::HashMap;

use proc_macro2::{Delimiter, Group, TokenTree};
use syn::spanned::Spanned;

use crate::frontend::diagnostic::{codes, span_in, Diagnostic, Diagnostics, Note};
use crate::metadata::SourceSpan;

/// Everything the structural pass found in one source file.
#[derive(Debug)]
pub(crate) struct ParsedFile {
    /// Hardware structs in declaration order.
    pub hw: Vec<HwStruct>,
    /// Combinational hardware functions in declaration order.
    pub functions: Vec<HwFunction>,
    /// Plain data structs by name; their layout is built on demand.
    pub data: HashMap<String, syn::ItemStruct>,
    /// Enums by name; their layout is built on demand.
    pub enums: HashMap<String, syn::ItemEnum>,
    /// Hardware implementations by struct name.
    pub impls: HashMap<String, HwImpl>,
    /// `mod name;` declarations, in source order. Each names another source file
    /// that is merged into this design; a single-file compile cannot load it.
    pub mods: Vec<ModDecl>,
}

/// One `mod name;` declaration.
#[derive(Debug, Clone)]
pub(crate) struct ModDecl {
    /// Module name as written (`mod leaf;` gives `leaf`).
    pub name: String,
    /// Where the `mod` statement was written; a missing file or a cycle is
    /// reported here.
    pub span: SourceSpan,
}

/// A `#[hardware_instance]` struct: one module.
#[derive(Debug, Clone)]
pub(crate) struct HwStruct {
    /// Struct name.
    pub name: String,
    /// Const generic parameters, with defaults when written.
    pub params: Vec<HwParam>,
    /// Fields in declaration order.
    pub fields: Vec<HwField>,
    /// Where the struct name was written.
    pub span: SourceSpan,
}

/// A const generic parameter such as `const N: usize`.
#[derive(Debug, Clone)]
pub(crate) struct HwParam {
    /// Parameter name.
    pub name: String,
    /// Default value, already evaluated.
    pub default: Option<u64>,
}

/// What a hardware struct field is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    /// `#[In]`
    In,
    /// `#[Out]`
    Out,
    /// `#[instance]`
    Instance,
    /// A field of type `Mem<T, N>`: a memory resource, recognised by its type
    /// instead of a marker because the type is what the Rust side can name.
    Mem,
}

/// The element type and depth of a `Mem<T, N>` field type.
pub(crate) fn memory_type(ty: &syn::Type) -> Option<(syn::Type, syn::Expr)> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    if path.qself.is_some() || path.path.leading_colon.is_some() || path.path.segments.len() != 1 {
        return None;
    }
    let segment = path.path.segments.last()?;
    if segment.ident != "Mem" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let mut arguments = arguments.args.iter();
    let Some(syn::GenericArgument::Type(element)) = arguments.next() else {
        return None;
    };
    let depth = arguments.next().and_then(const_argument)?;
    if arguments.next().is_some() {
        return None;
    }
    Some((element.clone(), depth))
}

/// True when a type path is spelled `Mem...`, even if it is malformed: the
/// caller can then report what a memory declaration needs instead of treating
/// the field as a value.
pub(crate) fn spells_memory(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(path)
        if path.qself.is_none()
            && path.path.segments.last().is_some_and(|segment| segment.ident == "Mem"))
}

/// One field of a hardware struct.
#[derive(Debug, Clone)]
pub(crate) struct HwField {
    /// Field name.
    pub name: String,
    /// Field role.
    pub kind: FieldKind,
    /// Written type, resolved later against the module's parameters.
    pub ty: syn::Type,
    /// Where the field was declared.
    pub span: SourceSpan,
}

/// A `#[hardware_instance]` function: one combinational function.
///
/// The parameters are the inputs and the return type lists the outputs, so a
/// call reads as `let (w, o) = xxx(y, z);`. The body is an ordinary block; it
/// is rejected at parse time if it contains an `always!` process.
//
#[derive(Debug, Clone)]
pub(crate) struct HwFunction {
    /// Function name.
    pub name: String,
    /// Input parameters in declaration order.
    pub params: Vec<HwFunctionParam>,
    /// Declared return type. The tuple form `(uint<8>, bool)` lists the
    /// outputs; a single non-tuple type is one output.
    pub ret: syn::Type,
    /// Body of the function.
    pub body: syn::Block,
    /// Where the function name was written.
    pub span: SourceSpan,
}

/// One input parameter of a hardware function.
#[derive(Debug, Clone)]
pub(crate) struct HwFunctionParam {
    /// Parameter name.
    pub name: String,
    /// Declared type, resolved later like a field type.
    pub ty: syn::Type,
    /// Where the parameter was declared.
    pub span: SourceSpan,
}

/// The hardware implementation of one struct.
#[derive(Debug, Clone)]
pub(crate) struct HwImpl {
    /// Struct this impl belongs to.
    pub struct_name: String,
    /// Const generic parameters declared on the impl.
    pub params: Vec<HwParam>,
    /// Arguments of the self type, for example `N` in `Adder<N>`.
    pub args: Vec<syn::Expr>,
    /// Body of `fn run(&mut self)`.
    pub run: syn::Block,
    /// Where `run` was written.
    pub span: SourceSpan,
}

/// Parse one source file into the structural model.
pub(crate) fn parse(source: &str, source_name: &str) -> Result<ParsedFile, Diagnostics> {
    let file = match syn::parse_file(source) {
        Ok(file) => file,
        Err(error) => return Err(Diagnostics::from(parse_error(source, source_name, &error))),
    };

    let mut diagnostics = Diagnostics::new();
    let mut hardware_names: Vec<String> = Vec::new();
    let mut function_names: Vec<String> = Vec::new();
    let mut data: HashMap<String, syn::ItemStruct> = HashMap::new();
    let mut enums: HashMap<String, syn::ItemEnum> = HashMap::new();
    let mut duplicate: Option<(String, SourceSpan)> = None;
    let mut reserved: Option<(String, SourceSpan)> = None;

    let mut mods: Vec<ModDecl> = Vec::new();
    for item in &file.items {
        // `mod name;` pulls in another file; whether it can be resolved is the
        // caller's business (a single-file compile reports it).
        if let syn::Item::Mod(item_mod) = item {
            let span = span_in(source_name, item_mod.ident.span())
                .unwrap_or_else(|| fallback_span(source_name));
            if item_mod.content.is_some() {
                diagnostics.push(
                    Diagnostic::error(
                        codes::UNSUPPORTED,
                        span,
                        "an inline `mod name { .. }` is not supported: write `mod name;` and put the items in `name.shy.rs`",
                    ),
                );
            } else {
                mods.push(ModDecl {
                    name: item_mod.ident.to_string(),
                    span,
                });
            }
            continue;
        }
        // `use` is accepted and ignored: every module item is merged into one
        // namespace, so a plain `use path::Name;` names something already visible.
        if let syn::Item::Use(item_use) = item {
            check_use(item_use, source_name, &mut diagnostics);
            continue;
        }
        let (name, span, kind) = match item {
            syn::Item::Struct(item_struct) => (
                item_struct.ident.to_string(),
                span_in(source_name, item_struct.ident.span()),
                DeclKind::Struct(Box::new(item_struct.clone())),
            ),
            syn::Item::Enum(item_enum) => (
                item_enum.ident.to_string(),
                span_in(source_name, item_enum.ident.span()),
                DeclKind::Enum(Box::new(item_enum.clone())),
            ),
            // `#[hardware_instance] fn` is a combinational function. A plain
            // `fn` (for example a `#[hardware_test]`) is not a design item and
            // is ignored here, as before.
            syn::Item::Fn(item_fn) if has_hardware_instance(&item_fn.attrs) => (
                item_fn.sig.ident.to_string(),
                span_in(source_name, item_fn.sig.ident.span()),
                DeclKind::Function,
            ),
            _ => continue,
        };
        let fallback = || span.unwrap_or_else(|| fallback_span(source_name));
        // `Option` and `Result` are part of the language: a source may not
        // redeclare them under the same name.
        if is_builtin_type_name(&name) {
            if reserved.is_none() {
                reserved = Some((name, fallback()));
            }
            continue;
        }
        // Structs, enums and functions share one name namespace: a call and a
        // type name must be unambiguous.
        let taken = hardware_names.contains(&name)
            || function_names.contains(&name)
            || data.contains_key(&name)
            || enums.contains_key(&name);
        if taken {
            if duplicate.is_none() {
                duplicate = Some((name, fallback()));
            }
            continue;
        }
        match kind {
            DeclKind::Struct(item_struct) => {
                if has_hardware_instance(&item_struct.attrs) {
                    hardware_names.push(name);
                } else {
                    data.insert(name, *item_struct);
                }
            }
            DeclKind::Enum(item_enum) => {
                enums.insert(name, *item_enum);
            }
            DeclKind::Function => function_names.push(name),
        }
    }

    if let Some((name, span)) = duplicate {
        diagnostics.push(Diagnostic::error(
            codes::DECLARATION,
            span,
            format!("`{name}` is declared more than once in this file"),
        ));
    }
    if let Some((name, span)) = reserved {
        diagnostics.push(
            Diagnostic::error(
                codes::DECLARATION,
                span,
                format!("`{name}` is a built-in type name and cannot be declared here"),
            )
            .with_note(
                "`Option<T>` and `Result<T, E>` are part of the language",
                None,
            ),
        );
    }

    let mut hw = Vec::new();
    for name in &hardware_names {
        let item_struct = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Struct(s) if s.ident == name.as_str() => Some(s),
                _ => None,
            })
            .expect("hardware struct was collected from this file");
        match hardware_struct(source_name, item_struct, &mut diagnostics) {
            Some(parsed) => hw.push(parsed),
            None => continue,
        }
    }

    let mut functions = Vec::new();
    for name in &function_names {
        let item_fn = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function) if function.sig.ident == name.as_str() => Some(function),
                _ => None,
            })
            .expect("hardware function was collected from this file");
        match hardware_function(source_name, item_fn, &mut diagnostics) {
            Some(parsed) => functions.push(parsed),
            None => continue,
        }
    }

    let mut impls: HashMap<String, HwImpl> = HashMap::new();
    for item in &file.items {
        let syn::Item::Impl(item_impl) = item else {
            continue;
        };
        let Some(self_name) = self_type_name(&item_impl.self_ty) else {
            continue;
        };
        if !hardware_names.iter().any(|name| name == &self_name) {
            // An impl of some other type, or of a plain data struct: not a
            // hardware body, nothing to do.
            continue;
        }
        let span = span_in(source_name, item_impl.self_ty.span())
            .unwrap_or_else(|| fallback_span(source_name));
        if let Some(existing) = impls.get(&self_name) {
            diagnostics.push(Diagnostic::error(
                codes::IMPL,
                span,
                format!(
                    "`{self_name}` has more than one hardware impl (first one at {})",
                    existing.span
                ),
            ));
            continue;
        }
        match hardware_impl(source_name, item_impl, self_name, &mut diagnostics) {
            Some(parsed) => {
                impls.insert(parsed.struct_name.clone(), parsed);
            }
            None => continue,
        }
    }

    if diagnostics.has_errors() {
        return Err(diagnostics);
    }
    Ok(ParsedFile {
        hw,
        functions,
        data,
        enums,
        impls,
        mods,
    })
}

/// Check a `use` statement: plain paths are accepted (everything lives in one
/// namespace), but a glob or an `as` rename cannot be honoured and is reported
/// rather than silently mis-resolved.
fn check_use(item: &syn::ItemUse, source_name: &str, diagnostics: &mut Diagnostics) {
    let span = span_in(source_name, item.span()).unwrap_or_else(|| fallback_span(source_name));
    fn walk(tree: &syn::UseTree, span: &SourceSpan, diagnostics: &mut Diagnostics) {
        match tree {
            syn::UseTree::Path(path) => walk(&path.tree, span, diagnostics),
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    walk(item, span, diagnostics);
                }
            }
            syn::UseTree::Name(_) => {}
            syn::UseTree::Rename(rename) => diagnostics.push(
                Diagnostic::error(
                    codes::UNSUPPORTED,
                    span.clone(),
                    format!(
                        "`use .. as {}` is not supported: every module item is already in one namespace, so write the plain name",
                        rename.rename
                    ),
                ),
            ),
            syn::UseTree::Glob(_) => diagnostics.push(Diagnostic::error(
                codes::UNSUPPORTED,
                span.clone(),
                "a glob `use` is not supported: every module item is already in one namespace, so name it directly",
            )),
        }
    }
    walk(&item.tree, &span, diagnostics);
    let _ = item.span();
}

fn fallback_span(source_name: &str) -> SourceSpan {
    SourceSpan::new(source_name, 1, 1)
}

/// True when an item carries the `#[hardware_instance]` marker.
///
/// The marker is consulted for a `struct` (one module) and a `fn` (one
/// combinational function); any other item is left alone, exactly as before.
fn has_hardware_instance(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("hardware_instance"))
}

fn self_type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

fn hardware_struct(
    source_name: &str,
    item: &syn::ItemStruct,
    diagnostics: &mut Diagnostics,
) -> Option<HwStruct> {
    let name = item.ident.to_string();
    let span =
        span_in(source_name, item.ident.span()).unwrap_or_else(|| fallback_span(source_name));

    let params = match const_params(source_name, &item.generics, diagnostics, &name) {
        Ok(params) => params,
        Err(()) => return None,
    };

    let fields = match &item.fields {
        syn::Fields::Named(named) => named,
        _ => {
            diagnostics.push(Diagnostic::error(
                codes::DECLARATION,
                span.clone(),
                format!("hardware instance `{name}` must declare named fields"),
            ));
            return None;
        }
    };

    let mut parsed_fields = Vec::new();
    for field in &fields.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        let field_span = span_in(source_name, ident.span()).unwrap_or_else(|| span.clone());
        let field_name = ident.to_string();

        let mut kinds: Vec<(FieldKind, &'static str)> = Vec::new();
        let mut removed_marker = false;
        for attr in &field.attrs {
            let Some(ident) = attr.path().get_ident() else {
                diagnostics.push(Diagnostic::error(
                    codes::DECLARATION,
                    field_span.clone(),
                    format!("field `{field_name}` has an attribute that is not a path; expected #[In], #[Out] or #[instance]"),
                ));
                continue;
            };
            match ident.to_string().as_str() {
                "In" => kinds.push((FieldKind::In, "#[In]")),
                "Out" => kinds.push((FieldKind::Out, "#[Out]")),
                "instance" => kinds.push((FieldKind::Instance, "#[instance]")),
                // Removed: a clock is an ordinary input now. Say so instead of
                // reporting a nameless unknown attribute.
                "Clk" | "clk" | "Clock" | "clock" => {
                    removed_marker = true;
                    diagnostics.push(
                        Diagnostic::error(
                            codes::DECLARATION,
                            field_span.clone(),
                            format!("`#[Clk]` was removed, but `{field_name}` still uses it"),
                        )
            .with_note(
                            "a clock is an ordinary input: write `#[In]`; the debugger's reference clock is configured per testbench",
                            None,
                        ),
                    );
                }
                other => diagnostics.push(
                    Diagnostic::error(
                        codes::DECLARATION,
                        field_span.clone(),
                        format!("unknown field attribute `#[{other}]` on `{field_name}`"),
                    )
                    .with_note("supported markers are #[In], #[Out] and #[instance]", None),
                ),
            }
        }

        // A `Mem<T, N>` field is a memory resource, so its type is its marker.
        // It is not a port and not an instance, and no attribute may say it is.
        if memory_type(&field.ty).is_some() {
            if !kinds.is_empty() {
                let names: Vec<&str> = kinds.iter().map(|(_, text)| *text).collect();
                diagnostics.push(
                    Diagnostic::error(
                        codes::DECLARATION,
                        field_span.clone(),
                        format!(
                            "memory `{field_name}` is a resource of this module, not a port: \
                             it takes no {} marker",
                            names.join(" / ")
                        ),
                    )
                    .with_note(
                        "declare it as `mem: Mem<T, N>`, then `mem.read(addr)` / `mem.write(addr, data)`",
                        None,
                    ),
                );
                continue;
            }
            if removed_marker {
                continue;
            }
            parsed_fields.push(HwField {
                name: field_name,
                kind: FieldKind::Mem,
                ty: field.ty.clone(),
                span: field_span,
            });
            continue;
        }
        if spells_memory(&field.ty) {
            diagnostics.push(
                Diagnostic::error(
                    codes::BAD_TYPE,
                    field_span.clone(),
                    format!("memory `{field_name}` needs its element type and depth"),
                )
                .with_note(
                    format!("declare it as `{field_name}: Mem<uint<8>, 256>`"),
                    None,
                ),
            );
            continue;
        }

        let kind = match kinds.len() {
            0 if removed_marker => continue,
            0 => {
                diagnostics.push(
                    Diagnostic::error(
                        codes::DECLARATION,
                        field_span.clone(),
                        format!("field `{field_name}` of hardware instance `{name}` has no marker"),
                    )
                    .with_note(
                        "mark it #[In], #[Out] or #[instance], or declare it as a memory with `Mem<T, N>`",
                        None,
                    ),
                );
                continue;
            }
            1 => kinds[0].0,
            _ => {
                let names: Vec<&str> = kinds.iter().map(|(_, text)| *text).collect();
                diagnostics.push(Diagnostic::error(
                    codes::DECLARATION,
                    field_span.clone(),
                    format!(
                        "field `{field_name}` has conflicting markers {}",
                        names.join(" and ")
                    ),
                ));
                continue;
            }
        };

        parsed_fields.push(HwField {
            name: field_name,
            kind,
            ty: field.ty.clone(),
            span: field_span,
        });
    }

    let mut seen: HashMap<String, SourceSpan> = HashMap::new();
    for field in &parsed_fields {
        if let Some(previous) = seen.insert(field.name.clone(), field.span.clone()) {
            diagnostics.push(Diagnostic::error(
                codes::DECLARATION,
                field.span.clone(),
                format!(
                    "field `{}` is declared twice (first at {previous})",
                    field.name
                ),
            ));
        }
    }

    Some(HwStruct {
        name,
        params,
        fields: parsed_fields,
        span,
    })
}

/// Validate and collect one `#[hardware_instance] fn`.
///
/// A hardware function is combinational: its parameters are the inputs, its
/// return type lists the outputs, and its body is ordinary DSL combinational
/// logic. Everything that would change that meaning is rejected here rather
/// than ignored: a modifier, a generic parameter, a where-clause, a `self`
/// receiver, a parameter without an explicit type, no return type, and any
/// `always!` process inside the body.
fn hardware_function(
    source_name: &str,
    item: &syn::ItemFn,
    diagnostics: &mut Diagnostics,
) -> Option<HwFunction> {
    let sig = &item.sig;
    let name = sig.ident.to_string();
    let span = span_in(source_name, sig.ident.span()).unwrap_or_else(|| fallback_span(source_name));

    // A modifier changes what the function is, so it is rejected instead of
    // being dropped.
    let modifier = [
        (sig.constness.is_some(), "`const fn`"),
        (sig.asyncness.is_some(), "`async fn`"),
        (sig.unsafety.is_some(), "`unsafe fn`"),
        (sig.abi.is_some(), "an `extern` function"),
        (sig.variadic.is_some(), "a variadic function"),
    ]
    .into_iter()
    .find_map(|(present, what)| present.then_some(what));
    if let Some(what) = modifier {
        diagnostics.push(Diagnostic::error(
            codes::UNSUPPORTED,
            span,
            format!("hardware function `{name}` is {what}, which has no hardware meaning"),
        ));
        return None;
    }

    if let Some(parameter) = sig.generics.params.first() {
        diagnostics.push(Diagnostic::error(
            codes::UNSUPPORTED,
            span_in(source_name, parameter.span()),
            format!(
                "hardware function `{name}` declares generic parameters; a combinational function has fixed input and output types"
            ),
        ));
        return None;
    }
    if let Some(where_clause) = &sig.generics.where_clause {
        diagnostics.push(Diagnostic::error(
            codes::UNSUPPORTED,
            span_in(source_name, where_clause.span()),
            format!(
                "hardware function `{name}` has a where-clause; a combinational function has fixed input and output types"
            ),
        ));
        return None;
    }

    let mut params = Vec::new();
    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(receiver) => {
                diagnostics.push(Diagnostic::error(
                    codes::UNSUPPORTED,
                    span_in(source_name, receiver.span()),
                    format!(
                        "hardware function `{name}` takes `self`; a combinational function has only input parameters"
                    ),
                ));
                return None;
            }
            syn::FnArg::Typed(typed) => {
                let syn::Pat::Ident(pattern) = &*typed.pat else {
                    diagnostics.push(Diagnostic::error(
                        codes::DECLARATION,
                        span_in(source_name, typed.pat.span()),
                        format!(
                            "a parameter of hardware function `{name}` is not a plain name; write `name: uint<8>`"
                        ),
                    ));
                    return None;
                };
                if pattern.by_ref.is_some() || pattern.subpat.is_some() {
                    diagnostics.push(Diagnostic::error(
                        codes::DECLARATION,
                        span_in(source_name, pattern.span()),
                        format!(
                            "parameter `{}` of hardware function `{name}` must be a plain name",
                            pattern.ident
                        ),
                    ));
                    return None;
                }
                let param_name = pattern.ident.to_string();
                let param_span =
                    span_in(source_name, pattern.ident.span()).unwrap_or_else(|| span.clone());
                if matches!(&*typed.ty, syn::Type::Infer(_)) {
                    diagnostics.push(
                        Diagnostic::error(
                            codes::BAD_TYPE,
                            param_span,
                            format!(
                                "parameter `{param_name}` of hardware function `{name}` needs an explicit type"
                            ),
                        )
                        .with_note(format!("write `{param_name}: uint<8>`"), None),
                    );
                    return None;
                }
                params.push(HwFunctionParam {
                    name: param_name,
                    ty: (*typed.ty).clone(),
                    span: param_span,
                });
            }
        }
    }

    let ret = match &sig.output {
        syn::ReturnType::Default => {
            missing_return(&name, &span, diagnostics);
            return None;
        }
        syn::ReturnType::Type(_, ty) => {
            if matches!(&**ty, syn::Type::Tuple(tuple) if tuple.elems.is_empty()) {
                missing_return(&name, &span, diagnostics);
                return None;
            }
            (**ty).clone()
        }
    };

    if let Some(at) = always_in_block(&item.block) {
        diagnostics.push(
            Diagnostic::error(
                codes::UNSUPPORTED,
                span_in(source_name, at),
                format!(
                    "hardware function `{name}` is combinational and cannot contain `always!`"
                ),
            )
            .with_note(
                "move sequential logic into a module: `impl Hardware for <Struct> { fn run(&mut self) { always! { .. } } }`",
                None,
            ),
        );
        return None;
    }

    Some(HwFunction {
        name,
        params,
        ret,
        body: (*item.block).clone(),
        span,
    })
}

/// Report a missing output list, explaining the tuple form.
fn missing_return(name: &str, span: &SourceSpan, diagnostics: &mut Diagnostics) {
    diagnostics.push(
        Diagnostic::error(
            codes::BAD_TYPE,
            Some(span.clone()),
            format!(
                "hardware function `{name}` declares no return type; a hardware function's outputs are its return value"
            ),
        )
            .with_note(
            format!("write `-> (uint<8>, bool)` and call it as `let (a, b) = {name}(..)`"),
            None,
        ),
    );
}

/// Where the first `always!` in `block` is written, if there is one.
///
/// The whole body is walked, so an `always!` nested in an `if`, a `match` arm
/// or any other statement is found as well as one at the top level. The raw
/// span is returned so a missing source location never hides the rejection.
fn always_in_block(block: &syn::Block) -> Option<proc_macro2::Span> {
    struct Finder {
        found: Option<proc_macro2::Span>,
    }
    impl<'ast> syn::visit::Visit<'ast> for Finder {
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            let is_always = mac
                .path
                .segments
                .last()
                .map(|segment| segment.ident == "always")
                .unwrap_or(false);
            if self.found.is_none() && is_always {
                self.found = Some(mac.span());
            }
            syn::visit::visit_macro(self, mac);
        }
    }
    let mut finder = Finder { found: None };
    syn::visit::Visit::visit_block(&mut finder, block);
    finder.found
}

fn hardware_impl(
    source_name: &str,
    item: &syn::ItemImpl,
    struct_name: String,
    diagnostics: &mut Diagnostics,
) -> Option<HwImpl> {
    let span =
        span_in(source_name, item.self_ty.span()).unwrap_or_else(|| fallback_span(source_name));

    match &item.trait_ {
        None => {}
        Some((_, path, _)) => {
            let is_hardware = path
                .segments
                .last()
                .map(|segment| segment.ident == "Hardware")
                .unwrap_or(false);
            if !is_hardware {
                let trait_name = path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .unwrap_or_else(|| "<unnamed>".to_string());
                diagnostics.push(
                    Diagnostic::error(
                        codes::IMPL,
                        span.clone(),
                        format!("impl for hardware instance `{struct_name}` implements `{trait_name}`, not `Hardware`"),
                    )
                    .with_note("only `impl Hardware for <struct>` (or a bare `impl <struct>`) provides the hardware body", None),
                );
                return None;
            }
        }
    }

    let params = match const_params(source_name, &item.generics, diagnostics, &struct_name) {
        Ok(params) => params,
        Err(()) => return None,
    };

    let (_, args) = match type_path_args(&item.self_ty) {
        Ok(parts) => parts,
        Err(message) => {
            diagnostics.push(Diagnostic::error(
                codes::BAD_TYPE,
                span.clone(),
                format!("{struct_name}: {message}"),
            ));
            return None;
        }
    };

    // An impl parameter that the self type never mentions cannot be inferred and
    // would silently pick a width, so it is rejected (mirrors rustc's E0207).
    for param in &params {
        let used = args.iter().any(|arg| expr_mentions_ident(arg, &param.name));
        if !used {
            diagnostics.push(Diagnostic::error(
                codes::IMPL,
                span.clone(),
                format!(
                    "impl for `{struct_name}` declares `const {}` but the self type does not use it",
                    param.name
                ),
            ));
            return None;
        }
    }

    if args.len() != params.len() {
        diagnostics.push(Diagnostic::error(
            codes::IMPL,
            span.clone(),
            format!(
                "impl for `{struct_name}` declares {} const parameter(s) but the self type passes {}",
                params.len(),
                args.len()
            ),
        ));
        return None;
    }

    let mut run: Option<&syn::ImplItemFn> = None;
    let mut others: Vec<String> = Vec::new();
    for impl_item in &item.items {
        match impl_item {
            syn::ImplItem::Fn(function) => {
                if function.sig.ident == "run" {
                    if run.is_some() {
                        diagnostics.push(Diagnostic::error(
                            codes::IMPL,
                            span.clone(),
                            format!("`{struct_name}` declares `run` more than once"),
                        ));
                        return None;
                    }
                    run = Some(function);
                } else {
                    others.push(function.sig.ident.to_string());
                }
            }
            syn::ImplItem::Const(constant) => others.push(constant.ident.to_string()),
            syn::ImplItem::Type(ty) => others.push(ty.ident.to_string()),
            syn::ImplItem::Macro(mac) => others.push(
                mac.mac
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default(),
            ),
            _ => {}
        }
    }

    let Some(run) = run else {
        diagnostics.push(Diagnostic::error(
            codes::IMPL,
            span.clone(),
            format!("impl for hardware instance `{struct_name}` has no `fn run(&mut self)`"),
        ));
        return None;
    };

    if !others.is_empty() {
        diagnostics.push(
            Diagnostic::error(
                codes::IMPL,
                span_in(source_name, run.sig.ident.span()).unwrap_or_else(|| span.clone()),
                format!(
                    "impl for `{struct_name}` contains items other than `run`: {}",
                    others.join(", ")
                ),
            )
            .with_note("the whole impl body is the hardware description; move helpers out of the hardware impl", None),
        );
        return None;
    }

    let run_span = span_in(source_name, run.sig.ident.span()).unwrap_or_else(|| span.clone());
    if let Err(diagnostic) = check_signature(source_name, &run.sig, &struct_name) {
        diagnostics.push(diagnostic);
        return None;
    }

    Some(HwImpl {
        struct_name,
        params,
        args,
        run: run.block.clone(),
        span: run_span,
    })
}

/// `fn run(&mut self)`: a plain method with no return value and no modifiers.
///
/// Everything a hardware body cannot express is rejected here rather than
/// silently ignored: an `async fn`, an `unsafe fn`, a generic `run` or a return
/// value would all change what the block means.
fn check_signature(
    source_name: &str,
    sig: &syn::Signature,
    struct_name: &str,
) -> Result<(), Diagnostic> {
    let span = span_in(source_name, sig.ident.span());
    let unsupported = |what: &str| {
        Diagnostic::error(
            codes::UNSUPPORTED,
            span.clone(),
            format!("`{struct_name}::run` is `{what}`, which has no hardware meaning"),
        )
    };
    if sig.constness.is_some() {
        return Err(unsupported("const"));
    }
    if sig.asyncness.is_some() {
        return Err(unsupported("async"));
    }
    if sig.unsafety.is_some() {
        return Err(unsupported("unsafe"));
    }
    if sig.abi.is_some() {
        return Err(unsupported("extern"));
    }
    if sig.variadic.is_some() {
        return Err(unsupported("variadic"));
    }
    if !sig.generics.params.is_empty() {
        return Err(Diagnostic::error(
            codes::UNSUPPORTED,
            span.clone(),
            format!("`{struct_name}::run` is generic; hardware parameters belong on the struct"),
        ));
    }
    match &sig.output {
        syn::ReturnType::Default => {}
        syn::ReturnType::Type(_, ty) => {
            let unit = matches!(&**ty, syn::Type::Tuple(tuple) if tuple.elems.is_empty());
            if !unit {
                return Err(Diagnostic::error(
                    codes::UNSUPPORTED,
                    span_in(source_name, ty.span()),
                    format!(
                        "`{struct_name}::run` returns a value; a hardware body returns nothing"
                    ),
                ));
            }
        }
    }

    let mut receiver = None;
    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(value) => {
                if receiver.is_some() {
                    return Err(Diagnostic::error(
                        codes::IMPL,
                        span,
                        format!("`{struct_name}::run` takes more than one receiver"),
                    ));
                }
                receiver = Some(value);
            }
            syn::FnArg::Typed(value) => {
                return Err(Diagnostic::error(
                    codes::IMPL,
                    span_in(source_name, value.span()),
                    format!(
                        "`{struct_name}::run` takes a parameter; only `&mut self` is supported"
                    ),
                ));
            }
        }
    }
    match receiver {
        Some(value) if value.reference.is_some() && value.mutability.is_some() => Ok(()),
        Some(_) => Err(Diagnostic::error(
            codes::IMPL,
            span,
            format!("`{struct_name}::run` must take `&mut self`"),
        )),
        None => Err(Diagnostic::error(
            codes::IMPL,
            span,
            format!("`{struct_name}::run` must be a method taking `&mut self`"),
        )),
    }
}

fn const_params(
    source_name: &str,
    generics: &syn::Generics,
    diagnostics: &mut Diagnostics,
    owner: &str,
) -> Result<Vec<HwParam>, ()> {
    let mut params = Vec::new();
    for param in &generics.params {
        match param {
            syn::GenericParam::Const(constant) => {
                let default = match &constant.default {
                    Some(expr) => match crate::frontend::consteval::eval(expr, &Default::default())
                    {
                        Ok(value) => Some(value),
                        Err(message) => {
                            diagnostics.push(Diagnostic::error(
                                codes::BAD_TYPE,
                                span_in(source_name, constant.span()),
                                format!(
                                    "default of `const {}` of `{owner}`: {message}",
                                    constant.ident
                                ),
                            ));
                            return Err(());
                        }
                    },
                    None => None,
                };
                if !matches!(&constant.ty, syn::Type::Path(path) if path.path.is_ident("usize")) {
                    diagnostics.push(Diagnostic::error(
                        codes::BAD_TYPE,
                        span_in(source_name, constant.ty.span()),
                        format!(
                            "`const {}` of `{owner}` must have type `usize` to be a hardware parameter",
                            constant.ident
                        ),
                    ));
                    return Err(());
                }
                params.push(HwParam {
                    name: constant.ident.to_string(),
                    default,
                });
            }
            syn::GenericParam::Lifetime(lifetime) => {
                diagnostics.push(Diagnostic::error(
                    codes::UNSUPPORTED,
                    span_in(source_name, lifetime.span()),
                    format!("hardware `{owner}` declares lifetime `{}`; only const parameters are supported", lifetime.lifetime),
                ));
                return Err(());
            }
            syn::GenericParam::Type(ty) => {
                diagnostics.push(Diagnostic::error(
                    codes::UNSUPPORTED,
                    span_in(source_name, ty.span()),
                    format!("hardware `{owner}` declares type parameter `{}`; only const parameters are supported", ty.ident),
                ));
                return Err(());
            }
        }
    }
    Ok(params)
}

/// A const generic argument, in either spelling `syn` produces.
///
/// A literal or a braced expression arrives as `GenericArgument::Const`, but a
/// bare name arrives as a *type* argument (`Adder<N>`, `uint<N>`), because `syn`
/// cannot tell a const parameter from a type name without name resolution. Both
/// are accepted here and evaluated later against the module's parameters.
pub(crate) fn const_argument(argument: &syn::GenericArgument) -> Option<syn::Expr> {
    match argument {
        syn::GenericArgument::Const(expr) => Some(expr.clone()),
        syn::GenericArgument::Type(syn::Type::Path(path))
            if path.qself.is_none()
                && path.path.leading_colon.is_none()
                && path.path.segments.len() == 1
                && matches!(path.path.segments[0].arguments, syn::PathArguments::None) =>
        {
            Some(syn::Expr::Path(syn::ExprPath {
                attrs: Vec::new(),
                qself: None,
                path: path.path.clone(),
            }))
        }
        _ => None,
    }
}

/// Name and const arguments of a type path, for example `Adder<N>`.
///
/// Only a single-segment path without a qualified self is accepted, so
/// `other::Adder<8>` cannot be mistaken for `Adder<8>`.
pub(crate) fn type_path_args(ty: &syn::Type) -> Result<(String, Vec<syn::Expr>), String> {
    let syn::Type::Path(path) = ty else {
        return Err("expected a struct path, found a different type".to_string());
    };
    if path.qself.is_some() {
        return Err("qualified paths (`<T as Trait>::X`) are not supported".to_string());
    }
    if path.path.leading_colon.is_some() || path.path.segments.len() != 1 {
        return Err("expected a single name, not a path with `::`".to_string());
    }
    let segment = &path.path.segments[0];
    let name = segment.ident.to_string();
    match &segment.arguments {
        syn::PathArguments::None => Ok((name, Vec::new())),
        syn::PathArguments::AngleBracketed(arguments) => {
            let mut args = Vec::new();
            for argument in &arguments.args {
                if let Some(expr) = const_argument(argument) {
                    args.push(expr);
                    continue;
                }
                match argument {
                    syn::GenericArgument::Type(_) => {
                        return Err(format!(
                        "`{name}` is given a type argument; hardware parameters are const widths"
                    ))
                    }
                    syn::GenericArgument::Lifetime(lifetime) => {
                        return Err(format!("`{name}` is given lifetime `{lifetime}`"))
                    }
                    other => {
                        return Err(format!(
                            "`{name}` is given an unsupported argument ({})",
                            describe_generic_argument(other)
                        ))
                    }
                }
            }
            Ok((name, args))
        }
        syn::PathArguments::Parenthesized(_) => {
            Err(format!("`{name}` is not a type, it takes arguments"))
        }
    }
}

fn expr_mentions_ident(expr: &syn::Expr, name: &str) -> bool {
    struct Finder<'a> {
        name: &'a str,
        found: bool,
    }
    impl syn::visit::Visit<'_> for Finder<'_> {
        fn visit_ident(&mut self, ident: &syn::Ident) {
            if ident == self.name {
                self.found = true;
            }
        }
    }
    let mut finder = Finder { name, found: false };
    syn::visit::Visit::visit_expr(&mut finder, expr);
    finder.found
}

/// Human-readable kind of an expression, used in diagnostics.
///
/// The frontend never echoes raw source tokens back into a message: a reader
/// gets the construct that is wrong, not a token soup.
pub(crate) fn describe_expr(expr: &syn::Expr) -> &'static str {
    match expr {
        syn::Expr::Lit(_) => "a literal",
        syn::Expr::Path(_) => "a name",
        syn::Expr::Binary(_) => "a binary operation",
        syn::Expr::Unary(_) => "a unary operation",
        syn::Expr::Paren(_) | syn::Expr::Group(_) => "a parenthesised expression",
        syn::Expr::Block(_) => "a block expression",
        syn::Expr::Cast(_) => "a cast",
        syn::Expr::Field(_) => "a field access",
        syn::Expr::Index(_) => "an index expression",
        syn::Expr::MethodCall(_) => "a method call",
        syn::Expr::Call(_) => "a function call",
        syn::Expr::If(_) => "an `if` expression",
        syn::Expr::Match(_) => "a `match` expression",
        syn::Expr::Struct(_) => "a struct literal",
        syn::Expr::Tuple(_) => "a tuple",
        syn::Expr::Array(_) => "an array",
        syn::Expr::Repeat(_) => "an array repetition",
        syn::Expr::Reference(_) => "a reference",
        syn::Expr::Range(_) => "a range",
        syn::Expr::Assign(_) => "an assignment",
        syn::Expr::ForLoop(_) => "a `for` loop",
        syn::Expr::While(_) => "a `while` loop",
        syn::Expr::Loop(_) => "a `loop`",
        syn::Expr::Closure(_) => "a closure",
        syn::Expr::Macro(_) => "a macro invocation",
        syn::Expr::Return(_) => "a `return`",
        syn::Expr::Break(_) => "a `break`",
        syn::Expr::Continue(_) => "a `continue`",
        syn::Expr::Let(_) => "a `let` expression",
        syn::Expr::Try(_) => "a `?` expression",
        syn::Expr::Await(_) => "an `.await` expression",
        syn::Expr::Unsafe(_) => "an `unsafe` block",
        syn::Expr::Const(_) => "a const block",
        syn::Expr::TryBlock(_) => "a `try` block",
        syn::Expr::Yield(_) => "a `yield` expression",
        syn::Expr::Infer(_) => "an inferred type placeholder",
        syn::Expr::RawAddr(_) => "a raw address expression",
        syn::Expr::Verbatim(_) => "unparsed input",
        _ => "an unsupported expression",
    }
}

/// Human-readable kind of a type, used in diagnostics.
pub(crate) fn describe_type(ty: &syn::Type) -> &'static str {
    match ty {
        syn::Type::Path(_) => "a path type",
        syn::Type::Reference(_) => "a reference type",
        syn::Type::Tuple(_) => "a tuple type",
        syn::Type::Array(_) => "an array type",
        syn::Type::Slice(_) => "a slice type",
        syn::Type::Ptr(_) => "a pointer type",
        syn::Type::BareFn(_) => "a function pointer type",
        syn::Type::ImplTrait(_) => "an `impl Trait` type",
        syn::Type::TraitObject(_) => "a trait object type",
        syn::Type::Infer(_) => "an inferred type",
        syn::Type::Macro(_) => "a macro type",
        syn::Type::Never(_) => "the never type",
        syn::Type::Paren(_) => "a parenthesised type",
        syn::Type::Group(_) => "a grouped type",
        _ => "an unsupported type",
    }
}

fn describe_generic_argument(argument: &syn::GenericArgument) -> &'static str {
    match argument {
        syn::GenericArgument::Type(_) => "a type",
        syn::GenericArgument::Const(_) => "a const expression",
        syn::GenericArgument::AssocType(_) => "an associated type binding",
        syn::GenericArgument::AssocConst(_) => "an associated const binding",
        syn::GenericArgument::Constraint(_) => "a trait bound",
        syn::GenericArgument::Lifetime(_) => "a lifetime",
        _ => "an unsupported generic argument",
    }
}

/// Which event triggers one `always!` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArmEdge {
    /// `#[Pedge]` / `#[posedge]`.
    Posedge,
    /// `#[Nedge]` / `#[negedge]`.
    Negedge,
}

/// One arm of the event form: `#[Pedge] self.clk => { ... }`.
///
/// The attribute says only which event enters the arm; what the arm *is* comes
/// from where it stands: the arms before the last one are asynchronous resets,
/// in source order, and the last arm is the regular edge branch. A process with
/// a single arm therefore needs no marking at all.
#[derive(Debug, Clone)]
pub(crate) struct SeqArm {
    /// The event that enters this arm.
    pub edge: ArmEdge,
    /// Signal whose edge enters the arm.
    pub signal: syn::Expr,
    /// Body of the arm.
    pub body: syn::Block,
    /// Where the arm's signal was written.
    pub span: proc_macro2::Span,
}

/// True when the macro body uses the arm form (`#[Pedge] x => { ... }`).
///
/// Only the top level is inspected, so a `match` inside an arm body cannot be
/// mistaken for an arm boundary.
pub(crate) fn has_event_arms(tokens: &proc_macro2::TokenStream) -> bool {
    let mut trees = tokens.clone().into_iter().peekable();
    while let Some(tree) = trees.next() {
        if let TokenTree::Punct(punct) = &tree {
            if punct.as_char() == '=' && punct.spacing() == proc_macro2::Spacing::Joint {
                if let Some(TokenTree::Punct(next)) = trees.peek() {
                    if next.as_char() == '>' {
                        return true;
                    }
                }
            }
        }
    }
    false
}

struct SeqArmList(Vec<SeqArm>);

impl syn::parse::Parse for SeqArmList {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut arms = Vec::new();
        while !input.is_empty() {
            let attrs = input.call(syn::Attribute::parse_outer)?;
            let mut edge: Option<ArmEdge> = None;
            let mut attribute_span = None;
            for attr in &attrs {
                let Some(ident) = attr.path().get_ident() else {
                    continue;
                };
                match ident.to_string().as_str() {
                    "Pedge" | "posedge" => edge = Some(ArmEdge::Posedge),
                    "Nedge" | "negedge" => edge = Some(ArmEdge::Negedge),
                    // Removed: the position of the arm says what it is. Say so
                    // instead of reporting a nameless unknown attribute.
                    "Reset" | "reset" | "AsyncReset" | "async_reset" => {
                        return Err(syn::Error::new(
                            attr.span(),
                            "`#[Reset]` was removed: the arms before the last one are the \
                             asynchronous resets, in source order, and the last arm is the \
                             regular edge branch",
                        ))
                    }
                    "Clk" | "clk" | "Clock" | "clock" => {
                        return Err(syn::Error::new(
                            attr.span(),
                            "`#[Clk]` was removed: the attribute says which event enters the arm, \
                             and the last arm is the regular edge branch",
                        ))
                    }
                    "doc" => {}
                    other => {
                        return Err(syn::Error::new(
                            attr.span(),
                            format!(
                                "unknown arm attribute #[{other}]: expected #[Pedge] or #[Nedge]"
                            ),
                        ))
                    }
                }
                if attribute_span.is_none() {
                    attribute_span = Some(attr.span());
                }
            }
            let Some(edge) = edge else {
                let span = attribute_span.unwrap_or_else(|| input.span());
                return Err(syn::Error::new(
                    span,
                    "every `always!` arm needs an event attribute: #[Pedge] or #[Nedge]",
                ));
            };
            let signal: syn::Expr = input.parse()?;
            let span = signal.span();
            input.parse::<syn::Token![=>]>()?;
            let body: syn::Block = input.parse()?;
            if input.peek(syn::Token![,]) {
                input.parse::<syn::Token![,]>()?;
            } else if input.peek(syn::Token![;]) {
                input.parse::<syn::Token![;]>()?;
            }
            arms.push(SeqArm {
                edge,
                signal,
                body,
                span,
            });
        }
        Ok(SeqArmList(arms))
    }
}

/// Parse the arms of an `always!` written in the event form.
pub(crate) fn parse_seq_arms(mac: &syn::Macro) -> Result<Vec<SeqArm>, syn::Error> {
    syn::parse2::<SeqArmList>(mac.tokens.clone()).map(|list| list.0)
}

/// The statements of a macro invocation such as `always! { ... }`.
///
/// The body is parsed as a Rust block, so a syntax error inside it points at the
/// offending token instead of failing later with a confusing message. This is
/// why the DSL's process bodies have to be ordinary Rust statements.
pub(crate) fn macro_body_block(
    source_name: &str,
    mac: &syn::Macro,
) -> Result<syn::Block, Diagnostic> {
    let group = Group::new(Delimiter::Brace, mac.tokens.clone());
    let stream = proc_macro2::TokenStream::from(TokenTree::Group(group));
    syn::parse2::<syn::Block>(stream).map_err(|error| {
        Diagnostic::error(
            codes::PARSE,
            span_in(source_name, error.span()),
            format!("could not read the body of `{}!`: {error}", macro_name(mac)),
        )
    })
}

/// Name of the macro being invoked, for diagnostics.
pub(crate) fn macro_name(mac: &syn::Macro) -> String {
    mac.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .unwrap_or_default()
}

/// Turn a `syn` error into a diagnostic, adding hints for the DSL spellings that
/// cannot be parsed as Rust at all.
pub(crate) fn parse_error(source: &str, source_name: &str, error: &syn::Error) -> Diagnostic {
    let span = span_in(source_name, error.span());
    let mut diagnostic = Diagnostic::error(codes::PARSE, span.clone(), error.to_string());
    for note in parse_hints(source, source_name, span.as_ref()) {
        diagnostic.notes.push(note);
    }
    diagnostic
}

/// Targeted hints for parse failures that come from a non-Rust spelling.
fn parse_hints(source: &str, source_name: &str, span: Option<&SourceSpan>) -> Vec<Note> {
    let mut notes = Vec::new();
    let Some(span) = span else { return notes };
    let lines: Vec<&str> = source.lines().collect();
    if span.line == 0 || span.line > lines.len() {
        return notes;
    }
    // Types may wrap over a couple of lines, so look a little upwards.
    let start = span.line.saturating_sub(3);
    let window = &lines[start..span.line];

    if window.iter().any(|line| line.contains("let self.")) {
        notes.push(Note {
            message: "`let self.field = ...` is not valid: drop `let` to drive the field (`self.field = ...`), \
                      or introduce a plain local (`let name: uint<8> = ...`)"
                .to_string(),
            span: None,
        });
    }

    if window
        .iter()
        .any(|line| has_unbraced_const_expression(line))
    {
        notes.push(Note {
            message: "const width expressions must be braced: write `uint<{ N + 1 }>` (or `zext::<{ N + 1 }>()`); \
                      the frontend evaluates them while compiling"
                .to_string(),
            span: Some(SourceSpan::new(source_name, span.line, 1)),
        });
    }
    notes
}

/// A named declaration, kept apart so structs, enums and functions share one
/// duplicate-name loop.
enum DeclKind {
    Struct(Box<syn::ItemStruct>),
    Enum(Box<syn::ItemEnum>),
    Function,
}

/// The enum names the language defines itself.
pub(crate) fn is_builtin_type_name(name: &str) -> bool {
    matches!(name, "Option" | "Result")
}

/// True when a line looks like `<... arithmetic...>`: a generic argument that
/// Rust only accepts when it is wrapped in braces.
fn has_unbraced_const_expression(line: &str) -> bool {
    let Some(open) = line.find('<') else {
        return false;
    };
    let rest = &line[open + 1..];
    let Some(close) = rest.find('>') else {
        return false;
    };
    let inner = &rest[..close];
    if inner.contains('{') || inner.is_empty() {
        return false;
    }
    inner.contains('+') || inner.contains('*') || inner.contains('/') || inner.contains('%')
}
