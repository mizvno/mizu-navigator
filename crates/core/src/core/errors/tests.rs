//! Tests for the errors module.

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
        expected: Box::new("num".to_string()),
        found: "bool",
    };
    assert_eq!(err.to_string(), "type error: expected `num`, found `bool`");
}

#[test]
fn type_error_fields_are_accessible() {
    let err = MizuError::TypeError {
        expected: Box::new("string".to_string()),
        found: "list",
    };
    if let MizuError::TypeError { expected, found } = err {
        assert_eq!(*expected, "string");
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
