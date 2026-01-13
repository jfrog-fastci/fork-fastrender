use std::fs;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn pageset_triage_markdown_is_deterministic() {
  let tmp = tempdir().expect("create tempdir");
  let progress_dir = tmp.path().join("progress_pages");
  fs::create_dir_all(&progress_dir).expect("create progress dir");

  fs::write(
    progress_dir.join("example.net.json"),
    r#"{
  "url": "https://example.net/",
  "status": "ok",
  "hotspot": "layout",
  "total_ms": 100.0
}
"#,
  )
  .expect("write progress example.net");

  fs::write(
    progress_dir.join("example.com.json"),
    r#"{
  "url": "https://example.com/",
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
  .expect("write progress example.com");

  fs::write(
    progress_dir.join("example.invalid.json"),
    r#"{
  "url": "https://example.invalid/",
  "status": "error",
  "hotspot": "fetch",
  "total_ms": 300.0,
  "auto_notes": "missing cache"
}
"#,
  )
  .expect("write progress example.invalid");

  let report_path = tmp.path().join("report.json");
  fs::write(
    &report_path,
    r#"{
  "results": [
    {
      "name": "example.com",
      "status": "diff",
      "before": "chrome/example.com.png",
      "after": "fastrender/example.com.png",
      "diff": "report_files/diffs/example.com.png",
      "metrics": {
        "diff_percentage": 50.0,
        "perceptual_distance": 0.5
      }
    },
    {
      "name": "example.invalid",
      "status": "error",
      "before": "chrome/example.invalid.png",
      "after": "fastrender/example.invalid.png",
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
      "--only",
      "example.com,example.invalid",
      "--out",
      out_path.to_str().unwrap(),
    ])
    .output()
    .expect("run xtask pageset-triage");

  assert!(
    output.status.success(),
    "pageset-triage should exit successfully; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let report = fs::read_to_string(&out_path).expect("read report.md");

  let expected = r#"# Pageset triage report

This is an editable template. Fill in the **Brokenness inventory** section for each page.

Pages: 2

## Summary

| stem | status | hotspot | total_ms | diff% | perceptual |
| --- | --- | --- | ---: | ---: | ---: |
| example.com | ok | paint | 200.00 | 50.0000% | 0.5000 |
| example.invalid | error | fetch | 300.00 | n/a | n/a |

## example.com

- URL: https://example.com/
- Fixture: OK (`tests/pages/fixtures/example.com/index.html`)
- Progress: status=ok hotspot=paint total_ms=200.00
- Accuracy: diff_percent=50.0000% perceptual=0.5000
- Diff report: status=diff (`report.html#entry-576846634e2714c6`)
  - before: `chrome/example.com.png`
  - after: `fastrender/example.com.png`
  - diff: `report_files/diffs/example.com.png`

### Commands

```bash
bash scripts/cargo_agent.sh xtask page-loop --fixture example.com --viewport 1200x800 --dpr 1.0 --media screen --chrome --overlay --inspect-dump-json --write-snapshot
```

### Brokenness inventory
- Layout:
  - [ ] ...
- Text:
  - [ ] ...
- Paint:
  - [ ] ...
- Resources:
  - [ ] ...

## example.invalid

- URL: https://example.invalid/
- Fixture: MISSING (expected `tests/pages/fixtures/example.invalid/index.html`)
- Progress: status=error hotspot=fetch total_ms=300.00
- Auto notes: missing cache
- Diff report: status=error (`report.html#entry-f81ea3a07d5a5458`)
  - before: `chrome/example.invalid.png`
  - after: `fastrender/example.invalid.png`
  - error: boom

### Commands

```bash
bash scripts/cargo_agent.sh xtask page-loop --pageset https://example.invalid/ --viewport 1200x800 --dpr 1.0 --media screen --chrome --overlay --inspect-dump-json --write-snapshot
```

Capture fixture:

```bash
bash scripts/cargo_agent.sh run --release --bin bundle_page -- fetch https://example.invalid/ --no-render --out target/page-fixture-bundles/example.invalid.tar --viewport 1200x800 --dpr 1.0
bash scripts/cargo_agent.sh xtask import-page-fixture target/page-fixture-bundles/example.invalid.tar example.invalid
bash scripts/cargo_agent.sh xtask validate-page-fixtures --only example.invalid
```

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
    .expect("run xtask pageset-triage");

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

#[test]
fn pageset_triage_top_worst_perceptual_orders_by_perceptual_then_diff_then_stem() {
  let tmp = tempdir().expect("create tempdir");
  let progress_dir = tmp.path().join("progress_pages");
  fs::create_dir_all(&progress_dir).expect("create progress dir");

  let pages = [
    // Highest perceptual should always win.
    ("zeta.com", 0.95, 1.0),
    // Same perceptual: diff% tie-breaks.
    ("gamma.com", 0.9, 3.0),
    // Same perceptual + same diff%: stem tie-breaks.
    ("alpha.com", 0.9, 2.0),
    ("beta.com", 0.9, 2.0),
    // Lower perceptual, even with huge diff%.
    ("delta.com", 0.8, 99.0),
  ];

  for (stem, perceptual, diff_percent) in pages {
    fs::write(
      progress_dir.join(format!("{stem}.json")),
      format!(
        r#"{{
  "url": "https://{stem}/",
  "status": "ok",
  "hotspot": "paint",
  "total_ms": 1.0,
  "accuracy": {{
    "diff_percent": {diff_percent},
    "perceptual": {perceptual}
  }}
}}
"#
      ),
    )
    .unwrap_or_else(|e| panic!("write progress {stem}: {e}"));
  }

  // No accuracy block => excluded from perceptual ranking.
  fs::write(
    progress_dir.join("noacc.com.json"),
    r#"{
  "url": "https://noacc.com/",
  "status": "ok",
  "hotspot": "paint",
  "total_ms": 1.0
}
"#,
  )
  .expect("write progress noacc.com");

  let out_path = tmp.path().join("report.md");
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "pageset-triage",
      "--progress-dir",
      progress_dir.to_str().unwrap(),
      "--top-worst-perceptual",
      "4",
      "--out",
      out_path.to_str().unwrap(),
    ])
    .output()
    .expect("run xtask pageset-triage");

  assert!(
    output.status.success(),
    "pageset-triage should exit successfully; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let report = fs::read_to_string(&out_path).expect("read report.md");

  // Table should list diff% + perceptual for selected rows.
  assert!(
    report.contains("| zeta.com | ok | paint | 1.00 | 1.0000% | 0.9500 |"),
    "expected zeta.com row in summary table.\nreport:\n{report}"
  );

  let idx_zeta = report.find("## zeta.com").expect("zeta.com section");
  let idx_gamma = report.find("## gamma.com").expect("gamma.com section");
  let idx_alpha = report.find("## alpha.com").expect("alpha.com section");
  let idx_beta = report.find("## beta.com").expect("beta.com section");

  assert!(
    idx_zeta < idx_gamma && idx_gamma < idx_alpha && idx_alpha < idx_beta,
    "expected perceptual selection ordering zeta -> gamma -> alpha -> beta.\nreport:\n{report}"
  );

  assert!(
    !report.contains("## noacc.com"),
    "expected pages without perceptual metrics to be excluded.\nreport:\n{report}"
  );
}

