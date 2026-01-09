#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, recv_for_tab, scroll_msg, viewport_changed_msg, TempSite,
};
use fastrender::ui::worker::spawn_ui_worker;
use fastrender::ui::{BrowserTabState, NavigationReason, TabId, UiToWorker, WorkerToUi};
use std::sync::mpsc::{Receiver, Sender};
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
  }
}

fn send_scroll_delta(ui_tx: &Sender<UiToWorker>, tab_id: TabId, delta: (f32, f32)) {
  if delta.0.abs() <= 1e-3 && delta.1.abs() <= 1e-3 {
    return;
  }
  ui_tx
    .send(scroll_msg(tab_id, delta, None))
    .expect("send Scroll");
}

fn handle_worker_message(tab: &mut BrowserTabState, ui_tx: &Sender<UiToWorker>, msg: WorkerToUi) {
  match msg {
    WorkerToUi::FrameReady { frame, .. } => {
      tab.scroll_state = frame.scroll_state.clone();
      if tab.pending_restore_scroll.is_some() {
        tab.note_scroll_restore_frame_ready();
      } else {
        tab
          .history
          .update_scroll(frame.scroll_state.viewport.x, frame.scroll_state.viewport.y);
      }

      if let Some(delta) = tab.take_scroll_restore_delta_if_ready() {
        send_scroll_delta(ui_tx, tab.id, delta);
      }
    }
    WorkerToUi::ScrollStateUpdated { scroll, .. } => {
      tab.scroll_state = scroll.clone();
      if tab.pending_restore_scroll.is_none() {
        tab.history.update_scroll(scroll.viewport.x, scroll.viewport.y);
      }
    }
    WorkerToUi::NavigationStarted { url, .. } => {
      tab.loading = true;
      tab.error = None;
      tab.pending_nav_url = Some(url);
    }
    WorkerToUi::NavigationCommitted { url, title, .. } => {
      if let Some(original) = tab.pending_nav_url.take() {
        tab.history.commit_navigation(&original, Some(&url));
      }
      if let Some(title) = title {
        tab.title = Some(title.clone());
        tab.history.set_title(title);
      }
      tab.loading = false;
      tab.error = None;
      tab.sync_nav_flags_from_history();

      tab.note_scroll_restore_nav_committed();
      if let Some(delta) = tab.take_scroll_restore_delta_if_ready() {
        send_scroll_delta(ui_tx, tab.id, delta);
      }
    }
    WorkerToUi::NavigationFailed { error, .. } => {
      tab.loading = false;
      tab.error = Some(error);
      tab.pending_nav_url = None;
      tab.clear_scroll_restore();
    }
    WorkerToUi::OpenSelectDropdown { .. }
    | WorkerToUi::SelectDropdownOpened { .. }
    | WorkerToUi::SelectDropdownClosed { .. }
    | WorkerToUi::Stage { .. }
    | WorkerToUi::LoadingState { .. }
    | WorkerToUi::DebugLog { .. } => {}
  }
}

fn wait_for_navigation_complete(
  tab: &mut BrowserTabState,
  ui_tx: &Sender<UiToWorker>,
  ui_rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
) {
  let deadline = Instant::now() + TIMEOUT;
  let mut saw_frame = false;
  let mut saw_commit = false;
  let mut trace: Vec<String> = Vec::new();

  while Instant::now() < deadline && !(saw_frame && saw_commit) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(msg) = recv_for_tab(ui_rx, tab_id, remaining, |_| true) else {
      break;
    };
    trace.push(describe_message(&msg));
    match msg {
      WorkerToUi::FrameReady { .. } => saw_frame = true,
      WorkerToUi::NavigationCommitted { .. } => saw_commit = true,
      WorkerToUi::NavigationFailed { error, .. } => {
        panic!("navigation failed: {error}\nmessages:\n{}", trace.join("\n"));
      }
      _ => {}
    }
    handle_worker_message(tab, ui_tx, msg);
  }

  assert!(
    saw_frame && saw_commit,
    "timed out waiting for navigation to complete\nmessages:\n{}",
    trace.join("\n")
  );
}

