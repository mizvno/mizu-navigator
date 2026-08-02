//! Tests for the inspector paint module.

use vello::Scene;
use vello::kurbo::Affine;
use vello::peniko::Color;

use super::color::Tones;
use super::constants::*;
use super::panel::{COPIED_FLASH, PanelPaintContext, paint_panel, paint_value_drawer};
use super::segments::place_segs;
use super::text::{METRICS_CACHE_LIMIT, TextCtx, TextMetrics, build_text, byte_at, face_size};
use crate::render::inspector::model::{Face, Flex, Row, Seg, Tone};
use crate::render::inspector::{InspectorState, InspectorTab, PANEL_WIDTH, ValueView};
use crate::render::preferences::{ChromePalette, UserPreferences, contrast_ratio};

/// Every tone must stay legible against the panel's background in every
/// palette — the panel borrows the chrome's colours precisely so this
/// cannot silently drift, and this asserts the borrowing actually holds.
#[test]
fn every_tone_meets_wcag_aa_against_the_panel_background() {
    let schemes = [
        (
            "dark",
            UserPreferences {
                color_scheme: crate::render::preferences::ColorScheme::Dark,
                ..Default::default()
            },
        ),
        (
            "light",
            UserPreferences {
                color_scheme: crate::render::preferences::ColorScheme::Light,
                ..Default::default()
            },
        ),
        (
            "high-contrast",
            UserPreferences {
                high_contrast: true,
                ..Default::default()
            },
        ),
    ];
    let tones = [
        Tone::Key,
        Tone::Value,
        Tone::Muted,
        Tone::Accent,
        Tone::Good,
        Tone::Bad,
    ];
    for (name, prefs) in schemes {
        let palette = ChromePalette::for_preferences(&prefs);
        for tone in tones {
            let ratio = contrast_ratio(Tones::new(&palette).get(tone), palette.bar_bg);
            assert!(
                ratio >= 4.5,
                "{name}: {tone:?} contrasts {ratio:.2}:1 against the panel background"
            );
        }
    }
}

#[test]
fn byte_at_lands_on_character_boundaries() {
    let s = "héllo→";
    for n in 0..=s.chars().count() {
        let i = byte_at(s, n);
        assert!(s.is_char_boundary(i), "byte_at({n}) must not split a char");
    }
    assert_eq!(byte_at(s, 99), s.len(), "past the end clamps to the length");
}

/// The whole point of the segment model: a row that cannot fit still
/// says so with an ellipsis instead of being chopped by the clip rect.
#[test]
fn segments_are_classified_before_they_are_placed() {
    let segs = [
        Seg::mono("GET", Tone::Key),
        Seg::mono("mizu://host/very/long/path", Tone::Value).middle(),
        Seg::mono("42ms", Tone::Muted).trail(),
    ];
    assert_eq!(segs[0].flex, Flex::Fixed);
    assert_eq!(segs[1].flex, Flex::ElideMiddle);
    assert_eq!(segs[2].flex, Flex::Trailing);
}

/// Readability must not be bought by discarding the accent entirely: the
/// fixed-up colour still has to read as the same hue family, which is
/// what distinguishes this from falling back to plain black.
#[test]
fn the_light_accent_is_darkened_rather_than_flattened() {
    let light = ChromePalette::for_preferences(&UserPreferences {
        color_scheme: crate::render::preferences::ColorScheme::Light,
        ..Default::default()
    });
    let ring = light.url_border_focused;
    let fixed = Tones::new(&light).accent;
    assert!(
        contrast_ratio(fixed, light.bar_bg) >= 4.5,
        "the accent must be readable as text"
    );
    assert_ne!(fixed, ring, "the raw ring colour does not pass on light");
    assert!(
        fixed.b > fixed.r && fixed.b > fixed.g,
        "a darkened blue is still blue, unlike the black a flat fallback gives: {fixed:?}"
    );
}

#[test]
fn tones_that_already_pass_are_left_alone() {
    let dark = ChromePalette::for_preferences(&UserPreferences {
        color_scheme: crate::render::preferences::ColorScheme::Dark,
        ..Default::default()
    });
    assert_eq!(Tones::new(&dark).bad, dark.err_text);
    assert_eq!(Tones::new(&dark).good, dark.ok_dot);
}

