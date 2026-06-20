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

#![forbid(unsafe_code)]

use ego_tree::{NodeId, Tree};
use std::collections::HashMap;

use crate::core::errors::MizuError;
use crate::core::types::StringInterner;
use crate::parser::logic::{
    Action, Expr, find_side_effect_call, parse_action_with_urls, parse_expr_standalone,
};
use crate::parser::urls::{EndpointKind, UrlRegistry};


/// The valid structural primitives in Mizu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    /// Root window node.
    Window,
    /// Structural container.
    Box,
    /// Text leaf or block.
    Text,
    /// Interactive button.
    Button,
    /// Input field.
    Input,
    /// Media leaf.
    Image,
    /// Rich text markdown block.
    Markdown,
    /// List iterator: `each item in list`.
    Each,
    /// A form container that batches input values and submits them atomically.
    /// Recognised attributes: `submit -> action`.
    Form,
}

impl Primitive {
    /// Returns the string representation of the primitive.
    pub fn as_str(&self) -> &'static str {
        match self {
            Primitive::Window => "window",
            Primitive::Box => "box",
            Primitive::Text => "text",
            Primitive::Button => "button",
            Primitive::Input => "input",
            Primitive::Image => "image",
            Primitive::Markdown => "markdown",
            Primitive::Each => "each",
            Primitive::Form => "form",
        }
    }
}

/// Represents a single node in the Mizu DOM tree.
#[derive(Debug, Clone, PartialEq)]
pub struct MizuNode {
    /// The primitive type of this node.
    pub primitive: Primitive,
    /// Inline attributes mapping (e.g. `class -> .card`).
    pub attributes: HashMap<String, String>,
    /// Behavioral event blocks mapping (e.g. `click -> EventBlock::Click`).
    pub events: HashMap<String, EventBlock>,
    /// For `each` nodes: `(item_variable, list_name)`, e.g. `each item in list` → `("item", "list")`.
    pub iterator_context: Option<(String, String)>,
    /// Runtime-evaluated conditional classes (applied in declaration order after the base class).
    pub conditional_classes: Vec<ConditionalClass>,
}

/// Represents a timer interval, either literal ms or a variable pointer.
#[derive(Debug, Clone, PartialEq)]
pub enum Interval {
    /// A constant interval in milliseconds.
    Literal(u64),
    /// A variable identifier whose value specifies milliseconds.
    Variable(String),
}

/// A behavioral event block attached to a node.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBlock {
    /// Triggered on click (e.g. `click -> Redirect("/home")`).
    Click {
        /// Action payload or destination.
        action: Action,
    },
    /// Triggered on form submit (e.g. `submit -> SendForm`).
    Submit {
        /// Action payload to execute on submission.
        action: Action,
    },
    /// Triggered on a recurring timer (e.g. `every 500ms -> count = count + 1`).
    Every {
        /// The time interval between triggers.
        interval: Interval,
        /// The action to execute on each tick.
        action: Action,
    },
}

/// A runtime-evaluated class binding declared as a child line of a node.
///
/// Syntax: `class <name> if <boolean-expr>`
///
/// If `condition` evaluates to `true` on a given paint frame, `class_name` is
/// added to the node's active class set for that frame (after the static base
/// class).  Multiple conditional classes may be active simultaneously.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionalClass {
    /// CSS class name to activate when the condition is truthy.
    pub class_name: String,
    /// Pure boolean expression evaluated at runtime (no side effects allowed).
    pub condition: Expr,
}


/// Returns the number of leading space characters in `line`.
#[inline]
fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Splits a string on its first whitespace boundary into `(first_word, rest)`.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    if let Some(pos) = s.find(|c: char| c.is_whitespace()) {
        (s[..pos].trim(), s[pos..].trim())
    } else {
        (s, "")
    }
}

/// Parses a double-quoted string from the start of `s`, resolving escape sequences.
/// Returns the parsed string and the remaining unparsed slice of `s`.
fn parse_quoted_string(s: &str) -> Result<(String, &str), MizuError> {
    if !s.starts_with('"') {
        return Err(MizuError::ParseError(
            "Expected opening double quote".to_string(),
        ));
    }

    let mut content = String::new();
    let mut chars = s[1..].char_indices();

    while let Some((idx, c)) = chars.next() {
        if c == '"' {
            let end_idx = 1 + idx + 1; // 1 for the initial quote, idx is offset in s[1..], + 1 for length of '"' (which is 1 byte)
            let rest = &s[end_idx..];
            return Ok((content, rest));
        } else if c == '\\' {
            if let Some((_, next_c)) = chars.next() {
                content.push(next_c);
            }
        } else {
            content.push(c);
        }
    }

    Err(MizuError::ParseError(
        "Unterminated double-quoted string".to_string(),
    ))
}

/// Parses a time interval string into an `Interval` enum.
fn parse_interval(s: &str) -> Interval {
    if let Some(num_str) = s.strip_suffix("ms") {
        if let Ok(ms) = num_str.parse::<u64>() {
            return Interval::Literal(ms);
        }
    } else if let Some(num_str) = s.strip_suffix('s')
        && let Ok(s_val) = num_str.parse::<f64>()
    {
        return Interval::Literal((s_val * 1000.0) as u64);
    }
    Interval::Variable(s.to_string())
}

/// Layout-only attribute keywords that are never valid as standalone tokens
/// inside a Mizu logic expression.  Detecting them early in the action string
/// produces a clear diagnostic instead of silent token loss.
///
/// The list is intentionally conservative — it excludes ambiguous words like
/// `type` or `width` that could legitimately be Mizu variable names.
const LAYOUT_ATTR_KEYWORDS: &[&str] = &["class", "id", "src", "href", "alt"];

