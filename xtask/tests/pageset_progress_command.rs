use std::collections::BTreeMap;

use xtask::{build_pageset_progress_run_command, PagesetFontMode};

fn command_env_map(cmd: &std::process::Command) -> BTreeMap<String, Option<String>> {
  cmd
    .get_envs()
    .map(|(k, v)| {
      (
        k.to_string_lossy().into_owned(),
        v.map(|v| v.to_string_lossy().into_owned()),
      )
    })
    .collect()
}

fn command_args(cmd: &std::process::Command) -> Vec<String> {
  cmd
    .get_args()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect()
}

#[test]
fn pageset_progress_command_system_fonts_overrides_env() {
  let cmd = build_pageset_progress_run_command(false, 2, 5, PagesetFontMode::System);
  let envs = command_env_map(&cmd);

  assert_eq!(
    envs.get("FASTR_USE_BUNDLED_FONTS"),
    Some(&Some("0".to_string())),
    "system-fonts mode should force bundled-font env vars off"
  );
  assert_eq!(
    envs.get("CI"),
    Some(&Some("0".to_string())),
    "system-fonts mode should force CI off so FontConfig::default uses system fonts"
  );

  let args = command_args(&cmd);
  assert!(
    !args.iter().any(|arg| arg == "--bundled-fonts"),
    "system-fonts mode should not pass --bundled-fonts to pageset_progress"
  );
}

#[test]
fn pageset_progress_command_bundled_fonts_adds_flag_without_env_overrides() {
  let cmd = build_pageset_progress_run_command(true, 1, 7, PagesetFontMode::Bundled);
  let envs = command_env_map(&cmd);
  assert!(
    !envs.contains_key("FASTR_USE_BUNDLED_FONTS") && !envs.contains_key("CI"),
    "bundled-fonts mode should not override the environment"
  );

  let args = command_args(&cmd);
  assert!(
    args.iter().any(|arg| arg == "--bundled-fonts"),
    "bundled-fonts mode should pass --bundled-fonts to pageset_progress"
  );
}
