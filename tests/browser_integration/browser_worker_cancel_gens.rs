#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);

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
fn browser_worker_cancel_navigation_via_ui_held_cancel_gens() {
  let _lock = super::stage_listener_test_lock();
  // Slow down render stages to make cancellation deterministic.
  let _delay = TestRenderDelayGuard::set(Some(1));

  let site = support::TempSite::new();

  let mut css = String::new();
  // Enough selectors to keep CSS parsing/cascade work non-trivial even in debug builds.
  for i in 0..4_000 {
    css.push_str(&format!(
      ".c{i} {{ padding: {}px; margin: {}px; border: {}px solid rgb({}, {}, {}); }}\n",
      i % 8,
      i % 8,
      i % 3,
      i % 255,
      (i * 3) % 255,
      (i * 7) % 255
    ));
  }
  site.write("style.css", &css);

  let mut body = String::new();
  for i in 0..8_000 {
    body.push_str(&format!("<div class=\"c{i}\">row {i}</div>\n"));
  }

  let heavy_url = site.write(
    "heavy.html",
    &format!(
      r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <link rel="stylesheet" href="style.css">
        </head>
        <body>
          {body}
        </body>
      </html>"#
    ),
  );

  let light_url = site.write("light.html", "<!doctype html><html><body>ok</body></html>");

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  worker
    .tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      Some("about:blank".to_string()),
      cancel.clone(),
    ))
    .expect("create tab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (240, 160), 1.0))
    .expect("viewport");

  // Wait for the initial about:blank frame so we know the worker is live.
  support::recv_for_tab(&worker.rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");
  let _ = support::drain_for(&worker.rx, Duration::from_millis(50));

  worker
    .tx
    .send(support::navigate_msg(
      tab_id,
      heavy_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate heavy");

  // Wait until the navigation is in-flight (we've seen both NavigationStarted and at least one
  // Stage heartbeat), then cancel it from the UI thread.
  let mut saw_started = false;
  let mut saw_stage = false;
  let mut captured: Vec<WorkerToUi> = Vec::new();
  let deadline = Instant::now() + TIMEOUT;
  while Instant::now() < deadline && !saw_stage {
    match worker.rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationStarted { tab_id: got, url } if *got == tab_id && url == &heavy_url => {
            saw_started = true;
          }
          WorkerToUi::Stage { tab_id: got, .. } if *got == tab_id && saw_started => {
            saw_stage = true;
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    saw_stage,
    "expected Stage heartbeat during heavy navigation; messages:\n{}",
    support::format_messages(&captured)
  );

  cancel.bump_nav();
  // Remove the synthetic slowdown so the follow-up navigation completes quickly.
  fastrender::render_control::set_test_render_delay_ms(None);
  worker
    .tx
    .send(support::navigate_msg(
      tab_id,
      light_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate light");

  let mut last_committed: Option<String> = None;
  let mut saw_light_commit = false;
  let mut saw_light_frame = false;
  let mut saw_heavy_commit = false;
  let mut saw_heavy_frame = false;

  let deadline = Instant::now() + TIMEOUT;
  while Instant::now() < deadline && !(saw_light_commit && saw_light_frame) {
    match worker.rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationCommitted { tab_id: got, url, .. } if *got == tab_id => {
            last_committed = Some(url.clone());
            if url == &light_url {
              saw_light_commit = true;
            }
            if url == &heavy_url {
              saw_heavy_commit = true;
            }
          }
          WorkerToUi::NavigationFailed { tab_id: got, url, .. } if *got == tab_id => {
            last_committed = Some(url.clone());
          }
          WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id => {
            if last_committed.as_deref() == Some(light_url.as_str()) {
              saw_light_frame = true;
            }
            if last_committed.as_deref() == Some(heavy_url.as_str()) {
              saw_heavy_frame = true;
            }
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  if !(saw_light_commit && saw_light_frame) {
    captured.extend(support::drain_for(&worker.rx, Duration::from_millis(200)));
  }

  assert!(
    saw_light_commit,
    "expected NavigationCommitted for light navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    saw_light_frame,
    "expected FrameReady for light navigation; messages:\n{}",
    support::format_messages(&captured)
  );

  assert!(
    !saw_heavy_commit,
    "expected cancelled heavy navigation to not commit; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_heavy_frame,
    "expected no FrameReady for cancelled heavy navigation; messages:\n{}",
    support::format_messages(&captured)
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
