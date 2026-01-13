use fastrender::api::{ConsoleMessageLevel, DiagnosticsLevel};
use fastrender::js::{Clock, EventLoop, JsExecutionOptions, ParseBudget, RunLimits, RunUntilIdleOutcome, VirtualClock};
use fastrender::{BrowserTab, RenderOptions, Result, VmJsBrowserTabExecutor};
use std::sync::Arc;
use std::time::Duration;

fn console_messages(tab: &BrowserTab, level: ConsoleMessageLevel) -> Vec<String> {
  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  diagnostics
    .console_messages
    .into_iter()
    .filter(|m| m.level == level)
    .map(|m| m.message)
    .collect()
}

fn console_logs(tab: &BrowserTab) -> Vec<String> {
  console_messages(tab, ConsoleMessageLevel::Log)
}

fn js_exception_messages(tab: &BrowserTab) -> Vec<String> {
  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  diagnostics.js_exceptions.into_iter().map(|e| e.message).collect()
}

struct Harness {
  tab: BrowserTab,
  clock: Arc<VirtualClock>,
  document_url: String,
  options: RenderOptions,
}

impl Harness {
  fn new(document_url: &str, js_execution_options: JsExecutionOptions) -> Result<Self> {
    let options = RenderOptions::new()
      .with_viewport(32, 32)
      .with_diagnostics_level(DiagnosticsLevel::Basic);

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let event_loop = EventLoop::<fastrender::BrowserTabHost>::with_clock(clock_for_loop);

    // Start with an empty document and drive the real navigation pipeline for each test.
    let tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "",
      options.clone(),
      VmJsBrowserTabExecutor::default(),
      event_loop,
      js_execution_options,
    )?;

    Ok(Self {
      tab,
      clock,
      document_url: document_url.to_string(),
      options,
    })
  }

  fn register_script_source(&mut self, url: &str, source: &str) {
    self.tab.register_script_source(url.to_string(), source.to_string());
  }

  fn register_html_source(&mut self, html: &str) {
    self
      .tab
      .register_html_source(self.document_url.clone(), html.to_string());
  }

  fn navigate(&mut self) -> Result<()> {
    self.tab.navigate_to_url(&self.document_url, self.options.clone())
  }

  fn navigate_to(&mut self, url: &str) -> Result<()> {
    self.tab.navigate_to_url(url, self.options.clone())
  }

  fn run_until_idle(&mut self) -> Result<()> {
    let outcome = self.tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);
    Ok(())
  }

  fn advance_clock(&self, delta: Duration) {
    self.clock.advance(delta);
  }
}

// --- P0: Scripts execute (basic page JS works) ---

#[test]
fn p0_inline_classic_scripts_execute_during_parse_and_current_script_is_set() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p0_inline.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <div id="before"></div>
      <script id="s">
        console.log("before:" + (document.getElementById("before") !== null));
        console.log("after:" + (document.getElementById("after") !== null));
        console.log("cs:" + document.currentScript.getAttribute("id"));
      </script>
      <div id="after"></div>
      <script>console.log("done");</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "before:true".to_string(),
      "after:false".to_string(),
      "cs:s".to_string(),
      "done".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p0_inline_script_errors_do_not_break_parsing() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p0_inline_error.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        console.log("before-throw");
        throw new Error("boom");
      </script>
      <script>console.log("after-throw");</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["before-throw".to_string(), "after-throw".to_string()]
  );
  let exc = js_exception_messages(&h.tab).join("\n");
  assert!(
    exc.contains("boom"),
    "expected JS exception mentioning boom, got: {exc:?}"
  );
  Ok(())
}

#[test]
fn p0_external_classic_scripts_are_parser_blocking_and_resolve_relative_src_against_document_url(
) -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/dir/page.html", js_options)?;

  // Only register the fully resolved absolute URL; if base URL resolution regresses, the script
  // will fail to load and this test will fail deterministically.
  h.register_script_source(
    "https://example.invalid/dir/a.js",
    r#"console.log("ext:" + (document.getElementById("after") !== null));"#,
  );

  h.register_html_source(
    r#"<!doctype html><body>
      <div id="before"></div>
      <script src="a.js"></script>
      <div id="after"></div>
      <script>console.log("after:" + (document.getElementById("after") !== null));</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["ext:false".to_string(), "after:true".to_string()]
  );
  Ok(())
}

