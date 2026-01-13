#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_checkbox_click_mirrors_checked_state_to_js() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
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
            #cb { position: absolute; left: 0; top: 0; width: 40px; height: 40px; }
            #go { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; display: block; }
          </style>
        </head>
        <body>
          <input id="cb" type="checkbox">
          <a id="go" href="next.html">next</a>
          <script>
            document.getElementById("cb").addEventListener("click", function () {
              document.body.setAttribute("data-cb", String(this.checked));
            });
            document.getElementById("go").addEventListener("click", function (ev) {
              // Allow navigation only if the checkbox click handler observed `checked === true`.
              if (document.body.getAttribute("data-cb") !== "true") {
                ev.preventDefault();
              }
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-form-control-mirror-checkbox",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 140), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click the checkbox to toggle it on. The JS click handler should observe `this.checked === true`
  // and persist it into `body[data-cb]`.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down checkbox");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up checkbox");

  // Click the link; its click handler allows navigation only if `data-cb` is `"true"`.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer down link");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer up link");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &next_url)
  })
  .unwrap_or_else(|| panic!("expected navigation to commit to {next_url}"));

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_text_input_mirrors_value_to_js() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
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
            #txt { position: absolute; left: 0; top: 0; width: 180px; height: 40px; font-size: 20px; }
            #go { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; display: block; }
          </style>
        </head>
        <body>
          <input id="txt" type="text">
          <a id="go" href="next.html">next</a>
          <script>
            document.getElementById("go").addEventListener("click", function (ev) {
              // Allow navigation only if JS observes the value typed via UiToWorker::TextInput.
              if (document.getElementById("txt").value !== "hello") {
                ev.preventDefault();
              }
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-form-control-mirror-text",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 140), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Focus the input.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down input");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up input");

  // Type text into the focused input.
  ui_tx
    .send(support::text_input(tab_id, "hello"))
    .expect("text input");

  // Click the link; its click handler allows navigation only when it sees `input.value === "hello"`.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer down link");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer up link");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &next_url)
  })
  .unwrap_or_else(|| panic!("expected navigation to commit to {next_url}"));

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_file_picker_choose_mirrors_internal_file_value_to_js() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
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
            #file { position: absolute; left: 0; top: 0; width: 180px; height: 40px; }
            #go { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; display: block; }
          </style>
        </head>
        <body>
          <input id="file" type="file">
          <a id="go" href="next.html">next</a>
          <script>
            document.getElementById("go").addEventListener("click", function (ev) {
              var f = document.getElementById("file");
              // The worker mirrors file input state into a synthetic `data-fastr-file-value`
              // attribute. Allow navigation only when it matches the chosen file.
              if (f.getAttribute("data-fastr-file-value") !== "C:\\fakepath\\selected.txt") {
                ev.preventDefault();
              }
            });
          </script>
        </body>
      </html>"#,
  );

  // Create the file selection target on disk.
  let file_path: PathBuf = site.dir.path().join("selected.txt");
  std::fs::write(&file_path, b"hello").expect("write selected file");

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-form-control-mirror-file",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 140), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Activate the file input to open the worker-managed file picker overlay.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down file input");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up file input");

  let input_node_id = match support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerOpened { .. })
  }) {
    Some(WorkerToUi::FilePickerOpened { input_node_id, .. }) => input_node_id,
    Some(other) => panic!("expected FilePickerOpened, got {other:?}"),
    None => panic!("timed out waiting for FilePickerOpened"),
  };

  ui_tx
    .send(UiToWorker::FilePickerChoose {
      tab_id,
      input_node_id,
      paths: vec![file_path],
    })
    .expect("FilePickerChoose");

  // Click the link; its click handler allows navigation only if the file selection attribute was
  // mirrored into dom2.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer down link");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer up link");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &next_url)
  })
  .unwrap_or_else(|| panic!("expected navigation to commit to {next_url}"));

  drop(ui_tx);
  join.join().expect("worker join");
}
