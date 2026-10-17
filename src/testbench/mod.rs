//! M2: `#[hardware_test]` functions become standalone Verilog-2005 testbenches.
//!
//! A ShyHDL test is ordinary Rust syntax, so it is parsed with `syn` and then
//! lowered to a plain Verilog testbench that runs on any Verilog-2005 tool
//! (REQ-VER-001, REQ-VER-002) without Rust DPI (REQ-VER-004). Time semantics are
//! delegated to the simulator: the generated testbench only uses `initial`,
//! `always`, `#delay`, event and condition waits (REQ-TEST-001), so a single
//! engine (`verilator --timing`, see [`crate::simulation`]) owns time and the
//! Rust side never implements a second scheduler (REQ-SIM-002).
//!
//! # What the generator needs
//!
//! The port list of the DUT top module ([`DesignInfo`]). The frontend produces
//! the RTL and the metadata; a testbench is not the place to re-parse or guess a
//! module interface, so a design without ports is an error instead of a guess.
//!
//! # Supported test syntax
//!
//! ```ignore
//! #[hardware_test(timeunit = "1ns", precision = "1ps")] // optional, this is the default
//! fn hold_reset() {
//!     let mut dut = Reg8::new();      // one instance per DUT top; ports are named nets
//!     let mut expect: uint<8> = 0;    // testbench locals need a known width
//!
//!     gen_clk(dut.clk, mhz(100));     // 100 MHz, 50% duty, exact period
//!     // or the explicit form: clock!(dut.clk, period = ns(10), initial = false);
//!     reset!(dut.rst_n, active_low, hold = ns(20));
//!
//!     initial! {
//!         dut_a = 0u8;
//!         delay(ns(1));               // #1
//!         wait(posedge(dut.clk));     // @(posedge dut_clk);
//!         wait(negedge(dut.clk));
//!         wait_until(dut_res == 0);   // wait (...);
//!         sample_after(posedge(dut.clk), ps(1)); // edge, then a positive settle offset
//!         nb_assign!(dut_a, 0x2a);    // dut_a <= ...;
//!         assert!(dut_rst_n == 1);    // no Verilog literals: Rust syntax only
//!         assert_eq!(dut_res, 0);
//!         for i in 0..8 { assert_eq!(dut_res, 0); pass!(); }
//!         finish!();                  // PASS when nothing failed, then $finish
//!     }
//! }
//! ```
//!
//! Only the documented subset is accepted. Anything else is reported as
//! [`Error::Unsupported`] with the source location of the offending construct
//! (REQ-TEST-010, REQ-VER-007); tokens are never passed through silently.
//!
//! # `match`
//!
//! A `match` over a typed value is lowered to a priority chain of the pattern
//! module's normalized tests:
//!
//! ```ignore
//! match dut.state {
//!     Idle => { pass!(); }
//!     Data(v) if v > 2 => { nb_assign!(dut_a, v); }
//!     Data(v) => { assert_eq!(dut_res, v); }
//!     other => { }                // a wildcard or a bare binding covers the rest
//! }
//! ```
//!
//! The scrutinee is resolved once: a whole signal is tested where it stands, and
//! anything else is copied into a generated temporary at the match's entry (never
//! with a continuous `assign`). Arms keep their source order, and the first pattern
//! that matches with a guard that holds wins; a guard that is false leaves the
//! match open for the other `|` alternatives of its arm and then for the arms after
//! it. The shared pattern analysis has to prove the unguarded arms total, and a
//! match it cannot prove exhaustive is rejected with a source location instead of
//! falling back to a run-time default.
//!
//! # `match` and `if` as values
//!
//! A `match`, or an `if` with an `else`, may also stand where a value is expected -
//! the initialiser of a `let`, the right of an assignment, `nb_assign!(..)`, an
//! assertion operand, a `match` arm's value - and anywhere inside such an
//! expression, including arithmetic, an index, a call argument or a nested
//! `match`/`if`:
//!
//! ```ignore
//! let mut y: uint<8> = match dut.state { Idle => 1u8, Data(v) => v };  // typed
//! let mut z = match dut.state { Idle => 1u8, _ => 2u8 };               // from `1u8`
//! dut.y = (match dut.state { Idle => 1, _ => 2 }) + 1;
//! match dut.state { Idle => 1u8, _ => 2u8 };   // value lowered, then discarded
//! ```
//!
//! Such a value is never written as a ternary: its branches become statements that
//! run where the source evaluates them, and the result is a hidden net the
//! surrounding expression reads. So a guard, a pattern binding and the
//! exhaustiveness proof behave exactly as in statement position, the scrutinee is
//! read once, and only the selected branch of an `if` or the taken arm of a `match`
//! runs its statements. Operands are read left to right like Rust: a value read
//! before a sibling's statements is captured first, and the left side of `&&`/`||`
//! is tested once, so a right-hand `match` cannot change what the operator saw.
//!
//! The type of such a value is never guessed from a width: it is the target's type
//! when the target has one, otherwise the one type its branches state (the arms
//! agree on a suffix like `1u8`, a cast, a variant or a signal), and a value that
//! states no type still needs the annotation. `wait_until(..)` is the one place
//! that refuses them, because it re-reads its condition as signals change and a
//! value lowered once would never be re-evaluated.
//!
//! # Result protocol
//!
//! The generated testbench prints machine-readable lines that
//! [`crate::simulation`] parses:
//!
//! * `SHYHDL_PASS test=<name> case=<n>` - an explicit `pass!()` marker completed.
//! * `SHYHDL_PASS test=<name> case=finish|complete` - the test reached its
//!   normal end (`finish!()`, or the last `initial!` block returned).
//! * `SHYHDL_FAIL test=<name> case=<n> check=... file=... line=...`
//! * `SHYHDL_TIMEOUT test=<name> at=<time>` - the `timeout!(...)` wrapper fired.
//! * `SHYHDL_SUMMARY test=<name> pass=<n> fail=<n> markers=<n> expected_markers=<n|unknown>`
//!
//! Nothing is inferred: a test that never reaches a normal end (a clock that
//! runs forever, a wait that never matches) prints no `SHYHDL_PASS` and is
//! reported as a timeout by the runner. After the first failed assertion the
//! testbench stops emitting `SHYHDL_PASS` entirely.

