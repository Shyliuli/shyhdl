//! Attribute macros that let ShyHDL be written inside ordinary Rust source.
//!
//! Each macro does two things and only two things:
//!
//! * it emits a **placeholder** item that keeps the Rust side compiling, and
//! * it **registers the original tokens as text**, so the library can compile the
//!   real design at run time.
//!
//! No hardware is generated here: the macros never look at the DSL semantics and
//! never panic. A token stream that does not parse as the expected item is
//! reported with `compile_error!` and passed through unchanged, so the user still
//! sees their own syntax error rather than a macro-internal one.
//!
//! # The registration contract
//!
//! Each macro submits one record, collected at link time, so nothing has to run at
//! start-up:
//!
//! ```text
//! ::shyhdl::__inventory::submit! {
//!     ::shyhdl::RegisteredPart {
//!         kind: ::shyhdl::PartKind::Instance,
//!         name: "Adder8",
//!         source: "#[hardware_instance]\nstruct Adder8 {.. }",
//!         file: file!(),
//!         line: line!(),
//!     }
//! }
//! ```
//!
//! The `shyhdl` crate therefore provides three things:
//!
//! * `pub enum PartKind { Instance, Data, Enum, Impl, Test }`,
//! * `pub struct RegisteredPart { kind, name, source, file, line }` with
//!   `inventory::collect!(RegisteredPart);`,
//! * `pub use inventory as __inventory;`.
//!
//! This crate has no `inventory` dependency: it only names the re-export.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use syn::visit_mut::VisitMut;
use syn::{parse_macro_input, Item};

/// The editor view of a hardware body.
///
/// The source keeps the DSL spelling; only the analyzer sees this rewritten
/// form. Two spellings cannot type-check as plain Rust and are rewritten here:
///
/// * `x as U<N>` becomes `U::<N>::from(x)`, because Rust has no cast to a
///   non-primitive type;
/// * an integer literal standing alone where a hardware value is expected
///   becomes `::shyhdl::__bits(..)`, whose width is inferred from the context.
///
/// Everything else is left untouched: the operators and width-changing methods
/// exist as real implementations behind `cfg(rust_analyzer)`.
struct EditorRewrite;

