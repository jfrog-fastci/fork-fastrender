use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable read by `src/bin/browser.rs` to apply an in-process address-space cap (MiB).
///
/// Preferred: pass `browser --mem-limit-mb <MiB>`.
pub const FASTR_BROWSER_MEM_LIMIT_MB_ENV: &str = "FASTR_BROWSER_MEM_LIMIT_MB";

/// Environment variable read by `src/bin/browser.rs` to run without initializing winit/wgpu.
///
/// Preferred: pass `browser --headless-smoke`.
pub const FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE";

/// Environment variable read by `src/bin/browser.rs` to show the in-app HUD overlay.
///
/// Preferred: pass `browser --hud` / `browser --no-hud`.
pub const FASTR_BROWSER_HUD_ENV: &str = "FASTR_BROWSER_HUD";

/// Legacy env var used to enable lightweight responsiveness/perf logging in the browser UI.
///
/// Preferred: pass `browser --perf-log`.
pub const FASTR_PERF_LOG_ENV: &str = "FASTR_PERF_LOG";

/// Legacy env var: optional output path for `FASTR_PERF_LOG` logs, when supported.
///
/// When unset/empty, logs are written to stdout so they can be piped/tee'd.
///
/// Preferred: pass `browser --perf-log-out <path>`.
pub const FASTR_PERF_LOG_OUT_ENV: &str = "FASTR_PERF_LOG_OUT";

/// Legacy env var used to write a Chrome trace of the windowed browser event loop.
///
/// Preferred: pass `browser --trace-out <path>`.
pub const FASTR_BROWSER_TRACE_OUT_ENV: &str = "FASTR_BROWSER_TRACE_OUT";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserCommandArgs {
  pub url: Option<String>,
  pub release: bool,
  /// When set, overrides `FASTR_BROWSER_HUD` for the spawned browser process.
  pub hud: Option<bool>,
  pub perf_log: bool,
  pub perf_log_out: Option<PathBuf>,
  pub trace_out: Option<PathBuf>,
  pub mem_limit_mb: Option<u64>,
  pub headless_smoke: bool,
}

/// Build the wrapper-safe command used to run the `browser` binary:
///
/// ```text
/// bash scripts/run_limited.sh --as 64G -- \
///   bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- <url>
/// ```
pub fn build_browser_command(repo_root: &Path, args: &BrowserCommandArgs) -> Command {
  let mut cmd = crate::cmd::run_limited_command_default(repo_root);
  cmd.current_dir(repo_root);

  cmd.arg("bash");
  cmd.arg(repo_root.join("scripts/cargo_agent.sh"));

  cmd.arg("run");
  if args.release {
    cmd.arg("--release");
  }
  cmd.args(["--features", "browser_ui"]);
  cmd.args(["--bin", "browser"]);

  // Always include the `--` separator so callers can append `<url>` without worrying about cargo
  // arg ordering. Cargo accepts a `run --` separator even when no trailing args are provided.
  cmd.arg("--");
  if let Some(enabled) = args.hud {
    cmd.arg(if enabled { "--hud" } else { "--no-hud" });
  }
  if let Some(out) = args.perf_log_out.as_ref() {
    cmd.arg("--perf-log-out");
    cmd.arg(out);
  } else if args.perf_log {
    cmd.arg("--perf-log");
  }
  if let Some(out) = args.trace_out.as_ref() {
    cmd.arg("--trace-out");
    cmd.arg(out);
  }
  if let Some(limit_mb) = args.mem_limit_mb {
    cmd.arg("--mem-limit-mb");
    cmd.arg(limit_mb.to_string());
  }
  if args.headless_smoke {
    cmd.arg("--headless-smoke");
  }
  if let Some(url) = args.url.as_deref() {
    cmd.arg(url);
  }

  cmd
}
