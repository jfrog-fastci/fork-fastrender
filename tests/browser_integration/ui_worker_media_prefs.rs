#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::style::media::{ColorScheme, ContrastPreference};
use fastrender::ui::messages::{
  BrowserMediaPreferences, NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

// Keep this generous: these tests do real rendering work and can contend for CPU under CI.
const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn media_fixture_for_query(media_query: &str) -> (support::TempSite, String) {
  let site = support::TempSite::new();
  let html = format!(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {{ margin: 0; padding: 0; width: 100%; height: 100%; }}
      body {{ background: rgb(255, 255, 255); }}
      @media {} {{
        body {{ background: rgb(0, 0, 0); }}
      }}
    </style>
  </head>
  <body></body>
</html>
"#,
    media_query
  );
  let url = site.write("index.html", &html);
  (site, url)
}

fn media_fixture() -> (support::TempSite, String) {
  media_fixture_for_query("(prefers-color-scheme: dark)")
}

fn wait_for_frame(
  rx: &fastrender::ui::WorkerToUiInbox,
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

fn wait_for_navigation_committed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> WorkerToUi {
  support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"))
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
fn ui_worker_media_preferences_update_is_observable_from_match_media_in_event_handlers() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page3_url = site.write("page3.html", r#"<!doctype html><html><body>page3</body></html>"#);
  let page2_url = site.write(
    "page2.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="page3.html">next</a>
          <script>
            document.getElementById("link").addEventListener("click", function (ev) {
              if (matchMedia("(prefers-color-scheme: dark)").matches) {
                ev.preventDefault();
              }
            });
          </script>
        </body>
      </html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="page2.html">next</a>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "ui_worker_media_prefs_match_media",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");

  // Initial load should succeed.
  let _frame0 = wait_for_frame(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Start with light media prefs (default) -> click should navigate to page2.
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

  match wait_for_navigation_committed(&ui_rx, tab_id) {
    WorkerToUi::NavigationCommitted { url, .. } => {
      assert_eq!(url, page2_url, "expected click to navigate to page2");
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
  let _frame1 = wait_for_frame(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Switch to dark preferences.
  ui_tx
    .send(UiToWorker::SetMediaPreferences {
      prefs: BrowserMediaPreferences {
        prefers_color_scheme: ColorScheme::Dark,
        prefers_contrast: ContrastPreference::NoPreference,
        prefers_reduced_motion: false,
      },
    })
    .expect("SetMediaPreferences");

  // Wait for the restyle-triggered repaint so we know the worker processed the preference update.
  let _frame2 = wait_for_frame(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Now that prefers-color-scheme is dark, the click handler should prevent default and navigation
  // to page3 must not occur.
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
    !msgs.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationCommitted { .. }
        | WorkerToUi::NavigationFailed { .. }
        | WorkerToUi::RequestOpenInNewTab { .. }
        | WorkerToUi::RequestOpenInNewTabRequest { .. }
    )),
    "expected matchMedia('(prefers-color-scheme: dark)') click handler to prevent navigation to {page3_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_worker_media_preferences_update_triggers_restyle_for_prefers_contrast() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let (_site, url) = media_fixture_for_query("(prefers-contrast: more)");
  let handle = spawn_ui_worker("ui_worker_media_prefs_contrast").expect("spawn ui worker");
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
    "expected initial render to use no-preference prefers-contrast"
  );

  // Drain any queued follow-up messages (scroll state updates, etc) so the next FrameReady we
  // observe is caused by the preference update.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  ui_tx
    .send(UiToWorker::SetMediaPreferences {
      prefs: BrowserMediaPreferences {
        prefers_color_scheme: ColorScheme::Light,
        prefers_contrast: ContrastPreference::More,
        prefers_reduced_motion: false,
      },
    })
    .expect("SetMediaPreferences");

  let frame1 = wait_for_frame(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame1.pixmap, 1, 1),
    [0, 0, 0, 255],
    "expected restyle to use prefers-contrast: more"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_worker_media_preferences_update_triggers_restyle_for_prefers_reduced_motion() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let (_site, url) = media_fixture_for_query("(prefers-reduced-motion: reduce)");
  let handle = spawn_ui_worker("ui_worker_media_prefs_reduced_motion").expect("spawn ui worker");
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
    "expected initial render to use no-preference prefers-reduced-motion"
  );

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  ui_tx
    .send(UiToWorker::SetMediaPreferences {
      prefs: BrowserMediaPreferences {
        prefers_color_scheme: ColorScheme::Light,
        prefers_contrast: ContrastPreference::NoPreference,
        prefers_reduced_motion: true,
      },
    })
    .expect("SetMediaPreferences");

  let frame1 = wait_for_frame(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame1.pixmap, 1, 1),
    [0, 0, 0, 255],
    "expected restyle to use prefers-reduced-motion: reduce"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_worker_media_preferences_do_not_override_explicit_renderer_env() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  use std::collections::HashMap;
  use fastrender::debug::runtime::RuntimeToggles;

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PREFERS_COLOR_SCHEME".to_string(),
    "light".to_string(),
  )]));
  let factory = support::deterministic_factory_with_runtime_toggles(toggles);

  let (_site, url) = media_fixture();
  let handle = spawn_ui_worker_with_factory("ui_worker_media_prefs_env_precedence", factory)
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame0 = wait_for_frame(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame0.pixmap, 1, 1),
    [255, 255, 255, 255],
    "expected runtime-toggle override to force light prefers-color-scheme"
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
    "expected runtime-toggle override to take precedence over UI defaults"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
