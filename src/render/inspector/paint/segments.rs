//! Row-segment placement: [`Placed`] (a segment resolved to a position and
//! elided text) and `place_segs` (packs a row's segments across its width,
//! sharing shortfall among elidable segments).

use crate::render::inspector::model::{Face, Flex, Tone};

use super::constants::*;
use super::text::TextCtx;

pub(super) struct Placed {
    pub(super) x: f32,
    pub(super) text: String,
    pub(super) tone: Tone,
    pub(super) face: Face,
    pub(super) swatch: Option<(u8, u8, u8, u8)>,
}

/// Extra width a segment needs for its colour chip, if it has one.
pub(super) fn swatch_width(seg: &crate::render::inspector::model::Seg) -> f32 {
    if seg.swatch.is_some() {
        SWATCH_SIZE + SWATCH_GAP
    } else {
        0.0
    }
}

/// Lays a row's segments out across `[x0, x1]`.
///
/// Fixed segments keep their natural width; trailing segments are packed
/// against `x1`; whatever is left over goes to the elidable segments, shared
/// in proportion to how much each of them wanted.  A row whose fixed content
/// alone overflows still does not bleed: the last leading segment is elided
/// as a backstop.
pub(super) fn place_segs(
    segs: &[crate::render::inspector::model::Seg],
    x0: f32,
    x1: f32,
    text: &mut TextCtx<'_>,
) -> Vec<Placed> {
    let mut placed = Vec::with_capacity(segs.len());
    if x1 <= x0 {
        return placed;
    }

    let leading: Vec<usize> = (0..segs.len())
        .filter(|&i| segs[i].flex != Flex::Trailing)
        .collect();
    let trailing: Vec<usize> = (0..segs.len())
        .filter(|&i| segs[i].flex == Flex::Trailing)
        .collect();

    // ── Reserve the trailing run, if the row can afford it ───────────────
    let mut trail_w = 0.0;
    let mut trail_widths = Vec::with_capacity(trailing.len());
    for (n, &i) in trailing.iter().enumerate() {
        let w = text.width(&segs[i].text, segs[i].face) + swatch_width(&segs[i]);
        trail_widths.push(w);
        trail_w += w + if n > 0 { SEG_GAP } else { 0.0 };
    }
    let show_trailing =
        !trailing.is_empty() && x1 - trail_w - SEG_GAP - x0 >= MIN_LEADING_WIDTH.min(x1 - x0);
    let lead_x1 = if show_trailing {
        x1 - trail_w - SEG_GAP
    } else {
        x1
    };

    // ── Fit the leading run ──────────────────────────────────────────────
    let gaps = SEG_GAP * leading.len().saturating_sub(1) as f32;
    let mut natural = Vec::with_capacity(leading.len());
    let mut fixed_w = 0.0;
    let mut flex_natural = 0.0;
    for &i in &leading {
        let w = text.width(&segs[i].text, segs[i].face) + swatch_width(&segs[i]);
        natural.push(w);
        if segs[i].flex == Flex::Fixed {
            fixed_w += w;
        } else {
            flex_natural += w;
        }
    }

    let avail = lead_x1 - x0 - gaps;
    let flex_budget = (avail - fixed_w).max(0.0);
    let squeeze = flex_natural > flex_budget && flex_natural > 0.0;

    let mut x = x0;
    for (n, &i) in leading.iter().enumerate() {
        let seg = &segs[i];
        let natural_w = natural[n];
        let sw = swatch_width(seg);
        let remaining = lead_x1 - x;
        if remaining <= 0.0 {
            break;
        }

        let (fitted, used) = if seg.flex == Flex::Fixed {
            // Fixed segments are short by contract; the elision here is a
            // backstop for a pathologically narrow panel, not a normal path.
            if natural_w <= remaining {
                (seg.text.clone(), natural_w)
            } else {
                let t = text.elide_tail(&seg.text, seg.face, remaining - sw);
                let w = text.width(&t, seg.face) + sw;
                (t, w)
            }
        } else if !squeeze {
            (seg.text.clone(), natural_w)
        } else {
            // Share the shortfall in proportion to appetite, so a row with a
            // long URL and a short label does not shrink both by half.
            let share = flex_budget * (natural_w / flex_natural);
            let target = share.min(remaining);
            if target < MIN_ELIDE_WIDTH {
                continue;
            }
            let t = match seg.flex {
                Flex::ElideMiddle => text.elide_middle(&seg.text, seg.face, target - sw),
                _ => text.elide_tail(&seg.text, seg.face, target - sw),
            };
            if t.is_empty() {
                continue;
            }
            let w = text.width(&t, seg.face) + sw;
            (t, w)
        };

        placed.push(Placed {
            x,
            text: fitted,
            tone: seg.tone,
            face: seg.face,
            swatch: seg.swatch,
        });
        x += used + SEG_GAP;
    }

    // ── Pack the trailing run against the right edge ─────────────────────
    if show_trailing {
        let mut tx = x1;
        for (n, &i) in trailing.iter().enumerate().rev() {
            tx -= trail_widths[n];
            placed.push(Placed {
                x: tx,
                text: segs[i].text.clone(),
                tone: segs[i].tone,
                face: segs[i].face,
                swatch: segs[i].swatch,
            });
            tx -= SEG_GAP;
        }
    }

    placed
}
