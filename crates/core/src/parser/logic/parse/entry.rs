//! Top-level entry points: `parse_logic` (the `logic_block` grammar),
//! `parse_root_timers`, `parse_expr_standalone`, and `check_dag` (the
//! anti-recursion topological-sort check run over the function call graph).

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol};

use super::super::ast::{ExprTree, MizuFunction, RootTimer, TimerInterval};
use super::super::comp::collect_calls;
use super::super::lexer::{Cursor, assert_cursor_empty, leading_spaces, lex};
use super::action::parse_action;
use super::expr::parse_expr_tree;
use super::functions::parse_function_block;

/// Parses the `logic_block` produced by [`super::split_source`] into a
/// validated, recursion-free `HashMap` of function definitions.
///
/// ## Grammar (excerpt)
///
/// ```text
/// // Inline form
/// vat(price: num) : price * 1.22
///
/// // Multi-line form
/// total(price: num, qty: num)
///     netto = price * qty
///     netto * 1.22
/// ```
///
/// ## Errors
///
/// * [`MizuError::ParseError`] — for any syntactic violation, unknown type
///   annotation, or detected recursion cycle.
///
/// # Examples
///
/// ```
/// use mizu_core::parser::logic::parse_logic;
/// use mizu_core::core::types::StringInterner;
/// let src = "    vat(p: num) : p * 1.22\n";
/// let mut interner = StringInterner::new();
/// let fns = parse_logic(src, &mut interner).unwrap();
/// assert!(!fns.is_empty());
/// assert!(interner.get("vat").is_some());
/// ```
pub fn parse_logic(
    logic_content: &str,
    interner: &mut StringInterner,
) -> Result<FxHashMap<Symbol, MizuFunction>, MizuError> {
    // ── Group lines into per-function slices ─────────────────────────────
    // A function definition starts at a line whose leading indent equals the
    // baseline of the block (the minimum non-empty-line indent).
    let all_lines: Vec<&str> = logic_content.lines().collect();

    // Find baseline indentation.
    let baseline = all_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_spaces(l))
        .min()
        .unwrap_or(0);

    // Collect function definition groups.
    let mut groups: Vec<Vec<&str>> = Vec::new();
    let mut current_group: Vec<&str> = Vec::new();

    for line in &all_lines {
        if line.trim().is_empty() {
            continue;
        }
        let indent = leading_spaces(line);
        // Skip root-level `timer` and `comp` declarations — handled by dedicated parsers.
        if indent == baseline {
            let stripped = &line[baseline.min(line.len())..];
            if stripped.trim_start().starts_with("timer ") || stripped.trim() == "timer" {
                if !current_group.is_empty() {
                    groups.push(current_group.clone());
                    current_group.clear();
                }
                continue;
            }
            if stripped.trim_start().starts_with("comp ") {
                if !current_group.is_empty() {
                    groups.push(current_group.clone());
                    current_group.clear();
                }
                continue;
            }
        }
        if indent == baseline && !current_group.is_empty() {
            groups.push(current_group.clone());
            current_group.clear();
        }
        // Strip the baseline indent from each line before handing to the parser.
        current_group.push(&line[baseline.min(line.len())..]);
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    // ── Parse each function group ────────────────────────────────────────
    let mut functions: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    for group in &groups {
        let (name, func) = parse_function_block(group, interner)?;
        let sym = interner.get_or_intern(&name);
        functions.insert(sym, func);
    }

    // ── Anti-recursion DAG check ─────────────────────────────────────────
    check_dag(&functions)?;

    Ok(functions)
}

/// Maximum number of root `timer` declarations a single document may carry.
///
/// Every declaration is an independent, self-rearming event source: the window
/// loop dispatches one `UiEvent::RootTimer` per due timer per tick, each of
/// which costs the logic worker a full action execution plus a computed-binding
/// recompute. `MAX_INSTRUCTIONS` bounds one execution; nothing bounds how many
/// executions a document may demand per second, so without a ceiling here the
/// document controls that number directly — and the 16 ms interval floor
/// applies per timer, not in aggregate.
///
/// 64 is far beyond any legitimate document (real ones declare a handful:
/// a clock, a poll, an animation) while keeping the worst-case dispatch rate
/// bounded by a constant rather than by document length. Exceeding it is a
/// parse error rather than a silent truncation, matching how every other
/// over-limit input in this crate is handled — a document whose timers were
/// quietly dropped would appear to work while behaving differently from what
/// its author wrote.
///
/// An unmeasured starting value, overridable for a single run via
/// `MIZU_MAX_ROOT_TIMERS` (see the module doc on [`crate::core::config`]).
pub static MAX_ROOT_TIMERS: std::sync::LazyLock<usize> =
    std::sync::LazyLock::new(|| crate::core::config::env_override("MIZU_MAX_ROOT_TIMERS", 64));

