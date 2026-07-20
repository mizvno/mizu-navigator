//! Builds the flat row lists displayed by each inspector tab.
//!
//! The model is a pure function of the manager's current state: every call to
//! [`build_rows`] produces the rows for the active tab from scratch.  Redraws
//! are event-driven, documents are small by design, and all inputs live on the
//! UI thread, so rebuilding is both cheap and always consistent.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use ego_tree::{NodeId as EgoNodeId, Tree};
use rustc_hash::FxHashMap;

use crate::core::types::{StringInterner, Symbol, Value, VariableStore};
use crate::parser::logic::{
    Action, BinOp, ComputedBinding, Expr, MizuFunction, RootTimer, TimerInterval,
};
use crate::parser::{EventBlock, MizuNode, StyleRules, UrlRegistry};
use crate::render::inspector::log::{InspectorLog, NetOutcome};
use crate::render::inspector::{InspectorState, InspectorTab};
use crate::render::layout_bridge::EachExpansion;
use crate::render::security::CapabilityPolicy;

/// Visual category of a row, mapped to a colour by the paint pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Section header.
    Header,
    /// Regular content.
    Normal,
    /// De-emphasised detail.
    Dim,
    /// Highlighted / accent (e.g. selected concepts, live values).
    Accent,
    /// Positive outcome (request ok, condition true).
    Good,
    /// Negative outcome (error, blocked).
    Bad,
}

/// One displayable row of the active inspector tab.
#[derive(Debug, Clone)]
pub struct Row {
    /// Indentation level (Elements tree depth; 0 elsewhere).
    pub indent: u8,
    /// Pre-formatted row text.
    pub text: String,
    /// Visual category.
    pub kind: RowKind,
    /// DOM node this row refers to (Elements rows), for selection/highlight.
    pub node: Option<EgoNodeId>,
    /// Whether the row can be expanded/collapsed (has children).
    pub expandable: bool,
}

impl Row {
    fn header(text: impl Into<String>) -> Self {
        Row {
            indent: 0,
            text: text.into(),
            kind: RowKind::Header,
            node: None,
            expandable: false,
        }
    }

