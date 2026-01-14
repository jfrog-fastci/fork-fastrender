use crate::backend::{Backend, BackendInit, BackendKind, BackendSelection};
use crate::discover::{TestCase, TestKind};
use crate::meta::parse_leading_meta;
use crate::wpt_fs::{WptFs, WptFsError};
use crate::wpt_report::{WptReport, WptSubtest};
#[cfg(feature = "vmjs")]
use crate::WptResourceFetcher;
use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
#[cfg(feature = "vmjs")]
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct RunnerConfig {
  pub default_timeout: Duration,
  pub long_timeout: Duration,
  pub max_tasks: usize,
  pub max_microtasks: usize,
  pub backend: BackendSelection,
}

impl Default for RunnerConfig {
  fn default() -> Self {
    Self {
      default_timeout: Duration::from_secs(5),
      long_timeout: Duration::from_secs(30),
      max_tasks: 100_000,
      max_microtasks: 100_000,
      backend: BackendSelection::Auto,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
  Pass,
  Fail(String),
  Skip(String),
  Error(String),
  Timeout,
}

#[derive(Debug, Clone)]
pub struct RunResult {
  pub outcome: RunOutcome,
  pub wpt_report: Option<WptReport>,
}

pub type RunResultResult = Result<RunResult, RunError>;

#[derive(Debug, Error)]
pub enum RunError {
  #[error("WPT fs error: {0}")]
  Fs(#[from] WptFsError),
  #[error("JS runtime error: {0}")]
  Js(String),
  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct Runner {
  fs: WptFs,
  config: RunnerConfig,
}

impl Runner {
  pub fn new(fs: WptFs, config: RunnerConfig) -> Self {
    Self { fs, config }
  }

  pub fn fs(&self) -> &WptFs {
    &self.fs
  }

  pub fn run_test(&self, test: &TestCase) -> RunResultResult {
    if let Some(reason) = test.kind.skip_reason() {
      return Ok(RunResult {
        outcome: RunOutcome::Skip(reason.to_string()),
        wpt_report: None,
      });
    }
    if !test.kind.is_runnable_in_window() {
      return Ok(RunResult {
        outcome: RunOutcome::Skip("unsupported test kind".to_string()),
        wpt_report: None,
      });
    }

    match test.kind {
      TestKind::Html => self.run_html_test_in_window(test),
      _ => self.run_js_test_in_window(test),
    }
  }

  fn run_js_test_in_window(&self, test: &TestCase) -> RunResultResult {
    let test_source = self.fs.read_to_string(&test.path)?;
    let meta = parse_leading_meta(&test_source);

    let base_dir = id_dir(&test.id);
    let timeout = timeout_for_directives(&meta.directives, &self.config);

    let mut scripts = Vec::new();
    push_testharness_bootstrap(&mut scripts);
    for url in meta.scripts {
      if is_required_harness_script(&url) || is_testharnessreport(&url) {
        continue;
      }
      scripts.push(ScriptToEval::Url(url));
    }
    scripts.push(ScriptToEval::Inline(test_source));

    self.run_scripts_in_window(test, &base_dir, scripts, timeout)
  }

  fn run_html_test_in_window(&self, test: &TestCase) -> RunResultResult {
    let html_source = self.fs.read_to_string(&test.path)?;
    let parsed = parse_html_test(&html_source)?;
    let uses_testharness = html_uses_testharness(&parsed);

    let timeout = match parsed.timeout {
      Some(HtmlTimeout::Long) => self.config.long_timeout,
      Some(HtmlTimeout::Short) => self.config.default_timeout,
      None => self.config.default_timeout,
    };

    let backend_kind = resolve_backend_kind(self.config.backend)?;
    if !backend_kind.is_available() {
      return Err(RunError::Js(format!(
        "selected backend `{backend_kind}` is not available in this build"
      )));
    }

    match backend_kind {
      BackendKind::VmJs => {
        #[cfg(feature = "vmjs")]
        {
          self.run_html_test_in_browser_tab(test, &html_source, uses_testharness, timeout)
        }
        #[cfg(not(feature = "vmjs"))]
        {
          Err(RunError::Js(format!(
            "selected backend `{backend_kind}` is not available in this build"
          )))
        }
      }
      BackendKind::VmJsRendered => {
        #[cfg(feature = "vmjs")]
        {
          let base_dir = id_dir(&test.id);

          if uses_testharness {
            let mut scripts = Vec::new();
            push_testharness_bootstrap(&mut scripts);
            for script in parsed.scripts {
              match script {
                ScriptToEval::Url(url) if is_required_harness_script(&url) => continue,
                ScriptToEval::Url(url) if is_testharnessreport(&url) => continue,
                other => scripts.push(other),
              }
            }

            self.run_scripts_in_window(test, &base_dir, scripts, timeout)
          } else {
            self.run_support_html_in_window(test, &base_dir, parsed.scripts, timeout)
          }
        }
        #[cfg(not(feature = "vmjs"))]
        {
          Err(RunError::Js(format!(
            "selected backend `{backend_kind}` is not available in this build"
          )))
        }
      }
      BackendKind::QuickJs => {
        let base_dir = id_dir(&test.id);

        if uses_testharness {
          let mut scripts = Vec::new();
          push_testharness_bootstrap(&mut scripts);
          for script in parsed.scripts {
            match script {
              ScriptToEval::Url(url) if is_required_harness_script(&url) => continue,
              ScriptToEval::Url(url) if is_testharnessreport(&url) => continue,
              other => scripts.push(other),
            }
          }

          self.run_scripts_in_window(test, &base_dir, scripts, timeout)
        } else {
          self.run_support_html_in_window(test, &base_dir, parsed.scripts, timeout)
        }
      }
    }
  }

  #[cfg(any(feature = "vmjs", feature = "quickjs"))]
  fn run_scripts_in_window(
    &self,
    test: &TestCase,
    base_dir: &str,
    scripts: Vec<ScriptToEval>,
    timeout: Duration,
  ) -> RunResultResult {
    let test_url = test.url();

    let backend_kind = resolve_backend_kind(self.config.backend)?;
    if !backend_kind.is_available() {
      return Err(RunError::Js(format!(
        "selected backend `{backend_kind}` is not available in this build"
      )));
    }

    let mut backend: Box<dyn Backend> = match backend_kind {
      BackendKind::QuickJs => {
        #[cfg(feature = "quickjs")]
        {
          Box::new(crate::engine::quickjs::QuickJsBackend::new())
        }
        #[cfg(not(feature = "quickjs"))]
        {
          return Err(RunError::Js(
            "selected backend `quickjs` is not available in this build".to_string(),
          ));
        }
      }
      BackendKind::VmJs => {
        #[cfg(feature = "vmjs")]
        {
          Box::new(crate::backend_vmjs::VmJsBackend::new(self.fs.clone()))
        }
        #[cfg(not(feature = "vmjs"))]
        {
          return Err(RunError::Js(
            "selected backend `vmjs` is not available in this build".to_string(),
          ));
        }
      }
      BackendKind::VmJsRendered => {
        #[cfg(feature = "vmjs")]
        {
          Box::new(crate::backend_vmjs_rendered::VmJsRenderedBackend::new(
            self.fs.clone(),
          ))
        }
        #[cfg(not(feature = "vmjs"))]
        {
          return Err(RunError::Js(
            "selected backend `vmjs-rendered` is not available in this build".to_string(),
          ));
        }
      }
    };

    if let Err(err) = backend.init_realm(
      BackendInit {
        test_url: test_url.clone(),
        fs: self.fs.clone(),
        timeout,
        max_tasks: self.config.max_tasks,
        max_microtasks: self.config.max_microtasks,
      },
      None,
    ) {
      return Ok(RunResult {
        outcome: map_backend_error(err)?,
        wpt_report: None,
      });
    }

    // Load / evaluate scripts in-order. If a script throws, surface it as a harness-level error
    // (via `window.onerror` if available, else by calling `__fastrender_wpt_report` directly).
    for script in scripts {
      let (src, name) = match script {
        ScriptToEval::Url(url) => {
          let path = self.fs.resolve_url(base_dir, &url)?;
          (self.fs.read_to_string(&path)?, url)
        }
        ScriptToEval::Inline(src) => (src, test_url.clone()),
      };

      if let Err(err) = backend.eval_script(&src, &name) {
        match err {
          RunError::Js(msg) if is_interrupt_error(&msg) => {
            return Ok(RunResult {
              outcome: RunOutcome::Timeout,
              wpt_report: None,
            })
          }
          RunError::Js(msg) => {
            // Some backends (vm-js) stash a report payload before returning an evaluation error.
            // Prefer that payload if it exists, otherwise fall back to a synthetic harness error.
            //
            // We *used* to rely on `best_effort_report_uncaught_error` + `drive_backend_until_report`
            // here, but if the JS context is too broken to run the reporting hook (or if the hook
            // itself fails), the runner would spin until its wall-clock timeout and misclassify the
            // test as `timed_out`.
            if let Some(report) = backend.take_report()? {
              let outcome = outcome_from_report(&report);
              return Ok(RunResult {
                outcome,
                wpt_report: Some(report),
              });
            }

            let _ = best_effort_report_uncaught_error(&mut *backend, &msg);
            if let Some(report) = backend.take_report()? {
              let outcome = outcome_from_report(&report);
              return Ok(RunResult {
                outcome,
                wpt_report: Some(report),
              });
            }

            let report = WptReport {
              file_status: "error".to_string(),
              harness_status: "error".to_string(),
              message: Some(msg.clone()),
              stack: None,
              subtests: Vec::new(),
            };
            return Ok(RunResult {
              outcome: RunOutcome::Error(msg),
              wpt_report: Some(report),
            });
          }
          other => return Err(other),
        }
      }
    }

    let (outcome, wpt_report) = drive_backend_until_report(&mut *backend)?;
    Ok(RunResult {
      outcome,
      wpt_report,
    })
  }

  #[cfg(any(feature = "vmjs", feature = "quickjs"))]
  fn run_support_html_in_window(
    &self,
    test: &TestCase,
    base_dir: &str,
    scripts: Vec<ScriptToEval>,
    timeout: Duration,
  ) -> RunResultResult {
    let test_url = test.url();

    let backend_kind = resolve_backend_kind(self.config.backend)?;
    if !backend_kind.is_available() {
      return Err(RunError::Js(format!(
        "selected backend `{backend_kind}` is not available in this build"
      )));
    }

    let mut backend: Box<dyn Backend> = match backend_kind {
      BackendKind::QuickJs => {
        #[cfg(feature = "quickjs")]
        {
          Box::new(crate::engine::quickjs::QuickJsBackend::new())
        }
        #[cfg(not(feature = "quickjs"))]
        {
          return Err(RunError::Js(
            "selected backend `quickjs` is not available in this build".to_string(),
          ));
        }
      }
      BackendKind::VmJs => {
        #[cfg(feature = "vmjs")]
        {
          Box::new(crate::backend_vmjs::VmJsBackend::new(self.fs.clone()))
        }
        #[cfg(not(feature = "vmjs"))]
        {
          return Err(RunError::Js(
            "selected backend `vmjs` is not available in this build".to_string(),
          ));
        }
      }
      BackendKind::VmJsRendered => {
        #[cfg(feature = "vmjs")]
        {
          Box::new(crate::backend_vmjs_rendered::VmJsRenderedBackend::new(
            self.fs.clone(),
          ))
        }
        #[cfg(not(feature = "vmjs"))]
        {
          return Err(RunError::Js(
            "selected backend `vmjs-rendered` is not available in this build".to_string(),
          ));
        }
      }
    };

    if let Err(err) = backend.init_realm(
      BackendInit {
        test_url: test_url.clone(),
        fs: self.fs.clone(),
        timeout,
        max_tasks: self.config.max_tasks,
        max_microtasks: self.config.max_microtasks,
      },
      None,
    ) {
      return Ok(RunResult {
        outcome: map_backend_error(err)?,
        wpt_report: None,
      });
    }

    for script in scripts {
      let (src, name) = match script {
        ScriptToEval::Url(url) => {
          let path = self.fs.resolve_url(base_dir, &url)?;
          (self.fs.read_to_string(&path)?, url)
        }
        ScriptToEval::Inline(src) => (src, test_url.clone()),
      };

      if let Err(err) = backend.eval_script(&src, &name) {
        match err {
          RunError::Js(msg) if is_interrupt_error(&msg) => {
            return Ok(RunResult {
              outcome: RunOutcome::Timeout,
              wpt_report: None,
            })
          }
          RunError::Js(msg) => {
            return Ok(RunResult {
              outcome: RunOutcome::Error(msg),
              wpt_report: None,
            })
          }
          other => return Err(other),
        }
      }
    }

    let outcome = drive_backend_until_idle(&mut *backend)?;
    Ok(RunResult {
      outcome,
      wpt_report: None,
    })
  }

  #[cfg(not(any(feature = "vmjs", feature = "quickjs")))]
  fn run_scripts_in_window(
    &self,
    _test: &TestCase,
    _base_dir: &str,
    _scripts: Vec<ScriptToEval>,
    _timeout: Duration,
  ) -> RunResultResult {
    Err(RunError::Js(
      "js-wpt-dom-runner was built without any JS backends; enable `vmjs` (recommended) or `quickjs`"
        .to_string(),
    ))
  }

  #[cfg(not(any(feature = "vmjs", feature = "quickjs")))]
  fn run_support_html_in_window(
    &self,
    _test: &TestCase,
    _base_dir: &str,
    _scripts: Vec<ScriptToEval>,
    _timeout: Duration,
  ) -> RunResultResult {
    Err(RunError::Js(
      "js-wpt-dom-runner was built without any JS backends; enable `vmjs` (recommended) or `quickjs`"
        .to_string(),
    ))
  }
}

fn resolve_backend_kind(config: BackendSelection) -> Result<BackendKind, RunError> {
  // Allow local debugging overrides via env var, but only when the runner is in `Auto` mode.
  let selection = if config == BackendSelection::Auto {
    BackendSelection::from_env()?.unwrap_or(BackendSelection::Auto)
  } else {
    config
  };
  Ok(selection.resolve())
}

#[cfg(feature = "vmjs")]
impl Runner {
  fn run_html_test_in_browser_tab(
    &self,
    test: &TestCase,
    html_source: &str,
    uses_testharness: bool,
    timeout: Duration,
  ) -> RunResultResult {
    use fastrender::api::{BrowserTab, DiagnosticsLevel, RenderOptions};
    use fastrender::js::{JsExecutionOptions, RunLimits, RunUntilIdleOutcome, RunUntilIdleStopReason};

    const REPORT_ATTR: &str = "data-fastrender-wpt-report";

    fn take_report_from_dom(tab: &BrowserTab) -> Option<String> {
      let dom = tab.dom();
      let html = dom.document_element()?;
      let raw = dom.get_attribute(html, REPORT_ATTR).ok().flatten()?;
      let raw = raw.trim();
      if raw.is_empty() {
        return None;
      }
      Some(raw.to_string())
    }

    // WPT HTML files commonly include `/resources/testharness.js` + `/resources/testharnessreport.js`,
    // but the offline runner relies on `/resources/fastrender_testharness_report.js` to emit a
    // machine-readable payload. When the test file does not include it, inject it immediately after
    // the harness script.
    fn patch_html_source(html: &str, uses_testharness: bool) -> String {
      if !uses_testharness {
        return html.to_string();
      }
      if html.contains("fastrender_testharness_report.js") {
        return html.to_string();
      }

      let reporter_tag = format!(
        r#"<script src="{FASTRENDER_TESTHARNESS_REPORT_JS}"></script>"#
      );

      // Best-effort insertion immediately after the first `testharness.js` script.
      if let Some(harness_idx) = html.find("testharness.js") {
        let after = &html[harness_idx..];
        if let Some(close_idx) = after.find("</script>") {
          let insert_at = harness_idx + close_idx + "</script>".len();
          let mut out = String::with_capacity(html.len() + reporter_tag.len() + 8);
          out.push_str(&html[..insert_at]);
          out.push('\n');
          out.push_str(&reporter_tag);
          out.push('\n');
          out.push_str(&html[insert_at..]);
          return out;
        }
      }

      // If we failed to inject the deterministic report hook, fall back to the original HTML. The
      // runner will treat missing report payloads as a harness-level error.
      html.to_string()
    }

    let test_url = test.url();
    let patched_html = patch_html_source(html_source, uses_testharness);
    let fetcher: Arc<dyn fastrender::resource::ResourceFetcher> =
      Arc::new(WptResourceFetcher::from_wpt_fs(&self.fs));

    let mut options = RenderOptions::default();
    options.diagnostics_level = DiagnosticsLevel::Basic;
    options.timeout = Some(timeout);

    // The vm-js backend enforces a per-spin JS wall-time budget via `JsExecutionOptions`. The safe
    // library default is intentionally short (500ms) and can interrupt larger WPT setup scripts (for
    // example `dom/common.js` range harness). Match the test timeout so scripts can complete.
    let mut js_execution_options = JsExecutionOptions::default();
    js_execution_options.event_loop_run_limits.max_wall_time = Some(timeout);
    js_execution_options.event_loop_run_limits.max_tasks = self.config.max_tasks;
    js_execution_options.event_loop_run_limits.max_microtasks = self.config.max_microtasks;
    // The curated HTML corpus includes `<script type="module">` and import maps. FastRender defaults
    // `supports_module_scripts=false` for hostile-input safety; enable it explicitly for the offline
    // WPT runner.
    js_execution_options.supports_module_scripts = true;

    let mut tab = match BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      &patched_html,
      &test_url,
      options,
      fetcher,
      js_execution_options,
    ) {
      Ok(tab) => tab,
      Err(err) => {
        let msg = err.to_string();
        // JS execution budget exhaustion (fuel/deadline) should classify as a timeout, not a hard
        // harness error.
        if is_interrupt_error(&msg) {
          return Ok(RunResult {
            outcome: RunOutcome::Timeout,
            wpt_report: None,
          });
        }
        return Err(RunError::Js(msg));
      }
    };

    fn parse_report_from_payload(payload: &str) -> Result<WptReport, RunError> {
      serde_json::from_str(payload).map_err(|err| {
        RunError::Js(format!(
          "failed to parse WPT report JSON from {REPORT_ATTR}: {err}; payload={payload:?}"
        ))
      })
    }

    // FastRender's `BrowserTab` does not block on async work (networking, iframe loads, etc). A test
    // may become temporarily idle while async operations are in flight, then resume later when those
    // operations enqueue tasks from other threads. Keep driving the event loop until:
    // - the WPT report payload appears (testharness tests), or the document becomes stably idle
    //   (helper HTML pages),
    // - we hit the overall wall-clock timeout.
    let deadline = std::time::Instant::now()
      .checked_add(timeout)
      .unwrap_or_else(std::time::Instant::now);

    let mut last_outcome: Option<RunUntilIdleOutcome> = None;
    let mut idle_streak: u8 = 0;
    loop {
      if let Some(payload) = take_report_from_dom(&tab) {
        let report = parse_report_from_payload(&payload)?;
        let outcome = outcome_from_report(&report);
        return Ok(RunResult {
          outcome,
          wpt_report: Some(report),
        });
      }

      let now = std::time::Instant::now();
      if now >= deadline {
        return Ok(RunResult {
          outcome: RunOutcome::Timeout,
          wpt_report: None,
        });
      }
      let remaining = deadline.duration_since(now);

      let limits = RunLimits {
        max_tasks: self.config.max_tasks,
        max_microtasks: self.config.max_microtasks,
        max_wall_time: Some(remaining),
      };
      let outcome = match tab.run_event_loop_until_idle(limits) {
        Ok(outcome) => outcome,
        Err(err) => {
          let msg = err.to_string();
          if is_interrupt_error(&msg) {
            return Ok(RunResult {
              outcome: RunOutcome::Timeout,
              wpt_report: None,
            });
          }
          return Err(RunError::Js(msg));
        }
      };
      last_outcome = Some(outcome);

      match outcome {
        RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::WallTime { .. }) => {
          return Ok(RunResult {
            outcome: RunOutcome::Timeout,
            wpt_report: None,
          });
        }

        RunUntilIdleOutcome::Idle => {
          if uses_testharness {
            // For testharness HTML tests, "idle but no report" can happen while async operations are
            // in flight. Keep retrying until the overall deadline.
            std::thread::sleep(Duration::from_millis(1));
            continue;
          }

          // Helper HTML pages don't produce a report payload. Treat "stably idle" as PASS
          // (best-effort) assuming no JS exceptions were thrown.
          idle_streak = idle_streak.saturating_add(1);
          if idle_streak >= 2 {
            if let Some(diag) = tab.diagnostics_snapshot() {
              if !diag.js_exceptions.is_empty() {
                return Ok(RunResult {
                  outcome: RunOutcome::Error(format!(
                    "HTML helper produced JS exceptions: {:?}",
                    diag.js_exceptions
                  )),
                  wpt_report: None,
                });
              }
            }
            return Ok(RunResult {
              outcome: RunOutcome::Pass,
              wpt_report: None,
            });
          }

          std::thread::sleep(Duration::from_millis(1));
        }

        RunUntilIdleOutcome::Stopped(_) => {
          idle_streak = 0;
          if uses_testharness {
            // Harness tests: surface as a harness-level error below (missing report payload).
            break;
          }

          // Helper pages: treat as a timeout (hit max tasks/microtasks before becoming idle).
          return Ok(RunResult {
            outcome: RunOutcome::Timeout,
            wpt_report: None,
          });
        }
      }
    }

    let diagnostics = tab.diagnostics_snapshot();
    let other = last_outcome.unwrap_or(RunUntilIdleOutcome::Idle);
    let msg = match diagnostics {
      Some(diag) if !diag.js_exceptions.is_empty() || !diag.console_messages.is_empty() => {
        format!(
          "HTML test produced no WPT report payload (event loop outcome={other:?}); js_exceptions={:?} console_messages={:?}",
          diag.js_exceptions, diag.console_messages
        )
      }
      _ => format!("HTML test produced no WPT report payload (event loop outcome={other:?})"),
    };
    Ok(RunResult {
      outcome: RunOutcome::Error(msg),
      wpt_report: None,
    })
  }
}

#[derive(Debug, Clone)]
enum ScriptToEval {
  Url(String),
  Inline(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HtmlTimeout {
  Short,
  Long,
}

#[derive(Debug, Clone)]
struct HtmlTestParseResult {
  timeout: Option<HtmlTimeout>,
  scripts: Vec<ScriptToEval>,
}

const TESTHARNESS_JS: &str = "/resources/testharness.js";
const FASTRENDER_TESTHARNESS_REPORT_JS: &str = "/resources/fastrender_testharness_report.js";
const TESTHARNESSREPORT_JS: &str = "/resources/testharnessreport.js";

fn timeout_for_directives(
  directives: &[crate::meta::MetaDirective],
  config: &RunnerConfig,
) -> Duration {
  if directives
    .iter()
    .any(|d| matches!(d, crate::meta::MetaDirective::TimeoutLong))
  {
    return config.long_timeout;
  }
  // `timeout=short` maps to the default timeout (we don't currently have a distinct "short" budget).
  config.default_timeout
}

fn push_testharness_bootstrap(out: &mut Vec<ScriptToEval>) {
  out.push(ScriptToEval::Url(TESTHARNESS_JS.to_string()));
  out.push(ScriptToEval::Url(
    FASTRENDER_TESTHARNESS_REPORT_JS.to_string(),
  ));
}

fn is_required_harness_script(url: &str) -> bool {
  is_equivalent_wpt_url(url, TESTHARNESS_JS)
    || is_equivalent_wpt_url(url, FASTRENDER_TESTHARNESS_REPORT_JS)
}

fn is_testharnessreport(url: &str) -> bool {
  is_equivalent_wpt_url(url, TESTHARNESSREPORT_JS)
}

fn is_equivalent_wpt_url(url: &str, expected_path: &str) -> bool {
  let url = url.trim();
  if url == expected_path {
    return true;
  }
  if let Some(stripped) = expected_path.strip_prefix('/') {
    if url == stripped {
      return true;
    }
  }
  if let Ok(parsed) = Url::parse(url) {
    let origin = parsed.origin().unicode_serialization();
    if origin == "https://web-platform.test" || origin == "http://web-platform.test" {
      return parsed.path() == expected_path;
    }
  }
  false
}

fn parse_html_test(source: &str) -> Result<HtmlTestParseResult, RunError> {
  let mut bytes = source.as_bytes();
  let dom = parse_document(RcDom::default(), Default::default())
    .from_utf8()
    .read_from(&mut bytes)?;

  let mut out = HtmlTestParseResult {
    timeout: None,
    scripts: Vec::new(),
  };
  collect_html_metadata(dom.document, &mut out);
  Ok(out)
}

fn html_uses_testharness(parsed: &HtmlTestParseResult) -> bool {
  parsed.scripts.iter().any(|script| match script {
    ScriptToEval::Url(url) => is_equivalent_wpt_url(url, TESTHARNESS_JS),
    ScriptToEval::Inline(_) => false,
  })
}

fn collect_html_metadata(handle: Handle, out: &mut HtmlTestParseResult) {
  if let NodeData::Element {
    ref name,
    ref attrs,
    ..
  } = handle.data
  {
    if name.local.as_ref().eq_ignore_ascii_case("meta") {
      let mut meta_name = None;
      let mut meta_content = None;
      for attr in attrs.borrow().iter() {
        if attr.name.local.as_ref().eq_ignore_ascii_case("name") {
          meta_name = Some(attr.value.to_string());
        } else if attr.name.local.as_ref().eq_ignore_ascii_case("content") {
          meta_content = Some(attr.value.to_string());
        }
      }
      if let (Some(meta_name), Some(meta_content)) = (meta_name, meta_content) {
        if meta_name.trim().eq_ignore_ascii_case("timeout") {
          match meta_content.trim().to_ascii_lowercase().as_str() {
            "long" => out.timeout = Some(HtmlTimeout::Long),
            "short" => out.timeout = Some(HtmlTimeout::Short),
            _ => {}
          }
        }
      }
    }

    if name.local.as_ref().eq_ignore_ascii_case("script") {
      let mut src = None;
      for attr in attrs.borrow().iter() {
        if attr.name.local.as_ref().eq_ignore_ascii_case("src") {
          src = Some(attr.value.to_string());
          break;
        }
      }

      if let Some(src) = src {
        let src = src.trim();
        if !src.is_empty() {
          out.scripts.push(ScriptToEval::Url(src.to_string()));
          return;
        }
      }

      let inline = extract_inline_script(&handle);
      out.scripts.push(ScriptToEval::Inline(inline));
      return;
    }
  }

  for child in handle.children.borrow().iter() {
    collect_html_metadata(child.clone(), out);
  }
}

fn extract_inline_script(handle: &Handle) -> String {
  let mut out = String::new();
  for child in handle.children.borrow().iter() {
    if let NodeData::Text { ref contents } = child.data {
      out.push_str(contents.borrow().as_ref());
    }
  }
  out
}

fn best_effort_report_uncaught_error(
  backend: &mut dyn Backend,
  message: &str,
) -> Result<(), RunError> {
  // Escape using JSON so we can embed it as a JS string literal.
  let msg = serde_json::to_string(message).map_err(|e| RunError::Js(e.to_string()))?;
  let src = format!(
    r#"(function () {{
      var msg = {msg};
      try {{
        if (typeof onerror === \"function\") {{
          try {{ onerror(msg, \"\", 0, 0, msg); return; }} catch (_e) {{}}
        }}
      }} catch (_e) {{}}
      try {{
        if (typeof __fastrender_wpt_report === \"function\") {{
          __fastrender_wpt_report({{
            file_status: \"error\",
            harness_status: \"error\",
            message: msg,
            stack: null,
            subtests: []
          }});
        }}
      }} catch (_e) {{}}
    }})();"#
  );
  backend.eval_script(&src, "fastrender_uncaught_error_report.js")
}

fn drive_backend_until_report(
  backend: &mut dyn Backend,
) -> Result<(RunOutcome, Option<WptReport>), RunError> {
  loop {
    if let Some(report) = backend.take_report()? {
      let outcome = outcome_from_report(&report);
      return Ok((outcome, Some(report)));
    }

    if backend.is_timed_out() {
      return Ok((RunOutcome::Timeout, None));
    }

    if let Err(err) = backend.drain_microtasks() {
      return Ok((map_backend_error(err)?, None));
    }

    if let Some(report) = backend.take_report()? {
      let outcome = outcome_from_report(&report);
      return Ok((outcome, Some(report)));
    }

    if backend.is_timed_out() {
      return Ok((RunOutcome::Timeout, None));
    }

    let did_work = match backend.poll_event_loop() {
      Ok(v) => v,
      Err(err) => return Ok((map_backend_error(err)?, None)),
    };

    if let Some(report) = backend.take_report()? {
      let outcome = outcome_from_report(&report);
      return Ok((outcome, Some(report)));
    }

    if backend.is_timed_out() {
      return Ok((RunOutcome::Timeout, None));
    }

    if !did_work {
      backend.idle_wait();
    }
  }
}

fn drive_backend_until_idle(backend: &mut dyn Backend) -> Result<RunOutcome, RunError> {
  loop {
    if backend.is_timed_out() {
      return Ok(RunOutcome::Timeout);
    }

    if let Err(err) = backend.drain_microtasks() {
      return Ok(map_backend_error(err)?);
    }

    if backend.is_timed_out() {
      return Ok(RunOutcome::Timeout);
    }

    let did_work = match backend.poll_event_loop() {
      Ok(v) => v,
      Err(err) => return Ok(map_backend_error(err)?),
    };

    if backend.is_timed_out() {
      return Ok(RunOutcome::Timeout);
    }

    if !did_work {
      return Ok(RunOutcome::Pass);
    }
  }
}

fn outcome_from_report(report: &WptReport) -> RunOutcome {
  let msg = report
    .message
    .clone()
    .or_else(|| first_nonpass_message(&report.subtests));
  match report.file_status.as_str() {
    "pass" => RunOutcome::Pass,
    "fail" => RunOutcome::Fail(msg.unwrap_or_else(|| "test failed".to_string())),
    "timeout" => RunOutcome::Timeout,
    "error" => RunOutcome::Error(msg.unwrap_or_else(|| "harness error".to_string())),
    other => RunOutcome::Error(format!("unknown file_status: {other}")),
  }
}

fn map_backend_error(err: RunError) -> Result<RunOutcome, RunError> {
  match err {
    RunError::Js(msg) if is_interrupt_error(&msg) => Ok(RunOutcome::Timeout),
    RunError::Js(msg) => Ok(RunOutcome::Error(msg)),
    other => Err(other),
  }
}

fn is_interrupt_error(msg: &str) -> bool {
  let lower = msg.to_ascii_lowercase();
  lower.contains("execution terminated: out of fuel")
    || lower.contains("execution terminated: deadline exceeded")
    || lower.contains("execution terminated: interrupted")
    || lower.contains("outoffuel")
    || lower.contains("deadlineexceeded")
    || lower.contains("interrupted")
    || lower.contains("interrupt")
}

fn id_dir(id: &str) -> String {
  match id.rsplit_once('/') {
    Some((dir, _file)) => dir.to_string(),
    None => String::new(),
  }
}

fn first_nonpass_message(subtests: &[WptSubtest]) -> Option<String> {
  for st in subtests {
    if st.status != "pass" {
      if let Some(msg) = &st.message {
        if !msg.is_empty() {
          return Some(msg.clone());
        }
      }
    }
  }
  None
}
