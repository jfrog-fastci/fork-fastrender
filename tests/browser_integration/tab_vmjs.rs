use fastrender::dom2::NodeId;
use fastrender::js::{Clock, EventLoop, RunLimits, RunUntilIdleOutcome, VirtualClock};
use fastrender::resource::{
  origin_from_url, FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource,
  ResourceFetcher,
};
use fastrender::{
  BrowserTab, BrowserTabHost, Error, RenderOptions, Result, RunUntilStableOutcome,
  VmJsBrowserTabExecutor,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedFetchRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
  client_origin: Option<String>,
  credentials_mode: FetchCredentialsMode,
}

#[derive(Default)]
struct StubFetcher {
  responses: Mutex<HashMap<String, FetchedResource>>,
  recorded: Mutex<Vec<RecordedFetchRequest>>,
}

impl StubFetcher {
  fn with_response(mut self, url: &str, resource: FetchedResource) -> Self {
    self
      .responses
      .get_mut()
      .unwrap()
      .insert(url.to_string(), resource);
    self
  }

  fn take_recorded(&self) -> Vec<RecordedFetchRequest> {
    std::mem::take(&mut *self.recorded.lock().unwrap())
  }
}

impl ResourceFetcher for StubFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    Ok(
      self
        .responses
        .lock()
        .unwrap()
        .get(url)
        .cloned()
        .unwrap_or_else(|| FetchedResource::new(Vec::new(), None)),
    )
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
    self.recorded.lock().unwrap().push(RecordedFetchRequest {
      url: req.url.to_string(),
      destination: req.destination,
      referrer_url: req.referrer_url.map(str::to_string),
      client_origin: req.client_origin.map(ToString::to_string),
      credentials_mode: req.credentials_mode,
    });
    self.fetch(req.url)
  }
}

#[test]
fn browser_tab_vmjs_executes_scripts_microtasks_timers_and_rerenders() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    VmJsBrowserTabExecutor::default(),
    event_loop,
  )?;

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
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-order")?.as_deref(),
    Some("script,microtask")
  );

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-order")?.as_deref(),
    Some("script,microtask")
  );
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

#[test]
fn browser_tab_vmjs_document_current_script_is_set_and_cleared_before_microtasks() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <body id="b">
        <script id="s">
          (function () {
            const b = document.getElementById("b");
            const cs = document.currentScript;
            b.setAttribute(
              "data-sync-current-script",
              cs === null ? "null" : cs.getAttribute("id")
            );
            queueMicrotask(function () {
              b.setAttribute(
                "data-microtask-current-script-is-null",
                String(document.currentScript === null)
              );
            });
          })();
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::default();
  let tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::default())?;

  let body = tab
    .dom()
    .get_element_by_id("b")
    .ok_or_else(|| Error::Other("expected #b element".to_string()))?;
  assert_eq!(
    get_attr(tab.dom(), body, "data-sync-current-script")?.as_deref(),
    Some("s")
  );
  assert_eq!(
    get_attr(tab.dom(), body, "data-microtask-current-script-is-null")?.as_deref(),
    Some("true")
  );

  Ok(())
}

#[test]
fn browser_tab_vmjs_request_animation_frame_runs_and_triggers_rerender() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
        <div id="box" class="a" data-order="script"></div>
        <script>
          (function () {
            const box = document.getElementById("box");
            requestAnimationFrame(function (ts) {
              box.setAttribute(
                "data-order",
                box.getAttribute("data-order") + ",raf"
              );
              box.setAttribute("data-ts", String(ts));
              queueMicrotask(function () {
                box.setAttribute(
                  "data-order",
                  box.getAttribute("data-order") + ",microtask"
                );
              });
              box.setAttribute("class", "b");
            });
          })();
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    VmJsBrowserTabExecutor::default(),
    event_loop,
  )?;

  let frame_a = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);

  let box_id = tab
    .dom()
    .get_element_by_id("box")
    .ok_or_else(|| Error::Other("expected #box element".to_string()))?;
  assert!(get_attr(tab.dom(), box_id, "data-ts")?.is_none());

  // `run_event_loop_until_idle` does not run requestAnimationFrame callbacks.
  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(get_attr(tab.dom(), box_id, "data-ts")?.is_none());

  if let Some(frame) = tab.render_if_needed()? {
    assert_eq!(rgba_at(&frame, 32, 32), [255, 0, 0, 255]);
  }
  assert!(tab.render_if_needed()?.is_none());

  // The timestamp passed to requestAnimationFrame is computed when the callback runs.
  clock.set_now(Duration::from_millis(123));

  assert!(matches!(
    tab.run_until_stable(10)?,
    RunUntilStableOutcome::Stable { .. }
  ));

  assert_eq!(
    get_attr(tab.dom(), box_id, "data-order")?.as_deref(),
    Some("script,raf,microtask")
  );
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-ts")?.as_deref(),
    Some("123")
  );

  let frame_b = tab
    .render_if_needed()?
    .expect("expected a new frame after requestAnimationFrame mutation");
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);
  assert!(tab.render_if_needed()?.is_none());

  Ok(())
}

