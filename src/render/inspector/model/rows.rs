//! Per-tab row builders: `elements_rows`, `style_rows`, `logic_rows`,
//! `events_rows`, and `network_rows`, plus each tab's private formatting
//! helpers.

use std::time::Instant;

use ego_tree::NodeId as EgoNodeId;

use crate::core::types::{Symbol, Value};
use crate::parser::logic::TimerInterval;
use crate::parser::{ConditionalClass, EventBlock, MizuNode, StyleRules};
use crate::render::inspector::InspectorState;
use crate::render::inspector::log::NetOutcome;

use super::format::{format_action, format_expr};
use super::types::{InspectorSources, Row, RowKind, Seg, Tone};

// ─────────────────────────────────────────────────────────────────────────────
// Elements
// ─────────────────────────────────────────────────────────────────────────────

/// Longest text preview kept inline on an element row before the painter's
/// elision takes over. This is a *semantic* cap — a paragraph's whole body on
/// one tree row would drown the structure even on a very wide panel — not a
/// width cap, which is the painter's job.
pub(super) const CONTENT_PREVIEW_CHARS: usize = 40;

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

pub(super) fn elements_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
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
pub(super) struct Decl {
    name: &'static str,
    value: String,
    /// Colour chip to show alongside the value, when the value names one.
    swatch: Option<crate::parser::MizuColor>,
}

impl Decl {
    pub(super) fn new(name: &'static str, value: impl Into<String>) -> Self {
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
pub(super) fn push_decl_rows(rows: &mut Vec<Row>, decls: &[Decl]) {
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

pub(super) fn style_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
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

pub(super) fn logic_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
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

pub(super) fn events_rows(src: &InspectorSources<'_>) -> Vec<Row> {
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
pub(super) fn fmt_bytes(b: usize) -> String {
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

pub(super) fn network_rows(src: &InspectorSources<'_>) -> Vec<Row> {
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
