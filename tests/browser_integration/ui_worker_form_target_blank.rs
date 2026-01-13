#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::FormSubmissionMethod;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn get_form_target_blank_requests_open_in_new_tab_without_same_tab_navigation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
    <form action="result.html" target="_blank">
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

  let handle = spawn_ui_worker("fastr-ui-worker-form-target-blank-get").expect("spawn ui worker");
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  let mut expected = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("q=a+b"));
  let expected_url = expected.to_string();

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::RequestOpenInNewTab { .. } | WorkerToUi::NavigationStarted { .. }
    )
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for RequestOpenInNewTab (or unexpected NavigationStarted); saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  match msg {
    WorkerToUi::RequestOpenInNewTab {
      tab_id: got_tab,
      url,
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(url, expected_url);
    }
    WorkerToUi::NavigationStarted { tab_id: got_tab, url } => {
      panic!(
        "expected RequestOpenInNewTab for target=_blank GET submit, got NavigationStarted(tab={got_tab:?}, url={url})"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let followups = support::drain_for(&ui_rx, Duration::from_millis(200));
  assert!(
    !followups
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::NavigationStarted { .. })),
    "expected no same-tab NavigationStarted after target=_blank GET submit; got:\n{}",
    support::format_messages(&followups)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn post_form_target_blank_requests_open_in_new_tab_request_without_same_tab_navigation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
    <form action="result.html" method="post" target="_blank">
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

  let handle =
    spawn_ui_worker("fastr-ui-worker-form-target-blank-post").expect("spawn ui worker");
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  let expected_url = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html")
    .to_string();

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::RequestOpenInNewTabRequest { .. } | WorkerToUi::NavigationStarted { .. }
    )
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for RequestOpenInNewTabRequest (or unexpected NavigationStarted); saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  match msg {
    WorkerToUi::RequestOpenInNewTabRequest {
      tab_id: got_tab,
      request,
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(request.url, expected_url);
      assert_eq!(request.method, FormSubmissionMethod::Post);
      assert_eq!(request.body, Some(b"q=a+b".to_vec()));
      assert!(
        request.headers.iter().any(|(name, value)| {
          name.eq_ignore_ascii_case("content-type")
            && value == "application/x-www-form-urlencoded"
        }),
        "expected urlencoded Content-Type header; got headers={:?}",
        request.headers
      );
    }
    WorkerToUi::NavigationStarted { tab_id: got_tab, url } => {
      panic!(
        "expected RequestOpenInNewTabRequest for target=_blank POST submit, got NavigationStarted(tab={got_tab:?}, url={url})"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let followups = support::drain_for(&ui_rx, Duration::from_millis(200));
  assert!(
    !followups
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::NavigationStarted { .. })),
    "expected no same-tab NavigationStarted after target=_blank POST submit; got:\n{}",
    support::format_messages(&followups)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

