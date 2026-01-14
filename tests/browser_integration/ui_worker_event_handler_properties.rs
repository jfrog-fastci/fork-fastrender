#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;

fn wait_for_first_frame(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { .. } => {}
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn query_body_attribute(
  tx: &std::sync::mpsc::Sender<UiToWorker>,
  tab_id: TabId,
  attr: &str,
) -> Option<String> {
  let (resp_tx, resp_rx) = std::sync::mpsc::channel();
  tx.send(UiToWorker::TestQueryJsDomAttribute {
    tab_id,
    element_id: None,
    attr: attr.to_string(),
    response: resp_tx,
  })
  .expect("send TestQueryJsDomAttribute");
  resp_rx
    .recv_timeout(support::DEFAULT_TIMEOUT)
    .expect("receive TestQueryJsDomAttribute response")
}

#[test]
fn window_onscroll_handler_property_fires_on_scroll_to() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>onscroll handler property</title>
    <style>
      html, body { margin: 0; padding: 0; }
      .spacer { height: 4000px; background: linear-gradient(#eee, #ccc); }
    </style>
    <script>
      window.onscroll = () => {
        document.body.setAttribute('data-scrolled', '1');
      };
    </script>
  </head>
  <body>
    <div class="spacer">scroll</div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-onscroll-handler-property",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");

  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("send ViewportChanged");
  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  wait_for_first_frame(&handle.ui_rx, tab_id);

  handle
    .ui_tx
    .send(support::scroll_to_msg(tab_id, (0.0, 250.0)))
    .expect("send ScrollTo");

  let scrolled = query_body_attribute(&handle.ui_tx, tab_id, "data-scrolled");
  assert_eq!(
    scrolled.as_deref(),
    Some("1"),
    "expected window.onscroll handler property to fire on ScrollTo"
  );

  handle.join().expect("join ui worker");
}

#[test]
fn window_onresize_handler_property_fires_on_viewport_changed() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>onresize handler property</title>
    <style>html, body { margin: 0; padding: 0; }</style>
    <script>
      window.onresize = () => {
        document.body.setAttribute('data-resized', '1');
      };
    </script>
  </head>
  <body>resize</body>
</html>
"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-onresize-handler-property",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");

  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (160, 120), 1.0))
    .expect("send ViewportChanged");
  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  wait_for_first_frame(&handle.ui_rx, tab_id);

  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 140), 1.0))
    .expect("send ViewportChanged resize");

  let resized = query_body_attribute(&handle.ui_tx, tab_id, "data-resized");
  assert_eq!(
    resized.as_deref(),
    Some("1"),
    "expected window.onresize handler property to fire on ViewportChanged"
  );

  handle.join().expect("join ui worker");
}
