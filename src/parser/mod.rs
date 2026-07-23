//! # `parser` — Mizu Preprocessing, Style Parsing & Logic Compilation
//!
//! This module implements the first three stages of the Mizu compilation pipeline:
//!
//! * **[`splitter`]** (Phase 2) — comment-stripping, import resolution, and
//!   macro-block splitting of a raw `.mizu` source file into three isolated
//!   text buffers (`logic`, `style`, `layout`).
//! * **[`style`]** (Phase 3) — structured style-sheet parser: tokenises the
//!   `style_block`, validates all 11 core Mizu properties, parses native hex
//!   colours, and produces a `HashMap<String, StyleRules>` ready for Taffy.
//! * **[`logic`]** (Phase 4) — logic block parser: tokenises function
//!   definitions, builds a typed AST via a Pratt parser, runs a DAG
//!   cycle-detection check (Kahn's algorithm) to enforce Turing-incompleteness,
//!   and exposes an expression evaluator.
//!
//! ## Pipeline Position
//!
//! ```text
//! .mizu source file
//!        │
//!        ▼
//! ┌─────────────────────────┐
//! │  parser::splitter       │  Phase 2
//! └────────────┬────────────┘
//!              │  ParsedSource
//!              ▼
//! ┌─────────────────────────┐
//! │  parser::style          │  Phase 3
//! └────────────┬────────────┘
//!              │  HashMap<String, StyleRules>
//!              ▼
//! ┌─────────────────────────┐
//! │  parser::logic          │  Phase 4
//! └────────────┬────────────┘
//!              │  HashMap<String, MizuFunction>
//!              ▼
//!       (Phase 5+) layout tree / renderer
//! ```
//!
//! ## Public Surface
//!
//! * [`splitter::ParsedSource`] / [`splitter::split_source`]
//! * [`style::StyleRules`] / [`style::MizuColor`] / [`style::MizuDimension`] / [`style::parse_style`]
//! * [`logic::MizuFunction`] / [`logic::Expr`] / [`logic::BinOp`] / [`logic::ValueType`] / [`logic::Action`]
//! * [`logic::parse_logic`] / [`logic::evaluate`] / [`logic::parse_action`] / [`logic::execute_action`]

#![forbid(unsafe_code)]

pub mod flow;
pub mod layout;
pub mod typecheck;
pub mod logic;
/// Thread logic worker for isolated logic and event execution
pub mod logic_worker;
pub mod splitter;
pub mod style;
/// Compile-time URL registry parser
pub mod urls;

pub use crate::core::types::Symbol;
pub use layout::{EventBlock, MizuNode, Primitive, parse_layout, parse_layout_with_urls};
pub use logic::{
    Action, BinOp, Expr, MizuFunction, NetworkMethod, RootTimer, TimerInterval, ValueType,
    evaluate, execute_action, parse_action, parse_action_with_urls, parse_logic, parse_root_timers,
};
pub use logic_worker::LogicWorker;
pub use splitter::{Origin, ParsedSource, split_source, split_source_with_origin};
pub use style::{
    MizuColor, MizuDimension, MizuFontFamily, MizuFontStyle, MizuOverflow, MizuTextAlign,
    StyleRules, StyleVariant, VariantCondition, parse_style, parse_style_with_variants,
};
pub use urls::{EndpointKind, UrlEndpoint, UrlRegistry, parse_urls};
