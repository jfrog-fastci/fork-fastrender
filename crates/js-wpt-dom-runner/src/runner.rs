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
    // Always load testharness.js + FastRender reporter shim first.
    script_urls.push("/resources/testharness.js".to_string());
    script_urls.push("/resources/fastrender_testharness_report.js".to_string());
    for url in meta.scripts {
      if url == "/resources/testharness.js"
        || url == "resources/testharness.js"
        || url == "/resources/fastrender_testharness_report.js"
        || url == "resources/fastrender_testharness_report.js"
      {
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

    // One-time realm initialization + script execution.
    let init = ctx.with(|ctx| -> Result<(), RunError> {
      let globals = ctx.globals();
      install_window_shims(ctx.clone(), &globals, &test_url)?;

      // Provide basic timer shims required by our vendored `testharness.js` subset.
      ctx
        .eval::<(), _>(TIMER_SHIM)
        .map_err(|e| RunError::Js(e.to_string()))?;

      // Define the host hook used by `fastrender_testharness_report.js` to emit a result payload.
      ctx
        .eval::<(), _>(FASTR_REPORT_HOOK)
        .map_err(|e| RunError::Js(e.to_string()))?;

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

      Ok(())
    });

    if let Err(err) = init {
      let outcome = match err {
        RunError::Js(msg) if msg.contains("interrupted") || msg.contains("Interrupt") => RunOutcome::Timeout,
        RunError::Js(msg) => RunOutcome::Error(msg),
        other => return Err(other),
      };
      return Ok(RunResult { outcome });
    }

    // Drive the minimal event loop until the reporter hook is called or we time out.
    let outcome = loop {
      if Instant::now() >= *deadline {
        break RunOutcome::Timeout;
      }

      // Run any queued Promise jobs (microtasks).
      if let Err(msg) = drain_promise_jobs(&rt) {
        if msg.contains("interrupted") || msg.contains("Interrupt") {
          break RunOutcome::Timeout;
        }
        break RunOutcome::Error(msg);
      }

      let poll = ctx.with(|ctx| -> Result<(bool, i32), RunError> {
        let globals = ctx.globals();
        let ran_timers: i32 = ctx
          .eval("__fastrender_poll_timers()")
          .map_err(|e| RunError::Js(e.to_string()))?;
        let did_report: Option<bool> = globals
          .get("__fastrender_wpt_report_called")
          .map_err(|e| RunError::Js(e.to_string()))?;
        Ok((did_report.unwrap_or(false), ran_timers))
      });
      let (did_report, ran_timers) = match poll {
        Ok(v) => v,
        Err(RunError::Js(msg)) if msg.contains("interrupted") || msg.contains("Interrupt") => {
          break RunOutcome::Timeout
        }
        Err(RunError::Js(msg)) => break RunOutcome::Error(msg),
        Err(other) => return Err(other),
      };

      if did_report {
        let report = ctx.with(|ctx| -> Result<(Option<String>, Option<String>), RunError> {
          let globals = ctx.globals();
          let file_status: Option<String> = globals
            .get("__fastrender_file_status")
            .map_err(|e| RunError::Js(e.to_string()))?;
          let message: Option<String> = globals
            .get("__fastrender_message")
            .map_err(|e| RunError::Js(e.to_string()))?;
          Ok((file_status, message))
        });
        let (file_status, message) = match report {
          Ok(v) => v,
          Err(RunError::Js(msg)) if msg.contains("interrupted") || msg.contains("Interrupt") => {
            break RunOutcome::Timeout
          }
          Err(RunError::Js(msg)) => break RunOutcome::Error(msg),
          Err(other) => return Err(other),
        };

        break match file_status.as_deref() {
          Some("pass") => RunOutcome::Pass,
          Some("fail") => RunOutcome::Fail(message.unwrap_or_else(|| "test failed".to_string())),
          Some("timeout") => RunOutcome::Timeout,
          Some("error") => RunOutcome::Error(message.unwrap_or_else(|| "harness error".to_string())),
          Some(other) => RunOutcome::Error(format!("unknown file_status: {other}")),
          None => RunOutcome::Error("missing fastrender testharness report".to_string()),
        };
      }

      if ran_timers == 0 {
        // Avoid busy-looping; timers use wall clock time (Date.now()) so sleeping advances them.
        std::thread::sleep(Duration::from_millis(1));
      }
    };

    // If the interrupt handler fired, QuickJS surfaces it as an eval error. Map it to Timeout.
    Ok(RunResult { outcome })
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

fn drain_promise_jobs(rt: &Runtime) -> Result<(), String> {
  // rquickjs exposes QuickJS's `JS_ExecutePendingJob`. The API is intentionally a bit low-level;
  // we loop until no more jobs are queued.
  loop {
    match rt.execute_pending_job() {
      Ok(true) => continue,
      Ok(false) => return Ok(()),
      Err(err) => return Err(err.to_string()),
    }
  }
}

const TIMER_SHIM: &str = r#"
// Minimal timer/event-loop shims used by the offline WPT harness.
//
// Notes:
// - Timers are tracked in JS (so we don't need Rust-side persistent handles).
// - We only implement `setTimeout`/`clearTimeout`/`queueMicrotask` for now.
// - `queueMicrotask` is implemented in terms of a 0ms timeout; QuickJS's native Promise job queue
//   is still drained from Rust via `Runtime::execute_pending_job`.
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.__fastrender_poll_timers === "function") return;

  var next_id = 1;
  var timers = new Map(); // id -> { cb, args, due }

  function nowMs() {
    return Date.now();
  }

  function normalizeDelay(ms) {
    var n = Number(ms);
    if (!isFinite(n) || isNaN(n)) n = 0;
    if (n < 0) n = 0;
    return n;
  }

  g.setTimeout = function (cb, ms /*, ...args */) {
    var id = next_id++;
    var delay = normalizeDelay(ms);
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    timers.set(id, { cb: cb, args: args, due: nowMs() + delay });
    return id;
  };

  g.clearTimeout = function (id) {
    timers.delete(Number(id));
  };

  g.queueMicrotask = function (cb) {
    // HTML queueMicrotask semantics are not modeled precisely yet; for the harness it is sufficient
    // that the callback runs after the current job.
    g.setTimeout(cb, 0);
  };

  g.__fastrender_poll_timers = function () {
    var now = nowMs();
    var due_ids = [];
    timers.forEach(function (entry, id) {
      if (entry.due <= now) due_ids.push(id);
    });
    due_ids.sort(function (a, b) { return a - b; });
    for (var i = 0; i < due_ids.length; i++) {
      var id = due_ids[i];
      var entry = timers.get(id);
      if (!entry) continue;
      timers.delete(id);
      if (typeof entry.cb === "function") {
        entry.cb.apply(g, entry.args);
      } else if (typeof entry.cb === "string") {
        // String handlers are legacy and intentionally unsupported in FastRender's harness.
        throw new Error("setTimeout string handler is not supported");
      } else {
        throw new Error("setTimeout callback is not callable");
      }
    }
    return due_ids.length;
  };
})();
"#;

const FASTR_REPORT_HOOK: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  g.__fastrender_wpt_report_called = false;
  g.__fastrender_file_status = null;
  g.__fastrender_harness_status = null;
  g.__fastrender_message = null;
  g.__fastrender_stack = null;

  g.__fastrender_wpt_report = function (payload) {
    g.__fastrender_wpt_report_called = true;
    g.__fastrender_file_status = payload && payload.file_status !== undefined ? payload.file_status : null;
    g.__fastrender_harness_status = payload && payload.harness_status !== undefined ? payload.harness_status : null;
    g.__fastrender_message = payload && payload.message !== undefined ? payload.message : null;
    g.__fastrender_stack = payload && payload.stack !== undefined ? payload.stack : null;
  };
})();
"#;
