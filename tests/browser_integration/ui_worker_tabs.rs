#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use super::support::create_tab_msg;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

fn wait_for_first_frame(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady { tab_id: msg_tab, frame }) if msg_tab == tab_id => return frame,
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for FrameReady for {tab_id:?}"),
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected while waiting for frame"),
    }
  }
}

fn pixmap_is_uniform_rgba(pixmap: &tiny_skia::Pixmap) -> bool {
  let data = pixmap.data();
  let Some(first) = data.get(0..4) else {
    return true;
  };
  data.chunks_exact(4).all(|px| px == first)
}

#[test]
fn multi_tab_navigations_are_scoped_by_tab_id() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-tabs-test").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab1 = TabId::new();
  let tab2 = TabId::new();

  ui_tx
    .send(create_tab_msg(tab1, Some("about:blank".to_string())))
    .expect("create tab1");
  ui_tx
    .send(create_tab_msg(tab2, Some("about:newtab".to_string())))
    .expect("create tab2");

  let deadline = Instant::now() + TEST_TIMEOUT;
  let mut started = [false, false];
  let mut committed = [false, false];
  let mut saw_frame = [false, false];
  let mut tab1_frame_uniform = None;
  let mut tab2_frame_uniform = None;

  while Instant::now() < deadline && !(saw_frame[0] && saw_frame[1] && committed[0] && committed[1])
  {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let msg = match ui_rx.recv_timeout(remaining) {
      Ok(msg) => msg,
      Err(RecvTimeoutError::Timeout) => break,
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
    };

    match msg {
      WorkerToUi::NavigationStarted { tab_id, url } => {
        if tab_id == tab1 {
          assert_eq!(url, "about:blank");
          started[0] = true;
        } else if tab_id == tab2 {
          assert_eq!(url, "about:newtab");
          started[1] = true;
        } else {
          panic!("unexpected NavigationStarted for unknown tab {tab_id:?}");
        }
      }
      WorkerToUi::NavigationCommitted { tab_id, url, .. } => {
        if tab_id == tab1 {
          assert!(
            started[0],
            "NavigationCommitted before NavigationStarted for tab1"
          );
          assert_eq!(url, "about:blank");
          committed[0] = true;
        } else if tab_id == tab2 {
          assert!(
            started[1],
            "NavigationCommitted before NavigationStarted for tab2"
          );
          assert_eq!(url, "about:newtab");
          committed[1] = true;
        } else {
          panic!("unexpected NavigationCommitted for unknown tab {tab_id:?}");
        }
      }
      WorkerToUi::Stage { tab_id, .. } => {
        assert!(
          tab_id == tab1 || tab_id == tab2,
          "unexpected Stage heartbeat for unknown tab {tab_id:?}"
        );
      }
      WorkerToUi::FrameReady { tab_id, frame } => {
        if tab_id == tab1 {
          saw_frame[0] = true;
          tab1_frame_uniform.get_or_insert_with(|| pixmap_is_uniform_rgba(&frame.pixmap));
        } else if tab_id == tab2 {
          saw_frame[1] = true;
          tab2_frame_uniform.get_or_insert_with(|| pixmap_is_uniform_rgba(&frame.pixmap));
        } else {
          panic!("unexpected FrameReady for unknown tab {tab_id:?}");
        }
      }
      _ => {}
    }
  }

  assert!(started[0], "expected NavigationStarted for tab1");
  assert!(committed[0], "expected NavigationCommitted for tab1");
  assert!(saw_frame[0], "expected FrameReady for tab1");

  assert!(started[1], "expected NavigationStarted for tab2");
  assert!(committed[1], "expected NavigationCommitted for tab2");
  assert!(saw_frame[1], "expected FrameReady for tab2");

  assert_eq!(
    tab1_frame_uniform,
    Some(true),
    "expected about:blank to render as a uniform pixmap"
  );
  assert_eq!(
    tab2_frame_uniform,
    Some(false),
    "expected about:newtab to render as a non-uniform pixmap (text/content visible)"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn close_tab_prevents_future_frames_for_that_tab() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-close-tab-test").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab1 = TabId::new();
  ui_tx
    .send(create_tab_msg(tab1, Some("about:newtab".to_string())))
    .expect("create tab1");
  let _ = wait_for_first_frame(&ui_rx, tab1, TEST_TIMEOUT);

  // Drain any non-frame messages that were queued by the initial navigation.
  while ui_rx.try_recv().is_ok() {}

  ui_tx.send(UiToWorker::CloseTab { tab_id: tab1 }).expect("close tab1");
  ui_tx
    .send(UiToWorker::RequestRepaint {
      tab_id: tab1,
      reason: RepaintReason::Explicit,
    })
    .expect("request repaint");

  let deadline = Instant::now() + Duration::from_millis(500);
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ui_rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady { tab_id, .. }) if tab_id == tab1 => {
        panic!("unexpected FrameReady for closed tab1")
      }
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => break,
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
    }
  }

  // Worker must stay alive and serve other tabs.
  let tab2 = TabId::new();
  ui_tx
    .send(create_tab_msg(tab2, Some("about:blank".to_string())))
    .expect("create tab2");
  let _ = wait_for_first_frame(&ui_rx, tab2, TEST_TIMEOUT);

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