#[test]
fn browser_tab_vmjs_tick_frame_runs_request_animation_frame_and_rerenders() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
        <script>
          (function () {
            const box = document.getElementById("box");
            box.setAttribute("data-order", "script");
            requestAnimationFrame(function (ts) {
              box.setAttribute(
                "data-order",
                box.getAttribute("data-order") + ",raf"
              );
              box.setAttribute("class", "b");
              box.setAttribute("data-ts", String(ts));
              queueMicrotask(function () {
                box.setAttribute(
                  "data-order",
                  box.getAttribute("data-order") + ",microtask"
                );
              });
            });
          })();
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    VmJsBrowserTabExecutor::default(),
    event_loop,
  )?;

  // 1. Render initial frame.
  let frame_a = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);

  let box_id = tab
    .dom()
    .get_element_by_id("box")
    .ok_or_else(|| Error::Other("expected #box element".to_string()))?;
  assert!(get_attr(tab.dom(), box_id, "data-ts")?.is_none());

  // 2. `run_event_loop_until_idle` does not run requestAnimationFrame callbacks.
  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(get_attr(tab.dom(), box_id, "data-ts")?.is_none());

  // Parsing completion queues DOMContentLoaded/load lifecycle tasks. They can change
  // `document.readyState`, which FastRender currently treats as a full DOM invalidation. Drain any
  // resulting render so the requestAnimationFrame-driven mutation is the only source of a new
  // frame.
  if let Some(frame) = tab.render_if_needed()? {
    assert_eq!(rgba_at(&frame, 32, 32), [255, 0, 0, 255]);
  }
  assert!(tab.render_if_needed()?.is_none());

  // 3. The timestamp passed to requestAnimationFrame is computed when the callback runs.
  clock.set_now(Duration::from_millis(123));

  // 4. Tick the frame: this should run rAF callbacks and re-render.
  let frame_b = tab
    .tick_frame()?
    .expect("expected tick_frame to render after requestAnimationFrame mutation");
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);

  // 5. requestAnimationFrame callback runs before its queued microtasks.
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-order")?.as_deref(),
    Some("script,raf,microtask")
  );
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-ts")?.as_deref(),
    Some("123")
  );

  // 6. No further work remains.
  assert!(tab.tick_frame()?.is_none());
  assert!(tab.render_if_needed()?.is_none());

  Ok(())
}

#[test]
fn browser_tab_vmjs_fetch_resolves_and_triggers_rerender() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let document_url = "https://client.example/page.html";
  let resource_url = "https://client.example/hello.txt";

  let fetcher = Arc::new(StubFetcher::default().with_response(
    resource_url,
    FetchedResource::new(b"hello".to_vec(), Some("text/plain".to_string())),
  ));

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
        <script>
          // `BrowserTab::from_html_with_document_url_and_fetcher` runs the event loop between parser
          // yield points (to allow async/defer loads to interleave with parsing). Defer `fetch()` to
          // a timer so the networking task runs only after the initial render.
          setTimeout(function () {
            fetch("hello.txt")
              .then((r) => r.text())
              .then((t) => {
                const box = document.getElementById("box");
                box.setAttribute("data-fetch", t);
                box.setAttribute("class", "b");
              })
              .catch((e) => {
                const box = document.getElementById("box");
                box.setAttribute("data-fetch", "err:" + String(e && e.name));
              });
          }, 0);
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html_with_document_url_and_fetcher(
    html,
    document_url,
    options,
    VmJsBrowserTabExecutor::default(),
    fetcher.clone(),
  )?;

  let box_id = tab
    .dom()
    .get_element_by_id("box")
    .ok_or_else(|| Error::Other("expected #box element".to_string()))?;
  assert!(get_attr(tab.dom(), box_id, "data-fetch")?.is_none());

  let frame_a = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);

  assert!(matches!(
    tab.run_until_stable(20)?,
    RunUntilStableOutcome::Stable { .. }
  ));

  assert_eq!(
    get_attr(tab.dom(), box_id, "data-fetch")?.as_deref(),
    Some("hello")
  );
  assert_eq!(get_attr(tab.dom(), box_id, "class")?.as_deref(), Some("b"));

  let recorded = fetcher.take_recorded();
  assert_eq!(recorded.len(), 1, "expected 1 fetch request: {recorded:?}");
  let expected_origin = origin_from_url(document_url)
    .expect("origin_from_url")
    .to_string();
  assert_eq!(recorded[0].url, resource_url);
  assert_eq!(recorded[0].destination, FetchDestination::Fetch);
  assert_eq!(recorded[0].referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    recorded[0].client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
  assert_eq!(
    recorded[0].credentials_mode,
    FetchCredentialsMode::SameOrigin
  );

  let frame_b = tab
    .render_if_needed()?
    .expect("expected a new frame after fetch-driven mutation");
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);
  assert!(tab.render_if_needed()?.is_none());

  Ok(())
}

