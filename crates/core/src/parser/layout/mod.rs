//! # `layout` — Mizu Layout Parser & Arena-based DOM Constructor
//!
//! This module implements Phase 5 of the Mizu compilation pipeline. It takes
//! the raw `layout_block` produced by [`super::splitter`], tokenises and parses
//! the structural hierarchy based on indentation, and constructs a tree-like
//! Document Object Model (DOM) using the [`ego-tree`] crate.
//!
//! ## Node text content
//!
//! Inline text (e.g. `text "Hello"`) is represented as a child `Primitive::Text`
//! node with the string stored in `attributes["content"]`.  The `inline_text`
//! field has been removed; read `node.attributes.get("content")` instead.
//!
//! Split by concern: [`types`] (`Primitive`, `MizuNode`, `EventBlock`,
//! `ConditionalClass`), [`helpers`] (token/string-shape leaf helpers),
//! and [`parse`] (attribute/event parsing plus the `parse_layout`/
//! `parse_layout_with_urls` tree builder).

#![forbid(unsafe_code)]

mod helpers;
mod parse;
#[cfg(test)]
mod tests;
mod types;

pub use parse::*;
pub use types::*;
