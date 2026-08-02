//! Native Vello-based browser chrome rendering.
//!
//! This module replaces the former egui-based `chrome.rs`. It renders the
//! URL bar, navigation buttons, and loading indicator directly into the Vello
//! [`vello::Scene`] without any egui intermediate pass.
//!
//! All layout coordinates are **logical pixels**. The caller is responsible for
//! applying `Affine::scale(dpi_scale)` so that everything scales correctly on
//! high-DPI displays.
//!
//! Split by concern: [`geometry`] (constants), [`types`] (`ChromeState` and
//! the hit-zone/layout types), [`state`] (`ChromeState`'s editing behavior),
//! [`hit_zones`] (point → zone resolution), [`cursor`] (Parley-aware
//! cursor/selection movement), [`text_layout`] (the shared text-layout
//! helper), and [`paint`] (the main paint function). Every public item is
//! re-exported here so `crate::render::chrome_vello::X` resolves exactly as
//! it did before the split.

#![forbid(unsafe_code)]

use parley::style::{FontFamily, FontFamilyName, GenericFamily, LineHeight, StyleProperty};
use vello::{
    Scene,
    kurbo::{Affine, BezPath, Circle, Rect, RoundedRect, Stroke},
    peniko::{BlendMode, Color, Compose, Fill, Mix},
};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::render::preferences::ChromePalette;

mod cursor;
mod geometry;
mod hit_zones;
mod paint;
mod state;
#[cfg(test)]
mod tests;
mod text_layout;
mod types;

pub use geometry::*;
pub use hit_zones::*;
pub use paint::*;
pub use types::*;
