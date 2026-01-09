use std::fs;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn pageset_triage_markdown_is_deterministic() {
  let tmp = tempdir().expect("create tempdir");
  let progress_dir = tmp.path().join("progress_pages");
  fs::create_dir_all(&progress_dir).expect("create progress dir");

  fs::write(
    progress_dir.join("a.com.json"),
    r#"{
  "url": "https://a.com/",
  "status": "ok",
  "hotspot": "layout",
  "total_ms": 100.0
}
"#,
  )
  .expect("write progress a.com");

  fs::write(
    progress_dir.join("b.com.json"),
    r#"{
  "url": "https://b.com/",
  "status": "ok",
  "hotspot": "paint",
  "total_ms": 200.0,
  "accuracy": {
    "diff_percent": 50.0,
    "perceptual": 0.5
  }
}
"#,
  )
  .expect("write progress b.com");

  fs::write(
    progress_dir.join("c.com.json"),
    r#"{
  "url": "https://c.com/",
  "status": "error",
  "hotspot": "fetch",
  "total_ms": 300.0,
  "auto_notes": "missing cache"
}
"#,
  )
  .expect("write progress c.com");

  let report_path = tmp.path().join("report.json");
  fs::write(
    &report_path,
    r#"{
  "results": [
    {
      "name": "b.com",
      "status": "diff",
      "before": "chrome/b.com.png",
      "after": "fastrender/b.com.png",
      "diff": "report_files/diffs/b.com.png",
      "metrics": {
        "diff_percentage": 50.0,
        "perceptual_distance": 0.5
      }
    },
    {
      "name": "c.com",
      "status": "error",
      "before": "chrome/c.com.png",
      "after": "fastrender/c.com.png",
      "error": "boom"
    }
  ]
}
"#,
  )
  .expect("write report.json");

  let out_path = tmp.path().join("report.md");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "pageset-triage",
      "--progress-dir",
      progress_dir.to_str().unwrap(),
      "--report",
      report_path.to_str().unwrap(),
      "--top-worst-accuracy",
      "1",
      "--top-slowest",
      "1",
      "--out",
      out_path.to_str().unwrap(),
    ])
    .output()
    .expect("run cargo xtask pageset-triage");

  assert!(
    output.status.success(),
    "pageset-triage should exit successfully; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let report = fs::read_to_string(&out_path).expect("read report.md");

  let expected = r#"# Pageset triage report

This is an editable template. Fill in the **Brokenness inventory** section for each page.

Pages: 2

## b.com

- URL: https://b.com/
- Fixture: `tests/pages/fixtures/b.com/index.html`
- Progress: status=ok hotspot=paint total_ms=200.00
- Accuracy: diff_percent=50.0000% perceptual=0.5000
- Diff report: status=diff (`report.html#entry-71f72aa5a9d6bd42`)
  - before: `chrome/b.com.png`
  - after: `fastrender/b.com.png`
  - diff: `report_files/diffs/b.com.png`

### Brokenness inventory
- Layout:
  - [ ] ...
- Text:
  - [ ] ...
- Paint:
  - [ ] ...
- Resources:
  - [ ] ...

## c.com

- URL: https://c.com/
- Fixture: `tests/pages/fixtures/c.com/index.html`
- Progress: status=error hotspot=fetch total_ms=300.00
- Auto notes: missing cache
- Diff report: status=error (`report.html#entry-cb54b39b547bf659`)
  - before: `chrome/c.com.png`
  - after: `fastrender/c.com.png`
  - error: boom

### Brokenness inventory
- Layout:
  - [ ] ...
- Text:
  - [ ] ...
- Paint:
  - [ ] ...
- Resources:
  - [ ] ...
"#;

  assert_eq!(report, expected);
}

#[test]
fn pageset_triage_includes_first_mismatch_when_present() {
  let tmp = tempdir().expect("create tempdir");
  let progress_dir = tmp.path().join("progress_pages");
  fs::create_dir_all(&progress_dir).expect("create progress dir");

  fs::write(
    progress_dir.join("a.com.json"),
    r#"{
  "url": "https://a.com/",
  "status": "ok",
  "hotspot": "paint",
  "total_ms": 123.0,
  "accuracy": {
    "diff_percent": 12.5,
    "perceptual": 0.25,
    "first_mismatch": {
      "x": 1,
      "y": 2,
      "baseline_rgba": [1, 2, 3, 4],
      "rendered_rgba": [250, 251, 252, 253]
    }
  }
}
"#,
  )
  .expect("write progress a.com");

  fs::write(
    progress_dir.join("b.com.json"),
    r#"{
  "url": "https://b.com/",
  "status": "ok",
  "hotspot": "paint",
  "total_ms": 456.0
}
"#,
  )
  .expect("write progress b.com");

  let report_path = tmp.path().join("report.json");
  fs::write(
    &report_path,
    r#"{
  "results": [
    {
      "name": "a.com",
      "status": "diff",
      "metrics": {
        "diff_percentage": 12.5,
        "perceptual_distance": 0.25,
        "first_mismatch": {
          "x": 99,
          "y": 88,
          "before_rgba": [0, 0, 0, 0],
          "after_rgba": [255, 255, 255, 255]
        }
      }
    },
    {
      "name": "b.com",
      "status": "diff",
      "metrics": {
        "diff_percentage": 55.0,
        "perceptual_distance": 0.75,
        "first_mismatch": {
          "x": 10,
          "y": 20,
          "before_rgba": [5, 6, 7, 8],
          "after_rgba": [9, 10, 11, 12]
        }
      }
    }
  ]
}
"#,
  )
  .expect("write report.json");

  let out_path = tmp.path().join("report.md");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "pageset-triage",
      "--progress-dir",
      progress_dir.to_str().unwrap(),
      "--report",
      report_path.to_str().unwrap(),
      "--only",
      "a.com,b.com",
      "--out",
      out_path.to_str().unwrap(),
    ])
    .output()
    .expect("run cargo xtask pageset-triage");

  assert!(
    output.status.success(),
    "pageset-triage should exit successfully; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let report = fs::read_to_string(&out_path).expect("read report.md");

  // When progress JSON contains `accuracy.first_mismatch`, we should prefer it over the diff report.
  assert!(
    report.contains(
      "  - first_mismatch: (1, 2) baseline_rgba=[1, 2, 3, 4] rendered_rgba=[250, 251, 252, 253]\n"
    ),
    "expected progress first_mismatch to be rendered.\nreport:\n{report}"
  );

  // When progress JSON has no accuracy block, fall back to diff report metrics (including first_mismatch).
  assert!(
    report.contains(
      "  - first_mismatch: (10, 20) baseline_rgba=[5, 6, 7, 8] rendered_rgba=[9, 10, 11, 12]\n"
    ),
    "expected diff report first_mismatch to be rendered.\nreport:\n{report}"
  );
}
