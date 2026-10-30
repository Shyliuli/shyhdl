//! The ShyHDL frontend: Rust-style source in, Verilog-2005 and typed modules
//! out.
//!
//! # Entry points
//!
//! * [`compile`] - compile one source file and return Verilog plus metadata,
//!   reporting the first error.
//! * [`compile_all`] - the same, but reporting every diagnostic it found.
//! * [`parse_design`] - lower without generating Verilog or metadata. This is
//!   what the testbench and waveform layers use.
//!
//! ```no_run
//! let source = std::fs::read_to_string("examples/adder.shy.rs").unwrap();
//! let compiled = shyhdl::frontend::compile(&source, "examples/adder.shy.rs", "Adder8").unwrap();
//! println!("{}", compiled.verilog);
//! for port in &compiled.design.top().ports {
//!     println!("{}: {:?} {} bits", port.verilog_name, port.dir, port.width);
//! }
//! ```
//!
//! # The source language
//!
//! A `#[hardware_instance]` struct is one module; its fields are ports
//! (`#[In]`, `#[Out]`) or child instances (`#[instance]`). A clock is an
//! ordinary `#[In]`: the language has no clock marking.
//! The `impl Hardware for...` body is `fn run(&mut self)`:
//!
//! ```text
//! fn run(&mut self) {
//!     self.adder.a = self.a;                 // continuous wiring, no child.run()
//!     let wide: uint<{ N + 1 }> = self.a.zext::<{ N + 1 }>() + self.b.zext::<{ N + 1 }>();
//!     self.sum = wide[0..N];                  // `[lo..hi]` takes the low bits
//!     self.flag = wide.get(N);
//!     self.halves[0..4] = wide[0..4];         // a bit range can be assigned
//!     if self.sel { self.y = self.a; } else { self.y = self.b; }   // default combinational logic
//!     always! {
//!         #[Nedge] self.rst_n => { self.q = 0; }   // arms before the last are asynchronous resets
//!         #[Pedge] self.clk => { if self.enable { self.q = self.d; } }   // the last arm is the clock
//!     }
//! }
//! ```
//!
//! ## Sequential processes
//!
//! `always!` lists its events as arms. `#[Pedge]`/`#[Nedge]` say which
//! transition enters an arm; where the arm stands says what it *is*:
//!
//! * every arm before the last one is an asynchronous reset. It goes into the
//!   sensitivity list and its body runs while the reset level is active:
//!   `#[Nedge]` means `if (!rst_n)`, `#[Pedge]` means `if (rst)`;
//! * the last arm is the regular edge branch: it runs when no reset level is
//!   active. A single arm therefore needs no marking at all;
//! * no role is inferred from a signal name or an edge polarity, and no
//!   attribute marks a reset: the order of the arms carries that meaning;
//! * the reset always has priority in the generated `if/else`, in source order,
//!   the first arm highest.
//!
//! An edge chain is accepted as an equivalent spelling when the intent is clear
//! from the control flow itself: every branch before the last is an
//! asynchronous reset, and the last edge branch is the regular edge branch.
//!
//! Inside an `always!` arm, module state is assigned non-blocking and a `let`
//! local is a blocking temporary, so reading a register inside the process sees
//! the previous value.
//!
//! ## Widths and bits
//!
//! Const width expressions are braced (`uint<{ N + 1 }>`), which is the only
//! spelling Rust - and therefore `syn` - accepts as a generic argument. The
//! frontend evaluates them itself while compiling; it never asks
//! rustc for generic const arithmetic. A bare `N + 1` is a syntax error and is
//! reported with that hint.
//!
//! Widths are never mixed implicitly: equal-width arithmetic wraps, and widening
//! or narrowing is written as `zext::<W>()`, `sext::<W>()`, `trunc::<W>()` or
//! `[lo..hi]` (or `.slice(lo, hi)`, which takes consecutive elements of an
//! array and keeps its type). Assignment targets may be a whole signal, a field of a
//! struct value, or a bit range (`self.y[0..4] = ...`, `self.y[2] = ...`), and
//! every bit of every driven signal must have exactly one driver.
//!
//! A struct without `#[hardware_instance]` is a plain data value with a fixed
//! layout: fields packed most significant first, no padding.
//!
//! # What is checked
//!
//! Widths are never mixed implicitly, out-of-range bits are errors, every bit
//! has one driver, every path of the default combinational logic assigns every
//! bit it drives, no
//! combinational loop (tracked per bit range) and no module recursion, child
//! inputs must be connected, and a name that cannot be a Verilog identifier is
//! reported instead of being renamed. See [`codes`] for the diagnostic codes.