#[test]
fn p0_external_classic_script_parse_errors_do_not_break_parsing() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p0_external_error.html", js_options)?;

  h.register_script_source("https://example.invalid/bad.js", "var = ;");
  h.register_html_source(
    r#"<!doctype html><body>
      <script src="https://example.invalid/bad.js"></script>
      <script>console.log("after");</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["after".to_string()]);
  assert!(
    !js_exception_messages(&h.tab).is_empty(),
    "expected syntax error to be reported to diagnostics"
  );
  Ok(())
}

#[test]
fn p0_event_loop_microtasks_and_timers_run_in_expected_order() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p0_event_loop.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        console.log("script-start");
        Promise.resolve().then(() => console.log("promise"));
        queueMicrotask(() => console.log("microtask"));
        setTimeout(() => console.log("timeout"), 0);
        setTimeout(() => {
          console.log("timeout2");
          Promise.resolve().then(() => console.log("promise2"));
        }, 0);
        setTimeout(() => console.log("timeout3"), 0);
        console.log("script-end");
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "script-start".to_string(),
      "script-end".to_string(),
      "promise".to_string(),
      "microtask".to_string(),
      "timeout".to_string(),
      "timeout2".to_string(),
      "promise2".to_string(),
      "timeout3".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p0_timers_follow_the_virtual_clock() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p0_timers_virtual_clock.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        setTimeout(() => console.log("t10"), 10);
        console.log("scheduled");
      </script>
    </body>"#,
  );

  h.navigate()?;
  // At t=0, the 10ms timer is not yet due.
  h.run_until_idle()?;
  assert_eq!(console_logs(&h.tab), vec!["scheduled".to_string()]);

  // Advance time deterministically and run again to service the timer.
  h.advance_clock(Duration::from_millis(10));
  h.run_until_idle()?;
  assert_eq!(
    console_logs(&h.tab),
    vec!["scheduled".to_string(), "t10".to_string()]
  );
  Ok(())
}

#[test]
fn p0_domcontentloaded_and_ready_state_transitions_are_correct_and_deferred_scripts_gate_domcontentloaded(
) -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p0_dcl.html", js_options)?;

  h.register_script_source("https://example.invalid/d.js", r#"console.log("defer");"#);
  h.register_html_source(
    r#"<!doctype html><html><head>
      <script>
        console.log("init:" + document.readyState);
        document.addEventListener("readystatechange", () => console.log("rs:" + document.readyState));
        document.addEventListener("DOMContentLoaded", () => console.log("dcl:" + document.readyState));
        window.addEventListener("load", () => console.log("load:" + document.readyState));
      </script>
      <script defer src="https://example.invalid/d.js"></script>
    </head>
    <body>
      <script>console.log("inline-end:" + document.readyState);</script>
    </body></html>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "init:loading".to_string(),
      "inline-end:loading".to_string(),
      "rs:interactive".to_string(),
      "defer".to_string(),
      "dcl:interactive".to_string(),
      "rs:complete".to_string(),
      "load:complete".to_string(),
    ]
  );
  Ok(())
}

// --- P1: Script ordering (complex pages work) ---

