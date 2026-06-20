//! Mizu primary orchestrator binary.
//!
//! Orchestrates the comment-stripping, import-resolution, logic compilation,
//! style parsing, arena-based DOM building, Taffy layout binding, layout
//! geometry calculation, and terminal reporting.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

use std::fs;
use std::path::Path;

use mizu::core::errors::MizuError;
use mizu::core::types::StringInterner;
use mizu::parser::logic::parse_computed;
use mizu::parser::{parse_layout, parse_logic, parse_style, parse_urls, split_source};
use mizu::render::run_window_loop;
use tracing_subscriber::EnvFilter;

/// Orchestrates the compiler phases and coordinates layout engine binding.
fn run() -> Result<(), MizuError> {
    let args: Vec<String> = std::env::args().collect();

    // Parse flags before the file argument.
    #[cfg(feature = "insecure-dev")]
    let allow_insecure = args.iter().any(|a| a == "--allow-insecure");
    let file_args: Vec<&String> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();

    if file_args.is_empty() {
        #[cfg(feature = "insecure-dev")]
        tracing::error!("usage: mizu [--allow-insecure] <file.mizu>");
        #[cfg(not(feature = "insecure-dev"))]
        tracing::error!("usage: mizu <file.mizu>");
        std::process::exit(1);
    }

    #[cfg(feature = "insecure-dev")]
    if allow_insecure {
        tracing::warn!(
            "--allow-insecure: TLS bypass active, restricted to local hosts — for development only"
        );
    }

    let file_path = file_args[0];
    let path = Path::new(file_path);

    let current_dir = path.parent().unwrap_or(Path::new("."));
    let current_dir = if current_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        current_dir
    };

    // Construct absolute canonical URI for the loaded file
    let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut path_str = canonical_path.to_string_lossy().into_owned();
    if path_str.starts_with(r"\\?\") {
        path_str = path_str[4..].to_string();
    }
    let window_url = format!("file:///{}", path_str.replace('\\', "/"));

    let bytes = fs::read(path)?;
    let source = String::from_utf8_lossy(&bytes).to_string();

    // Phase 2: Macro splitter
    let parsed = split_source(&source, current_dir)?;

    // Create shared interner for logic and layout
    let mut interner = StringInterner::new();

    // Phase 2b: Parse URL registry (compile-time endpoint aliases)
    let url_registry = if !parsed.urls_block.trim().is_empty() {
        parse_urls(&parsed.urls_block, &mut interner)?
    } else {
        rustc_hash::FxHashMap::default()
    };

    // Phase 4: Compile logic
    let logic_fns = parse_logic(&parsed.logic_block, &mut interner)?;
    let computed_bindings = parse_computed(&parsed.logic_block, &mut interner)?;

    // Phase 3: Parse styles
    let style_rules = parse_style(&parsed.style_block)?;

    // Phase 5: Assemble arena DOM
    let dom_tree = parse_layout(&parsed.layout_block, &mut interner)?;

    // Phase 8: Start native window and event loop
    run_window_loop(
        dom_tree,
        style_rules,
        logic_fns,
        interner,
        url_registry,
        window_url,
        #[cfg(feature = "insecure-dev")]
        allow_insecure,
        computed_bindings,
    )?;

    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    if let Err(e) = run() {
        tracing::error!("compiler error: {e}");
        std::process::exit(1);
    }
}
