use fastrender::ui::messages::{TabId, WorkerToUi};
use fastrender::ui::protocol_limits::{
  sanitize_worker_to_ui_clipboard_message, MAX_CLIPBOARD_TEXT_BYTES,
};

#[test]
fn ui_set_clipboard_text_is_bounded_before_os_clipboard() {
  // Construct a payload that crosses the byte limit *inside* a multibyte UTF-8 sequence so we
  // exercise boundary-safe truncation.
  let tab_id = TabId::new();
  let mut text = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES - 1);
  text.push('é'); // 2-byte UTF-8 sequence.
  let original_bytes = text.len();
  assert!(
    original_bytes > MAX_CLIPBOARD_TEXT_BYTES,
    "expected test payload to exceed limit"
  );

  let (msg, clipboard) =
    sanitize_worker_to_ui_clipboard_message(WorkerToUi::SetClipboardText { tab_id, text });

  let clipboard = clipboard.expect("expected clipboard sanitize result");
  assert_eq!(clipboard.tab_id, tab_id);
  assert!(clipboard.truncated);
  assert_eq!(clipboard.original_bytes, original_bytes);
  assert!(
    clipboard.text.len() <= MAX_CLIPBOARD_TEXT_BYTES,
    "expected clipboard text bounded to <= {MAX_CLIPBOARD_TEXT_BYTES} bytes, got {}",
    clipboard.text.len()
  );
  assert_eq!(clipboard.text.len(), MAX_CLIPBOARD_TEXT_BYTES - 1);

  // The reducer path should not retain the oversized payload.
  match msg {
    WorkerToUi::SetClipboardText { tab_id: msg_tab, text } => {
      assert_eq!(msg_tab, tab_id);
      assert!(
        text.is_empty(),
        "expected clipboard payload stripped before reducer"
      );
    }
    other => panic!("expected SetClipboardText message, got {other:?}"),
  }
}
