//! HTML/CSS-based browser chrome ("chrome frame") helpers.
//!
//! This module is part of the renderer-chrome workstream: rendering the browser UI using
//! FastRender itself.

pub mod context_menu;
pub mod document;
pub mod dom_mutation;
pub mod modal_dialog;
pub mod state_to_html;
pub mod status_bar;
pub mod dialog;
pub mod ids;
pub mod event;
pub mod runtime;
mod theme;
pub mod geometry;

pub use document::{ChromeFrameDocument, ChromeFrameOutput};
pub use state_to_html::chrome_frame_html_from_state;
pub use event::ChromeFrameEvent;
pub use ids::{
  CHROME_ADDRESS_BAR_ID, CHROME_ADDRESS_FORM_ID, CHROME_CONTENT_FRAME_ID, CHROME_NEW_TAB_ID,
  CHROME_OMNIBOX_POPUP_ID, CHROME_TAB_STRIP_ID, CHROME_TOOLBAR_ID,
};
pub use runtime::{ChromeFrameRuntime, ChromeFrameRuntimeOutput};
pub use status_bar::StatusBarDocument;

#[cfg(test)]
mod clipboard_ime_tests;
