use std::ffi::OsStr;
use std::path::PathBuf;

use xtask::page_loop_plan::{
  build_bins_command, build_inspect_frag_command, build_render_fixtures_command,
  inspect_frag_executable, render_fixtures_executable, InspectFragCommandArgs,
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
    .find_map(|(k, v)| {
      (k == OsStr::new(key)).then(|| v.map(|v| v.to_string_lossy().into_owned()))
    })
    .flatten()
}

fn assert_has_run_limited_wrapper(args: &[String]) {
  assert!(
    args.iter().any(|arg| arg.ends_with("scripts/run_limited.sh")),
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
}

#[test]
fn page_loop_build_command_builds_required_bins_in_release() {
  let repo_root = repo_root();
  let cmd = build_bins_command(
    &repo_root,
    false,
    &["render_fixtures", "inspect_frag", "diff_renders"],
  );

  assert_eq!(cmd.get_program().to_string_lossy(), "bash");

  let args = cmd_args(&cmd);
  assert!(
    args.iter().any(|arg| arg.ends_with("scripts/cargo_agent.sh")),
    "expected cargo_agent.sh in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "build"),
    "expected `build` in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--release"),
    "expected --release in args, got {args:?}"
  );
  for bin in ["render_fixtures", "inspect_frag", "diff_renders"] {
    assert!(
      args.windows(2).any(|w| w == ["--bin", bin]),
      "expected --bin {bin} in args, got {args:?}"
    );
  }

  assert_eq!(
    cmd.get_current_dir().map(|p| p.to_path_buf()),
    Some(repo_root.clone()),
    "expected command to run from repo root"
  );
}

#[test]
fn page_loop_render_fixtures_runs_prebuilt_binary_under_run_limited() {
  let repo_root = repo_root();
  let fixtures_dir = repo_root.join("tests/pages/fixtures");
  let out_dir = repo_root.join("target/page_loop_test_out");
  let cmd = build_render_fixtures_command(
    &repo_root,
    false,
    &fixtures_dir,
    &out_dir,
    "example.com",
    1,
    (1040, 1240),
    1.0,
    "screen",
    60,
    true,
    true,
  );

  assert_eq!(cmd.get_program().to_string_lossy(), "bash");

  let args = cmd_args(&cmd);
  assert_has_run_limited_wrapper(&args);
  assert!(
    !args.iter().any(|arg| arg.ends_with("scripts/cargo_agent.sh")),
    "expected render_fixtures command to run the built executable directly (no cargo_agent.sh), got {args:?}"
  );
  let expected_exe = render_fixtures_executable(&repo_root, false)
    .to_string_lossy()
    .into_owned();
  assert!(
    args.iter().any(|arg| arg == &expected_exe),
    "expected render_fixtures executable {expected_exe} in args, got {args:?}"
  );
  assert!(
    args
      .iter()
      .any(|arg| arg.ends_with(format!("render_fixtures{}", std::env::consts::EXE_SUFFIX).as_str())),
    "expected render_fixtures executable suffix in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--patch-html-for-chrome-baseline"),
    "expected page-loop Chrome diff mode to patch HTML for baseline parity, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--system-fonts"),
    "expected page-loop Chrome diff mode to enable system fonts so generic families match Chrome, got {args:?}"
  );
  assert!(
    args
      .windows(2)
      .any(|w| w == ["--animation-time-ms", "4940"]),
    "expected page-loop Chrome diff mode to sample animated images at 4940ms (align with Chrome screenshot timing), got {args:?}"
  );
  assert_eq!(
    cmd_env(&cmd, "FASTR_USE_BUNDLED_FONTS").as_deref(),
    None,
    "page-loop should not force FASTR_USE_BUNDLED_FONTS; render_fixtures owns its font config"
  );
  assert_eq!(
    cmd.get_current_dir().map(|p| p.to_path_buf()),
    Some(repo_root.clone()),
    "expected command to run from repo root"
  );
}

#[test]
fn page_loop_inspect_frag_runs_prebuilt_binary_under_run_limited() {
  let repo_root = repo_root();
  let fixture_html = repo_root.join("tests/pages/fixtures/example.com/index.html");
  let overlay_png = repo_root.join("target/page_loop_test_out/example.com.png");
  let inspect_dir = repo_root.join("target/page_loop_test_out/inspect");

  let cmd = build_inspect_frag_command(
    &repo_root,
    false,
    &InspectFragCommandArgs {
      fixture_html,
      overlay_png: Some(overlay_png),
      dump_json_dir: Some(inspect_dir),
      filter_selector: None,
      filter_id: None,
      dump_custom_properties: true,
      custom_property_prefix: vec!["--color".to_string()],
      custom_properties_limit: Some(10),
      patch_html_for_chrome_baseline: false,
      viewport: (1040, 1240),
      dpr: 1.0,
      media: "screen".to_string(),
      timeout: 60,
    },
  );

  assert_eq!(cmd.get_program().to_string_lossy(), "bash");

  let args = cmd_args(&cmd);
  assert_has_run_limited_wrapper(&args);
  let expected_exe = inspect_frag_executable(&repo_root, false)
    .to_string_lossy()
    .into_owned();
  assert!(
    args.iter().any(|arg| arg == &expected_exe),
    "expected inspect_frag executable {expected_exe} in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg.ends_with(format!("inspect_frag{}", std::env::consts::EXE_SUFFIX).as_str())),
    "expected inspect_frag executable in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--render-overlay"),
    "expected --render-overlay in args, got {args:?}"
  );
  assert!(
    args.iter().any(|arg| arg == "--dump-json"),
    "expected --dump-json in args, got {args:?}"
  );
  assert!(
    args.iter()
      .any(|arg| arg == "--dump-custom-properties")
      && args.iter().any(|arg| arg == "--custom-properties-limit")
      && args.iter().any(|arg| arg == "10")
      && args.iter().any(|arg| arg == "--custom-property-prefix=--color"),
    "expected custom property dump args, got {args:?}"
  );

  assert_eq!(
    cmd_env(&cmd, "FASTR_USE_BUNDLED_FONTS").as_deref(),
    None,
    "page-loop should not force FASTR_USE_BUNDLED_FONTS; inspect_frag owns its font config"
  );
  assert_eq!(
    cmd.get_current_dir().map(|p| p.to_path_buf()),
    Some(repo_root.clone()),
    "expected command to run from repo root"
  );
}

#[test]
fn page_loop_build_command_in_debug_omits_release_flag() {
  let repo_root = repo_root();
  let cmd = build_bins_command(&repo_root, true, &["render_fixtures"]);

  let args = cmd_args(&cmd);
  assert!(
    !args.iter().any(|arg| arg == "--release"),
    "expected debug build command to omit --release, got {args:?}"
  );
}

#[test]
fn page_loop_render_fixtures_uses_debug_executable_when_requested() {
  let repo_root = repo_root();
  let fixtures_dir = repo_root.join("tests/pages/fixtures");
  let out_dir = repo_root.join("target/page_loop_test_out");
  let cmd = build_render_fixtures_command(
    &repo_root,
    true,
    &fixtures_dir,
    &out_dir,
    "example.com",
    1,
    (1040, 1240),
    1.0,
    "screen",
    60,
    false,
    false,
  );

  let args = cmd_args(&cmd);
  let expected_exe = render_fixtures_executable(&repo_root, true)
    .to_string_lossy()
    .into_owned();
  assert!(
    args.iter().any(|arg| arg == &expected_exe),
    "expected debug render_fixtures executable {expected_exe} in args, got {args:?}"
  );
}
