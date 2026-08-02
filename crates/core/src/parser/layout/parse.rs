//! Entry points: `parse_attributes_and_events`/`parse_primitive_and_attrs`
//! (per-line attribute and event parsing) and `parse_layout`/
//! `parse_layout_with_urls` (the indentation-driven DOM tree builder).

use ego_tree::{NodeId, Tree};
use rustc_hash::FxHashMap;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol};
use crate::parser::logic::{
    Expr, MizuFunction, find_side_effect_call, parse_action_with_urls, parse_expr_standalone,
};
use crate::parser::urls::{EndpointKind, UrlRegistry};

use super::helpers::*;
use super::types::*;

pub type AttrsAndEvents = (FxHashMap<String, String>, FxHashMap<String, EventBlock>);
fn parse_attributes_and_events(
    mut s: &str,
    interner: &mut StringInterner,
) -> Result<AttrsAndEvents, MizuError> {
    let mut attrs = FxHashMap::default();
    let mut events = FxHashMap::default();
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
                "download -> alias is no longer supported; use click -> download(alias)"
                    .to_string(),
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
            return Err(MizuError::ParseError(
                "node timers are not allowed; declare a root timer in the logic block instead"
                    .to_string(),
            ));
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

        if key == "class" && value.starts_with('.') {
            return Err(MizuError::ParseError(format!(
                "class value `{value}` must not start with `.`; write `class {}` instead",
                &value[1..]
            )));
        }
        let final_value = value;

        // `dir` (ux-7): base text/layout direction, inherited down the
        // tree — see `render::bidi` and `docs/design/bidi.md`. Validated
        // here (fail-secure allowlist, matching every other small-fixed-set
        // attribute/property in this codebase) rather than accepted as a
        // free-form string like `href`/`alt`.
        if key == "dir" && !matches!(final_value.as_str(), "ltr" | "rtl" | "auto") {
            return Err(MizuError::ParseError(format!(
                "invalid value `{final_value}` for `dir`; must be `ltr`, `rtl`, or `auto`"
            )));
        }

        // `lang`: settable on `doc` as the document-wide default and
        // overridable on any node, inherited down the tree exactly like
        // `dir` (see `render::bidi::resolve_lang`) — but unlike `dir`'s
        // fixed 3-value set, language subtags are open-ended, so this is a
        // shape check (BCP-47-ish), not an exhaustive allowlist.
        if key == "lang" && !is_valid_lang_tag(&final_value) {
            return Err(MizuError::ParseError(format!(
                "invalid value `{final_value}` for `lang`; expected a lowercase 2-3-letter \
                 language subtag, optionally followed by `-` and an uppercase 2-letter \
                 region subtag (e.g. `it`, `en`, `en-US`, `zh-CN`)"
            )));
        }

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
                attributes: FxHashMap::default(),
                events: FxHashMap::default(),
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
        "doc" => Primitive::Doc,
        "window" => {
            return Err(MizuError::ParseError(format!(
                "line {line_num}: `window` is no longer supported as the root primitive; use `doc` instead"
            )));
        }
        "box" => Primitive::Box,
        "t" | "text" => Primitive::Text,
        "button" => Primitive::Button,
        "input" => Primitive::Input,
        "image" => Primitive::Image,
        "markdown" => Primitive::Markdown,
        "form" => Primitive::Form,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Primitive::Heading,
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

    // `h1`-`h6` are six spellings of one `Heading` primitive; the digit is
    // the only difference between them, so it's stored as the `level`
    // attribute — the match arm above only accepts these six literal
    // spellings, so `prim_lower[1..]` always parses as `1..=6`.
    if primitive == Primitive::Heading {
        attributes.insert("level".to_string(), prim_lower[1..].to_string());
    }

    // `title` is only meaningful on `doc` (it sets the OS window title, not
    // visible page content) — reject it loudly on any other primitive
    // instead of silently ignoring it.
    if attributes.contains_key("title") && primitive != Primitive::Doc {
        return Err(MizuError::ParseError(format!(
            "line {line_num}: `title` is only valid on `doc`, found on `{}`",
            primitive.as_str()
        )));
    }

    // For Text nodes, store inline text directly in "content" attribute (no
    // child node). `doc` no longer accepts positional inline text at all —
    // the OS window title must be set via the explicit `title "..."`
    // attribute; this closes the "bare string after the tag means
    // something different per primitive" inconsistency the old sugar had.
    // For other primitives, the inline_text is returned to the caller,
    // which will create a child node.
    let child_inline_text = if primitive == Primitive::Text {
        if let Some(text) = inline_text {
            attributes.insert("content".to_string(), text);
        }
        None
    } else if primitive == Primitive::Doc && inline_text.is_some() {
        return Err(MizuError::ParseError(format!(
            "line {line_num}: `doc` no longer accepts positional inline text for the window \
             title; use an explicit `title \"...\"` attribute instead"
        )));
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
///   is not `doc`, multiple roots are defined, or bad syntax), or if a media
///   alias is undeclared or points to a non-media endpoint.
pub fn parse_layout(
    layout_content: &str,
    interner: &mut StringInterner,
) -> Result<Tree<MizuNode>, MizuError> {
    parse_layout_with_urls(
        layout_content,
        interner,
        None,
        false,
        &rustc_hash::FxHashMap::default(),
    )
}

/// Like [`parse_layout`] but accepts an optional [`UrlRegistry`] for media alias validation
/// and an `is_remote_origin` flag that blocks `file://` asset references at parse time.
///
/// `functions` is the document's user-defined-function table (from
/// `parse_logic`, parsed before the layout block) — needed by the P1 purity
/// checker (`find_side_effect_call`) to recognise a call to a user-defined
/// function inside a conditional class condition as pure-by-construction,
/// as opposed to an unknown name (rejected fail-secure). Callers with no
/// functions in scope (most tests) may pass `&FxHashMap::default()`.
pub fn parse_layout_with_urls(
    layout_content: &str,
    interner: &mut StringInterner,
    url_registry: Option<&UrlRegistry>,
    is_remote_origin: bool,
    functions: &FxHashMap<Symbol, MizuFunction>,
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
    if first_node.primitive != Primitive::Doc {
        return Err(MizuError::ParseError(format!(
            "line {}: root element must be `doc`, found `{}`",
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
                let mut m = FxHashMap::default();
                m.insert("content".to_string(), text_str);
                m
            },
            events: FxHashMap::default(),
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
                "download -> alias is no longer supported; use click -> download(alias)"
                    .to_string(),
            ));
        } else if first_word == "every" {
            return Err(MizuError::ParseError(format!(
                "line {}: node timers are not allowed; declare a root timer in the logic block instead",
                line_idx + 1
            )));
        } else if first_word == "click" || first_word == "submit" {
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

        // ── Conditional class: `class <name> if <expr>` (toggle) or ─────────
        // `class <cond> ? "a" : "b"` (ternary, `if`/`then`/`else` spelling
        // also accepted since it's the same underlying expression grammar) ─
        if first_word == "class" {
            let (first_tok, rest2) = split_first_word(rest);
            if first_tok.is_empty() {
                return Err(MizuError::ParseError(format!(
                    "line {}: `class` child line is missing the class name or condition",
                    line_idx + 1
                )));
            }
            let (second_tok, expr_str) = split_first_word(rest2);

            let conditional_class = if second_tok == "if" {
                // ── Toggle form: `class <name> if <expr>` ──
                let class_name = first_tok;
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
                if let Some(bad_fn) =
                    find_side_effect_call(condition.root(), &condition.arena, interner, functions)
                {
                    return Err(MizuError::ParseError(format!(
                        "line {}: conditional class condition must be pure — \
                         `{bad_fn}` is a side-effecting call",
                        line_idx + 1
                    )));
                }
                ConditionalClass::Toggle {
                    class_name: class_name.to_string(),
                    condition,
                }
            } else {
                // ── Ternary form: `class <cond> ? "a" : "b"` ──
                let expr = parse_expr_standalone(rest, interner).map_err(|e| {
                    MizuError::ParseError(format!(
                        "line {}: `class {rest}` is neither a valid `if` toggle nor a \
                         valid `?:` ternary conditional class: {e}",
                        line_idx + 1
                    ))
                })?;
                if !matches!(expr.root(), Expr::IfElse { .. }) {
                    return Err(MizuError::ParseError(format!(
                        "line {}: `class {rest}` is missing the `if` keyword (for a toggle) \
                         or a `?:` ternary",
                        line_idx + 1
                    )));
                }
                if let Some(bad_fn) =
                    find_side_effect_call(expr.root(), &expr.arena, interner, functions)
                {
                    return Err(MizuError::ParseError(format!(
                        "line {}: conditional class condition must be pure — \
                         `{bad_fn}` is a side-effecting call",
                        line_idx + 1
                    )));
                }
                if let Some(bad_branch) = find_non_literal_string_branch(expr.root(), &expr.arena) {
                    return Err(MizuError::ParseError(format!(
                        "line {}: ternary conditional class `class {rest}` has {bad_branch} \
                         as a branch; every branch must be a string literal",
                        line_idx + 1
                    )));
                }
                ConditionalClass::Ternary { expr }
            };

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
                        "line {}: `class {rest}` has no parent node",
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
                .push(conditional_class);
            continue;
        }

        // ── Parse Primitive Nodes ───────────────────────────────────────────
        let (mut node, is_markdown, inline_text) =
            parse_primitive_and_attrs(trimmed, line_idx + 1, interner)?;

        // ── Image src: reject absolute network URLs unconditionally ─────────
        // A literal `mizu://` (or http[s]://) in `src` is a network channel
        // that bypasses the `urls` registry entirely — a tracking-pixel /
        // exfiltration vector. Only a declared `media` alias or a local
        // relative path is allowed. This runs regardless of whether a registry
        // was supplied, so the rule cannot be skipped.
        if node.primitive == Primitive::Image
            && let Some(src_val) = node.attributes.get("src")
            && (src_val.starts_with("mizu://")
                || src_val.starts_with("http://")
                || src_val.starts_with("https://"))
        {
            return Err(MizuError::ParseError(format!(
                "line {}: absolute URLs are not allowed in src; declare a media alias in the urls block",
                line_idx + 1
            )));
        }

        // ── Compile-time media guard + alias resolution ─────────────────────
        // If a URL registry is provided, validate that `image src: alias`
        // points to a declared `media` endpoint and rewrite the attribute to
        // the endpoint's absolute URL — the renderer consumes concrete URLs.
        // (`download(alias)` is validated in `parse_action_with_urls` at action parse time.)
        if let Some(registry) = url_registry
            && node.primitive == Primitive::Image
            && let Some(src_alias) = node.attributes.get("src").cloned()
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

            // A relative path (contains `.` or `/`) or a sandboxed local
            // `file://` (local documents only) is used as-is by the renderer;
            // absolute network URLs were already rejected above. Everything
            // else is a symbolic alias that must resolve against the registry.
            let is_direct_path = src_alias.contains('/')
                || src_alias.contains('.')
                || (src_alias.starts_with("file://") && !is_remote_origin);
            if !is_direct_path {
                let sym = interner.get_or_intern(&src_alias);
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
                    Some(ep) => {
                        node.attributes
                            .insert("src".to_string(), ep.raw_target.clone());
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

        if node.primitive == Primitive::Each {
            for &(_, parent_id) in stack.iter() {
                if let Some(parent_node) = tree.get(parent_id)
                    && parent_node.value().primitive == Primitive::Each
                {
                    return Err(MizuError::ParseError(format!(
                        "line {}: nested `each` blocks are strictly forbidden",
                        line_idx + 1
                    )));
                }
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
                    let mut m = FxHashMap::default();
                    m.insert("content".to_string(), text_str);
                    m
                },
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            };
            if let Some(mut node_mut) = tree.get_mut(new_id) {
                node_mut.append(text_node);
            }
        }

        stack.push((indent, new_id));
        if stack.len() > MAX_LAYOUT_DEPTH {
            return Err(MizuError::ParseError(format!(
                "line {}: layout nesting too deep (max {MAX_LAYOUT_DEPTH} levels)",
                line_idx + 1
            )));
        }
    }

    Ok(tree)
}
