use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Guardrail: this repo standardizes on LLVM's production GC strategy name (`coreclr`) for all
/// statepoint/stackmap fixtures and codegen.
///
/// LLVM also ships a demo strategy (`statepoint-` + `example`). Allowing it to creep back into
/// non-doc fixtures makes it too easy to accidentally generate inconsistent IR across modules.
#[test]
fn statepoint_example_strategy_is_docs_only() {
  let native_js_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let ecma_rs_root = native_js_dir
    .parent()
    .expect("native-js should be nested under vendor/ecma-rs")
    .to_path_buf();

  // Keep the needle split so this test file itself doesn't contain it.
  let needle = [b"statepoint-".as_slice(), b"example".as_slice()].concat();
  let mut offenders = Vec::new();
  scan_dir(&ecma_rs_root, &ecma_rs_root, &needle, &mut offenders);

  let needle_str = String::from_utf8_lossy(&needle);
  assert!(
    offenders.is_empty(),
    "`{}` should only appear in markdown docs; found in:\n{}",
    needle_str,
    offenders.join("\n")
  );
}

fn scan_dir(root: &Path, dir: &Path, needle: &[u8], offenders: &mut Vec<String>) {
  let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read_dir {}: {e}", dir.display()));
  for entry in entries {
    let entry = entry.unwrap_or_else(|e| panic!("failed to read entry in {}: {e}", dir.display()));
    let path = entry.path();
    let name = entry.file_name();

    if path.is_dir() {
      if should_skip_dir(&name) {
        continue;
      }
      scan_dir(root, &path, needle, offenders);
      continue;
    }

    if !path.is_file() || should_skip_file(&path) {
      continue;
    }

    let Ok(bytes) = fs::read(&path) else {
      continue;
    };
    if bytes.windows(needle.len()).any(|w| w == needle) {
      let rel = path.strip_prefix(root).unwrap_or(&path);
      offenders.push(rel.display().to_string());
    }
  }
}

fn should_skip_dir(name: &OsStr) -> bool {
  matches!(
    name.to_str(),
    Some(
      // Build artifacts.
      "target"
        // Huge vendored test suites (not relevant to native GC strategy choices).
        | "test262"
        | "test262-semantic"
        // parse-js has a TypeScript conformance submodule; scanning it is slow and irrelevant.
        | "TypeScript"
        // Misc VCS metadata.
        | ".git"
    )
  )
}

fn should_skip_file(path: &Path) -> bool {
  // Allow the demo strategy name (`statepoint-` + `example`) in markdown docs (where we mention it
  // as an alternative strategy).
  if path.extension().and_then(|e| e.to_str()) == Some("md") {
    return true;
  }

  // Skip obvious binary assets / generated blobs.
  if matches!(
    path.extension().and_then(|e| e.to_str()),
    Some(
      "bin"
        | "o"
        | "a"
        | "so"
        | "dylib"
        | "rlib"
        | "rmeta"
        | "wasm"
        | "png"
        | "jpg"
        | "jpeg"
        | "gif"
        | "webp"
        | "avif"
        | "ttf"
        | "otf"
        | "woff"
        | "woff2"
    )
  ) {
    return true;
  }

  // Be defensive: don't scan very large files (the string we're looking for should only appear
  // in small sources/fixtures).
  if let Ok(meta) = fs::metadata(path) {
    if meta.len() > 2 * 1024 * 1024 {
      return true;
    }
  }

  false
}