    fn plain(indent: u8, text: impl Into<String>, kind: RowKind) -> Self {
        Row {
            indent,
            text: text.into(),
            kind,
            node: None,
            expandable: false,
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

/// Compact one-line description of a DOM node.
pub fn node_label(node: &MizuNode, truncated_count: Option<usize>) -> String {
    let mut label = node.primitive.as_str().to_string();
    if let Some(id) = node.attributes.get("id") {
        label.push_str(&format!(" #{id}"));
    }
    if let Some(class) = node.attributes.get("class") {
        label.push_str(&format!(" .{}", class.trim_start_matches('.')));
    }
    if let Some(content) = node.attributes.get("content") {
        let preview: String = content.chars().take(24).collect();
        if content.chars().count() > 24 {
            label.push_str(&format!(" \"{preview}…\""));
        } else {
            label.push_str(&format!(" \"{preview}\""));
        }
    }
    let mut markers = String::new();
    if node.events.contains_key("click") {
        markers.push_str(" [click]");
    }
    if node.events.contains_key("submit") {
        markers.push_str(" [submit]");
    }
    label.push_str(&markers);
    if let Some(count) = truncated_count {
        label.push_str(&format!(" [+{} hidden]", count));
    }
    label
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
        let arrow = if !has_children {
            "  "
        } else if collapsed {
            "> "
        } else {
            "v "
        };
        let kind = if state.selected == Some(id) {
            RowKind::Accent
        } else {
            RowKind::Normal
        };
        let truncated = src.each_expansion.truncated.get(&id).copied();
        rows.push(Row {
            indent: depth,
            text: format!("{arrow}{}", node_label(node_ref.value(), truncated)),
            kind,
            node: Some(id),
            expandable: has_children,
        });
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
    }
}

/// Lists the explicitly-set properties of a style rule as `name: value` lines.
fn style_props(rules: &StyleRules) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = &rules.width {
        out.push(format!("width: {}", fmt_dimension(v)));
    }
    if let Some(v) = &rules.height {
        out.push(format!("height: {}", fmt_dimension(v)));
    }
    if let Some(v) = &rules.padding {
        out.push(format!("padding: {}", fmt_dimension(v)));
    }
    if let Some(v) = &rules.margin {
        out.push(format!("margin: {}", fmt_dimension(v)));
    }
    if let Some(v) = &rules.gap {
        out.push(format!("gap: {}", fmt_dimension(v)));
    }
    if let Some(v) = &rules.direction {
        out.push(format!("direction: {v:?}").to_lowercase());
    }
    if let Some(v) = &rules.justify {
        out.push(format!("justify: {v:?}").to_lowercase());
    }
    if let Some(v) = &rules.align {
        out.push(format!("align: {v:?}").to_lowercase());
    }
    match &rules.background {
        Some(crate::parser::style::MizuBackground::Solid(c)) => {
            out.push(format!("background: {}", fmt_color(c)));
        }
        Some(crate::parser::style::MizuBackground::LinearGradient { angle, start, end }) => {
            out.push(format!(
                "background: linear-gradient({angle}deg, {}, {})",
                fmt_color(start),
                fmt_color(end)
            ));
        }
        None => {}
    }
    if let Some(v) = &rules.background_image {
        out.push(format!("background-image: {v}"));
    }
    if let Some(c) = &rules.color {
        out.push(format!("color: {}", fmt_color(c)));
    }
    if let Some(v) = rules.font_size {
        out.push(format!("font-size: {v}"));
    }
    if let Some(v) = rules.border_radius {
        out.push(format!("border-radius: {v}"));
    }
    if let Some(v) = rules.border_width {
        out.push(format!("border-width: {v}"));
    }
    if let Some(c) = &rules.border_color {
        out.push(format!("border-color: {}", fmt_color(c)));
    }
    if rules.z_index != 0 {
        out.push(format!("z-index: {}", rules.z_index));
    }
    out
}

fn style_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
    let Some(sel) = state.selected else {
        return vec![Row::plain(
            0,
            "no element selected — pick one in Elem",
            RowKind::Dim,
        )];
    };
    let Some(node_ref) = src.dom.get(sel) else {
        return vec![Row::plain(0, "selection is stale", RowKind::Dim)];
    };
    let node = node_ref.value();
    let mut rows = Vec::new();
    let truncated = src.each_expansion.truncated.get(&sel).copied();
    rows.push(Row::header(format!("SELECTED  {}", node_label(node, truncated))));

    // ── Box metrics ──────────────────────────────────────────────────────
    if let Some(&t_id) = src.node_to_taffy_id.get(&sel)
        && let Ok(layout) = src.taffy.layout(t_id)
    {
        rows.push(Row::header("BOX"));
        rows.push(Row::plain(
            1,
            format!(
                "size {:.0} x {:.0}   at ({:.0}, {:.0})",
                layout.size.width, layout.size.height, layout.location.x, layout.location.y
            ),
            RowKind::Normal,
        ));
    }

    // ── Style cascade: tag rules, then class rules, then conditionals ────
    let tag = node.primitive.as_str();
    if let Some(rules) = src.style_rules.get(tag) {
        rows.push(Row::header(format!("RULES  {tag}")));
        for prop in style_props(rules) {
            rows.push(Row::plain(1, prop, RowKind::Normal));
        }
    }
    if let Some(class) = node.attributes.get("class") {
        let class_name = class.trim_start_matches('.');
        if let Some(rules) = src.style_rules.get(class_name) {
            rows.push(Row::header(format!("RULES  .{class_name}")));
            for prop in style_props(rules) {
                rows.push(Row::plain(1, prop, RowKind::Normal));
            }
        }
    }

