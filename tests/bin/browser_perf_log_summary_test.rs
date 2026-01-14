use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn json_output_parses_and_contains_expected_keys() {
  let input = r#"
{"event":"run_start","t_ms":0,"rss_bytes":1000}
{"event":"frame","ui_frame_ms":10.0,"worker_msgs_ms":1.0,"upload_ms":2.0,"egui_ms":3.0,"tessellate_ms":1.0,"wgpu_ms":2.0,"present_ms":4.0,"total_ms":15.0,"extra_field":"ok"}
{"event":"frame","ui_frame_ms":20.0,"worker_msgs_ms":2.0,"upload_ms":4.0,"egui_ms":6.0,"tessellate_ms":2.0,"wgpu_ms":4.0,"present_ms":8.0,"total_ms":30.0}
{"event":"ttfp","ttfp_ms":100.0}
{"event":"input","input_kind":"mouse_wheel","input_to_present_ms":5.0}
{"event":"input","input_kind":"keyboard","input_to_present_ms":11.0}
{"event":"resize","resize_to_present_ms":7.0}
{"event":"cpu_summary","cpu_percent_recent":10.0}
{"event":"frame_upload","upload_total_ms":3.0,"upload_last_ms":3.0,"overwritten_frames":2}
{"event":"worker_wake_summary","worker_msgs_forwarded_per_sec":10.0,"worker_msgs_processed_per_sec":9.0,"worker_wakes_handled_per_sec":2.0,"worker_wake_events_sent_per_sec":1.0,"worker_wake_events_coalesced_per_sec":99.0,"worker_followup_wakes_per_sec":0.5,"worker_empty_wakes_per_sec":0.25,"worker_pending_msgs_estimate":12,"worker_msgs_per_nonempty_wake":4.0,"worker_last_drain":8,"worker_max_drain":16}
{"event":"worker_wake_summary","worker_msgs_forwarded_per_sec":20.0,"worker_msgs_processed_per_sec":18.0,"worker_wakes_handled_per_sec":4.0,"worker_wake_events_sent_per_sec":2.0,"worker_wake_events_coalesced_per_sec":50.0,"worker_followup_wakes_per_sec":1.0,"worker_empty_wakes_per_sec":0.0,"worker_pending_msgs_estimate":6,"worker_msgs_per_nonempty_wake":5.0,"worker_last_drain":10,"worker_max_drain":20}

{"type":"ui_frame_time","frame_time_ms":30.0,"ts_ms":3}
{"type":"resource_sample","cpu_percent":20.0,"rss_bytes":2000,"unknown":true}
{"event":"memory_summary","rss_bytes":3000,"rss_mb":0.0}
{"event":"run_end","t_ms":1000,"rss_bytes":4000}
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
    "ui_frame_worker_msgs_ms",
    "ui_frame_upload_ms",
    "ui_frame_egui_ms",
    "ui_frame_tessellate_ms",
    "ui_frame_wgpu_ms",
    "ui_frame_present_ms",
    "ui_frame_cpu_ms",
    "ttfp_ms",
    "scroll_latency_ms",
    "resize_latency_ms",
    "input_latency_ms",
    "tab_switch_latency_ms",
    "upload_total_ms",
    "upload_last_ms",
    "coalesced_frames",
    "cpu_percent",
    "worker_msgs_forwarded_per_sec",
    "worker_wake_events_coalesced_per_sec",
    "worker_pending_msgs_estimate",
    "worker_last_drain",
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
  assert_eq!(rss_bytes["count"].as_u64(), Some(4));
  assert_eq!(rss_bytes["min"].as_u64(), Some(1000));
  assert_eq!(rss_bytes["max"].as_u64(), Some(4000));
  assert_eq!(rss_bytes["mean"].as_f64(), Some(2500.0));

  let ui_frame_worker_msgs_ms = &value["ui_frame_worker_msgs_ms"];
  assert_eq!(ui_frame_worker_msgs_ms["count"].as_u64(), Some(2));
  assert_eq!(ui_frame_worker_msgs_ms["mean"].as_f64(), Some(1.5));
  assert_eq!(ui_frame_worker_msgs_ms["p50"].as_f64(), Some(1.5));
  assert_eq!(ui_frame_worker_msgs_ms["p95"].as_f64(), Some(1.95));
  assert_eq!(ui_frame_worker_msgs_ms["max"].as_f64(), Some(2.0));

  let ui_frame_cpu_ms = &value["ui_frame_cpu_ms"];
  assert_eq!(ui_frame_cpu_ms["count"].as_u64(), Some(2));
  assert_eq!(ui_frame_cpu_ms["mean"].as_f64(), Some(22.5));
  assert_eq!(ui_frame_cpu_ms["p50"].as_f64(), Some(22.5));
  assert_eq!(ui_frame_cpu_ms["p95"].as_f64(), Some(29.25));
  assert_eq!(ui_frame_cpu_ms["max"].as_f64(), Some(30.0));

  let worker_msgs_forwarded = &value["worker_msgs_forwarded_per_sec"];
  assert_eq!(worker_msgs_forwarded["count"].as_u64(), Some(2));
  assert_eq!(worker_msgs_forwarded["min"].as_f64(), Some(10.0));
  assert_eq!(worker_msgs_forwarded["max"].as_f64(), Some(20.0));
  assert_eq!(worker_msgs_forwarded["mean"].as_f64(), Some(15.0));

  let worker_pending = &value["worker_pending_msgs_estimate"];
  assert_eq!(worker_pending["count"].as_u64(), Some(2));
  assert_eq!(worker_pending["min"].as_f64(), Some(6.0));
  assert_eq!(worker_pending["max"].as_f64(), Some(12.0));
  assert_eq!(worker_pending["mean"].as_f64(), Some(9.0));

  let worker_last_drain = &value["worker_last_drain"];
  assert_eq!(worker_last_drain["count"].as_u64(), Some(2));
  assert_eq!(worker_last_drain["min"].as_f64(), Some(8.0));
  assert_eq!(worker_last_drain["max"].as_f64(), Some(10.0));
  assert_eq!(worker_last_drain["mean"].as_f64(), Some(9.0));
}
