use fastrender::dom2::{Document, NodeId};
use fastrender::js::RunLimits;
use fastrender::{BrowserTab, Error, RenderOptions, Result, RunUntilStableOutcome, VmJsBrowserTabExecutor};
use std::time::Duration;

fn attr(dom: &Document, node: NodeId, name: &str) -> Result<Option<String>> {
  dom
    .get_attribute(node, name)
    .map(|value| value.map(|s| s.to_string()))
    .map_err(|err| Error::Other(err.to_string()))
}

fn generous_run_limits() -> RunLimits {
  // `JsExecutionOptions::default()` uses a short 500ms wall-time budget per event loop spin.
  // Observer delivery involves a full render/layout pass (and may be slower in CI), so bump this to
  // keep the test deterministic while still bounding tasks/microtasks.
  RunLimits {
    max_tasks: 10_000,
    max_microtasks: 100_000,
    max_wall_time: Some(Duration::from_secs(5)),
  }
}

#[test]
fn resize_observer_delivered_via_microtask_before_intersection_observer_task() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #t { width: 10px; height: 10px; }
        </style>
      </head>
      <body>
        <div id="t"></div>
        <script>
          const t = document.getElementById("t");
          const log = [];

          new ResizeObserver(() => log.push("resize")).observe(t);
          new IntersectionObserver(() => log.push("intersection")).observe(t);

          Promise.resolve().then(() => log.push("promise"));
          queueMicrotask(() => log.push("qm"));

          setTimeout(() => {
            log.push("timeout");
            document.body.dataset.log = log.join("|");
          }, 0);
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::new())?;

  // Drive the tab until it becomes stable, allowing enough frames for:
  // - parse-time Promise/queueMicrotask jobs
  // - a render/layout pass to queue observer entries
  // - microtask delivery of ResizeObserver
  // - task delivery of IntersectionObserver
  // - timer task delivery for setTimeout(…, 0)
  let outcome = tab.run_until_stable_with_run_limits(generous_run_limits(), 20)?;
  assert!(
    matches!(outcome, RunUntilStableOutcome::Stable { .. }),
    "expected run_until_stable to reach Stable, got {outcome:?}"
  );

  let body = tab
    .dom()
    .body()
    .ok_or_else(|| Error::Other("missing <body> element".to_string()))?;
  let log = attr(tab.dom(), body, "data-log")?;

  assert_eq!(
    log.as_deref(),
    Some("promise|qm|resize|intersection|timeout"),
    "expected microtasks to run before observer delivery, ResizeObserver to run in a microtask, and IntersectionObserver to run as a task"
  );
  Ok(())
}

#[test]
fn intersection_observer_take_records_drains_queue() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #t { width: 10px; height: 10px; }
        </style>
      </head>
      <body>
        <div id="t"></div>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::new())?;

  // Ensure layout/geometry is computed at least once before observing; this lets an implementation
  // that caches geometry queue an initial entry synchronously on `observe()`.
  let _ = tab.render_frame()?;

  // Inject a dynamic script so the observer/takeRecords logic runs after the initial render.
  {
    let dom = tab.dom_mut();
    let body = dom
      .body()
      .ok_or_else(|| Error::Other("missing <body> element".to_string()))?;
    let script = dom.create_element("script", "");
    let text = dom.create_text(
      r#"
        (function () {
          const t = document.getElementById("t");
          const io = new IntersectionObserver(() => {});
          io.observe(t);

          document.body.dataset.take = String(io.takeRecords().length);
          setTimeout(() => {
            document.body.dataset.take2 = String(io.takeRecords().length);
          }, 0);
        })();
      "#,
    );
    dom
      .append_child(script, text)
      .map_err(|err| Error::Other(err.to_string()))?;
    dom
      .append_child(body, script)
      .map_err(|err| Error::Other(err.to_string()))?;
  }

  let outcome = tab.run_until_stable_with_run_limits(generous_run_limits(), 20)?;
  assert!(
    matches!(outcome, RunUntilStableOutcome::Stable { .. }),
    "expected run_until_stable to reach Stable, got {outcome:?}"
  );

  let body = tab
    .dom()
    .body()
    .ok_or_else(|| Error::Other("missing <body> element".to_string()))?;
  let take = attr(tab.dom(), body, "data-take")?
    .ok_or_else(|| Error::Other("missing data-take attribute".to_string()))?
    .parse::<usize>()
    .map_err(|err| Error::Other(format!("invalid data-take attribute: {err}")))?;
  let take2 = attr(tab.dom(), body, "data-take2")?
    .ok_or_else(|| Error::Other("missing data-take2 attribute".to_string()))?
    .parse::<usize>()
    .map_err(|err| Error::Other(format!("invalid data-take2 attribute: {err}")))?;

  assert!(
    take >= 1,
    "expected initial takeRecords() to observe at least one entry (take={take}, take2={take2})"
  );
  assert_eq!(
    take2, 0,
    "expected takeRecords() queue to be drained after the first call"
  );
  Ok(())
}

