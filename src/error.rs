//! The crate error type: every variant carries the path, source location or
//! underlying cause needed to diagnose the failure.

use std::path::PathBuf;

use crate::metadata::SourceSpan;

/// Result alias for fallible crate operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Failure of a ShyHDL operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem failure, always carrying the path being touched.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// Path that was being read or written.
        path: PathBuf,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },

    /// Source text is not valid ShyHDL syntax.
    #[error("failed to parse {path}: {source}")]
    Parse {
        /// Path of the source file.
        path: PathBuf,
        /// Syntax error, which itself points at a line and column.
        #[source]
        source: syn::Error,
    },

    /// A construct outside the supported subset, located in source.
    #[error("unsupported construct at {span}: {message}")]
    Unsupported {
        /// Location of the offending construct.
        span: SourceSpan,
        /// What is unsupported and, when known, why.
        message: String,
    },

    /// A type has no valid fixed bit width, or its width does not fit. See
    /// `ValueType::bit_width`.
    #[error("type layout error: {message}")]
    TypeLayout {
        /// What went wrong.
        message: String,
        /// Source location, when the caller knows one.
        span: Option<SourceSpan>,
    },

    /// Loaded metadata is inconsistent or does not describe the recording.
    #[error("invalid design metadata: {message}")]
    InvalidMetadata {
        /// What is inconsistent.
        message: String,
    },

    /// Design metadata could not be encoded or decoded.
    #[error("failed to {action} design metadata: {source}")]
    Json {
        /// `read` or `write`.
        action: &'static str,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// A source location was requested from a span that has none.
    #[error("no source location available for {file}")]
    MissingSourceLocation {
        /// File the span was expected to point into.
        file: String,
    },

    /// Something the caller asked for is not available in this run.
    #[error("{message}")]
    Unavailable {
        /// What is missing.
        message: &'static str,
    },

    /// The optional NVBoard backend could not build, start or keep running. The
    /// message carries the tool output that explains why.
    #[error("nvboard: {message}")]
    Board {
        /// What failed.
        message: String,
    },
}
