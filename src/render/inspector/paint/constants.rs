//! Layout/typography/decoration constants shared across the paint module.

// ── Typography ───────────────────────────────────────────────────────────────

/// Size of monospace data text.
pub(super) const MONO_SIZE: f32 = 11.5;
/// Size of UI-face labels and prose.
pub(super) const UI_SIZE: f32 = 11.5;
/// Size of a section header's label — small, tracked, and uppercase.
pub(super) const HEADER_SIZE: f32 = 10.0;
/// Extra tracking applied to header labels.
pub(super) const HEADER_TRACKING: f32 = 0.7;

/// Horizontal gap between two segments of a row.
pub(super) const SEG_GAP: f32 = 6.0;
/// Narrowest an elidable segment may be squeezed before it is dropped
/// entirely — below this it is all ellipsis and no information.
pub(super) const MIN_ELIDE_WIDTH: f32 = 22.0;
/// Leading width that must survive before a row is allowed to spend any of
/// its space on right-aligned metrics.
pub(super) const MIN_LEADING_WIDTH: f32 = 90.0;

/// Side of the colour chip drawn before a colour-valued segment.
pub(super) const SWATCH_SIZE: f32 = 9.0;
/// Gap between a colour chip and the text it annotates.
pub(super) const SWATCH_GAP: f32 = 5.0;

// ── Decoration ───────────────────────────────────────────────────────────────

/// Alpha applied to the accent color for the page highlight's fill; the
/// border reuses the accent at full strength.
pub(super) const HIGHLIGHT_FILL_ALPHA: u8 = 0x2d;
/// Alpha of the vertical guides that trace the Elements tree's indentation.
pub(super) const GUIDE_ALPHA: u8 = 0x44;
/// Alpha of a section header's trailing hairline.
pub(super) const RULE_ALPHA: u8 = 0x66;

/// Height of the accent underline marking the active tab / engaged picker.
pub(super) const ACTIVE_UNDERLINE_H: f32 = 2.0;
/// Width of the accent bar marking the selected row.
pub(super) const SELECTION_BAR_W: f32 = 2.0;

/// Width of the scrollbar thumb, and its inset from the panel's right edge.
pub(super) const SCROLLBAR_W: f32 = 4.0;
pub(super) const SCROLLBAR_INSET: f32 = 3.0;
/// Shortest the scroll thumb may become, so it stays grabbable in a long log.
pub(super) const MIN_THUMB_H: f32 = 26.0;
