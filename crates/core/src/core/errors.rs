//! # `errors` — Unified Error Taxonomy for the Mizu Compiler & Runtime
//!
//! This module is the single source of truth for every failure mode that the
//! Mizu toolchain can encounter.  All subsystems — the parser, the type-checker,
//! the evaluator, and I/O adapters — surface their failures as variants of
//! [`MizuError`], ensuring that call-sites never need to juggle heterogeneous
//! error types or reach for `.unwrap()`.
//!
//! ## Design Rationale
//!
//! * **`thiserror`** is used instead of `anyhow` because Mizu is a *library-
//!   grade* crate (compiler + runtime).  Library crates must expose typed errors
//!   so that embedders can pattern-match on specific failure modes.  `anyhow` is
//!   appropriate only for application-level binaries.
//! * Each variant carries enough structured context (expected/found types, the
//!   offending identifier name, the raw I/O error) to produce actionable
//!   diagnostics without any string scanning by the caller.
//! * The `#[from]` attribute on `IoError` auto-generates a `From<std::io::Error>`
//!   impl, bridging the filesystem world into the Mizu error hierarchy
//!   transparently and without boilerplate.

#![forbid(unsafe_code)]

use thiserror::Error;

/// The canonical error type for every Mizu subsystem.
///
/// Consumers should match on concrete variants to distinguish recoverable
/// conditions (e.g., a missing variable that might be user input) from hard
/// failures (e.g., a type mismatch during compilation).
///
/// # Examples
///
/// ```
/// use mizu_core::core::errors::MizuError;
///
/// fn lookup(name: &str) -> Result<(), MizuError> {
///     Err(MizuError::VariableNotFound(name.to_owned()))
/// }
///
/// assert!(matches!(lookup("x"), Err(MizuError::VariableNotFound(_))));
/// ```
#[derive(Debug, Error)]
pub enum MizuError {
    /// A syntactic or structural error encountered while parsing a `.mizu`
    /// source file or an `.mss` style sheet.
    ///
    /// The inner [`String`] carries a human-readable description of *what* was
    /// wrong (e.g., `"unexpected token '>' at line 4, column 12"`).
    #[error("parse error: {0}")]
    ParseError(String),

    /// A semantic type mismatch detected during type-checking or evaluation.
    ///
    /// Both `expected` and `found` are type-name strings (e.g., `"num"`,
    /// `"bool"`, `"list<num>"`). `found` is sourced from [`crate::parser::logic::type_name`] and
    /// is a static string literal, while `expected` can be a dynamically formatted type like `"list<num>"`.
    #[error("type error: expected `{expected}`, found `{found}`")]
    TypeError {
        /// The Mizu type name that was required in this position.
        expected: String,
        /// The Mizu type name actually produced by evaluation.
        found: &'static str,
    },

    /// A static load-time type error detected during whole-document type checking.
    ///
    /// The inner [`String`] contains a human-readable description of the error.
    #[error("static type error: {0}")]
    StaticTypeError(String),

    /// A variable was referenced in an expression but was never bound in the
    /// enclosing scope.
    ///
    /// The inner [`String`] is the exact identifier as it appeared in the
    /// source, preserving case for faithful error reporting.
    #[error("variable not found: `{0}`")]
    VariableNotFound(String),

    /// A variable was referenced during interpolation but was never bound in the store.
    #[error("binding not found: `{0}`")]
    BindingNotFound(String),

    /// A filesystem or I/O operation failed (e.g., reading a `.mizu` source
    /// file, writing a compiled artefact, loading a font).
    ///
    /// The `#[from]` attribute generates a blanket `From<std::io::Error>` impl,
    /// so callers can use `?` on any `std::io` operation and have the error
    /// automatically converted into this variant.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// An evaluation-time error denoting a division or modulo by zero.
    #[error("division by zero")]
    DivisionByZero,

    /// A network-related error (e.g. URI parsing, QUIC, Token Scope).
    #[error("network error: {0}")]
    Network(String),

