use std::path::PathBuf;
use std::process::Command;

use llvm_stackmaps::StackMaps;
use llvm_stackmaps::verify::{verify_stackmaps_bytes, VerifyOptions};

const TWO_STATEPOINTS: &[u8] = include_bytes!("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin");

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join(rel)
}

fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 slice"))
}

fn first_record_location_kind_offset(bytes: &[u8]) -> usize {
    // StackMap v3 header:
    //   u8 version
    //   u8 reserved0
    //   u16 reserved1
    //   u32 num_functions
    //   u32 num_constants
    //   u32 num_records
    assert!(bytes.len() >= 16);
    assert_eq!(bytes[0], 3, "fixture must start with StackMap v3 blob");

    let num_functions = u32_le(bytes, 4) as usize;
    let num_constants = u32_le(bytes, 8) as usize;

    let function_table_end = 16 + num_functions * 24;
    let constants_end = function_table_end + num_constants * 8;
    let first_record = constants_end;

    // Record header is 16 bytes; first location entry starts immediately after.
    first_record + 16
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

#[test]
fn verify_enriches_parse_error_with_inferred_pc_when_possible() {
    let maps = StackMaps::parse(TWO_STATEPOINTS).expect("parse fixture");
    let expected = maps.callsites()[0];

    // Corrupt the first location kind so `StackMaps::parse` fails with an unknown kind. The
    // verifier's offset scanner does not interpret location kinds, so it can still infer the
    // callsite PC and enrich the parse error.
    let mut bad = TWO_STATEPOINTS.to_vec();
    let kind_off = first_record_location_kind_offset(TWO_STATEPOINTS);
    bad[kind_off] = 0xFF;

    let report = verify_stackmaps_bytes(&bad, VerifyOptions::default());
    assert!(!report.ok());

    let failure = report
        .failures
        .iter()
        .find(|f| f.kind == "parse_error")
        .expect("expected parse_error failure");
    assert_eq!(failure.pc, Some(expected.pc));
    assert_eq!(failure.function_address, Some(expected.function_address));
    assert_eq!(failure.record_index, Some(expected.record_index));
}

fn build_minimal_blob(func_addr: u64, rec_id: u64, inst_off: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[3, 0, 0, 0]);
    bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
    bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
    bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

    bytes.extend_from_slice(&(func_addr).to_le_bytes());
    bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
    bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

    bytes.extend_from_slice(&(rec_id).to_le_bytes());
    bytes.extend_from_slice(&(inst_off).to_le_bytes());
    bytes.extend_from_slice(&(0u16).to_le_bytes());
    bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

    // Live-out header (already aligned): padding + num_liveouts.
    bytes.extend_from_slice(&(0u16).to_le_bytes());
    bytes.extend_from_slice(&(0u16).to_le_bytes());
    // Align record end: 16 + 4 = 20 => +4.
    bytes.extend_from_slice(&[0u8; 4]);

    bytes
}

fn build_constindex_blob(func_addr: u64, rec_id: u64, inst_off: u32, constant: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[3, 0, 0, 0]);
    bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
    bytes.extend_from_slice(&(1u32).to_le_bytes()); // num constants
    bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

    // Function entry: 1 record.
    bytes.extend_from_slice(&(func_addr).to_le_bytes());
    bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
    bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

    // Constants table.
    bytes.extend_from_slice(&(constant).to_le_bytes());

    // Record header with num_locations=1.
    bytes.extend_from_slice(&(rec_id).to_le_bytes());
    bytes.extend_from_slice(&(inst_off).to_le_bytes());
    bytes.extend_from_slice(&(0u16).to_le_bytes());
    bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

    // Location: ConstantIndex(0)
    bytes.push(5); // kind
    bytes.push(0); // reserved0
    bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
    bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
    bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
    bytes.extend_from_slice(&(0i32).to_le_bytes()); // constants[0]

    // Align to 8 before live-out header: record header (16) + loc (12) = 28 => +4.
    bytes.extend_from_slice(&[0u8; 4]);

    // Live-out header: padding + num_liveouts=0.
    bytes.extend_from_slice(&(0u16).to_le_bytes());
    bytes.extend_from_slice(&(0u16).to_le_bytes());

    // Align record end: 32 + 4 = 36 => +4.
    bytes.extend_from_slice(&[0u8; 4]);

    bytes
}

#[test]
fn verifier_accepts_identical_duplicate_callsite_pcs() {
    let blob_a = build_minimal_blob(0x1000, 1, 0x10);
    let blob_b = build_minimal_blob(0x1000, 1, 0x10);

    let mut section = Vec::new();
    section.extend_from_slice(&blob_a);
    section.extend_from_slice(&blob_b);

    let report = verify_stackmaps_bytes(&section, VerifyOptions::default());
    assert!(report.ok(), "unexpected failures: {:#?}", report.failures);
}

#[test]
fn verifier_accepts_duplicate_callsite_pcs_even_when_constindex_indices_differ() {
    let blob_a = build_constindex_blob(0x1000, 1, 0x10, 0x1122_3344_5566_7788);
    let blob_b = build_constindex_blob(0x1000, 1, 0x10, 0x1122_3344_5566_7788);

    let mut section = Vec::new();
    section.extend_from_slice(&blob_a);
    section.extend_from_slice(&blob_b);

    let report = verify_stackmaps_bytes(&section, VerifyOptions::default());
    assert!(report.ok(), "unexpected failures: {:#?}", report.failures);
}
