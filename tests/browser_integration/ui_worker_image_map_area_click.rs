#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn fixture() -> (support::TempSite, String, String) {
  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r##"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      img { width: 100px; height: 100px; display: block; }
    </style>
  </head>
  <body>
    <img usemap="#m" src="img.svg" alt="map">
    <map name="m">
      <area id="a" href="target.html" shape="rect" coords="0,0,100,100">
    </map>
  </body>
</html>
"##,
  );
  let _img_url = site.write(
    "img.svg",
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
  <rect width="100" height="100" fill="rgb(255,0,0)"/>
</svg>
"#,
  );
  let _target_url = site.write(
    "target.html",
    r#"<!doctype html><html><head><meta charset="utf-8"></head><body>Target</body></html>"#,
  );

  let expected_target_url = url::Url::parse(&index_url)
    .expect("parse index URL")
    .join("target.html")
    .expect("resolve target URL")
    .to_string();

  (site, index_url, expected_target_url)
}

#[test]
fn image_map_area_behaves_like_link_for_context_menu_and_click() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, index_url, expected_target_url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-image-map-area-click").expect("spawn ui worker");
  let tab_id = TabId::new();

  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 200), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      index_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Wait for an initial frame so hit-testing has prepared layout artifacts (including image maps).
  let _frame_msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  // Drain any follow-up messages from the initial navigation to keep assertions scoped to the
  // context-menu/click interactions below.
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(100));

  let pos_css = (10.0, 10.0);

  // Context menu hit-testing should resolve the <area href> to an absolute URL.
  worker
    .ui_tx
    .send(UiToWorker::ContextMenuRequest { tab_id, pos_css })
    .unwrap();

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ContextMenu {
      tab_id: got_tab,
      pos_css: got_pos,
      link_url,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(got_pos, pos_css);
      assert_eq!(link_url.as_deref(), Some(expected_target_url.as_str()));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(100));

  // Primary click within the mapped rectangle should trigger same-tab navigation.
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut saw_started = false;
  let mut saw_committed = false;
  let mut saw_frame = false;

  while Instant::now() < deadline {
    match worker.ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationStarted {
          tab_id: msg_tab,
          url,
        } if msg_tab == tab_id => {
          if url == expected_target_url {
            saw_started = true;
          }
        }
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          ..
        } if msg_tab == tab_id => {
          if url == expected_target_url {
            saw_committed = true;
          }
        }
        WorkerToUi::FrameReady {
          tab_id: msg_tab, ..
        } if msg_tab == tab_id => {
          if saw_committed {
            saw_frame = true;
            break;
          }
        }
        WorkerToUi::RequestOpenInNewTab {
          tab_id: msg_tab,
          url,
        } if msg_tab == tab_id => {
          panic!("expected same-tab navigation, got RequestOpenInNewTab({url})");
        }
        WorkerToUi::NavigationFailed {
          tab_id: msg_tab,
          url,
          error,
          ..
        } if msg_tab == tab_id => {
          panic!("navigation failed for {url}: {error}");
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    saw_started,
    "expected NavigationStarted for mapped <area> URL {expected_target_url}"
  );
  assert!(
    saw_committed,
    "expected NavigationCommitted for mapped <area> URL {expected_target_url}"
  );
  assert!(
    saw_frame,
    "expected FrameReady after navigating to mapped <area> URL {expected_target_url}"
  );

  worker.join().unwrap();
}
