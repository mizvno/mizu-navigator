//! Window management: the `MizuWindowManager` state, the Winit event loop,
//! and navigation/input response handling.
//!
//! ## Module Layout
//!
//! * [`manager`] — `MizuWindowManager` and its lifecycle/state methods.
//! * [`navigate`] — URL resolution and navigation/network-result handling.
//! * [`input`] — form submission and clipboard/text-extraction helpers.
//! * [`focus`] — keyboard focus order (Tab/Shift-Tab) and click/submit
//!   event resolution shared between the mouse click handler and keyboard
//!   activation.
//! * [`event_loop`] — `run_window_loop`, the Winit event loop itself.
//!
//! Every item that was previously a direct member of this module is
//! re-exported below, so `crate::render::window::X` paths are unaffected by
//! this split.

#![forbid(unsafe_code)]

mod event_loop;
mod focus;
mod input;
mod manager;
mod navigate;
#[cfg(test)]
mod tests;

pub use crate::render::image_codec::{AnimatedImage, AssetSlot, Frame, decode_image_bytes};
pub use event_loop::run_window_loop;
pub use manager::MizuWindowManager;
pub(crate) use focus::is_focusable;
pub(crate) use navigate::chrome_url_to_file_sandbox_base;
