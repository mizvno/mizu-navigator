#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::types::VariableStore;
use crate::parser::{MizuNode, Primitive, StyleRules};
use crate::render::vello_pipeline::to_vello_color;
use ego_tree::{NodeId as EgoNodeId, Tree};
use std::collections::HashMap;

/// Extracts placeholder variable names within curly braces `{name}` from a string.
pub fn extract_placeholders(text: &str) -> Vec<String> {
    let mut placeholders = Vec::new();
    let mut remaining = text;
    while let Some(start_idx) = remaining.find('{') {
        let after_brace = &remaining[start_idx + 1..];
        if let Some(end_idx) = after_brace.find('}') {
            let var_name = &after_brace[..end_idx];
            placeholders.push(var_name.to_string());
            remaining = &after_brace[end_idx + 1..];
        } else {
            remaining = after_brace;
        }
    }
    placeholders
}

/// Computes logical size and Parley text layout for a DOM node.
///
/// For `input` nodes the rendered text comes from `local_inputs` (the live
/// per-node typing buffers, keyed by u32 id).  An untouched, unfocused input
/// shows its `placeholder` attribute dimmed; an empty focused input renders a
/// single space so the line metrics — and therefore the box height — stay
/// stable across the empty ↔ non-empty transition.
#[allow(clippy::too_many_arguments)]
pub fn calculate_node_text(
    node_id: EgoNodeId,
    dom: &Tree<MizuNode>,
    style_rules: &HashMap<String, StyleRules>,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
    store: &VariableStore,
    available_width: Option<f32>,
    local_inputs: &rustc_hash::FxHashMap<u32, String>,
    node_id_to_u32: &HashMap<EgoNodeId, u32>,
    focused_input: Option<EgoNodeId>,
) -> Option<((f32, f32), parley::Layout<vello::peniko::Color>)> {
    let node_ref = dom.get(node_id)?;
    let mizu_node = node_ref.value();

    if mizu_node.primitive == Primitive::Window {
        return None;
    }

    let is_input = mizu_node.primitive == Primitive::Input;
    let mut is_placeholder = false;
    let raw_text = if is_input {
        let typed = node_id_to_u32
            .get(&node_id)
            .and_then(|u| local_inputs.get(u))
            .map(String::as_str)
            .unwrap_or("");
        if !typed.is_empty() {
            typed.to_string()
        } else if focused_input != Some(node_id)
            && let Some(ph) = mizu_node.attributes.get("placeholder")
            && !ph.is_empty()
        {
            is_placeholder = true;
            ph.clone()
        } else {
            // Invisible single space: keeps line metrics stable and puts the
            // caret at the left edge when the input is focused and empty.
            " ".to_string()
        }
    } else if let Some(text) = mizu_node.attributes.get("content") {
        text.clone()
    } else {
        return None;
    };

    let mut font_size = 16.0f32;
    let mut text_color = vello::peniko::Color::WHITE;

    let mut merged = StyleRules::default();
    if let Some(tag_rules) = style_rules.get(mizu_node.primitive.as_str()) {
        merged = merged.merge(tag_rules.clone());
    }
    if let Some(class_attr) = mizu_node.attributes.get("class")
        && let Some(rules) = style_rules.get(class_attr)
    {
        merged = merged.merge(rules.clone());
    }

    if let Some(fs) = merged.font_size {
        font_size = fs;
    }
    if let Some(ref tc) = merged.color {
        text_color = to_vello_color(tc);
    }
    if is_placeholder {
        // Placeholder renders dimmed: same hue, reduced alpha.
        text_color = vello::peniko::Color::rgba8(text_color.r, text_color.g, text_color.b, 120);
    }

    let text_to_draw = if mizu_node.primitive == Primitive::Input {
        raw_text
    } else {
        store.interpolate(&raw_text).unwrap_or_else(|e| match &e {
            MizuError::BindingNotFound(name) => format!("{{missing: {}}}", name),
            _ => format!("{{error: {}}}", e),
        })
    };

    let mut builder = layout_cx.ranged_builder(font_cx, &text_to_draw, 1.0, true);
    let fallbacks = vec![
        parley::style::FontFamilyName::named("Segoe UI"),
        parley::style::FontFamilyName::named("Arial"),
        parley::style::FontFamilyName::named("Meiryo"),
        parley::style::FontFamilyName::named("Yu Gothic"),
        parley::style::FontFamilyName::named("Hiragino Sans"),
        parley::style::FontFamilyName::Generic(parley::style::GenericFamily::SansSerif),
    ];
    let font_family = parley::style::FontFamily::List(std::borrow::Cow::Owned(fallbacks));
    builder.push_default(parley::style::StyleProperty::FontFamily(font_family));
    builder.push_default(parley::style::StyleProperty::FontSize(font_size));
    builder.push_default(parley::style::StyleProperty::Brush(text_color));
    builder.push_default(parley::style::StyleProperty::LineHeight(
        parley::style::LineHeight::FontSizeRelative(1.2),
    ));

    let mut layout = builder.build(&text_to_draw);
    // Inputs are single-line: long text is clipped by the paint layer instead
    // of wrapping (which would grow the box height while typing).
    let mut is_nowrap = is_input;
    if let Some(parent) = node_ref.parent()
        && parent.value().primitive == Primitive::Button
    {
        is_nowrap = true;
    }

    let max_advance = if is_nowrap { None } else { available_width };

    layout.break_all_lines(max_advance);

    let y_offset = if let Some(first_line) = layout.lines().next() {
        first_line.metrics().ascent - first_line.metrics().baseline
    } else {
        0.0
    };

    let width = layout.width().ceil() + 1.0;
    let mut height = (layout.height() + y_offset).ceil() + 1.0;

    if is_nowrap && let Some(first_line) = layout.lines().next() {
        height = (first_line.metrics().line_height + y_offset).ceil() + 1.0;
    }

    Some(((width, height), layout))
}
