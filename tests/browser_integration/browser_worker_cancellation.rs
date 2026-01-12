#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(15);
// Worker startup + first render can take a few seconds under parallel load (CI), and cancellation
// tests need enough slack to cancel in-flight work before it commits.
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn navigation_cancellation_drops_stale_frame_and_is_silent() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let _delay = support::TestRenderDelayGuard::set(Some(1));

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .expect("create tab");
  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (128, 128),
      dpr: 1.0,
    })
    .expect("viewport");

  let first_url = "about:test-heavy".to_string();
  let second_url = "about:blank".to_string();

  worker
    .tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: first_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate first");

  let deadline = Instant::now() + MAX_WAIT;
  let mut started_first = false;
  let mut sent_second = false;
  let mut last_committed: Option<String> = None;
  let mut committed_first = false;
  let mut saw_second_frame = false;
  let mut saw_first_frame = false;
  let mut saw_failed_first = false;
  let mut saw_debug_log = false;
  let mut captured: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationStarted { tab_id: got, url }
            if *got == tab_id && url == &first_url =>
          {
            started_first = true;
          }
          WorkerToUi::Stage { tab_id: got, .. }
            if *got == tab_id && started_first && !sent_second =>
          {
            // Simulate UI-driven cancellation while the worker is blocked in the first navigation.
            cancel.bump_nav();
            worker
              .tx
              .send(UiToWorker::Navigate {
                tab_id,
                url: second_url.clone(),
                reason: NavigationReason::TypedUrl,
              })
              .expect("navigate second");
            sent_second = true;
          }
          WorkerToUi::NavigationCommitted {
            tab_id: got, url, ..
          } if *got == tab_id => {
            last_committed = Some(url.clone());
            if url == &first_url {
              committed_first = true;
            }
          }
          WorkerToUi::NavigationFailed {
            tab_id: got, url, ..
          } if *got == tab_id => {
            if url == &first_url {
              saw_failed_first = true;
            }
          }
          WorkerToUi::DebugLog { tab_id: got, .. } if *got == tab_id => {
            saw_debug_log = true;
          }
          WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id => {
            if last_committed.as_deref() == Some(second_url.as_str()) {
              saw_second_frame = true;
              captured.push(msg);
              break;
            }
            if last_committed.as_deref() == Some(first_url.as_str()) {
              saw_first_frame = true;
              captured.push(msg);
              break;
            }
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  drop(worker.tx);
  worker.join.join().expect("worker join");

  assert!(
    started_first,
    "expected to observe NavigationStarted for the first URL; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    sent_second,
    "expected to observe a stage heartbeat during the first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    saw_second_frame,
    "expected FrameReady for the second navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !committed_first,
    "expected no NavigationCommitted for the cancelled first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_first_frame,
    "expected no FrameReady for the cancelled first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_failed_first,
    "expected no NavigationFailed for the cancelled first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_debug_log,
    "expected no DebugLog during cancellation; messages:\n{}",
    support::format_messages(&captured)
  );
}

