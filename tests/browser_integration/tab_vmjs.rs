use fastrender::dom2::NodeId;
use fastrender::js::{Clock, EventLoop, RunLimits, RunUntilIdleOutcome, VirtualClock};
use fastrender::{BrowserTab, BrowserTabHost, Error, RenderOptions, Result, VmJsBrowserTabExecutor};
use std::sync::Arc;
use std::time::Duration;

use super::support::rgba_at;

fn get_attr(dom: &fastrender::dom2::Document, node: NodeId, name: &str) -> Result<Option<String>> {
  Ok(
    dom
      .get_attribute(node, name)
      .map_err(|e| Error::Other(e.to_string()))?
      .map(|v| v.to_string()),
  )
}

#[test]
fn browser_tab_vmjs_executes_scripts_microtasks_timers_and_rerenders() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
        <script id="s">
          (function () {
            const box = document.getElementById("box");
            const d = document.createElement("div");
            d.setAttribute("id", "added");
            document.body.appendChild(d);

            box.setAttribute("data-order", "script");
            queueMicrotask(function () {
              box.setAttribute(
                "data-order",
                box.getAttribute("data-order") + ",microtask"
              );
            });
            setTimeout(function () {
              box.setAttribute(
                "data-order",
                box.getAttribute("data-order") + ",timer"
              );
              box.setAttribute("class", "b");
            }, 10);
          })();
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab =
    BrowserTab::from_html_with_event_loop(html, options, VmJsBrowserTabExecutor::default(), event_loop)?;

  let frame_a = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);
  assert!(tab.render_if_needed()?.is_none());

  let box_id = tab
    .dom()
    .get_element_by_id("box")
    .ok_or_else(|| Error::Other("expected #box element".to_string()))?;
  let added = tab.dom().get_element_by_id("added");
  assert!(added.is_some(), "expected script-time appendChild mutation");

  // The parser-inserted script should have run a microtask checkpoint after executing, so the
  // `queueMicrotask` callback must run before any timer tasks.
  assert_eq!(get_attr(tab.dom(), box_id, "data-order")?.as_deref(), Some("script,microtask"));

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(get_attr(tab.dom(), box_id, "data-order")?.as_deref(), Some("script,microtask"));
  assert_eq!(get_attr(tab.dom(), box_id, "class")?.as_deref(), Some("a"));
  // Parsing completion queues DOMContentLoaded/load lifecycle tasks. They can change
  // `document.readyState`, which FastRender currently treats as a full DOM invalidation. Drain any
  // resulting render before advancing time so the timer-driven mutation is the only source of a
  // new frame.
  if let Some(frame) = tab.render_if_needed()? {
    assert_eq!(rgba_at(&frame, 32, 32), [255, 0, 0, 255]);
  }
  assert!(tab.render_if_needed()?.is_none());

  // Advance time so the timeout fires.
  clock.advance(Duration::from_millis(10));
  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-order")?.as_deref(),
    Some("script,microtask,timer")
  );

  let frame_b = tab
    .render_if_needed()?
    .expect("expected a new frame after timer-driven mutation");
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);
  assert!(tab.render_if_needed()?.is_none());

  Ok(())
}
