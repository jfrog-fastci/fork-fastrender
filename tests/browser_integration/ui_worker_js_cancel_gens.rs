#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::error::RenderStage;
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use fastrender::Error;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[derive(Clone)]
struct SlowTxtFetcher {
  started_tx: mpsc::Sender<()>,
}

impl ResourceFetcher for SlowTxtFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    if url.ends_with("slow.txt") {
      let _ = self.started_tx.send(());
      let start = Instant::now();
      loop {
        // This is a cancellation poll point: with a DeadlineGuard installed around JS execution,
        // `CancelGens` bumps cause `check_active` to error immediately.
        if let Err(err) = fastrender::render_control::check_active(RenderStage::Script) {
          return Err(Error::Render(err));
        }
        if start.elapsed() > Duration::from_secs(2) {
          return Err(Error::Other(
            "SlowTxtFetcher timed out waiting for UI cancellation".to_string(),
          ));
        }
        std::thread::sleep(Duration::from_millis(5));
      }
    }
    support::FileResourceFetcher.fetch(url)
  }
}

#[test]
fn ui_worker_cancel_gens_bump_interrupts_js_event_handler() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  site.write("slow.txt", "ok");
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #target { position: absolute; left: 0; top: 0; width: 120px; height: 60px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="target"></div>
          <script>
            document.getElementById("target").addEventListener("click", function () {
              var xhr = new XMLHttpRequest();
              xhr.open("GET", "slow.txt", false);
              xhr.send(null);
            });
          </script>
        </body>
      </html>"#,
  );

  let (started_tx, started_rx) = mpsc::channel::<()>();
  let fetcher = Arc::new(SlowTxtFetcher { started_tx }) as Arc<dyn ResourceFetcher>;
  let factory = support::deterministic_factory_with_fetcher(fetcher).expect("build factory");
  let handle =
    spawn_ui_worker_with_factory("fastr-ui-worker-js-cancel-gens", factory).expect("spawn worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  ui_tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      Some(index_url.clone()),
      cancel.clone(),
    ))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");

  started_rx
    .recv_timeout(TIMEOUT)
    .expect("expected slow fetch to start in JS click handler");

  let cancel_start = Instant::now();
  cancel.bump_paint();

  let mut cancel_elapsed: Option<Duration> = None;
  let mut debug_line: Option<String> = None;
  let mut captured: Vec<WorkerToUi> = Vec::new();

  let deadline = Instant::now() + Duration::from_secs(5);
  while Instant::now() < deadline && debug_line.is_none() {
    match ui_rx.recv_timeout(Duration::from_millis(25)) {
      Ok(msg) => {
        if let WorkerToUi::DebugLog { tab_id: got, line } = &msg {
          if *got == tab_id {
            cancel_elapsed = Some(cancel_start.elapsed());
            debug_line = Some(line.clone());
          }
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  drop(ui_tx);
  join.join().expect("worker join");

  let Some(elapsed) = cancel_elapsed else {
    panic!(
      "expected a DebugLog after cancelling JS click handler; got:\n{}",
      support::format_messages(&captured)
    );
  };
  let line = debug_line.unwrap_or_default();
  assert!(
    line.contains("timed out")
      || line.contains("timeout")
      || line.contains("Timeout")
      || line.contains("interrupted"),
    "expected DebugLog line to mention timeout/cancellation, got: {line}\nmessages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    elapsed < Duration::from_secs(1),
    "expected CancelGens bump to interrupt JS quickly (elapsed={elapsed:?}); DebugLog: {line}\nmessages:\n{}",
    support::format_messages(&captured)
  );
}
