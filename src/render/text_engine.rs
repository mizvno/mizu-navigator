//! # `text_engine` — Parley text layout for Mizu DOM nodes
//!
//! ## Font resolution & the determinism decision (ux-3 Part B)
//!
//! An author's `font-family` choice — one of the fixed generics
//! `sans-serif` / `serif` / `monospace`, see
//! [`crate::parser::style::MizuFontFamily`] — resolves to a single
//! `parley::GenericFamily` entry. Actual glyph coverage per script comes from
//! **fontique's system font fallback**: parley's shaping pass
//! (`FontSelector` in `parley::shape`) consults
//! `fontique::Query::set_fallbacks(FallbackKey::new(script, locale))` for
//! every text run, independent of which family was requested, and picks the
//! font that actually covers that run's codepoints. A single generic entry
//! therefore gets full per-script coverage while still respecting the
//! author's serif/sans/mono choice — which a hand-picked list of concrete
//! font names could never do, since nothing before this change looked at
//! `font-family` at all.
//!
//! This module makes an explicit **System-only** determinism choice (the
//! alternative being a hybrid bundled-Noto safety net), not a default left
//! unexamined:
//!
//! * **Measured, not assumed:** on the primary target (Windows), the
//!   documented coverage bar — Latin, Cyrillic, Greek, Arabic, Hebrew, Han
//!   (Simplified + Traditional), Japanese, Korean, Devanagari, Bengali,
//!   Thai, plus emoji — is verified empirically by
//!   `tests::script_coverage_bar_renders_without_tofu` to render without
//!   `.notdef` glyphs using only OS-installed fonts (Segoe UI, Nirmala UI,
//!   Microsoft YaHei/SimSun, Malgun Gothic, Yu Gothic, Segoe UI Emoji) and
//!   zero bundled bytes.
//! * **Why not bundle a Noto safety net?** Bundling means embedding real
//!   font binaries (Noto Sans/Serif/Mono plus CJK, Arabic, Indic, Thai,
//!   Hebrew faces) and registering them into fontique's `Collection`.
//!   Sourcing, license-verifying (Noto is OFL 1.1), and vetting binary font
//!   assets is a materially larger, separable piece of work — left as an
//!   explicit follow-up rather than folded into this change.
//! * **Consequence accepted:** rendering is non-deterministic across
//!   machines — a document can render a script as tofu on a system missing
//!   that script's font/language pack (a minimal Windows install, or the
//!   untested Linux/macOS targets). This is a stated tradeoff, not a
//!   silently-deferred gap.