impl VisitMut for EditorRewrite {
    fn visit_expr_mut(&mut self, expr: &mut syn::Expr) {
        syn::visit_mut::visit_expr_mut(self, expr);
        match expr {
            syn::Expr::Cast(cast) => {
                let Some((constructor, width)) = hardware_type(&cast.ty) else {
                    return;
                };
                let value = &cast.expr;
                *expr = syn::parse_quote!(#constructor::<#width>::from(#value));
            }
            // `x[i]` is a bit or an element, `x[lo..hi]` a bit range. The DSL
            // has no Rust indexing operator, so the editor view calls the
            // trait method that names the same operation.
            syn::Expr::Index(index) => {
                let base = &index.expr;
                let step = &index.index;
                *expr = if matches!(&**step, syn::Expr::Range(_)) {
                    syn::parse_quote!(#base.__bits(#step))
                } else {
                    syn::parse_quote!(#base.__bit(#step))
                };
            }
            // A write through an index is a method call in the editor view:
            // the DSL's `x[i] = v` cannot use Rust's `IndexMut`, which cannot
            // produce a sized value from a range.
            syn::Expr::Assign(assign) => {
                let syn::Expr::Index(index) = &*assign.left else {
                    return;
                };
                let base = &index.expr;
                let step = &index.index;
                let value = &assign.right;
                *expr = if matches!(&**step, syn::Expr::Range(_)) {
                    syn::parse_quote!(#base.__set_bits(#step, #value))
                } else {
                    syn::parse_quote!(#base.__set_bit(#step, #value))
                };
            }
            // `match self.sel { 0 =>.., 1 =>.. }` compares a hardware value
            // with integer patterns, which Rust patterns cannot express. When
            // every arm is a literal, a range, an or-pattern or `_`, the match
            // becomes a guard chain over a bound scrutinee: same order, same
            // result, and the analyzer accepts it.
            syn::Expr::Match(item) => {
                if let Some(rewritten) = rewrite_match(item) {
                    *expr = rewritten;
                }
            }
            // An enum constructor takes hardware values: `Op::Load(0x40)`.
            // Only an upper-case callee is treated as one, so ordinary calls
            // such as `ns(1)` keep their integer argument.
            syn::Expr::Call(call) => {
                let syn::Expr::Path(callee) = &*call.func else {
                    return;
                };
                let is_constructor = callee
                    .path
                    .segments
                    .last()
                    .and_then(|segment| segment.ident.to_string().chars().next())
                    .is_some_and(|first| first.is_uppercase());
                if is_constructor {
                    for argument in &mut call.args {
                        wrap_literal(argument);
                    }
                }
            }
            _ => {}
        }
    }

    fn visit_expr_assign_mut(&mut self, assign: &mut syn::ExprAssign) {
        syn::visit_mut::visit_expr_assign_mut(self, assign);
        wrap_literal(&mut assign.right);
    }

    /// A memory write takes the element type as its second argument:
    /// `mem.write(addr, 0)` passes a literal where that type is expected.
    fn visit_expr_method_call_mut(&mut self, call: &mut syn::ExprMethodCall) {
        syn::visit_mut::visit_expr_method_call_mut(self, call);
        if call.method == "write" {
            if let Some(value) = call.args.iter_mut().last() {
                wrap_literal(value);
            }
        }
    }

    /// An integer literal next to a hardware value is a hardware value of the
    /// other operand's width: `a + 1`, `a & 0xF0`, `a >> 1`, `a == 0`.
    ///
    /// Only the literal side is wrapped, and only when the other side is not a
    /// literal itself, so the operator implementations see two values of the
    /// same type.
    fn visit_expr_binary_mut(&mut self, binary: &mut syn::ExprBinary) {
        syn::visit_mut::visit_expr_binary_mut(self, binary);
        let left_literal = is_integer_literal(&binary.left);
        let right_literal = is_integer_literal(&binary.right);
        if left_literal && !right_literal {
            wrap_literal(&mut binary.left);
        } else if right_literal && !left_literal {
            wrap_literal(&mut binary.right);
        }
    }

    fn visit_local_mut(&mut self, local: &mut syn::Local) {
        syn::visit_mut::visit_local_mut(self, local);
        if let Some(init) = &mut local.init {
            wrap_literal(&mut init.expr);
        }
    }

    fn visit_field_value_mut(&mut self, field: &mut syn::FieldValue) {
        syn::visit_mut::visit_field_value_mut(self, field);
        wrap_literal(&mut field.expr);
    }

    fn visit_expr_array_mut(&mut self, array: &mut syn::ExprArray) {
        syn::visit_mut::visit_expr_array_mut(self, array);
        for element in &mut array.elems {
            wrap_literal(element);
        }
    }

    fn visit_arm_mut(&mut self, arm: &mut syn::Arm) {
        syn::visit_mut::visit_arm_mut(self, arm);
        wrap_literal(&mut arm.body);
    }

    /// A const generic argument such as `U<{ W + 1 }>` uses arithmetic on a
    /// generic parameter, which stable Rust rejects outside a few forms. The
    /// editor view only needs a well-formed width, so such an argument becomes
    /// the literal `1`; every occurrence of the same expression is rewritten
    /// the same way, so the types still agree with each other.
    fn visit_angle_bracketed_generic_arguments_mut(
        &mut self,
        arguments: &mut syn::AngleBracketedGenericArguments,
    ) {
        syn::visit_mut::visit_angle_bracketed_generic_arguments_mut(self, arguments);
        for argument in &mut arguments.args {
            let syn::GenericArgument::Const(value) = argument else {
                continue;
            };
            if !matches!(
                value,
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(_),
                    ..
                })
            ) {
                *value = syn::parse_quote!(1);
            }
        }
    }

    /// `uint<N>` / `int<N>` are the DSL spellings of the library's `U<N>` /
    /// `I<N>`; the Rust view has to name the types that exist.
    fn visit_type_path_mut(&mut self, path: &mut syn::TypePath) {
        syn::visit_mut::visit_type_path_mut(self, path);
        if path.qself.is_some() || path.path.segments.len() != 1 {
            return;
        }
        let segment = &mut path.path.segments[0];
        let replacement = match segment.ident.to_string().as_str() {
            "uint" => "U",
            "int" => "I",
            _ => return,
        };
        segment.ident = syn::Ident::new(replacement, segment.ident.span());
    }

    fn visit_type_array_mut(&mut self, array: &mut syn::TypeArray) {
        syn::visit_mut::visit_type_array_mut(self, array);
        if !matches!(
            &array.len,
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(_),
                ..
            }) | syn::Expr::Path(_)
        ) {
            array.len = syn::parse_quote!(1);
        }
    }

    /// Rewrite the body of `initial!`, whose tokens are ordinary statements.
    ///
    /// A macro body is token trees, not expressions, so the assignment and
    /// index rewrites above never see it. Only `initial!` has a body that is a
    /// plain statement list; `always!` carries event arms and is left alone.
    fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
        syn::visit_mut::visit_macro_mut(self, mac);
        let name = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_default();
        if name == "initial" {
            let tokens = mac.tokens.clone();
            let Ok(mut block) = syn::parse2::<syn::Block>(quote!({ #tokens })) else {
                return;
            };
            rewrite_statements(self, &mut block.stmts);
            let statements = block.stmts;
            mac.tokens = quote!(#(#statements)*);
            return;
        }
        // `assert_eq!(dut.sum, 0x15)` compares a hardware value with a literal;
        // the literal is a hardware value of the same type.
        if matches!(name.as_str(), "assert" | "assert_eq" | "assert_ne") {
            let tokens = mac.tokens.clone();
            let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
            let Ok(mut arguments) = syn::parse::Parser::parse2(parser, tokens) else {
                return;
            };
            for argument in &mut arguments {
                self.visit_expr_mut(argument);
                wrap_literal(argument);
            }
            let arguments = arguments.into_iter().collect::<Vec<_>>();
            mac.tokens = quote!(#(#arguments),*);
        }
    }
}

/// `U<N>` / `I<N>` / `uint<N>` / `int<N>` in type position, as the
/// constructor and width to rewrite a cast to.
fn hardware_type(ty: &syn::Type) -> Option<(syn::Ident, syn::Expr)> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    if path.qself.is_some() || path.path.segments.len() != 1 {
        return None;
    }
    let segment = path.path.segments.first()?;
    let constructor = match segment.ident.to_string().as_str() {
        "U" | "uint" => syn::Ident::new("U", segment.ident.span()),
        "I" | "int" => syn::Ident::new("I", segment.ident.span()),
        _ => return None,
    };
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let mut args = arguments.args.iter();
    let syn::GenericArgument::Const(width) = args.next()? else {
        return None;
    };
    if args.next().is_some() {
        return None;
    }
    Some((constructor, width.clone()))
}

/// Wrap a bare integer literal in `::shyhdl::__bits(..)`, so a value position
/// that expects a hardware value accepts it with the width inferred from the
/// context. Any other expression is left alone.
fn wrap_literal(expr: &mut syn::Expr) {
    if !is_integer_literal(expr) {
        return;
    }
    let value = expr.clone();
    *expr = syn::parse_quote!(::shyhdl::__bits(#value));
}

/// True for a bare integer literal, the only form the rewrite wraps.
fn is_integer_literal(expr: &syn::Expr) -> bool {
    matches!(
        expr,
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(_),
            ..
        })
    )
}

/// Rewrite a match whose arms are integer patterns into a guard chain.
///
/// Returns `None` when any arm cannot be expressed as a comparison (a binding,
/// a variant, a slice,...): the match is then left exactly as written, so the
/// rewrite never changes what the code means.
fn rewrite_match(item: &syn::ExprMatch) -> Option<syn::Expr> {
    // A scrutinee with side effects would run twice if it appeared in every
    // guard; only a place or a path is bound once and compared.
    if !matches!(
        &*item.expr,
        syn::Expr::Path(_) | syn::Expr::Field(_) | syn::Expr::Index(_)
    ) {
        return None;
    }
    let scrutinee = syn::Ident::new("__shyhdl_scrutinee", proc_macro2::Span::call_site());
    let scrutinee_expr: syn::Expr = syn::parse_quote!(#scrutinee);
    let mut arms: Vec<syn::Arm> = Vec::new();
    let mut has_wildcard = false;
    for arm in &item.arms {
        // An or-pattern matches when any alternative matches, so each
        // alternative becomes its own guarded arm: same order, same result.
        for pattern in or_alternatives(&arm.pat) {
            let alternative = pattern_alternative(pattern, &scrutinee_expr)?;
            let mut condition = alternative.condition;
            if let Some((_, guard)) = &arm.guard {
                let mut guard = (**guard).clone();
                for (name, value) in &alternative.bindings {
                    substitute_ident(&mut guard, name, value);
                }
                EditorRewrite.visit_expr_mut(&mut guard);
                condition = match condition {
                    Some(existing) => Some(syn::parse_quote!((#existing) && (#guard))),
                    None => Some(guard),
                };
            }
            let body = &arm.body;
            let statement = if alternative.bindings.is_empty() {
                quote!(#body)
            } else {
                let lets = alternative
                    .bindings
                    .iter()
                    .map(|(name, value)| quote!(let #name = #value;));
                quote!({
                    #(#lets)*
                    #body
                })
            };
            match condition {
                Some(condition) => arms.push(syn::parse_quote!(_ if #condition => #statement)),
                None => {
                    has_wildcard = true;
                    arms.push(syn::parse_quote!(_ => #statement));
                }
            }
        }
    }
    if !has_wildcard {
        arms.push(syn::parse_quote!(_ => unreachable!()));
    }
    let scrutinee_expr = &item.expr;
    Some(syn::parse_quote! {
        {
            let #scrutinee = #scrutinee_expr;
            match () {
                #(#arms),*
            }
        }
    })
}

/// The alternatives of a top-level or-pattern; a plain pattern is one.
fn or_alternatives(pattern: &syn::Pat) -> Vec<&syn::Pat> {
    match pattern {
        syn::Pat::Or(or) => or.cases.iter().collect(),
        other => vec![other],
    }
}

/// One alternative of an integer pattern, ready to become a guarded arm.
struct Alternative {
    /// The comparison that must hold, or `None` for the wildcard.
    condition: Option<syn::Expr>,
    /// Names the pattern binds, and the value each names.
    bindings: Vec<(syn::Ident, syn::Expr)>,
}

/// Rewrite one (non-or) pattern into a condition plus its bindings.
///
/// Only patterns that Rust cannot match against a hardware value are handled:
/// integer literals, ranges of integers, bit masks, and array patterns built
/// from those and from plain bindings. Anything else makes the caller abandon
/// the rewrite and keep the match exactly as written.
fn pattern_alternative(pattern: &syn::Pat, scrutinee: &syn::Expr) -> Option<Alternative> {
    match pattern {
        syn::Pat::Wild(_) => Some(Alternative {
            condition: None,
            bindings: Vec::new(),
        }),
        syn::Pat::Lit(syn::PatLit { lit, .. }) => {
            let syn::Lit::Int(_) = lit else {
                return None;
            };
            Some(Alternative {
                condition: Some(syn::parse_quote!(#scrutinee == ::shyhdl::__bits(#lit))),
                bindings: Vec::new(),
            })
        }
        syn::Pat::Ident(binding)
            if binding.by_ref.is_none()
                && binding.subpat.is_none()
                && binding.mutability.is_none() =>
        {
            Some(Alternative {
                condition: None,
                bindings: vec![(binding.ident.clone(), scrutinee.clone())],
            })
        }
        syn::Pat::Paren(paren) => pattern_alternative(&paren.pat, scrutinee),
        syn::Pat::Range(range) => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(lower),
                ..
            }) = range.start.as_deref()?
            else {
                return None;
            };
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(upper),
                ..
            }) = range.end.as_deref()?
            else {
                return None;
            };
            let comparison =
                if range.limits == syn::RangeLimits::Closed(syn::token::DotDotEq::default()) {
                    syn::parse_quote!(
                        #scrutinee >= ::shyhdl::__bits(#lower)
                            && #scrutinee <= ::shyhdl::__bits(#upper)
                    )
                } else {
                    syn::parse_quote!(
                        #scrutinee >= ::shyhdl::__bits(#lower)
                            && #scrutinee < ::shyhdl::__bits(#upper)
                    )
                };
            Some(Alternative {
                condition: Some(comparison),
                bindings: Vec::new(),
            })
        }
        // An array pattern such as `[x, 5]`: each element is a place of the
        // scrutinee, so the element patterns compose into one condition.
        syn::Pat::Slice(slice) => {
            let mut condition: Option<syn::Expr> = None;
            let mut bindings: Vec<(syn::Ident, syn::Expr)> = Vec::new();
            for (index, element) in slice.elems.iter().enumerate() {
                let place: syn::Expr = syn::parse_quote!(#scrutinee[#index]);
                let alternative = pattern_alternative(element, &place)?;
                if let Some(next) = alternative.condition {
                    condition = match condition {
                        Some(existing) => Some(syn::parse_quote!((#existing) && (#next))),
                        None => Some(next),
                    };
                }
                bindings.extend(alternative.bindings);
            }
            Some(Alternative {
                condition,
                bindings,
            })
        }
        _ => None,
    }
}

/// Replace every reference to `name` in an expression with `value`.
fn substitute_ident(expr: &mut syn::Expr, name: &syn::Ident, value: &syn::Expr) {
    struct Substitute<'a> {
        name: &'a syn::Ident,
        value: &'a syn::Expr,
    }
    impl syn::visit_mut::VisitMut for Substitute<'_> {
        fn visit_expr_mut(&mut self, expr: &mut syn::Expr) {
            if let syn::Expr::Path(path) = expr {
                if path.qself.is_none()
                    && path.path.segments.len() == 1
                    && path.path.segments[0].ident == *self.name
                {
                    *expr = self.value.clone();
                    return;
                }
            }
            syn::visit_mut::visit_expr_mut(self, expr);
        }
    }
    Substitute { name, value }.visit_expr_mut(expr);
}

/// Rewrite a statement list, including the macros inside it.
///
/// `visit_block_mut` reaches the expressions of a statement, but a macro
/// such as `assert_eq!` keeps its arguments as raw tokens, so each one is
/// handled here as well.
fn rewrite_statements(rewriter: &mut EditorRewrite, statements: &mut [syn::Stmt]) {
    for statement in statements.iter_mut() {
        if let syn::Stmt::Macro(item) = statement {
            rewriter.visit_macro_mut(&mut item.mac);
        }
        if let syn::Stmt::Expr(expression, _) = statement {
            rewriter.visit_expr_mut(expression);
        }
        if let syn::Stmt::Local(local) = statement {
            rewriter.visit_local_mut(local);
        }
    }
}

/// The editor view of an impl block: the same items, with the body rewritten
/// so it type-checks as ordinary Rust.
fn editor_impl(item: &syn::ItemImpl) -> TokenStream2 {
    let mut item = item.clone();
    EditorRewrite.visit_item_impl_mut(&mut item);
    quote! {
        #[allow(dead_code, unused_variables, unused_assignments, unreachable_patterns)]
        #item
    }
}

/// The editor view of a function: the same body, rewritten and with the
/// original return value kept.
fn editor_fn(item: &syn::ItemFn) -> TokenStream2 {
    let mut item = item.clone();
    EditorRewrite.visit_item_fn_mut(&mut item);
    quote! {
        #[allow(dead_code, unused_variables, unused_assignments, unreachable_patterns)]
        #item
    }
}

/// The editor view of a test: the body is analyzed, and the placeholder's
/// `TestId` return is appended so call sites still type-check.
fn editor_test(item: &syn::ItemFn, name_literal: &syn::LitStr) -> TokenStream2 {
    let mut item = item.clone();
    EditorRewrite.visit_item_fn_mut(&mut item);
    let vis = &item.vis;
    let ident = &item.sig.ident;
    let body = &item.block;
    quote! {
        #[allow(dead_code, unused_variables, unused_assignments, unreachable_patterns)]
        #vis fn #ident() -> ::shyhdl::TestId {
            #body
            ::shyhdl::TestId(#name_literal)
        }
    }
}

/// The editor view of a data struct or enum.
///
/// The frontend reads the declaration as bytes of a fixed width; in the editor
/// those values are copied out of places, compared and printed, so the type
/// needs the traits those operations require. A trait the declaration already
/// derives is not added twice.
fn editor_data(input: &Item) -> TokenStream2 {
    let mut input = input.clone();
    let attrs = match &mut input {
        Item::Struct(item) => &mut item.attrs,
        Item::Enum(item) => &mut item.attrs,
        _ => unreachable!("called for a struct or an enum"),
    };
    let mut derived: Vec<String> = Vec::new();
    for attr in attrs.iter() {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if let Some(ident) = meta.path.get_ident() {
                derived.push(ident.to_string());
            }
            Ok(())
        });
    }
    let missing: Vec<syn::Ident> = ["Clone", "Copy", "PartialEq", "Eq", "Debug"]
        .into_iter()
        .filter(|name| !derived.iter().any(|existing| existing == name))
        .map(|name| syn::Ident::new(name, Span::call_site()))
        .collect();
    if !missing.is_empty() {
        attrs.insert(0, syn::parse_quote!(#[derive(#(#missing),*)]));
    }
    quote! {
        #[allow(dead_code)]
        #input
    }
}

/// The `new()` constructor the editor view exposes for a hardware struct.
fn editor_constructor(item: &syn::ItemStruct) -> TokenStream2 {
    let vis = &item.vis;
    let ident = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    quote! {
        #[allow(dead_code)]
        impl #impl_generics #ident #ty_generics #where_clause {
            #vis fn new() -> Self {
                unreachable!("hardware modules are compiled by ShyHDL, not constructed by Rust")
            }
        }
    }
}

/// `#[hardware_instance]` on a struct: one ShyHDL hardware module.
///
/// The placeholder is a unit struct with the same visibility, name and const
/// generic parameters (`pub struct Adder<const N: usize>;`). A type parameter
/// cannot appear in a unit struct, so when one is written it is kept alive with a
/// `PhantomData` field instead.
#[proc_macro_attribute]
pub fn hardware_instance(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as Item);
    match input {
        Item::Struct(item_struct) => {
            let name = item_struct.ident.to_string();
            // The registered text keeps the attribute: the frontend looks for it to
            // tell a hardware struct from a plain data struct.
            let source = with_attribute(
                &syn::parse_quote!(#[hardware_instance]),
                item_struct.to_token_stream(),
            );

            let declared = hardware_struct(&item_struct);
            let editor_constructor = editor_constructor(&item_struct);
            let registration = register("Instance", &name, &source);
            quote! {
                #declared
                #[cfg(rust_analyzer)]
                #editor_constructor
                #registration
            }
            .into()
        }
        // A `fn` marked with the attribute is a combinational function: a DSL
        // declaration with no Rust-side item. The editor still sees the body.
        Item::Fn(item_fn) => {
            let name = item_fn.sig.ident.to_string();
            let source = with_attribute(
                &syn::parse_quote!(#[hardware_instance]),
                item_fn.to_token_stream(),
            );
            let editor = editor_fn(&item_fn);
            let registration = register("Function", &name, &source);
            quote! {
                #[cfg(rust_analyzer)]
                #editor
                #registration
            }
            .into()
        }
        other => wrong_item(
            "`#[hardware_instance]`",
            "a struct or a combinational function",
            other.to_token_stream(),
        ),
    }
}

/// `#[hardware]` on a struct, an enum or an impl block: a data type or a body.
///
/// A struct or enum keeps its own definition, because the DSL types such as
/// `U<8>` are provided by the library and the Rust side stays usable. An impl
/// block becomes `impl.. ::shyhdl::Hardware for Type.. {}`: the trait has a
/// default `run`, so an empty impl is the placeholder for a body that is compiled
/// later from the registered text.
#[proc_macro_attribute]
pub fn hardware(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as Item);
    match input {
        Item::Struct(_) | Item::Enum(_) => {
            let name = match &input {
                Item::Struct(item_struct) => item_struct.ident.to_string(),
                Item::Enum(item_enum) => item_enum.ident.to_string(),
                _ => unreachable!("matched above"),
            };
            let source =
                with_attribute(&attribute_named("hardware", &attr), input.to_token_stream());
            // The definition is a placeholder: its variants and fields are used
            // by the frontend, so rustc must not report them as dead code.
            let placeholder = quote! {
                #[allow(dead_code)]
                #input
            };
            // The editor analyzes the declared fields and their uses in `run`,
            // so a data value has to support the operations the DSL writes on
            // it: copying out of a place, comparing, and printing in
            // `assert_eq!`.
            let editor = editor_data(&input);
            let kind = match &input {
                Item::Struct(_) => "Data",
                _ => "Enum",
            };
            let registration = register(kind, &name, &source);
            quote! {
                #[cfg(not(rust_analyzer))]
                #placeholder
                #[cfg(rust_analyzer)]
                #editor
                #registration
            }
            .into()
        }
        Item::Impl(item_impl) => {
            let Some(name) = self_type_name(&item_impl.self_ty) else {
                return wrong_item(
                    "`#[hardware]` on an impl",
                    "an impl block whose type has a name",
                    item_impl.to_token_stream(),
                );
            };
            let source = with_attribute(
                &attribute_named("hardware", &attr),
                item_impl.to_token_stream(),
            );

            let (impl_generics, _, where_clause) = item_impl.generics.split_for_impl();
            let self_ty = &item_impl.self_ty;
            let placeholder = quote! {
                impl #impl_generics ::shyhdl::Hardware for #self_ty #where_clause {}
            };
            let registration = register("Impl", &name, &source);
            let editor = editor_impl(&item_impl);
            quote! {
                #[cfg(not(rust_analyzer))]
                #placeholder
                #[cfg(rust_analyzer)]
                #editor
                #registration
            }
            .into()
        }
        other => wrong_item(
            "`#[hardware]`",
            "a struct, an enum or an impl block",
            other.to_token_stream(),
        ),
    }
}

/// `#[hardware_test]` on a function: one ShyHDL testbench.
///
/// The placeholder is a function with the same name and visibility that returns
/// the test's `TestId`, so a test can be handed to the simulator
/// (`sim.run(adds)`). The registered text carries the attribute with its
/// arguments (`timeunit`, `precision`,..) and the original body.
#[proc_macro_attribute]
pub fn hardware_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as Item);
    match input {
        Item::Fn(item_fn) => {
            let name = item_fn.sig.ident.to_string();
            let source = with_attribute(
                &attribute_named("hardware_test", &attr),
                item_fn.to_token_stream(),
            );

            // The placeholder returns the test's id, so a test is run by handing it
            // to the simulator (`sim.run(adds)`). A ShyHDL test takes no arguments
            // and returns nothing, whatever the written signature says.
            let vis = &item_fn.vis;
            let ident = &item_fn.sig.ident;
            // `quote!` interpolates tokens, so the name goes in as a string literal.
            let name_literal = syn::LitStr::new(&name, Span::call_site());
            let placeholder = quote! {
                #[allow(dead_code)]
                #vis fn #ident() -> ::shyhdl::TestId {
                    ::shyhdl::TestId(#name_literal)
                }
            };
            let editor = editor_test(&item_fn, &name_literal);
            let registration = register("Test", &name, &source);
            quote! {
                #[cfg(not(rust_analyzer))]
                #placeholder
                #[cfg(rust_analyzer)]
                #editor
                #registration
            }
            .into()
        }
        other => wrong_item("`#[hardware_test]`", "a function", other.to_token_stream()),
    }
}

/// The struct view used only by rust-analyzer. The normal placeholder is kept
/// deliberately empty so the DSL fields are not type-checked by rustc; the
/// analyzer needs those fields to resolve `self.foo` and offer completion.
fn hardware_struct(item: &syn::ItemStruct) -> TokenStream2 {
    let mut item = item.clone();
    for field in &mut item.fields {
        field.attrs.clear();
    }
    EditorRewrite.visit_item_struct_mut(&mut item);
    keep_type_parameters_alive(&mut item);
    quote! {
        #[allow(dead_code)]
        #item
    }
}

/// A type parameter that no field mentions would be an error ("parameter is
/// never used"). The DSL never mentions one either, so it is kept alive with a
/// marker field, exactly as the empty placeholder used to do.
fn keep_type_parameters_alive(item: &mut syn::ItemStruct) {
    let parameters: Vec<String> = item
        .generics
        .params
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter) => Some(parameter.ident.to_string()),
            _ => None,
        })
        .collect();
    if parameters.is_empty() {
        return;
    }
    let mentioned = item.fields.iter().any(|field| {
        let spelled = field.ty.to_token_stream().to_string();
        parameters
            .iter()
            .any(|name| spelled.contains(name.as_str()))
    });
    if mentioned {
        return;
    }
    let identifiers: Vec<syn::Ident> = parameters
        .iter()
        .map(|name| syn::Ident::new(name, Span::call_site()))
        .collect();
    let syn::Fields::Named(named) = &mut item.fields else {
        return;
    };
    named.named.push(syn::parse_quote! {
        #[allow(dead_code)]
        __shyhdl_marker: ::core::marker::PhantomData<(#(#identifiers,)*)>
    });
}
/// The attribute as written (`#[name]` or `#[name(args)]`), rebuilt for the
/// registered text.
fn attribute_named(name: &str, args: &TokenStream) -> syn::Attribute {
    let ident = syn::Ident::new(name, Span::call_site());
    if args.is_empty() {
        syn::parse_quote!(#[#ident])
    } else {
        let args: TokenStream2 = args.clone().into();
        syn::parse_quote!(#[#ident(#args)])
    }
}

/// The name of the type an impl block is for.
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

/// The registered text: the attribute on its own line, then the item's tokens.
///
/// One newline is all the separation the front end needs, and it keeps the
/// `#[hardware_instance]` / `#[hardware]` / `#[hardware_test]` prefix that tells
/// the parts apart when they are parsed again.
fn with_attribute(attribute: &syn::Attribute, item: TokenStream2) -> String {
    format!("{}\n{}", attribute.to_token_stream(), item)
}

/// The registration record: one `RegisteredPart` submitted to the library's
/// inventory, so it is collected at link time with no start-up code.
fn register(kind: &str, name: &str, source: &str) -> TokenStream2 {
    let kind = syn::Ident::new(kind, Span::call_site());
    let name = syn::LitStr::new(name, Span::call_site());
    let source = syn::LitStr::new(source, Span::call_site());
    quote! {
        ::shyhdl::__inventory::submit! {
            ::shyhdl::RegisteredPart {
                kind: ::shyhdl::PartKind::#kind,
                name: #name,
                source: #source,
                file: file!(),
                line: line!(),
            }
        }
    }
}

/// Report a wrong item kind without hiding what the user wrote.
fn wrong_item(macro_name: &str, expected: &str, original: TokenStream2) -> TokenStream {
    let message = format!("{macro_name} can only be used on {expected}");
    let error = syn::Error::new(Span::call_site(), message).to_compile_error();
    quote! {
        #error
        #original
    }
    .into()
}