#[test]
fn p1_defer_scripts_execute_after_parsing_in_document_order_before_domcontentloaded() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p1_defer_order.html", js_options)?;

  h.register_script_source("https://example.invalid/a.js", r#"console.log("a");"#);
  h.register_script_source("https://example.invalid/b.js", r#"console.log("b");"#);
  h.register_html_source(
    r#"<!doctype html><body>
      <script defer src="https://example.invalid/a.js"></script>
      <script defer src="https://example.invalid/b.js"></script>
      <script>
        console.log("inline");
        document.addEventListener("DOMContentLoaded", () => console.log("dcl"));
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "inline".to_string(),
      "a".to_string(),
      "b".to_string(),
      "dcl".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p1_base_href_does_not_affect_relative_defer_script_src_resolution_after_discovery() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/dir/page.html", js_options)?;

  h.register_script_source("https://example.invalid/dir/a.js", r#"console.log("resolved:dir");"#);
  h.register_script_source(
    "https://example.invalid/base/a.js",
    r#"console.log("resolved:base");"#,
  );
  h.register_html_source(
    r#"<!doctype html><html><head>
      <script defer src="a.js"></script>
      <base href="https://example.invalid/base/">
      <script>
        console.log("inline");
        document.addEventListener("DOMContentLoaded", () => console.log("dcl"));
      </script>
    </head><body></body></html>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  let logs = console_logs(&h.tab);
  assert!(
    logs.contains(&"resolved:dir".to_string()),
    "expected defer script to resolve against the base URL in effect at discovery time, got: {logs:?}"
  );
  assert!(
    !logs.contains(&"resolved:base".to_string()),
    "expected defer script to NOT resolve against a later <base href>, got: {logs:?}"
  );
  assert_eq!(
    logs,
    vec![
      "inline".to_string(),
      "resolved:dir".to_string(),
      "dcl".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p1_async_classic_scripts_do_not_block_parsing_when_not_preloaded() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p1_async_not_fast.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script async src="https://example.invalid/a.js"></script>
      <script>console.log("inline");</script>
    </body>"#,
  );

  // Navigate first so the parser discovers the async script *before* its source is registered.
  // This ensures the parser continues parsing without yielding to run the async script early.
  h.navigate()?;

  h.register_script_source("https://example.invalid/a.js", r#"console.log("async");"#);
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["inline".to_string(), "async".to_string()]
  );
  Ok(())
}

#[test]
fn p1_async_classic_scripts_can_interleave_ahead_of_later_parser_scripts_when_fast() -> Result<()> {
  // Use a tiny parse budget so streaming parsing yields back to the event loop after encountering
  // the async script, giving its execution task a chance to run before the later inline script is
  // parsed.
  let js_options = JsExecutionOptions {
    dom_parse_budget: ParseBudget::new(1),
    ..JsExecutionOptions::default()
  };
  let mut h = Harness::new("https://example.invalid/p1_async_fast.html", js_options)?;

  // Pre-register the script source so it loads immediately and can execute on the first parse
  // yield.
  h.register_script_source("https://example.invalid/a.js", r#"console.log("async");"#);
  h.register_html_source(
    r#"<!doctype html><body>
      <script async src="https://example.invalid/a.js"></script>
      <script>console.log("inline");</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["async".to_string(), "inline".to_string()]
  );
  Ok(())
}

#[test]
fn p1_dynamic_external_scripts_are_async_by_default() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p1_dynamic_async_default.html", js_options)?;

  let external_url = "https://example.invalid/dyn.js";
  h.register_script_source(external_url, r#"console.log("ext");"#);
  h.register_html_source(&format!(
    r#"<!doctype html><body>
      <script>
        const s = document.createElement("script");
        s.src = "{external_url}";
        document.body.appendChild(s);
        console.log("after");
      </script>
    </body>"#
  ));

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["after".to_string(), "ext".to_string()]
  );
  Ok(())
}

#[test]
fn p1_dynamic_external_scripts_with_async_false_execute_in_insertion_order() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p1_dynamic_ordered.html", js_options)?;

  h.register_script_source("https://example.invalid/a.js", r#"console.log("A");"#);
  h.register_script_source("https://example.invalid/b.js", r#"console.log("B");"#);
  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        const a = document.createElement("script");
        a.src = "https://example.invalid/a.js";
        a.async = false;
        document.body.appendChild(a);

        const b = document.createElement("script");
        b.src = "https://example.invalid/b.js";
        b.async = false;
        document.body.appendChild(b);

        console.log("after");
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["after".to_string(), "A".to_string(), "B".to_string()]
  );
  Ok(())
}

#[test]
fn p1_document_write_inserts_into_the_token_stream_during_parsing_and_supports_nested_writes() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p1_document_write.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        document.write('<div id="w1"></div>');
        document.write('<script>document.write("<div id=\\"w2\\"></div>");</' + 'script>');
      </script>
      <script>
        console.log("w1:" + (document.getElementById("w1") !== null));
        console.log("w2:" + (document.getElementById("w2") !== null));
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["w1:true".to_string(), "w2:true".to_string()]
  );
  Ok(())
}

#[test]
fn p1_document_write_is_noop_after_parsing_completes() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p1_document_write_late.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        document.addEventListener("DOMContentLoaded", () => {
          document.write('<div id="late"></div>');
          console.log("late:" + (document.getElementById("late") !== null));
        });
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["late:false".to_string()]);
  Ok(())
}

// --- P2: Modules (modern JS works) ---

#[test]
fn p2_module_scripts_are_deferred_by_default_and_delay_domcontentloaded() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_module_defer.html", js_options)?;

  h.register_script_source("https://example.invalid/mod.js", r#"console.log("module");"#);
  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        document.addEventListener("DOMContentLoaded", () => console.log("dcl"));
      </script>
      <script type="module" src="https://example.invalid/mod.js"></script>
      <script>console.log("inline");</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["inline".to_string(), "module".to_string(), "dcl".to_string()]
  );
  Ok(())
}

