#![cfg(feature = "browser_ui")]

use fastrender::render_control::StageHeartbeat;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  fn set(ms: Option<u64>) -> Self {
    fastrender::render_control::set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    fastrender::render_control::set_test_render_delay_ms(None);
  }
}

#[test]
fn cancellation_on_new_navigation() {
  let _stage_lock = super::stage_listener_test_lock();
  // Slow down deadline checks so the first navigation stays in-flight long enough for the UI to
  // bump cancellation.
  let delay_guard = TestRenderDelayGuard::set(Some(2));

  let cancel = CancelGens::new();
  let tab_id = TabId::new();

  let worker = spawn_ui_worker("fastr-browser-worker-cancel-nav-test").expect("spawn worker");
  let fastrender::ui::UiWorkerHandle { ui_tx, ui_rx, join } = worker;

  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .unwrap();

  cancel.bump_paint();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .unwrap();

  let url1 = "about:test-heavy".to_string();
  let url2 = "about:blank".to_string();

  cancel.bump_nav();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url1.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  // Ensure the worker picked up the first navigation before we bump cancellation.
  loop {
    match ui_rx.recv_timeout(Duration::from_secs(10)) {
      Ok(WorkerToUi::NavigationStarted { tab_id: msg_id, url }) if msg_id == tab_id => {
        if url == url1 {
          break;
        }
      }
      Ok(_) => continue,
      Err(err) => panic!("timed out waiting for NavigationStarted: {err}"),
    }
  }

  // Bump nav generation while the first navigation is still in-flight; enqueue a second
  // navigation.
  cancel.bump_nav();
  drop(delay_guard);
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url2.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let mut saw_commit_url2 = false;
  let mut saw_frame_after_commit = false;

  while let Ok(msg) = ui_rx.recv_timeout(Duration::from_secs(30)) {
    match msg {
      WorkerToUi::NavigationCommitted { tab_id: msg_id, url, .. } if msg_id == tab_id => {
        assert_ne!(url, url1, "first navigation should not commit after cancellation");
        if url == url2 {
          saw_commit_url2 = true;
        }
      }
      WorkerToUi::FrameReady { tab_id: msg_id, .. } if msg_id == tab_id => {
        if saw_commit_url2 {
          saw_frame_after_commit = true;
          break;
        }
      }
      WorkerToUi::NavigationFailed { tab_id: msg_id, url, .. } if msg_id == tab_id => {
        // Cancellation may surface as a timeout/cancel failure; just ensure it doesn't commit.
        assert_ne!(url, url2, "second navigation should not fail");
      }
      _ => {}
    }
  }

  assert!(
    saw_commit_url2 && saw_frame_after_commit,
    "expected second navigation to commit and produce a frame"
  );

  drop(ui_tx);
  join.join().expect("join worker");
}

#[test]
fn cancellation_on_scroll_drops_stale_frames() {
  let _stage_lock = super::stage_listener_test_lock();

  let cancel = CancelGens::new();
  let tab_id = TabId::new();

  let worker = spawn_ui_worker("fastr-browser-worker-cancel-scroll-test").expect("spawn worker");
  let fastrender::ui::UiWorkerHandle { ui_tx, ui_rx, join } = worker;

  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .unwrap();

  cancel.bump_paint();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (160, 120),
      dpr: 1.0,
    })
    .unwrap();

  let url = "about:test-heavy".to_string();

  cancel.bump_nav();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  // Wait for the initial navigation to commit and produce a frame.
  let mut committed = false;
  let mut saw_initial_frame = false;
  while let Ok(msg) = ui_rx.recv_timeout(Duration::from_secs(30)) {
    match msg {
      WorkerToUi::NavigationCommitted { tab_id: msg_id, url: committed_url, .. }
        if msg_id == tab_id =>
      {
        assert_eq!(committed_url, url);
        committed = true;
      }
      WorkerToUi::FrameReady { tab_id: msg_id, .. } if msg_id == tab_id => {
        if committed {
          saw_initial_frame = true;
          break;
        }
      }
      _ => {}
    }
  }
  assert!(saw_initial_frame, "expected initial frame after navigation commit");

  // Trigger a scroll repaint, then cancel it mid-flight by bumping paint generation and sending a
  // second scroll. The worker should drop any stale paint output for the first scroll.
  // Drain any straggler stage heartbeats from the navigation so the paint heartbeat below is tied
  // to the first scroll.
  {
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while std::time::Instant::now() < deadline {
      match ui_rx.recv_timeout(Duration::from_millis(10)) {
        Ok(_) => continue,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
      }
    }
  }

  // Slow down deadline checks so the first scroll repaint remains in-flight long enough for the
  // UI to bump paint cancellation.
  let delay_guard = TestRenderDelayGuard::set(Some(2));
  cancel.bump_paint();
  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 200.0),
      pointer_css: None,
    })
    .unwrap();

  // Wait until we observe the scroll repaint enter the paint stages so we can cancel it mid-flight.
  let mut pre_cancel: Vec<WorkerToUi> = Vec::new();
  loop {
    match ui_rx.recv_timeout(Duration::from_secs(10)) {
      Ok(msg) => {
        let saw_paint_stage = matches!(
          &msg,
          WorkerToUi::Stage { tab_id: msg_id, stage }
            if *msg_id == tab_id
              && matches!(*stage, StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize)
        );
        pre_cancel.push(msg);
        if saw_paint_stage {
          break;
        }
      }
      Err(err) => panic!("timed out waiting for paint stage heartbeat: {err}"),
    }
  }

  cancel.bump_paint();
  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 200.0),
      pointer_css: None,
    })
    .unwrap();
  // The second scroll can render at full speed; we only needed the artificial delay to keep the
  // first paint busy long enough to trigger cancellation.
  drop(delay_guard);

  let mut saw_scroll1_frame = false;
  let mut saw_scroll2_frame = false;

  for msg in pre_cancel {
    if let WorkerToUi::FrameReady { tab_id: msg_id, frame } = msg {
      if msg_id != tab_id {
        continue;
      }
      let y = frame.scroll_state.viewport.y;
      if (y - 200.0).abs() < 5.0 {
        saw_scroll1_frame = true;
      }
      if (y - 400.0).abs() < 5.0 {
        saw_scroll2_frame = true;
      }
    }
  }

  while let Ok(msg) = ui_rx.recv_timeout(Duration::from_secs(30)) {
    if let WorkerToUi::FrameReady { tab_id: msg_id, frame } = msg {
      if msg_id != tab_id {
        continue;
      }
      let y = frame.scroll_state.viewport.y;
      if (y - 200.0).abs() < 5.0 {
        saw_scroll1_frame = true;
      }
      if (y - 400.0).abs() < 5.0 {
        saw_scroll2_frame = true;
        break;
      }
    }
  }

  assert!(
    saw_scroll2_frame,
    "expected a committed frame for the second scroll repaint"
  );
  assert!(
    !saw_scroll1_frame,
    "stale frame from first scroll repaint should be dropped"
  );

  drop(ui_tx);
  join.join().expect("join worker");
}
