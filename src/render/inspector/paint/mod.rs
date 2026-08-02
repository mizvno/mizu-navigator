//! Vello/Parley painting of the inspector panel and the page highlight.
//!
//! All geometry is computed in logical pixels and scaled once via the
//! `Affine::scale(dpi)` transform, mirroring the chrome bar's approach.
//!
//! ## Fitting text, rather than clipping it
//!
//! The panel is a fixed 420 logical pixels wide and shows URLs, expressions,
//! and error messages that are routinely longer than that.  Nothing here is
//! allowed to run off the edge and be chopped by the clip rect: every row is
//! measured against the width actually available to it, and the segments
//! marked [`Flex::Elide`] / [`Flex::ElideMiddle`] absorb the shortfall with a
//! visible ellipsis, so the reader can always tell that there is more.
//!
//! Measuring is the expensive part of that, so [`TextMetrics`] caches it and
//! short-circuits the common case: data is set in monospace, and a monospace
//! ASCII run's width is a multiplication, which also makes eliding it O(1)
//! instead of a binary search over layouts.
//!
//! Split by concern: [`constants`] (typography/decoration sizes),
//! [`color`] (per-paint [`Tones`] and the `readable`/`faded` helpers),
//! [`text`] ([`TextMetrics`] and the text-building/measuring/eliding
//! helpers), [`segments`] (row-segment placement), and [`panel`] (the
//! panel/row/drawer/scrollbar/highlight painters).

#![forbid(unsafe_code)]

mod color;
mod constants;
mod panel;
mod segments;
#[cfg(test)]
mod tests;
mod text;

pub use panel::*;
pub use text::TextMetrics;
