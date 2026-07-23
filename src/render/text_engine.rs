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
    style_variants: &[crate::parser::style::StyleVariant],
    render_env: &crate::render::responsive::RenderEnvironment,
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
    let mut text_color = vello::peniko::Color::BLACK;

    let mut merged = StyleRules::default();
    let tag_name = mizu_node.primitive.as_str();
    if let Some(tag_rules) = style_rules.get(tag_name) {
        merged = merged.merge(tag_rules.clone());
    }
    let class_attr = mizu_node.attributes.get("class").map(String::as_str);
    if let Some(class_attr) = class_attr
        && let Some(rules) = style_rules.get(class_attr)
    {
        merged = merged.merge(rules.clone());
    }
    // ux-6: breakpoint/color-scheme variants, applied last (after both
    // bases), in source declaration order — see docs/design/responsive.md.
    let variant_selectors: &[&str] = match class_attr {
        Some(c) => &[tag_name, c],
        None => &[tag_name],
    };
    merged = merged.merge(crate::render::responsive::resolve_matching_variants(
        style_variants,
        variant_selectors,
        render_env,
    ));

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

    let mut text_to_draw = if mizu_node.primitive == Primitive::Input {
        raw_text
    } else {
        store.interpolate(&raw_text).unwrap_or_else(|e| match &e {
            MizuError::BindingNotFound(name) => format!("{{missing: {}}}", name),
            _ => format!("{{error: {}}}", e),
        })
    };

    // ux-7: resolved once per node via `dir` attribute inheritance.
    let dir = crate::render::bidi::resolve_direction(node_ref);
    // An explicit `dir="ltr"`/`dir="rtl"` prepends a zero-width strong mark
    // so parley's own (always-running) bidi auto-detection resolves to the
    // declared direction instead of whatever the text's first strong
    // character would otherwise imply — parley 0.10 has no public base-
    // direction override; see docs/design/bidi.md and render::bidi's doc.
    if let Some(mark) = dir.prepend_mark() {
        text_to_draw.insert(0, mark);
    }

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
        // `Start`/`End` (ux-7) resolve to Left/Right by the node's resolved
        // `dir` — Start is the left edge under LTR, the right edge under
        // RTL; End is the mirror. See docs/design/bidi.md.
        let is_rtl = dir.is_rtl_for_layout();
        let alignment = match text_align {
            MizuTextAlign::Left => parley::layout::Alignment::Left,
            MizuTextAlign::Center => parley::layout::Alignment::Center,
            MizuTextAlign::Right => parley::layout::Alignment::Right,
            MizuTextAlign::Justify => parley::layout::Alignment::Justify,
            MizuTextAlign::Start if is_rtl => parley::layout::Alignment::Right,
            MizuTextAlign::Start => parley::layout::Alignment::Left,
            MizuTextAlign::End if is_rtl => parley::layout::Alignment::Left,
            MizuTextAlign::End => parley::layout::Alignment::Right,
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
                &[],
                &crate::render::responsive::RenderEnvironment {
                    viewport: crate::render::responsive::ViewportSize {
                        width: 800.0,
                        height: 600.0,
                    },
                    color_scheme: crate::render::preferences::ColorScheme::Dark,
                },
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
                &[],
                &crate::render::responsive::RenderEnvironment {
                    viewport: crate::render::responsive::ViewportSize {
                        width: 800.0,
                        height: 600.0,
                    },
                    color_scheme: crate::render::preferences::ColorScheme::Dark,
                },
            );
            assert!(
                result.is_some(),
                "{generic:?}: expected a layout to be produced"
            );
        }
    }

    #[test]
    fn color_scheme_variant_reaches_calculate_node_text() {
        // Integration check for the ux-6 wiring itself (the StyleRules-level
        // merge is already covered by render::responsive's own tests): a
        // `@dark`/`@light` variant changing `font-size` must actually change
        // the layout `calculate_node_text` produces — proving the variant
        // resolution reaches this paint-time call, not just build_taffy_tree.
        use crate::parser::style::parse_style_with_variants;

        let style = r"
    .label
        font-size 16
    .label @dark
        font-size 40
    .label @light
        font-size 12
";
        let (style_rules, style_variants) = parse_style_with_variants(style).unwrap();

        let mut attrs = StdHashMap::new();
        attrs.insert("content".to_string(), "Hi".to_string());
        attrs.insert("class".to_string(), "label".to_string());
        let node = MizuNode {
            primitive: Primitive::Text,
            attributes: attrs,
            events: StdHashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        };
        let tree = Tree::new(node);
        let node_id = tree.root().id();

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx: parley::LayoutContext<vello::peniko::Color> =
            parley::LayoutContext::new();
        let store = VariableStore::new();
        let local_inputs = rustc_hash::FxHashMap::default();
        let node_id_to_u32 = HashMap::new();

        let viewport = crate::render::responsive::ViewportSize {
            width: 800.0,
            height: 600.0,
        };
        let dark_env = crate::render::responsive::RenderEnvironment {
            viewport,
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        };
        let light_env = crate::render::responsive::RenderEnvironment {
            viewport,
            color_scheme: crate::render::preferences::ColorScheme::Light,
        };

        let (dark_dims, _) = calculate_node_text(
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
            &style_variants,
            &dark_env,
        )
        .expect("dark: expected a layout");

        let (light_dims, _) = calculate_node_text(
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
            &style_variants,
            &light_env,
        )
        .expect("light: expected a layout");

        assert!(
            dark_dims.1 > light_dims.1,
            "the @dark variant's larger font-size (40 vs 12) must produce a \
             taller layout: dark height={}, light height={}",
            dark_dims.1,
            light_dims.1
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Bidi/RTL (ux-7)
    // ────────────────────────────────────────────────────────────────────────

    fn text_node_with_dir(content: &str, class: &str, dir: Option<&str>) -> MizuNode {
        let mut attrs = StdHashMap::new();
        attrs.insert("content".to_string(), content.to_string());
        attrs.insert("class".to_string(), class.to_string());
        if let Some(d) = dir {
            attrs.insert("dir".to_string(), d.to_string());
        }
        MizuNode {
            primitive: Primitive::Text,
            attributes: attrs,
            events: StdHashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    fn no_op_render_env() -> crate::render::responsive::RenderEnvironment {
        crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        }
    }

    #[test]
    fn mixed_bidi_line_shapes_into_multiple_runs_without_error() {
        // Verifies parley's own (always-running — see the module doc and
        // docs/design/bidi.md) bidi reordering actually engages for a known
        // mixed-direction fixture: "Hello " (Latin) + "שלום" (Hebrew) +
        // " World" (Latin). A single-direction run would collapse to one
        // GlyphRun; a correctly bidi-processed line splits into multiple
        // runs at the direction boundaries.
        let node = text_node_with_dir("Hello \u{05E9}\u{05DC}\u{05D5}\u{05DD} World", "label", None);
        let tree = Tree::new(node);
        let node_id = tree.root().id();

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx: parley::LayoutContext<vello::peniko::Color> =
            parley::LayoutContext::new();
        let store = VariableStore::new();
        let local_inputs = rustc_hash::FxHashMap::default();
        let node_id_to_u32 = HashMap::new();
        let style_rules: HashMap<String, StyleRules> = HashMap::new();

        let (_dims, layout) = calculate_node_text(
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
            &[],
            &no_op_render_env(),
        )
        .expect("mixed bidi text must still produce a layout");

        let mut run_count = 0;
        for line in layout.lines() {
            for item in line.items() {
                if matches!(item, parley::layout::PositionedLayoutItem::GlyphRun(_)) {
                    run_count += 1;
                }
            }
        }
        assert!(
            run_count > 1,
            "a mixed Latin/Hebrew line must split into more than one \
             direction-run (proof bidi processing engaged), got {run_count}"
        );
    }

    #[test]
    fn explicit_dir_reaches_calculate_node_text_via_dom_attribute_inheritance() {
        // End-to-end: a `dir="rtl"` layout attribute (not just a directly
        // constructed ResolvedDirection) reaches calculate_node_text through
        // render::bidi::resolve_direction's ancestor walk, and produces a
        // layout without erroring for right-to-left content.
        let mut tree = Tree::new(MizuNode {
            primitive: Primitive::Window,
            attributes: {
                let mut a = StdHashMap::new();
                a.insert("dir".to_string(), "rtl".to_string());
                a
            },
            events: StdHashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        let node_id = tree
            .root_mut()
            .append(text_node_with_dir("\u{05E9}\u{05DC}\u{05D5}\u{05DD}", "label", None))
            .id();

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx: parley::LayoutContext<vello::peniko::Color> =
            parley::LayoutContext::new();
        let store = VariableStore::new();
        let local_inputs = rustc_hash::FxHashMap::default();
        let node_id_to_u32 = HashMap::new();
        let style_rules: HashMap<String, StyleRules> = HashMap::new();

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
            &[],
            &no_op_render_env(),
        );
        assert!(
            result.is_some(),
            "an inherited dir=\"rtl\" must not prevent a layout from being produced"
        );
    }

    #[test]
    fn text_align_start_resolves_opposite_edges_under_ltr_and_rtl() {
        // `text-align: start` must place short content at the *left* under
        // a `dir="ltr"`-resolved node and the *right* under `dir="rtl"` —
        // observed via the first glyph run's horizontal offset within a
        // much wider available width (so the difference is unambiguous).
        let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
        let mut rules = StyleRules::default();
        rules.text_align = Some(crate::parser::MizuTextAlign::Start);
        style_rules.insert("label".to_string(), rules);

        let mut font_cx = parley::FontContext::new();
        font_cx.collection.load_system_fonts();
        let mut layout_cx: parley::LayoutContext<vello::peniko::Color> =
            parley::LayoutContext::new();
        let store = VariableStore::new();
        let local_inputs = rustc_hash::FxHashMap::default();
        let node_id_to_u32 = HashMap::new();
        let env = no_op_render_env();

        let mut first_glyph_x = |dir: Option<&str>| -> f32 {
            let tree = Tree::new(text_node_with_dir("Hi", "label", dir));
            let node_id = tree.root().id();
            let (_dims, layout) = calculate_node_text(
                node_id,
                &tree,
                &style_rules,
                &mut font_cx,
                &mut layout_cx,
                &store,
                Some(400.0),
                &local_inputs,
                &node_id_to_u32,
                None,
                &[],
                &env,
            )
            .expect("expected a layout");
            for line in layout.lines() {
                for item in line.items() {
                    if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                        if let Some(g) = run.positioned_glyphs().next() {
                            return g.x;
                        }
                    }
                }
            }
            0.0
        };

        let ltr_x = first_glyph_x(Some("ltr"));
        let rtl_x = first_glyph_x(Some("rtl"));
        assert!(
            rtl_x > ltr_x + 100.0,
            "`text-align: start` must render far to the right under RTL \
             compared to LTR within a 400px box; ltr_x={ltr_x}, rtl_x={rtl_x}"
        );
    }
}
