use std::path::PathBuf;
use std::process::Command;

use llvm_stackmaps::verify::{verify_stackmaps_bytes, VerifyOptions};

#[test]
fn corrupt_fixture_fails_with_specific_verifier_error() {
    let bytes = include_bytes!("fixtures/corrupt_bad_base_reg.stackmaps.bin");
    let report = verify_stackmaps_bytes(bytes, VerifyOptions::default());
    assert!(!report.ok(), "corrupt fixture unexpectedly passed");

    assert!(
        report
            .failures
            .iter()
            .any(|f| f.kind == "gc_root_unsupported_base_reg"),
        "expected gc_root_unsupported_base_reg failure, got: {:#?}",
        report.failures
    );

    // Ensure the CLI does not panic and exits non-zero, while still producing JSON output.
    let exe = env!("CARGO_BIN_EXE_verify_stackmaps");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/corrupt_bad_base_reg.stackmaps.bin");

    let out = Command::new(exe)
        .arg("--raw")
        .arg(&path)
        .output()
        .expect("run verify_stackmaps");

    assert!(
        !out.status.success(),
        "expected non-zero exit status for corrupt fixture"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"ok\":false"), "expected ok=false JSON: {stdout}");
    assert!(
        stdout.contains("\"kind\":\"gc_root_unsupported_base_reg\""),
        "expected failure kind in JSON: {stdout}"
    );
}