#[test]
fn p2_nomodule_classic_scripts_are_suppressed_when_modules_are_supported() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_nomodule_supported.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script nomodule>console.log('nomodule');</script>
      <script>console.log('classic');</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["classic".to_string()]);
  Ok(())
}

#[test]
fn p2_nomodule_classic_scripts_execute_when_modules_are_not_supported() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = false;
  let mut h = Harness::new("https://example.invalid/p2_nomodule_unsupported.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script nomodule>console.log('nomodule');</script>
      <script>console.log('classic');</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["nomodule".to_string(), "classic".to_string()]
  );
  Ok(())
}

#[test]
fn p2_module_scripts_observe_document_current_script_null() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_module_current_script.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script type="module">console.log('cs:' + (document.currentScript === null));</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["cs:true".to_string()]);
  Ok(())
}

#[test]
fn p2_async_module_scripts_execute_asap_before_later_parser_scripts() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  js_options.dom_parse_budget = ParseBudget::new(1);
  let mut h = Harness::new("https://example.invalid/p2_async_module.html", js_options)?;

  h.register_script_source(
    "https://example.invalid/async_mod.js",
    r#"console.log("async-module");"#,
  );

  // Make the HTML large enough to require multiple streaming pump calls so the parse budget forces
  // a yield to the event loop before we reach the later inline script.
  let filler = "x".repeat(9 * 1024);
  h.register_html_source(&format!(
    r#"<!doctype html><html><head>
        <script type="module" async src="https://example.invalid/async_mod.js"></script>
      </head><body>
        <!-- {filler} -->
        <script>console.log("inline");</script>
      </body></html>"#,
  ));

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["async-module".to_string(), "inline".to_string()]
  );
  Ok(())
}

#[test]
fn p2_dynamic_module_scripts_with_async_false_execute_in_insertion_order() -> Result<()> {
  let js_options = JsExecutionOptions {
    supports_module_scripts: true,
    ..Default::default()
  };
  let mut h = Harness::new("https://example.invalid/p2_dynamic_module_ordered.html", js_options)?;

  h.register_script_source("https://example.invalid/a.js", "console.log('A');");
  h.register_script_source("https://example.invalid/b.js", "console.log('B');");
  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        const a = document.createElement('script');
        a.type = 'module';
        a.src = 'https://example.invalid/a.js';
        a.async = false;
        document.body.appendChild(a);

        const b = document.createElement('script');
        b.type = 'module';
        b.src = 'https://example.invalid/b.js';
        b.async = false;
        document.body.appendChild(b);

        console.log('after');
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["after".to_string(), "A".to_string(), "B".to_string()]
  );
  Ok(())
}

