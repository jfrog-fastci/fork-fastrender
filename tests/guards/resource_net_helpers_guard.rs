//! Guard that prevents ad-hoc localhost bind helpers from being reintroduced into resource tests.
//!
//! Resource integration tests often spin up local TCP servers. Binding localhost can fail in some
//! CI/sandboxed environments, so these tests should use the shared
//! [`crate::common::net::try_bind_localhost`] helper which prints a consistent skip message and
//! returns `None` instead of panicking.
//!
//! The only exception is `tests/resource/http_www_fallback_test.rs`, which needs custom dual-stack
//! binding logic to validate `localhost`/`www.localhost` fallback behaviour.

use std::fs;
use std::path::PathBuf;

use regex::Regex;
use walkdir::WalkDir;

#[test]
fn resource_tests_do_not_define_try_bind_localhost_helpers() {
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let resource_dir = root.join("tests").join("resource");
  assert!(
    resource_dir.is_dir(),
    "expected resource test dir to exist: {}",
    resource_dir.display()
  );

  let fn_decl =
    Regex::new(r"(?m)^\s*fn\s+try_bind_localhost\b").expect("regex should compile");

  let mut offenders = Vec::new();
  for entry in WalkDir::new(&resource_dir).into_iter().filter_map(Result::ok) {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }

    let content =
      fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    if fn_decl.is_match(&content) {
      let rel = path.strip_prefix(&root).unwrap_or(path);
      offenders.push(rel.display().to_string());
    }
  }

  offenders.sort();
  assert_eq!(
    offenders,
    vec!["tests/resource/http_www_fallback_test.rs".to_string()],
    "resource tests should use the shared try_bind_localhost helper in crate::common::net (except \
the http_www_fallback_test special-case); found local fn try_bind_localhost definitions in:\n{}",
    offenders.join("\n")
  );
}

