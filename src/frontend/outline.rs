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
