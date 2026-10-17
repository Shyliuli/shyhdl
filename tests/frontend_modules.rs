//! Multi-file designs: `mod name;` loads another source file into the same
//! design namespace.
//!
//! The files live in `tests/fixtures/modules/`, so these tests exercise the real
//! loader (paths, nesting, missing files, cycles) rather than a stub.

use std::path::{Path, PathBuf};

use shyhdl::frontend::{codes, compile, compile_all_file, compile_file, load_design, Diagnostic};

/// Directory of the fixture files.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/modules")
}

/// The entry file of one case directory.
fn entry(case: &str) -> PathBuf {
    fixtures().join(case).join("top.shy.rs")
}

/// The single error a failing load reports.
fn error_of(path: &Path, top: &str) -> Diagnostic {
    match compile_file(path, top) {
        Ok(compiled) => panic!(
            "expected a diagnostic, but the design compiled:\n{}",
            compiled.verilog
        ),
        Err(diagnostic) => diagnostic,
    }
}

#[test]
fn a_mod_loads_the_file_next_to_it_into_one_namespace() {
    let compiled =
        compile_file(&fixtures().join("top.shy.rs"), "TourTop").expect("the split design compiles");
    let verilog = &compiled.verilog;

    // The child files' modules are part of the design, and the entry file uses
    // their types without a module path.
    assert!(verilog.contains("module LeafAdder"), "{verilog}");
    assert!(verilog.contains("module Tagger"), "{verilog}");
    assert!(verilog.contains("module TourTop"), "{verilog}");
    // The hierarchy is instantiated, not inlined: both children appear as ports
    // of the top module.
    assert!(verilog.contains(".sum("), "{verilog}");
    assert!(verilog.contains(".flag("), "{verilog}");

    // Files are loaded entry first, then depth first: three files for this case,
    // and `tag.shy.rs` is two `mod` hops away.
    let loaded = load_design(&fixtures().join("top.shy.rs")).expect("the design loads");
    let names: Vec<&str> = loaded.files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names[0].ends_with("top.shy.rs"), "{names:?}");
    assert!(names[1].ends_with("leaf.shy.rs"), "{names:?}");
    assert!(names[2].ends_with("tag.shy.rs"), "{names:?}");
}

#[test]
fn a_missing_mod_file_is_reported_where_the_mod_is_written() {
    let diagnostic = error_of(&entry("missing"), "MissingTop");
    assert_eq!(diagnostic.code, codes::DECLARATION);
    assert!(
        diagnostic.message.contains("mod ghost;"),
        "the message names the module: {diagnostic}"
    );
    // Both places the loader looks are named, so the fix is obvious.
    assert!(diagnostic.message.contains("ghost.shy.rs"), "{diagnostic}");
    assert!(
        diagnostic.message.contains("ghost/mod.shy.rs"),
        "{diagnostic}"
    );
    // The error points at the `mod` statement, not at the file that is missing.
    let span = diagnostic.span.clone().expect("the diagnostic is located");
    assert_eq!(span.line, 2, "{diagnostic}");
}

#[test]
fn a_mod_cycle_is_reported_instead_of_recursing() {
    // The entry file only reaches the cycle through `mod b;` / `mod a;`.
    let diagnostic = error_of(&fixtures().join("cycle/a.shy.rs"), "CycleTop");
    assert_eq!(diagnostic.code, codes::DECLARATION);
    assert!(
        diagnostic.message.contains("cycle"),
        "the message says it is a cycle: {diagnostic}"
    );
    // The chain names the files involved, so the loop is visible.
    assert!(diagnostic.message.contains("a.shy.rs"), "{diagnostic}");
    assert!(diagnostic.message.contains("b.shy.rs"), "{diagnostic}");
    assert!(diagnostic.span.is_some(), "{diagnostic}");
}

#[test]
fn one_name_declared_in_two_files_is_reported_with_both_locations() {
    let diagnostic = error_of(&entry("duplicate"), "DuplicateTop");
    assert_eq!(diagnostic.code, codes::DECLARATION);
    assert!(
        diagnostic.message.contains("SharedName") && diagnostic.message.contains("more than once"),
        "{diagnostic}"
    );
    // The note carries the location of the first declaration.
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.message.contains("first declaration")),
        "{diagnostic}"
    );
}

#[test]
fn a_single_source_compile_points_at_compile_file_for_mod() {
    let source =
        std::fs::read_to_string(fixtures().join("top.shy.rs")).expect("the entry file is readable");
    let diagnostic = match compile(&source, "top.shy.rs", "TourTop") {
        Ok(compiled) => panic!(
            "a single-source compile cannot resolve `mod`, but it compiled:\n{}",
            compiled.verilog
        ),
        Err(diagnostic) => diagnostic,
    };
    assert_eq!(diagnostic.code, codes::DECLARATION);
    assert!(
        diagnostic.message.contains("mod leaf;"),
        "the message names the statement: {diagnostic}"
    );
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.message.contains("compile_file")),
        "the note names the entry point to use: {diagnostic}"
    );
    assert!(diagnostic.span.is_some(), "{diagnostic}");
}

#[test]
fn compile_all_file_reports_every_problem_it_finds() {
    // One file names two files that do not exist, and both are reported: a
    // failing `mod` does not hide the next one.
    let directory = std::env::temp_dir().join(format!(
        "shyhdl-modules-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a temporary directory");
    let top = directory.join("top.shy.rs");
    std::fs::write(
        &top,
        "mod absent_a;\nmod absent_b;\n\n\
         #[hardware_instance]\n\
         struct Twice {\n    #[In] a: bool,\n    #[Out] y: bool,\n}\n\
         impl Hardware for Twice {\n    fn run(&mut self) {\n        self.y = self.a;\n    }\n}\n",
    )
    .expect("write the entry file");

    let errors = compile_all_file(&top, "Twice").expect_err("both missing files are errors");
    let reported: Vec<String> = errors.errors().map(|error| error.message.clone()).collect();
    assert!(
        reported.iter().any(|message| message.contains("absent_a")),
        "{reported:?}"
    );
    assert!(
        reported.iter().any(|message| message.contains("absent_b")),
        "{reported:?}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}
