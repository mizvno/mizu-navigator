//! `translate_style`: converts Mizu's `StyleRules` into native Taffy
//! `Style`, resolving viewport-relative dimensions and bidi-aware
//! logical-to-physical property mapping along the way.

use taffy::style::{FlexDirection, Overflow, Style};

use crate::parser::{MizuOverflow, StyleRules};
use crate::render::bidi::ResolvedDirection;
use crate::render::responsive::{ViewportSize, resolve_dimension};

use super::helpers::{
    to_taffy_dimension, to_taffy_length_percentage, to_taffy_length_percentage_auto,
};

/// Translates Mizu custom StyleRules into Native Taffy styles.
/// Converts percentage values (0.0 to 100.0) into fractions (0.0 to 1.0).
/// `viewport` resolves any `vw`/`vh`/`vmin`/`vmax` dimensions (ux-6) against
/// the current content viewport before handing off to Taffy, which only
/// ever sees pixels or (parent-relative) percent. `dir` (ux-7) resolves
/// `margin-inline-*`/`padding-inline-*` to a physical left/right side and
/// mirrors a `row` flex container to `RowReverse` under RTL — see
/// `docs/design/bidi.md`.
pub fn translate_style(
    rules: &StyleRules,
    viewport: ViewportSize,
    dir: ResolvedDirection,
) -> Style {
    let mut style = Style::default();
    let is_rtl = dir.is_rtl_for_layout();

    // 1. width / height
    if let Some(dim) = &rules.width {
        style.size.width = to_taffy_dimension(resolve_dimension(dim, viewport));
    }
    if let Some(dim) = &rules.height {
        style.size.height = to_taffy_dimension(resolve_dimension(dim, viewport));
    }

    // 2. padding
    if let Some(dim) = &rules.padding {
        let taffy_val = to_taffy_length_percentage(resolve_dimension(dim, viewport));
        style.padding = taffy::geometry::Rect {
            left: taffy_val,
            right: taffy_val,
            top: taffy_val,
            bottom: taffy_val,
        };
    }
    // `padding-inline-start`/`-end` (ux-7) override just one physical side
    // of whatever the uniform `padding` above set.
    if let Some(dim) = &rules.padding_inline_start {
        let v = to_taffy_length_percentage(resolve_dimension(dim, viewport));
        if is_rtl {
            style.padding.right = v;
        } else {
            style.padding.left = v;
        }
    }
    if let Some(dim) = &rules.padding_inline_end {
        let v = to_taffy_length_percentage(resolve_dimension(dim, viewport));
        if is_rtl {
            style.padding.left = v;
        } else {
            style.padding.right = v;
        }
    }

    // 3. margin
    if let Some(dim) = &rules.margin {
        let taffy_val = to_taffy_length_percentage_auto(resolve_dimension(dim, viewport));
        style.margin = taffy::geometry::Rect {
            left: taffy_val,
            right: taffy_val,
            top: taffy_val,
            bottom: taffy_val,
        };
    }
    // `margin-inline-start`/`-end` (ux-7) — same override rule as padding.
    if let Some(dim) = &rules.margin_inline_start {
        let v = to_taffy_length_percentage_auto(resolve_dimension(dim, viewport));
        if is_rtl {
            style.margin.right = v;
        } else {
            style.margin.left = v;
        }
    }
    if let Some(dim) = &rules.margin_inline_end {
        let v = to_taffy_length_percentage_auto(resolve_dimension(dim, viewport));
        if is_rtl {
            style.margin.left = v;
        } else {
            style.margin.right = v;
        }
    }

    // 4. gap
    if let Some(dim) = &rules.gap {
        let taffy_val = to_taffy_length_percentage(resolve_dimension(dim, viewport));
        style.gap = taffy::geometry::Size {
            width: taffy_val,
            height: taffy_val,
        };
    }

    // 5. flex properties
    if let Some(flex_dir) = rules.flex_direction {
        // ux-7: a `row` container mirrors under RTL — `column` is a
        // vertical axis and unaffected by horizontal text direction. The
        // author-facing grammar only ever produces `Row`/`Column` (see
        // `parser::style::apply_property`), so this is the only
        // substitution needed; see `docs/design/bidi.md` §2.
        style.flex_direction = if is_rtl && flex_dir == FlexDirection::Row {
            FlexDirection::RowReverse
        } else {
            flex_dir
        };
    }
    if let Some(justify) = rules.justify {
        style.justify_content = Some(justify);
    }
    if let Some(align) = rules.align {
        style.align_items = Some(align);
    }

    // 7. border
    if let Some(border_width) = rules.border_width {
        let taffy_val = taffy::style::LengthPercentage::Length(border_width);
        style.border = taffy::geometry::Rect {
            left: taffy_val,
            right: taffy_val,
            top: taffy_val,
            bottom: taffy_val,
        };
    }

    // 8. overflow — maps MizuOverflow to Taffy's Point<Overflow> (x and y axis).
    let taffy_overflow = match rules.overflow {
        MizuOverflow::Visible => Overflow::Visible,
        MizuOverflow::Hidden => Overflow::Hidden,
        MizuOverflow::Scroll => Overflow::Scroll,
    };
    style.overflow = taffy::geometry::Point {
        x: taffy_overflow,
        y: taffy_overflow,
    };

    // 9. display — overrides Taffy display mode when explicitly set.
    if let Some(display) = rules.display {
        style.display = display;
    }

    style
}
