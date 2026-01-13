#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;

use super::support::{
  create_tab_msg, navigate_msg, pointer_down, pointer_up, recv_for_tab, viewport_changed_msg, TempSite,
  DEFAULT_TIMEOUT,
};

#[test]
fn untrusted_page_cannot_navigate_to_chrome_action_or_chrome_scheme() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            a {
              position: absolute;
              left: 0;
              width: 240px;
              height: 80px;
              display: block;
              font: 24px/80px sans-serif;
              color: #fff;
              text-decoration: none;
              padding-left: 10px;
              box-sizing: border-box;
            }
            #act { top: 0; background: rgb(200, 0, 0); }
            #chr { top: 100px; background: rgb(0, 140, 0); }
          </style>
        </head>
        <body>
          <a id="act" href="chrome-action:back">chrome-action</a>
          <a id="chr" href="chrome://styles/chrome.css">chrome://</a>
        </body>
      </html>
    "#,
  );

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-untrusted-chrome-schemes-test")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (300, 220), 1.0))
    .expect("viewport");
  ui_tx
    .send(navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for the initial frame so we can send deterministic click coordinates.
  recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("FrameReady for initial page load");
  while ui_rx.try_recv().is_ok() {}

  let click_and_assert_navigation_failed = |(x, y): (f32, f32), expected_url: &str| {
    ui_tx
      .send(pointer_down(tab_id, (x, y), PointerButton::Primary))
      .expect("pointer down");
    ui_tx
      .send(pointer_up(tab_id, (x, y), PointerButton::Primary))
      .expect("pointer up");

    let Some(msg) = recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
      matches!(msg, WorkerToUi::NavigationFailed { url, .. } if url == expected_url)
    }) else {
      panic!("timed out waiting for NavigationFailed for {expected_url}");
    };

    let WorkerToUi::NavigationFailed { error, .. } = msg else {
      unreachable!();
    };

    let lowered = error.to_ascii_lowercase();
    assert!(
      lowered.contains("unsupported") && lowered.contains("scheme"),
      "expected error to mention unsupported URL scheme; got: {error}"
    );
    while ui_rx.try_recv().is_ok() {}
  };

  click_and_assert_navigation_failed((10.0, 10.0), "chrome-action:back");
  click_and_assert_navigation_failed((10.0, 110.0), "chrome://styles/chrome.css");

  drop(ui_tx);
  join.join().expect("join ui worker");
}

