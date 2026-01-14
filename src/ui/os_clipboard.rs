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
///
/// Security/perf: the OS clipboard can contain arbitrarily large text. We clamp the returned string
/// to `ui::clipboard::MAX_CLIPBOARD_TEXT_BYTES` so UI→worker paste messages cannot allocate or ship
/// unbounded data.
pub fn read_text() -> Option<String> {
  let mut clipboard = Clipboard::new().ok()?;
  let mut text = clipboard.get_text().ok()?;
  crate::ui::clipboard::clamp_clipboard_text_in_place(&mut text);
  Some(text)
}

/// Write UTF-8 text to the OS clipboard.
///
/// Returns `true` when the clipboard write succeeded.
///
/// Best-effort: callers may ignore failures (for example in headless CI environments).
///
/// Security/perf: clamp text to `ui::clipboard::MAX_CLIPBOARD_TEXT_BYTES` so callers never pass
/// attacker-controlled huge strings into OS clipboard APIs.
pub fn write_text(text: &str) -> bool {
  let text = crate::ui::clipboard::clamp_clipboard_text(text);
  let Ok(mut clipboard) = Clipboard::new() else {
    return false;
  };
  clipboard.set_text(text.to_string()).is_ok()
}
