//! Row model types: [`Tone`], [`Face`], [`Flex`], [`Seg`], [`RowKind`],
//! [`InspectValue`], [`Row`], [`InspectorSources`], and the top-level
//! [`build_rows`] dispatcher.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use ego_tree::{NodeId as EgoNodeId, Tree};
use rustc_hash::FxHashMap;

use crate::core::types::{Symbol, VariableStore};
use crate::parser::logic::{ComputedBinding, MizuFunction, RootTimer};
use crate::parser::{MizuNode, StyleRules, UrlRegistry};
use crate::render::inspector::log::InspectorLog;
use crate::render::inspector::{
    DETAIL_ROW_HEIGHT, HEADER_ROW_HEIGHT, InspectorState, InspectorTab, ROW_HEIGHT,
};
use crate::render::layout_bridge::EachExpansion;
use crate::render::security::CapabilityPolicy;

use super::rows::{elements_rows, events_rows, logic_rows, network_rows, style_rows};

// ─────────────────────────────────────────────────────────────────────────────
// Row vocabulary
// ─────────────────────────────────────────────────────────────────────────────

/// Semantic colour of a segment.
///
/// Every tone resolves to an entry of the chrome palette — the panel keeps no
/// colours of its own, so it follows light/dark/high-contrast exactly as the
/// tab strip does, and stays inside the palette's audited contrast set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Property names, labels, structural punctuation.
    Key,
    /// The thing the reader actually came for: values, targets, expressions.
    Value,
    /// Supporting detail — dependencies, timestamps, hints.
    Muted,
    /// Live/selected/highlighted, e.g. a just-mutated variable.
    Accent,
    /// Positive outcome (request ok, condition true).
    Good,
    /// Negative outcome (error, blocked, failed condition).
    Bad,
}

/// Type face of a segment.
///
/// Prose and labels are set in the UI face and data in monospace, which is
/// what separates a readable panel from a terminal dump — and monospace also
/// buys the painter an exact O(1) width for ASCII runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    /// UI sans, for labels and prose.
    Ui,
    /// UI sans, semibold — section headers and emphasis.
    UiStrong,
    /// Monospace, for anything that is code, a URL, or a value.
    Mono,
}

/// How a segment behaves when the row is narrower than its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flex {
    /// Never elided; laid out at its natural width. Use for short tags whose
    /// meaning is destroyed by cutting them (`GET`, `BLOCKED`, `12.4s`).
    Fixed,
    /// Absorbs the leftover width and is elided from the tail (`long nam…`).
    Elide,
    /// Absorbs the leftover width and is elided in the middle
    /// (`mizu://host/…/leaf.json`), for URLs and paths where the tail carries
    /// as much meaning as the head.
    ElideMiddle,
    /// Right-aligned against the row's trailing edge, natural width, dropped
    /// entirely when there is not enough room for it *and* the leading
    /// content. Use for metrics: duration, byte count, item counts.
    Trailing,
}

/// One typed run of text within a row.
#[derive(Debug, Clone)]
pub struct Seg {
    /// The full, untruncated text. The painter decides what fits.
    pub text: String,
    /// Semantic colour.
    pub tone: Tone,
    /// Type face.
    pub face: Face,
    /// Behaviour under width pressure.
    pub flex: Flex,
    /// Optional colour chip painted before the text, for style values that
    /// name a colour — far quicker to read than the hex triplet alone.
    pub swatch: Option<(u8, u8, u8, u8)>,
}

impl Seg {
    /// A monospace segment — the default for data.
    pub fn mono(text: impl Into<String>, tone: Tone) -> Self {
        Seg {
            text: text.into(),
            tone,
            face: Face::Mono,
            flex: Flex::Fixed,
            swatch: None,
        }
    }

    /// A UI-face segment, for labels and prose.
    pub fn ui(text: impl Into<String>, tone: Tone) -> Self {
        Seg {
            face: Face::Ui,
            ..Seg::mono(text, tone)
        }
    }

    /// A semibold UI-face segment, for section headers.
    pub fn strong(text: impl Into<String>, tone: Tone) -> Self {
        Seg {
            face: Face::UiStrong,
            ..Seg::mono(text, tone)
        }
    }

    /// Marks this segment as the one that absorbs slack and elides from the
    /// tail.
    pub fn elide(mut self) -> Self {
        self.flex = Flex::Elide;
        self
    }

    /// Marks this segment as absorbing slack and eliding in the middle.
    pub fn middle(mut self) -> Self {
        self.flex = Flex::ElideMiddle;
        self
    }

    /// Right-aligns this segment against the row's trailing edge.
    pub fn trail(mut self) -> Self {
        self.flex = Flex::Trailing;
        self
    }

    /// Attaches a colour chip, painted immediately before the text.
    pub fn swatch(mut self, c: &crate::parser::MizuColor) -> Self {
        self.swatch = Some((c.r, c.g, c.b, c.a));
        self
    }
}

/// Structural role of a row, which fixes its height and its decoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Section header: extra leading, a trailing hairline, never selectable.
    Header,
    /// A primary entry.
    Item,
    /// A continuation line hanging off the item above it.
    Detail,
    /// A "nothing here" placeholder.
    Empty,
}

