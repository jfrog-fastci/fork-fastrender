//! Safety limits for untrusted UI↔worker protocol payloads.
//!
//! The browser UI treats all [`crate::ui::messages::WorkerToUi`] messages as untrusted: a compromised
//! renderer process could attempt to send arbitrarily large strings to cause OOM, UI hangs, or
//! spoofing (control characters, extremely long URLs/titles, etc).
//!
//! These limits are intentionally conservative and should be enforced before:
//! - storing worker-provided strings in the UI state model, and
//! - forwarding worker-provided payloads into OS/platform APIs (clipboard, window title, etc).

use crate::ui::messages::{TabId, WorkerToUi};

/// Maximum bytes kept for a URL shown in chrome state (address bar, hover URL, downloads, etc).
pub const MAX_URL_BYTES: usize = 16 * 1024; // 16 KiB

/// Maximum bytes kept for a document title shown in chrome/tab UI.
pub const MAX_TITLE_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum bytes kept for an error string displayed in the UI.
pub const MAX_ERROR_BYTES: usize = 16 * 1024; // 16 KiB

/// Maximum bytes kept for a warning toast string displayed in the UI.
pub const MAX_WARNING_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum bytes kept for a single worker debug log line stored in tab state.
pub const MAX_DEBUG_LOG_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum bytes kept for a find-in-page query echoed back by the worker.
pub const MAX_FIND_QUERY_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum bytes allowed in a worker-provided favicon payload.
///
/// This is intentionally kept small: favicons are used for chrome UI decoration and should never be
/// large enough to create meaningful memory/GPU pressure.
///
/// Keep this in sync with the worker-side favicon limits in `ui/render_worker.rs`.
pub const MAX_FAVICON_BYTES: usize = 32 * 32 * 4; // 4 KiB

/// Maximum bytes kept for clipboard text set by the worker.
///
/// This limit is enforced before the browser forwards the text to OS clipboard APIs.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024; // 64 KiB

/// Maximum bytes kept for a download file name reported by the worker.
pub const MAX_DOWNLOAD_FILE_NAME_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum number of flattened items allowed in a `<select>` control snapshot.
///
/// This includes both `<option>` entries and optgroup labels.
pub const MAX_SELECT_ITEMS: usize = 2048;

/// Maximum UTF-8 byte length allowed for `<select>` option labels/values surfaced to the UI.
///
/// UI code should truncate any longer strings on a character boundary.
pub const MAX_SELECT_LABEL_BYTES: usize = 1024;

/// Maximum UTF-8 byte length allowed for input picker `value` strings (date/time picker).
pub const MAX_INPUT_VALUE_BYTES: usize = 1024;

/// Maximum UTF-8 byte length allowed for `<input type=file accept>` attribute strings.
pub const MAX_ACCEPT_ATTR_BYTES: usize = 1024;

// -----------------------------------------------------------------------------
// Untrusted form submission payload limits (worker → UI)
// -----------------------------------------------------------------------------
//
// `WorkerToUi::RequestOpenInNewTabRequest` includes an owned `FormSubmission` (method + headers +
// optional body). Even though this message is usually triggered by user interaction, it still
// crosses the renderer/worker trust boundary: a compromised renderer (or malicious page that
// manages to synthesize extreme input values) must not be able to force the windowed browser UI to
// allocate/retain huge vectors and hang/crash.
//
// These limits are applied by
// `ui::untrusted::validate_untrusted_form_submission_for_open_in_new_tab_request` before the
// windowed browser UI creates a new tab and forwards the request back to the worker.

/// Maximum number of HTTP headers accepted in an untrusted `FormSubmission` payload.
pub const MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_COUNT: usize = 64;

/// Maximum total UTF-8 bytes accepted across all header name/value strings.
pub const MAX_OPEN_IN_NEW_TAB_REQUEST_TOTAL_HEADER_BYTES: usize = 16 * 1024; // 16 KiB

/// Maximum UTF-8 byte length accepted for a single header name.
pub const MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_NAME_BYTES: usize = 256;

/// Maximum UTF-8 byte length accepted for a single header value.
pub const MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_VALUE_BYTES: usize = 8 * 1024; // 8 KiB

/// Maximum bytes accepted for the request body in `RequestOpenInNewTabRequest`.
///
/// This is intentionally bounded: the UI forwards the body back to the worker over an in-memory
/// channel, so extremely large payloads could cause OOM or long GC/allocator pauses.
pub const MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES: usize = 512 * 1024; // 512 KiB

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardTextLimitResult {
  pub tab_id: TabId,
  pub text: String,
  pub truncated: bool,
  pub original_bytes: usize,
}

fn truncate_utf8_to_boundary(s: &str, max_bytes: usize) -> usize {
  if s.len() <= max_bytes {
    return s.len();
  }
  let mut end = max_bytes;
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  end
}

/// Enforce the clipboard text size limit, truncating to a valid UTF-8 boundary when needed.
///
/// The returned string always satisfies `text.len() <= MAX_CLIPBOARD_TEXT_BYTES`.
pub fn enforce_clipboard_text_limit(tab_id: TabId, mut text: String) -> ClipboardTextLimitResult {
  let original_bytes = text.len();
  if original_bytes <= MAX_CLIPBOARD_TEXT_BYTES {
    return ClipboardTextLimitResult {
      tab_id,
      text,
      truncated: false,
      original_bytes,
    };
  }

  let end = truncate_utf8_to_boundary(&text, MAX_CLIPBOARD_TEXT_BYTES);
  text.truncate(end);
  // Drop attacker-controlled excess capacity eagerly so the UI doesn't retain a huge allocation
  // between frames.
  text.shrink_to_fit();

  ClipboardTextLimitResult {
    tab_id,
    text,
    truncated: true,
    original_bytes,
  }
}

/// Apply clipboard size limits to [`WorkerToUi::SetClipboardText`].
///
/// Returns a version of the message that can be passed to shared reducers (the reducer does not
/// store clipboard contents), plus the bounded clipboard text for front-ends that integrate with
/// the OS clipboard.
pub fn sanitize_worker_to_ui_clipboard_message(
  msg: WorkerToUi,
) -> (WorkerToUi, Option<ClipboardTextLimitResult>) {
  match msg {
    WorkerToUi::SetClipboardText { tab_id, text } => {
      let result = enforce_clipboard_text_limit(tab_id, text);
      (
        WorkerToUi::SetClipboardText {
          tab_id,
          text: String::new(),
        },
        Some(result),
      )
    }
    other => (other, None),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn enforce_clipboard_text_limit_truncates_at_utf8_boundary() {
    let tab_id = TabId(1);
    let mut text = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES - 1);
    text.push('é'); // 2-byte UTF-8 sequence, forcing an unaligned boundary at MAX bytes.
    assert!(text.len() > MAX_CLIPBOARD_TEXT_BYTES);

    let result = enforce_clipboard_text_limit(tab_id, text);
    assert!(result.truncated);
    assert!(result.text.len() <= MAX_CLIPBOARD_TEXT_BYTES);
    assert_eq!(result.text.len(), MAX_CLIPBOARD_TEXT_BYTES - 1);
  }
}
