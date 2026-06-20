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
pub fn calculate_node_text(
    node_id: EgoNodeId,
    dom: &Tree<MizuNode>,
    style_rules: &HashMap<String, StyleRules>,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
    store: &VariableStore,
    available_width: Option<f32>,
) -> Option<((f32, f32), parley::Layout<vello::peniko::Color>)> {
    let node_ref = dom.get(node_id)?;
    let mizu_node = node_ref.value();

    if mizu_node.primitive == Primitive::Window {
        return None;
    }

    let raw_text = if mizu_node.primitive == Primitive::Input {
        String::new()
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
    let mut is_nowrap = false;
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
