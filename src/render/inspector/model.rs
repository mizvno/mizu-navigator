//! Builds the row lists displayed by each inspector tab.
//!
//! The model is a pure function of the manager's current state: every call to
//! [`build_rows`] produces the rows for the active tab from scratch.  Redraws
//! are event-driven, documents are small by design, and all inputs live on the
//! UI thread, so rebuilding is both cheap and always consistent.
//!
//! ## Rows are structured, not pre-formatted
//!
//! A [`Row`] is a list of typed [`Seg`]ments rather than one finished string.
//! That split is what lets the paint pass do its job properly: it can colour a
//! key differently from its value, set code in monospace and labels in the UI
//! face, right-align durations against the panel's edge, and — critically —
//! decide *at paint time*, against the real measured width, which segment to
//! elide and how (see [`Flex`]).  A pre-joined string can do none of that; it
//! can only be clipped mid-glyph.
//!
//! The corollary is that **the model never truncates for display**.  It hands
//! over the full text and lets the painter fit it.  The only truncation that
//! remains is [`crate::render::inspector::log`]'s memory bound on retained log
//! strings, which is deliberately far wider than any panel.
//!
//! ## Reading a value that does not fit
//!
//! Eliding a long URL or expression to fit a 420px row is right for the row —
//! but it must not be the only way to read that value, the way the old panel
//! left it. Any row whose text is long enough that elision is a real risk
//! carries an [`InspectValue`] payload: the row's full, untruncated text plus
//! a short label. Clicking the row opens the panel's value-inspection drawer
//! (see [`crate::render::inspector::ValueView`]) with that text word-wrapped
//! and independently scrollable — the same shape as a browser's Network
//! request-details pane or an Elements attribute editor, rather than a tooltip
//! that vanishes when the mouse moves.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use ego_tree::{NodeId as EgoNodeId, Tree};
use rustc_hash::FxHashMap;