#[test]
fn rapid_scroll_cancels_stale_paint() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  let mut body = String::new();
  // Keep the document big enough to scroll, but not so large that test render delay hooks make
  // the initial frame excessively slow under CI load.
  for i in 0..16 {
    body.push_str(&format!("<div class=\"row\">row {i}</div>\n"));
  }
  let url = site.write(
    "scroll.html",
    &format!(
      r#"<!doctype html>
        <html>
          <head>
            <style>
              html, body {{ margin: 0; padding: 0; }}
              .row {{ height: 40px; border-bottom: 1px solid #ccc; }}
            </style>
          </head>
          <body>
            {body}
          </body>
        </html>"#
    ),
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url),
      cancel: cancel.clone(),
    })
    .expect("create tab");
  worker
    .tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("set active tab");
  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("viewport");

  // Wait for the initial frame so we have layout cache and a document installed.
  let deadline = Instant::now() + MAX_WAIT;
  let mut initial_trace: Vec<WorkerToUi> = Vec::new();
  let mut saw_initial_frame = false;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker
      .rx
      .recv_timeout(remaining.min(Duration::from_millis(200)))
    {
      Ok(msg) => {
        if matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id) {
          saw_initial_frame = true;
          initial_trace.push(msg);
          break;
        }
        initial_trace.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(
    saw_initial_frame,
    "initial frame\nmessages:\n{}",
    support::format_messages(&initial_trace)
  );
  let _ = support::drain_for(&worker.rx, Duration::from_millis(100));

  // Enable an artificial render delay for the scroll paints only. This keeps the initial navigation
  // fast (so we don't flake on the first frame) while ensuring the scroll paint is slow enough that
  // we can deterministically cancel it after receiving a stage heartbeat.
  let delay_guard = support::TestRenderDelayGuard::set(Some(1));
  // Scroll repaints intentionally do not forward `WorkerToUi::Stage` messages (stage forwarding is
  // scoped to navigation jobs). Install a process-global stage listener so the test can observe when
  // the first scroll paint enters the paint pipeline.
  let worker_thread = worker.join.thread().id();
  let (paint_stage_tx, paint_stage_rx) = std::sync::mpsc::channel::<StageHeartbeat>();
  let _global_stage_guard = GlobalStageListenerGuard::new(std::sync::Arc::new(move |stage| {
    if std::thread::current().id() != worker_thread {
      return;
    }
    if matches!(
      stage,
      StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
    ) {
      let _ = paint_stage_tx.send(stage);
    }
  }));
  while paint_stage_rx.try_recv().is_ok() {}

  worker
    .tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 80.0),
      pointer_css: None,
    })
    .expect("scroll 1");

  // Capture messages for assertions/debugging across both scroll repaints.
  let mut frames = Vec::new();
  let mut captured = Vec::new();

  // Wait until we see at least one paint-stage heartbeat during the first scroll paint, then cancel it.
  let _ = paint_stage_rx
    .recv_timeout(MAX_WAIT)
    .expect("paint stage heartbeat during scroll paint");

  cancel.bump_paint();
  worker
    .tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 80.0),
      pointer_css: None,
    })
    .expect("scroll 2");

  let deadline = Instant::now() + MAX_WAIT;
  while Instant::now() < deadline {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::FrameReady { tab_id: got, frame } if *got == tab_id => {
            frames.push(frame.scroll_state.clone());
            captured.push(msg);
            break;
          }
          WorkerToUi::DebugLog { tab_id: got, .. } if *got == tab_id => {
            captured.push(msg);
            break;
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  // Ensure a stale frame does not arrive after the latest scroll frame.
  captured.extend(support::drain_for(&worker.rx, Duration::from_secs(1)));

  // Clear synthetic slowdown before shutting down the worker (which can be slow under CI load).
  drop(delay_guard);

  drop(worker.tx);
  worker.join.join().expect("worker join");

  assert!(
    captured
      .iter()
      .all(|msg| !matches!(msg, WorkerToUi::DebugLog { tab_id: got, .. } if *got == tab_id)),
    "expected no DebugLog during scroll cancellation; messages:\n{}",
    support::format_messages(&captured)
  );

  assert_eq!(
    frames.len(),
    1,
    "expected exactly one FrameReady after scroll cancellation; messages:\n{}",
    support::format_messages(&captured)
  );

  let frame_scroll = &frames[0];
  assert!(
    (frame_scroll.viewport.y - 160.0).abs() < 0.5,
    "expected painted scroll_y ~= 160, got {:?}; messages:\n{}",
    frame_scroll.viewport,
    support::format_messages(&captured)
  );

  let latest = captured
    .iter()
    .rev()
    .find_map(|msg| match msg {
      WorkerToUi::ScrollStateUpdated {
        tab_id: got,
        scroll,
      } if *got == tab_id => Some(scroll.clone()),
      _ => None,
    })
    .unwrap_or_else(|| {
      panic!(
        "expected at least one ScrollStateUpdated; messages:\n{}",
        support::format_messages(&captured)
      )
    });
  assert_eq!(
    latest.viewport,
    frame_scroll.viewport,
    "expected FrameReady scroll_state to match ScrollStateUpdated; messages:\n{}",
    support::format_messages(&captured)
  );

  assert!(
    captured
      .iter()
      .filter(|msg| matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id))
      .count()
      == 1,
    "unexpected additional FrameReady messages after latest scroll frame; messages:\n{}",
    support::format_messages(&captured)
  );
}

