#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::render_control::StageHeartbeat;
use fastrender::scroll::ScrollState;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::{spawn_ui_worker, spawn_ui_worker_for_test};
use std::time::{Duration, Instant};

// These tests spawn a full UI worker (prepare + paint); keep the timeout generous to avoid flakes
// on contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn wait_for_navigation_committed(
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
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_frame(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_scroll_update(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  mut pred: impl FnMut(&ScrollState) -> bool,
) -> ScrollState {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => pred(scroll),
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for ScrollStateUpdated for tab {tab_id:?}"));
  match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => scroll,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_clipboard_text(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::SetClipboardText { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for SetClipboardText for tab {tab_id:?}"));
  match msg {
    WorkerToUi::SetClipboardText { text, .. } => text,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn preorder_id_for_html_id(dom: &fastrender::dom::DomNode, target_id: &str) -> usize {
  let mut stack: Vec<&fastrender::dom::DomNode> = vec![dom];
  let mut next_id = 1usize;
  while let Some(node) = stack.pop() {
    if node
      .get_attribute_ref("id")
      .is_some_and(|id| id == target_id)
    {
      return next_id;
    }
    // Pre-order traversal: push children in reverse DOM order.
    for child in node.children.iter().rev() {
      stack.push(child);
    }
    next_id = next_id.saturating_add(1);
  }
  panic!("missing element with id={target_id:?}");
}

#[test]
fn ui_worker_a11y_set_text_value_clamps_maxlength() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #field { position: absolute; left: 0; top: 0; width: 240px; height: 40px; }
      #go { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="result.html" method="get">
      <input id="field" name="q" maxlength="5" value="">
      <button id="go" type="submit">Go</button>
    </form>
  </body>
</html>
"#;
  let page_url = site.write("page.html", page_html);
  let _result_url = site.write("result.html", "<!doctype html><html><body>ok</body></html>");

  let mut parsed_dom = fastrender::dom::parse_html(page_html).expect("parse html");
  let input_node_id = preorder_id_for_html_id(&parsed_dom, "field");
  let button_node_id = preorder_id_for_html_id(&parsed_dom, "go");
  drop(parsed_dom);

  let handle = spawn_ui_worker("fastr-ui-worker-a11y-maxlength").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: page_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), page_url);
  // Wait for a painted frame so the tab is ready for accessibility actions.
  let _ = wait_for_frame(&ui_rx, tab_id);

  ui_tx
    .send(UiToWorker::A11ySetTextValue {
      tab_id,
      node_id: input_node_id,
      value: "hello world".to_string(),
    })
    .expect("a11y set text value");

  ui_tx
    .send(UiToWorker::A11yActivate {
      tab_id,
      node_id: button_node_id,
    })
    .expect("a11y activate");
  let committed = wait_for_navigation_committed(&ui_rx, tab_id);
  let parsed = url::Url::parse(&committed).expect("committed URL should parse");
  let q = parsed
    .query_pairs()
    .find_map(|(k, v)| (k == "q").then_some(v.to_string()))
    .unwrap_or_default();
  assert_eq!(q, "hello", "committed URL was {committed:?}");

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn ui_worker_applies_a11y_actions_to_page_content() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #field { position: absolute; left: 0; top: 0; width: 240px; height: 40px; }
      /* Force a scrollable document so ScrollIntoView / focus scroll can be asserted. */
      #spacer { position: absolute; left: 0; top: 0; width: 1px; height: 2200px; }
      #go { position: absolute; left: 0; top: 2000px; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="result.html" method="get">
      <input id="field" name="q" value="">
      <div id="spacer"></div>
      <button id="go" type="submit">Go</button>
    </form>
  </body>
</html>
"#;
  let page_url = site.write("page.html", page_html);
  let _result_url = site.write("result.html", "<!doctype html><html><body>ok</body></html>");

  let mut parsed_dom = fastrender::dom::parse_html(page_html).expect("parse html");
  let input_node_id = preorder_id_for_html_id(&parsed_dom, "field");
  let button_node_id = preorder_id_for_html_id(&parsed_dom, "go");
  // Drop the parsed DOM early (keeps the borrow checker happy if the parser internals change).
  drop(parsed_dom);

  let handle = spawn_ui_worker("fastr-ui-worker-a11y-actions").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: page_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), page_url);
  // Wait for a painted frame so layout artifacts exist for ScrollIntoView / focus scroll.
  let initial_frame = wait_for_frame(&ui_rx, tab_id);
  let baseline_scroll = initial_frame.scroll_state.viewport;

  // Drain follow-up messages from the initial navigation before asserting accessibility-driven
  // scrolling.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Scroll the button into view via accessibility action.
  ui_tx
    .send(UiToWorker::A11yScrollIntoView {
      tab_id,
      node_id: button_node_id,
    })
    .expect("a11y scroll into view");
  let scrolled_down = wait_for_scroll_update(&ui_rx, tab_id, |scroll| {
    scroll.viewport.y > baseline_scroll.y + 1.0
  });
  assert!(
    scrolled_down.viewport.y > 0.0,
    "expected ScrollIntoView to scroll down; got {:?}",
    scrolled_down.viewport
  );

  // Focus the input via accessibility action. Since we're currently scrolled down, focus scroll
  // should bring it back into view (near the top).
  ui_tx
    .send(UiToWorker::A11ySetFocus {
      tab_id,
      node_id: input_node_id,
    })
    .expect("a11y focus");
  let scrolled_up = wait_for_scroll_update(&ui_rx, tab_id, |scroll| {
    scroll.viewport.y < scrolled_down.viewport.y - 1.0
  });
  assert!(
    scrolled_up.viewport.y < scrolled_down.viewport.y,
    "expected focusing the input to scroll back toward the top; before={:?} after={:?}",
    scrolled_down.viewport,
    scrolled_up.viewport
  );

  // Set the input value and selection via accessibility actions, then verify via Copy.
  ui_tx
    .send(UiToWorker::A11ySetTextValue {
      tab_id,
      node_id: input_node_id,
      value: "hello world".to_string(),
    })
    .expect("a11y set text value");
  ui_tx
    .send(UiToWorker::A11ySetTextSelectionRange {
      tab_id,
      node_id: input_node_id,
      start: 0,
      end: 5,
    })
    .expect("a11y set selection");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(wait_for_clipboard_text(&ui_rx, tab_id), "hello");

  // Activate the submit button via accessibility action; the resulting navigation should include
  // the updated form value.
  ui_tx
    .send(UiToWorker::A11yActivate {
      tab_id,
      node_id: button_node_id,
    })
    .expect("a11y activate");
  let committed = wait_for_navigation_committed(&ui_rx, tab_id);

  let parsed = url::Url::parse(&committed).expect("committed URL should parse");
  let q = parsed
    .query_pairs()
    .find_map(|(k, v)| (k == "q").then_some(v.to_string()))
    .unwrap_or_default();
  assert_eq!(q, "hello world", "committed URL was {committed:?}");

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn ui_worker_does_not_emit_redundant_scroll_updates_for_a11y_scroll_after_paint_cancel() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      /* Force a scrollable document so ScrollIntoView can be asserted. */
      #spacer { position: absolute; left: 0; top: 0; width: 1px; height: 2200px; }
      #go { position: absolute; left: 0; top: 2000px; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <div id="spacer"></div>
    <button id="go" type="button">Go</button>
  </body>
</html>
"#;
  let page_url = site.write("page.html", page_html);

  let mut parsed_dom = fastrender::dom::parse_html(page_html).expect("parse html");
  let button_node_id = preorder_id_for_html_id(&parsed_dom, "go");
  drop(parsed_dom);

  // Make paints slow so we can deterministically cancel the repaint triggered by A11yScrollIntoView.
  let cancel_gens = CancelGens::new();
  let (ui_tx, ui_rx, join) =
    spawn_ui_worker_for_test("fastr-ui-worker-a11y-scroll-dedup", Some(50))
      .expect("spawn ui worker")
      .split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_with_cancel(
      tab_id,
      None,
      cancel_gens.clone(),
    ))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: page_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), page_url);
  wait_for_frame(&ui_rx, tab_id);

  // Ensure the channel is quiet before issuing the a11y scroll request.
  for _ in ui_rx.try_iter() {}

  ui_tx
    .send(UiToWorker::A11yScrollIntoView {
      tab_id,
      node_id: button_node_id,
    })
    .expect("a11y scroll into view");

  let mut first_scroll: Option<ScrollState> = None;
  let mut matching_scroll_updates = 0usize;
  let mut canceled_paint = false;
  let mut cancel_instant: Option<Instant> = None;

  let start = Instant::now();
  while start.elapsed() < TIMEOUT {
    match ui_rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => match msg {
        WorkerToUi::ScrollStateUpdated {
          tab_id: got,
          scroll,
        } if got == tab_id => {
          // Ignore any stray scroll updates from earlier stages (should be none after the drain
          // above) until we see the A11yScrollIntoView scroll down.
          if scroll.viewport.y <= 0.0 {
            continue;
          }
          if first_scroll.is_none() {
            first_scroll = Some(scroll.clone());
          }
          if first_scroll
            .as_ref()
            .is_some_and(|s| s.viewport == scroll.viewport)
          {
            matching_scroll_updates += 1;
          }
        }
        WorkerToUi::Stage { tab_id: got, stage } if got == tab_id => {
          if !canceled_paint
            && matches!(
              stage,
              StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
            )
          {
            cancel_gens.bump_paint();
            canceled_paint = true;
            cancel_instant = Some(Instant::now());
          }
        }
        // Once we've canceled the in-flight paint, allow a short follow-up window to observe any
        // redundant ScrollStateUpdated emissions caused by the stale output.
        WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id => {
          if canceled_paint {
            break;
          }
        }
        WorkerToUi::NavigationFailed { url, error, .. } => {
          panic!("navigation failed for {url}: {error}")
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        if canceled_paint && cancel_instant.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    canceled_paint,
    "expected to observe paint stage heartbeats to cancel in-flight paint"
  );
  let first_scroll = first_scroll.expect("expected A11yScrollIntoView to emit ScrollStateUpdated");
  assert!(
    first_scroll.viewport.y > 0.0,
    "expected A11yScrollIntoView to scroll down; got {:?}",
    first_scroll.viewport
  );
  assert_eq!(
    matching_scroll_updates, 1,
    "expected exactly one ScrollStateUpdated for unchanged scroll state after cancel; got {matching_scroll_updates}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
