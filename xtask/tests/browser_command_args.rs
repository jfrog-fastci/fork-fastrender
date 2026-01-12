use std::ffi::OsStr;
use std::path::PathBuf;

use xtask::browser::{
  build_browser_command, BrowserCommandArgs, FASTR_BROWSER_MEM_LIMIT_MB_ENV,
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
}

#[test]
fn browser_command_supports_release_url_and_env_flags() {
  let repo_root = repo_root();
  let url = "https://example.com/".to_string();
  let cmd = build_browser_command(
    &repo_root,
    &BrowserCommandArgs {
      url: Some(url.clone()),
      release: true,
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

  assert_eq!(
    cmd_env(&cmd, FASTR_BROWSER_MEM_LIMIT_MB_ENV).as_deref(),
    Some("1024"),
    "expected mem limit env to be set"
  );
  assert_eq!(
    cmd_env(&cmd, FASTR_TEST_BROWSER_HEADLESS_SMOKE_ENV).as_deref(),
    Some("1"),
    "expected headless smoke env to be set"
  );
}
