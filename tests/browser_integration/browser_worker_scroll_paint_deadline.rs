#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use std::time::{Duration, Instant};

#[test]
fn scroll_paint_deadline_allows_followup_scroll_processing() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _budget = crate::common::global_state::EnvVarGuard::set("FASTR_SCROLL_PAINT_BUDGET_MS", "10");

  let site = support::TempSite::new();
  let scroll_url = site.write(
    "scroll.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Scroll Fixture</title>
  </head>
  <body style="margin: 0">
    <div style="height: 4000px">scroll</div>
  </body>
</html>"#,
  );

  // Simulate a very slow paint pipeline by slowing every render deadline check. Without a scroll
  // paint budget, a scroll-triggered repaint can monopolize the worker for seconds.
  let worker = fastrender::ui::spawn_browser_worker_for_test(Some(200))
    .expect("spawn browser worker for test");

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  worker
    .tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      Some(scroll_url),
      cancel,
    ))
    .expect("CreateTab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("ViewportChanged");

  let initial_frame = support::recv_until(
    &worker.rx,
    Duration::from_secs(30),
    |msg| matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id),
  )
  .unwrap_or_else(|| panic!("timed out waiting for initial FrameReady for tab {tab_id:?}"));
  let WorkerToUi::FrameReady { frame, .. } = initial_frame else {
    unreachable!()
  };
  assert_eq!(frame.viewport_css, (64, 64));

  // Drain the post-frame ScrollStateUpdated/LoadingState/etc so our subsequent waits don't match
  // stale messages.
  for _ in worker.rx.try_iter() {}

  worker
    .tx
    .send(support::scroll_msg(tab_id, (0.0, 40.0), None))
    .expect("scroll 1");
  let first_scroll_msg = support::recv_until(&worker.rx, Duration::from_secs(2), |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { tab_id: t, scroll } if *t == tab_id && scroll.viewport.y > 0.0)
  })
  .unwrap_or_else(|| panic!("timed out waiting for first ScrollStateUpdated for tab {tab_id:?}"));
  let WorkerToUi::ScrollStateUpdated {
    scroll: first_scroll,
    ..
  } = first_scroll_msg
  else {
    unreachable!()
  };

  // Give the worker a moment to enter the (slow) scroll repaint so the next scroll arrives while
  // rendering is in-flight.
  std::thread::sleep(Duration::from_millis(20));

  let start = Instant::now();
  worker
    .tx
    .send(support::scroll_msg(tab_id, (0.0, 40.0), None))
    .expect("scroll 2");

  let second_scroll_msg = support::recv_until(&worker.rx, Duration::from_secs(5), |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { tab_id: t, scroll } if *t == tab_id && scroll.viewport.y > first_scroll.viewport.y + 1.0)
  })
  .unwrap_or_else(|| {
    panic!(
      "timed out waiting for follow-up ScrollStateUpdated; first={:?}",
      first_scroll.viewport
    )
  });
  let WorkerToUi::ScrollStateUpdated {
    scroll: second_scroll,
    ..
  } = second_scroll_msg
  else {
    unreachable!()
  };

  assert!(
    start.elapsed() < Duration::from_secs(1),
    "follow-up scroll update took too long: {:?} (first={:?} second={:?})",
    start.elapsed(),
    first_scroll.viewport,
    second_scroll.viewport
  );

  worker
    .tx
    .send(UiToWorker::CloseTab { tab_id })
    .expect("CloseTab");

  drop(worker.tx);
  worker.join.join().expect("join browser worker");
}