#[test]
fn browser_tab_vmjs_fetch_rejects_when_signal_is_pre_aborted() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let document_url = "https://client.example/page.html";

  let fetcher = Arc::new(StubFetcher::default());

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <div id="box"></div>
        <script>
          (function () {
            const box = document.getElementById("box");
            const c = new AbortController();
            c.abort();
            fetch("hello.txt", { signal: c.signal })
              .then(() => { box.setAttribute("data-fetch", "unexpected"); })
              .catch((e) => { box.setAttribute("data-fetch", e && e.name); });
          })();
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html_with_document_url_and_fetcher(
    html,
    document_url,
    options,
    VmJsBrowserTabExecutor::default(),
    fetcher.clone(),
  )?;

  let box_id = tab
    .dom()
    .get_element_by_id("box")
    .ok_or_else(|| Error::Other("expected #box element".to_string()))?;

  // The promise rejection happens synchronously, and Promise reactions should be run via the
  // microtask checkpoint performed after the parser-inserted script executes.
  assert_eq!(
    get_attr(tab.dom(), box_id, "data-fetch")?.as_deref(),
    Some("AbortError")
  );
  assert!(
    fetcher.take_recorded().is_empty(),
    "expected aborted fetch to never call ResourceFetcher"
  );

  let frame = tab.render_frame()?;
  assert_eq!(rgba_at(&frame, 32, 32), [255, 0, 0, 255]);
  Ok(())
}

#[test]
fn browser_tab_vmjs_fetch_can_be_aborted_after_scheduling_before_execution() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let document_url = "https://client.example/page.html";

  let fetcher = Arc::new(StubFetcher::default());

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
        <script>
          (function () {
            const box = document.getElementById("box");
            const c = new AbortController();
            // Defer the fetch so the initial render happens before the abort-driven rejection.
            setTimeout(function () {
              fetch("hello.txt", { signal: c.signal })
                .then(() => { box.setAttribute("data-fetch", "unexpected"); })
                .catch((e) => {
                  box.setAttribute("data-fetch", e && e.name);
                  box.setAttribute("class", "b");
                });
              // Abort after scheduling `fetch()` but before the networking task begins.
              c.abort();
            }, 0);
          })();
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html_with_document_url_and_fetcher(
    html,
    document_url,
    options,
    VmJsBrowserTabExecutor::default(),
    fetcher.clone(),
  )?;

  let box_id = tab
    .dom()
    .get_element_by_id("box")
    .ok_or_else(|| Error::Other("expected #box element".to_string()))?;

  let frame_a = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);

  assert!(matches!(
    tab.run_until_stable(20)?,
    RunUntilStableOutcome::Stable { .. }
  ));

  assert_eq!(
    get_attr(tab.dom(), box_id, "data-fetch")?.as_deref(),
    Some("AbortError")
  );
  assert_eq!(get_attr(tab.dom(), box_id, "class")?.as_deref(), Some("b"));
  assert!(
    fetcher.take_recorded().is_empty(),
    "expected aborted fetch to never call ResourceFetcher"
  );

  let frame_b = tab
    .render_if_needed()?
    .expect("expected a new frame after abort-driven fetch rejection");
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);
  assert!(tab.render_if_needed()?.is_none());
  Ok(())
}
