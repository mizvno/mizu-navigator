//! # Executable Reference Test Suite
//!
//! Machine-checks every `.mizu` example in `docs/reference/examples/`.
//! Fixtures whose filenames start with `err_` are expected to fail at parse
//! time; all others are expected to succeed.
//!
//! This test file is the anti-drift mechanism described in the language
//! reference: a spec example that cannot be parsed is caught immediately,
//! preventing the documentation from silently diverging from the implementation.

use std::path::Path;

use mizu::parser::{parse_layout, parse_logic, parse_style, parse_urls, split_source};
use mizu::core::types::StringInterner;

/// Root directory of the fixture files, relative to the crate root (where
/// `cargo test` runs).
const FIXTURES_DIR: &str = "docs/reference/examples";

/// Fully parses all four blocks of a `.mizu` source file.
///
/// Returns `Ok(())` on success or an `Err(String)` describing the first parse
/// failure.
fn parse_document(source: &str, fixture_name: &str) -> Result<(), String> {
    let dir = Path::new(".");
    let parsed = split_source(source, dir)
        .map_err(|e| format!("{fixture_name}: split_source failed: {e}"))?;

    let mut interner = StringInterner::new();

    if !parsed.urls_block.trim().is_empty() {
        parse_urls(&parsed.urls_block, &mut interner)
            .map_err(|e| format!("{fixture_name}: parse_urls failed: {e}"))?;
    }

    if !parsed.logic_block.trim().is_empty() {
        parse_logic(&parsed.logic_block, &mut interner)
            .map_err(|e| format!("{fixture_name}: parse_logic failed: {e}"))?;
    }

    if !parsed.style_block.trim().is_empty() {
        parse_style(&parsed.style_block)
            .map_err(|e| format!("{fixture_name}: parse_style failed: {e}"))?;
    }

    if !parsed.layout_block.trim().is_empty() {
        parse_layout(&parsed.layout_block, &mut interner)
            .map_err(|e| format!("{fixture_name}: parse_layout failed: {e}"))?;
    }

    Ok(())
}

/// Runs the fixture suite: parses every `.mizu` file in FIXTURES_DIR.
///
/// Fixtures starting with `err_` must produce a parse error.
/// All others must succeed.
#[test]
fn reference_examples_are_parseable() {
    let fixtures_path = Path::new(FIXTURES_DIR);
    assert!(
        fixtures_path.exists(),
        "Fixture directory `{FIXTURES_DIR}` not found. Run `cargo test` from the crate root."
    );

    let entries: Vec<_> = std::fs::read_dir(fixtures_path)
        .expect("cannot read fixtures directory")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x == "mizu")
                .unwrap_or(false)
        })
        .collect();

    assert!(
        !entries.is_empty(),
        "No .mizu fixtures found in `{FIXTURES_DIR}`"
    );

    let mut failures: Vec<String> = Vec::new();

    for entry in &entries {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy();
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

        let is_error_fixture = name.starts_with("err_");
        let result = parse_document(&source, &name);

        if is_error_fixture {
            if result.is_ok() {
                failures.push(format!(
                    "EXPECTED FAILURE but got Ok: {name}"
                ));
            }
        } else if let Err(e) = result {
            failures.push(format!("UNEXPECTED FAILURE: {e}"));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} fixture(s) failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    println!(
        "reference_examples: {}/{} fixtures passed ({} ok, {} error-expected)",
        entries.len(),
        entries.len(),
        entries.iter().filter(|e| !e.file_name().to_string_lossy().starts_with("err_")).count(),
        entries.iter().filter(|e| e.file_name().to_string_lossy().starts_with("err_")).count(),
    );
}

// ── Individual fixture smoke tests ────────────────────────────────────────────
// These give targeted names to each fixture so failures are easy to bisect.

macro_rules! fixture_ok {
    ($name:ident, $file:expr) => {
        #[test]
        fn $name() {
            let source = std::fs::read_to_string(concat!("docs/reference/examples/", $file))
                .expect(concat!("cannot read fixture: ", $file));
            parse_document(&source, $file).expect(concat!("fixture should parse: ", $file));
        }
    };
}

macro_rules! fixture_err {
    ($name:ident, $file:expr) => {
        #[test]
        fn $name() {
            let source = std::fs::read_to_string(concat!("docs/reference/examples/", $file))
                .expect(concat!("cannot read fixture: ", $file));
            let result = parse_document(&source, $file);
            assert!(
                result.is_err(),
                "fixture `{}` should produce a parse error, but parsed Ok",
                $file
            );
        }
    };
}

fixture_ok!(fixture_01_minimal,       "01_minimal.mizu");
fixture_ok!(fixture_02_logic_basics,  "02_logic_basics.mizu");
fixture_ok!(fixture_03_counter,       "03_counter.mizu");
fixture_ok!(fixture_04_urls_fetch,    "04_urls_fetch.mizu");
fixture_ok!(fixture_05_comp,          "05_comp.mizu");
fixture_ok!(fixture_06_each,          "06_each.mizu");
fixture_ok!(fixture_07_timer,         "07_timer.mizu");
fixture_ok!(fixture_08_style,         "08_style.mizu");

fixture_err!(fixture_err_recursion,        "err_recursion.mizu");
fixture_err!(fixture_err_nested_each,      "err_nested_each.mizu");
fixture_err!(fixture_err_absolute_img_src, "err_absolute_img_src.mizu");