#[test]
fn p2_module_top_level_await_works_when_it_settles_via_microtasks() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_tla.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script type="module">
        await Promise.resolve();
        console.log("tla");
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["tla".to_string()]);
  Ok(())
}

#[test]
fn p2_static_import_graph_executes_once_and_is_cached_across_multiple_module_scripts() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_module_cache.html", js_options)?;

  h.register_script_source(
    "https://example.invalid/dep.js",
    r#"console.log("dep"); export const x = 1;"#,
  );
  h.register_script_source(
    "https://example.invalid/a.js",
    r#"import { x } from "./dep.js"; console.log("a:" + x);"#,
  );
  h.register_script_source(
    "https://example.invalid/b.js",
    r#"import { x } from "./dep.js"; console.log("b:" + x);"#,
  );
  h.register_html_source(
    r#"<!doctype html><body>
      <script type="module" src="https://example.invalid/a.js"></script>
      <script type="module" src="https://example.invalid/b.js"></script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["dep".to_string(), "a:1".to_string(), "b:1".to_string()]
  );
  Ok(())
}

#[test]
fn p2_dynamic_import_works_from_classic_and_module_scripts_and_honors_import_maps() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_dynamic_import.html", js_options)?;

  let mapped = "data:text/javascript,export%20default%20123%3B";
  h.register_html_source(&format!(
    r#"<!doctype html><body>
      <script type="importmap">{{"imports":{{"foo":"{mapped}"}}}}</script>
      <script>
        import("foo").then(m => console.log("classic:" + m.default));
      </script>
      <script type="module">
        import("foo").then(m => console.log("module:" + m.default));
      </script>
    </body>"#
  ));

  h.navigate()?;
  h.run_until_idle()?;

  // Ordering between the two dynamic imports is not spec-fixed; both must resolve correctly.
  let mut logs = console_logs(&h.tab);
  logs.sort();
  assert_eq!(
    logs,
    vec!["classic:123".to_string(), "module:123".to_string()]
  );
  Ok(())
}

#[test]
fn p2_import_meta_resolve_resolves_with_import_maps_and_base_url() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/dir/page.html", js_options)?;

  h.register_script_source(
    "https://example.invalid/dir/entry.js",
    r#"
      console.log("url:" + import.meta.url);
      console.log("foo:" + import.meta.resolve("foo"));
      console.log("rel:" + import.meta.resolve("./rel.js"));
    "#,
  );
  h.register_html_source(
    r#"<!doctype html><body>
      <script type="importmap">{"imports":{"foo":"https://example.invalid/mapped/foo.js"}}</script>
      <script type="module" src="https://example.invalid/dir/entry.js"></script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "url:https://example.invalid/dir/entry.js".to_string(),
      "foo:https://example.invalid/mapped/foo.js".to_string(),
      "rel:https://example.invalid/dir/rel.js".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p2_import_maps_support_scoped_mappings() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_importmap_scopes.html", js_options)?;

  let global = "data:text/javascript,export%20default%20%22global%22%3B";
  let scoped = "data:text/javascript,export%20default%20%22scoped%22%3B";
  let importmap = format!(
    r#"{{"imports":{{"foo":"{global}"}},"scopes":{{"https://example.invalid/scope/":{{"foo":"{scoped}"}}}}}}"#
  );

  h.register_script_source(
    "https://example.invalid/scope/entry.js",
    r#"import x from "foo"; console.log(x);"#,
  );
  h.register_html_source(&format!(
    r#"<!doctype html><body>
      <script type="importmap">{importmap}</script>
      <script type="module" src="https://example.invalid/scope/entry.js"></script>
    </body>"#
  ));

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["scoped".to_string()]);
  Ok(())
}

#[test]
fn p2_import_map_parse_errors_surface_in_diagnostics() -> Result<()> {
  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut h = Harness::new("https://example.invalid/p2_importmap_error.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script type="importmap">{</script>
      <script>console.log("ok");</script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["ok".to_string()]);
  assert!(
    !js_exception_messages(&h.tab).is_empty(),
    "expected import map parse error to be reported to diagnostics"
  );
  Ok(())
}

