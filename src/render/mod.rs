//! Rendering and window management subsystem.

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
