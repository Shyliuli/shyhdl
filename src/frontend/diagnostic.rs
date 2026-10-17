//! Frontend diagnostics.
//!
//! Every message the frontend produces is one [`Diagnostic`]: a severity, a
//! stable machine-readable code, a human message and - whenever it is derivable
//! from the parsed source - the location in the source file (REQ-VER-007).
//!
//! Nothing here invents a location. When `proc_macro2` hands back a span with no
//! position (a span built by hand rather than by the parser) the diagnostic is
//! emitted without one and says so, instead of pointing at a plausible-looking
//! line.

use std::fmt;

use crate::metadata::SourceSpan;

/// How severe a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Compilation failed; no Verilog is produced.
    Error,
    /// The design compiles, but something is worth reporting.
    Warning,
}

impl Severity {
    /// Lower-case name used in printed output.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A secondary message attached to a [`Diagnostic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// What to tell the user.
    pub message: String,
    /// Where the note applies, when known.
    pub span: Option<SourceSpan>,
}

/// Stable diagnostic codes.
///
/// Codes are part of the observable surface: they are what a test or a user can
/// match on, so they never change meaning.
pub mod codes {
    /// Source is not valid ShyHDL (Rust syntax) at all.
    pub const PARSE: &str = "SH-F001";
    /// A construct outside the supported subset (REQ-LANG-046 and friends).
    pub const UNSUPPORTED: &str = "SH-F002";
    /// A name does not resolve.
    pub const UNKNOWN_NAME: &str = "SH-F003";
    /// A type is missing, unknown or has an invalid width.
    pub const BAD_TYPE: &str = "SH-F004";
    /// Two values of different widths were combined (REQ-LANG-034).
    pub const WIDTH_MISMATCH: &str = "SH-F005";
    /// An index, slice or literal is out of range (REQ-LANG-034).
    pub const OUT_OF_RANGE: &str = "SH-F006";
    /// The literal's width cannot be inferred (REQ-LANG-030).
    pub const WIDTH_UNKNOWN: &str = "SH-F007";
    /// A field is missing or duplicated, or a marker is wrong.
    pub const DECLARATION: &str = "SH-F008";
    /// The requested top module does not exist or is not concrete.
    pub const TOP: &str = "SH-F009";
    /// An input port (or clock) is written by its own module.
    pub const INPUT_WRITTEN: &str = "SH-F010";
    /// Two processes, or a process and the wiring, drive the same bits
    /// (REQ-LANG-018).
    pub const MULTI_DRIVER: &str = "SH-F011";
    /// A combinational process leaves bits undriven on some path
    /// (REQ-LANG-015, REQ-LANG-022).
    pub const INCOMPLETE_PATH: &str = "SH-F012";
    /// A combinational loop (REQ-LANG-023).
    pub const COMB_LOOP: &str = "SH-F013";
    /// A module instantiates itself, directly or indirectly.
    pub const MODULE_RECURSION: &str = "SH-F014";
    /// A child input port is not connected (REQ-MVP-013).
    pub const UNCONNECTED_INPUT: &str = "SH-F015";
    /// A signal is never driven.
    pub const UNDRIVEN: &str = "SH-F016";
    /// Two generated names would collide.
    pub const NAME_COLLISION: &str = "SH-F017";
    /// The body of `always!` does not have the required edge-chain shape.
    pub const SEQ_SHAPE: &str = "SH-F018";
    /// A loop bound is not a build-time constant (REQ-LANG-046).
    pub const NOT_CONSTANT: &str = "SH-F019";
    /// A hardware struct has no `impl Hardware`, or has two.
    pub const IMPL: &str = "SH-F020";
    /// A struct layout is invalid (recursive, empty, unsupported field).
    pub const LAYOUT: &str = "SH-F021";
    /// A child output port is never read.
    pub const UNREAD_OUTPUT: &str = "SH-F022";
    /// A declared hardware struct is not reachable from the top module.
    pub const UNREACHABLE: &str = "SH-F023";
    /// A capability this build does not have yet.
    pub const NOT_IMPLEMENTED: &str = "SH-F024";
    /// A process that drives nothing (warning; usually a leftover block).
    pub const EMPTY_PROCESS: &str = "SH-F025";
    /// An internal invariant of the compiler was violated (a bug, never a user
    /// error). Reported instead of silently producing incomplete output.
    pub const INTERNAL: &str = "SH-F026";
    /// A source name cannot be used as a Verilog identifier.
    pub const RESERVED_NAME: &str = "SH-F027";
}

/// One message from the frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Error or warning.
    pub severity: Severity,
    /// Stable code, see [`codes`].
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
    /// Source location, when the parsed span carried one.
    pub span: Option<SourceSpan>,
    /// Additional context, for example the "did you mean" hint.
    pub notes: Vec<Note>,
}