/// Scans `action_str` for layout attribute keywords appearing as complete
/// whitespace-delimited words.  Returns the first offending keyword if found.
///
/// This is a defence-in-depth companion to the cursor-exhaustion check in
/// `parse_action`: it fires earlier and produces a more actionable error
/// message pointing to the specific keyword.
fn find_trailing_layout_keyword(action_str: &str) -> Option<&'static str> {
    for word in action_str.split_whitespace() {
        if let Some(&kw) = LAYOUT_ATTR_KEYWORDS.iter().find(|&&kw| kw == word) {
            return Some(kw);
        }
    }
    None
}

/// Parses inline attribute key-value pairs (e.g. `type "text" class .input`) and inline events.
pub type AttrsAndEvents = (HashMap<String, String>, HashMap<String, EventBlock>);
fn parse_attributes_and_events(
    mut s: &str,
    interner: &mut StringInterner,
) -> Result<AttrsAndEvents, MizuError> {
    let mut attrs = HashMap::new();
    let mut events = HashMap::new();
    loop {
        s = s.trim_start();
        if s.is_empty() {
            break;
        }

        // Parse key
        let key_end = s.find(|c: char| c.is_whitespace() || c == '=');
        let (key, rest) = if let Some(end) = key_end {
            (&s[..end], &s[end..])
        } else {
            (s, "")
        };

        if key.is_empty() {
            return Err(MizuError::ParseError("Expected attribute key".to_string()));
        }

        // Check if this key is an event keyword AND followed by `->`
        if key == "bind" {
            return Err(MizuError::ParseError(
                "bind is no longer supported: use `class name if condition` in the style block to control visibility".to_string(),
            ));
        } else if key == "download" {
            return Err(MizuError::ParseError(
                "download -> alias is no longer supported; use click -> download(alias)".to_string(),
            ));
        } else if key == "click" || key == "submit" {
            let rest_trimmed = rest.trim_start();
            if let Some(stripped) = rest_trimmed.strip_prefix("->") {
                let action_str = stripped.trim();
                // Pre-check: catch layout keywords before the expression parser sees
                // them.  The expression parser's cursor-exhaustion check is the
                // canonical backstop, but this fires first and gives a clearer hint.
                if let Some(kw) = find_trailing_layout_keyword(action_str) {
                    return Err(MizuError::ParseError(format!(
                        "layout attribute `{kw}` found inside `{key} ->` action\n  \
                         hint: `{key} ->` consumes the entire line — move `{kw}` to \
                         the element line:\n    \
                         bad:  button {key} -> action {kw} \"value\"\n    \
                         good: button {kw} \"value\"\n    \
                         good:     {key} -> action"
                    )));
                }
                let event = match key {
                    "click" => EventBlock::Click {
                        action: crate::parser::logic::parse_action(action_str, interner)?,
                    },
                    "submit" => EventBlock::Submit {
                        action: crate::parser::logic::parse_action(action_str, interner)?,
                    },
                    _ => {
                        return Err(MizuError::ParseError(
                            "internal: unexpected event keyword".to_string(),
                        ));
                    }
                };
                events.insert(key.to_string(), event);
                break; // Action consumes the rest of the line
            }
        } else if key == "every" {
            let rest_trimmed = rest.trim_start();
            if let Some(arrow_pos) = rest_trimmed.find("->") {
                let interval_str = rest_trimmed[..arrow_pos].trim();
                let action_str = rest_trimmed[arrow_pos + 2..].trim();
                // Same pre-check for `every` actions.
                if let Some(kw) = find_trailing_layout_keyword(action_str) {
                    return Err(MizuError::ParseError(format!(
                        "layout attribute `{kw}` found inside `every ->` action\n  \
                         hint: move `{kw}` to the element line"
                    )));
                }
                events.insert(
                    "every".to_string(),
                    EventBlock::Every {
                        interval: parse_interval(interval_str),
                        action: crate::parser::logic::parse_action(action_str, interner)?,
                    },
                );
                break; // Action consumes the rest of the line
            }
        }

        // Validate key format
        if !key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return Err(MizuError::ParseError(format!(
                "Invalid attribute key `{key}`"
            )));
        }

        let mut rest = rest.trim_start();
        if rest.starts_with('=') {
            rest = rest[1..].trim_start();
        }
        if rest.is_empty() {
            return Err(MizuError::ParseError(format!(
                "Attribute `{key}` is missing a value"
            )));
        }

        // Parse value
        let value: String;
        let rest_s: &str;
        if rest.starts_with('"') {
            let (val, remaining) = parse_quoted_string(rest)?;
            value = val;
            rest_s = remaining;
        } else {
            let val_end = rest.find(|c: char| c.is_whitespace());
            if let Some(end) = val_end {
                value = rest[..end].to_string();
                rest_s = &rest[end..];
            } else {
                value = rest.to_string();
                rest_s = "";
            }
        }

        if value.is_empty() {
            return Err(MizuError::ParseError(format!(
                "Attribute `{key}` is missing a value"
            )));
        }

        let final_value = if key == "class" && value.starts_with('.') {
            value[1..].to_string()
        } else {
            value
        };

        attrs.insert(key.to_string(), final_value);
        s = rest_s;
    }
    Ok((attrs, events))
}

