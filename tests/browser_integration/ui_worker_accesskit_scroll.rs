#![cfg(feature = "browser_ui")]

use fastrender::dom::{enumerate_dom_ids, parse_html_with_options, DomNode, DomParseOptions};
use fastrender::ui::encode_page_node_id;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};

fn wait_for_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .unwrap_or(Duration::from_secs(0));
    assert!(
      remaining > Duration::ZERO,
      "timed out waiting for FrameReady"
    );
    let msg = rx.recv_timeout(remaining).expect("worker msg");
    if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
      if got == tab_id {
        return frame;
      }
    }
  }
}

fn wait_for_page_generation(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) -> u32 {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .unwrap_or(Duration::from_secs(0));
    assert!(
      remaining > Duration::ZERO,
      "timed out waiting for PageAccessibility"
    );
    let msg = rx.recv_timeout(remaining).expect("worker msg");
    if let WorkerToUi::PageAccessibility {
      tab_id: got,
      document_generation,
      ..
    } = msg
    {
      if got == tab_id {
        return document_generation;
      }
    }
  }
}

fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
      return *ids
        .get(&(node as *const DomNode))
        .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("no element with id attribute {id_attr:?}");
}

#[test]
fn a11y_scroll_into_view_scrolls_viewport_to_reveal_target_node() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { height: 2000px; background: rgb(0,0,0); position: relative; }
          #target {
            position: absolute;
            left: 10px;
            top: 1500px;
            width: 120px;
            height: 30px;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <div id="target">hello</div>
      </body>
    </html>
  "#;

  // Resolve the DOM pre-order id using the same DOM parser helpers used across tests.
  let dom = parse_html_with_options(html, DomParseOptions::default()).expect("parse html");
  let target_node_id = node_id_by_id_attr(&dom, "target");

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-accesskit-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected initial scroll position to be at top"
  );

  let document_generation = wait_for_page_generation(&ui_rx, tab_id, DEFAULT_TIMEOUT);

  let request = accesskit::ActionRequest {
    action: accesskit::Action::ScrollIntoView,
    target: encode_page_node_id(tab_id, document_generation, target_node_id),
    data: None,
  };

  ui_tx
    .send(UiToWorker::AccessKitActionRequest { tab_id, request })
    .expect("AccessKitAction");

  // The target element is at 1500px; a scroll-into-view request should scroll the viewport down so
  // it becomes visible.
  let scroll = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for scroll-into-view update"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if got == tab_id => {
          if scroll.viewport.y > 0.0 {
            break scroll;
          }
        }
        _ => {}
      }
    }
  };

  let scroll_y = scroll.viewport.y;
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected scroll y > 0, got {scroll_y}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
