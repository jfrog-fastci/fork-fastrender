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

/// Maximum bytes kept for clipboard text set by the worker.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024; // 64 KiB

/// Maximum bytes kept for a download file name reported by the worker.
pub const MAX_DOWNLOAD_FILE_NAME_BYTES: usize = 8 * 1024; // 8 KiB