use crate::core::types::{Symbol, Value, VariableStore};
use crate::parser::logic::{
    Action, BinOp, ComputedBinding, Expr, ExprArena, MizuFunction, RootTimer, TimerInterval,
};
use crate::parser::{ConditionalClass, EventBlock, MizuNode, StyleRules, UrlRegistry};
use crate::render::inspector::log::{InspectorLog, NetOutcome};
use crate::render::inspector::{
    DETAIL_ROW_HEIGHT, HEADER_ROW_HEIGHT, InspectorState, InspectorTab, ROW_HEIGHT,
};
use crate::render::layout_bridge::EachExpansion;
use crate::render::security::CapabilityPolicy;

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
    fn maybe_inspect(mut self, title: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        if text.chars().count() > INSPECTABLE_MIN_CHARS {
            self.inspect = Some(InspectValue {
                title: title.into(),
                text,
            });
        }
        self
    }

    fn header(title: impl Into<String>) -> Self {
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
    fn header_n(title: impl Into<String>, n: usize) -> Self {
        let mut row = Row::header(title);
        row.segs.push(Seg::mono(n.to_string(), Tone::Muted).trail());
        row
    }

    fn item(indent: u8, segs: Vec<Seg>) -> Self {
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

    fn detail(indent: u8, segs: Vec<Seg>) -> Self {
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

    fn empty(indent: u8, text: impl Into<String>) -> Self {
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

// ─────────────────────────────────────────────────────────────────────────────
// Elements
// ─────────────────────────────────────────────────────────────────────────────

/// Longest text preview kept inline on an element row before the painter's
/// elision takes over. This is a *semantic* cap — a paragraph's whole body on
/// one tree row would drown the structure even on a very wide panel — not a
/// width cap, which is the painter's job.
const CONTENT_PREVIEW_CHARS: usize = 40;

/// Builds the segments describing a DOM node, in the order they read:
/// tag, `#id`, `.class`, `"content"`, event markers, hidden-children note.
pub fn node_label_segs(node: &MizuNode, truncated_count: Option<usize>) -> Vec<Seg> {
    let mut segs = vec![Seg::mono(node.primitive.as_str(), Tone::Accent)];
    if let Some(id) = node.attributes.get("id") {
        segs.push(Seg::mono(format!("#{id}"), Tone::Value));
    }
    if let Some(class) = node.attributes.get("class") {
        segs.push(Seg::mono(
            format!(".{}", class.trim_start_matches('.')),
            Tone::Key,
        ));
    }
    if let Some(content) = node.attributes.get("content") {
        let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview: String = flat.chars().take(CONTENT_PREVIEW_CHARS).collect();
        let ellipsis = if flat.chars().count() > CONTENT_PREVIEW_CHARS {
            "…"
        } else {
            ""
        };
        // The element's own text is the widest thing on the row and the least
        // structural, so it is what yields first when the panel is narrow.
        segs.push(Seg::mono(format!("\"{preview}{ellipsis}\""), Tone::Muted).elide());
    }
    for event in ["click", "submit"] {
        if node.events.contains_key(event) {
            segs.push(Seg::mono(format!("[{event}]"), Tone::Good));
        }
    }
    if let Some(count) = truncated_count {
        segs.push(Seg::mono(format!("+{count} hidden"), Tone::Bad).trail());
    }
    segs
}

/// Compact one-line description of a DOM node.
pub fn node_label(node: &MizuNode, truncated_count: Option<usize>) -> String {
    node_label_segs(node, truncated_count)
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn elements_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
    let mut rows = Vec::new();
    // Iterative DFS honouring the collapse set.
    let mut stack: Vec<(EgoNodeId, u8)> = vec![(src.dom.root().id(), 0)];
    while let Some((id, depth)) = stack.pop() {
        let Some(node_ref) = src.dom.get(id) else {
            continue;
        };
        let has_children = node_ref.has_children();
        let collapsed = state.collapsed.contains(&id);
        let truncated = src.each_expansion.truncated.get(&id).copied();
        let node = node_ref.value();
        let mut row = Row {
            kind: RowKind::Item,
            indent: depth,
            segs: node_label_segs(node, truncated),
            node: Some(id),
            expandable: has_children,
            expanded: has_children && !collapsed,
            inspect: None,
        };
        if let Some(content) = node.attributes.get("content") {
            let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
            row = row.maybe_inspect("Text content", flat);
        }
        rows.push(row);
        if has_children && !collapsed {
            // Push children in reverse so they pop in document order.
            let children: Vec<EgoNodeId> = node_ref.children().map(|c| c.id()).collect();
            for child in children.into_iter().rev() {
                stack.push((child, depth.saturating_add(1)));
            }
        }
    }
    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Style
// ─────────────────────────────────────────────────────────────────────────────

fn fmt_color(c: &crate::parser::MizuColor) -> String {
    if c.a == 255 {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    }
}

fn fmt_dimension(d: &crate::parser::MizuDimension) -> String {
    match d {
        crate::parser::MizuDimension::Pixels(v) => format!("{v}"),
        crate::parser::MizuDimension::Percent(v) => format!("{v}%"),
        crate::parser::MizuDimension::ViewportWidth(v) => format!("{v}vw"),
        crate::parser::MizuDimension::ViewportHeight(v) => format!("{v}vh"),
        crate::parser::MizuDimension::ViewportMin(v) => format!("{v}vmin"),
        crate::parser::MizuDimension::ViewportMax(v) => format!("{v}vmax"),
    }
}

/// One explicitly-set declaration of a style rule.
struct Decl {
    name: &'static str,
    value: String,
    /// Colour chip to show alongside the value, when the value names one.
    swatch: Option<crate::parser::MizuColor>,
}

impl Decl {
    fn new(name: &'static str, value: impl Into<String>) -> Self {
        Decl {
            name,
            value: value.into(),
            swatch: None,
        }
    }

    fn colored(name: &'static str, c: &crate::parser::MizuColor) -> Self {
        Decl {
            name,
            value: fmt_color(c),
            swatch: Some(c.clone()),
        }
    }
}

/// Lists the explicitly-set declarations of a style rule.
fn style_decls(rules: &StyleRules) -> Vec<Decl> {
    let mut out = Vec::new();
    if let Some(v) = &rules.width {
        out.push(Decl::new("width", fmt_dimension(v)));
    }
    if let Some(v) = &rules.height {
        out.push(Decl::new("height", fmt_dimension(v)));
    }
    if let Some(v) = &rules.padding {
        out.push(Decl::new("padding", fmt_dimension(v)));
    }
    if let Some(v) = &rules.margin {
        out.push(Decl::new("margin", fmt_dimension(v)));
    }
    if let Some(v) = &rules.gap {
        out.push(Decl::new("gap", fmt_dimension(v)));
    }
    if let Some(v) = &rules.flex_direction {
        out.push(Decl::new("flex-direction", format!("{v:?}").to_lowercase()));
    }
    if let Some(v) = &rules.justify {
        out.push(Decl::new("justify", format!("{v:?}").to_lowercase()));
    }
    if let Some(v) = &rules.align {
        out.push(Decl::new("align", format!("{v:?}").to_lowercase()));
    }
    match &rules.background {
        Some(crate::parser::style::MizuBackground::Solid(c)) => {
            out.push(Decl::colored("background", c));
        }
        Some(crate::parser::style::MizuBackground::LinearGradient { angle, start, end }) => {
            out.push(Decl::new(
                "background",
                format!(
                    "linear-gradient({angle}deg, {}, {})",
                    fmt_color(start),
                    fmt_color(end)
                ),
            ));
        }
        None => {}
    }
    if let Some(v) = &rules.background_image {
        out.push(Decl::new("background-image", v.clone()));
    }
    if let Some(c) = &rules.color {
        out.push(Decl::colored("color", c));
    }
    if let Some(v) = rules.font_size {
        out.push(Decl::new("font-size", format!("{v}")));
    }
    if let Some(v) = rules.border_radius {
        out.push(Decl::new("border-radius", format!("{v}")));
    }
    if let Some(v) = rules.border_width {
        out.push(Decl::new("border-width", format!("{v}")));
    }
    if let Some(c) = &rules.border_color {
        out.push(Decl::colored("border-color", c));
    }
    if rules.z_index != 0 {
        out.push(Decl::new("z-index", format!("{}", rules.z_index)));
    }
    out
}

/// Emits a block of `name: value` rows with the names padded to a common
/// width, so the values line up in a column.
///
/// Padding with spaces is exact here because declaration names are set in the
/// monospace face — the same reason a terminal can align this way.
fn push_decl_rows(rows: &mut Vec<Row>, decls: &[Decl]) {
    let width = decls.iter().map(|d| d.name.len()).max().unwrap_or(0);
    for decl in decls {
        let mut value = Seg::mono(&decl.value, Tone::Value).elide();
        if let Some(c) = &decl.swatch {
            value = value.swatch(c);
        }
        rows.push(
            Row::item(
                1,
                vec![
                    Seg::mono(format!("{:<width$}", decl.name, width = width), Tone::Key),
                    value,
                ],
            )
            .maybe_inspect(decl.name, &decl.value),
        );
    }
}

fn style_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
    let Some(sel) = state.selected else {
        return vec![Row::empty(
            0,
            "No element selected. Pick one in Elements, or use the picker.",
        )];
    };
    let Some(node_ref) = src.dom.get(sel) else {
        return vec![Row::empty(
            0,
            "Selection is stale — the document was replaced.",
        )];
    };
    let node = node_ref.value();
    let mut rows = Vec::new();
    let truncated = src.each_expansion.truncated.get(&sel).copied();

    rows.push(Row::header("Selected"));
    rows.push(Row::item(1, node_label_segs(node, truncated)));

    // ── Box metrics ──────────────────────────────────────────────────────
    if let Some(&t_id) = src.node_to_taffy_id.get(&sel)
        && let Ok(layout) = src.taffy.layout(t_id)
    {
        rows.push(Row::header("Box"));
        push_decl_rows(
            &mut rows,
            &[
                Decl::new(
                    "size",
                    format!("{:.0} × {:.0}", layout.size.width, layout.size.height),
                ),
                Decl::new(
                    "offset",
                    format!("{:.0}, {:.0}", layout.location.x, layout.location.y),
                ),
            ],
        );
    }

    // ── Style cascade: tag rules, then class rules, then conditionals ────
    let tag = node.primitive.as_str();
    if let Some(rules) = src.style_rules.get(tag) {
        let decls = style_decls(rules);
        rows.push(Row::header_n(format!("Rules · {tag}"), decls.len()));
        push_decl_rows(&mut rows, &decls);
    }
    if let Some(class) = node.attributes.get("class") {
        let class_name = class.trim_start_matches('.');
        if let Some(rules) = src.style_rules.get(class_name) {
            let decls = style_decls(rules);
            rows.push(Row::header_n(format!("Rules · .{class_name}"), decls.len()));
            push_decl_rows(&mut rows, &decls);
        }
    }

    if !node.conditional_classes.is_empty() {
        rows.push(Row::header_n(
            "Conditional classes",
            node.conditional_classes.len(),
        ));
        // Conditions are pure by construction, so evaluating them here is
        // side-effect free; the store clone isolates the instruction budget.
        let mut eval_store = src.store.clone();
        for cc in &node.conditional_classes {
            match cc {
                ConditionalClass::Toggle {
                    class_name,
                    condition,
                } => {
                    let active = crate::parser::logic::evaluate(
                        condition.root(),
                        &condition.arena,
                        &mut eval_store,
                        src.logic_fns,
                        0,
                    );
                    let (status, tone) = match active {
                        Ok(Value::Bool(true)) => ("on", Tone::Good),
                        Ok(_) => ("off", Tone::Muted),
                        Err(_) => ("err", Tone::Bad),
                    };
                    rows.push(Row::item(
                        1,
                        vec![
                            Seg::mono(format!("{status:<3}"), tone),
                            Seg::mono(format!(".{class_name}"), Tone::Value),
                        ],
                    ));
                    let condition_text =
                        format_expr(condition.root(), &condition.arena, &src.store.interner);
                    rows.push(
                        Row::detail(
                            2,
                            vec![
                                Seg::mono("if", Tone::Key),
                                Seg::mono(condition_text.clone(), Tone::Muted).elide(),
                            ],
                        )
                        .maybe_inspect("Condition", condition_text),
                    );
                }
                ConditionalClass::Ternary { expr } => {
                    let result = crate::parser::logic::evaluate(
                        expr.root(),
                        &expr.arena,
                        &mut eval_store,
                        src.logic_fns,
                        0,
                    );
                    let (label, tone) = match result {
                        Ok(Value::String(s)) => (format!(".{s}"), Tone::Good),
                        Ok(_) => ("<non-string result>".to_string(), Tone::Bad),
                        Err(_) => ("<error>".to_string(), Tone::Bad),
                    };
                    rows.push(Row::item(
                        1,
                        vec![
                            Seg::mono("?  ", Tone::Muted),
                            Seg::mono(label, tone),
                            Seg::ui("ternary", Tone::Muted).trail(),
                        ],
                    ));
                    let expr_text = format_expr(expr.root(), &expr.arena, &src.store.interner);
                    rows.push(
                        Row::detail(
                            2,
                            vec![
                                Seg::mono("=", Tone::Key),
                                Seg::mono(expr_text.clone(), Tone::Muted).elide(),
                            ],
                        )
                        .maybe_inspect("Expression", expr_text),
                    );
                }
            }
        }
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Logic
// ─────────────────────────────────────────────────────────────────────────────

/// Renders a value and picks the tone that tells you its type at a glance.
fn value_seg(v: &Value) -> Seg {
    let (text, tone) = match v {
        Value::String(s) => (format!("\"{s}\""), Tone::Value),
        Value::Bool(true) => ("true".to_string(), Tone::Good),
        Value::Bool(false) => ("false".to_string(), Tone::Muted),
        Value::Null => ("null".to_string(), Tone::Muted),
        other => (format!("{other}"), Tone::Value),
    };
    Seg::mono(text, tone).elide()
}

/// How long a freshly-mutated variable stays highlighted in the Logic tab.
const MUTATION_FLASH: std::time::Duration = std::time::Duration::from_millis(1500);

fn logic_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
    let mut rows = Vec::new();

    // ── Information flow ─────────────────────────────────────────────────
    rows.push(Row::header("Information flow"));
    match state.flow_metrics {
        Some((sources, sinks, violations)) => {
            let (label, tone) = if violations == 0 {
                ("no violations".to_string(), Tone::Good)
            } else {
                (format!("{violations} violations"), Tone::Bad)
            };
            rows.push(Row::item(
                1,
                vec![
                    Seg::mono(label, tone),
                    Seg::mono(format!("{sources} sources · {sinks} sinks"), Tone::Muted).trail(),
                ],
            ));
        }
        None => rows.push(Row::empty(1, "Flow metrics not available.")),
    }

    let interner = &src.store.interner;
    let now = Instant::now();
    let is_fresh = |sym: &Symbol| {
        src.recent_mutations
            .get(sym)
            .map(|at| now.duration_since(*at) < MUTATION_FLASH)
            .unwrap_or(false)
    };
    let comp_names: std::collections::HashSet<Symbol> =
        src.computed_bindings.iter().map(|cb| cb.name).collect();

    // ── Variables (sorted by name; computed ones listed separately) ──────
    let mut vars: Vec<(String, Seg, bool)> = src
        .store
        .evaluator
        .global_store
        .iter()
        .filter(|(sym, _)| !comp_names.contains(sym))
        .filter_map(|(sym, val)| {
            interner
                .resolve(*sym)
                .map(|name| (name.to_string(), value_seg(val), is_fresh(sym)))
        })
        .collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));

    rows.push(Row::header_n("Variables", vars.len()));
    if vars.is_empty() {
        rows.push(Row::empty(1, "This document declares no state."));
    }
    // Names are monospace, so padding to the longest one aligns the values
    // into a column the eye can scan straight down.
    let name_w = vars.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0);
    for (name, value, fresh) in vars {
        let name_tone = if fresh { Tone::Accent } else { Tone::Key };
        let value_text = value.text.clone();
        rows.push(
            Row::item(
                1,
                vec![
                    Seg::mono(format!("{name:<name_w$}"), name_tone),
                    Seg::mono("=", Tone::Muted),
                    value,
                ],
            )
            .maybe_inspect(name, value_text),
        );
    }

    // ── Computed bindings ────────────────────────────────────────────────
    rows.push(Row::header_n("Computed", src.computed_bindings.len()));
    if src.computed_bindings.is_empty() {
        rows.push(Row::empty(1, "No derived values."));
    }
    for cb in src.computed_bindings {
        let name = interner.resolve(cb.name).unwrap_or("?");
        let current = src
            .store
            .evaluator
            .global_store
            .get(&cb.name)
            .map(value_seg)
            .unwrap_or_else(|| Seg::mono("null", Tone::Muted));
        let name_tone = if is_fresh(&cb.name) {
            Tone::Accent
        } else {
            Tone::Key
        };
        rows.push(Row::item(
            1,
            vec![
                Seg::mono(name, name_tone),
                Seg::mono("=", Tone::Muted),
                current,
            ],
        ));
        let expr_text = format_expr(cb.expr.root(), &cb.expr.arena, interner);
        rows.push(
            Row::detail(2, vec![Seg::mono(expr_text.clone(), Tone::Muted).elide()])
                .maybe_inspect("Expression", expr_text),
        );
        let deps: Vec<&str> = cb
            .depends_on
            .iter()
            .filter_map(|d| interner.resolve(*d))
            .collect();
        if !deps.is_empty() {
            let deps_text = deps.join(", ");
            rows.push(
                Row::detail(
                    2,
                    vec![
                        Seg::ui("depends on", Tone::Muted),
                        Seg::mono(deps_text.clone(), Tone::Muted).elide(),
                    ],
                )
                .maybe_inspect("Depends on", deps_text),
            );
        }
    }

    // ── Functions ────────────────────────────────────────────────────────
    let mut fns: Vec<(String, Vec<String>)> = src
        .logic_fns
        .iter()
        .filter_map(|(sym, f)| {
            interner.resolve(*sym).map(|name| {
                let params: Vec<String> = f
                    .params
                    .iter()
                    .map(|(p, ty)| {
                        let pname = interner.resolve(*p).unwrap_or("?");
                        format!("{pname}: {ty}")
                    })
                    .collect();
                (name.to_string(), params)
            })
        })
        .collect();
    fns.sort_by(|a, b| a.0.cmp(&b.0));

    rows.push(Row::header_n("Functions", fns.len()));
    if fns.is_empty() {
        rows.push(Row::empty(1, "No functions declared."));
    }
    for (name, params) in fns {
        let signature = format!("{name}({})", params.join(", "));
        rows.push(
            Row::item(
                1,
                vec![
                    Seg::mono(name, Tone::Value),
                    Seg::mono(format!("({})", params.join(", ")), Tone::Muted).elide(),
                ],
            )
            .maybe_inspect("Signature", signature),
        );
    }
    if !src.logic_fns.is_empty() {
        rows.push(Row::detail(
            1,
            vec![Seg::ui("call graph verified acyclic", Tone::Good)],
        ));
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Events
// ─────────────────────────────────────────────────────────────────────────────

fn fmt_millis(ms: u64) -> String {
    if ms >= 1000 && ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

fn countdown_seg(deadline: Option<Instant>, now: Instant) -> Seg {
    match deadline {
        Some(d) if d > now => Seg::mono(
            format!("next in {:.1}s", (d - now).as_secs_f32()),
            Tone::Muted,
        ),
        Some(_) => Seg::mono("due", Tone::Accent),
        None => Seg::mono("idle", Tone::Muted),
    }
    .trail()
}

fn events_rows(src: &InspectorSources<'_>) -> Vec<Row> {
    let mut rows = Vec::new();
    let interner = &src.store.interner;
    let now = Instant::now();

    // ── Declared timers ──────────────────────────────────────────────────
    rows.push(Row::header_n("Timers", src.root_timers.len()));
    if src.root_timers.is_empty() {
        rows.push(Row::empty(1, "No timers declared."));
    }
    for (idx, rt) in src.root_timers.iter().enumerate() {
        let interval = match &rt.interval {
            TimerInterval::Millis(ms) => fmt_millis(*ms),
            TimerInterval::Variable(name) => format!("{{{name}}}"),
        };
        let deadline = src
            .root_timer_queue
            .iter()
            .find(|(_, idxs)| idxs.contains(&idx))
            .map(|(d, _)| *d);
        rows.push(Row::item(
            1,
            vec![
                Seg::mono("every", Tone::Key),
                Seg::mono(interval, Tone::Value),
                countdown_seg(deadline, now),
            ],
        ));
        let action_text = format_action(&rt.action, interner);
        rows.push(
            Row::detail(
                2,
                vec![
                    Seg::mono("→", Tone::Muted),
                    Seg::mono(action_text.clone(), Tone::Muted).elide(),
                ],
            )
            .maybe_inspect("Action", action_text),
        );
    }

    // ── Declared actions ─────────────────────────────────────────────────
    let mut actions: Vec<(String, String, String)> = Vec::new();
    for node_ref in src.dom.nodes() {
        for (event_name, block) in &node_ref.value().events {
            let action = match block {
                EventBlock::Click { action } => action,
                EventBlock::Submit { action } => action,
            };
            actions.push((
                node_ref.value().primitive.as_str().to_string(),
                event_name.clone(),
                format_action(action, interner),
            ));
        }
    }

    rows.push(Row::header_n("Actions", actions.len()));
    if actions.is_empty() {
        rows.push(Row::empty(1, "No event handlers declared."));
    }
    for (tag, event, action) in actions {
        rows.push(Row::item(
            1,
            vec![
                Seg::mono(tag, Tone::Accent),
                Seg::mono(format!("on:{event}"), Tone::Key),
            ],
        ));
        rows.push(
            Row::detail(
                2,
                vec![
                    Seg::mono("→", Tone::Muted),
                    Seg::mono(action.clone(), Tone::Muted).elide(),
                ],
            )
            .maybe_inspect("Action", action),
        );
    }

    // ── Runtime log (newest first) ───────────────────────────────────────
    rows.push(Row::header_n("Activity", src.log.events.len()));
    if src.log.events.is_empty() {
        rows.push(Row::empty(1, "Nothing has happened yet."));
    }
    for entry in src.log.events.iter().rev() {
        rows.push(
            Row::item(
                1,
                vec![
                    Seg::mono(format!("{:<6}", entry.kind.tag()), Tone::Key),
                    Seg::mono(&entry.detail, Tone::Value).elide(),
                    Seg::mono(src.log.fmt_ts(entry.at), Tone::Muted).trail(),
                ],
            )
            .maybe_inspect(entry.kind.tag().trim(), &entry.detail),
        );
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Network
// ─────────────────────────────────────────────────────────────────────────────

/// Formats a byte count so a 2 MB response does not read as `2097152B`.
fn fmt_bytes(b: usize) -> String {
    const KIB: f64 = 1024.0;
    let b = b as f64;
    if b < KIB {
        format!("{b:.0} B")
    } else if b < KIB * KIB {
        format!("{:.1} KB", b / KIB)
    } else {
        format!("{:.1} MB", b / (KIB * KIB))
    }
}

fn network_rows(src: &InspectorSources<'_>) -> Vec<Row> {
    let mut rows = Vec::new();
    let interner = &src.store.interner;

    // ── Declared endpoints ───────────────────────────────────────────────
    let mut endpoints: Vec<(&'static str, String, String)> = src
        .url_registry
        .iter()
        .filter_map(|(sym, ep)| {
            interner.resolve(*sym).map(|alias| {
                let kind = match ep.kind {
                    crate::parser::EndpointKind::Api => "api",
                    crate::parser::EndpointKind::Media => "media",
                };
                (kind, alias.to_string(), ep.raw_target.clone())
            })
        })
        .collect();
    endpoints.sort_by(|a, b| a.1.cmp(&b.1));

    rows.push(Row::header_n("Endpoints", endpoints.len()));
    if endpoints.is_empty() {
        rows.push(Row::empty(1, "This document declares no network access."));
    }
    let alias_w = endpoints.iter().map(|(_, a, _)| a.len()).max().unwrap_or(0);
    for (kind, alias, target) in endpoints {
        rows.push(Row::item(
            1,
            vec![
                Seg::mono(format!("{kind:<5}"), Tone::Muted),
                Seg::mono(format!("{alias:<alias_w$}"), Tone::Value),
            ],
        ));
        // The target goes on its own line: URLs are the longest strings in
        // the panel, and sharing a row with the alias would elide both.
        rows.push(
            Row::detail(2, vec![Seg::mono(target.clone(), Tone::Muted).middle()])
                .maybe_inspect("Target", target),
        );
    }

    // ── Storage budget ───────────────────────────────────────────────────
    let used = src.capability_policy.bytes_stored();
    let quota = src.capability_policy.quota_bytes;
    let pct = if quota > 0 {
        (used as f64 / quota as f64 * 100.0).min(999.0)
    } else {
        0.0
    };
    rows.push(Row::header("Storage"));
    rows.push(Row::item(
        1,
        vec![
            Seg::mono(
                format!("{} / {}", fmt_bytes(used), fmt_bytes(quota)),
                Tone::Value,
            ),
            Seg::mono(
                format!("{pct:.0}%"),
                if pct >= 90.0 { Tone::Bad } else { Tone::Muted },
            )
            .trail(),
        ],
    ));

    // ── Request log (newest first) ───────────────────────────────────────
    rows.push(Row::header_n("Requests", src.log.network.len()));
    if src.log.network.is_empty() {
        rows.push(Row::empty(1, "No requests yet."));
    }
    for entry in src.log.network.iter().rev() {
        let tone = match &entry.outcome {
            NetOutcome::Ok => Tone::Good,
            NetOutcome::Failed(_) | NetOutcome::Blocked(_) => Tone::Bad,
            NetOutcome::Pending => Tone::Muted,
            NetOutcome::Redirect => Tone::Accent,
        };
        let mut metrics = Vec::new();
        if let Some(ms) = entry.duration_ms {
            metrics.push(format!("{ms}ms"));
        }
        if let Some(b) = entry.bytes {
            metrics.push(fmt_bytes(b));
        }

        let mut head = vec![
            Seg::mono(format!("{:<8}", entry.outcome.tag()), tone),
            Seg::mono(format!("{:<5}", entry.verb), Tone::Key),
            Seg::mono(src.log.fmt_ts(entry.at), Tone::Muted),
        ];
        if !metrics.is_empty() {
            head.push(Seg::mono(metrics.join(" · "), Tone::Muted).trail());
        }
        rows.push(Row::item(1, head));
        // The target is the payload of the row and gets a full line of its
        // own, middle-elided so both host and path stay legible.
        rows.push(
            Row::detail(2, vec![Seg::mono(&entry.target, Tone::Value).middle()])
                .maybe_inspect("Target", &entry.target),
        );
        match &entry.outcome {
            NetOutcome::Failed(reason) | NetOutcome::Blocked(reason) => {
                rows.push(
                    Row::detail(2, vec![Seg::mono(reason, Tone::Bad).elide()])
                        .maybe_inspect("Reason", reason),
                );
            }
            _ => {}
        }
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Expression / action pretty-printing
// ─────────────────────────────────────────────────────────────────────────────

fn binop_str(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Le => "<=",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

/// Renders an expression back to compact Mizu-like source.
///
/// Depth is naturally bounded: the parser rejects nesting beyond
/// `MAX_PARSE_DEPTH` (256), well within the native stack.
pub fn format_expr(
    e: &Expr,
    arena: &ExprArena,
    interner: &crate::core::types::FrozenInterner,
) -> String {
    match e {
        Expr::Literal(v) => match v {
            Value::String(s) => format!("\"{s}\""),
            other => format!("{other}"),
        },
        Expr::Variable(sym) => interner.resolve(*sym).unwrap_or("?").to_string(),
        Expr::BinaryOp { left, op, right } => format!(
            "{} {} {}",
            format_expr(&arena[*left], arena, interner),
            binop_str(op),
            format_expr(&arena[*right], arena, interner)
        ),
        Expr::FunctionCall {
            name,
            args_start,
            args_len,
        } => {
            let args: Vec<String> = arena
                .args(*args_start, *args_len)
                .iter()
                .map(|&a| format_expr(&arena[a], arena, interner))
                .collect();
            format!(
                "{}({})",
                interner.resolve(*name).unwrap_or("?"),
                args.join(", ")
            )
        }
        Expr::Let { name, value, body } => format!(
            "{} = {}; {}",
            interner.resolve(*name).unwrap_or("?"),
            format_expr(&arena[*value], arena, interner),
            format_expr(&arena[*body], arena, interner)
        ),
        Expr::Not(inner) => format!("!{}", format_expr(&arena[*inner], arena, interner)),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => format!(
            "{} ? {} : {}",
            format_expr(&arena[*condition], arena, interner),
            format_expr(&arena[*then_expr], arena, interner),
            format_expr(&arena[*else_expr], arena, interner)
        ),
        Expr::FieldAccess {
            base,
            field,
            field_hash: _,
        } => {
            let field_name = interner.resolve(*field).unwrap_or("?");
            format!(
                "{}.{field_name}",
                format_expr(&arena[*base], arena, interner)
            )
        }
    }
}

/// Renders an action back to compact Mizu-like source.
pub fn format_action(a: &Action, interner: &crate::core::types::FrozenInterner) -> String {
    match a {
        Action::Assign { target, expr } => {
            format!(
                "{target} = {}",
                format_expr(expr.root(), &expr.arena, interner)
            )
        }
        Action::Eval(e) => format_expr(e.root(), &e.arena, interner),
        Action::Navigate { url } => {
            format!("navigate {}", format_expr(url.root(), &url.arena, interner))
        }
        Action::NetworkCall {
            method,
            alias_sym,
            target_var,
            ..
        } => format!(
            "{}({}) -> {}",
            method.as_str(),
            interner.resolve(*alias_sym).unwrap_or("?"),
            target_var
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::StringInterner;

    #[test]
    fn format_expr_roundtrips_simple_source() {
        let mut interner = StringInterner::new();
        let expr = crate::parser::logic::parse_expr_standalone("count > 4 && !busy", &mut interner)
            .unwrap();
        let interner = interner.freeze();
        assert_eq!(
            format_expr(expr.root(), &expr.arena, &interner),
            "count > 4 && !busy"
        );
    }

    #[test]
    fn format_action_assign() {
        let mut interner = StringInterner::new();
        let action =
            crate::parser::logic::parse_action("count = count + 1", &mut interner).unwrap();
        let interner = interner.freeze();
        assert_eq!(format_action(&action, &interner), "count = count + 1");
    }

    fn node_with(attrs: &[(&str, &str)], events: &[&str]) -> MizuNode {
        let mut attributes = FxHashMap::default();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v.to_string());
        }
        let mut event_map = FxHashMap::default();
        let mut it = StringInterner::new();
        for name in events {
            let action = crate::parser::logic::parse_action("x = 1", &mut it).unwrap();
            event_map.insert((*name).to_string(), EventBlock::Click { action });
        }
        MizuNode {
            primitive: crate::parser::Primitive::Button,
            attributes,
            events: event_map,
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    #[test]
    fn node_label_shows_events_and_class() {
        let node = node_with(&[("class", "card")], &["click"]);
        let label = node_label(&node, None);
        assert!(label.contains("button"));
        assert!(label.contains(".card"));
        assert!(label.contains("[click]"));
    }

    #[test]
    fn node_label_segments_are_individually_toned() {
        let node = node_with(&[("id", "save"), ("class", "primary")], &[]);
        let segs = node_label_segs(&node, None);
        assert_eq!(segs[0].text, "button");
        assert_eq!(segs[0].tone, Tone::Accent, "the tag carries the accent");
        assert_eq!(segs[1].text, "#save");
        assert_eq!(segs[2].text, ".primary");
        assert!(
            segs.iter().all(|s| s.flex != Flex::Elide),
            "with no text content there is nothing that should yield first"
        );
    }

    #[test]
    fn long_content_is_the_segment_that_yields() {
        let long = "a".repeat(400);
        let node = node_with(&[("content", long.as_str())], &[]);
        let segs = node_label_segs(&node, None);
        let content = segs
            .iter()
            .find(|s| s.text.starts_with('"'))
            .expect("content segment");
        assert_eq!(
            content.flex,
            Flex::Elide,
            "the text preview must be the segment the painter shrinks"
        );
        assert!(
            content.text.chars().count() <= CONTENT_PREVIEW_CHARS + 3,
            "a whole paragraph must not be pasted onto one tree row"
        );
    }

    #[test]
    fn content_preview_collapses_newlines() {
        let node = node_with(&[("content", "first\n\n  second")], &[]);
        let label = node_label(&node, None);
        assert!(
            label.contains("\"first second\""),
            "embedded whitespace must not break the single-line row: {label}"
        );
    }

    #[test]
    fn row_heights_differ_by_role() {
        assert!(
            Row::header("x").height() > Row::item(0, vec![]).height(),
            "headers need the extra leading that separates sections"
        );
        assert!(Row::detail(0, vec![]).height() < Row::item(0, vec![]).height());
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn declaration_names_are_padded_into_a_column() {
        let mut rows = Vec::new();
        push_decl_rows(
            &mut rows,
            &[Decl::new("gap", "4"), Decl::new("border-radius", "8")],
        );
        let widths: Vec<usize> = rows.iter().map(|r| r.segs[0].text.len()).collect();
        assert_eq!(
            widths[0], widths[1],
            "values must line up in a column, so names pad to a common width"
        );
    }
}