impl Diagnostic {
    /// An error.
    pub fn error(
        code: &'static str,
        span: impl Into<Option<SourceSpan>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            span: span.into(),
            notes: Vec::new(),
        }
    }

    /// A warning.
    pub fn warning(
        code: &'static str,
        span: impl Into<Option<SourceSpan>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message: message.into(),
            span: span.into(),
            notes: Vec::new(),
        }
    }

    /// Attach a secondary message.
    pub fn with_note(
        mut self,
        message: impl Into<String>,
        span: impl Into<Option<SourceSpan>>,
    ) -> Self {
        self.notes.push(Note {
            message: message.into(),
            span: span.into(),
        });
        self
    }

    /// True for errors, the only diagnostics that stop compilation.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Render as `<severity>[<code>] <location>: <message>` plus indented notes.
    pub fn to_report_string(&self) -> String {
        use fmt::Write as _;

        let mut out = String::new();
        let _ = write!(out, "{}[{}]: ", self.severity, self.code);
        match &self.span {
            Some(span) => {
                let _ = write!(out, "{span}: ");
            }
            None => out.push_str("(no source location): "),
        }
        out.push_str(&self.message);
        for note in &self.notes {
            out.push_str("\n  note: ");
            if let Some(span) = &note.span {
                let _ = write!(out, "{span}: ");
            }
            out.push_str(&note.message);
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_report_string())
    }
}

/// A collection of diagnostics in the order they were found.
///
/// `Display` prints every diagnostic on its own line, so a CLI can report the
/// whole set with one `eprintln!("{diagnostics}")`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics(Vec<Diagnostic>);

impl Diagnostics {
    /// An empty collection.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Append one diagnostic.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.0.push(diagnostic);
    }

    /// Append an error.
    pub fn error(
        &mut self,
        code: &'static str,
        span: impl Into<Option<SourceSpan>>,
        message: impl Into<String>,
    ) {
        self.push(Diagnostic::error(code, span, message));
    }

    /// Append a warning.
    pub fn warning(
        &mut self,
        code: &'static str,
        span: impl Into<Option<SourceSpan>>,
        message: impl Into<String>,
    ) {
        self.push(Diagnostic::warning(code, span, message));
    }

    /// Append an error if a result is `Err`.
    pub fn extend_from<T>(&mut self, result: Result<T, Diagnostic>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(diagnostic) => {
                self.push(diagnostic);
                None
            }
        }
    }

    /// Move all diagnostics out.
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.0
    }

    /// All diagnostics.
    pub fn as_slice(&self) -> &[Diagnostic] {
        &self.0
    }

    /// Iterate over errors.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter().filter(|d| d.is_error())
    }

    /// Iterate over warnings.
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter().filter(|d| !d.is_error())
    }

    /// True when at least one error was recorded.
    pub fn has_errors(&self) -> bool {
        self.0.iter().any(Diagnostic::is_error)
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// First error, if any.
    pub fn first_error(&self) -> Option<&Diagnostic> {
        self.errors().next()
    }

    /// Consume the collection and return the first error.
    ///
    /// Only meaningful after [`Diagnostics::has_errors`]; when the collection
    /// holds only warnings the first warning is returned rather than a made-up
    /// error.
    pub fn into_first_error(self) -> Diagnostic {
        let mut first_warning = None;
        for diagnostic in self.0 {
            if diagnostic.is_error() {
                return diagnostic;
            }
            if first_warning.is_none() {
                first_warning = Some(diagnostic);
            }
        }
        first_warning.unwrap_or_else(|| {
            Diagnostic::error(
                codes::NOT_IMPLEMENTED,
                None,
                "compilation failed without a recorded diagnostic",
            )
        })
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            f.write_str(&diagnostic.to_report_string())?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}

impl From<Diagnostic> for Diagnostics {
    fn from(diagnostic: Diagnostic) -> Self {
        Self(vec![diagnostic])
    }
}

/// Convert a parsed span into a source location in `file`.
///
/// `None` when the span carries no position, which is reported by the caller
/// instead of being replaced with a fabricated line number.
pub(crate) fn span_in(file: &str, span: proc_macro2::Span) -> Option<SourceSpan> {
    SourceSpan::from_proc_macro2(file, span).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_includes_location_and_notes() {
        let diagnostic = Diagnostic::error(
            codes::WIDTH_MISMATCH,
            SourceSpan::new("m.shy.rs", 4, 9),
            "width mismatch",
        )
        .with_note("target `c` is 8 bits", None);
        let report = diagnostic.to_report_string();
        assert!(
            report.starts_with("error[SH-F005]: m.shy.rs:4:9: width mismatch"),
            "{report}"
        );
        assert!(report.contains("note: target `c` is 8 bits"), "{report}");
    }

    #[test]
    fn missing_location_is_stated_not_invented() {
        let report = Diagnostic::error(codes::PARSE, None, "boom").to_report_string();
        assert!(report.contains("(no source location)"), "{report}");
    }

    #[test]
    fn diagnostics_separate_errors_from_warnings() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.warning(codes::UNREACHABLE, None, "unused module");
        diagnostics.error(codes::PARSE, None, "bad syntax");
        assert!(diagnostics.has_errors());
        assert_eq!(diagnostics.errors().count(), 1);
        assert_eq!(diagnostics.warnings().count(), 1);
        assert_eq!(diagnostics.to_string().lines().count(), 2);
    }
}