#[test]
fn bump_paint_during_navigation_does_not_emit_navigation_failed() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  // Slow down deadline checks so we have time to bump paint cancellation during the navigation
  // prepare stage (before the initial paint begins).
  let delay_guard = support::TestRenderDelayGuard::set(Some(2));

  let site = support::TempSite::new();
  let mut body = String::new();
  for i in 0..128 {
    body.push_str(&format!("<div class=\"row\">row {i}</div>\n"));
  }
  let url = site.write(
    "nav.html",
    &format!(
      r#"<!doctype html>
        <html>
          <head>
            <style>
              html, body {{ margin: 0; padding: 0; font: 14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }}
              .row {{ height: 24px; border-bottom: 1px solid #ccc; }}
            </style>
          </head>
          <body>
            {body}
          </body>
        </html>"#
    ),
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .expect("create tab");

  const VIEWPORT_A: (u32, u32) = (200, 120);
  const VIEWPORT_B: (u32, u32) = (240, 120);

  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: VIEWPORT_A,
      dpr: 1.0,
    })
    .expect("viewport");

  worker
    .tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");

  // Wait until the navigation has started and we observe at least one non-paint stage heartbeat,
  // then bump paint and enqueue a viewport change. This should cancel the navigation's initial
  // paint without producing a navigation failure/error page.
  let deadline = Instant::now() + MAX_WAIT;
  let mut started = false;
  let mut bumped = false;
  let mut captured: Vec<WorkerToUi> = Vec::new();
  while Instant::now() < deadline && !bumped {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationStarted {
            tab_id: got,
            url: started_url,
          } if *got == tab_id && started_url == &url => {
            started = true;
          }
          WorkerToUi::Stage { tab_id: got, stage }
            if *got == tab_id
              && started
              && !bumped
              && !matches!(
                stage,
                StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
              ) =>
          {
            bumped = true;
            cancel.bump_paint();
            worker
              .tx
              .send(UiToWorker::ViewportChanged {
                tab_id,
                viewport_css: VIEWPORT_B,
                dpr: 1.0,
              })
              .expect("viewport B");
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(
    bumped,
    "timed out waiting for prepare-stage heartbeat during navigation; messages:\n{}",
    support::format_messages(&captured)
  );

  // Disable the artificial slowdown once the cancellation-triggering bump has happened; the rest
  // of the test just needs the navigation + repaint to complete.
  drop(delay_guard);

  let deadline = Instant::now() + MAX_WAIT;
  let mut saw_failed = false;
  let mut saw_committed = false;
  let mut saw_frame = None::<(u32, u32)>;
  let mut saw_stale_frame_after_commit = false;
  while Instant::now() < deadline && !(saw_committed && saw_frame.is_some()) {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationCommitted {
            tab_id: got,
            url: got_url,
            ..
          } if *got == tab_id && got_url == &url => {
            saw_committed = true;
          }
          WorkerToUi::NavigationFailed {
            tab_id: got,
            url: got_url,
            ..
          } if *got == tab_id && got_url == &url => {
            saw_failed = true;
          }
          WorkerToUi::FrameReady { tab_id: got, frame } if *got == tab_id => {
            // Only accept a frame rendered with the updated viewport; an in-flight stale paint frame
            // should be cancelled/dropped.
            if saw_committed && frame.viewport_css == VIEWPORT_A {
              saw_stale_frame_after_commit = true;
            }
            if frame.viewport_css == VIEWPORT_B {
              saw_frame = Some(frame.viewport_css);
            }
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  drop(worker.tx);
  worker.join.join().expect("worker join");

  assert!(
    !saw_failed,
    "unexpected NavigationFailed after bump_paint during navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_stale_frame_after_commit,
    "saw stale FrameReady (viewport A) after navigation committed; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    saw_committed,
    "expected NavigationCommitted after bump_paint during navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert_eq!(
    saw_frame,
    Some(VIEWPORT_B),
    "expected FrameReady after cancellation to use the updated viewport; messages:\n{}",
    support::format_messages(&captured)
  );
}

#[test]
fn canceled_navigation_does_not_mutate_committed_base_url_hints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  let page_a_url = site.write(
    "a/index.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link { display: block; width: 160px; height: 60px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a href="target.html" id="link">Go</a>
        </body>
      </html>
    "##,
  );
  let target_a_url = site.write(
    "a/target.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>Target A</body>
      </html>
    "##,
  );

  // The cancelled navigation targets a different directory, so a stale base URL hint would cause
  // `href="target.html"` to resolve to `b/target.html` instead of `a/target.html`.
  let slow_b_url = {
    let mut body = String::new();
    // Keep this comfortably large so the navigation remains in-flight long enough for us to cancel
    // after we observe stage heartbeats.
    for i in 0..8000 {
      body.push_str(&format!("<div class=\"row\">row {i}</div>\n"));
    }
    site.write(
      "b/slow.html",
      &format!(
        r##"<!doctype html>
          <html>
            <head>
              <style>
                html, body {{ margin: 0; padding: 0; }}
                .row {{ height: 16px; border-bottom: 1px solid #ccc; }}
              </style>
            </head>
            <body>
              {body}
            </body>
          </html>
        "##,
      ),
    )
  };
  let target_b_url = site.write(
    "b/target.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>Target B</body>
      </html>
    "##,
  );

  let cancel = CancelGens::new();
  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();

  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(page_a_url.clone()),
      cancel: cancel.clone(),
    })
    .expect("CreateTab");
  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("ViewportChanged");

  let deadline = Instant::now() + TIMEOUT;
  let mut captured: Vec<WorkerToUi> = Vec::new();
  let mut initial_frame = None::<fastrender::ui::messages::RenderedFrame>;
  while Instant::now() < deadline && initial_frame.is_none() {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { frame, .. } => {
          initial_frame = Some(frame);
        }
        other => captured.push(other),
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  let initial_frame = initial_frame.unwrap_or_else(|| {
    captured.extend(support::drain_for(&worker.rx, Duration::from_millis(200)));
    panic!(
      "timed out waiting for initial FrameReady; messages:\n{}",
      support::format_messages(&captured)
    )
  });
  assert_eq!(
    support::rgba_at(&initial_frame.pixmap, 10, 10),
    [255, 0, 0, 255],
    "expected Page A link background at top before cancellation"
  );

  // Drain follow-up messages from the initial navigation to reduce flakiness.
  let _ = support::drain_for(&worker.rx, Duration::from_millis(100));

  worker
    .tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: slow_b_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate to slow page B");

  // Wait until rendering work for B has actually started (Stage heartbeat), then cancel it.
  let _started = support::recv_for_tab(
    &worker.rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url == &slow_b_url),
  )
  .expect("expected NavigationStarted for B");
  let _stage = support::recv_for_tab(&worker.rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::Stage { .. })
  })
  .expect("expected Stage heartbeat for B");
  cancel.bump_nav();

  // Click the link in Page A. If the cancelled navigation mutated the committed document's base URL
  // hint, this will incorrectly resolve to `b/target.html`.
  worker
    .tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("PointerDown");
  worker
    .tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("PointerUp");

  let deadline = Instant::now() + TIMEOUT;
  let mut committed_url: Option<String> = None;
  let mut final_pixel: Option<[u8; 4]> = None;
  let mut saw_b_commit_or_fail = false;
  let mut captured: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline && (committed_url.is_none() || final_pixel.is_none()) {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationCommitted { url, .. } => {
            if url == &slow_b_url || url == &target_b_url {
              saw_b_commit_or_fail = true;
            }
            if url == &target_a_url {
              committed_url = Some(url.clone());
            }
          }
          WorkerToUi::NavigationFailed { url, .. } => {
            if url == &slow_b_url || url == &target_b_url {
              saw_b_commit_or_fail = true;
            }
          }
          WorkerToUi::FrameReady { frame, .. } => {
            if committed_url.is_some() {
              // Sample away from the default body text so antialiasing doesn't affect the pixel.
              final_pixel = Some(support::rgba_at(&frame.pixmap, 190, 110));
            }
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  if committed_url.is_none() || final_pixel.is_none() {
    captured.extend(support::drain_for(&worker.rx, Duration::from_millis(200)));
  }

  assert!(
    !saw_b_commit_or_fail,
    "expected cancelled navigation to B to be silent; messages:\n{}",
    support::format_messages(&captured)
  );

  assert_eq!(
    committed_url.as_deref(),
    Some(target_a_url.as_str()),
    "expected click to resolve against Page A base URL (a/target.html); messages:\n{}",
    support::format_messages(&captured)
  );
  assert_eq!(
    final_pixel,
    Some([0, 255, 0, 255]),
    "expected navigation to a/target.html to paint green background; messages:\n{}",
    support::format_messages(&captured)
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
