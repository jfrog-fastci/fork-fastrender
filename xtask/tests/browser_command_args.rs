use std::ffi::OsStr;
use std::path::PathBuf;

use xtask::browser::{
  build_browser_command, BrowserCommandArgs, FASTR_BROWSER_HUD_ENV, FASTR_BROWSER_MEM_LIMIT_MB_ENV,
  FASTR_BROWSER_TRACE_OUT_ENV, FASTR_PERF_LOG_ENV, FASTR_PERF_LOG_OUT_ENV,
  FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV,
};

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn cmd_args(cmd: &std::process::Command) -> Vec<String> {
  cmd
    .get_args()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect()
}

fn cmd_env(cmd: &std::process::Command, key: &str) -> Option<String> {
  cmd
    .get_envs()
    .find_map(|(k, v)| (k == OsStr::new(key)).then(|| v.map(|v| v.to_string_lossy().into_owned())))
    .flatten()
}

#[test]
fn browser_command_wraps_cargo_with_run_limited_and_cargo_agent() {
  let repo_root = repo_root();
  let cmd = build_browser_command(&repo_root, &BrowserCommandArgs::default());

  assert_eq!(cmd.get_program().to_string_lossy(), "bash");

  let args = cmd_args(&cmd);
  assert!(
    args
      .iter()
      .any(|arg| arg.ends_with("scripts/run_limited.sh")),
    "expected run_limited.sh in args, got {args:?}"
  );
  assert!(
    args.windows(2).any(|w| w == ["--as", "64G"]),
    "expected --as 64G in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--"),
    "expected `--` separator in args, got {args:?}"
  );
  assert!(
    args
      .iter()
      .any(|arg| arg.ends_with("scripts/cargo_agent.sh")),
    "expected cargo_agent.sh in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--features")
      && args.iter().any(|arg| arg == "browser_ui")
      && args.iter().any(|arg| arg == "--bin")
      && args.iter().any(|arg| arg == "browser"),
    "expected cargo args for `--features browser_ui --bin browser`, got {args:?}"
  );

  assert_eq!(
    cmd.get_current_dir().map(|p| p.to_path_buf()),
    Some(repo_root.clone()),
    "expected command to run from repo root"
  );

  assert!(
    cmd_env(&cmd, FASTR_BROWSER_HUD_ENV).is_none(),
    "expected default browser command not to set {FASTR_BROWSER_HUD_ENV}"
  );
  assert!(
    cmd_env(&cmd, FASTR_PERF_LOG_ENV).is_none(),
    "expected default browser command not to set {FASTR_PERF_LOG_ENV}"
  );
  assert!(
    cmd_env(&cmd, FASTR_PERF_LOG_OUT_ENV).is_none(),
    "expected default browser command not to set {FASTR_PERF_LOG_OUT_ENV}"
  );
  assert!(
    cmd_env(&cmd, FASTR_BROWSER_TRACE_OUT_ENV).is_none(),
    "expected default browser command not to set {FASTR_BROWSER_TRACE_OUT_ENV}"
  );
  assert!(
    cmd_env(&cmd, FASTR_BROWSER_MEM_LIMIT_MB_ENV).is_none(),
    "expected default browser command not to set {FASTR_BROWSER_MEM_LIMIT_MB_ENV}"
  );
  assert!(
    cmd_env(&cmd, FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV).is_none(),
    "expected default browser command not to set {FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV}"
  );
}

#[test]
fn browser_command_supports_release_url_and_cli_flags() {
  let repo_root = repo_root();
  let url = "https://example.com/".to_string();
  let perf_log_out = PathBuf::from("target/perf.log");
  let trace_out = PathBuf::from("target/trace.json");
  let perf_log_out_value = perf_log_out.to_string_lossy().into_owned();
  let trace_out_value = trace_out.to_string_lossy().into_owned();
  let cmd = build_browser_command(
    &repo_root,
    &BrowserCommandArgs {
      url: Some(url.clone()),
      release: true,
      hud: Some(true),
      perf_log: true,
      perf_log_out: Some(perf_log_out.clone()),
      trace_out: Some(trace_out.clone()),
      mem_limit_mb: Some(1024),
      headless_smoke: true,
    },
  );

  let args = cmd_args(&cmd);
  assert!(
    args.iter().any(|arg| arg == "--release"),
    "expected --release in args, got {args:?}"
  );
  assert!(
    args.last() == Some(&url),
    "expected URL to be the final arg, got {args:?}"
  );

  assert!(
    args.iter().any(|arg| arg == "--hud"),
    "expected --hud in args, got {args:?}"
  );
  assert!(
    args
      .windows(2)
      .any(|w| w == ["--perf-log-out", perf_log_out_value.as_str()]),
    "expected --perf-log-out <path> in args, got {args:?}"
  );
  assert!(
    args
      .windows(2)
      .any(|w| w == ["--trace-out", trace_out_value.as_str()]),
    "expected --trace-out <path> in args, got {args:?}"
  );
  assert!(
    args.windows(2).any(|w| w == ["--mem-limit-mb", "1024"]),
    "expected --mem-limit-mb 1024 in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--headless-smoke"),
    "expected --headless-smoke in args, got {args:?}"
  );

  assert!(
    cmd_env(&cmd, FASTR_BROWSER_TRACE_OUT_ENV).is_none(),
    "expected trace out env not to be set when CLI flag is available"
  );
  assert!(
    cmd_env(&cmd, FASTR_BROWSER_MEM_LIMIT_MB_ENV).is_none(),
    "expected mem limit env not to be set when CLI flag is available"
  );
  assert!(
    cmd_env(&cmd, FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV).is_none(),
    "expected headless smoke env not to be set when CLI flag is available"
  );
  assert!(
    cmd_env(&cmd, FASTR_BROWSER_HUD_ENV).is_none(),
    "expected hud env not to be set when CLI flag is available"
  );
  assert!(
    cmd_env(&cmd, FASTR_PERF_LOG_ENV).is_none(),
    "expected perf log env not to be set when CLI flag is available"
  );
  assert!(
    cmd_env(&cmd, FASTR_PERF_LOG_OUT_ENV).is_none(),
    "expected perf log out env not to be set when CLI flag is available"
  );
}
