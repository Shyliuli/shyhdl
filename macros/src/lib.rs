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
//!         source: "#[hardware_instance]\nstruct Adder8 { .. }",
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
use syn::{parse_macro_input, Item};

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

            let placeholder = instance_placeholder(&item_struct);
            let registration = register("Instance", &name, &source);
            quote! {
                #placeholder
                #registration
            }
            .into()
        }
        other => wrong_item(
            "`#[hardware_instance]`",
            "a struct",
            other.to_token_stream(),
        ),
    }
}

/// `#[hardware]` on a struct, an enum or an impl block: a data type or a body.
///
/// A struct or enum keeps its own definition, because the DSL types such as
/// `U<8>` are provided by the library and the Rust side stays usable. An impl
/// block becomes `impl .. ::shyhdl::Hardware for Type .. {}`: the trait has a
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
            let kind = match &input {
                Item::Struct(_) => "Data",
                _ => "Enum",
            };
            let registration = register(kind, &name, &source);
            quote! {
                #placeholder
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
            quote! {
                #placeholder
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
/// arguments (`timeunit`, `precision`, ..) and the original body.
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
            let registration = register("Test", &name, &source);
            quote! {
                #placeholder
                #registration
            }
            .into()
        }
        other => wrong_item("`#[hardware_test]`", "a function", other.to_token_stream()),
    }
}

/// The unit-struct placeholder for a `#[hardware_instance]` struct.
fn instance_placeholder(item: &syn::ItemStruct) -> TokenStream2 {
    let vis = &item.vis;
    let ident = &item.ident;

    // The declaration's own generics, without the where clause (it is emitted
    // after the field list).
    let mut generics = item.generics.clone();
    let where_clause = generics.where_clause.take();

    let type_params: Vec<&syn::Ident> = generics
        .params
        .iter()
        .filter_map(|param| match param {
            syn::GenericParam::Type(param) => Some(&param.ident),
            _ => None,
        })
        .collect();
    if type_params.is_empty() {
        // Const parameters need nothing: a unit struct may carry them unused.
        return quote! {
            #vis struct #ident #generics #where_clause;
        };
    }
    quote! {
        #vis struct #ident #generics #where_clause {
            __shyhdl_marker: ::core::marker::PhantomData<(#(#type_params,)*)>,
        }
    }
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