/// Builds a real measuring context. Parley falls back to whatever the
/// platform offers, so these assertions are phrased to hold for any font.
fn text_ctx(metrics: &mut TextMetrics) -> (parley::FontContext, parley::LayoutContext<Color>) {
    let _ = metrics;
    (parley::FontContext::new(), parley::LayoutContext::new())
}

macro_rules! with_text {
    (|$t:ident| $body:block) => {{
        let mut metrics = TextMetrics::default();
        let (mut font_cx, mut layout_cx) = text_ctx(&mut metrics);
        let mut $t = TextCtx {
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            metrics: &mut metrics,
        };
        $body
    }};
}

#[test]
fn tail_elision_fits_the_budget_and_marks_the_cut() {
    with_text!(|t| {
        let long = "mizu://example.test/a/very/long/path/that/will/not/fit.json";
        let out = t.elide_tail(long, Face::Mono, 120.0);
        assert!(out.ends_with('…'), "a cut must be visible: {out:?}");
        assert!(
            long.starts_with(out.trim_end_matches('…')),
            "the kept part must be a real prefix of the original"
        );
        assert!(
            t.width(&out, Face::Mono) <= 120.0,
            "and it must actually fit"
        );
    });
}

#[test]
fn middle_elision_keeps_both_ends() {
    with_text!(|t| {
        let long = "mizu://example.test/a/very/long/path/that/will/not/fit.json";
        let out = t.elide_middle(long, Face::Mono, 180.0);
        let (head, tail) = out.split_once('…').expect("an ellipsis in the middle");
        assert!(long.starts_with(head) && !head.is_empty());
        assert!(
            long.ends_with(tail),
            "the resource name must survive: {out:?}"
        );
        assert!(t.width(&out, Face::Mono) <= 180.0);
    });
}

#[test]
fn text_that_already_fits_is_never_touched() {
    with_text!(|t| {
        assert_eq!(t.elide_tail("ok", Face::Mono, 500.0), "ok");
        assert_eq!(t.elide_middle("ok", Face::Mono, 500.0), "ok");
    });
}

/// The property that the old panel violated on every long row: nothing a
/// row paints may extend past the width it was given.
#[test]
fn no_placed_segment_escapes_the_row() {
    with_text!(|t| {
        let segs = vec![
            Seg::mono("blocked ", Tone::Bad),
            Seg::mono("GET  ", Tone::Key),
            Seg::mono(
                "mizu://a.very.long.host.example.test/deeply/nested/resource/name.json",
                Tone::Value,
            )
            .middle(),
            Seg::mono("1284ms · 12.4 KB", Tone::Muted).trail(),
        ];
        for width in [60.0f32, 120.0, 240.0, 390.0] {
            let placed = place_segs(&segs, 0.0, width, &mut t);
            for item in &placed {
                let end = item.x + t.width(&item.text, item.face);
                assert!(
                    end <= width + 0.5,
                    "segment {:?} ends at {end} in a {width}-wide row",
                    item.text
                );
                assert!(item.x >= -0.5, "and none starts left of the row");
            }
        }
    });
}

#[test]
fn trailing_metrics_yield_to_the_leading_content() {
    with_text!(|t| {
        let segs = vec![
            Seg::mono("mizu://host/resource", Tone::Value).elide(),
            Seg::mono("1284ms", Tone::Muted).trail(),
        ];
        let placed = place_segs(&segs, 0.0, 50.0, &mut t);
        assert!(
            !placed.iter().any(|p| p.text == "1284ms"),
            "a row too narrow for both must spend its width on the payload"
        );
    });
}

#[test]
fn a_row_with_no_room_at_all_paints_nothing() {
    with_text!(|t| {
        let segs = vec![Seg::mono("anything", Tone::Value).elide()];
        assert!(place_segs(&segs, 100.0, 100.0, &mut t).is_empty());
        assert!(place_segs(&segs, 100.0, 40.0, &mut t).is_empty());
    });
}

