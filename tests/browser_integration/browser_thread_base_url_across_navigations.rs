#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);

fn wait_for_frame_for_committed_url(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  url: &str,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  let mut last_committed: Option<String> = None;
  let mut msgs: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline {
    match rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationCommitted {
          tab_id: got,
          url: committed,
          title,
          can_go_back,
          can_go_forward,
        } => {
          if got == tab_id {
            last_committed = Some(committed.clone());
          }
          msgs.push(WorkerToUi::NavigationCommitted {
            tab_id: got,
            url: committed,
            title,
            can_go_back,
            can_go_forward,
          });
        }
        WorkerToUi::NavigationFailed {
          tab_id: got,
          url: failed,
          error,
          can_go_back,
          can_go_forward,
        } => {
          if got == tab_id {
            last_committed = Some(failed.clone());
          }
          msgs.push(WorkerToUi::NavigationFailed {
            tab_id: got,
            url: failed,
            error,
            can_go_back,
            can_go_forward,
          });
        }
        WorkerToUi::FrameReady { tab_id: got, frame } => {
          if got == tab_id && last_committed.as_deref() == Some(url) {
            return frame;
          }
          msgs.push(WorkerToUi::FrameReady { tab_id: got, frame });
        }
        other => msgs.push(other),
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  panic!(
    "timed out waiting for FrameReady committed to {url}; got:\n{}",
    support::format_messages(&msgs)
  );
}

#[test]
fn browser_thread_updates_base_url_between_navigations_in_same_tab() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site_red = support::TempSite::new();
  let red_url = site_red.write(
    "index.html",
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <link rel="stylesheet" href="style.css">
      </head>
      <body>
        <div id="fill"></div>
      </body>
    </html>"#,
  );
  site_red.write(
    "style.css",
    "html,body{margin:0;padding:0} #fill{width:2000px;height:2000px;background: rgb(255,0,0);}",
  );

  let site_green = support::TempSite::new();
  let green_url = site_green.write(
    "index.html",
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <link rel="stylesheet" href="style.css">
      </head>
      <body>
        <div id="fill"></div>
      </body>
    </html>"#,
  );
  site_green.write(
    "style.css",
    "html,body{margin:0;padding:0} #fill{width:2000px;height:2000px;background: rgb(0,255,0);}",
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId::new();
  worker
    .tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      Some("about:blank".to_string()),
      CancelGens::new(),
    ))
    .expect("CreateTab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("ViewportChanged");

  // Immediately navigate to the first file URL after setting the viewport. This ensures the
  // navigation runs with the final viewport size and avoids relying on the initial about:blank
  // navigation to complete first.
  worker
    .tx
    .send(support::navigate_msg(
      tab_id,
      red_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate red");

  let frame1 = wait_for_frame_for_committed_url(&worker.rx, tab_id, &red_url, TIMEOUT);
  assert_eq!(support::rgba_at(&frame1.pixmap, 1, 1), [255, 0, 0, 255]);

  // Drain any queued messages so subsequent waits are attributable to the second navigation.
  let _ = support::drain_for(&worker.rx, Duration::from_millis(50));

  worker
    .tx
    .send(support::navigate_msg(
      tab_id,
      green_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  let frame2 = wait_for_frame_for_committed_url(&worker.rx, tab_id, &green_url, TIMEOUT);
  assert_eq!(support::rgba_at(&frame2.pixmap, 1, 1), [0, 255, 0, 255]);

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
