//! [`ParsedSource`] (the output of a successful split), plus the internal
//! `ActiveBlock`/`ImportTarget` dispatch enums and the [`Origin`] trust
//! boundary that gates import resolution.

/// The output of a successful [`split_source`](super::split_source) call.
///
/// Each field holds the raw, comment-stripped, import-resolved content of the
/// corresponding macro-block, with the leading section keyword line itself
/// omitted (i.e., the first line is the first *body* line of the block).
///
/// ## Empty blocks
///
/// If a source file omits a macro-block entirely, the corresponding field
/// contains only blank padding lines (one per content line in other blocks).
/// Use `.trim().is_empty()` rather than `.is_empty()` to test for absence.
///
/// ## Indentation preservation
///
/// Every content line retains its original indentation relative to the source
/// file.  Downstream parsers are responsible for interpreting that indentation
/// according to the Mizu grammar for each block type.
///
/// ## Line-offset alignment
///
/// For every content line dispatched to an active block, a blank sentinel line
/// (`""`) is appended to each inactive block buffer.  This guarantees that
/// line *N* of any buffer corresponds to line *N* of the virtual interleaved
/// stream, enabling accurate line numbers in downstream error messages.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSource {
    /// Comment-stripped, import-resolved content of the `logic` macro-block.
    pub logic_block: String,
    /// Comment-stripped, import-resolved content of the `style` macro-block.
    pub style_block: String,
    /// Comment-stripped content of the `layout` macro-block.
    pub layout_block: String,
    /// Comment-stripped content of the `urls` macro-block (URL registry).
    pub urls_block: String,
}

/// Tracks which macro-block is currently being accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActiveBlock {
    /// No section keyword has been seen yet.
    None,
    Logic,
    Style,
    Layout,
    /// URL registry block — declares `api` and `media` endpoint aliases.
    Urls,
}

/// Identifies which buffer an imported file should be spliced into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportTarget {
    Logic,
    Style,
}

/// Trust boundary that governs how `import`/`include` directives are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Document loaded from the local filesystem.  Imports are resolved
    /// relative to — and confined within — the document's own directory.
    LocalFile,
    /// Document delivered over the network (e.g. via `mizu://`).  Imports are
    /// forbidden entirely and **no** filesystem access is performed, so a
    /// hostile remote document cannot read arbitrary local files via
    /// `import "../../secret"`.
    Network,
}
