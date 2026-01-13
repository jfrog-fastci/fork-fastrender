//! HTML/CSS-based browser chrome ("chrome frame") helpers.
//!
//! This module is part of the renderer-chrome workstream: rendering the browser UI using
//! FastRender itself.

pub mod context_menu;
pub mod document;
pub mod dom_mutation;
pub mod modal_dialog;
pub mod state_to_html;
pub mod dialog;
mod theme;
pub mod geometry;

pub use document::{ChromeFrameDocument, ChromeFrameOutput};
pub use state_to_html::chrome_frame_html_from_state;

#[cfg(test)]
mod clipboard_ime_tests;
