//! What one source file declares, for callers that need the candidates before
//! compiling anything.
//!
//! The frontend compiles one top module at a time, but an editor has to find out
//! which modules a file even has. This module answers that from the existing
//! structural pass, so no second parser is grown for it.

use crate::frontend::diagnostic::Diagnostics;
use crate::frontend::parse;

/// Names of the `#[hardware_instance]` structs in `source`, in source order.
///
/// A file that is not valid ShyHDL at all declares nothing that can be named;
/// use [`concrete_instances`] when the syntax errors are needed as well.
pub fn declared_instances(source: &str, source_name: &str) -> Vec<String> {
    match parse::parse(source, source_name) {
        Ok(parsed) => parsed.hw.iter().map(|hw| hw.name.clone()).collect(),
        Err(_) => Vec::new(),
    }
}

/// The declared instances that can be compiled as a top module with no
/// arguments: every const parameter has a default.
///
/// A struct with a required parameter (`Adder<const N: usize>`) is a module that
/// is built at a concrete width by whoever instantiates it. Probing it as a top
/// would report "parameter `N` is not given and has no default", which is an
/// artifact of the probe rather than a property of the file, so such a struct is
/// checked through the tops that instantiate it instead.
///
/// `Err` carries the diagnostics of a file that is not valid ShyHDL at all.
pub fn concrete_instances(source: &str, source_name: &str) -> Result<Vec<String>, Diagnostics> {
    let parsed = parse::parse(source, source_name)?;
    Ok(parsed
        .hw
        .iter()
        .filter(|hw| hw.params.iter().all(|param| param.default.is_some()))
        .map(|hw| hw.name.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::codes;

    const SOURCE: &str = r#"
#[hardware_instance]
struct First {
    #[In] a: bool,
}

#[hardware_instance]
struct Generic<const N: usize> {
    #[In] a: bool,
}

#[hardware_instance]
struct Defaulted<const N: usize = 4> {
    #[In] a: bool,
}
"#;

    #[test]
    fn declared_instances_are_listed_in_source_order() {
        assert_eq!(
            declared_instances(SOURCE, "m.shy.rs"),
            ["First", "Generic", "Defaulted"]
        );
    }

    #[test]
    fn only_structs_without_required_parameters_are_concrete_tops() {
        assert_eq!(
            concrete_instances(SOURCE, "m.shy.rs").unwrap(),
            ["First", "Defaulted"]
        );
    }

    #[test]
    fn a_syntax_error_is_returned_instead_of_an_outline() {
        let errors = concrete_instances("struct {", "m.shy.rs").unwrap_err();
        assert!(!errors.is_empty());
        assert!(errors.as_slice().iter().all(|d| d.code == codes::PARSE));
        assert!(declared_instances("struct {", "m.shy.rs").is_empty());
    }
}
