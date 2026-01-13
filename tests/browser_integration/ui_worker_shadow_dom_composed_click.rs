#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_shadow_dom_composed_click_allows_document_listener_to_prevent_default() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }</style>
        </head>
        <body>next</body>
      </html>"#,
  );

  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
          </style>
        </head>
        <body>
          <div id="host">
            <template shadowrootmode="open">
              <!-- Keep the link a predictable hit target so pointer events land on the <a>. -->
              <a href="next.html" style="display: block; width: 120px; height: 40px; background: rgb(255, 0, 0);">next</a>
            </template>
          </div>
          <script>
            // Important: this listener is outside the shadow tree. Without composed propagation, the
            // click will not reach the document and navigation will proceed.
            document.addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-shadow-dom-composed-click",
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

  // Click the link inside the shadow root. The document-level click listener should still observe
  // the event (via composed propagation) and preventDefault(), suppressing navigation.
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

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| matches!(msg, WorkerToUi::NavigationStarted { .. })),
    "expected document-level click preventDefault to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );
  assert!(
    !msgs.iter().any(|msg| matches!(msg, WorkerToUi::NavigationCommitted { .. })),
    "expected document-level click preventDefault to suppress navigation commit to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