use std::path::PathBuf;

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Attribute, Item, ItemFn, Lit, Meta};

use crate::error::{Error, Result};
use crate::metadata::{SourceSpan, ValueType};

mod expr;
mod lower;

use lower::Generator;

/// Direction of a DUT port as the testbench sees it.
///
/// A clock is an ordinary input (REQ-TEST-020): which input the test drives as a
/// clock is decided by the testcase with `gen_clk(..)`/`clock!(..)`, not by a
/// marking on the design port, so there is no third direction here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDirection {
    /// Driven by the testbench: a `reg` net in the generated testbench.
    In,
    /// Driven by the DUT: a `wire` net that the test may only read.
    Out,
}

impl PortDirection {
    /// Whether the testbench drives this port.
    pub fn is_driven_by_test(self) -> bool {
        matches!(self, PortDirection::In)
    }

    /// Human-readable name used in diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            PortDirection::In => "input",
            PortDirection::Out => "output",
        }
    }
}

/// One port of the DUT top module.
///
/// `source_name` is the name a test writes (`dut.rst_n`), `verilog_name` is the
/// name in the generated RTL used for the named port connection
/// (REQ-VER-005: readable, stable names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModulePort {
    /// Name as written in the ShyHDL source.
    pub source_name: String,
    /// Name of the port in the generated RTL.
    pub verilog_name: String,
    /// Direction from the testbench's point of view.
    pub direction: PortDirection,
    /// Type, which fixes the bit layout of the port (REQ-LANG-004).
    pub ty: ValueType,
}

impl ModulePort {
    /// Build a port description.
    pub fn new(
        source_name: impl Into<String>,
        verilog_name: impl Into<String>,
        direction: PortDirection,
        ty: ValueType,
    ) -> Self {
        Self {
            source_name: source_name.into(),
            verilog_name: verilog_name.into(),
            direction,
            ty,
        }
    }