/// The full, untruncated text behind a row that may be shown elided.
///
/// Attached only when the text is long enough that eliding it is a real risk
/// (see [`worth_inspecting`]) — most rows are short enough that a "show more"
/// affordance would just be noise.
#[derive(Debug, Clone)]
pub struct InspectValue {
    /// Short label shown in the drawer's header, e.g. `"Value"`, `"Target"`.
    pub title: String,
    /// The complete text, exactly as it appears in the document or the log —
    /// never truncated, never re-flowed except by the drawer's own wrapping.
    pub text: String,
}

/// Shortest text length worth offering a "show more" affordance for.
///
/// Below this, a value elides at most a character or two even in a narrow
/// panel, so opening a whole drawer for it would be worse than the elision.
const INSPECTABLE_MIN_CHARS: usize = 28;

/// One displayable row of the active inspector tab.
#[derive(Debug, Clone)]
pub struct Row {
    /// Structural role.
    pub kind: RowKind,
    /// Indentation level (Elements tree depth; small fixed values elsewhere).
    pub indent: u8,
    /// The row's content, in reading order. Trailing segments are laid out
    /// from the right edge regardless of their position in this list.
    pub segs: Vec<Seg>,
    /// DOM node this row refers to (Elements rows), for selection/highlight.
    pub node: Option<EgoNodeId>,
    /// Whether the row has children that can be shown or hidden.
    pub expandable: bool,
    /// Whether those children are currently shown.
    pub expanded: bool,
    /// The row's full value, when it is long enough that the elided segments
    /// painted on screen might not be the whole story.
    pub inspect: Option<InspectValue>,
}

impl Row {
    /// This row's height in logical pixels.
    pub fn height(&self) -> f32 {
        match self.kind {
            RowKind::Header => HEADER_ROW_HEIGHT,
            RowKind::Detail => DETAIL_ROW_HEIGHT,
            RowKind::Item | RowKind::Empty => ROW_HEIGHT,
        }
    }

    /// Whether clicking this row can change the selection.
    pub fn selectable(&self) -> bool {
        self.node.is_some()
    }

    /// The row's text with segments joined by single spaces.
    ///
    /// Used by tests and by anything that needs a plain reading of the row
    /// (the panel itself never renders this — it lays the segments out).
    pub fn plain_text(&self) -> String {
        self.segs
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Attaches a full-value payload, but only when `text` is long enough
    /// that eliding it is a real risk — see [`INSPECTABLE_MIN_CHARS`].
    pub(super) fn maybe_inspect(
        mut self,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let text = text.into();
        if text.chars().count() > INSPECTABLE_MIN_CHARS {
            self.inspect = Some(InspectValue {
                title: title.into(),
                text,
            });
        }
        self
    }

    pub(super) fn header(title: impl Into<String>) -> Self {
        Row {
            kind: RowKind::Header,
            indent: 0,
            segs: vec![Seg::strong(title, Tone::Accent)],
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        }
    }

    /// A section header carrying a count, right-aligned in the header rule.
    pub(super) fn header_n(title: impl Into<String>, n: usize) -> Self {
        let mut row = Row::header(title);
        row.segs.push(Seg::mono(n.to_string(), Tone::Muted).trail());
        row
    }

    pub(super) fn item(indent: u8, segs: Vec<Seg>) -> Self {
        Row {
            kind: RowKind::Item,
            indent,
            segs,
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        }
    }

    pub(super) fn detail(indent: u8, segs: Vec<Seg>) -> Self {
        Row {
            kind: RowKind::Detail,
            indent,
            segs,
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        }
    }

    pub(super) fn empty(indent: u8, text: impl Into<String>) -> Self {
        Row {
            kind: RowKind::Empty,
            indent,
            segs: vec![Seg::ui(text, Tone::Muted)],
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        }
    }
}

/// Read-only borrows of every manager field the row builders consume.
pub struct InspectorSources<'a> {
    /// Document tree.
    pub dom: &'a Tree<MizuNode>,
    /// Taffy layout engine (box metrics).
    pub taffy: &'a taffy::TaffyTree<EgoNodeId>,
    /// DOM → Taffy id mapping.
    pub node_to_taffy_id: &'a HashMap<EgoNodeId, taffy::prelude::NodeId>,
    /// Parsed style sheet.
    pub style_rules: &'a HashMap<String, StyleRules>,
    /// UI-thread variable store (mirror of the logic worker's state).
    pub store: &'a VariableStore,
    /// Compiled logic functions.
    pub logic_fns: &'a FxHashMap<Symbol, MizuFunction>,
    /// Computed bindings in topological order.
    pub computed_bindings: &'a [ComputedBinding],
    /// Compile-time endpoint aliases.
    pub url_registry: &'a UrlRegistry,
    /// Root-level `timer` declarations.
    pub root_timers: &'a [RootTimer],
    /// Pending root-timer deadlines (values index into `root_timers`).
    pub root_timer_queue: &'a BTreeMap<Instant, Vec<usize>>,
    /// Per-origin storage budget.
    pub capability_policy: &'a CapabilityPolicy,
    /// Runtime activity log.
    pub log: &'a InspectorLog,
    /// Instant of the most recent mutation per variable (drives value flash).
    pub recent_mutations: &'a FxHashMap<Symbol, Instant>,
    /// Expansion metadata for lists, including budget truncation.
    pub each_expansion: &'a EachExpansion,
}

/// Builds the row list for the active tab.
pub fn build_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
    match state.tab {
        InspectorTab::Elements => elements_rows(src, state),
        InspectorTab::Style => style_rows(src, state),
        InspectorTab::Logic => logic_rows(src, state),
        InspectorTab::Events => events_rows(src),
        InspectorTab::Network => network_rows(src),
    }
}
