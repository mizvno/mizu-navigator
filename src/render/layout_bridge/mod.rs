//! DOM → Taffy layout tree construction, including `Each`-block expansion.
//!
//! Split by concern: [`expansion`] (`Each`-node Taffy expansion, virtualization
//! windowing), [`helpers`] (small Taffy-tree helpers shared by expansion and
//! the build pass), [`style`] (`translate_style`, Mizu → Taffy style
//! translation), and [`build`] (`TaffyBuildContext`/`build_taffy_tree`, the
//! recursive DOM→Taffy builder).

#![forbid(unsafe_code)]

mod build;
mod expansion;
mod helpers;
mod style;
#[cfg(test)]
mod tests;

pub use build::{TaffyBuildContext, build_taffy_tree};
pub use expansion::{
    DEFAULT_ROW_HEIGHT_ESTIMATE_PX, EachExpansion, EachGroupEntries, EachIterationOverrides,
    MAX_SYNTHETIC_LAYOUT_NODES, VIRTUALIZATION_BUFFER_ROWS, expand_each_nodes,
};
pub use style::translate_style;
