#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::time::Duration;
use url::Url;

// Navigation + rendering on CI can take a few seconds when tests run in parallel; keep this
// generous to avoid flakes.
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn click_submit_navigates_to_get_form_submission_url() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #q { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="result.html">
      <input id="q" name="q" value="a b">
      <input id="submit" type="submit" value="Go">
    </form>
  </body>
</html>
"#,
  );
  let _result_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>ok</body></html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-form-submit").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 160), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for the initial frame so hit testing works.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));

  // Drain any queued messages (navigation committed, loading state, etc) so assertions are scoped
  // to the submit click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer up");

  let mut expected = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("q=a+b"));
  let expected_url = expected.to_string();

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url == &expected_url)
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationStarted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &expected_url)
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}
