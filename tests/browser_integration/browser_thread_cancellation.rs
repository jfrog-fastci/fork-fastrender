#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use std::time::{Duration, Instant};

// Worker startup + navigation + render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn rapid_navigation_cancels_stale_navigation_without_emitting_stale_commits_or_frames() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  site.write(
    "style.css",
    r#"
      html, body { margin: 0; padding: 0; }
      body { font: 14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }
      .row { padding: 4px 8px; border-bottom: 1px solid rgba(0, 0, 0, 0.08); }
      .row:nth-child(even) { background: rgba(127, 127, 127, 0.08); }
    "#,
  );

  let mut body = String::new();
  // Enough DOM nodes to keep prepare/layout/paint busy long enough that we can deterministically
  // cancel mid-navigation via stage heartbeats.
  for i in 0..15_000u32 {
    use std::fmt::Write;
    let _ = writeln!(&mut body, "<div class=\"row\">row {i}</div>");
  }

  let heavy_url = site.write(
    "heavy.html",
    &format!(
      r#"<!doctype html>
        <html>
          <head>
            <meta charset="utf-8">
            <title>Heavy</title>
            <link rel="stylesheet" href="style.css">
          </head>
          <body>
            {body}
          </body>
        </html>
      "#
    ),
  );

  let cheap_url = "about:blank".to_string();

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  tx.send(support::create_tab_msg_with_cancel(
    tab_id,
    Some(cheap_url.clone()),
    cancel.clone(),
  ))
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (240, 140), 1.0))
    .expect("ViewportChanged");

  // Wait for the initial `about:blank` render so the tab has an installed document.
  let _ = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");

  // Clear any queued messages from the initial navigation/render.
  let _ = support::drain_for(&rx, Duration::from_millis(100));

  tx.send(support::navigate_msg(
    tab_id,
    heavy_url.clone(),
    NavigationReason::TypedUrl,
  ))
  .expect("Navigate heavy");

  let deadline = Instant::now() + TIMEOUT;
  let mut captured: Vec<WorkerToUi> = Vec::new();
  let mut saw_heavy_started = false;
  let mut sent_cancel = false;
  let mut last_committed: Option<String> = None;
  let mut saw_cheap_commit = false;
  let mut saw_cheap_frame = false;

  while Instant::now() < deadline {
    match rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationStarted {
            tab_id: got,
            url,
          } if *got == tab_id && url == &heavy_url => {
            saw_heavy_started = true;
          }
          WorkerToUi::Stage { tab_id: got, .. } if *got == tab_id && saw_heavy_started && !sent_cancel => {
            // Cancel the in-flight heavy navigation before the worker can emit its commit/frame.
            cancel.bump_nav();
            tx.send(support::navigate_msg(
              tab_id,
              cheap_url.clone(),
              NavigationReason::TypedUrl,
            ))
            .expect("Navigate cheap");
            sent_cancel = true;
          }
          WorkerToUi::NavigationCommitted {
            tab_id: got,
            url,
            ..
          } if *got == tab_id => {
            last_committed = Some(url.clone());
            if url == &cheap_url {
              saw_cheap_commit = true;
            }
          }
          WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id => {
            if last_committed.as_deref() == Some(cheap_url.as_str()) {
              saw_cheap_frame = true;
            }
          }
          _ => {}
        }
        captured.push(msg);
        if saw_cheap_commit && saw_cheap_frame {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  if !(sent_cancel && saw_cheap_commit && saw_cheap_frame) {
    captured.extend(support::drain_for(&rx, Duration::from_millis(200)));
    panic!(
      "timed out waiting for cheap navigation commit+frame (sent_cancel={sent_cancel}, saw_cheap_commit={saw_cheap_commit}, saw_cheap_frame={saw_cheap_frame}); messages:\n{}",
      support::format_messages(&captured)
    );
  }

  // After observing the cheap commit+frame, ensure the cancelled heavy navigation does not emit a
  // stale commit/frame or a cancellation error.
  let drained = support::drain_for(&rx, Duration::from_secs(1));
  captured.extend(drained);

  let saw_heavy_committed = captured.iter().any(|msg| matches!(
    msg,
    WorkerToUi::NavigationCommitted { tab_id: got, url, .. }
      if *got == tab_id && url == &heavy_url
  ));
  assert!(
    !saw_heavy_committed,
    "expected no NavigationCommitted for cancelled heavy navigation; messages:\n{}",
    support::format_messages(&captured)
  );

  let saw_heavy_failed = captured.iter().any(|msg| matches!(
    msg,
    WorkerToUi::NavigationFailed { tab_id: got, url, .. }
      if *got == tab_id && url == &heavy_url
  ));
  assert!(
    !saw_heavy_failed,
    "expected cancellation to be silent (no NavigationFailed for heavy URL); messages:\n{}",
    support::format_messages(&captured)
  );

  let frame_count = captured.iter().filter(|msg| matches!(
    msg,
    WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id
  )).count();
  assert_eq!(
    frame_count,
    1,
    "expected exactly one FrameReady (for the cheap navigation); messages:\n{}",
    support::format_messages(&captured)
  );

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

