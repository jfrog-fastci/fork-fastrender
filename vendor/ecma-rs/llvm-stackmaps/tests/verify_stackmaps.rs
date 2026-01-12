use std::path::PathBuf;
use std::process::Command;

use llvm_stackmaps::verify::{verify_stackmaps_bytes, VerifyOptions};

const TWO_STATEPOINTS: &[u8] = include_bytes!("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin");

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join(rel)
}

#[test]
fn verify_fixture_succeeds_and_json_is_deterministic() {
    let report = verify_stackmaps_bytes(TWO_STATEPOINTS, VerifyOptions::default());
    assert!(report.ok(), "unexpected failures: {:#?}", report.failures);

    assert_eq!(report.records, 2);
    assert_eq!(report.callsites, 2);
    assert_eq!(report.decoded_statepoints, 2);

    let json1 = report.to_json();
    let json2 = report.to_json();
    assert_eq!(json1, json2);
    assert!(json1.contains("\"ok\":true"));
}

#[test]
fn verify_binary_outputs_same_json_as_library() {
    let report = verify_stackmaps_bytes(TWO_STATEPOINTS, VerifyOptions::default());
    assert!(report.ok(), "unexpected failures: {:#?}", report.failures);

    let path = fixture_path("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin");
    let exe = env!("CARGO_BIN_EXE_verify_stackmaps");
    let out = Command::new(exe)
        .arg("--raw")
        .arg(&path)
        .output()
        .expect("run verify_stackmaps");
    assert!(
        out.status.success(),
        "verify_stackmaps failed (status={})\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert_eq!(stdout, report.to_json());
}

#[test]
fn verify_corrupt_input_returns_structured_error_and_does_not_panic() {
    let mut bad = TWO_STATEPOINTS.to_vec();
    bad.truncate(10);

    let report = verify_stackmaps_bytes(&bad, VerifyOptions::default());
    assert!(!report.ok());
    assert!(
        report.failures.iter().any(|f| f.kind == "parse_error"),
        "expected parse_error failure, got: {:#?}",
        report.failures
    );

    let tmp = tempfile::NamedTempFile::new().expect("create tempfile");
    std::fs::write(tmp.path(), &bad).expect("write tempfile");

    let exe = env!("CARGO_BIN_EXE_verify_stackmaps");
    let out = Command::new(exe)
        .arg("--raw")
        .arg(tmp.path())
        .output()
        .expect("run verify_stackmaps");
    assert!(
        !out.status.success(),
        "expected non-zero exit status for corrupt input"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"ok\":false"),
        "expected ok=false in JSON output, got: {stdout}"
    );
}