    /// Bit width of the port, from its type.
    pub fn width(&self) -> Result<u32> {
        self.ty.bit_width()
    }
}

/// Everything the testbench generator needs to know about the design under
/// test: the top module and its ports.
///
/// This is deliberately a view, not a second frontend: it is built from the
/// `Design` the frontend already produced. See the crate's `frontend` module for
/// the design model; the adapter is a few lines and lives in the integration
/// layer so this module stays independently testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignInfo {
    /// Verilog module name of the top module, as instantiated by the testbench.
    pub top: String,
    /// Source-level name of the top module, which is what `Module::new()` names.
    pub top_source_name: String,
    /// Ports in declaration order.
    pub ports: Vec<ModulePort>,
}

impl DesignInfo {
    /// Build a design view.
    pub fn new(
        top: impl Into<String>,
        top_source_name: impl Into<String>,
        ports: Vec<ModulePort>,
    ) -> Self {
        Self {
            top: top.into(),
            top_source_name: top_source_name.into(),
            ports,
        }
    }

    /// The port a test addresses as `instance.source_name`, if it exists.
    pub fn port(&self, source_name: &str) -> Option<&ModulePort> {
        self.ports
            .iter()
            .find(|port| port.source_name == source_name)
    }
}

/// A Verilog time scale or precision: `1ns`, `10ps`, `100us`, ...
///
/// Only the values Verilog-2005 allows in a `` `timescale `` directive are
/// accepted (1, 10 or 100 times a unit), so an invalid scale fails here with the
/// text that was wrong instead of producing an invalid timescale (REQ-TEST-004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeScale {
    text: String,
    femtoseconds: u64,
}

impl TimeScale {
    /// Parse `"1ns"`, `"ns"` (same as `1ns`), `"10ps"`, ...
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| Self::invalid(text, "missing time unit (s, ms, us, ns, ps, fs)"))?;
        let (digits, unit) = trimmed.split_at(split);
        let magnitude: u64 = if digits.is_empty() {
            1
        } else {
            digits
                .parse()
                .map_err(|_| Self::invalid(text, "the magnitude must be a positive integer"))?
        };
        if magnitude != 1 && magnitude != 10 && magnitude != 100 {
            return Err(Self::invalid(
                text,
                "Verilog time units may only be 1, 10 or 100 times a unit",
            ));
        }
        let unit_fs = Self::unit_femtoseconds(unit).ok_or_else(|| {
            Self::invalid(text, "unknown time unit, expected s, ms, us, ns, ps or fs")
        })?;
        let femtoseconds = magnitude
            .checked_mul(unit_fs)
            .ok_or_else(|| Self::invalid(text, "time scale is too large"))?;
        Ok(Self {
            text: format!("{magnitude}{unit}"),
            femtoseconds,
        })
    }

    fn invalid(text: &str, reason: &str) -> Error {
        Error::TypeLayout {
            message: format!("invalid time value `{text}`: {reason}"),
            span: None,
        }
    }

    fn unit_femtoseconds(unit: &str) -> Option<u64> {
        match unit {
            "s" => Some(1_000_000_000_000_000),
            "ms" => Some(1_000_000_000_000),
            "us" => Some(1_000_000_000),
            "ns" => Some(1_000_000),
            "ps" => Some(1_000),
            "fs" => Some(1),
            _ => None,
        }
    }

    /// The scale as it appears in a `` `timescale `` directive.
    pub fn as_verilog(&self) -> &str {
        &self.text
    }

    /// The scale in femtoseconds.
    pub fn femtoseconds(&self) -> u64 {
        self.femtoseconds
    }
}

impl std::fmt::Display for TimeScale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// One port net of one DUT instance in the generated testbench.
///
/// This is what maps design metadata (`instance_path` rooted at the design top)
/// onto recorded waveform signals: the debug root of the instance is
/// [`DutInstance::scope`], and the signal of a port is `scope + "." + verilog_name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DutPortBinding {
    /// Port name as written in the ShyHDL source, which is what the test uses.
    pub source_name: String,
    /// Port name in the generated RTL.
    pub verilog_name: String,
    /// Testbench net connected to the port (`<instance var>_<verilog name>`).
    pub net: String,
    /// Direction from the testbench's point of view.
    pub direction: PortDirection,
    /// Width of the port in bits.
    pub width: u32,
}