/// Internal helper to parse a primitive name, its optional inline text, and attributes.
/// Returns the parsed `MizuNode`, a boolean indicating if it is a markdown block,
/// and an optional inline text string (to be turned into a child Text node by the caller).
fn parse_primitive_and_attrs(
    content: &str,
    line_num: usize,
    interner: &mut StringInterner,
) -> Result<(MizuNode, bool, Option<String>), MizuError> {
    let (prim_name, rest) = split_first_word(content);
    let prim_lower = prim_name.to_lowercase();

    // Handle `each` separately — it carries iterator_context, not attributes
    if prim_lower == "each" {
        let words: Vec<&str> = rest.split_whitespace().collect();
        if words.len() == 3 && words[1] == "in" {
            let item_var = words[0].to_string();
            let list_name = words[2].to_string();
            let node = MizuNode {
                primitive: Primitive::Each,
                attributes: HashMap::new(),
                events: HashMap::new(),
                iterator_context: Some((item_var, list_name)),
                conditional_classes: Vec::new(),
            };
            return Ok((node, false, None));
        } else {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: invalid `each` syntax: expected `each <item> in <list>`, got `each {rest}`"
            )));
        }
    }

    let primitive = match prim_lower.as_str() {
        "window" => Primitive::Window,
        "box" => Primitive::Box,
        "t" | "text" => Primitive::Text,
        "button" => Primitive::Button,
        "input" => Primitive::Input,
        "image" => Primitive::Image,
        "markdown" => Primitive::Markdown,
        "form" => Primitive::Form,
        _ => {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: Illegal primitive name `{prim_name}`"
            )));
        }
    };

    let mut inline_text = None;
    let mut attrs_str = rest;
    let mut is_markdown = false;

    if primitive == Primitive::Markdown && rest.starts_with("\"\"\"") {
        is_markdown = true;
        attrs_str = "";
    } else if rest.starts_with('"') {
        let (text, remaining) = parse_quoted_string(rest)
            .map_err(|e| MizuError::ParseError(format!("line {line_num}: {e}")))?;
        inline_text = Some(text);
        attrs_str = remaining;
    }

    let (mut attributes, events) = parse_attributes_and_events(attrs_str, interner)
        .map_err(|e| MizuError::ParseError(format!("line {line_num}: {e}")))?;

    // For Text nodes, store inline text directly in "content" attribute (no child node).
    // For other primitives, the inline_text is returned to the caller which will create a child.
    let child_inline_text = if primitive == Primitive::Text {
        if let Some(text) = inline_text {
            attributes.insert("content".to_string(), text);
        }
        None
    } else {
        inline_text
    };

    Ok((
        MizuNode {
            primitive,
            attributes,
            events,
            iterator_context: None,
            conditional_classes: Vec::new(),
        },
        is_markdown,
        child_inline_text,
    ))
}


/// Parses the `layout_block` produced by [`super::split_source`] into a
/// hierarchical, arena-based DOM tree.
///
/// When `url_registry` is `Some`, media compile-time guards are applied:
/// any `image src: alias` or `download -> alias` whose alias does not exist in
/// the registry as [`EndpointKind::Media`] is a hard compile error.
///
/// # Errors
///
/// * [`MizuError::ParseError`] — if structural constraints are violated (e.g. root node
///   is not `window`, multiple roots are defined, or bad syntax), or if a media
///   alias is undeclared or points to a non-media endpoint.
pub fn parse_layout(
    layout_content: &str,
    interner: &mut StringInterner,
) -> Result<Tree<MizuNode>, MizuError> {
    parse_layout_with_urls(layout_content, interner, None, false)
}

