#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::style::media::{ColorScheme, ContrastPreference};
use fastrender::ui::messages::{
  BrowserMediaPreferences, NavigationReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

// Keep this generous: these tests do real rendering work and can contend for CPU under CI.
const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn media_fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; width: 100%; height: 100%; }
      body { background: rgb(255, 255, 255); }
      @media (prefers-color-scheme: dark) {
        body { background: rgb(0, 0, 0); }
      }
    </style>
  </head>
  <body></body>
</html>
"#,
  );
  (site, url)
}

fn wait_for_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. } => true,
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } => {
      assert_eq!(got, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed {
      tab_id: got,
      url,
      error,
      ..
    } => {
      assert_eq!(got, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn ui_worker_media_preferences_update_triggers_restyle_for_prefers_color_scheme() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let (_site, url) = media_fixture();
  let handle = spawn_ui_worker("ui_worker_media_prefs_color_scheme").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  let frame0 = wait_for_frame(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame0.pixmap, 1, 1),
    [255, 255, 255, 255],
    "expected initial render to use light prefers-color-scheme"
  );

  // Drain any queued follow-up messages (scroll state updates, etc) so the next FrameReady we
  // observe is caused by the preference update.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  ui_tx
    .send(UiToWorker::SetMediaPreferences {
      prefs: BrowserMediaPreferences {
        prefers_color_scheme: ColorScheme::Dark,
        prefers_contrast: ContrastPreference::NoPreference,
        prefers_reduced_motion: false,
      },
    })
    .expect("SetMediaPreferences");

  let frame1 = wait_for_frame(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame1.pixmap, 1, 1),
    [0, 0, 0, 255],
    "expected restyle to use dark prefers-color-scheme"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_worker_media_preferences_do_not_override_explicit_renderer_env() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  use fastrender::debug::runtime::{with_runtime_toggles, RuntimeToggles};
  use std::collections::HashMap;
  use std::sync::Arc;

  with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_PREFERS_COLOR_SCHEME".to_string(),
      "light".to_string(),
    )]))),
    || {
      let (_site, url) = media_fixture();
      let handle =
        spawn_ui_worker("ui_worker_media_prefs_env_precedence").expect("spawn ui worker");
      let (ui_tx, ui_rx, join) = handle.split();

      let tab_id = TabId::new();
      ui_tx
        .send(support::create_tab_msg(tab_id, None))
        .expect("CreateTab");
      ui_tx
        .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
        .expect("ViewportChanged");
      ui_tx
        .send(support::navigate_msg(
          tab_id,
          url,
          NavigationReason::TypedUrl,
        ))
        .expect("Navigate");

      let frame0 = wait_for_frame(&ui_rx, tab_id);
      assert_eq!(
        support::rgba_at(&frame0.pixmap, 1, 1),
        [255, 255, 255, 255],
        "expected env override to force light prefers-color-scheme"
      );

      let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

      ui_tx
        .send(UiToWorker::SetMediaPreferences {
          prefs: BrowserMediaPreferences {
            prefers_color_scheme: ColorScheme::Dark,
            prefers_contrast: ContrastPreference::NoPreference,
            prefers_reduced_motion: false,
          },
        })
        .expect("SetMediaPreferences");

      let frame1 = wait_for_frame(&ui_rx, tab_id);
      assert_eq!(
        support::rgba_at(&frame1.pixmap, 1, 1),
        [255, 255, 255, 255],
        "expected renderer env override to take precedence over UI defaults"
      );

      drop(ui_tx);
      join.join().expect("join ui worker thread");
    },
  );
}
