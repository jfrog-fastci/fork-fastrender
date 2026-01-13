#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};

#[test]
fn worker_emits_page_accesskit_subtree_with_document_and_button() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head><meta charset="utf-8"></head>
        <body>
          <button>Submit</button>
        </body>
      </html>"#,
  );

  let worker = fastrender::ui::spawn_browser_worker_for_test(None).expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .expect("CreateTab");

  // Keep the viewport small so this test stays fast.
  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 100),
      dpr: 1.0,
    })
    .expect("ViewportChanged");

  worker
    .tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate");

  let msg = support::recv_for_tab(&worker.rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::PageAccessKitSubtree { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for PageAccessKitSubtree for tab {tab_id:?}"));

  let subtree = match msg {
    WorkerToUi::PageAccessKitSubtree { subtree, .. } => subtree,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}")
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };

  let mut saw_document = false;
  let mut saw_submit_button = false;
  for (_id, node) in &subtree.nodes {
    if node.role() == accesskit::Role::Document {
      saw_document = true;
    }
    if node.role() == accesskit::Role::Button && node.name().unwrap_or("").trim() == "Submit" {
      saw_submit_button = true;
    }
  }

  assert!(
    saw_document,
    "expected AccessKit subtree to contain a Document node; got nodes={:?}",
    subtree
      .nodes
      .iter()
      .map(|(_id, node)| format!("{:?}({:?})", node.role(), node.name()))
      .collect::<Vec<_>>()
  );
  assert!(
    saw_submit_button,
    "expected AccessKit subtree to contain Button named 'Submit'; got nodes={:?}",
    subtree
      .nodes
      .iter()
      .map(|(_id, node)| format!("{:?}({:?})", node.role(), node.name()))
      .collect::<Vec<_>>()
  );

  drop(worker.tx);
  worker.join.join().expect("join worker thread");
}