/// Like [`parse_layout`] but accepts an optional [`UrlRegistry`] for media alias validation
/// and an `is_remote_origin` flag that blocks `file://` asset references at parse time.
pub fn parse_layout_with_urls(
    layout_content: &str,
    interner: &mut StringInterner,
    url_registry: Option<&UrlRegistry>,
    is_remote_origin: bool,
) -> Result<Tree<MizuNode>, MizuError> {
    let all_lines: Vec<&str> = layout_content.lines().collect();

    // Filter out blank or whitespace-only lines.
    let non_empty_lines: Vec<(usize, &str)> = all_lines
        .into_iter()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();

    if non_empty_lines.is_empty() {
        return Err(MizuError::ParseError(
            "Layout block cannot be empty".to_string(),
        ));
    }

    let mut lines = non_empty_lines.into_iter().peekable();

    let (first_line_idx, first_line) = match lines.next() {
        Some(val) => val,
        None => {
            return Err(MizuError::ParseError(
                "Layout block cannot be empty".to_string(),
            ));
        }
    };
    let baseline = leading_spaces(first_line);
    let trimmed_first = first_line.trim();

    let (first_node, _, first_inline_text) =
        parse_primitive_and_attrs(trimmed_first, first_line_idx + 1, interner)?;
    if first_node.primitive != Primitive::Window {
        return Err(MizuError::ParseError(format!(
            "line {}: root element must be `window`, found `{}`",
            first_line_idx + 1,
            trimmed_first.split_whitespace().next().unwrap_or("")
        )));
    }

    let mut tree = Tree::new(first_node);
    let root_id = tree.root_mut().id();

    // If the root node had inline text, add it as a child Text node
    if let Some(text_str) = first_inline_text {
        let text_node = MizuNode {
            primitive: Primitive::Text,
            attributes: {
                let mut m = HashMap::new();
                m.insert("content".to_string(), text_str);
                m
            },
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        };
        if let Some(mut n) = tree.get_mut(root_id) {
            n.append(text_node);
        }
    }

    let mut stack: Vec<(usize, NodeId)> = vec![(baseline, root_id)];

    while let Some((line_idx, line)) = lines.next() {
        let indent = leading_spaces(line);
        let trimmed = line.trim();

        // ── Check for Event Blocks ──────────────────────────────────────────
        let (first_word, rest) = split_first_word(trimmed);
        if first_word == "bind" {
            return Err(MizuError::ParseError(
                "bind is no longer supported: use `class name if condition` in the style block to control visibility".to_string(),
            ));
        } else if first_word == "download" {
            return Err(MizuError::ParseError(
                "download -> alias is no longer supported; use click -> download(alias)".to_string(),
            ));
        } else if first_word == "click" || first_word == "submit" || first_word == "every" {
            let arrow_pos = rest.find("->").ok_or_else(|| {
                MizuError::ParseError(format!(
                    "line {}: Event `{first_word}` is missing the `->` arrow syntax",
                    line_idx + 1
                ))
            })?;
            let value = rest[arrow_pos + 2..].trim();
            if value.is_empty() {
                return Err(MizuError::ParseError(format!(
                    "line {}: Event `{first_word}` is missing its action or variable payload",
                    line_idx + 1
                )));
            }

            let event = match first_word {
                "click" => EventBlock::Click {
                    action: parse_action_with_urls(value, interner, url_registry)?,
                },
                "submit" => EventBlock::Submit {
                    action: parse_action_with_urls(value, interner, url_registry)?,
                },
                "every" => {
                    let interval_str = rest[..arrow_pos].trim();
                    if interval_str.is_empty() {
                        return Err(MizuError::ParseError(format!(
                            "line {}: Event `every` is missing its interval",
                            line_idx + 1
                        )));
                    }
                    EventBlock::Every {
                        interval: parse_interval(interval_str),
                        action: parse_action_with_urls(value, interner, url_registry)?,
                    }
                }
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {}: internal: unexpected event keyword `{first_word}`",
                        line_idx + 1
                    )));
                }
            };

            // Pop stack elements where stack_indent >= indent.
            while let Some(&(stack_indent, _)) = stack.last() {
                if stack_indent >= indent {
                    stack.pop();
                } else {
                    break;
                }
            }

            let parent_id = match stack.last() {
                Some(&(_, id)) => id,
                None => {
                    return Err(MizuError::ParseError(format!(
                        "line {}: Event `{first_word}` has no parent node",
                        line_idx + 1
                    )));
                }
            };

            let mut parent_node_mut = tree.get_mut(parent_id).ok_or_else(|| {
                MizuError::ParseError(format!(
                    "line {}: Internal error: parent node not found in tree",
                    line_idx + 1
                ))
            })?;
            parent_node_mut
                .value()
                .events
                .insert(first_word.to_string(), event);

            continue;
        }

        // ── Conditional class: `class <name> if <expr>` ────────────────────
        if first_word == "class" {
            let (class_name, rest2) = split_first_word(rest);
            if class_name.is_empty() {
                return Err(MizuError::ParseError(format!(
                    "line {}: `class` child line is missing the class name",
                    line_idx + 1
                )));
            }
            let (if_kw, expr_str) = split_first_word(rest2);
            if if_kw != "if" {
                return Err(MizuError::ParseError(format!(
                    "line {}: conditional class `class {class_name}` is missing the `if` keyword",
                    line_idx + 1
                )));
            }
            if expr_str.is_empty() {
                return Err(MizuError::ParseError(format!(
                    "line {}: conditional class `class {class_name} if` is missing the condition",
                    line_idx + 1
                )));
            }
            let condition = parse_expr_standalone(expr_str, interner).map_err(|e| {
                MizuError::ParseError(format!(
                    "line {}: conditional class expression error: {e}",
                    line_idx + 1
                ))
            })?;
            if let Some(bad_fn) = find_side_effect_call(&condition, interner) {
                return Err(MizuError::ParseError(format!(
                    "line {}: conditional class condition must be pure — \
                     `{bad_fn}` is a side-effecting call",
                    line_idx + 1
                )));
            }

            while let Some(&(stack_indent, _)) = stack.last() {
                if stack_indent >= indent {
                    stack.pop();
                } else {
                    break;
                }
            }
            let parent_id = match stack.last() {
                Some(&(_, id)) => id,
                None => {
                    return Err(MizuError::ParseError(format!(
                        "line {}: `class {class_name} if` has no parent node",
                        line_idx + 1
                    )));
                }
            };
            tree.get_mut(parent_id)
                .ok_or_else(|| {
                    MizuError::ParseError(format!(
                        "line {}: Internal error: parent node not found",
                        line_idx + 1
                    ))
                })?
                .value()
                .conditional_classes
                .push(ConditionalClass {
                    class_name: class_name.to_string(),
                    condition,
                });
            continue;
        }

        // ── Parse Primitive Nodes ───────────────────────────────────────────
        let (mut node, is_markdown, inline_text) =
            parse_primitive_and_attrs(trimmed, line_idx + 1, interner)?;

        // ── Compile-time media guard ────────────────────────────────────────
        // If a URL registry is provided, validate that `image src: alias`
        // points to a declared `media` endpoint.
        // (`download(alias)` is validated in `parse_action_with_urls` at action parse time.)
        #[allow(clippy::collapsible_if)]
        if let Some(registry) = url_registry {
            if node.primitive == Primitive::Image
                && let Some(src_alias) = node.attributes.get("src")
            {
                // Remote-origin documents must never embed local file:// assets.
                // Catch this at parse time so the error appears with the source line number
                // rather than as a runtime network failure.
                if is_remote_origin && src_alias.starts_with("file://") {
                    return Err(MizuError::ParseError(format!(
                        "line {}: SecurityViolation: remote documents cannot embed \
                         local file:// assets (src: {src_alias})",
                        line_idx + 1
                    )));
                }

                // Direct paths (containing `.` or `/`, or starting with a URL scheme)
                // are used as-is by the renderer — only symbolic aliases need registry validation.
                let is_direct_path = src_alias.contains('/')
                    || src_alias.contains('.')
                    || src_alias.starts_with("mizu://")
                    || (src_alias.starts_with("file://") && !is_remote_origin);
                if !is_direct_path {
                    let sym = interner.get_or_intern(src_alias);
                    match registry.get(&sym) {
                        None => {
                            return Err(MizuError::ParseError(format!(
                                "line {}: image `src` alias `{src_alias}` is not declared \
                                 in the `urls` block",
                                line_idx + 1
                            )));
                        }
                        Some(ep) if ep.kind != EndpointKind::Media => {
                            return Err(MizuError::ParseError(format!(
                                "line {}: image `src` alias `{src_alias}` points to an \
                                 `api` endpoint, not a `media` endpoint",
                                line_idx + 1
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }

        if is_markdown {
            let mut markdown_content = String::new();
            let (_, rest) = split_first_word(trimmed); // prim_name is "markdown", rest starts with `"""`
            let inline_rest = &rest[3..];
            let mut found_close = false;

            if let Some(close_pos) = inline_rest.find("\"\"\"") {
                markdown_content.push_str(&inline_rest[..close_pos]);
                found_close = true;
            } else {
                markdown_content.push_str(inline_rest);
                markdown_content.push('\n');
                for (_, next_line) in lines.by_ref() {
                    if let Some(close_pos) = next_line.find("\"\"\"") {
                        markdown_content.push_str(&next_line[..close_pos]);
                        found_close = true;
                        break;
                    } else {
                        markdown_content.push_str(next_line);
                        markdown_content.push('\n');
                    }
                }
            }

            if !found_close {
                return Err(MizuError::ParseError(format!(
                    "line {}: Unterminated markdown triple-quoted block",
                    line_idx + 1
                )));
            }

            // Store markdown content in the "content" attribute
            node.attributes
                .insert("content".to_string(), markdown_content);
        }

        // Pop stack elements where stack_indent >= indent.
        while let Some(&(stack_indent, _)) = stack.last() {
            if stack_indent >= indent {
                stack.pop();
            } else {
                break;
            }
        }

        let parent_id = match stack.last() {
            Some(&(_, id)) => id,
            None => {
                let prim_name = trimmed.split_whitespace().next().unwrap_or("");
                return Err(MizuError::ParseError(format!(
                    "line {}: Node `{}` has no parent (multiple root elements are not allowed)",
                    line_idx + 1,
                    prim_name
                )));
            }
        };

        let new_id = tree
            .get_mut(parent_id)
            .ok_or_else(|| {
                MizuError::ParseError(format!(
                    "line {}: Internal error: parent node not found in tree",
                    line_idx + 1
                ))
            })?
            .append(node)
            .id();

        // If the node had inline text, add it as a child Text node
        if let Some(text_str) = inline_text {
            let text_node = MizuNode {
                primitive: Primitive::Text,
                attributes: {
                    let mut m = HashMap::new();
                    m.insert("content".to_string(), text_str);
                    m
                },
                events: HashMap::new(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            };
            if let Some(mut node_mut) = tree.get_mut(new_id) {
                node_mut.append(text_node);
            }
        }

        stack.push((indent, new_id));
    }

    Ok(tree)
}


#[cfg(test)]
mod tests {
    use super::{
        EventBlock, Interval, Primitive, parse_interval, parse_layout, parse_layout_with_urls,
    };
    use crate::core::errors::MizuError;
    use crate::core::types::StringInterner;
    use crate::parser::logic::parse_action;
    use crate::parser::urls::UrlRegistry;

    #[test]
    fn media_guard_rejects_undeclared_image_alias() {
        // Mirrors the navigation path: an `image src` alias that is not present
        // in the (here empty) `urls` registry must be rejected at parse time.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"undeclared_alias\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("undeclared_alias") && msg.contains("not declared"),
                    "error should name the undeclared media alias: {msg}"
                );
            }
            other => panic!("expected ParseError for undeclared media alias, got: {other:?}"),
        }
    }

    #[test]
    fn media_guard_skipped_without_registry() {
        // Without a registry (`None`), the guard must not fire — `parse_layout`
        // keeps its lenient behaviour.
        let mut interner = StringInterner::new();
        let layout = "window \"App\"\n    image src \"anything\"\n";
        let result = parse_layout(layout, &mut interner);
        assert!(result.is_ok(), "no registry → no media guard: {result:?}");
    }

    #[test]
    fn direct_path_src_skips_media_guard() {
        // A plain filename with an extension is a direct path, not an alias —
        // the registry guard must not fire even when a registry is present.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"test.png\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        assert!(
            result.is_ok(),
            "direct filename with extension must bypass guard: {result:?}"
        );
    }

    #[test]
    fn direct_path_with_slash_skips_guard() {
        // A path containing `/` is always a direct path — guard skipped.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"./img/logo.png\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        assert!(
            result.is_ok(),
            "relative path with slash must bypass guard: {result:?}"
        );
    }

    #[test]
    fn file_url_src_blocked_from_remote_origin() {
        // When is_remote_origin=true, file:// in `image src` must be rejected
        // at parse time with a SecurityViolation message.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"file:///etc/passwd\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), true);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("SecurityViolation"),
                    "error must mention SecurityViolation: {msg}"
                );
                assert!(
                    msg.contains("file://"),
                    "error must contain the offending URL prefix: {msg}"
                );
            }
            other => panic!("expected ParseError(SecurityViolation), got: {other:?}"),
        }
    }

    #[test]
    fn file_url_src_allowed_from_local_origin() {
        // When is_remote_origin=false, file:// in `image src` must be treated as
        // a direct path and not trigger the registry validation error.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"file:///home/user/img.png\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        assert!(
            result.is_ok(),
            "file:// must be allowed in local-origin documents: {result:?}"
        );
    }

    #[test]
    fn mizu_url_src_skips_guard() {
        // An absolute mizu:// URL is a direct path — guard skipped.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"mizu://cdn.example.com/img.png\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        assert!(result.is_ok(), "mizu:// URL must bypass guard: {result:?}");
    }

    #[test]
    fn symbolic_alias_still_validated() {
        // A pure identifier (no `.` or `/`) is treated as a symbolic alias and
        // must still be rejected when absent from the registry.
        let mut interner = StringInterner::new();
        let registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let layout = "window \"App\"\n    image src \"cdn_icons\"\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("cdn_icons") && msg.contains("not declared"),
                    "error must name the undeclared alias: {msg}"
                );
            }
            other => panic!("expected ParseError for undeclared symbolic alias, got: {other:?}"),
        }
    }

    #[test]
    fn test_parse_interval() {
        assert_eq!(parse_interval("500ms"), Interval::Literal(500));
        assert_eq!(parse_interval("2s"), Interval::Literal(2000));
        assert_eq!(parse_interval("1.5s"), Interval::Literal(1500));
        assert_eq!(
            parse_interval("speed"),
            Interval::Variable("speed".to_string())
        );
    }

    #[test]
    fn test_empty_layout_fails() {
        let result = parse_layout("   \n  \n", &mut StringInterner::new());
        assert!(matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("empty")));
    }

    #[test]
    fn test_root_must_be_window() {
        let result = parse_layout("    box\n", &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("root element must be `window`"))
        );
    }

    #[test]
    fn test_multi_tiered_dom_tree() {
        let layout = r#"
    window "Mizu App"
        box class .container
            text "Welcome to Mizu"
            button "Submit"
                click -> Redirect("/home")
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let root = tree.root();
        assert_eq!(root.value().primitive, Primitive::Window);
        // "Mizu App" is now a child Text node
        let window_text_child = root
            .children()
            .find(|n| n.value().primitive == Primitive::Text);
        assert_eq!(
            window_text_child
                .and_then(|n| n.value().attributes.get("content"))
                .map(|s| s.as_str()),
            Some("Mizu App")
        );

        let mut children = root
            .children()
            .filter(|n| n.value().primitive != Primitive::Text);
        let box_node = children.next().unwrap();
        assert_eq!(box_node.value().primitive, Primitive::Box);
        assert_eq!(
            box_node.value().attributes.get("class").map(|s| s.as_str()),
            Some("container")
        );

        let mut box_children = box_node.children();
        let text_node = box_children.next().unwrap();
        assert_eq!(text_node.value().primitive, Primitive::Text);
        // "Welcome to Mizu" is stored as "content" attribute on the Text node itself
        assert_eq!(
            text_node
                .value()
                .attributes
                .get("content")
                .map(|s| s.as_str()),
            Some("Welcome to Mizu")
        );

        let button_node = box_children.next().unwrap();
        assert_eq!(button_node.value().primitive, Primitive::Button);
        // "Submit" is a child Text node of the button
        let btn_text = button_node
            .children()
            .find(|n| n.value().primitive == Primitive::Text);
        assert_eq!(
            btn_text
                .and_then(|n| n.value().attributes.get("content"))
                .map(|s| s.as_str()),
            Some("Submit")
        );
        assert_eq!(
            button_node.value().events.get("click"),
            Some(&EventBlock::Click {
                action: parse_action("Redirect(\"/home\")", &mut StringInterner::new()).unwrap()
            })
        );
    }

    #[test]
    fn test_attribute_extraction() {
        let layout = r#"
    window "App"
        input type "text" placeholder "Enter Username" class .input-field val 42
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let input_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Input)
            .unwrap();
        let attrs = &input_node.value().attributes;
        assert_eq!(attrs.get("type").map(|s| s.as_str()), Some("text"));
        assert_eq!(
            attrs.get("placeholder").map(|s| s.as_str()),
            Some("Enter Username")
        );
        assert_eq!(attrs.get("class").map(|s| s.as_str()), Some("input-field"));
        assert_eq!(attrs.get("val").map(|s| s.as_str()), Some("42"));
    }

    #[test]
    fn test_event_blocks() {
        let layout = r#"
    window "App"
        button "Submit"
            click -> ActionPerform
        box
            submit -> FormSubmit
"#;
        // Use a shared interner so symbols match between parse_layout and parse_action.
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        // Skip the Text child inserted for "App"
        let mut children = tree
            .root()
            .children()
            .filter(|n| n.value().primitive != Primitive::Text);

        let btn = children.next().unwrap();
        assert_eq!(
            btn.value().events.get("click"),
            Some(&EventBlock::Click {
                action: parse_action("ActionPerform", &mut interner).unwrap()
            })
        );

        let bx = children.next().unwrap();
        assert_eq!(
            bx.value().events.get("submit"),
            Some(&EventBlock::Submit {
                action: parse_action("FormSubmit", &mut interner).unwrap()
            })
        );
    }

    #[test]
    fn test_bind_keyword_produces_error() {
        let layout = "window \"App\"\n    input\n        bind -> user.name\n";
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(msg)) if msg.contains("bind is no longer supported"))
        );
    }

    #[test]
    fn test_every_event_block() {
        let layout = r#"
    window "App"
        box
            every 500ms -> count = count + 1
        text "Time"
            every tick_rate -> UpdateTime
"#;
        // Use a shared interner so symbols match between parse_layout and parse_action.
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        // Skip the Text child inserted for "App"
        let mut children = tree.root().children().filter(|n| {
            n.value().primitive != Primitive::Text || n.value().events.contains_key("every")
        });

        let bx = children.next().unwrap();
        assert_eq!(
            bx.value().events.get("every"),
            Some(&EventBlock::Every {
                interval: Interval::Literal(500),
                action: parse_action("count = count + 1", &mut interner).unwrap(),
            })
        );

        let txt = children.next().unwrap();
        assert_eq!(
            txt.value().events.get("every"),
            Some(&EventBlock::Every {
                interval: Interval::Variable("tick_rate".to_string()),
                action: parse_action("UpdateTime", &mut interner).unwrap(),
            })
        );
    }

    #[test]
    fn test_markdown_multiline_block() {
        let layout = r#"
    window "App"
        markdown """
            # Header
            This is a multi-line markdown block.
            - Item 1
            - Item 2
        """
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let markdown_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Markdown)
            .unwrap();
        assert_eq!(markdown_node.value().primitive, Primitive::Markdown);
        let content = markdown_node.value().attributes.get("content").unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("This is a multi-line markdown block."));
        assert!(content.contains("- Item 1"));
    }

    #[test]
    fn test_illegal_primitive_fails() {
        let layout = r#"
    window "App"
        invalid_primitive "Error"
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("Illegal primitive name"))
        );
    }

    #[test]
    fn test_multiple_roots_fail() {
        let layout = r#"
    window "App"
    box
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("multiple root elements"))
        );
    }

    #[test]
    fn test_badly_formatted_attributes_fail() {
        let layout = r#"
    window "App"
        button class.btn
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("Invalid attribute key"))
        );
    }

    #[test]
    fn test_missing_event_payload_fails() {
        let layout = r#"
    window "App"
        button
            click ->
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("missing its action or variable payload"))
        );
    }

    #[test]
    fn test_case_insensitive_primitives_and_equal_sign_attributes() {
        let layout = r#"
    WINDOW "App" class=".window"
        BOX class = ".container"
            text "Hello"
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let root = tree.root();
        assert_eq!(root.value().primitive, Primitive::Window);
        assert_eq!(
            root.value().attributes.get("class").map(|s| s.as_str()),
            Some("window")
        );

        let box_node = root
            .children()
            .find(|n| n.value().primitive == Primitive::Box)
            .unwrap();
        assert_eq!(box_node.value().primitive, Primitive::Box);
        assert_eq!(
            box_node.value().attributes.get("class").map(|s| s.as_str()),
            Some("container")
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Trailing layout keywords after actions must be hard errors, not silent loss
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_trailing_class_after_click_action_is_error() {
        // Before the fix this silently dropped `class "btn"`.
        let layout = r#"
    window "App"
        button class "expected-btn"
            click -> count = count + 1 class "wrong"
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            result.is_err(),
            "expected error for trailing `class` after action, but parse succeeded: {result:?}"
        );
        if let Err(MizuError::ParseError(ref msg)) = result {
            assert!(
                msg.contains("class"),
                "error message should mention `class`, got: {msg}"
            );
        }
    }

    #[test]
    fn test_trailing_id_after_click_action_is_error() {
        let layout = r#"
    window "App"
        button
            click -> x = 1 id "my-btn"
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            result.is_err(),
            "expected error for trailing `id` after action"
        );
    }

    #[test]
    fn test_trailing_src_after_click_action_is_error() {
        let layout = r#"
    window "App"
        button
            click -> x = 1 src "image.png"
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            result.is_err(),
            "expected error for trailing `src` after action"
        );
    }

    #[test]
    fn test_trailing_class_after_every_action_is_error() {
        let layout = r#"
    window "App"
        box class "timer"
            every 1s -> tick = tick + 1 class "wrong"
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(
            result.is_err(),
            "expected error for trailing `class` after every action"
        );
    }

    #[test]
    fn test_clean_action_with_class_on_element_line_is_ok() {
        // Regression: the correct form must still parse successfully.
        let layout = r#"
    window "App"
        button class "btn"
            click -> count = count + 1
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let root = tree.root();
        let btn = root
            .children()
            .find(|n| n.value().primitive == Primitive::Button)
            .unwrap();
        assert_eq!(btn.value().primitive, Primitive::Button);
        assert_eq!(
            btn.value().attributes.get("class").map(|s| s.as_str()),
            Some("btn")
        );
        assert!(btn.value().events.contains_key("click"));
    }

    #[test]
    fn test_t_alias_for_text() {
        // Use window without inline text so the only Text child is the one from `t "hello"`
        let layout = r#"
    window
        t "hello"
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let root = tree.root();
        let text_node = root
            .children()
            .find(|n| n.value().primitive == Primitive::Text)
            .unwrap();
        assert_eq!(text_node.value().primitive, Primitive::Text);
        let content_child = text_node
            .value()
            .attributes
            .get("content")
            .map(|s| s.as_str());
        assert_eq!(content_child, Some("hello"));
    }

    #[test]
    fn test_each_parsing() {
        let layout = r#"
    window "App"
        each article in articles
            text "item"
"#;
        let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
        let root = tree.root();
        let each_node = root
            .children()
            .find(|n| n.value().primitive == Primitive::Each)
            .unwrap();
        assert_eq!(each_node.value().primitive, Primitive::Each);
        assert_eq!(
            each_node.value().iterator_context,
            Some(("article".to_string(), "articles".to_string()))
        );
    }

    #[test]
    fn test_each_invalid_syntax_fails() {
        let layout = r#"
    window "App"
        each item
"#;
        let result = parse_layout(layout, &mut StringInterner::new());
        assert!(result.is_err(), "expected error for invalid each syntax");
    }

    // ────────────────────────────────────────────────────────────────────────
    // `download(alias)` built-in function
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_download_builtin_parsed() {
        use crate::parser::logic::{Action, Expr};
        use crate::parser::urls::{EndpointKind, UrlEndpoint};

        let mut interner = StringInterner::new();
        let backup_sym = interner.get_or_intern("backup_alias");
        let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
        registry.insert(
            backup_sym,
            UrlEndpoint {
                kind: EndpointKind::Media,
                raw_target: "mizu://cdn.local/backup.zip".to_string(),
            },
        );

        let layout = "window \"App\"\n    button\n        click -> download(backup_alias)\n";
        let tree = parse_layout_with_urls(layout, &mut interner, Some(&registry), false).unwrap();
        let btn = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Button)
            .expect("button node not found");
        let click_event = btn
            .value()
            .events
            .get("click")
            .expect("click event not found");
        match click_event {
            EventBlock::Click {
                action: Action::Eval(Expr::FunctionCall { name, args }),
            } => {
                assert_eq!(
                    interner.resolve(*name),
                    Some("download"),
                    "function name should be 'download'"
                );
                assert_eq!(args.len(), 1, "download should have 1 argument");
                match &args[0] {
                    Expr::Variable(sym) => assert_eq!(interner.resolve(*sym), Some("backup_alias")),
                    other => panic!("expected Variable arg, got {other:?}"),
                }
            }
            other => panic!("expected Click {{ Action::Eval(FunctionCall) }}, got {other:?}"),
        }
    }

    #[test]
    fn test_download_old_syntax_error() {
        // `button download -> backup_alias` → ParseError with migration hint
        let layout = "window \"App\"\n    button download -> backup_alias\n";
        let mut interner = StringInterner::new();
        let result = parse_layout(layout, &mut interner);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("download") && msg.contains("click"),
                    "error should mention download and the new syntax: {msg}"
                );
            }
            other => panic!("expected ParseError for old download syntax, got: {other:?}"),
        }
    }

    #[test]
    fn test_download_api_alias_rejected() {
        use crate::parser::urls::{EndpointKind, UrlEndpoint};

        let mut interner = StringInterner::new();
        let api_sym = interner.get_or_intern("api_alias");
        let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
        registry.insert(
            api_sym,
            UrlEndpoint {
                kind: EndpointKind::Api,
                raw_target: "mizu://api.local/v1/data".to_string(),
            },
        );

        let layout = "window \"App\"\n    button\n        click -> download(api_alias)\n";
        let result = parse_layout_with_urls(layout, &mut interner, Some(&registry), false);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("api_alias") && (msg.contains("api") || msg.contains("media")),
                    "error should mention alias and endpoint kind: {msg}"
                );
            }
            other => panic!("expected ParseError for api download alias, got: {other:?}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Conditional classes
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_conditional_class_parsed() {
        // A `class active if flag` child line should produce a non-empty
        // conditional_classes vec on the parent box node.
        let layout = "window\n    box class base\n        class active if flag\n";
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        let box_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Box)
            .expect("box node not found");
        assert_eq!(
            box_node.value().attributes.get("class").map(|s| s.as_str()),
            Some("base")
        );
        assert_eq!(box_node.value().conditional_classes.len(), 1);
        assert_eq!(box_node.value().conditional_classes[0].class_name, "active");
    }

    #[test]
    fn test_conditional_class_applied() {
        // Condition evaluates to true → the expression result is Bool(true).
        use crate::core::types::{Value, VariableStore};
        use crate::parser::logic::evaluate;
        use rustc_hash::FxHashMap;

        let layout = "window\n    box class base\n        class active if flag\n";
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        let box_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Box)
            .unwrap();
        let cc = &box_node.value().conditional_classes[0];

        let mut store = VariableStore::with_interner(interner);
        store.set("flag", Value::Bool(true));
        let result = evaluate(&cc.condition, &mut store, &FxHashMap::default(), 0).unwrap();
        assert_eq!(
            result,
            Value::Bool(true),
            "condition with flag=true should be truthy"
        );
    }

    #[test]
    fn test_conditional_class_not_applied() {
        // Condition evaluates to false → the expression result is Bool(false).
        use crate::core::types::{Value, VariableStore};
        use crate::parser::logic::evaluate;
        use rustc_hash::FxHashMap;

        let layout = "window\n    box class base\n        class active if flag\n";
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        let box_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Box)
            .unwrap();
        let cc = &box_node.value().conditional_classes[0];

        let mut store = VariableStore::with_interner(interner);
        store.set("flag", Value::Bool(false));
        let result = evaluate(&cc.condition, &mut store, &FxHashMap::default(), 0).unwrap();
        assert_eq!(
            result,
            Value::Bool(false),
            "condition with flag=false should be falsy"
        );
    }

    #[test]
    fn test_multiple_conditional_classes() {
        // Three classes: two conditions true, one false → 2 truthy, 1 falsy.
        use crate::core::types::{Value, VariableStore};
        use crate::parser::logic::evaluate;
        use rustc_hash::FxHashMap;

        let layout = "window\n    box\n        class a if flag_a\n        class b if flag_b\n        class c if flag_c\n";
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        let box_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Box)
            .unwrap();
        assert_eq!(box_node.value().conditional_classes.len(), 3);

        let mut store = VariableStore::with_interner(interner);
        store.set("flag_a", Value::Bool(true));
        store.set("flag_b", Value::Bool(false));
        store.set("flag_c", Value::Bool(true));

        let fns: FxHashMap<_, _> = FxHashMap::default();
        let ccs = &box_node.value().conditional_classes;
        let truthy_count = ccs
            .iter()
            .filter(|cc| {
                matches!(
                    evaluate(&cc.condition, &mut store, &fns, 0),
                    Ok(Value::Bool(true))
                )
            })
            .count();
        assert_eq!(truthy_count, 2, "two of three conditions should be truthy");
    }

    #[test]
    fn test_conditional_class_with_field_access() {
        // Condition `item.done` on a Record value resolves correctly.
        use crate::core::types::{Value, VariableStore};
        use crate::parser::logic::evaluate;
        use rustc_hash::FxHashMap;
        use std::sync::Arc;

        let layout = "window\n    box\n        class active if item.done\n";
        let mut interner = StringInterner::new();
        let tree = parse_layout(layout, &mut interner).unwrap();
        let box_node = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Box)
            .unwrap();
        assert_eq!(box_node.value().conditional_classes[0].class_name, "active");

        let mut record_map: std::collections::BTreeMap<Arc<str>, Value> =
            std::collections::BTreeMap::new();
        record_map.insert(Arc::from("done"), Value::Bool(true));

        let mut store = VariableStore::with_interner(interner);
        store.set("item", Value::Record(Arc::new(record_map)));

        let cc = &box_node.value().conditional_classes[0];
        let result = evaluate(&cc.condition, &mut store, &FxHashMap::default(), 0).unwrap();
        assert_eq!(
            result,
            Value::Bool(true),
            "item.done should resolve to true"
        );
    }

    #[test]
    fn test_conditional_class_with_action_rejected() {
        // A condition that calls a side-effecting built-in must produce ParseError.
        let layout = "window\n    box\n        class active if GET(api_alias)\n";
        let mut interner = StringInterner::new();
        let result = parse_layout(layout, &mut interner);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("GET") || msg.contains("side-effect") || msg.contains("pure"),
                    "error should mention GET or purity: {msg}"
                );
            }
            other => panic!("expected ParseError for side-effecting condition, got: {other:?}"),
        }
    }
}
