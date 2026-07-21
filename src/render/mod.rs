//! Rendering and window management subsystem.

/// Accent color for focus indication, shared between the chrome URL bar's
/// focused-state border and the DOM keyboard-focus ring so the two read as
/// the same visual language.
pub(crate) const FOCUS_RING_COLOR: vello::peniko::Color = vello::peniko::Color::rgba8(85, 153, 255, 255);

/// Read-only accessibility tree (accesskit), derived from the same DOM the
/// renderer paints.
pub mod accessibility;
/// Native Vello-based browser chrome (navigation bar).
pub mod chrome_vello;
/// Spatial hit-testing for input events.
pub mod hit_test;
/// In-window developer inspector panel (F12).
pub mod inspector;
/// Image and animation decoders.
pub mod image_codec;
/// Layout tree builder and translator.
pub mod layout_bridge;
/// Unified navigation policy — single choke point for all document-level
/// navigation decisions (invariants N1–N5).
pub mod navigation;
/// User preference detection (light/dark, high-contrast, reduced-motion)
/// and the theme-aware chrome palette.
pub mod preferences;
/// Capability action dispatch and sandboxing utilities.
// Security note: actions execute unconditionally; there is no per-document
// permission model. Enforcement is at the transport layer (TLS-only QUIC).
pub mod security;
/// Typography and text layout engine.
pub mod text_engine;
/// GPU-accelerated 2D graphics rendering pipeline using Vello.
pub mod vello_pipeline;
/// Window creation, event loop, and resizing management.
pub mod window;

pub use window::run_window_loop;
