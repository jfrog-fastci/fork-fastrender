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

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut paths = Vec::new();
  for entry in fs::read_dir(fixture_dir())? {
    let entry = entry?;
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

  let mut failures = 0usize;
  for path in paths {
    let src = fs::read_to_string(&path)?;
    let Some(expected) = parse_expected(&src, &path) else {
      eprintln!("SKIP {} (no // EXPECT: and no .out)", path.display());
      continue;
    };
    let actual = match run_fixture_ts_with_name(&path.to_string_lossy(), &src) {
      Ok(v) => v,
      Err(diag) => {
        failures += 1;
        eprintln!("FAIL {} (diagnostic): {diag:?}", path.display());
        continue;
      }
    };

    if actual == expected {
      println!("ok {}", path.display());
    } else {
      failures += 1;
      println!(
        "FAIL {} expected={expected:?} actual={actual:?}",
        path.display()
      );
    }
  }

  if failures > 0 {
    Err(format!("{failures} fixture(s) failed").into())
  } else {
    Ok(())
  }
}

