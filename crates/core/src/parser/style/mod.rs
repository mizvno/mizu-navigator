//! # `style` — Mizu Style Sheet Parser (Phase 3 + Phase 11)
//!
//! This module tokenizes the `style_block` produced by [`super::splitter`]
//! into a typed, validated [`HashMap`] of class-name → [`StyleRules`] pairs,
//! ready to be handed to the Taffy layout engine in Phase 4 and the Vello
//! rendering pipeline in Phase 11.
//!
//! ## Grammar (excerpt from MIZU_GUIDELINES.md § 2.2)
//!
//! ```text
//! .class_name
//!     property value
//!     property value
//! .another_class
//!     property value
//! ```
//!
//! Rules:
//! * The `.class_name` selector sits at the **baseline indentation level**
//!   (the minimum indentation found in the block — set dynamically from the
//!   first non-empty line).
//! * Properties are on lines indented *deeper* than baseline.
//! * Syntax is `key value` — **no colons, no semicolons**.
//! * Hex colours start with `#` and are **unquoted**.
//!
//! ## Type Mapping
//!
//! | Mizu keyword | Mizu value form    | Rust representation       |
//! |--------------|--------------------|---------------------------|
//! | `width`, `height`, `padding`, `margin`, `gap` | `100` or `50%` | [`MizuDimension`] |
//! | `direction`  | `row` \| `column`  | [`taffy::style::FlexDirection`] |
//! | `justify`    | `center` etc.      | [`taffy::style::JustifyContent`] |
//! | `align`      | `stretch` etc.     | [`taffy::style::AlignItems`] |
//! | `background`, `color` | `#rrggbb` | [`MizuColor`] |
//! | `font-size`, `border-radius` | `14` | `f32` |
//! | `overflow`   | `visible` \| `hidden` \| `scroll` | [`MizuOverflow`] |
//! | `z-index`    | `-5`, `0`, `10`    | `i32` |
//!
//! ## Pipeline Position
//!
//! ```text
//! style_block: String   (from parser::splitter)
//!        │
//!        ▼
//! ┌─────────────────────────────┐
//! │  parser::style::parse_style │  ← this module
//! │  (Phase 3)                  │
//! │  • indentation detection    │
//! │  • selector / property scan │
//! │  • hex color parsing        │
//! │  • Taffy type mapping       │
//! └─────────────┬───────────────┘
//!               │  HashMap<String, StyleRules>
//!               ▼
//!       (Phase 4) Taffy layout tree construction
//! ```
//!
//! Split by concern: [`types`] (`MizuColor`..`StyleRules`, plus `StyleRules`'
//! merge/variant-application methods), [`parse`] (`parse_style`/
//! `parse_style_with_variants`/`apply_property`), and [`values`] (the
//! token-level parsers `apply_property` dispatches to).

#![forbid(unsafe_code)]

mod parse;
#[cfg(test)]
mod tests;
mod types;
mod values;

pub use parse::*;
pub use types::*;
