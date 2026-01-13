//! Best-effort OS clipboard access helpers for windowed UIs.
//!
//! The browser UI historically relied on egui's `PlatformOutput` plumbing to write clipboard data,
//! but alternate frontends (e.g. compositor-based) may not have access to egui-winit.
//!
//! Keep this module intentionally tiny: callers should treat clipboard access as opportunistic and
//! never panic if the platform clipboard is unavailable (common in headless CI environments).

#![cfg(feature = "browser_ui")]

use arboard::Clipboard;

/// Read UTF-8 text from the OS clipboard.
///
/// Returns `None` on any error (clipboard unavailable, non-text content, etc).
pub fn read_text() -> Option<String> {
  let mut clipboard = Clipboard::new().ok()?;
  clipboard.get_text().ok()
}

/// Write UTF-8 text to the OS clipboard.
///
/// Best-effort: errors are ignored so callers don't have to special-case headless platforms.
pub fn write_text(text: &str) {
  let Ok(mut clipboard) = Clipboard::new() else {
    return;
  };
  let _ = clipboard.set_text(text.to_string());
}

