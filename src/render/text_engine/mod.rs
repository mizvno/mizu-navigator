//! # `text_engine` — Parley text layout for Mizu DOM nodes
//!
//! ## Font resolution & the determinism decision (ux-3 Part B, superseded)
//!
//! An author's `font-family` choice — one of the fixed generics
//! `sans-serif` / `serif` / `monospace`, see
//! [`crate::parser::style::MizuFontFamily`] — resolves to a single
//! `parley::GenericFamily` entry. Actual glyph coverage per script comes from
//! **fontique's script/locale fallback map**: parley's shaping pass
//! (`FontSelector` in `parley::shape`) consults
//! `fontique::Query::set_fallbacks(FallbackKey::new(script, locale))` for
//! every text run, independent of which family was requested, and picks the
//! font registered for that run's script. A single generic entry therefore
//! gets full per-script coverage while still respecting the author's
//! serif/sans/mono choice.
//!
//! This module originally made an explicit **System-only** determinism
//! choice (relying on OS-installed fonts via fontique's system backend,
//! never bundling anything). That has since been superseded by
//! [`crate::render::embedded_fonts`]: the font stack is now
//! **embedded-only** — `render::window::manager::construct` builds the
//! `FontContext` via `embedded_fonts::new_font_context`, which constructs
//! fontique's `Collection` with `system_fonts: false` and registers 11
//! IBM Plex faces (Regular+Bold) directly, covering Latin, Cyrillic, Greek,
//! Arabic, Hebrew, Devanagari, Thai, Japanese, Simplified Chinese,
//! Traditional Chinese, and Korean.
//!
//! * **Why the change:** the system backend (fontique's DirectWrite/
//!   CoreText/fontconfig FFI, gated behind parley's `system` Cargo feature)
//!   was the only unsafe code anywhere in the parley/fontique/skrifa/
//!   read-fonts stack — everything else in that stack is
//!   `#![forbid(unsafe_code)]` or measured at zero unsafe blocks. Embedding
//!   fonts and disabling the `system` feature entirely (see this crate's
//!   `Cargo.toml`) removes that FFI surface from the compiled binary, not
//!   just from the runtime code path.
//! * **Consequence accepted:** coverage is now a *fixed, audited* list
//!   instead of "whatever the OS has installed" — deterministic across
//!   machines, but narrower. Two scripts the old system-only coverage bar
//!   exercised are **not** in the embedded set and now render as tofu:
//!   **Bengali** and **emoji** (IBM Plex ships neither). Extending coverage
//!   means sourcing and embedding additional faces the same way the current
//!   11 were — not a config flag.
//! * **Verified:** `tests::script_coverage_bar_renders_without_tofu` runs
//!   the real `calculate_node_text` path against `embedded_fonts::new_font_context`
//!   for exactly the 11 embedded scripts and asserts zero `.notdef` glyphs.

#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::types::VariableStore;
use crate::parser::{
    MizuFontFamily, MizuFontStyle, MizuNode, MizuTextAlign, Primitive, StyleRules,
};
use crate::render::vello_pipeline::to_vello_color;
use ego_tree::{NodeId as EgoNodeId, Tree};
use std::borrow::Cow;
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

/// Context for [`calculate_node_text`], bundling everything loop-invariant
/// across the many nodes a single dirty-layout pass recomputes text for.
/// `node_id` and `available_width` stay direct parameters since they vary
/// per call within that pass.
pub struct TextLayoutContext<'a> {
    /// The DOM tree `node_id` is looked up against.
    pub dom: &'a Tree<MizuNode>,
    /// Active tag/class style rules.
    pub style_rules: &'a HashMap<String, StyleRules>,
    /// Parley font context.
    pub font_cx: &'a mut parley::FontContext,
    /// Parley layout context.
    pub layout_cx: &'a mut parley::LayoutContext<vello::peniko::Color>,
    /// The runtime variable store, for `{var}` interpolation.
    pub store: &'a VariableStore,
    /// Live per-node typing buffers for `input` nodes, keyed by u32 id.
    pub local_inputs: &'a rustc_hash::FxHashMap<u32, String>,
    /// Mapping of DOM node IDs to their u32 id (the `local_inputs` key space).
    pub node_id_to_u32: &'a HashMap<EgoNodeId, u32>,
    /// The currently keyboard-focused node, if any.
    pub focused_input: Option<EgoNodeId>,
    /// ux-6 breakpoint/color-scheme style variants.
    pub style_variants: &'a [crate::parser::style::StyleVariant],
    /// Current viewport size / color-scheme snapshot variants resolve against.
    pub render_env: &'a crate::render::responsive::RenderEnvironment,
}

