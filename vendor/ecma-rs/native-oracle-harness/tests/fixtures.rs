use diagnostics::files::SimpleFiles;
use diagnostics::render::render_diagnostic;
use native_oracle_harness::run_fixture_ts_with_name;
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/native_oracle")
}

fn parse_expected(source: &str, path: &Path) -> Option<String> {
  for line in source.lines() {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix("// EXPECT:") {
      return Some(rest.trim().to_string());
    }
  }

  let out_path = path.with_extension("out");
  fs::read_to_string(out_path).ok().map(|s| s.trim_end().to_string())
}

#[test]
fn native_oracle_fixtures_pass() {
  let mut paths = Vec::new();
  for entry in fs::read_dir(fixture_dir()).expect("read fixture dir") {
    let entry = entry.expect("fixture dir entry");
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    match path.extension().and_then(|e| e.to_str()) {
      Some("ts") | Some("tsx") => paths.push(path),
      _ => {}
    }
  }
  paths.sort();

  assert!(!paths.is_empty(), "expected at least one fixture");

  let mut ran = 0usize;
  for path in paths {
    let src = fs::read_to_string(&path).expect("read fixture");
    let Some(expected) = parse_expected(&src, &path) else {
      // Some fixtures are intended only for TS→JS erasure coverage (or are executed via other
      // tests, e.g. Promise-returning `.js` fixtures). Only fixtures that declare an expected
      // output participate in oracle output comparison.
      continue;
    };

    let actual = match run_fixture_ts_with_name(&path.to_string_lossy(), &src) {
      Ok(v) => v,
      Err(diag) => {
        let mut files = SimpleFiles::new();
        files.add(path.to_string_lossy(), src);
        let rendered = render_diagnostic(&files, &diag);
        panic!("fixture {} failed:\n{rendered}", path.display());
      }
    };

    assert_eq!(actual, expected, "fixture {}", path.display());
    ran += 1;
  }

  assert!(ran > 0, "expected at least one fixture with // EXPECT:");
}
