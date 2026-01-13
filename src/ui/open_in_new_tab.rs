//! Helpers for front-ends implementing "open in new tab" behaviour.
//!
//! The core `BrowserAppState` reducer intentionally does **not** create tabs in response to
//! `WorkerToUi::RequestOpenInNewTab{,Request}` messages: front-ends own tab identity (`TabId`) and
//! are responsible for allocating a new tab and issuing the corresponding `UiToWorker` messages.
//!
//! The windowed browser UI (`src/bin/browser.rs`) uses these helpers to keep the tab-creation logic
//! testable and to ensure untrusted worker payloads (notably `FormSubmission` bodies) are validated
//! before being forwarded back to the worker.

use std::collections::HashMap;

use crate::interaction::FormSubmission;
use crate::ui::cancel::CancelGens;
use crate::ui::messages::{NavigationReason, RepaintReason, UiToWorker};
use crate::ui::untrusted::{
  validate_untrusted_form_submission_for_open_in_new_tab_request, UntrustedFormSubmissionError,
};
use crate::ui::{BrowserAppState, BrowserTabState, TabId};

/// Create a new tab and navigate it using an explicit request (e.g. form POST).
///
/// This helper mutates `browser_state` and `tab_cancel` to add and activate the new tab, then
/// returns the `UiToWorker` messages that must be sent to the worker to realize the navigation.
pub fn open_request_in_new_tab_state(
  browser_state: &mut BrowserAppState,
  tab_cancel: &mut HashMap<TabId, CancelGens>,
  request: FormSubmission,
  reason: NavigationReason,
) -> (TabId, Vec<UiToWorker>) {
  let url = request.url.clone();
  let new_tab_id = TabId::new();
  let mut tab_state = BrowserTabState::new(new_tab_id, url.clone());
  tab_state.loading = true;
  tab_state.unresponsive = false;
  tab_state.last_worker_msg_at = std::time::SystemTime::now();
  let cancel: CancelGens = tab_state.cancel.clone();
  tab_cancel.insert(new_tab_id, cancel.clone());
  browser_state.push_tab(tab_state, true);

  let msgs = vec![
    UiToWorker::CreateTab {
      tab_id: new_tab_id,
      initial_url: None,
      cancel,
    },
    UiToWorker::SetActiveTab { tab_id: new_tab_id },
    UiToWorker::NavigateRequest {
      tab_id: new_tab_id,
      request,
      reason,
    },
    UiToWorker::RequestRepaint {
      tab_id: new_tab_id,
      reason: RepaintReason::Explicit,
    },
  ];

  (new_tab_id, msgs)
}

/// Validate an untrusted `FormSubmission` payload and, on success, create a new tab + navigation.
///
/// This should be used by windowed UIs when handling `WorkerToUi::RequestOpenInNewTabRequest`.
///
/// On error, no state is mutated and the caller should surface an appropriate user-facing warning
/// (e.g. via a toast).
pub fn open_untrusted_request_in_new_tab(
  browser_state: &mut BrowserAppState,
  tab_cancel: &mut HashMap<TabId, CancelGens>,
  request: FormSubmission,
  reason: NavigationReason,
) -> Result<(TabId, Vec<UiToWorker>), UntrustedFormSubmissionError> {
  let request = validate_untrusted_form_submission_for_open_in_new_tab_request(request)?;
  Ok(open_request_in_new_tab_state(
    browser_state,
    tab_cancel,
    request,
    reason,
  ))
}
