#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_middle_click_ignores_click_prevent_default_and_still_opens_new_tab() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let _next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html><body>next</body></html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
          <script>
            // Middle clicks should dispatch `auxclick`, not `click`. Preventing default on `click`
            // should therefore *not* suppress the worker's middle-click open-in-new-tab behaviour.
            document.getElementById("link").addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let expected_url = url::Url::parse(&index_url)
    .expect("parse index url")
    .join("next.html")
    .expect("resolve next.html")
    .to_string();

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-auxclick-click-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  // Drain any follow-up messages from the initial navigation to keep assertions scoped to the click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Middle,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Middle,
    ))
    .expect("pointer up");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::RequestOpenInNewTab { .. }
        | WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationFailed { .. }
        | WorkerToUi::RequestOpenInNewTabRequest { .. }
    )
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for RequestOpenInNewTab after middle-clicking link; saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  match msg {
    WorkerToUi::RequestOpenInNewTab { tab_id: got, url } => {
      assert_eq!(got, tab_id);
      assert_eq!(url, expected_url);
    }
    WorkerToUi::RequestOpenInNewTabRequest { request, .. } => {
      panic!(
        "expected RequestOpenInNewTab for link middle click, got RequestOpenInNewTabRequest({:?})",
        request
      );
    }
    WorkerToUi::NavigationStarted { url, .. } => {
      panic!("expected RequestOpenInNewTab for link middle click, got NavigationStarted({url})");
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("unexpected NavigationFailed for link middle click to {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_middle_click_respects_auxclick_prevent_default() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let _next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html><body>next</body></html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
          <script>
            document.getElementById("link").addEventListener("auxclick", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-auxclick-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  // Drain any follow-up messages from the initial navigation to keep assertions scoped to the click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Middle,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Middle,
    ))
    .expect("pointer up");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
          | WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
      )
    }),
    "expected auxclick preventDefault to suppress middle-click open-in-new-tab; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
