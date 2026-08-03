//! Tests for the text_engine module.

use super::*;
use rustc_hash::FxHashMap;

/// The "No Tofu" coverage bar (ux-3, modeled on Noto's own coverage
/// benchmark): every script here must shape without a single `.notdef`
/// (glyph id 0) glyph through the real `calculate_node_text` path — the
/// same generic-family resolution + embedded-font script fallback
/// documented in the module doc's determinism note (superseded from
/// system-only to embedded-only). Table-driven so a regression in fallback
/// for any single script fails loudly and by name.
///
/// Scoped to exactly the 11 scripts `embedded_fonts::new_font_context`
/// bundles. Bengali and emoji were part of the old system-only coverage
/// bar (OS fonts covered them) but IBM Plex ships neither — they are a
/// known, accepted gap, not tested here (see module doc).
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
    ("Thai", "สวัสดีชาวโลก"),
];

fn text_node(content: &str) -> MizuNode {
    let mut attrs = FxHashMap::default();
    attrs.insert("content".to_string(), content.to_string());
    MizuNode {
        primitive: Primitive::Text,
        attributes: attrs,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

#[test]
fn script_coverage_bar_renders_without_tofu() {
    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let style_rules: HashMap<String, StyleRules> = HashMap::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();

    let mut failures = Vec::new();
    for &(label, sample) in COVERAGE_BAR {
        let tree = Tree::new(text_node(sample));
        let node_id = tree.root().id();
        let Some((_dims, layout)) = calculate_node_text(
            node_id,
            None,
            &mut TextLayoutContext {
                dom: &tree,
                style_rules: &style_rules,
                font_cx: &mut font_cx,
                layout_cx: &mut layout_cx,
                store: &store,
                local_inputs: &local_inputs,
                node_id_to_u32: &node_id_to_u32,
                focused_input: None,
                style_variants: &[],
                render_env: &crate::render::responsive::RenderEnvironment {
                    viewport: crate::render::responsive::ViewportSize {
                        width: 800.0,
                        height: 600.0,
                    },
                    color_scheme: crate::render::preferences::ColorScheme::Dark,
                },
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
    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();

    for generic in [
        crate::parser::MizuFontFamily::SansSerif,
        crate::parser::MizuFontFamily::Serif,
        crate::parser::MizuFontFamily::Monospace,
    ] {
        let mut attrs = FxHashMap::default();
        attrs.insert("content".to_string(), "Hello".to_string());
        attrs.insert("class".to_string(), "label".to_string());
        let node = MizuNode {
            primitive: Primitive::Text,
            attributes: attrs,
            events: FxHashMap::default(),
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
            None,
            &mut TextLayoutContext {
                dom: &tree,
                style_rules: &style_rules,
                font_cx: &mut font_cx,
                layout_cx: &mut layout_cx,
                store: &store,
                local_inputs: &local_inputs,
                node_id_to_u32: &node_id_to_u32,
                focused_input: None,
                style_variants: &[],
                render_env: &crate::render::responsive::RenderEnvironment {
                    viewport: crate::render::responsive::ViewportSize {
                        width: 800.0,
                        height: 600.0,
                    },
                    color_scheme: crate::render::preferences::ColorScheme::Dark,
                },
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

    let mut attrs = FxHashMap::default();
    attrs.insert("content".to_string(), "Hi".to_string());
    attrs.insert("class".to_string(), "label".to_string());
    let node = MizuNode {
        primitive: Primitive::Text,
        attributes: attrs,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    };
    let tree = Tree::new(node);
    let node_id = tree.root().id();

    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
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
        None,
        &mut TextLayoutContext {
            dom: &tree,
            style_rules: &style_rules,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            store: &store,
            local_inputs: &local_inputs,
            node_id_to_u32: &node_id_to_u32,
            focused_input: None,
            style_variants: &style_variants,
            render_env: &dark_env,
        },
    )
    .expect("dark: expected a layout");

    let (light_dims, _) = calculate_node_text(
        node_id,
        None,
        &mut TextLayoutContext {
            dom: &tree,
            style_rules: &style_rules,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            store: &store,
            local_inputs: &local_inputs,
            node_id_to_u32: &node_id_to_u32,
            focused_input: None,
            style_variants: &style_variants,
            render_env: &light_env,
        },
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
// Lang (ux-8)
// ────────────────────────────────────────────────────────────────────────

fn text_node_with_lang(content: &str, lang: Option<&str>) -> MizuNode {
    let mut attrs = FxHashMap::default();
    attrs.insert("content".to_string(), content.to_string());
    if let Some(l) = lang {
        attrs.insert("lang".to_string(), l.to_string());
    }
    MizuNode {
        primitive: Primitive::Text,
        attributes: attrs,
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    }
}

#[test]
fn lang_reaches_calculate_node_text_via_dom_attribute_inheritance() {
    // End-to-end: a `lang="ja"` on a `doc` ancestor reaches
    // calculate_node_text through render::bidi::resolve_lang's ancestor
    // walk (mirroring the `dir` test above) and doesn't prevent a layout
    // from being produced — the Locale StyleProperty this feeds is
    // consumed inside fontique's fallback query, which has no public
    // introspection hook, so "still produces a layout, unaffected" is
    // the observable-from-here guarantee.
    let mut tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: {
            let mut a = FxHashMap::default();
            a.insert("lang".to_string(), "ja".to_string());
            a
        },
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let node_id = tree
        .root_mut()
        .append(text_node_with_lang("こんにちは", None))
        .id();

    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();
    let style_rules: HashMap<String, StyleRules> = HashMap::new();

    let result = calculate_node_text(
        node_id,
        None,
        &mut TextLayoutContext {
            dom: &tree,
            style_rules: &style_rules,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            store: &store,
            local_inputs: &local_inputs,
            node_id_to_u32: &node_id_to_u32,
            focused_input: None,
            style_variants: &[],
            render_env: &no_op_render_env(),
        },
    );
    assert!(
        result.is_some(),
        "an inherited lang=\"ja\" must not prevent a layout from being produced"
    );
}

#[test]
fn absent_lang_does_not_prevent_a_layout() {
    // No `lang` anywhere in the ancestor chain: resolve_lang returns
    // `None`, no Locale StyleProperty is pushed, and shaping proceeds
    // exactly as it did before ux-8.
    let tree = Tree::new(text_node_with_lang("Hello", None));
    let node_id = tree.root().id();

    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();
    let style_rules: HashMap<String, StyleRules> = HashMap::new();

    let result = calculate_node_text(
        node_id,
        None,
        &mut TextLayoutContext {
            dom: &tree,
            style_rules: &style_rules,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            store: &store,
            local_inputs: &local_inputs,
            node_id_to_u32: &node_id_to_u32,
            focused_input: None,
            style_variants: &[],
            render_env: &no_op_render_env(),
        },
    );
    assert!(
        result.is_some(),
        "no lang anywhere must still produce a layout"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Bidi/RTL (ux-7)
// ────────────────────────────────────────────────────────────────────────

fn text_node_with_dir(content: &str, class: &str, dir: Option<&str>) -> MizuNode {
    let mut attrs = FxHashMap::default();
    attrs.insert("content".to_string(), content.to_string());
    attrs.insert("class".to_string(), class.to_string());
    if let Some(d) = dir {
        attrs.insert("dir".to_string(), d.to_string());
    }
    MizuNode {
        primitive: Primitive::Text,
        attributes: attrs,
        events: FxHashMap::default(),
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
    let node = text_node_with_dir(
        "Hello \u{05E9}\u{05DC}\u{05D5}\u{05DD} World",
        "label",
        None,
    );
    let tree = Tree::new(node);
    let node_id = tree.root().id();

    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();
    let style_rules: HashMap<String, StyleRules> = HashMap::new();

    let (_dims, layout) = calculate_node_text(
        node_id,
        None,
        &mut TextLayoutContext {
            dom: &tree,
            style_rules: &style_rules,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            store: &store,
            local_inputs: &local_inputs,
            node_id_to_u32: &node_id_to_u32,
            focused_input: None,
            style_variants: &[],
            render_env: &no_op_render_env(),
        },
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
        primitive: Primitive::Doc,
        attributes: {
            let mut a = FxHashMap::default();
            a.insert("dir".to_string(), "rtl".to_string());
            a
        },
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let node_id = tree
        .root_mut()
        .append(text_node_with_dir(
            "\u{05E9}\u{05DC}\u{05D5}\u{05DD}",
            "label",
            None,
        ))
        .id();

    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();
    let style_rules: HashMap<String, StyleRules> = HashMap::new();

    let result = calculate_node_text(
        node_id,
        None,
        &mut TextLayoutContext {
            dom: &tree,
            style_rules: &style_rules,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            store: &store,
            local_inputs: &local_inputs,
            node_id_to_u32: &node_id_to_u32,
            focused_input: None,
            style_variants: &[],
            render_env: &no_op_render_env(),
        },
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

    let mut font_cx = crate::render::embedded_fonts::new_font_context();
    let mut layout_cx: parley::LayoutContext<vello::peniko::Color> = parley::LayoutContext::new();
    let store = VariableStore::new().freeze();
    let local_inputs = rustc_hash::FxHashMap::default();
    let node_id_to_u32 = HashMap::new();
    let env = no_op_render_env();

    let mut first_glyph_x = |dir: Option<&str>| -> f32 {
        let tree = Tree::new(text_node_with_dir("Hi", "label", dir));
        let node_id = tree.root().id();
        let (_dims, layout) = calculate_node_text(
            node_id,
            Some(400.0),
            &mut TextLayoutContext {
                dom: &tree,
                style_rules: &style_rules,
                font_cx: &mut font_cx,
                layout_cx: &mut layout_cx,
                store: &store,
                local_inputs: &local_inputs,
                node_id_to_u32: &node_id_to_u32,
                focused_input: None,
                style_variants: &[],
                render_env: &env,
            },
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
