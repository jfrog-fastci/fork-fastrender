//! Regression tests for treating `WorkerToUi` messages as untrusted input.
//!
//! In a multiprocess browser, `WorkerToUi` arrives over an IPC boundary from an untrusted renderer
//! process. Even in the thread-based worker configuration, we want to enforce the same trust
//! boundary so malicious or malformed messages cannot spoof UI state or crash the browser.

use fastrender::ui::messages::{CursorKind, TabId, WorkerToUi};
use fastrender::ui::{BrowserAppState, BrowserTabState};

fn has_control_chars(s: &str) -> bool {
  // The UI-side sanitization contract is to strip ASCII control characters. (See
  // `ui::untrusted::sanitize_untrusted_text`.)
  s.chars().any(|c| c.is_ascii_control())
}

#[test]
fn browser_validates_untrusted_worker_messages() {
  let tab_id = TabId(1);

  let mut app_state = BrowserAppState::new();
  app_state.push_tab(
    BrowserTabState::new(tab_id, "about:newtab".to_string()),
    true,
  );

  let before_url = app_state
    .tab(tab_id)
    .and_then(|t| t.current_url.clone())
    .expect("tab should have current_url");
  let before_committed = app_state
    .tab(tab_id)
    .and_then(|t| t.committed_url.clone())
    .expect("tab should have committed_url");
  let before_address_bar = app_state.chrome.address_bar_text.clone();

  // ---------------------------------------------------------------------------
  // NavigationCommitted: disallowed scheme should not spoof address bar / tab URL.
  // ---------------------------------------------------------------------------
  let malicious_url = "javascript:alert(1)".to_string();
  let committed = WorkerToUi::NavigationCommitted {
    tab_id,
    url: malicious_url.clone(),
    title: Some("owned".to_string()),
    can_go_back: true,
    can_go_forward: true,
  };

  let committed_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    app_state.apply_worker_msg(committed);
  }));
  assert!(
    committed_result.is_ok(),
    "NavigationCommitted with an invalid URL must not panic"
  );

  let tab = app_state.tab(tab_id).expect("tab should exist");
  assert_eq!(
    tab.current_url.as_deref(),
    Some(before_url.as_str()),
    "tab current_url must not be updated to a disallowed scheme"
  );
  assert_eq!(
    tab.committed_url.as_deref(),
    Some(before_committed.as_str()),
    "tab committed_url must not be updated to a disallowed scheme"
  );
  assert_ne!(
    app_state.chrome.address_bar_text, malicious_url,
    "address bar must not be spoofed by a disallowed scheme"
  );
  // Also ensure we didn't clear/change address bar state unexpectedly.
  assert_eq!(
    app_state.chrome.address_bar_text, before_address_bar,
    "address bar text should remain unchanged when dropping an invalid navigation commit"
  );

  // ---------------------------------------------------------------------------
  // RequestOpenInNewTab: invalid URL should not panic or mutate existing tab state.
  // ---------------------------------------------------------------------------
  let open_in_new_tab = WorkerToUi::RequestOpenInNewTab {
    tab_id,
    url: malicious_url.clone(),
  };

  let open_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    app_state.apply_worker_msg(open_in_new_tab);
  }));
  assert!(
    open_result.is_ok(),
    "RequestOpenInNewTab with invalid URL must not panic"
  );
  let tab = app_state.tab(tab_id).expect("tab should exist");
  assert_eq!(
    tab.current_url.as_deref(),
    Some(before_url.as_str()),
    "RequestOpenInNewTab must not mutate current_url"
  );

  // ---------------------------------------------------------------------------
  // Favicon: invalid pixel buffer length must be ignored (no panic, no update).
  // ---------------------------------------------------------------------------
  let bad_favicon = WorkerToUi::Favicon {
    tab_id,
    rgba: vec![0u8; 3],
    width: 2,
    height: 2,
  };

  let favicon_update = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    app_state.apply_worker_msg(bad_favicon)
  }))
  .expect("invalid favicon must not panic");

  assert!(
    favicon_update.favicon_ready.is_none(),
    "invalid favicon payload must be ignored"
  );
  assert!(
    app_state
      .tab(tab_id)
      .and_then(|t| t.favicon_meta.as_ref())
      .is_none(),
    "tab favicon meta must not be updated for invalid favicon payload"
  );

  // ---------------------------------------------------------------------------
  // DebugLog: control characters should be stripped and long lines truncated.
  // ---------------------------------------------------------------------------
  const LONG_LEN: usize = 250_000;
  let mut debug_line = String::with_capacity(LONG_LEN + 64);
  debug_line.push_str("prefix ");
  debug_line.push('\n');
  debug_line.push('\u{0000}');
  debug_line.push_str(&"A".repeat(LONG_LEN));
  debug_line.push('\u{001b}');
  debug_line.push_str(" suffix");

  let debug_msg = WorkerToUi::DebugLog {
    tab_id,
    line: debug_line.clone(),
  };

  let debug_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    app_state.apply_worker_msg(debug_msg);
  }));
  assert!(
    debug_result.is_ok(),
    "DebugLog with long/control payload must not panic"
  );

  let stored = app_state
    .tab(tab_id)
    .expect("tab should exist")
    .debug_log()
    .last()
    .expect("debug log line should be stored");

  assert!(
    !has_control_chars(stored),
    "expected debug log line to have control characters stripped; got: {stored:?}"
  );

  // DebugLog is intended for lightweight developer diagnostics; the browser should not store or
  // render unbounded attacker-controlled payloads.
  const MAX_DEBUG_LINE_LEN: usize = fastrender::ui::protocol_limits::MAX_DEBUG_LOG_BYTES;
  assert!(
    stored.len() <= MAX_DEBUG_LINE_LEN,
    "expected debug log line to be truncated to <= {MAX_DEBUG_LINE_LEN} bytes, got {} bytes",
    stored.len()
  );
  assert!(
    stored.len() < debug_line.len(),
    "expected debug log line to be truncated (len {} -> {})",
    debug_line.len(),
    stored.len()
  );
}

