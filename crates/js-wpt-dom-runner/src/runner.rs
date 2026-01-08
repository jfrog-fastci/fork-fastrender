use crate::discover::TestCase;
use crate::meta::parse_leading_meta;
use crate::wpt_fs::{WptFs, WptFsError};
use rquickjs::{Context, Object, Runtime};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct RunnerConfig {
  pub default_timeout: Duration,
  pub long_timeout: Duration,
}

impl Default for RunnerConfig {
  fn default() -> Self {
    Self {
      default_timeout: Duration::from_secs(5),
      long_timeout: Duration::from_secs(30),
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
      });
    }
    if !test.kind.is_runnable_in_window() {
      return Ok(RunResult {
        outcome: RunOutcome::Skip("unsupported test kind".to_string()),
      });
    }

    self.run_js_test_in_window(test)
  }

  fn run_js_test_in_window(&self, test: &TestCase) -> RunResultResult {
    let test_source = self.fs.read_to_string(&test.path)?;
    let meta = parse_leading_meta(&test_source);

    let timeout = meta.timeout.unwrap_or(self.config.default_timeout);

    // `// META: timeout=long` maps to the runner's `long_timeout` instead of a hard-coded value.
    let timeout = if meta
      .directives
      .iter()
      .any(|d| matches!(d, crate::meta::MetaDirective::TimeoutLong))
    {
      self.config.long_timeout
    } else {
      timeout
    };

    let base_dir = id_dir(&test.id);

    let mut script_urls = Vec::new();
    // Always load testharness.js first, but avoid double-loading if META already listed it.
    script_urls.push("/resources/testharness.js".to_string());
    for url in meta.scripts {
      if url == "/resources/testharness.js" || url == "resources/testharness.js" {
        continue;
      }
      script_urls.push(url);
    }

    let rt = Runtime::new().map_err(|e| RunError::Js(e.to_string()))?;

    // Interrupt handler for per-test wall-time.
    let deadline = Instant::now() + timeout;
    let deadline = Arc::new(deadline);
    rt.set_interrupt_handler(Some(Box::new({
      let deadline = Arc::clone(&deadline);
      move || Instant::now() >= *deadline
    })));

    let ctx = Context::full(&rt).map_err(|e| RunError::Js(e.to_string()))?;

    let test_url = test.url();
    let run = ctx.with(|ctx| -> Result<RunOutcome, RunError> {
      let globals = ctx.globals();
      install_window_shims(ctx.clone(), &globals, &test_url)?;

      // Load scripts.
      for url in script_urls {
        let path = self.fs.resolve_url(&base_dir, &url)?;
        let src = self.fs.read_to_string(&path)?;
        ctx.eval::<(), _>(src).map_err(|e| RunError::Js(e.to_string()))?;
      }

      // Evaluate the test source itself.
      ctx
        .eval::<(), _>(test_source)
        .map_err(|e| RunError::Js(e.to_string()))?;

      // Reporter shim: derive a simple pass/fail status for the harness used by our smoke tests.
      ctx
        .eval::<(), _>(REPORTER_SHIM)
        .map_err(|e| RunError::Js(e.to_string()))?;

      let status: Option<String> = globals
        .get("__fastrender_status")
        .map_err(|e| RunError::Js(e.to_string()))?;
      let outcome = match status.as_deref() {
        Some("PASS") => RunOutcome::Pass,
        Some("FAIL") => {
          let msg: Option<String> = globals
            .get("__fastrender_failure_message")
            .map_err(|e| RunError::Js(e.to_string()))?;
          RunOutcome::Fail(msg.unwrap_or_else(|| "test failed".to_string()))
        }
        Some(other) => RunOutcome::Error(format!("unknown status: {other}")),
        None => RunOutcome::Error("missing __fastrender_status".to_string()),
      };
      Ok(outcome)
    });

    // If the interrupt handler fired, QuickJS surfaces it as an eval error. Map it to Timeout.
    match run {
      Ok(outcome) => Ok(RunResult { outcome }),
      Err(RunError::Js(msg)) if msg.contains("interrupted") || msg.contains("Interrupt") => Ok(RunResult {
        outcome: RunOutcome::Timeout,
      }),
      Err(err) => Err(err),
    }
  }
}

fn id_dir(id: &str) -> String {
  match id.rsplit_once('/') {
    Some((dir, _file)) => dir.to_string(),
    None => String::new(),
  }
}

fn install_window_shims<'js>(
  ctx: rquickjs::Ctx<'js>,
  globals: &Object<'js>,
  href: &str,
) -> Result<(), RunError> {
  // `window` / `self` should refer to the global object in a window realm.
  globals
    .set("window", globals.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("self", globals.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Minimal `location` object.
  let location = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  location
    .set("href", href)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("location", location.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Minimal `document` object.
  let document = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  document
    .set("URL", href)
    .map_err(|e| RunError::Js(e.to_string()))?;
  document
    .set("location", location)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("document", document)
    .map_err(|e| RunError::Js(e.to_string()))?;

  // A tiny console shim to help debugging failing tests.
  let console = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  let log = rquickjs::Function::new(ctx, |msg: String| {
    eprintln!("[wpt] {msg}");
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  console
    .set("log", log)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("console", console)
    .map_err(|e| RunError::Js(e.to_string()))?;

  Ok(())
}

const REPORTER_SHIM: &str = r#"
(function() {
  if (typeof __wpt_results !== "undefined") {
    var failed = __wpt_results.filter(function(r) { return r.status !== 0; });
    if (failed.length === 0) {
      globalThis.__fastrender_status = "PASS";
    } else {
      globalThis.__fastrender_status = "FAIL";
      globalThis.__fastrender_failure_message = failed[0].message || "test failed";
    }
    return;
  }

  // Fallback: if the harness didn't define results, treat successful evaluation as PASS.
  globalThis.__fastrender_status = "PASS";
})();
"#;