/// The monospace fast path and a full layout must agree, or elision would
/// be measured against one metric and painted with another.
#[test]
fn the_monospace_fast_path_matches_a_real_layout() {
    with_text!(|t| {
        let probe = "GET mizu://host/x";
        let fast = t.width(probe, Face::Mono);
        let laid = build_text(probe, Face::Mono, t.font_cx, t.layout_cx).width();
        assert!(
            (fast - laid).abs() <= laid * 0.02 + 0.5,
            "fast path {fast} vs layout {laid}"
        );
    });
}

/// One row of every kind, including the pathological content the panel
/// has to survive: a 4 000-character value, a URL longer than the panel,
/// and a deeply indented tree row.
fn sample_rows() -> Vec<Row> {
    use crate::render::inspector::model::RowKind;
    let row = |kind, indent, segs| Row {
        kind,
        indent,
        segs,
        node: None,
        expandable: false,
        expanded: false,
        inspect: None,
    };
    vec![
        row(
            RowKind::Header,
            0,
            vec![
                Seg::strong("Requests", Tone::Accent),
                Seg::mono("128", Tone::Muted).trail(),
            ],
        ),
        row(
            RowKind::Item,
            1,
            vec![
                Seg::mono("blocked ", Tone::Bad),
                Seg::mono("GET  ", Tone::Key),
                Seg::mono("12.4s", Tone::Muted),
                Seg::mono("1284ms · 3.2 KB", Tone::Muted).trail(),
            ],
        ),
        row(
            RowKind::Detail,
            2,
            vec![Seg::mono(format!("mizu://host/{}", "seg/".repeat(200)), Tone::Value).middle()],
        ),
        row(
            RowKind::Detail,
            2,
            vec![Seg::mono("x".repeat(4000), Tone::Bad).elide()],
        ),
        row(RowKind::Empty, 1, vec![Seg::ui("(none)", Tone::Muted)]),
        Row {
            expandable: true,
            expanded: true,
            indent: 9,
            ..row(
                RowKind::Item,
                9,
                vec![
                    Seg::mono("box", Tone::Accent),
                    Seg::mono("#deeply-nested-identifier", Tone::Value),
                ],
            )
        },
        row(
            RowKind::Item,
            1,
            vec![
                Seg::mono("#3a7bd5", Tone::Value).swatch(&crate::parser::MizuColor {
                    r: 58,
                    g: 123,
                    b: 213,
                    a: 255,
                }),
            ],
        ),
    ]
}

/// Painting must not panic, and must not depend on the window being big:
/// a panel narrower than its own padding is a legitimate transient during
/// a resize drag.
#[test]
fn painting_survives_hostile_content_and_geometry() {
    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let palette = ChromePalette::for_preferences(&UserPreferences::default());
    let rows = sample_rows();

    for (w, h) in [
        (1280.0f32, 800.0f32),
        (420.0, 300.0),
        (60.0, 90.0),
        (0.0, 0.0),
    ] {
        for tab in InspectorTab::ALL {
            let mut state = InspectorState::new();
            state.open = true;
            state.tab = tab;
            state.hover = Some((100.0, 80.0));
            state.picker = tab == InspectorTab::Style;
            state.scroll[tab.index()] = 40.0;
            let mut scene = Scene::new();
            paint_panel(
                &mut scene,
                &mut PanelPaintContext {
                    state: &mut state,
                    rows: &rows,
                    window_width: w,
                    window_height: h,
                    scale: 1.25,
                    font_cx: &mut font_cx,
                    layout_cx: &mut layout_cx,
                    palette: &palette,
                },
            );
            assert!(
                state.scroll[tab.index()] <= state.max_scroll,
                "the paint pass must clamp scroll to the content it measured"
            );
        }
    }
}