// --- P3: Advanced lifecycle ---

#[test]
fn p3_load_event_waits_for_stylesheets_and_async_external_script_execution() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p3_load.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><html><head>
      <script>
        document.addEventListener("DOMContentLoaded", () => console.log("dcl"));
        window.addEventListener("load", () => console.log("load"));
      </script>
      <script async src="https://example.invalid/a.js"></script>
      <link rel="stylesheet" href="data:text/css,body{color:red}">
    </head><body></body></html>"#,
  );

  // Register the async script only after navigation begins so DOMContentLoaded can run first.
  h.navigate()?;
  h.register_script_source("https://example.invalid/a.js", r#"console.log("script");"#);
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["dcl".to_string(), "script".to_string(), "load".to_string()]
  );
  Ok(())
}

#[test]
fn p3_load_event_waits_for_async_module_script_execution() -> Result<()> {
  let js_options = JsExecutionOptions {
    supports_module_scripts: true,
    ..Default::default()
  };
  let mut h = Harness::new("https://example.invalid/p3_load_module.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><html><head>
      <script>
        document.addEventListener("DOMContentLoaded", () => console.log("dcl"));
        window.addEventListener("load", () => console.log("load"));
      </script>
      <script type="module" async src="https://example.invalid/amod.js"></script>
    </head><body></body></html>"#,
  );

  // Register the async module script only after navigation begins so DOMContentLoaded can run
  // first.
  h.navigate()?;
  h.register_script_source("https://example.invalid/amod.js", r#"console.log("module");"#);
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec!["dcl".to_string(), "module".to_string(), "load".to_string()]
  );
  Ok(())
}

#[test]
fn p3_unhandledrejection_event_fires_for_unhandled_promise_rejections() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p3_unhandled.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        addEventListener("unhandledrejection", (e) => {
          console.log("unhandled:" + e.reason);
          e.preventDefault();
        });
        Promise.reject("boom");
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["unhandled:boom".to_string()]);
  Ok(())
}

#[test]
fn p3_beforeunload_can_cancel_navigation() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let page1_url = "https://example.invalid/p3_beforeunload.html";
  let page2_url = "https://example.invalid/p3_beforeunload_next.html";
  let mut h = Harness::new(page1_url, js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <div id="page1"></div>
      <script>
        addEventListener("beforeunload", (e) => {
          console.log("beforeunload");
          e.preventDefault();
          e.returnValue = "stay";
        });
        addEventListener("unload", () => console.log("unload"));
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  h.tab.register_html_source(
    page2_url.to_string(),
    r#"<!doctype html><body>
      <div id="page2"></div>
      <script>console.log("page2");</script>
    </body>"#
      .to_string(),
  );
  h.navigate_to(page2_url)?;
  h.run_until_idle()?;

  assert_eq!(console_logs(&h.tab), vec!["beforeunload".to_string()]);
  assert!(
    h.tab.dom().get_element_by_id("page1").is_some(),
    "expected canceled navigation to remain on page1"
  );
  assert!(
    h.tab.dom().get_element_by_id("page2").is_none(),
    "expected canceled navigation to not commit page2"
  );
  Ok(())
}

