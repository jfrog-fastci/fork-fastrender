use crate::backend::{Backend, BackendInit, BackendKind, BackendSelection};
use crate::discover::{TestCase, TestKind};
use crate::meta::parse_leading_meta;
use crate::wpt_fs::{WptFs, WptFsError};
use crate::wpt_report::{WptReport, WptSubtest};
use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
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

    let timeout = match parsed.timeout {
      Some(HtmlTimeout::Long) => self.config.long_timeout,
      Some(HtmlTimeout::Short) => self.config.default_timeout,
      None => self.config.default_timeout,
    };

    let base_dir = id_dir(&test.id);

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
  }

  fn run_scripts_in_window(
    &self,
    test: &TestCase,
    base_dir: &str,
    scripts: Vec<ScriptToEval>,
    timeout: Duration,
  ) -> RunResultResult {
    let test_url = test.url();

    // Allow local debugging overrides via env var, but only when the runner is in `Auto` mode.
    let selection = if self.config.backend == BackendSelection::Auto {
      BackendSelection::from_env()?.unwrap_or(BackendSelection::Auto)
    } else {
      self.config.backend
    };
    let backend_kind = selection.resolve();
    if !backend_kind.is_available() {
      return Err(RunError::Js(format!(
        "selected backend `{backend_kind}` is not available in this build"
      )));
    }

    let mut backend: Box<dyn Backend> = match backend_kind {
      BackendKind::VmJs => Box::new(crate::backend_vmjs::VmJsBackend::new()),
    };

    if let Err(err) = backend.init_realm(BackendInit {
      test_url: test_url.clone(),
      timeout,
      max_tasks: self.config.max_tasks,
      max_microtasks: self.config.max_microtasks,
    }) {
      return Ok(RunResult {
        outcome: map_backend_error(err)?,
        wpt_report: None,
      });
    }

    // Load / evaluate scripts in-order. If a script throws, surface it as a harness-level error
    // (via `window.onerror` if available, else by calling `__fastrender_wpt_report` directly).
    for script in scripts {
      let src = match script {
        ScriptToEval::Url(url) => {
          let path = self.fs.resolve_url(base_dir, &url)?;
          self.fs.read_to_string(&path)?
        }
        ScriptToEval::Inline(src) => src,
      };

      if let Err(err) = backend.eval_script(&src) {
        match err {
          RunError::Js(msg) if is_interrupt_error(&msg) => {
            return Ok(RunResult {
              outcome: RunOutcome::Timeout,
              wpt_report: None,
            })
          }
          RunError::Js(msg) => {
            let _ = best_effort_report_uncaught_error(&mut *backend, &msg);
            break;
          }
          other => return Err(other),
        }
      }
    }

    let (outcome, wpt_report) = drive_backend_until_report(&mut *backend)?;
    Ok(RunResult { outcome, wpt_report })
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
        if (typeof onerror === "function") {{
          try {{ onerror(msg, "", 0, 0, msg); return; }} catch (_e) {{}}
        }}
      }} catch (_e) {{}}
      try {{
        if (typeof __fastrender_wpt_report === "function") {{
          __fastrender_wpt_report({{
            file_status: "error",
            harness_status: "error",
            message: msg,
            stack: null,
            subtests: []
          }});
        }}
      }} catch (_e) {{}}
    }})();"#
  );
  backend.eval_script(&src)
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
  msg.contains("interrupted") || msg.contains("Interrupt")
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