/// Parses all `timer <interval> -> <action>` declarations from a `logic_block`.
///
/// Timer lines are silently skipped by [`parse_logic`]; this function handles
/// them as a second, independent pass over the same content.
///
/// # Errors
///
/// Returns [`MizuError::ParseError`] if a declaration is malformed, or if the
/// document declares more than [`MAX_ROOT_TIMERS`] of them.
///
/// ## Syntax
///
/// ```text
/// timer 500ms  -> count = count + 1
/// timer 1000ms -> refresh()
/// timer tick   -> tick = tick + 1   // variable interval
/// ```
///
/// The interval suffix `ms` is stripped; if the value is a plain number it is
/// treated as [`TimerInterval::Millis`], otherwise as [`TimerInterval::Variable`].
pub fn parse_root_timers(
    logic_content: &str,
    interner: &mut StringInterner,
) -> Result<Vec<RootTimer>, MizuError> {
    let mut timers: Vec<RootTimer> = Vec::new();

    let all_lines: Vec<&str> = logic_content.lines().collect();

    // Find baseline indentation (same as in parse_logic).
    let baseline = all_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_spaces(l))
        .min()
        .unwrap_or(0);

    for raw_line in &all_lines {
        if raw_line.trim().is_empty() {
            continue;
        }
        let indent = leading_spaces(raw_line);
        if indent != baseline {
            continue;
        }
        let stripped = &raw_line[baseline.min(raw_line.len())..].trim_end();
        let Some(rest) = stripped.strip_prefix("timer ") else {
            continue;
        };

        // Split on `->` to get `interval_str` and `action_str`.
        let arrow_pos = rest.find("->").ok_or_else(|| {
            MizuError::ParseError(format!("timer declaration missing `->`: `{stripped}`"))
        })?;
        let interval_str = rest[..arrow_pos].trim();
        let action_str = rest[arrow_pos + 2..].trim();

        // Parse the root-timer interval.
        // Accepted forms: `500ms`, `60s`, `1s`, bare integer (ms), variable name.
        let interval = if let Some(ms_str) = interval_str.strip_suffix("ms") {
            match ms_str.trim().parse::<u64>() {
                Ok(ms) => TimerInterval::Millis(ms),
                Err(_) => TimerInterval::Variable(ms_str.trim().to_string()),
            }
        } else if let Some(s_str) = interval_str.strip_suffix('s') {
            match s_str.trim().parse::<f64>() {
                Ok(s_val) => TimerInterval::Millis((s_val * 1000.0) as u64),
                Err(_) => TimerInterval::Variable(s_str.trim().to_string()),
            }
        } else {
            // Bare number or variable name without suffix.
            match interval_str.parse::<u64>() {
                Ok(ms) => TimerInterval::Millis(ms),
                Err(_) => TimerInterval::Variable(interval_str.to_string()),
            }
        };

        // Checked before parsing the action, so an over-long timer block costs
        // one comparison per extra line rather than a full action parse each.
        if timers.len() == *MAX_ROOT_TIMERS {
            return Err(MizuError::ParseError(format!(
                "document declares more than {} root `timer` declarations; \
                 each one is an independent event source and the total dispatch \
                 rate must stay bounded",
                *MAX_ROOT_TIMERS
            )));
        }

        let action = parse_action(action_str, interner)?;
        timers.push(RootTimer { interval, action });
    }

    Ok(timers)
}

/// Parses a standalone expression string into an [`ExprTree`].
///
/// Used by the layout parser to parse conditional class conditions
/// (e.g., the `flag` part of `class active if flag`).
///
/// # Errors
///
/// Returns [`MizuError::ParseError`] if the input is syntactically invalid
/// or if tokens remain unconsumed after the expression.
pub fn parse_expr_standalone(
    expr: &str,
    interner: &mut StringInterner,
) -> Result<ExprTree, MizuError> {
    let tokens = lex(expr)?;
    let mut cursor = Cursor::new(&tokens);
    let tree = parse_expr_tree(&mut cursor, interner)?;
    assert_cursor_empty(&cursor, "")?;
    Ok(tree)
}

/// Runs Kahn's BFS topological sort over the function call-graph to detect
/// cycles (i.e., recursion).
///
/// # Errors
///
/// Returns [`MizuError::ParseError`] if any cycle is detected.
fn check_dag(functions: &FxHashMap<Symbol, MizuFunction>) -> Result<(), MizuError> {
    let mut edges: FxHashMap<Symbol, FxHashSet<Symbol>> = FxHashMap::default();
    let mut in_degree: FxHashMap<Symbol, usize> = FxHashMap::default();

    let function_names: FxHashSet<Symbol> = functions.keys().copied().collect();

    for &sym in functions.keys() {
        edges.entry(sym).or_default();
        in_degree.entry(sym).or_insert(0);
    }

    for (&sym, func) in functions {
        let mut calls: FxHashSet<Symbol> = FxHashSet::default();
        collect_calls(
            func.body.root(),
            &func.body.arena,
            &mut calls,
            &function_names,
        );

        for callee in calls {
            if functions.contains_key(&callee) {
                edges.entry(sym).or_default().insert(callee);
                *in_degree.entry(callee).or_insert(0) += 1;
            }
        }
    }

    // Kahn's BFS: start with all nodes of in-degree 0.
    let mut queue: VecDeque<Symbol> = in_degree
        .iter()
        .filter_map(|(&sym, &deg)| if deg == 0 { Some(sym) } else { None })
        .collect();

    let mut visited = 0usize;

    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(neighbours) = edges.get(&node) {
            let neighbours: Vec<Symbol> = neighbours.iter().copied().collect();
            for neighbour in neighbours {
                let deg = in_degree.entry(neighbour).or_insert(0);
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.push_back(neighbour);
                }
            }
        }
    }

    if visited != functions.len() {
        return Err(MizuError::ParseError(
            "Recursion and infinite loops are strictly forbidden: \
             a cycle was detected in the function call graph"
                .to_owned(),
        ));
    }

    Ok(())
}
