#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Clipboard tests exercise real worker threads and rendering; allow some slack on CI.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

fn next_clipboard_text(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::SetClipboardText { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for SetClipboardText for tab {tab_id:?}"));
  match msg {
    WorkerToUi::SetClipboardText { text, .. } => text,
    other => panic!("unexpected message while waiting for SetClipboardText: {other:?}"),
  }
}

fn run_copy(html: &str) -> String {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write("index.html", html);

  let handle =
    spawn_ui_worker("fastr-ui-worker-document-selection-clipboard").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url.clone()),
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (800, 200),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Ensure layout cache exists (required for selection serialization).
  let _ = next_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  let text = next_clipboard_text(&ui_rx, tab_id);

  drop(ui_tx);
  join.join().expect("join ui worker thread");

  text
}

#[test]
fn ui_document_selection_copy_inserts_newline_between_paragraphs() {
  let text = run_copy(
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}</style><p>hello</p><p>world</p>"#,
  );
  assert_eq!(text, "hello\nworld");
}

#[test]
fn ui_document_selection_copy_inserts_newline_for_br() {
  let text = run_copy(
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}</style><p>hello<br>world</p>"#,
  );
  assert_eq!(text, "hello\nworld");
}

#[test]
fn ui_document_selection_copy_does_not_insert_newline_between_inline_spans() {
  let text = run_copy(
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}</style><p><span>hello </span><span>world</span></p>"#,
  );
  assert_eq!(text, "hello world");
}

#[test]
fn ui_document_selection_copy_serializes_simple_table_with_tabs_and_newlines() {
  let text = run_copy(
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}</style><table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>"#,
  );
  assert_eq!(text, "a\tb\nc\td");
}
#[test]
fn ui_document_selection_copy_preserves_preformatted_spaces() {
  let text = run_copy(
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}</style><pre>hello   world</pre>"#,
  );
  assert_eq!(text, "hello   world");
}

#[test]
fn ui_document_selection_copy_preserves_preformatted_trailing_spaces() {
  let text = run_copy(
    "<!doctype html><meta charset=\"utf-8\"><style>html,body{margin:0;padding:0}</style><pre>hello   </pre>",
  );
  assert_eq!(text, "hello   ");
}

#[test]
fn ui_document_selection_copy_preserves_preformatted_newlines() {
  let text = run_copy(
    "<!doctype html><meta charset=\"utf-8\"><style>html,body{margin:0;padding:0}</style><pre>hello\nworld</pre>",
  );
  assert_eq!(text, "hello\nworld");
}

#[test]
fn ui_document_selection_copy_does_not_double_newline_after_pre_trailing_newline() {
  let text = run_copy(
    "<!doctype html><meta charset=\"utf-8\"><style>html,body{margin:0;padding:0}</style><pre>hello\n</pre><p>world</p>",
  );
  assert_eq!(text, "hello\nworld");
}