pub mod diagnostic;
pub mod mir;

mod analysis;
mod consteval;
mod lower;
mod meta;
mod outline;
mod parse;
mod verilog;

pub use diagnostic::{codes, Diagnostic, Diagnostics, Note, Severity};
pub use mir::{
    BinOp, BodyItem, CaseArm, ClockSpec, Connection, Design, Edge, Expr, ExprKind, Instance,
    LValue, Memory, Module, Param, Port, PortDir, Process, ProcessKind, ResetSpec, Signal,
    SignalId, SignalKind, SignalScope, Stmt, StructTy, Ty, TyField, UnOp,
};
pub use outline::{concrete_instances, declared_instances};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::metadata::{DesignMetadata, SourceSpan};
use diagnostic::span_in;

/// A compiled design: the Verilog text, the debug metadata, and the typed
/// modules the other layers consume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledDesign {
    /// Generated Verilog-2005 for the whole design, top module first.
    pub verilog: String,
    /// Shared debug metadata for this build.
    pub metadata: DesignMetadata,
    /// The typed design the Verilog was generated from.
    pub design: Design,
    /// Non-fatal diagnostics found while compiling.
    pub warnings: Vec<Diagnostic>,
}

impl CompiledDesign {
    /// The typed design.
    pub fn design(&self) -> &Design {
        &self.design
    }

    /// The top module, whose ports the testbench drives.
    pub fn top(&self) -> &Module {
        self.design.top()
    }

    /// The module with this Verilog name, for example `Adder_N8`.
    pub fn module(&self, verilog_name: &str) -> Option<&Module> {
        self.design.module(verilog_name)
    }

    /// Non-fatal diagnostics found while compiling.
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }
}

/// Compile `source` and report the first error.
///
/// `top` names the module to build: `"Adder8"`, or `"Adder<8>"` when it is
/// generic. See [`compile_all`] to get every diagnostic.
pub fn compile(source: &str, source_name: &str, top: &str) -> Result<CompiledDesign, Diagnostic> {
    compile_all(source, source_name, top).map_err(Diagnostics::into_first_error)
}

/// Compile the entry file `source_path`, merging the files its `mod name;`
/// declarations name.
///
/// A `mod name;` is resolved **relative to the file that contains it**: the
/// first existing of `name.shy.rs` and `name/mod.shy.rs` in that directory. The
/// items of every loaded file are merged into one design namespace, so a type or
/// a hardware module defined in a child file is visible from the entry file
/// without a module path, and `top` is just the module name.
///
/// A missing file, an import cycle and a name declared in two files are errors
/// located at the `mod` statement or at both declarations; nothing is skipped.
/// Use this instead of [`compile`] whenever the source contains `mod`.
pub fn compile_file(source_path: &Path, top: &str) -> Result<CompiledDesign, Diagnostic> {
    compile_all_file(source_path, top).map_err(Diagnostics::into_first_error)
}

/// [`compile_file`], reporting every diagnostic that was found.
pub fn compile_all_file(source_path: &Path, top: &str) -> Result<CompiledDesign, Diagnostics> {
    let loaded = load_design(source_path)?;
    let design = lower::lower_parsed(loaded.parsed, &loaded.entry_name, top)?;
    let verilog = verilog::emit(&design);
    let metadata = meta::build_metadata(&design).map_err(|error| {
        Diagnostics::from(Diagnostic::error(
            codes::INTERNAL,
            None,
            format!("the design metadata could not be built: {error}"),
        ))
    })?;
    let warnings = design.warnings().to_vec();
    Ok(CompiledDesign {
        verilog,
        metadata,
        design,
        warnings,
    })
}