#[test]
fn pageset_triage_top_worst_perceptual_out_of_range_does_not_error() {
  let tmp = tempdir().expect("create tempdir");
  let progress_dir = tmp.path().join("progress_pages");
  fs::create_dir_all(&progress_dir).expect("create progress dir");

  for (stem, perceptual) in [("a.com", 0.1), ("b.com", 0.2), ("c.com", 0.3)] {
    fs::write(
      progress_dir.join(format!("{stem}.json")),
      format!(
        r#"{{
  "url": "https://{stem}/",
  "status": "ok",
  "hotspot": "paint",
  "total_ms": 1.0,
  "accuracy": {{
    "diff_percent": 1.0,
    "perceptual": {perceptual}
  }}
}}
"#
      ),
    )
    .unwrap_or_else(|e| panic!("write progress {stem}: {e}"));
  }

  let out_path = tmp.path().join("report.md");
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "pageset-triage",
      "--progress-dir",
      progress_dir.to_str().unwrap(),
      "--top-worst-perceptual",
      "10",
      "--out",
      out_path.to_str().unwrap(),
    ])
    .output()
    .expect("run xtask pageset-triage");

  assert!(
    output.status.success(),
    "pageset-triage should exit successfully; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let report = fs::read_to_string(&out_path).expect("read report.md");
  assert!(
    report.contains("Pages: 3\n"),
    "expected to include all eligible rows even when N is out of range.\nreport:\n{report}"
  );
}
