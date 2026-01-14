#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

use super::support::{create_tab_msg, drain_for, scroll_msg, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT};

#[test]
fn page_a11y_does_not_reemit_full_subtree_on_scroll_bursts() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  // Build a long, moderately large DOM so the accessibility subtree is non-trivial.
  let mut body = String::new();
  for i in 0..800usize {
    body.push_str(&format!("<div class=\"row\">Row {i}</div>\n"));
  }
  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; }}
          .row {{ height: 20px; }}
        </style>
      </head>
      <body>
        {body}
      </body>
    </html>
    "#
  );

  let site = TempSite::new();
  let url = site.write("index.html", &html);

  let handle = spawn_ui_worker("fastr-ui-worker-page-a11y-dedup-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(create_tab_msg(tab_id, Some(url)))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (320, 240), 1.0))
    .expect("ViewportChanged");

  // Wait for the first paint so the document has cached layout artifacts.
  let _ = super::support::recv_until(&ui_rx, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id)
  })
  .expect("wait for initial FrameReady");

  // Enable page accessibility and wait for the initial subtree.
  ui_tx
    .send(UiToWorker::SetPageA11yEnabled {
      tab_id,
      enabled: true,
    })
    .expect("enable page a11y");
  let _ = super::support::recv_until(&ui_rx, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::PageAccessKitSubtree { tab_id: got, .. } if *got == tab_id)
  })
  .expect("wait for initial PageAccessKitSubtree");

  // Drain any follow-up messages triggered by enabling.
  let _ = drain_for(&ui_rx, Duration::from_millis(200));

  // Spam scroll input. The worker will coalesce scroll bursts into fewer paints, but it must not
  // rebuild/re-emit the full page accessibility subtree for each scroll frame.
  for _ in 0..50 {
    ui_tx
      .send(scroll_msg(tab_id, (0.0, 40.0), None))
      .expect("Scroll");
  }

  let msgs = drain_for(&ui_rx, Duration::from_secs(2));

  let frames_after_scroll = msgs
    .iter()
    .filter(|msg| matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id))
    .count();
  assert!(
    frames_after_scroll > 0,
    "expected at least one FrameReady after scroll burst; got messages:\n{}",
    super::support::format_messages(&msgs)
  );

  let subtree_after_scroll = msgs
    .iter()
    .filter(|msg| {
      matches!(msg, WorkerToUi::PageAccessKitSubtree { tab_id: got, .. } if *got == tab_id)
    })
    .count();
  assert!(
    subtree_after_scroll <= 1,
    "expected scroll burst to not trigger repeated PageAccessKitSubtree messages; got messages:\n{}",
    super::support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