    /// A DNS resolution error propagated from the DoT resolver.
    ///
    /// Unlike the generic [`Network`](Self::Network) variant, this carries the
    /// strongly-typed [`hickory_resolver::error::ResolveError`] so that callers
    /// can match on concrete [`hickory_resolver::error::ResolveErrorKind`] variants
    /// instead of scraping formatted error strings.  The `#[from]` attribute
    /// auto-generates `From<ResolveError> for MizuError`, enabling `?` propagation
    /// in async DNS call sites without losing type information.
    #[cfg(not(kani))]
    #[error("DNS error: {0}")]
    DnsError(#[from] hickory_resolver::error::ResolveError),

    /// A runtime execution error (e.g. an invalid action evaluation).
    #[error("execution error: {0}")]
    ExecutionError(String),

    /// Execution interrupted because the maximum allowed time budget was exceeded.
    #[error("timeout: execution budget exceeded")]
    Timeout,

    /// A security policy violation: quota exceeded, gesture required, or
    /// other capability-gate enforcement.
    #[error("security violation: {0}")]
    SecurityViolation(String),

    /// Multiple parse errors collected in a single pass (style parser).
    ///
    /// The inner [`Vec`] contains every individual error found; the caller can
    /// inspect them all rather than being forced to fix-and-reparse repeatedly.
    #[error("{} parse error(s):\n{}", .0.len(), .0.iter().enumerate().map(|(i, e)| format!("  {}. {e}", i + 1)).collect::<Vec<_>>().join("\n"))]
    MultipleErrors(Vec<MizuError>),
}

#[cfg(test)]
mod tests {
    use super::MizuError;
    use std::io;

    #[test]
    fn parse_error_stores_message() {
        let msg = "unexpected token '}' at line 3";
        let err = MizuError::ParseError(msg.to_owned());
        assert_eq!(err.to_string(), format!("parse error: {msg}"));
    }

    #[test]
    fn parse_error_is_debug_printable() {
        let err = MizuError::ParseError("oops".to_owned());
        let _ = format!("{err:?}");
    }

    #[test]
    fn type_error_formats_expected_and_found() {
        let err = MizuError::TypeError {
            expected: "num".to_string(),
            found: "bool",
        };
        assert_eq!(err.to_string(), "type error: expected `num`, found `bool`");
    }

    #[test]
    fn type_error_fields_are_accessible() {
        let err = MizuError::TypeError {
            expected: "string".to_string(),
            found: "list",
        };
        if let MizuError::TypeError { expected, found } = err {
            assert_eq!(expected, "string");
            assert_eq!(found, "list");
        } else {
            panic!("unexpected variant");
        }
    }


    #[test]
    fn variable_not_found_stores_identifier() {
        let err = MizuError::VariableNotFound("total_price".to_owned());
        assert_eq!(err.to_string(), "variable not found: `total_price`");
    }


    #[test]
    fn io_error_converts_via_from() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file missing");
        let mizu_err = MizuError::from(io_err);
        assert!(mizu_err.to_string().contains("I/O error"));
    }

    #[test]
    fn security_violation_stores_message() {
        let msg = "storage quota exceeded: 600000 / 524288 bytes";
        let err = MizuError::SecurityViolation(msg.to_owned());
        assert_eq!(err.to_string(), format!("security violation: {msg}"));
        assert!(matches!(err, MizuError::SecurityViolation(_)));
    }

    #[test]
    fn io_error_question_mark_operator_compiles() {
        // This function simulates a call-site using `?` to propagate io::Error.
        fn read_source() -> Result<(), MizuError> {
            let _bytes = std::fs::read("__mizu_nonexistent_fixture__.mizu")?;
            Ok(())
        }
        // The file does not exist, so this must return Err(MizuError::IoError(…)).
        let result = read_source();
        assert!(matches!(result, Err(MizuError::IoError(_))));
    }
}
