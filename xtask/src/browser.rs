use std::path::Path;
use std::process::Command;

/// Environment variable read by `src/bin/browser.rs` to apply an in-process address-space cap (MiB).
pub const FASTR_BROWSER_MEM_LIMIT_MB_ENV: &str = "FASTR_BROWSER_MEM_LIMIT_MB";

/// Environment variable read by `src/bin/browser.rs` to run without initializing winit/wgpu.
pub const FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserCommandArgs {
  pub url: Option<String>,
  pub release: bool,
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

  // Apply in-process guardrails / test hooks expected by `src/bin/browser.rs`.
  if let Some(limit_mb) = args.mem_limit_mb {
    cmd.env(FASTR_BROWSER_MEM_LIMIT_MB_ENV, limit_mb.to_string());
  }
  if args.headless_smoke {
    cmd.env(FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV, "1");
  }

  cmd.arg("bash");
  cmd.arg(repo_root.join("scripts/cargo_agent.sh"));

  cmd.arg("run");
  if args.release {
    cmd.arg("--release");
  }
  cmd.args(["--features", "browser_ui"]);
  cmd.args(["--bin", "browser"]);

  // Always include the `--` separator so callers can append `<url>` without worrying about cargo
  // arg ordering. `cargo run -- ...` with no trailing args is still valid.
  cmd.arg("--");
  if let Some(url) = args.url.as_deref() {
    cmd.arg(url);
  }

  cmd
}

