#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, TabId};
use std::path::Path;
use tempfile::tempdir;
use url::Url;

fn file_url(path: &Path) -> String {
  Url::from_file_path(path)
    .ok()
    .expect("convert path to file:// URL")
    .to_string()
}

#[test]
fn ui_worker_emits_page_accessibility_snapshot() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("a11y.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <html>
        <head><meta charset="utf-8"></head>
        <body>
          <button id="btn">Click</button>
        </body>
      </html>
    "#,
  )
  .expect("write fixture html");
  let url = file_url(&path);

  let h = WorkerHarness::spawn();

  let tab_id = TabId::new();
  h.send(create_tab_msg(tab_id, None));
  h.send(viewport_changed_msg(tab_id, (256, 256), 1.0));
  h.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl));

  let mut saw_frame = false;
  let mut a11y_node_count: Option<usize> = None;
  let events = h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    match ev {
      WorkerToUiEvent::FrameReady { tab_id: t, .. } if *t == tab_id => {
        saw_frame = true;
      }
      WorkerToUiEvent::PageAccessibility {
        tab_id: t,
        node_count,
      } if *t == tab_id => {
        a11y_node_count = Some(*node_count);
      }
      _ => {}
    }
    saw_frame && a11y_node_count.is_some()
  });

  let node_count = a11y_node_count.unwrap_or_else(|| {
    panic!("did not observe PageAccessibility event; got {events:?}");
  });
  assert!(node_count > 0);
}

