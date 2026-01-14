#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, RenderedFrame, RepaintReason, TabId,
  UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

fn next_navigation_committed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => url,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}")
    }
    other => panic!("unexpected WorkerToUi while waiting for NavigationCommitted: {other:?}"),
  }
}

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi while waiting for FrameReady: {other:?}"),
  }
}

#[test]
fn link_resolution_follows_js_modified_base_href_after_dom_sync() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  // Fixture site:
  // - dir/index.html: link to `next.html`
  // - JS updates <base href> to `subdir/`
  // - both `dir/next.html` and `dir/subdir/next.html` exist so we can distinguish resolution.
  let next_url_old_base = site.write("dir/next.html", "<!doctype html><title>old</title>");
  let next_url_new_base = site.write("dir/subdir/next.html", "<!doctype html><title>new</title>");
  let index_url = site.write(
    "dir/index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <base href="./">
    <style>
      html, body { margin: 0; padding: 0; }
      /* Keep the link a stable hit target at the top-left for deterministic pointer input. */
      #lnk {
        position: absolute;
        top: 0;
        left: 0;
        width: 100px;
        height: 40px;
        display: block;
        background: rgb(255, 0, 0);
      }
    </style>
    <script>
      // Mutate the document base URL so relative links resolve against /subdir/.
      document.querySelector('base').setAttribute('href', 'subdir/');
    </script>
  </head>
  <body>
    <a id="lnk" href="next.html"></a>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-js-base-url-sync").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id: tab,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id: tab,
      viewport_css: (240, 160),
      dpr: 1.0,
    })
    .expect("viewport");

  ui_tx
    .send(UiToWorker::Navigate {
      tab_id: tab,
      url: index_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");
  assert_eq!(next_navigation_committed(&ui_rx, tab), index_url);
  let _first_frame = next_frame_ready(&ui_rx, tab);

  // Trigger a paint so the worker can sync dom2→dom1 state (including `<base href>`) into the
  // render document before we click.
  ui_tx
    .send(UiToWorker::RequestRepaint {
      tab_id: tab,
      reason: RepaintReason::Explicit,
    })
    .expect("repaint");
  let _synced_frame = next_frame_ready(&ui_rx, tab);

  // Clicking the link should resolve against the JS-updated base URL (dir/subdir/next.html), not
  // the original base (dir/next.html).
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id: tab,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id: tab,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  let committed = next_navigation_committed(&ui_rx, tab);
  assert_eq!(committed, next_url_new_base);
  assert_ne!(committed, next_url_old_base);

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
