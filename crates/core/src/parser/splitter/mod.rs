//! # `splitter` — Line-by-Line Macro-Block Preprocessor
//!
//! This module is the **first formal pass** of the Mizu compilation pipeline.
//! It takes raw `.mizu` source text and produces a [`ParsedSource`] struct
//! containing three isolated, comment-free, import-resolved text buffers —
//! one per macro-block — ready for subsequent parsing phases.
//!
//! ## Responsibilities (in order of execution)
//!
//! 1. **Comment stripping** — removes everything after the first `;;` token
//!    that is *not* inside a string literal, on every line.
//! 2. **Import resolution** — resolves `import "file"` (or its synonym
//!    `include "file"`) directives at indentation 0 by reading the target file
//!    from the local filesystem, verifying it carries no nested imports, and
//!    splicing its content into the appropriate block buffer (`logic` for
//!    `.mlg`, `style` for `.mss`).  Import resolution is governed by an
//!    [`Origin`] trust boundary: documents delivered over the network may not
//!    use imports at all, and local imports are confined to the document's own
//!    directory (no path traversal outside it).
//! 3. **Section dispatch** — routes indented content lines into the correct
//!    block buffer (`logic`, `style`, `layout`, or `urls`) based on the most
//!    recently seen zero-indented section keyword.
//! 4. **Blank-line padding** — for every dispatched content line, empty
//!    sentinel lines are appended to all *inactive* buffers, preserving
//!    file-offset alignment so that downstream parsers can produce accurate
//!    line numbers in error messages.
//! 5. **Validation** — rejects any zero-indented token that is not a section
//!    keyword or a valid import directive, as well as any indented content
//!    encountered before the first section keyword.
//!
//! ## What This Module Does NOT Do
//!
//! * It does **not** parse expressions, property values, or structural
//!   primitives — that is the responsibility of Phase 3+ parsers.
//! * It does **not** validate the *content* of the injected blocks (e.g.,
//!   whether a `.mlg` file contains syntactically valid Mizu functions).
//! * It does **not** interpret multi-line `"""` blocks; the layout parser owns
//!   all block-level structure.
//!
//! Split by concern: [`types`] (`ParsedSource`/`Origin`/internal dispatch
//! enums), [`helpers`] (leaf helpers: comment stripping, import-path
//! parsing), and [`split`] (`split_source`/`split_source_with_origin`/
//! `process_import`).

#![forbid(unsafe_code)]

mod helpers;
mod split;
#[cfg(test)]
mod tests;
mod types;

pub use split::{split_source, split_source_with_origin};
pub use types::{Origin, ParsedSource};
