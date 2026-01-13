#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn extract_clipboard_text(messages: Vec<WorkerToUi>) -> Option<String> {
  messages.into_iter().find_map(|msg| match msg {
    WorkerToUi::SetClipboardText { text, .. } => Some(text),
    _ => None,
  })
}

#[test]
fn ui_document_selection_double_click_selects_word_and_suppresses_link_navigation() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #link { position: absolute; top: 0; left: 10px; }
</style>
<a id="link" href="dest.html">hello</a>
"#,
  );
  let _dest_url = site.write(
    "dest.html",
    r#"<!doctype html><meta charset="utf-8"><p>dest</p>"#,
  );

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    // Load HTML from the fixture string, but keep a `file://` URL so relative href resolution works.
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #link { position: absolute; top: 0; left: 10px; }
</style>
<a id="link" href="dest.html">hello</a>
"#,
    &index_url,
    (320, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let down_msgs = controller.handle_message(support::pointer_down_with(
    tab_id,
    (20.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  assert!(
    down_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected pointer down to repaint selection state"
  );

  let up_msgs = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (20.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  assert!(
    !up_msgs.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationCommitted { .. }
        | WorkerToUi::NavigationFailed { .. }
    )),
    "double-click selection should suppress link navigation, got messages: {up_msgs:?}"
  );
  assert_eq!(
    controller.current_url(),
    index_url,
    "double-click selection should not update the current URL"
  );

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("hello"),
    "expected double-click word selection to copy just the clicked word"
  );

  Ok(())
}

#[test]
fn ui_document_selection_triple_click_selects_paragraph_or_line() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #p { position: absolute; top: 0; left: 10px; margin: 0; }
</style>
<p id="p">alpha <span>beta</span></p>
"#,
    "https://example.invalid/",
    (420, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    (40.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    3,
  ))?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (40.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("alpha beta"),
    "expected triple-click selection to copy the full paragraph/line"
  );

  Ok(())
}