#[test]
fn p3_pagehide_pageshow_fire_on_navigation_and_bfcache_transitions() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let page1_url = "https://example.invalid/p3_pageshow.html";
  let page2_url = "https://example.invalid/p3_pageshow_next.html";
  let mut h = Harness::new(page1_url, js_options)?;
  h.register_html_source(
    r#"<!doctype html><body>
      <div id="page1"></div>
      <script>
        addEventListener("beforeunload", () => console.log("beforeunload"));
        addEventListener("pagehide", (e) => console.log("pagehide:" + e.persisted));
        addEventListener("unload", () => console.log("unload"));
      </script>
    </body>"#,
  );
  h.navigate()?;
  h.run_until_idle()?;

  h.tab.register_html_source(
    page2_url.to_string(),
    r#"<!doctype html><body>
      <div id="page2"></div>
      <script>
        addEventListener("pageshow", (e) => console.log("pageshow:" + e.persisted));
        document.addEventListener("DOMContentLoaded", () => console.log("DOMContentLoaded"));
        addEventListener("load", () => console.log("load"));
      </script>
    </body>"#
      .to_string(),
  );
  h.navigate_to(page2_url)?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "beforeunload".to_string(),
      "pagehide:false".to_string(),
      "unload".to_string(),
      "pageshow:false".to_string(),
      "DOMContentLoaded".to_string(),
      "load".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p3_visibilitychange_fires_when_document_visibility_changes() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p3_visibility.html", js_options)?;
  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        document.addEventListener("visibilitychange", () => console.log(document.visibilityState));
      </script>
    </body>"#,
  );
  h.navigate()?;
  h.run_until_idle()?;
  assert!(console_logs(&h.tab).is_empty());
  h.tab.set_hidden(true)?;
  h.run_until_idle()?;
  assert_eq!(console_logs(&h.tab), vec!["hidden".to_string()]);
  Ok(())
}

#[test]
fn p3_window_onerror_fires_for_uncaught_errors() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p3_onerror.html", js_options)?;
  h.register_html_source(
    r#"<!doctype html><body>
      <script>
        window.onerror = function () { console.log("onerror"); };
        addEventListener("error", () => console.log("error"));
        setTimeout(() => {
          console.log("timer");
          throw new Error("boom");
        }, 0);
        console.log("scheduled");
      </script>
    </body>"#,
  );
  h.navigate()?;
  h.run_until_idle()?;
  assert_eq!(
    console_logs(&h.tab),
    vec![
      "scheduled".to_string(),
      "timer".to_string(),
      "error".to_string(),
      "onerror".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn p3_load_event_waits_for_images() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p3_images.html", js_options)?;

  // Use a non-data URL and override it via the script source registry (the override fetcher is used
  // for all destinations). This keeps the test deterministic while still exercising the image load
  // blocker pipeline.
  let img_url = "https://example.invalid/img.png";
  h.register_script_source(img_url, "fake image bytes");
  h.register_html_source(&format!(
    r#"<!doctype html><body>
      <img src="{img_url}">
      <script>
        document.addEventListener("DOMContentLoaded", () => {{
          console.log("dcl");
          setTimeout(() => console.log("timer"), 0);
        }});
        window.addEventListener("load", () => console.log("load"));
      </script>
    </body>"#
  ));
  h.navigate()?;
  h.run_until_idle()?;
  assert_eq!(
    console_logs(&h.tab),
    vec!["dcl".to_string(), "timer".to_string(), "load".to_string()]
  );
  Ok(())
}

#[test]
fn p3_load_event_waits_for_images_inserted_after_domcontentloaded() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/p3_images_dynamic.html", js_options)?;

  // Insert an image after DOMContentLoaded (via a microtask) and ensure it still delays `load`.
  //
  // This exercises the edge case where `load` may already have been queued by the time the image
  // enters the DOM: the lifecycle must discover the new load blocker before dispatch.
  let img_url = "https://example.invalid/dyn.png";
  h.register_script_source(img_url, "fake image bytes");
  h.register_html_source(&format!(
    r#"<!doctype html><body>
      <script>
        document.addEventListener("DOMContentLoaded", () => {{
          console.log("dcl");
          Promise.resolve().then(() => {{
            console.log("microtask");
            const img = document.createElement("img");
            img.src = "{img_url}";
            document.body.appendChild(img);
            setTimeout(() => console.log("timer"), 0);
          }});
        }});
        window.addEventListener("load", () => console.log("load"));
      </script>
    </body>"#
  ));
  h.navigate()?;
  h.run_until_idle()?;
  assert_eq!(
    console_logs(&h.tab),
    vec![
      "dcl".to_string(),
      "microtask".to_string(),
      "timer".to_string(),
      "load".to_string()
    ]
  );
  Ok(())
}