/// A clock the test generates, identified where the recording will contain it.
///
/// `signal` is the full path of the clock signal in the waveform, for example
/// `TOP.tb_reg8_hold.u_dut.clk`: it is exactly what a replay needs for its clock
/// step mode (REQ-DBG-023), so nothing about the clock has to be guessed from a
/// name suffix or a port marking (REQ-DBG-027).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedClock {
    /// Variable the test bound the DUT to (`let mut <var> = Module::new()`).
    pub var: String,
    /// Full waveform path of the generated clock signal.
    pub signal: String,
}

/// One DUT instance the generated testbench creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DutInstance {
    /// Variable the test binds the instance to (`let mut <var> = Module::new()`).
    pub var: String,
    /// Verilog instance name in the testbench (`u_<var>`).
    pub instance: String,
    /// DUT module name in the generated RTL.
    pub module: String,
    /// Source-level module name of the DUT.
    pub module_source_name: String,
    /// Waveform scope of this instance, for example `TOP.tb_reg8_hold.u_dut`.
    ///
    /// The `TOP` prefix is the model name the simulator runner gives the
    /// top-level Verilog instance, and the middle element is the generated
    /// testbench module, so a replay reads exactly this scope instead of
    /// guessing a suffix.
    pub scope: String,
    /// Port nets of this instance, in design order.
    pub ports: Vec<DutPortBinding>,
}

/// A generated, standalone Verilog-2005 testbench.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestbenchArtifact {
    /// Complete testbench source, including the `` `timescale `` directive and
    /// the DUT instantiation (REQ-VER-003: TB and DUT stay separate files, and
    /// the DUT is not re-emitted here).
    pub verilog: String,
    /// Name of the `#[hardware_test]` function.
    pub name: String,
    /// Verilog module name of the generated testbench.
    pub top: String,
    /// C++ model class name the simulator backend uses (`V` + [`Self::top`]).
    pub top_type: String,
    /// Time unit of the generated testbench.
    pub timeunit: TimeScale,
    /// Time precision of the generated testbench.
    pub timeprecision: TimeScale,
    /// Number of `pass!()` markers that are unconditionally reachable, or `None`
    /// when a marker sits under an `if`, inside `always!`, or the test can end
    /// early through `finish!()`/`timeout!()`. Only used to detect a test that
    /// stopped before all of its explicitly marked cases ran.
    pub expected_passes: Option<u64>,
    /// DUT instances created by the test, in declaration order. Each carries its
    /// own port nets and waveform scope, so several instances of the same module
    /// never share or confuse signal names.
    pub instances: Vec<DutInstance>,
    /// Clocks the test generates (`gen_clk(..)` or `clock!(..)`), in declaration
    /// order, each with its full path in the recording.
    pub clocks: Vec<GeneratedClock>,
    /// Location of the test function.
    pub source: SourceSpan,
}

/// One `#[hardware_test]` function found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestInfo {
    /// Test function name.
    pub name: String,
    /// Time unit declared on the attribute, or the `1ns` default.
    pub timeunit: TimeScale,
    /// Time precision declared on the attribute, or the `1ps` default.
    pub timeprecision: TimeScale,
    /// Location of the test function.
    pub source: SourceSpan,
}

