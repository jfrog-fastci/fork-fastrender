#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, recv_for_tab, scroll_msg, viewport_changed_msg, TempSite,
};
use fastrender::ui::worker::spawn_ui_worker;
use fastrender::ui::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const VIEWPORT_CSS: (u32, u32) = (240, 120);
const DPR: f32 = 1.0;
// Navigations can be slow under parallel test load (render threads + DOM/style/layout), so give
// scroll restoration a little more runway.
const TIMEOUT: Duration = Duration::from_secs(20);

fn describe_message(msg: &WorkerToUi) -> String {
  match msg {
    WorkerToUi::Stage { stage, .. } => format!("Stage({stage:?})"),
    WorkerToUi::NavigationStarted { url, .. } => format!("NavigationStarted({url})"),
    WorkerToUi::NavigationCommitted { url, .. } => format!("NavigationCommitted({url})"),
    WorkerToUi::NavigationFailed { url, error, .. } => format!("NavigationFailed({url}, {error})"),
    WorkerToUi::ScrollStateUpdated { scroll, .. } => {
      format!("ScrollStateUpdated(viewport={:?})", scroll.viewport)
    }
    WorkerToUi::FrameReady { frame, .. } => {
      format!("FrameReady(viewport={:?})", frame.scroll_state.viewport)
    }
    WorkerToUi::OpenSelectDropdown { select_node_id, .. } => {
      format!("OpenSelectDropdown(select_node_id={select_node_id})")
    }
    WorkerToUi::SelectDropdownOpened { select_node_id, .. } => {
      format!("SelectDropdownOpened(select_node_id={select_node_id})")
    }
    WorkerToUi::SelectDropdownClosed { .. } => "SelectDropdownClosed".to_string(),
    WorkerToUi::LoadingState { loading, .. } => format!("LoadingState({loading})"),
    WorkerToUi::DebugLog { line, .. } => format!("DebugLog({})", line.trim_end()),
    other => format!("{other:?}"),
  }
}

fn wait_for_navigation_committed(ui_rx: &Receiver<WorkerToUi>, tab_id: TabId) -> String {
  let deadline = Instant::now() + TIMEOUT;
  let mut trace = Vec::<String>::new();

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(msg) = recv_for_tab(ui_rx, tab_id, remaining, |_| true) else {
      break;
    };
    trace.push(describe_message(&msg));
    match msg {
      WorkerToUi::NavigationCommitted { url, .. } => return url,
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed: {url}: {error}\nmessages:\n{}", trace.join("\n"));
      }
      _ => {}
    }
  }

  panic!(
    "timed out waiting for NavigationCommitted\nmessages:\n{}",
    trace.join("\n")
  );
}

fn wait_for_frame_ready(ui_rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let deadline = Instant::now() + TIMEOUT;
  let mut trace = Vec::<String>::new();

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(msg) = recv_for_tab(ui_rx, tab_id, remaining, |_| true) else {
      break;
    };
    trace.push(describe_message(&msg));
    match msg {
      WorkerToUi::FrameReady { frame, .. } => return frame,
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed: {url}: {error}\nmessages:\n{}", trace.join("\n"));
      }
      _ => {}
    }
  }

  panic!(
    "timed out waiting for FrameReady\nmessages:\n{}",
    trace.join("\n")
  );
}

fn wait_for_scroll_y_at_least(ui_rx: &Receiver<WorkerToUi>, tab_id: TabId, min_y: f32) -> f32 {
  let deadline = Instant::now() + TIMEOUT;
  let mut trace = Vec::<String>::new();

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(msg) = recv_for_tab(ui_rx, tab_id, remaining, |_| true) else {
      break;
    };
    trace.push(describe_message(&msg));

    match msg {
      WorkerToUi::ScrollStateUpdated { scroll, .. } => {
        let y = scroll.viewport.y;
        if y >= min_y {
          return y;
        }
      }
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed: {url}: {error}\nmessages:\n{}", trace.join("\n"));
      }
      _ => {}
    }
  }

  panic!(
    "timed out waiting for scroll_y >= {min_y}\nmessages:\n{}",
    trace.join("\n")
  );
}

#[test]
fn back_navigation_restores_viewport_scroll_from_history() {
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();
  let url1 = site.write(
    "page1.html",
    r#"<!doctype html>
      <meta charset="utf-8" />
      <style>
        html, body { margin: 0; padding: 0; }
        .spacer { height: 2000px; background: #ddd; }
      </style>
      <body>
        <div>page1</div>
        <div class="spacer"></div>
      </body>
    "#,
  );
  let url2 = site.write(
    "page2.html",
    r#"<!doctype html>
      <meta charset="utf-8" />
      <style>
        html, body { margin: 0; padding: 0; }
        .spacer { height: 2000px; background: #cdf; }
      </style>
      <body>
        <div>page2</div>
        <div class="spacer"></div>
      </body>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-history-scroll-restore").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, VIEWPORT_CSS, DPR))
    .expect("ViewportChanged");

  ui_tx
    .send(navigate_msg(tab_id, url1.clone(), NavigationReason::TypedUrl))
    .expect("Navigate url1");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), url1);
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 320.0), None))
    .expect("Scroll url1");
  let scroll_y_1 = wait_for_scroll_y_at_least(&ui_rx, tab_id, 200.0);

  ui_tx
    .send(navigate_msg(tab_id, url2.clone(), NavigationReason::TypedUrl))
    .expect("Navigate url2");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), url2);
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 640.0), None))
    .expect("Scroll url2");
  let scroll_y_2 = wait_for_scroll_y_at_least(&ui_rx, tab_id, 400.0);

  ui_tx.send(UiToWorker::GoBack { tab_id }).expect("GoBack");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), url1);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert!(
    (frame.scroll_state.viewport.y - scroll_y_1).abs() < 2.0,
    "expected back navigation to restore scroll_y ~= {scroll_y_1} (got {:?})",
    frame.scroll_state.viewport
  );

  ui_tx
    .send(UiToWorker::GoForward { tab_id })
    .expect("GoForward");
  assert_eq!(wait_for_navigation_committed(&ui_rx, tab_id), url2);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert!(
    (frame.scroll_state.viewport.y - scroll_y_2).abs() < 2.0,
    "expected forward navigation to restore scroll_y ~= {scroll_y_2} (got {:?})",
    frame.scroll_state.viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
