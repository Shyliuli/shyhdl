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

    /// A construct outside the supported subset, located in source
    /// (REQ-LANG-046, REQ-DBG-059).
    #[error("unsupported construct at {span}: {message}")]
    Unsupported {
        /// Location of the offending construct.
        span: SourceSpan,
        /// What is unsupported and, when known, why.
        message: String,
    },

    /// A type has no valid fixed bit width, or its width does not fit
    /// (REQ-LANG-034). See `ValueType::bit_width`.
    #[error("type layout error: {message}")]
    TypeLayout {
        /// What went wrong.
        message: String,
        /// Source location, when the caller knows one.
        span: Option<SourceSpan>,
    },

    /// Loaded metadata is internally inconsistent and cannot drive the debugger,
    /// for example a duplicate variable id. See `DesignMetadata::validate`.
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

    /// The recording and the symbols come from different builds, so replay is
    /// refused (REQ-DBG-010).
    #[error("build id mismatch: symbols are from {expected}, recording is from {found}")]
    BuildIdMismatch {
        /// Build id of the symbols being used.
        expected: String,
        /// Build id recorded in the artifact.
        found: String,
    },

    /// The artifact was written by a build with a different metadata schema.
    #[error(
        "metadata schema version {found} is not supported, this build reads version {expected}"
    )]
    SchemaVersion {
        /// Version this build understands.
        expected: u32,
        /// Version found in the artifact.
        found: u32,
    },

    /// A source location was requested from a span that has none.
    #[error("no source location available for {file}")]
    MissingSourceLocation {
        /// File the span was expected to point into.
        file: String,
    },

    /// Functionality that is planned but absent from this build. Reported as an
    /// error so an unimplemented path can never look like a successful run.
    #[error("{feature} is not implemented in this build")]
    NotImplemented {
        /// What is missing, named after the milestone that owns it.
        feature: &'static str,
    },
}