/// Compile one `#[hardware_test]` function into a standalone Verilog-2005
/// testbench.
///
/// `design` must describe the DUT top module that the test instantiates with
/// `Module::new()`. Errors carry the source location of the construct that is
/// wrong (REQ-VER-007); nothing is emitted for a test that does not type-check
/// against the supported subset.
pub fn compile_testbench(
    source: &str,
    source_name: &str,
    test_name: &str,
    design: &DesignInfo,
) -> Result<TestbenchArtifact> {
    if design.ports.is_empty() {
        return Err(Error::TypeLayout {
            message: format!(
                "design `{}` has no ports, so no testbench can be generated; \
                 the DUT port list (In/Out/Clock) comes from the frontend",
                design.top
            ),
            span: None,
        });
    }
    let file = parse_file(source, source_name)?;
    let item = find_test(&file, test_name, source_name)?;
    let (timeunit, timeprecision) = parse_test_attributes(&item.attrs, source_name)?;
    let source_span = source_span(source_name, item.sig.ident.span());
    let mut generator = Generator::new(
        source_name,
        design,
        test_name,
        timeunit.clone(),
        timeprecision.clone(),
    )?;
    let verilog = generator.generate(item)?;
    let top = generator.top().to_string();
    Ok(TestbenchArtifact {
        verilog,
        name: test_name.to_string(),
        top_type: format!("V{top}"),
        top,
        timeunit,
        timeprecision,
        expected_passes: generator.expected_passes(),
        instances: generator.dut_instances(),
        clocks: generator.clocks(),
        source: source_span,
    })
}

/// List the `#[hardware_test]` functions of a source file, for a CLI `--list`.
pub fn list_tests(source: &str, source_name: &str) -> Result<Vec<TestInfo>> {
    let file = parse_file(source, source_name)?;
    let mut tests = Vec::new();
    for item in &file.items {
        let Item::Fn(item) = item else { continue };
        if !has_test_attribute(&item.attrs) {
            continue;
        }
        let (timeunit, timeprecision) = parse_test_attributes(&item.attrs, source_name)?;
        tests.push(TestInfo {
            name: item.sig.ident.to_string(),
            timeunit,
            timeprecision,
            source: source_span(source_name, item.sig.ident.span()),
        });
    }
    Ok(tests)
}

fn parse_file(source: &str, source_name: &str) -> Result<syn::File> {
    syn::parse_file(source).map_err(|source| Error::Parse {
        path: PathBuf::from(source_name),
        source,
    })
}

fn has_test_attribute(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("hardware_test"))
}

fn find_test<'a>(file: &'a syn::File, test_name: &str, source_name: &str) -> Result<&'a ItemFn> {
    let mut named = None;
    let mut tests = Vec::new();
    for item in &file.items {
        let Item::Fn(item) = item else { continue };
        if !has_test_attribute(&item.attrs) {
            if item.sig.ident == test_name {
                named = Some(item.sig.ident.span());
            }
            continue;
        }
        if item.sig.ident == test_name {
            return Ok(item);
        }
        tests.push(item.sig.ident.to_string());
    }
    if let Some(span) = named {
        return Err(Error::Unsupported {
            span: source_span(source_name, span),
            message: format!(
                "function `{test_name}` is not a test: add `#[hardware_test]` to generate a \
                 testbench from it"
            ),
        });
    }
    let available = if tests.is_empty() {
        "none".to_string()
    } else {
        tests.join(", ")
    };
    Err(Error::Unsupported {
        span: SourceSpan::new(source_name, 1, 1),
        message: format!(
            "no `#[hardware_test]` function named `{test_name}` in {source_name}; \
             available tests: {available}"
        ),
    })
}

