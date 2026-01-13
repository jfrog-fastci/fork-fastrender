#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use url::Url;

// Navigation + rendering on CI can take a few seconds when tests run in parallel; keep this
// generous to avoid flakes.
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn click_image_input_submits_form() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  // Use a tiny SVG data URL so the control has a valid `src` without relying on external fixtures.
  let image_src = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='1'%20height='1'%3E%3C/svg%3E";

  let page_url = site.write(
    "page.html",
    &format!(
      r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {{ margin: 0; padding: 0; }}
      #img {{ position: absolute; left: 0; top: 0; width: 120px; height: 40px; }}
    </style>
  </head>
  <body>
    <form action="result.html">
      <input id="img" type="image" name="img" src="{image_src}">
    </form>
  </body>
</html>
"#
    ),
  );
  let _result_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>ok</body></html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-image-input-submit").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 160), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for the initial frame so hit testing works.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));

  // Drain any queued messages (navigation committed, loading state, etc) so assertions are scoped
  // to the image input click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Click the `<input type=image>`.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  let expected_base = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");

  let nav_started = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    let WorkerToUi::NavigationStarted { url, .. } = msg else {
      return false;
    };
    let Ok(parsed) = Url::parse(url) else {
      return false;
    };
    parsed.scheme() == expected_base.scheme() && parsed.path() == expected_base.path()
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationStarted(result.html); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  let submitted_url = match nav_started {
    WorkerToUi::NavigationStarted { url, .. } => url,
    other => panic!("expected NavigationStarted, got {other:?}"),
  };
  let submitted = Url::parse(&submitted_url).expect("parse submitted URL");

  let mut has_submitter_param = false;
  if let Some(query) = submitted.query() {
    // We only care that at least one of the submitter keys exists; don't depend on ordering or
    // coordinate behaviour (`img` for MVP, or `img.x`/`img.y` for full semantics).
    for (key, _value) in url::form_urlencoded::parse(query.as_bytes()) {
      if key == "img" || key == "img.x" || key == "img.y" {
        has_submitter_param = true;
        break;
      }
    }
  }
  assert!(
    has_submitter_param,
    "expected submitted URL to include submitter parameter for <input type=image name=img>; got {submitted_url}"
  );

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &submitted_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted({submitted_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}