#[test]
fn hovered_url_from_worker_rejects_javascript_scheme() {
  let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let tab_id = app.active_tab_id().unwrap_or(TabId(1));

  app.apply_worker_msg(WorkerToUi::HoverChanged {
    tab_id,
    hovered_url: Some("javascript:alert(1)".to_string()),
    tooltip: None,
    cursor: CursorKind::Pointer,
  });

  let tab = app.tab(tab_id).expect("tab state should exist");
  assert!(
    tab.hovered_url.is_none(),
    "expected javascript: hovered_url to be dropped, got {:?}",
    tab.hovered_url
  );
}

#[test]
fn open_in_new_tab_request_from_worker_is_size_limited() {
  use fastrender::interaction::{FormSubmission, FormSubmissionMethod};
  use fastrender::ui::cancel::CancelGens;
  use fastrender::ui::messages::NavigationReason;
  use fastrender::ui::open_in_new_tab::open_untrusted_request_in_new_tab;
  use fastrender::ui::protocol_limits::MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES;
  use fastrender::ui::untrusted::UntrustedFormSubmissionError;
  use std::collections::HashMap;

  let mut app_state = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let before_tabs = app_state.tabs.len();
  let before_active = app_state.active_tab_id();
  let mut tab_cancel: HashMap<TabId, CancelGens> = HashMap::new();

  // Simulate a malicious worker sending an absurdly large POST body. The windowed browser UI should
  // treat this as untrusted IPC and drop it without creating a new tab or forwarding any worker
  // messages.
  let request = FormSubmission {
    url: "https://example.com/".to_string(),
    method: FormSubmissionMethod::Post,
    headers: vec![(
      "content-type".to_string(),
      "application/x-www-form-urlencoded".to_string(),
    )],
    body: Some(vec![0u8; MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES + 1]),
  };

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    open_untrusted_request_in_new_tab(
      &mut app_state,
      &mut tab_cancel,
      request,
      NavigationReason::LinkClick,
    )
  }))
  .expect("open_untrusted_request_in_new_tab must not panic");

  assert_eq!(
    result.unwrap_err(),
    UntrustedFormSubmissionError::BodyTooLarge
  );
  assert_eq!(
    app_state.tabs.len(),
    before_tabs,
    "expected no new tab to be created when dropping untrusted payload"
  );
  assert_eq!(
    app_state.active_tab_id(),
    before_active,
    "active tab should remain unchanged when dropping untrusted payload"
  );
  assert!(
    tab_cancel.is_empty(),
    "expected no cancel state to be recorded when request is dropped"
  );
}