/// Computes logical size and Parley text layout for a DOM node.
///
/// For `input` nodes the rendered text comes from `local_inputs` (the live
/// per-node typing buffers, keyed by u32 id).  An untouched, unfocused input
/// shows its `placeholder` attribute dimmed; an empty focused input renders a
/// single space so the line metrics — and therefore the box height — stay
/// stable across the empty ↔ non-empty transition.
pub fn calculate_node_text(
    node_id: EgoNodeId,
    available_width: Option<f32>,
    ctx: &mut TextLayoutContext<'_>,
) -> Option<((f32, f32), parley::Layout<vello::peniko::Color>)> {
    let node_ref = ctx.dom.get(node_id)?;
    let mizu_node = node_ref.value();

    if mizu_node.primitive == Primitive::Doc {
        return None;
    }

    let is_input = mizu_node.primitive == Primitive::Input;
    let mut is_placeholder = false;
    let raw_text = if is_input {
        let typed = ctx
            .node_id_to_u32
            .get(&node_id)
            .and_then(|u| ctx.local_inputs.get(u))
            .map(String::as_str)
            .unwrap_or("");
        if !typed.is_empty() {
            typed.to_string()
        } else if ctx.focused_input != Some(node_id)
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
    let tag_name = mizu_node.style_tag_name();
    if let Some(tag_rules) = ctx.style_rules.get(tag_name.as_ref()) {
        // merge_from(&ref) clones only the fields that actually win rather
        // than the entire StyleRules struct (avoids 3× full-clone in the
        // common tag + class + id cascade — see style.rs::merge_from).
        merged.merge_from(tag_rules);
    }
    let class_attr = mizu_node.attributes.get("class").map(String::as_str);
    if let Some(class_attr) = class_attr
        && let Some(rules) = ctx.style_rules.get(class_attr)
    {
        merged.merge_from(rules);
    }
    // Id styles — highest specificity, applied after tag and class (stored
    // `#`-prefixed in the same rules map, so it can't collide with a
    // same-named class or tag).
    //
    // Build the "#id" lookup key on a small heap String whose capacity is
    // pre-sized to id.len()+1 — avoids the generic format!("#{id}") which
    // always allocates a brand-new buffer of unknown size.
    let id_key: Option<String> = mizu_node.attributes.get("id").map(|id| {
        let mut k = String::with_capacity(id.len() + 1);
        k.push('#');
        k.push_str(id);
        k
    });
    if let Some(ref id_key) = id_key
        && let Some(rules) = ctx.style_rules.get(id_key.as_str())
    {
        merged.merge_from(rules);
    }
    // ux-6: breakpoint/color-scheme variants, applied last (after all three
    // bases), in source declaration order — see docs/design/responsive.md.
    let mut variant_selectors: Vec<&str> = vec![tag_name.as_ref()];
    if let Some(c) = class_attr {
        variant_selectors.push(c);
    }
    if let Some(ref k) = id_key {
        variant_selectors.push(k.as_str());
    }
    merged.merge_from(&crate::render::responsive::resolve_matching_variants(
        ctx.style_variants,
        &variant_selectors,
        ctx.render_env,
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

    let interpolated: String;
    // ux-7: resolved once per node via `dir` attribute inheritance.
    let dir = crate::render::bidi::resolve_direction(node_ref);

    // Build `text_to_draw` as a `Cow<'_, str>` so that:
    //  * Nodes without a bidi-mark prepend pay zero allocation (Borrowed).
    //  * Nodes that need the mark allocate exactly one String of the right
    //    capacity instead of the old `insert(0, mark)` which shifted O(N)
    //    bytes in an already-allocated buffer.
    // An explicit `dir="ltr"`/`dir="rtl"` prepends a zero-width strong mark
    // so parley's own (always-running) bidi auto-detection resolves to the
    // declared direction instead of whatever the text's first strong
    // character would otherwise imply — parley 0.10 has no public base-
    // direction override; see docs/design/bidi.md and render::bidi's doc.
    let text_to_draw: Cow<'_, str> = if mizu_node.primitive == Primitive::Input {
        match dir.prepend_mark() {
            Some(mark) => {
                let mut s = String::with_capacity(raw_text.len() + mark.len_utf8());
                s.push(mark);
                s.push_str(&raw_text);
                Cow::Owned(s)
            }
            None => Cow::Owned(raw_text),
        }
    } else {
        interpolated = ctx
            .store
            .interpolate(&raw_text)
            .unwrap_or_else(|e| match &e {
                MizuError::BindingNotFound(name) => format!("{{missing: {}}}", name),
                _ => format!("{{error: {}}}", e),
            });
        match dir.prepend_mark() {
            Some(mark) => {
                let mut s = String::with_capacity(interpolated.len() + mark.len_utf8());
                s.push(mark);
                s.push_str(&interpolated);
                Cow::Owned(s)
            }
            None => Cow::Borrowed(interpolated.as_str()),
        }
    };

    let mut builder = ctx
        .layout_cx
        .ranged_builder(ctx.font_cx, &text_to_draw, 1.0, true);

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
    let font_family =
        parley::style::FontFamily::Single(parley::style::FontFamilyName::Generic(generic_family));
    builder.push_default(parley::style::StyleProperty::FontFamily(font_family));
    // ux-8: the resolved `lang` (ancestor-inherited, see render::bidi::resolve_lang)
    // feeds fontique's per-run fallback query as a locale hint — it disambiguates
    // Han-unification fallback (e.g. picking a Japanese vs. Simplified-Chinese
    // face for CJK code points) but does not affect icu_segmenter's line/word
    // breaking, which parley invokes with locale-invariant options regardless
    // (verified against parley 0.10's `analysis` module: no `Language`/`Locale`
    // is threaded into its icu_segmenter calls, unlike `shape::FontSelector`,
    // which passes it straight into `fontique::FallbackKey::new`).
    if let Some(lang) = crate::render::bidi::resolve_lang(node_ref)
        && let Ok(locale) = lang.parse::<parley::style::Language>()
    {
        builder.push_default(parley::style::StyleProperty::Locale(Some(locale)));
    }
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
mod tests;