/// Every source file one design is loaded from, entry file first.
pub struct LoadedDesign {
    /// Parsed and merged items of every file.
    parsed: parse::ParsedFile,
    /// Name of the entry file, as diagnostics report it.
    entry_name: String,
    /// `(name, source)` of every loaded file, for the layers that scan for
    /// `#[hardware_test]` functions.
    pub files: Vec<(String, String)>,
}

/// Load `source_path` and every file it pulls in with `mod`, and merge them.
///
/// The layers that work on a single source text (the testbench scanner) use
/// [`LoadedDesign::files`]; the frontend uses the merged items.
pub fn load_design(source_path: &Path) -> Result<LoadedDesign, Diagnostics> {
    let mut files = Vec::new();
    let mut visited: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    let mut parsed = parse::ParsedFile {
        hw: Vec::new(),
        functions: Vec::new(),
        data: HashMap::new(),
        enums: HashMap::new(),
        impls: HashMap::new(),
        mods: Vec::new(),
    };
    let mut declared: HashMap<String, crate::metadata::SourceSpan> = HashMap::new();
    let mut diagnostics = Diagnostics::new();
    load_one(
        source_path,
        None,
        &mut files,
        &mut visited,
        &mut stack,
        &mut parsed,
        &mut declared,
        &mut diagnostics,
    );
    if diagnostics.has_errors() {
        return Err(diagnostics);
    }
    let entry_name = files
        .first()
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| source_path.display().to_string());
    Ok(LoadedDesign {
        parsed,
        entry_name,
        files,
    })
}

/// Load one file and, recursively, the files its `mod` declarations name.
#[allow(clippy::too_many_arguments)]
fn load_one(
    path: &Path,
    origin: Option<&SourceSpan>,
    files: &mut Vec<(String, String)>,
    visited: &mut Vec<PathBuf>,
    stack: &mut Vec<PathBuf>,
    parsed: &mut parse::ParsedFile,
    declared: &mut HashMap<String, SourceSpan>,
    diagnostics: &mut Diagnostics,
) {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // On the loading stack: the file is one of its own ancestors, so the `mod`
    // graph has a cycle and the design can never be completed.
    if stack.contains(&canonical) {
        let name = path.display().to_string();
        let mut chain: Vec<String> = stack
            .iter()
            .map(|entry| entry.display().to_string())
            .collect();
        chain.push(name.clone());
        diagnostics.push(
            Diagnostic::error(
                codes::DECLARATION,
                origin.cloned(),
                format!("`mod` cycle: {}", chain.join(" -> ")),
            )
            .with_note(
                "a file cannot load itself, directly or through another file",
                None,
            ),
        );
        return;
    }
    if visited.contains(&canonical) {
        // Already loaded, but not an ancestor: a diamond-shaped `mod` graph is
        // not a cycle, and the items are merged once.
        return;
    }
    visited.push(canonical.clone());
    stack.push(canonical.clone());

    let name = path.display().to_string();
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                codes::DECLARATION,
                Some(SourceSpan::new(name.clone(), 1, 1)),
                format!("cannot read `{name}`: {error}"),
            ));
            stack.pop();
            return;
        }
    };
    files.push((name.clone(), source.clone()));

    let file = match parse::parse(&source, &name) {
        Ok(file) => file,
        Err(errors) => {
            for error in errors.errors() {
                diagnostics.push(error.clone());
            }
            stack.pop();
            return;
        }
    };

    // Merge this file's items into the one design namespace. A name that is
    // already declared elsewhere is an error naming both declarations.
    for item in &file.hw {
        merge_name(
            &item.name,
            &item.span,
            parsed,
            declared,
            diagnostics,
            |parsed| parsed.hw.push(item.clone()),
        );
    }
    for (type_name, item) in &file.data {
        let span = span_in(&name, item.ident.span())
            .unwrap_or_else(|| SourceSpan::new(name.clone(), 1, 1));
        merge_name(type_name, &span, parsed, declared, diagnostics, |parsed| {
            parsed.data.insert(type_name.clone(), item.clone());
        });
    }
    for (type_name, item) in &file.enums {
        let span = span_in(&name, item.ident.span())
            .unwrap_or_else(|| SourceSpan::new(name.clone(), 1, 1));
        merge_name(type_name, &span, parsed, declared, diagnostics, |parsed| {
            parsed.enums.insert(type_name.clone(), item.clone());
        });
    }
    // Functions share the struct/enum namespace, so a call is never ambiguous
    // with a type. Appending keeps the declaration order of the load.
    for function in &file.functions {
        merge_name(
            &function.name,
            &function.span,
            parsed,
            declared,
            diagnostics,
            |parsed| parsed.functions.push(function.clone()),
        );
    }
    for (struct_name, implementation) in &file.impls {
        if let Some(existing) = parsed.impls.get(struct_name) {
            diagnostics.push(Diagnostic::error(
                codes::IMPL,
                Some(implementation.span.clone()),
                format!(
                    "`{struct_name}` has more than one hardware impl (the first one is at {})",
                    existing.span
                ),
            ));
            continue;
        }
        parsed
            .impls
            .insert(struct_name.clone(), implementation.clone());
    }

    // `mod name;` looks next to this file first, then in `name/mod.shy.rs`.
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    for declaration in &file.mods {
        let flat = directory.join(format!("{}.shy.rs", declaration.name));
        let nested = directory.join(&declaration.name).join("mod.shy.rs");
        let candidate = if flat.is_file() {
            flat
        } else if nested.is_file() {
            nested
        } else {
            diagnostics.push(
                Diagnostic::error(
                    codes::DECLARATION,
                    Some(declaration.span.clone()),
                    format!(
                        "`mod {};` has no file: looked for `{}` and `{}`",
                        declaration.name,
                        flat.display(),
                        nested.display()
                    ),
                )
                .with_note(
                    "a `mod` is resolved next to the file that contains it",
                    None,
                ),
            );
            continue;
        };
        load_one(
            &candidate,
            Some(&declaration.span),
            files,
            visited,
            stack,
            parsed,
            declared,
            diagnostics,
        );
    }
    stack.pop();
}