/// Parse `#[hardware_test]` / `#[hardware_test(timeunit = "1ns", precision = "1ps")]`.
///
/// The attribute is also legal Rust, so the same file stays parseable as Rust
/// syntax; only this generator interprets it.
fn parse_test_attributes(attrs: &[Attribute], source_name: &str) -> Result<(TimeScale, TimeScale)> {
    let mut timeunit = None;
    let mut precision = None;
    for attr in attrs {
        if !attr.path().is_ident("hardware_test") {
            continue;
        }
        match &attr.meta {
            Meta::Path(_) => {}
            Meta::List(list) => {
                let mut seen = Vec::new();
                list.parse_nested_meta(|meta| {
                    let key = meta
                        .path
                        .get_ident()
                        .map(ToString::to_string)
                        .ok_or_else(|| meta.error("expected `timeunit` or `precision`"))?;
                    let value = meta.value()?;
                    let text = parse_meta_text(value)?;
                    seen.push((key, text, meta.path.span()));
                    Ok(())
                })
                .map_err(|source| Error::Parse {
                    path: PathBuf::from(source_name),
                    source,
                })?;
                for (key, text, span) in seen {
                    match key.as_str() {
                        "timeunit" => timeunit = Some(parse_scale(&text, span, source_name)?),
                        "precision" => precision = Some(parse_scale(&text, span, source_name)?),
                        other => {
                            return Err(Error::Unsupported {
                                span: source_span(source_name, span),
                                message: format!(
                                    "unknown `#[hardware_test]` option `{other}`; \
                                     supported options are `timeunit` and `precision`"
                                ),
                            })
                        }
                    }
                }
            }
            Meta::NameValue(_) => {
                return Err(Error::Unsupported {
                    span: source_span(source_name, attr.span()),
                    message: "write `#[hardware_test]` or \
                              `#[hardware_test(timeunit = \"1ns\", precision = \"1ps\")]`"
                        .to_string(),
                })
            }
        }
    }
    let timeunit = timeunit.unwrap_or(TimeScale::parse("1ns")?);
    let precision = precision.unwrap_or(TimeScale::parse("1ps")?);
    let ratio = timeunit.femtoseconds() / precision.femtoseconds();
    if precision.femtoseconds() > timeunit.femtoseconds()
        || !timeunit
            .femtoseconds()
            .is_multiple_of(precision.femtoseconds())
        || !is_power_of_ten(ratio)
    {
        return Err(Error::TypeLayout {
            message: format!(
                "timeunit {timeunit} and precision {precision} are not compatible: the precision \
                 must be a power of ten times smaller than the time unit"
            ),
            span: None,
        });
    }
    Ok((timeunit, precision))
}

/// `ratio` is a power of ten, which is what makes decimal conversion exact.
fn is_power_of_ten(mut ratio: u64) -> bool {
    if ratio == 0 {
        return false;
    }
    while ratio.is_multiple_of(10) {
        ratio /= 10;
    }
    ratio == 1
}

fn parse_scale(text: &str, span: Span, source_name: &str) -> Result<TimeScale> {
    TimeScale::parse(text).map_err(|error| match error {
        Error::TypeLayout { message, .. } => Error::Unsupported {
            span: source_span(source_name, span),
            message,
        },
        other => other,
    })
}

/// Accept a string literal (`"1ns"`) or a bare unit ident (`ns`).
fn parse_meta_text(input: syn::parse::ParseStream<'_>) -> syn::Result<String> {
    let expr: syn::Expr = input.parse()?;
    match &expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: Lit::Str(text),
            ..
        }) => Ok(text.value()),
        syn::Expr::Path(path) if path.path.segments.len() == 1 => {
            Ok(path.path.segments[0].ident.to_string())
        }
        other => Err(syn::Error::new(
            other.span(),
            "expected a time value such as \"1ns\" or ps",
        )),
    }
}

fn source_span(source_name: &str, span: Span) -> SourceSpan {
    SourceSpan::from_proc_macro2(source_name, span)
        .unwrap_or_else(|_| SourceSpan::new(source_name, 1, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_scale_accepts_verilog_values() {
        assert_eq!(TimeScale::parse("1ns").unwrap().femtoseconds(), 1_000_000);
        assert_eq!(TimeScale::parse("ns").unwrap().as_verilog(), "1ns");
        assert_eq!(TimeScale::parse(" 100ps ").unwrap().as_verilog(), "100ps");
        assert_eq!(
            TimeScale::parse("10us").unwrap().femtoseconds(),
            10_000_000_000
        );
    }

    #[test]
    fn time_scale_rejects_invalid_values() {
        assert!(TimeScale::parse("3ns").is_err());
        assert!(TimeScale::parse("1").is_err());
        assert!(TimeScale::parse("1xs").is_err());
        assert!(TimeScale::parse("-1ns").is_err());
        assert!(TimeScale::parse("0ns").is_err());
    }
}