    if !node.conditional_classes.is_empty() {
        rows.push(Row::header("CONDITIONAL CLASSES"));
        // Conditions are pure by construction, so evaluating them here is
        // side-effect free; the store clone isolates the instruction budget.
        let mut eval_store = src.store.clone();
        for cc in &node.conditional_classes {
            let active = crate::parser::logic::evaluate(
                &cc.condition,
                &mut eval_store,
                src.logic_fns,
                0,
            );
            let (status, kind) = match active {
                Ok(Value::Bool(true)) => ("ON ", RowKind::Good),
                Ok(_) => ("off", RowKind::Dim),
                Err(_) => ("err", RowKind::Bad),
            };
            rows.push(Row::plain(
                1,
                format!(
                    "[{status}] .{}  if {}",
                    cc.class_name,
                    format_expr(&cc.condition, &src.store.interner)
                ),
                kind,
            ));
        }
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Logic
// ─────────────────────────────────────────────────────────────────────────────

fn fmt_value(v: &Value) -> String {
    let s = match v {
        Value::String(s) => format!("\"{s}\""),
        other => format!("{other}"),
    };
    crate::render::inspector::log::truncate_detail(&s)
}

/// How long a freshly-mutated variable stays highlighted in the Logic tab.
const MUTATION_FLASH: std::time::Duration = std::time::Duration::from_millis(1500);

fn logic_rows(src: &InspectorSources<'_>, state: &InspectorState) -> Vec<Row> {
    let mut rows = Vec::new();
    
    // ── Information Flow ──────────────────────────────────────────────────
    rows.push(Row::header("INFORMATION FLOW"));
    if let Some((sources, sinks, violations)) = state.flow_metrics {
        rows.push(Row::plain(
            1,
            format!("flow: {sources} sources, {sinks} sinks, {violations} violations"),
            if violations == 0 { RowKind::Good } else { RowKind::Bad },
        ));
    } else {
        rows.push(Row::plain(1, "flow metrics not available", RowKind::Dim));
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

    // ── Variables (sorted by name, computed ones listed separately) ──────
    rows.push(Row::header("VARIABLES"));
    let mut vars: Vec<(String, String, bool)> = src
        .store
        .state_machine
        .global_store
        .iter()
        .filter(|(sym, _)| !comp_names.contains(sym))
        .filter_map(|(sym, val)| {
            interner
                .resolve(*sym)
                .map(|name| (name.to_string(), fmt_value(val), is_fresh(sym)))
        })
        .collect();
    vars.sort();
    if vars.is_empty() {
        rows.push(Row::plain(1, "(none)", RowKind::Dim));
    }
    for (name, val, fresh) in vars {
        let kind = if fresh { RowKind::Accent } else { RowKind::Normal };
        rows.push(Row::plain(1, format!("{name} = {val}"), kind));
    }

    // ── Computed bindings ─────────────────────────────────────────────────
    rows.push(Row::header("COMPUTED"));
    if src.computed_bindings.is_empty() {
        rows.push(Row::plain(1, "(none)", RowKind::Dim));
    }
    for cb in src.computed_bindings {
        let name = interner.resolve(cb.name).unwrap_or("?");
        let current = src
            .store
            .state_machine
            .global_store
            .get(&cb.name)
            .map(fmt_value)
            .unwrap_or_else(|| "null".to_string());
        let kind = if is_fresh(&cb.name) {
            RowKind::Accent
        } else {
            RowKind::Normal
        };
        rows.push(Row::plain(
            1,
            format!("comp {name} = {}", format_expr(&cb.expr, interner)),
            kind,
        ));
        let deps: Vec<&str> = cb
            .depends_on
            .iter()
            .filter_map(|d| interner.resolve(*d))
            .collect();
        rows.push(Row::plain(
            2,
            format!("= {current}   deps: {}", deps.join(", ")),
            RowKind::Dim,
        ));
    }

    // ── Functions ─────────────────────────────────────────────────────────
    rows.push(Row::header("FUNCTIONS  (call graph: acyclic, checked)"));
    let mut fns: Vec<String> = src
        .logic_fns
        .iter()
        .filter_map(|(sym, f)| {
            interner.resolve(*sym).map(|name| {
                let params: Vec<String> = f
                    .params
                    .iter()
                    .map(|(p, ty)| {
                        let pname = interner.resolve(*p).unwrap_or("?");
                        match ty {
                            Some(t) => format!("{pname}: {}", t.as_str()),
                            None => pname.to_string(),
                        }
                    })
                    .collect();
                format!("{name}({})", params.join(", "))
            })
        })
        .collect();
    fns.sort();
    if fns.is_empty() {
        rows.push(Row::plain(1, "(none)", RowKind::Dim));
    }
    for f in fns {
        rows.push(Row::plain(1, f, RowKind::Normal));
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

fn fmt_countdown(deadline: Option<Instant>, now: Instant) -> String {
    match deadline {
        Some(d) if d > now => format!("next in {:.1}s", (d - now).as_secs_f32()),
        Some(_) => "due".to_string(),
        None => "idle".to_string(),
    }
}

fn events_rows(src: &InspectorSources<'_>) -> Vec<Row> {
    let mut rows = Vec::new();
    let interner = &src.store.interner;
    let now = Instant::now();

    // ── Declared timers ──────────────────────────────────────────────────
    rows.push(Row::header("TIMERS  (declared)"));
    let mut any_timer = false;
    for (idx, rt) in src.root_timers.iter().enumerate() {
        any_timer = true;
        let interval = match &rt.interval {
            TimerInterval::Millis(ms) => fmt_millis(*ms),
            TimerInterval::Variable(name) => format!("{{{name}}}"),
        };
        let deadline = src
            .root_timer_queue
            .iter()
            .find(|(_, idxs)| idxs.contains(&idx))
            .map(|(d, _)| *d);
        rows.push(Row::plain(
            1,
            format!(
                "timer {interval} -> {}   ({})",
                format_action(&rt.action, interner),
                fmt_countdown(deadline, now)
            ),
            RowKind::Normal,
        ));
    }
    if !any_timer {
        rows.push(Row::plain(1, "(none)", RowKind::Dim));
    }

    // ── Declared actions ─────────────────────────────────────────────────
    rows.push(Row::header("ACTIONS  (declared)"));
    let mut any_action = false;
    for node_ref in src.dom.nodes() {
        for (event_name, block) in &node_ref.value().events {
            let action = match block {
                EventBlock::Click { action } => action,
                EventBlock::Submit { action } => action,
            };
            any_action = true;
            rows.push(Row::plain(
                1,
                format!(
                    "{} {} -> {}",
                    node_ref.value().primitive.as_str(),
                    event_name,
                    format_action(action, interner)
                ),
                RowKind::Normal,
            ));
        }
    }
    if !any_action {
        rows.push(Row::plain(1, "(none)", RowKind::Dim));
    }

    // ── Runtime log (newest first) ───────────────────────────────────────
    rows.push(Row::header("LOG  (newest first)"));
    if src.log.events.is_empty() {
        rows.push(Row::plain(1, "(empty)", RowKind::Dim));
    }
    for entry in src.log.events.iter().rev() {
        rows.push(Row::plain(
            1,
            format!(
                "{}  {}  {}",
                src.log.fmt_ts(entry.at),
                entry.kind.tag(),
                entry.detail
            ),
            RowKind::Dim,
        ));
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Network
// ─────────────────────────────────────────────────────────────────────────────

fn network_rows(src: &InspectorSources<'_>) -> Vec<Row> {
    let mut rows = Vec::new();
    let interner = &src.store.interner;

    // ── Declared endpoints ───────────────────────────────────────────────
    rows.push(Row::header("ENDPOINTS  (declared in urls)"));
    let mut endpoints: Vec<String> = src
        .url_registry
        .iter()
        .filter_map(|(sym, ep)| {
            interner.resolve(*sym).map(|alias| {
                let kind = match ep.kind {
                    crate::parser::EndpointKind::Api => "api  ",
                    crate::parser::EndpointKind::Media => "media",
                };
                format!("{kind} {alias}  {}", ep.raw_target)
            })
        })
        .collect();
    endpoints.sort();
    if endpoints.is_empty() {
        rows.push(Row::plain(1, "(none — no network access declared)", RowKind::Dim));
    }
    for ep in endpoints {
        rows.push(Row::plain(1, ep, RowKind::Normal));
    }

    // ── Storage budget ───────────────────────────────────────────────────
    rows.push(Row::header("STORAGE"));
    rows.push(Row::plain(
        1,
        format!(
            "quota: {} / {} bytes",
            src.capability_policy.bytes_stored, src.capability_policy.quota_bytes
        ),
        RowKind::Normal,
    ));

    // ── Request log (newest first) ───────────────────────────────────────
    rows.push(Row::header("REQUESTS  (newest first)"));
    if src.log.network.is_empty() {
        rows.push(Row::plain(1, "(empty)", RowKind::Dim));
    }
    for entry in src.log.network.iter().rev() {
        let kind = match &entry.outcome {
            NetOutcome::Ok => RowKind::Good,
            NetOutcome::Failed(_) | NetOutcome::Blocked(_) => RowKind::Bad,
            NetOutcome::Pending => RowKind::Dim,
            NetOutcome::Redirect => RowKind::Accent,
        };
        let mut line = format!(
            "{}  {:<7} {:<5} {}",
            src.log.fmt_ts(entry.at),
            entry.outcome.tag(),
            entry.verb,
            entry.target
        );
        if let Some(ms) = entry.duration_ms {
            line.push_str(&format!("  {ms}ms"));
        }
        if let Some(b) = entry.bytes {
            line.push_str(&format!("  {b}B"));
        }
        rows.push(Row::plain(1, line, kind));
        match &entry.outcome {
            NetOutcome::Failed(reason) | NetOutcome::Blocked(reason) => {
                rows.push(Row::plain(
                    2,
                    crate::render::inspector::log::truncate_detail(reason),
                    RowKind::Bad,
                ));
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
pub fn format_expr(e: &Expr, interner: &StringInterner) -> String {
    match e {
        Expr::Literal(v) => match v {
            Value::String(s) => format!("\"{s}\""),
            other => format!("{other}"),
        },
        Expr::Variable(sym) => interner.resolve(*sym).unwrap_or("?").to_string(),
        Expr::BinaryOp { left, op, right } => format!(
            "{} {} {}",
            format_expr(left, interner),
            binop_str(op),
            format_expr(right, interner)
        ),
        Expr::FunctionCall { name, args } => {
            let args: Vec<String> = args.iter().map(|a| format_expr(a, interner)).collect();
            format!(
                "{}({})",
                interner.resolve(*name).unwrap_or("?"),
                args.join(", ")
            )
        }
        Expr::Let { name, value, body } => format!(
            "{} = {}; {}",
            interner.resolve(*name).unwrap_or("?"),
            format_expr(value, interner),
            format_expr(body, interner)
        ),
        Expr::Not(inner) => format!("!{}", format_expr(inner, interner)),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => format!(
            "{} ? {} : {}",
            format_expr(condition, interner),
            format_expr(then_expr, interner),
            format_expr(else_expr, interner)
        ),
        Expr::FieldAccess { base, field } => {
            format!("{}.{field}", format_expr(base, interner))
        }
    }
}

/// Renders an action back to compact Mizu-like source.
pub fn format_action(a: &Action, interner: &StringInterner) -> String {
    match a {
        Action::Assign { target, expr } => {
            format!("{target} = {}", format_expr(expr, interner))
        }
        Action::Eval(e) => format_expr(e, interner),
        Action::Navigate { url } => format!("navigate {}", format_expr(url, interner)),
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
        let expr =
            crate::parser::logic::parse_expr_standalone("count > 4 && !busy", &mut interner)
                .unwrap();
        assert_eq!(format_expr(&expr, &interner), "count > 4 && !busy");
    }

    #[test]
    fn format_action_assign() {
        let mut interner = StringInterner::new();
        let action =
            crate::parser::logic::parse_action("count = count + 1", &mut interner).unwrap();
        assert_eq!(format_action(&action, &interner), "count = count + 1");
    }

    #[test]
    fn node_label_shows_events_and_class() {
        use std::collections::HashMap;
        let mut attributes = HashMap::new();
        attributes.insert("class".to_string(), "card".to_string());
        let mut events = HashMap::new();
        let mut it = StringInterner::new();
        let action = crate::parser::logic::parse_action("x = 1", &mut it).unwrap();
        events.insert("click".to_string(), EventBlock::Click { action });
        let node = MizuNode {
            primitive: crate::parser::Primitive::Button,
            attributes,
            events,
            iterator_context: None,
            conditional_classes: Vec::new(),
        };
        let label = node_label(&node, None);
        assert!(label.contains("button"));
        assert!(label.contains(".card"));
        assert!(label.contains("[click]"));
    }
}
