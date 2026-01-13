#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_ismap_click_appends_coords() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      img { width: 100px; height: 100px; display: block; }
    </style>
  </head>
  <body>
    <a id="link" href="target.html"><img id="img" ismap src="img.svg"></a>
  </body>
</html>
"#,
  );
  let _img_url = site.write(
    "img.svg",
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
  <rect width="100" height="100" fill="rgb(255, 0, 0)"/>
</svg>
"#,
  );
  let _target_url = site.write(
    "target.html",
    r#"<!doctype html>
<html><body>target</body></html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-ismap-click").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      index_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for the initial frame so hit testing works.
  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }

  // Drain any queued messages (navigation committed, loading state, repaints, etc) so assertions
  // are scoped to the server-side image map click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  let click_pos_css = (10.2, 20.7);
  ui_tx
    .send(support::pointer_down(
      tab_id,
      click_pos_css,
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      click_pos_css,
      PointerButton::Primary,
    ))
    .expect("pointer up");

  let expected_url = Url::parse(&index_url)
    .expect("parse index url")
    .join("target.html?10,20")
    .expect("resolve expected target url")
    .to_string();

  let nav_started = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationStarted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationStarted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  match nav_started {
    WorkerToUi::NavigationStarted { url, .. } => {
      assert_eq!(
        url, expected_url,
        "expected server-side image map click to append `?x,y` coordinates"
      );
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("expected NavigationStarted/NavigationFailed, got {other:?}"),
  }

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &expected_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  drop(ui_tx);
  join.join().expect("join ui worker");
}

