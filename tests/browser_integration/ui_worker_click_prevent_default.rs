#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{KeyAction, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_click_prevent_default_blocks_navigation() {
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
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
          <script>
            var link = document.getElementById("link");
            link.addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-click-prevent-default",
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

  // Click the link. JS `preventDefault()` should suppress the worker's navigation scheduling.
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
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected click preventDefault to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_click_prevent_default_blocks_navigation_without_id() {
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
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            a { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a href="next.html">next</a>
          <script>
            var link = document.querySelector("a");
            link.addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-click-prevent-default-without-id",
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

  // Click the link. JS `preventDefault()` should suppress the worker's navigation scheduling.
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
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected click preventDefault to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_enter_on_focused_link_respects_click_prevent_default() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html>
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
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
          <script>
            document.getElementById("link").addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-enter-click-prevent-default",
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
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click once to focus the link (preventDefault suppresses navigation).
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
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Press Enter to activate the focused link. JS `preventDefault()` should still suppress the
  // worker's navigation scheduling.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::Enter))
    .expect("key enter");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected Enter key activation to respect click preventDefault and suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_enter_on_focused_link_respects_click_prevent_default_without_id() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html>
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
            a { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; }
          </style>
        </head>
        <body>
          <a href="next.html">next</a>
          <script>
            document.querySelector("a").addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-enter-click-prevent-default-without-id",
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
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click once to focus the link (preventDefault suppresses navigation).
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
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Press Enter to activate the focused link. JS `preventDefault()` should still suppress the
  // worker's navigation scheduling.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::Enter))
    .expect("key enter");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected Enter key activation to respect click preventDefault and suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_click_prevent_default_without_id_handles_wbr_preorder_shift() {
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

  // Insert many `<wbr>` elements before the link. The renderer injects a synthetic ZWSP text child
  // for each `<wbr>`, which shifts renderer preorder ids relative to dom2 node indices. The UI
  // worker should still dispatch the click event to the correct dom2 `NodeId` so JS `preventDefault`
  // suppresses navigation even without stable `id` attributes.
  let wbrs = "<wbr>".repeat(32);
  let index_html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body {{ margin: 0; padding: 0; }}
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            a {{ position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }}
          </style>
        </head>
        <body>
          <div style="display:none">{wbrs}</div>
          <a href="next.html">next</a>
          <script>
            document.querySelector("a").addEventListener("click", function (ev) {{
              ev.preventDefault();
            }});
          </script>
        </body>
      </html>"#
  );
  let index_url = site.write("index.html", &index_html);

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-click-prevent-default-wbr-preorder-shift",
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
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected click preventDefault (without element ids) to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_submit_prevent_default_after_click_dom_mutation_preorder_shift() {
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

  // The click handler mutates the DOM by inserting many nodes before the form, shifting renderer
  // preorder ids relative to the current dom2 traversal order. The UI worker must still dispatch
  // the subsequent `"submit"` event to the correct form so `preventDefault()` suppresses
  // navigation.
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            /* Give the submit button a predictable hit target. */
            button { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <form action="next.html" method="get">
            <button type="submit">go</button>
          </form>
          <script>
            window.didMutate = false;
            var btn = document.querySelector("button");
            var form = document.querySelector("form");
            btn.addEventListener("click", function () {
              window.didMutate = true;
              var container = document.createElement("div");
              container.style.display = "none";
              for (var i = 0; i < 64; i++) {
                container.appendChild(document.createElement("span"));
              }
              document.body.insertBefore(container, document.body.firstChild);
            });
            form.addEventListener("submit", function (ev) {
              if (window.didMutate) ev.preventDefault();
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-submit-prevent-default-dom-mutation",
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

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(800));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected submit preventDefault to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
