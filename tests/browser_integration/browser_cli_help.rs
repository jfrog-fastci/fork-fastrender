#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

fn find_log_style_token_with_colon(help: &str) -> Option<String> {
  fn scan_token(help: &str, needle: &str, is_token_char: fn(u8) -> bool) -> Option<String> {
    let bytes = help.as_bytes();
    for (start, _) in help.match_indices(needle) {
      let mut end = start + needle.len();
      while end < bytes.len() && is_token_char(bytes[end]) {
        end += 1;
      }
      if end < bytes.len() && bytes[end] == b':' {
        return Some(help[start..=end].to_string());
      }
    }
    None
  }

  scan_token(help, "FASTR_", |b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    .or_else(|| {
      scan_token(help, "--", |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

fn extract_long_flags(help: &str) -> Vec<String> {
  let mut out = Vec::new();
  for token in help.split_whitespace() {
    let Some(rest) = token.strip_prefix("--") else {
      continue;
    };

    let mut flag = String::from("--");
    for ch in rest.chars() {
      if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
        flag.push(ch);
      } else {
        break;
      }
    }
    if flag.len() > 2 {
      out.push(flag);
    }
  }
  out.sort();
  out.dedup();
  out
}

#[test]
fn browser_help_exits_successfully_without_startup_logs() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--help")
    // Set invalid values for crash/watchdog env vars so this test will catch regressions where
    // startup/env parsing happens *before* clap prints help + exits.
    .env("FASTR_BROWSER_ALLOW_CRASH_URLS", "maybe")
    .env("FASTR_BROWSER_RENDERER_WATCHDOG", "maybe")
    .env("FASTR_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS", "wat")
    .output()
    .expect("spawn browser --help");

  assert!(
    output.status.success(),
    "browser --help exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  // clap writes help to stdout; other early errors/warnings (if any) tend to go to stderr.
  // Concatenate both streams so failures remain diagnosable.
  let help = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );
  assert!(
    help.contains("Usage:"),
    "expected help usage in output, got:\n{help}"
  );
  assert!(
    help.contains("Supported schemes:"),
    "expected help to mention supported schemes, got:\n{help}"
  );
  assert!(
    help.contains("attempts to restore the previous session"),
    "expected help to mention session restore startup behaviour, got:\n{help}"
  );
  assert!(
    help.contains("Use `--restore`"),
    "expected help to mention how to override session restore behaviour, got:\n{help}"
  );
  for flag in [
    "--hud",
    "--no-hud",
    "--restore",
    "--no-restore",
    "--session-path",
    "--download-dir",
    "--mem-limit-mb",
    "--perf-log",
    "--perf-log-out",
    "--trace-out",
    "--power-preference",
    "--force-fallback-adapter",
    "--wgpu-backends",
    "--renderer-watchdog",
    "--no-renderer-watchdog",
    "--renderer-watchdog-timeout-ms",
    "--headless-smoke",
    "--headless-crash-smoke",
    "--exit-immediately",
  ] {
    assert!(
      help.contains(flag),
      "expected help to mention {flag}, got:\n{help}"
    );
  }

  // `--help` should exit early (clap prints help and terminates) before any runtime startup/logging.
  // Historically, this has regressed when new env vars / crash handling is wired up in `run()`.
  if let Some(token) = find_log_style_token_with_colon(&help) {
    panic!("expected --help to exit before startup logging (found {token}), got:\n{help}");
  }

  // Multiprocess/crash-handling flags should remain visible in `--help` output.
  let long_flags = extract_long_flags(&help);
  assert!(
    long_flags.iter().any(|flag| flag.contains("crash-smoke")),
    "expected --help to mention a crash smoke flag, got flags:\n{long_flags:#?}\nfull help:\n{help}"
  );
  assert!(
    long_flags.iter().any(|flag| flag.contains("watchdog")),
    "expected --help to mention a watchdog flag, got flags:\n{long_flags:#?}\nfull help:\n{help}"
  );
}
