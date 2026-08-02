//! Hardware-accelerated 2D rendering pipeline using `vello`.
//!
//! ## Phase 11 additions
//!
//! * **Z-index depth sorting** — before painting its children, a node sorts
//!   them by their resolved `z-index` value (ascending).  Nodes with a higher
//!   `z-index` are drawn last, appearing on top of siblings with a lower value.
//!
//! * **Overflow clipping** — if a node's style carries `overflow hidden` or
//!   `overflow scroll`, the renderer wraps the child paint pass inside a
//!   `scene.push_layer(…)` / `scene.pop_layer()` pair so that children are
//!   hard-clipped to the container's layout rectangle.
//!
//! * **Scroll translation** — if a node has an entry in `scroll_offsets`, its
//!   children are shifted upward by the scroll offset via
//!   `Affine::translate((0, -scroll_y))` composed with the existing DPI scale
//!   transform.  The container's *own* background is painted without the
//!   translation so it always fills its layout rect.
//!
//! Split by concern: [`helpers`] (color conversion, `PaintContext`, media URL
//! resolution, conditional class evaluation) and [`paint`] (`paint_node`/
//! `paint_each`).

#![forbid(unsafe_code)]

mod helpers;
mod paint;
#[cfg(test)]
mod tests;

pub use helpers::*;
pub use paint::*;