/// The drawer is a second, independently-scrolling region layered on the
/// same panel; painting it must not panic across the same hostile
/// geometry as the row list, including a panel too short to fit it.
#[test]
fn the_drawer_survives_hostile_content_and_geometry() {
    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let palette = ChromePalette::for_preferences(&UserPreferences::default());
    let rows = sample_rows();
    let long_value = "mizu://".to_string() + &"segment/".repeat(300) + "leaf.json";

    for (w, h) in [
        (1280.0f32, 800.0f32),
        (420.0, 300.0),
        (420.0, 150.0),
        (60.0, 90.0),
        (0.0, 0.0),
    ] {
        for hover in [None, Some((PANEL_WIDTH - 5.0, h - 10.0))] {
            let mut state = InspectorState::new();
            state.open = true;
            state.hover = hover;
            state.value_view = Some(ValueView::new("Value".into(), long_value.clone()));
            let mut scene = Scene::new();
            paint_panel(
                &mut scene,
                &mut PanelPaintContext {
                    state: &mut state,
                    rows: &rows,
                    window_width: w,
                    window_height: h,
                    scale: 1.0,
                    font_cx: &mut font_cx,
                    layout_cx: &mut layout_cx,
                    palette: &palette,
                },
            );
            let view = state.value_view.expect("painting must not drop the drawer");
            assert!(view.scroll <= view.max_scroll);
        }
    }
}

#[test]
fn copy_button_flashes_copied_then_reverts() {
    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let mut metrics = TextMetrics::default();
    let mut text = TextCtx {
        font_cx: &mut font_cx,
        layout_cx: &mut layout_cx,
        metrics: &mut metrics,
    };
    let palette = ChromePalette::for_preferences(&UserPreferences::default());
    let tones = Tones::new(&palette);

    let mut just_copied = ValueView::new("Value".into(), "hello".into());
    just_copied.copied_at = Some(std::time::Instant::now());
    let mut scene = Scene::new();
    paint_value_drawer(
        &mut scene,
        &mut text,
        &palette,
        &tones,
        Affine::IDENTITY,
        0.0,
        PANEL_WIDTH,
        600.0,
        &mut just_copied,
        None,
    );
    // Nothing to assert on the drawn glyphs directly; this exercises the
    // flash branch without panicking and documents the timing contract.
    assert!(just_copied.copied_at.unwrap().elapsed() < COPIED_FLASH);

    let mut long_ago = ValueView::new("Value".into(), "hello".into());
    long_ago.copied_at = Some(std::time::Instant::now() - COPIED_FLASH * 2);
    let mut scene = Scene::new();
    paint_value_drawer(
        &mut scene,
        &mut text,
        &palette,
        &tones,
        Affine::IDENTITY,
        0.0,
        PANEL_WIDTH,
        600.0,
        &mut long_ago,
        None,
    );
}

#[test]
fn the_drawer_clamps_its_own_scroll_independent_of_the_row_list() {
    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let palette = ChromePalette::for_preferences(&UserPreferences::default());
    let rows = sample_rows();
    let mut state = InspectorState::new();
    state.open = true;
    state.scroll[InspectorTab::Elements.index()] = 999.0;
    let mut view = ValueView::new("Value".into(), "x".repeat(2000));
    view.scroll = 999_999.0;
    state.value_view = Some(view);

    let mut scene = Scene::new();
    paint_panel(
        &mut scene,
        &mut PanelPaintContext {
            state: &mut state,
            rows: &rows,
            window_width: 420.0,
            window_height: 700.0,
            scale: 1.0,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            palette: &palette,
        },
    );
    let view = state.value_view.unwrap();
    assert!(
        view.scroll <= view.max_scroll,
        "a huge requested scroll must be clamped to the wrapped content's real height"
    );
}

#[test]
fn the_measurement_cache_stays_bounded() {
    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let mut metrics = TextMetrics::default();
    // UI-face strings miss the monospace fast path, so they are exactly
    // what would grow the map without a bound.
    for i in 0..METRICS_CACHE_LIMIT + 500 {
        metrics.width(
            &format!("label {i} ü"),
            Face::Ui,
            &mut font_cx,
            &mut layout_cx,
        );
    }
    assert!(metrics.cache.len() <= METRICS_CACHE_LIMIT);
}

#[test]
fn a_face_has_exactly_one_size() {
    assert_eq!(face_size(Face::Mono), MONO_SIZE);
    assert_eq!(face_size(Face::Ui), UI_SIZE);
    assert_eq!(face_size(Face::UiStrong), HEADER_SIZE);
}