fn wait_for_scroll_y_at_least(
  tab: &mut BrowserTabState,
  ui_tx: &Sender<UiToWorker>,
  ui_rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  min_y: f32,
) {
  let deadline = Instant::now() + TIMEOUT;
  let mut trace: Vec<String> = Vec::new();

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(msg) = recv_for_tab(ui_rx, tab_id, remaining, |_| true) else {
      break;
    };
    trace.push(describe_message(&msg));
    handle_worker_message(tab, ui_tx, msg);

    if tab.scroll_state.viewport.y >= min_y {
      return;
    }
  }

  panic!(
    "timed out waiting for scroll_y >= {min_y} (got {})\nmessages:\n{}",
    tab.scroll_state.viewport.y,
    trace.join("\n")
  );
}

fn wait_for_restored_scroll_y(
  tab: &mut BrowserTabState,
  ui_tx: &Sender<UiToWorker>,
  ui_rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_y: f32,
) {
  let deadline = Instant::now() + TIMEOUT;
  let mut saw_scroll_update = false;
  let mut saw_frame = false;
  let mut trace: Vec<String> = Vec::new();

  while Instant::now() < deadline && !(saw_scroll_update && saw_frame) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(msg) = recv_for_tab(ui_rx, tab_id, remaining, |_| true) else {
      break;
    };
    trace.push(describe_message(&msg));

    match &msg {
      WorkerToUi::ScrollStateUpdated { scroll, .. } => {
        if (scroll.viewport.y - expected_y).abs() < 2.0 {
          saw_scroll_update = true;
        }
      }
      WorkerToUi::FrameReady { frame, .. } => {
        if (frame.scroll_state.viewport.y - expected_y).abs() < 2.0 {
          saw_frame = true;
        }
      }
      WorkerToUi::NavigationFailed { error, .. } => {
        panic!("navigation failed: {error}\nmessages:\n{}", trace.join("\n"));
      }
      _ => {}
    }

    handle_worker_message(tab, ui_tx, msg);
  }

  assert!(
    saw_scroll_update && saw_frame,
    "timed out waiting for restored scroll_y ~= {expected_y} (got {})\nmessages:\n{}",
    tab.scroll_state.viewport.y,
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

  // Start on url1.
  let mut tab = BrowserTabState::new(tab_id, url1.clone());
  tab.loading = true;
  tab.pending_nav_url = Some(url1.clone());
  ui_tx
    .send(navigate_msg(tab_id, url1.clone(), NavigationReason::TypedUrl))
    .expect("Navigate url1");
  wait_for_navigation_complete(&mut tab, &ui_tx, &ui_rx, tab_id);

  // Scroll url1 and verify history was updated.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 320.0), None))
    .expect("Scroll url1");
  wait_for_scroll_y_at_least(&mut tab, &ui_tx, &ui_rx, tab_id, 200.0);
  let saved_scroll_y = tab.history.current().unwrap().scroll_y;
  assert!(
    saved_scroll_y >= 200.0,
    "expected TabHistory to store url1 scroll_y >= 200, got {saved_scroll_y}"
  );

  // Navigate to url2 and scroll to a different offset.
  tab.clear_scroll_restore();
  tab.history.push(url2.clone());
  tab.sync_nav_flags_from_history();
  tab.loading = true;
  tab.error = None;
  tab.pending_nav_url = Some(url2.clone());
  ui_tx
    .send(navigate_msg(tab_id, url2.clone(), NavigationReason::TypedUrl))
    .expect("Navigate url2");
  wait_for_navigation_complete(&mut tab, &ui_tx, &ui_rx, tab_id);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 640.0), None))
    .expect("Scroll url2");
  wait_for_scroll_y_at_least(&mut tab, &ui_tx, &ui_rx, tab_id, 400.0);

  // Go back to url1 and assert the worker ends up at the stored history scroll offset.
  let (back_url, back_target_y) = {
    let entry = tab.history.go_back().expect("can go back");
    (entry.url.clone(), entry.scroll_y)
  };
  tab.sync_nav_flags_from_history();
  tab.begin_scroll_restore(0.0, back_target_y);
  tab.loading = true;
  tab.error = None;
  tab.pending_nav_url = Some(back_url.clone());
  ui_tx
    .send(navigate_msg(tab_id, back_url, NavigationReason::BackForward))
    .expect("Navigate back");

  wait_for_restored_scroll_y(&mut tab, &ui_tx, &ui_rx, tab_id, back_target_y);
  assert!(
    tab.pending_restore_scroll.is_none(),
    "pending_restore_scroll should be cleared after restoration"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