/// Merge one declared name, reporting a duplicate with both locations.
fn merge_name(
    name: &str,
    span: &SourceSpan,
    parsed: &mut parse::ParsedFile,
    declared: &mut HashMap<String, SourceSpan>,
    diagnostics: &mut Diagnostics,
    insert: impl FnOnce(&mut parse::ParsedFile),
) {
    if let Some(first) = declared.get(name) {
        diagnostics.push(
            Diagnostic::error(
                codes::DECLARATION,
                Some(span.clone()),
                format!("`{name}` is declared more than once in this design"),
            )
            .with_note(format!("the first declaration is at {first}"), None),
        );
        return;
    }
    declared.insert(name.to_string(), span.clone());
    insert(parsed);
}

/// Compile `source` and report every diagnostic that was found.
///
/// Lowering reports the errors it can find independently: a broken field does
/// not hide the next one, and each failing statement is skipped so the rest of
/// the module is still checked.
pub fn compile_all(
    source: &str,
    source_name: &str,
    top: &str,
) -> Result<CompiledDesign, Diagnostics> {
    let design = lower::lower(source, source_name, top)?;
    let verilog = verilog::emit(&design);
    let metadata = meta::build_metadata(&design).map_err(|error| {
        Diagnostics::from(Diagnostic::error(
            codes::INTERNAL,
            None,
            format!("the design metadata could not be built: {error}"),
        ))
    })?;
    let warnings = design.warnings().to_vec();
    Ok(CompiledDesign {
        verilog,
        metadata,
        design,
        warnings,
    })
}

/// Lower `source` without generating Verilog or metadata.
///
/// Use this when only the typed model is needed, for example to generate a
/// testbench for [`Design::top`]. Warnings are available through
/// [`Design::warnings`].
pub fn parse_design(source: &str, source_name: &str, top: &str) -> Result<Design, Diagnostics> {
    lower::lower(source, source_name, top)
}
