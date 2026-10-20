//! Building the shared [`DesignMetadata`] from a lowered design.
//!
//! The debugger reads this to turn a recorded RTL signal back into a typed
//! source value.
//!
//! Variables are emitted **per instance**, not per module type: `RegisterPair`
//! with two `Register` children gets two sets of variables, addressed
//! `RegisterPair.wide.q` and `RegisterPair.narrow.q`. Two instances of the same
//! module therefore never share an id, and the whole tree below the top module
//! is covered. The top module's own signals use the top module's Verilog name as
//! their root path; a consumer that knows the name the testbench gives the DUT
//! instance can substitute that root.
//!
//! Process locals are deliberately *not* debug variables: a temporary inside a
//! process is not a stable observation point. They stay reachable
//! through [`crate::frontend::Design`] for tooling that needs the full model.

use crate::error::Error;
use crate::frontend::mir::{Design, Module, SignalScope};
use crate::metadata::{DesignMetadata, Variable};

/// Build the metadata for a lowered design.
///
/// Fails when an instantiated module is missing from the design: that would mean
/// a part of the hierarchy has no symbols, which must never be reported as a
/// complete metadata set.
pub(crate) fn build_metadata(design: &Design) -> Result<DesignMetadata, Error> {
    let top = design.top().verilog_name.clone();

    let mut variables = Vec::new();
    collect_variables(design, design.top(), &top, &mut variables)?;

    Ok(DesignMetadata::new(top, variables))
}

/// Add one module instance's variables, then recurse into its children.
///
/// The instantiation graph is acyclic (recursion is a compile error), so this
/// terminates.
fn collect_variables(
    design: &Design,
    module: &Module,
    path: &str,
    out: &mut Vec<Variable>,
) -> Result<(), Error> {
    for signal in &module.signals {
        if !matches!(signal.scope, SignalScope::Module) {
            continue;
        }
        // Compiler-introduced signals (version wires, temporaries, the parent net
        // of a child output) are not source variables: exposing them would give
        // the debugger names the user never wrote. A child's real signals are
        // emitted by the recursion below.
        let Some(source_name) = signal.source_name.clone() else {
            continue;
        };
        out.push(Variable {
            id: format!("{path}/{}", signal.name),
            source_name,
            instance_path: path.to_string(),
            rtl_name: signal.name.clone(),
            ty: signal.ty.to_value_type(),
            source: signal
                .source
                .clone()
                .unwrap_or_else(|| module.source.clone()),
        });
    }

    for instance in &module.instances {
        let Some(child) = design.module(&instance.module_verilog_name) else {
            // Never skip a level silently: metadata that misses part of the
            // hierarchy would look complete to the debugger.
            return Err(Error::InvalidMetadata {
                message: format!(
                    "module `{}` instantiated as `{path}.{}` is missing from the design",
                    instance.module_verilog_name, instance.verilog_name
                ),
            });
        };
        collect_variables(
            design,
            child,
            &format!("{path}.{}", instance.verilog_name),
            out,
        )?;
    }
    Ok(())
}
