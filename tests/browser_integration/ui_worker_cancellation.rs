#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg_with_cancel, navigate_msg, scroll_msg, viewport_changed_msg};
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::scroll::ScrollState;
use fastrender::ui::worker_loop::spawn_ui_worker_for_test;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const MAX_WAIT: Duration = Duration::from_secs(15);

fn recv_until<F: FnMut(&WorkerToUi) -> bool>(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
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
fn rapid_navigation_cancels_stale_navigation() {
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

  let url_a = format!("file://{}/a.html", dir.path().display());
  let url_b = format!("file://{}/b.html", dir.path().display());

  let handle =
    spawn_ui_worker_for_test("fastr-ui-worker-cancel-nav", Some(10)).expect("spawn ui worker");
  let cancel_gens = handle.cancel_gens();
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg_with_cancel(tab_id, None, cancel_gens.clone()))
    .unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();

  ui_tx
    .send(navigate_msg(tab_id, url_a.clone(), NavigationReason::TypedUrl))
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
    .send(navigate_msg(tab_id, url_b.clone(), NavigationReason::TypedUrl))
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
          } if *msg_tab == tab_id && url == &url_b => committed_b = true,
          WorkerToUi::FrameReady { tab_id: msg_tab, .. } if *msg_tab == tab_id => {
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
      WorkerToUi::FrameReady { tab_id: msg_tab, .. } if *msg_tab == tab_id => {
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

  let url = format!("file://{}/scroll.html", dir.path().display());

  let handle =
    spawn_ui_worker_for_test("fastr-ui-worker-cancel-scroll", Some(50)).expect("spawn ui worker");
  let cancel_gens = handle.cancel_gens();
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg_with_cancel(tab_id, None, cancel_gens.clone()))
    .unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();

  ui_tx
    .send(navigate_msg(tab_id, url.clone(), NavigationReason::TypedUrl))
    .unwrap();

  let _initial = recv_until(&ui_rx, MAX_WAIT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }));

  // Clear any remaining stage/navigation messages before we start the scroll assertions.
  for _ in ui_rx.try_iter() {}

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 80.0), None))
    .unwrap();
  // Give the worker thread a chance to begin painting the first scroll. The per-thread render delay
  // ensures the paint stays in-flight long enough to reliably cancel.
  std::thread::sleep(Duration::from_millis(20));
  cancel_gens.bump_paint();
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 80.0), None))
    .unwrap();

  let mut latest_scroll: Option<ScrollState> = None;
  let mut frames: Vec<ScrollState> = Vec::new();

  let start = Instant::now();
  while start.elapsed() < MAX_WAIT {
    match ui_rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => match msg {
        WorkerToUi::ScrollStateUpdated { tab_id: msg_tab, scroll } if msg_tab == tab_id => {
          latest_scroll = Some(scroll);
        }
        WorkerToUi::FrameReady { tab_id: msg_tab, frame } if msg_tab == tab_id => {
          frames.push(frame.scroll_state.clone());
          break;
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

  let Some(latest) = latest_scroll else {
    panic!("expected ScrollStateUpdated before FrameReady");
  };
  assert_eq!(
    latest.viewport, frame_scroll.viewport,
    "expected FrameReady scroll_state to match ScrollStateUpdated"
  );

  // Ensure a stale FrameReady doesn't arrive after the latest frame.
  let extra_frame = recv_until(&ui_rx, Duration::from_secs(1), |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  });
  assert!(
    extra_frame.iter().all(|msg| !matches!(msg, WorkerToUi::FrameReady { .. })),
    "unexpected additional FrameReady messages after latest scroll frame: {extra_frame:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
