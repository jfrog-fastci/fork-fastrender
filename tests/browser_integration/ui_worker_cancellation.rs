#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg_with_cancel, navigate_msg, scroll_msg, viewport_changed_msg};
use fastrender::render_control::StageHeartbeat;
use fastrender::scroll::ScrollState;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_for_test;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const MAX_WAIT: Duration = Duration::from_secs(15);

fn pixmap_is_uniform_rgba(pixmap: &tiny_skia::Pixmap) -> bool {
  let data = pixmap.data();
  let Some(first) = data.get(0..4) else {
    return true;
  };
  data.chunks_exact(4).all(|px| px == first)
}

fn recv_until<F: FnMut(&WorkerToUi) -> bool>(
  rx: &fastrender::ui::WorkerToUiInbox,
  timeout: Duration,
  mut predicate: F,
) -> Vec<WorkerToUi> {
  let start = Instant::now();
  let mut out = Vec::new();
  while start.elapsed() < timeout {
    match rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => {
        if predicate(&msg) {
          out.push(msg);
          break;
        }
        out.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  out
}

#[test]
fn ui_worker_nav_generation_cancels_in_flight_navigation_and_drops_stale_frame() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  // Slow down render stages to make cancellation deterministic.
  let (ui_tx, ui_rx, join) = spawn_ui_worker_for_test("fastr-ui-worker-cancel-nav-gens", Some(20))
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  ui_tx
    .send(create_tab_msg_with_cancel(tab_id, None, cancel.clone()))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("ViewportChanged");

  cancel.bump_nav();
  ui_tx
    .send(navigate_msg(
      tab_id,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate about:newtab");

  let started = recv_until(&ui_rx, MAX_WAIT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationStarted { tab_id: msg_tab, url, .. }
        if *msg_tab == tab_id && url == "about:newtab"
    )
  });
  assert!(
    started.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { tab_id: msg_tab, url, .. }
        if *msg_tab == tab_id && url == "about:newtab"
    )),
    "expected NavigationStarted for about:newtab (messages={started:?})"
  );

  // Bumping nav cancels both prepare and paint for the in-flight navigation.
  cancel.bump_nav();
  ui_tx
    .send(navigate_msg(
      tab_id,
      "about:blank".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate about:blank");

  // The first frame we receive must correspond to about:blank. about:newtab has non-uniform pixels.
  let deadline = Instant::now() + MAX_WAIT;
  let frame = loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ui_rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => break frame,
      Ok(WorkerToUi::NavigationFailed { url, error, .. }) => {
        panic!("navigation failed for {url}: {error}");
      }
      Ok(_) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
    }
  };

  assert!(
    pixmap_is_uniform_rgba(&frame.pixmap),
    "expected about:blank to render as uniform pixmap; got non-uniform pixels (did cancellation fail?)"
  );

  // Ensure a stale FrameReady doesn't arrive after the latest navigation frame.
  let extra_frame = recv_until(
    &ui_rx,
    Duration::from_secs(1),
    |msg| matches!(msg, WorkerToUi::FrameReady { tab_id: msg_tab, .. } if *msg_tab == tab_id),
  );
  assert!(
    extra_frame
      .iter()
      .all(|msg| !matches!(msg, WorkerToUi::FrameReady { .. })),
    "unexpected additional FrameReady messages after latest navigation frame: {extra_frame:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn rapid_navigation_cancels_stale_navigation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");

  std::fs::write(
    dir.path().join("style.css"),
    r#"
      html, body { margin: 0; padding: 0; }
      body { font: 14px sans-serif; }
      .box { width: 160px; height: 80px; background: rgb(10, 20, 30); margin: 8px; }
    "#,
  )
  .expect("write style.css");

  std::fs::write(
    dir.path().join("a.html"),
    r#"<!doctype html>
      <html>
        <head>
          <title>Page A</title>
          <link rel="stylesheet" href="style.css">
        </head>
        <body>
          <div class="box"></div>
          <div>AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA</div>
        </body>
      </html>
    "#,
  )
  .expect("write a.html");

  std::fs::write(
    dir.path().join("b.html"),
    r#"<!doctype html>
      <html>
        <head>
          <title>Page B</title>
          <link rel="stylesheet" href="style.css">
        </head>
        <body>
          <div class="box"></div>
          <div>BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB</div>
        </body>
      </html>
    "#,
  )
  .expect("write b.html");

  let url_a = url::Url::from_file_path(dir.path().join("a.html"))
    .unwrap()
    .to_string();
  let url_b = url::Url::from_file_path(dir.path().join("b.html"))
    .unwrap()
    .to_string();

  let cancel_gens = CancelGens::new();
  let (ui_tx, ui_rx, join) = spawn_ui_worker_for_test("fastr-ui-worker-cancel-nav", Some(10))
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg_with_cancel(
      tab_id,
      None,
      cancel_gens.clone(),
    ))
    .unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();

  ui_tx
    .send(navigate_msg(
      tab_id,
      url_a.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let mut messages = recv_until(&ui_rx, MAX_WAIT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationStarted { tab_id: msg_tab, url, .. }
        if *msg_tab == tab_id && url == &url_a
    )
  });
  assert!(
    messages.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { tab_id: msg_tab, url, .. }
        if *msg_tab == tab_id && url == &url_a
    )),
    "expected NavigationStarted for A (messages={messages:?})"
  );

  cancel_gens.bump_nav();
  ui_tx
    .send(navigate_msg(
      tab_id,
      url_b.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let mut committed_b = false;
  let mut saw_b_frame = false;

  let start = Instant::now();
  while start.elapsed() < MAX_WAIT && !(committed_b && saw_b_frame) {
    match ui_rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationCommitted {
            tab_id: msg_tab,
            url,
            ..
          } if *msg_tab == tab_id && url == &url_b => {
            committed_b = true;
          }
          WorkerToUi::FrameReady {
            tab_id: msg_tab, ..
          } if *msg_tab == tab_id => {
            saw_b_frame = true;
          }
          _ => {}
        }
        messages.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let mut committed_a = false;
  let mut failed_a = false;

  for msg in &messages {
    match msg {
      WorkerToUi::NavigationCommitted { url, .. } => {
        if url == &url_a {
          committed_a = true;
        }
      }
      WorkerToUi::NavigationFailed { url, .. } => {
        if url == &url_a {
          failed_a = true;
        }
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab, ..
      } if *msg_tab == tab_id => {
        // Already tracked via the loop above.
      }
      _ => {}
    }
  }

  assert!(
    committed_b,
    "expected NavigationCommitted for B (messages={messages:?})"
  );
  assert!(
    saw_b_frame,
    "expected FrameReady for B (messages={messages:?})"
  );
  assert!(
    !committed_a,
    "expected no NavigationCommitted for A (messages={messages:?})"
  );
  assert!(
    !failed_a,
    "expected no NavigationFailed for A (cancellation should be silent; messages={messages:?})"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn rapid_scroll_cancels_stale_paint() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");

  let mut body = String::new();
  for i in 0..64 {
    body.push_str(&format!("<div class=\"row\">row {i}</div>\n"));
  }

  std::fs::write(
    dir.path().join("scroll.html"),
    format!(
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
        </html>
      "#
    ),
  )
  .expect("write scroll.html");

  let url = url::Url::from_file_path(dir.path().join("scroll.html"))
    .unwrap()
    .to_string();

  let cancel_gens = CancelGens::new();
  let (ui_tx, ui_rx, join) = spawn_ui_worker_for_test("fastr-ui-worker-cancel-scroll", Some(50))
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg_with_cancel(
      tab_id,
      None,
      cancel_gens.clone(),
    ))
    .unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();

  ui_tx
    .send(navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let _initial = recv_until(&ui_rx, MAX_WAIT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  });

  // Clear any remaining stage/navigation messages before we start the scroll assertions.
  for _ in ui_rx.try_iter() {}

  ui_tx.send(scroll_msg(tab_id, (0.0, 80.0), None)).unwrap();

  // Wait for the scroll repaint to enter the paint stages so we can cancel it mid-flight.
  let is_paint_stage = |msg: &WorkerToUi| {
    matches!(
      msg,
      WorkerToUi::Stage { tab_id: msg_tab, stage }
        if *msg_tab == tab_id
          && matches!(
            stage,
            StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
          )
    )
  };
  let paint_stage_msgs = recv_until(&ui_rx, MAX_WAIT, |msg| is_paint_stage(msg));
  assert!(
    paint_stage_msgs.iter().any(is_paint_stage),
    "expected paint stage heartbeat during first scroll repaint (messages={paint_stage_msgs:?})"
  );

  cancel_gens.bump_paint();
  ui_tx.send(scroll_msg(tab_id, (0.0, 80.0), None)).unwrap();

  // `ScrollStateUpdated` may arrive either before or after the corresponding `FrameReady`. Some
  // straggler scroll updates (e.g. from the initial navigation) can arrive after we start this
  // test, so match by viewport to ensure we associate the scroll update with the latest painted
  // frame.
  let mut matching_scroll_update: Option<ScrollState> = None;
  let mut pending_scroll_updates: Vec<ScrollState> = Vec::new();
  let mut frames: Vec<ScrollState> = Vec::new();

  let start = Instant::now();
  while start.elapsed() < MAX_WAIT && (frames.is_empty() || matching_scroll_update.is_none()) {
    match ui_rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => match msg {
        WorkerToUi::ScrollStateUpdated {
          tab_id: msg_tab,
          scroll,
        } if msg_tab == tab_id => {
          if let Some(frame_scroll) = frames.first() {
            if scroll.viewport == frame_scroll.viewport {
              matching_scroll_update = Some(scroll);
            }
          } else {
            pending_scroll_updates.push(scroll);
          }
        }
        WorkerToUi::FrameReady {
          tab_id: msg_tab,
          frame,
        } if msg_tab == tab_id => {
          frames.push(frame.scroll_state.clone());
          if frames.len() == 1 && matching_scroll_update.is_none() {
            let expected_viewport = frame.scroll_state.viewport;
            if let Some(idx) = pending_scroll_updates
              .iter()
              .position(|scroll| scroll.viewport == expected_viewport)
            {
              matching_scroll_update = Some(pending_scroll_updates.remove(idx));
            }
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert_eq!(
    frames.len(),
    1,
    "expected exactly one FrameReady after scroll; frames={frames:?}"
  );
  let frame_scroll = &frames[0];
  assert!(
    (frame_scroll.viewport.y - 160.0).abs() < 0.5,
    "expected painted scroll_y ~= 160, got {:?}",
    frame_scroll.viewport
  );

  let Some(latest) = matching_scroll_update else {
    panic!("expected ScrollStateUpdated message matching the scroll frame");
  };
  assert_eq!(
    latest.viewport, frame_scroll.viewport,
    "expected FrameReady.scroll_state to match ScrollStateUpdated"
  );

  // Ensure a stale FrameReady doesn't arrive after the latest frame.
  let extra_frame = recv_until(&ui_rx, Duration::from_secs(1), |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  });
  assert!(
    extra_frame
      .iter()
      .all(|msg| !matches!(msg, WorkerToUi::FrameReady { .. })),
    "unexpected additional FrameReady messages after latest scroll frame: {extra_frame:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn rapid_ticks_cancel_stale_paint() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");

  std::fs::write(
    dir.path().join("anim.html"),
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            html, body { background: rgb(0, 0, 0); }
            #box {
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
              animation: fade 100ms linear infinite;
            }
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  )
  .expect("write anim.html");

  let url = url::Url::from_file_path(dir.path().join("anim.html"))
    .unwrap()
    .to_string();

  let cancel_gens = CancelGens::new();
  // Slow down paints so tick bursts have time to cancel in-flight work.
  let (ui_tx, ui_rx, join) = spawn_ui_worker_for_test("fastr-ui-worker-cancel-tick", Some(50))
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId::new();

  ui_tx
    .send(create_tab_msg_with_cancel(
      tab_id,
      Some(url.clone()),
      cancel_gens.clone(),
    ))
    .unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (64, 64), 1.0))
    .unwrap();

  // Wait for the initial navigation frame so the document is prepared.
  let initial = recv_until(&ui_rx, MAX_WAIT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { tab_id: msg_tab, .. } if *msg_tab == tab_id
    )
  });
  let initial_frame = initial.iter().find_map(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  });
  assert!(
    initial_frame.is_some_and(|frame| frame.next_tick.is_some()),
    "expected animation page to request ticks; messages={initial:?}"
  );

  // Drain any remaining stage/navigation messages before we start the tick assertions.
  for _ in ui_rx.try_iter() {}

  // Start a tick-driven paint and wait for it to enter the paint stages so we can cancel it
  // mid-flight.
  cancel_gens.bump_paint();
  ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .unwrap();

  let is_paint_stage = |msg: &WorkerToUi| {
    matches!(
      msg,
      WorkerToUi::Stage { tab_id: msg_tab, stage }
        if *msg_tab == tab_id
          && matches!(stage, StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize)
    )
  };
  let paint_stage_msgs = recv_until(&ui_rx, MAX_WAIT, |msg| is_paint_stage(msg));
  assert!(
    paint_stage_msgs.iter().any(is_paint_stage),
    "expected paint stage heartbeat during tick repaint (messages={paint_stage_msgs:?})"
  );

  // Send a burst of ticks; each tick should cancel any in-flight paint so we only render the final
  // animation state.
  for _ in 0..5 {
    cancel_gens.bump_paint();
    ui_tx
      .send(UiToWorker::Tick {
        tab_id,
        delta: Duration::from_millis(16),
      })
      .unwrap();
  }

  let mut frames = Vec::new();
  let start = Instant::now();
  while start.elapsed() < MAX_WAIT && frames.len() < 1 {
    match ui_rx.recv_timeout(Duration::from_millis(50)) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => {
        frames.push(frame);
      }
      Ok(_) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert_eq!(
    frames.len(),
    1,
    "expected exactly one FrameReady after tick burst; frames={} (slow paint should cancel stale frames)",
    frames.len()
  );

  // Ensure a stale FrameReady doesn't arrive after the latest frame.
  let extra_frame = recv_until(&ui_rx, Duration::from_secs(1), |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { tab_id: msg_tab, .. } if *msg_tab == tab_id
    )
  });
  assert!(
    extra_frame
      .iter()
      .all(|msg| !matches!(msg, WorkerToUi::FrameReady { .. })),
    "unexpected additional FrameReady messages after latest tick frame: {extra_frame:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
