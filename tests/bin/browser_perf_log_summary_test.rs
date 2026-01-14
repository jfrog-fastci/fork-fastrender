use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn json_output_parses_and_contains_expected_keys() {
  let input = r#"
{"event":"frame","ui_frame_ms":10.0,"extra_field":"ok"}
{"event":"frame","ui_frame_ms":20.0}
{"event":"ttfp","ttfp_ms":100.0}
{"event":"input","input_kind":"mouse_wheel","input_to_present_ms":5.0}
{"event":"input","input_kind":"keyboard","input_to_present_ms":11.0}
{"event":"resize","resize_to_present_ms":7.0}
{"event":"cpu_summary","cpu_percent_recent":10.0}
{"event":"frame_upload","upload_total_ms":3.0,"upload_last_ms":3.0,"overwritten_frames":2}

{"type":"ui_frame_time","frame_time_ms":30.0,"ts_ms":3}
{"type":"resource_sample","cpu_percent":20.0,"rss_bytes":2000,"unknown":true}
{"event":"memory_summary","rss_bytes":3000,"rss_mb":0.0}
{"type":"future_event","foo":1,"bar":"baz"}
"#;

  let mut child = Command::new(env!("CARGO_BIN_EXE_browser_perf_log_summary"))
    .arg("--json")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn browser_perf_log_summary");

  {
    let mut stdin = child.stdin.take().expect("stdin available");
    stdin.write_all(input.as_bytes()).expect("write stdin");
  }

  let output = child
    .wait_with_output()
    .expect("wait for browser_perf_log_summary");

  assert!(
    output.status.success(),
    "expected exit code 0; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let value: serde_json::Value =
    serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");

  for key in [
    "meta",
    "ui_frame_time_ms",
    "ttfp_ms",
    "scroll_latency_ms",
    "resize_latency_ms",
    "input_latency_ms",
    "tab_switch_latency_ms",
    "upload_total_ms",
    "upload_last_ms",
    "coalesced_frames",
    "cpu_percent",
    "rss_bytes",
    "rss_mb",
    "rss_first_mb",
    "rss_last_mb",
    "rss_delta_mb",
  ] {
    assert!(
      value.get(key).is_some(),
      "expected JSON output to contain key {key}; got:\n{}",
      String::from_utf8_lossy(&output.stdout)
    );
  }

  // Ensure `memory_summary` RSS samples (schema v2) are included in RSS aggregation (alongside the
  // legacy `resource_sample` events).
  let rss_bytes = &value["rss_bytes"];
  assert_eq!(rss_bytes["count"].as_u64(), Some(2));
  assert_eq!(rss_bytes["min"].as_u64(), Some(2000));
  assert_eq!(rss_bytes["max"].as_u64(), Some(3000));
  assert_eq!(rss_bytes["mean"].as_f64(), Some(2500.0));
}