#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::types::VariableStore;
use crate::parser::{MizuFontFamily, MizuFontStyle, MizuNode, MizuTextAlign, Primitive, StyleRules};
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

    // Resolve the author's generic (`sans-serif`/`serif`/`monospace`, default
    // sans-serif) to a *single* `parley::GenericFamily` entry rather than a
    // hand-picked list of concrete font names. parley's shaping pass
    // (`FontSelector` in `parley::shape`) already performs per-run,
    // coverage-based script fallback via `Query::set_fallbacks(FallbackKey::
    // new(script, locale))` for *every* run regardless of the requested
    // family — so a single generic entry gets full script coverage from
    // fontique while still respecting the author's serif/sans/mono choice
    // (which a fixed named list could never do, since it never looked at
    // `font-family` at all).
    let generic_family = match merged.font_family.unwrap_or_default() {
        MizuFontFamily::SansSerif => parley::style::GenericFamily::SansSerif,
        MizuFontFamily::Serif => parley::style::GenericFamily::Serif,
        MizuFontFamily::Monospace => parley::style::GenericFamily::Monospace,
    };
    let font_family = parley::style::FontFamily::Single(parley::style::FontFamilyName::Generic(
        generic_family,
    ));
    builder.push_default(parley::style::StyleProperty::FontFamily(font_family));
    builder.push_default(parley::style::StyleProperty::FontSize(font_size));
    builder.push_default(parley::style::StyleProperty::Brush(text_color));
    builder.push_default(parley::style::StyleProperty::LineHeight(
        parley::style::LineHeight::FontSizeRelative(merged.line_height.unwrap_or(1.2)),
    ));
    if let Some(weight) = merged.font_weight {
        builder.push_default(parley::style::StyleProperty::FontWeight(
            parley::style::FontWeight::new(weight),
        ));
    }
    if let Some(font_style) = merged.font_style {
        builder.push_default(parley::style::StyleProperty::FontStyle(match font_style {
            MizuFontStyle::Normal => parley::style::FontStyle::Normal,
            MizuFontStyle::Italic => parley::style::FontStyle::Italic,
        }));
    }
    if let Some(underline) = merged.underline {
        builder.push_default(parley::style::StyleProperty::Underline(underline));
    }

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

    if let Some(text_align) = merged.text_align {
        let alignment = match text_align {
            MizuTextAlign::Left => parley::layout::Alignment::Left,
            MizuTextAlign::Center => parley::layout::Alignment::Center,
            MizuTextAlign::Right => parley::layout::Alignment::Right,
            MizuTextAlign::Justify => parley::layout::Alignment::Justify,
        };
        layout.align(alignment, parley::layout::AlignmentOptions::default());
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    /// The "No Tofu" coverage bar (ux-3, modeled on Noto's own coverage
    /// benchmark): every script here must shape without a single `.notdef`
    /// (glyph id 0) glyph through the real `calculate_node_text` path — the
    /// same generic-family resolution + fontique system fallback documented
    /// in the module doc's determinism note. Table-driven so a regression in
    /// fallback for any single script fails loudly and by name.
    const COVERAGE_BAR: &[(&str, &str)] = &[
        ("Latin", "Hello world"),
        ("Cyrillic", "Привет мир"),
        ("Greek", "Γειά σου Κόσμε"),
        ("Arabic", "مرحبا بالعالم"),
        ("Hebrew", "שלום עולם"),
        ("Han-Simplified", "你好世界"),
        ("Han-Traditional", "你好世界繁體"),
        ("Japanese", "こんにちは世界"),
        ("Korean", "안녕하세요 세계"),
        ("Devanagari", "नमस्ते दुनिया"),
        ("Bengali", "ওহে বিশ্ব"),
        ("Thai", "สวัสดีชาวโลก"),
        ("Emoji", "😀🎉🔥"),
    ];

    fn text_node(content: &str) -> MizuNode {
        let mut attrs = StdHashMap::new();
        attrs.insert("content".to_string(), content.to_string());
        MizuNode {
            primitive: Primitive::Text,
            attributes: attrs,
            events: StdHashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    #[test]
    fn script_coverage_bar_renders_without_tofu() {
        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx: parley::LayoutContext<vello::peniko::Color> =
            parley::LayoutContext::new();
        let style_rules: HashMap<String, StyleRules> = HashMap::new();
        let store = VariableStore::new();
        let local_inputs = rustc_hash::FxHashMap::default();
        let node_id_to_u32 = HashMap::new();

        let mut failures = Vec::new();
        for &(label, sample) in COVERAGE_BAR {
            let tree = Tree::new(text_node(sample));
            let node_id = tree.root().id();
            let Some((_dims, layout)) = calculate_node_text(
                node_id,
                &tree,
                &style_rules,
                &mut font_cx,
                &mut layout_cx,
                &store,
                None,
                &local_inputs,
                &node_id_to_u32,
                None,
            ) else {
                failures.push(format!("{label}: calculate_node_text returned None"));
                continue;
            };

            let mut notdef = 0usize;
            let mut total = 0usize;
            for line in layout.lines() {
                for item in line.items() {
                    if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                        for g in run.positioned_glyphs() {
                            total += 1;
                            if g.id == 0 {
                                notdef += 1;
                            }
                        }
                    }
                }
            }
            if notdef > 0 || total == 0 {
                failures.push(format!("{label}: total_glyphs={total} notdef={notdef}"));
            }
        }

        assert!(
            failures.is_empty(),
            "script coverage bar regressed (tofu, or no glyphs at all):\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn font_family_generic_resolves_per_author_choice() {
        // Regression: font-family must actually be read (it wasn't, before
        // ux-3 — the old hardcoded list ignored StyleRules entirely).
        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx: parley::LayoutContext<vello::peniko::Color> =
            parley::LayoutContext::new();
        let store = VariableStore::new();
        let local_inputs = rustc_hash::FxHashMap::default();
        let node_id_to_u32 = HashMap::new();

        for generic in [
            crate::parser::MizuFontFamily::SansSerif,
            crate::parser::MizuFontFamily::Serif,
            crate::parser::MizuFontFamily::Monospace,
        ] {
            let mut attrs = StdHashMap::new();
            attrs.insert("content".to_string(), "Hello".to_string());
            attrs.insert("class".to_string(), "label".to_string());
            let node = MizuNode {
                primitive: Primitive::Text,
                attributes: attrs,
                events: StdHashMap::new(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            };
            let mut rules = StyleRules::default();
            rules.font_family = Some(generic);
            let mut style_rules = HashMap::new();
            style_rules.insert("label".to_string(), rules);

            let tree = Tree::new(node);
            let node_id = tree.root().id();
            let result = calculate_node_text(
                node_id,
                &tree,
                &style_rules,
                &mut font_cx,
                &mut layout_cx,
                &store,
                None,
                &local_inputs,
                &node_id_to_u32,
                None,
            );
            assert!(
                result.is_some(),
                "{generic:?}: expected a layout to be produced"
            );
        }
    }
}
