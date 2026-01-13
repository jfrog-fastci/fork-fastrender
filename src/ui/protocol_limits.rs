//! Safety limits for untrusted UI↔worker protocol payloads.
//!
//! The browser UI treats all [`crate::ui::messages::WorkerToUi`] messages as untrusted: a compromised
//! renderer process could attempt to send arbitrarily large strings to cause OOM, UI hangs, or
//! spoofing (control characters, extremely long URLs/titles, etc).
//!
//! These limits are intentionally conservative (KiB, not MiB) and are applied before storing
//! worker-provided strings in the UI state model.

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

