//! Chrome geometry constants (tab strip / nav bar dimensions) and the note
//! on why colors are not constants here (see `preferences::ChromePalette`).

// ── Geometry ─────────────────────────────────────────────────────────────────

/// Height of the tab strip band, above the navigation bar.
pub const TAB_STRIP_HEIGHT: f32 = 26.0;

/// Height of the navigation bar band (back/reload/forward + URL bar).
pub(super) const BAR_HEIGHT: f32 = 28.0;

/// Total height of the chrome in logical pixels — both bands.
///
/// Every content-space calculation in the renderer treats this as "the
/// document's vertical offset", so keeping the tab strip *inside* it (rather
/// than adding a second offset) is what lets the strip be introduced without
/// touching hit tests, clip rects, or viewport maths.
pub const CHROME_HEIGHT: f32 = TAB_STRIP_HEIGHT + BAR_HEIGHT;

/// Minimum/maximum width of one tab in the strip.
pub(super) const TAB_MIN_W: f32 = 80.0;
pub(super) const TAB_MAX_W: f32 = 200.0;
/// Width of a tab's close affordance, at the tab's right edge.
pub(super) const TAB_CLOSE_W: f32 = 16.0;
/// Width of the trailing "new tab" button.
pub(super) const NEW_TAB_W: f32 = 24.0;
/// Horizontal padding inside a tab, before its title.
pub(super) const TAB_TEXT_PAD: f32 = 6.0;

pub(super) const BTN_Y: f32 = TAB_STRIP_HEIGHT + 4.0;
pub(super) const BTN_H: f32 = 20.0;
pub(super) const BTN_W: f32 = 24.0;
/// The history-sidebar toggle leads the bar, on the same edge as the panel it
/// opens — a left-docked panel with a right-edge switch reads as unrelated.
pub(super) const HISTORY_X: f32 = 4.0;
pub(super) const BACK_X: f32 = 32.0;
pub(super) const FORWARD_X: f32 = 60.0;
pub(super) const RELOAD_X: f32 = 88.0;
/// The X coordinate where the URL bar starts.
pub const URL_BAR_X: f32 = 116.0;
pub(super) const URL_BAR_Y: f32 = TAB_STRIP_HEIGHT + 3.0;
pub(super) const URL_BAR_H: f32 = 22.0;
/// Width reserved for the status indicator on the right.
pub const STATUS_W: f32 = 40.0;
/// Text padding inside the URL bar.
pub(super) const URL_TEXT_PAD: f32 = 4.0;
/// Font size used throughout the chrome bar.
pub(super) const CHROME_FONT_SIZE: f32 = 12.0;

// The chrome's colors are no longer fixed constants (ux-5): they come from a
// `ChromePalette` chosen by `render::preferences::ChromePalette::for_preferences`
// from the caller's detected `UserPreferences` (light/dark, forced on
// high-contrast), passed into `paint_chrome` on every frame.
