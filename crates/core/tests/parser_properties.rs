//! Property tests for the parsing surface.
//!
//! These exist because the parsers are the part of this crate that Kani cannot
//! reach and that matters most. Every `.mizu` byte a remote document supplies
//! flows through them before any policy runs, so a panic here is a
//! denial-of-service on the whole browser, not a failed parse.
//!
//! Kani is the wrong tool for them twice over. The layout/logic parsers build
//! the self-referential `Value`/`Expr` types that CBMC does not scale to (see
//! the note at the top of `parser::logic::eval`), and `MizuUri::parse` reaches
//! `url`/`idna`, which ICEs Kani's codegen for the entire crate the moment it
//! becomes reachable from a harness (see the note at the end of `core::uri`).
//! Property testing runs natively and has neither limit, so the two techniques
//! divide the work rather than compete: Kani proves the small pure
//! classifiers exhaustively, proptest hammers the parsers.
//!
//! The contract asserted here is deliberately weak, and that is the point: a
//! parser may reject anything it likes, but it must **never panic**, never
//! overflow the stack, and never hang. Rejection is a correct answer for
//! malformed input; a crash is not, and neither is silently accepting.

use mizu_core::core::types::StringInterner;
use proptest::prelude::*;

/// Source text built from the fragments a `.mizu` document is made of.
///
/// Uniformly random strings would spend nearly every case being rejected at
/// the first byte, exercising the error path and nothing else. Assembling
/// lines from real block keywords, indentation, quotes and delimiters gets the
/// generator past the outer structure and into the code that actually walks a
/// document, which is where a panic would live.
fn source_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("layout".to_string()),
        Just("style".to_string()),
        Just("logic".to_string()),
        Just("urls".to_string()),
        Just("doc".to_string()),
        Just("box".to_string()),
        Just("text".to_string()),
        Just("button".to_string()),
        Just("each item in list".to_string()),
        Just("click -> navigate(\"/x\")".to_string()),
        Just(".card".to_string()),
        Just("width 100".to_string()),
        Just("color #ff0000".to_string()),
        Just("fn main()".to_string()),
        Just("x = 1 + 2".to_string()),
        Just("if x > 1".to_string()),
        Just("\"unterminated".to_string()),
        Just("    ".to_string()),
        Just("\t".to_string()),
        Just("{".to_string()),
        Just("}".to_string()),
        Just("(".to_string()),
        Just(")".to_string()),
        Just("\"".to_string()),
        Just("\\".to_string()),
        Just(";;".to_string()),
        Just("->".to_string()),
        Just("?".to_string()),
        Just(":".to_string()),
        Just("é".to_string()),
        Just("中".to_string()),
        Just("\u{202e}".to_string()),
        "[a-z0-9_.]{0,8}",
    ]
}

fn source_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(source_fragment(), 0..24).prop_map(|parts| {
        // Join with a mix of newlines and spaces so both line-oriented and
        // token-oriented paths see structure rather than one long line.
        parts
            .chunks(3)
            .map(|c| c.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

proptest! {
    // Parsers are line/character walkers, so a few hundred varied documents
    // find far more than the default 256 uniform ones while keeping the suite
    // fast enough to stay in the normal `cargo test` run.
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// The style parser accepts or rejects, but never panics.
    #[test]
    fn parse_style_never_panics(src in source_text()) {
        let _ = mizu_core::parser::style::parse_style(&src);
    }

    /// The layout parser accepts or rejects, but never panics.
    ///
    /// This one also walks an indentation stack and builds a DOM, so it is the
    /// most likely of the three to be tripped by pathological nesting.
    #[test]
    fn parse_layout_never_panics(src in source_text()) {
        let mut interner = StringInterner::new();
        let _ = mizu_core::parser::layout::parse_layout(&src, &mut interner);
    }

    /// The logic parser accepts or rejects, but never panics.
    #[test]
    fn parse_logic_never_panics(src in source_text()) {
        let mut interner = StringInterner::new();
        let _ = mizu_core::parser::logic::parse_logic(&src, &mut interner);
    }

    /// The block splitter runs before every other parser and is the first
    /// thing untrusted bytes touch.
    #[test]
    fn split_source_never_panics(src in source_text()) {
        let _ = mizu_core::parser::splitter::split_source(&src, std::path::Path::new("."));
    }

    /// `MizuUri::parse` is the origin authority: everything downstream trusts
    /// the domain it hands back, so it must survive arbitrary input.
    #[test]
    fn mizu_uri_parse_never_panics(s in "\\PC{0,40}") {
        let _ = mizu_core::core::uri::MizuUri::parse(&s);
        let _ = mizu_core::core::uri::MizuUri::parse(&format!("mizu://{s}"));
    }

    /// A parsed URI must never carry a control character into the rest of the
    /// system, whatever the input looked like.
    ///
    /// The WHATWG parser strips tab/CR/LF and percent-encodes other C0 bytes
    /// rather than rejecting them, so "the parser handles it" is not the same
    /// as "the output is clean" — this pins the output, not the intent.
    #[test]
    fn parsed_uri_components_are_control_free(s in "\\PC{0,40}") {
        if let Ok(uri) = mizu_core::core::uri::MizuUri::parse(&format!("mizu://{s}")) {
            prop_assert!(
                !uri.domain.bytes().any(|b| b < 0x20 || b == 0x7f),
                "parsed domain {:?} carries a control character",
                uri.domain
            );
            prop_assert!(
                !uri.path.bytes().any(|b| b < 0x20 || b == 0x7f),
                "parsed path {:?} carries a control character",
                uri.path
            );
        }
    }
}
