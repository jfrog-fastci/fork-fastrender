use std::process::Command;

#[test]
fn font_coverage_rejects_missing_input_source() {
  let output = Command::new(env!("CARGO_BIN_EXE_font_coverage"))
    .output()
    .expect("run font_coverage with no args");

  assert!(
    !output.status.success(),
    "expected failure for missing args, got status {:?}",
    output.status.code()
  );

  // Clap (and our defensive validation) should treat argument validation as a usage error (exit 2).
  assert_eq!(
    output.status.code(),
    Some(2),
    "expected exit code 2 for usage error, stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("--text") && stderr.contains("--html-file"),
    "expected stderr to mention both input flags, got:\n{stderr}"
  );
  assert!(
    !stderr.contains("panicked"),
    "expected no panic output, got:\n{stderr}"
  );
}

#[test]
fn font_coverage_rejects_conflicting_input_sources() {
  let output = Command::new(env!("CARGO_BIN_EXE_font_coverage"))
    .args(["--text", "hi", "--html-file", "dummy.html"])
    .output()
    .expect("run font_coverage with conflicting args");

  assert!(
    !output.status.success(),
    "expected failure for conflicting args, got status {:?}",
    output.status.code()
  );

  assert_eq!(
    output.status.code(),
    Some(2),
    "expected exit code 2 for usage error, stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("--text") && stderr.contains("--html-file"),
    "expected stderr to mention both input flags, got:\n{stderr}"
  );
  assert!(
    !stderr.contains("panicked"),
    "expected no panic output, got:\n{stderr}"
  );
}

