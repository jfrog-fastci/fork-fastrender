use typecheck_ts_harness::triage;
use typecheck_ts_harness::expectations::ExpectationKind;
use typecheck_ts_harness::Expectations;

#[test]
fn emit_manifest_includes_tracking_issue_for_non_pass_only() {
  let report = triage::analyze_report_json_str(
    r#"
{
  "compare_mode": "none",
  "results": [
    {
      "id": "mismatch.ts",
      "outcome": "rust_extra_diagnostics",
      "rust": { "status": "ok", "diagnostics": [{ "engine": "rust", "code": "TS1234", "file": "/mismatch.ts", "start": 0, "end": 1 }] },
      "tsc": { "status": "ok", "diagnostics": [] },
      "expectation": { "expectation": "pass", "expected": false }
    },
    {
      "id": "xpass.ts",
      "outcome": "match",
      "rust": { "status": "ok", "diagnostics": [] },
      "tsc": { "status": "ok", "diagnostics": [] },
      "expectation": { "expectation": "xfail", "expected": false, "from_manifest": true }
    }
  ]
}
"#,
    10,
  )
  .expect("analyze report");

  let mut buf = Vec::new();
  triage::print_manifest_suggestions_toml(&report, &mut buf).expect("emit manifest");
  let emitted = String::from_utf8(buf).expect("utf8");

  let expectations = Expectations::from_str(&emitted).expect("snippet parses as manifest");

  let mismatch = expectations.lookup("mismatch.ts");
  assert_eq!(mismatch.expectation.kind, ExpectationKind::Xfail);
  assert_eq!(mismatch.expectation.tracking_issue.as_deref(), Some("TODO"));

  let xpass = expectations.lookup("xpass.ts");
  assert_eq!(xpass.expectation.kind, ExpectationKind::Pass);
  assert_eq!(xpass.expectation.tracking_issue.as_deref(), None);
}

