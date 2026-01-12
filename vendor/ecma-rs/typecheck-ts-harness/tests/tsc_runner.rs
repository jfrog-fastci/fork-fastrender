#![cfg(feature = "with-node")]

mod common;

use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use typecheck_ts_harness::tsc::TscRequest;

#[test]
fn reports_single_file_error() {
  let mut runner = match common::runner_or_skip("tsc runner tests") {
    Some(runner) => runner,
    None => return,
  };

  let name: Arc<str> = Arc::from("main.ts");
  let mut files = HashMap::new();
  files.insert(Arc::clone(&name), Arc::from("const value: string = 1;"));

  let request = TscRequest {
    root_names: vec![name],
    files,
    options: Map::new(),
    diagnostics_only: true,
    trace_resolution: false,
    type_queries: Vec::new(),
  };

  let output = runner.check(request).expect("tsc output");
  assert_eq!(output.diagnostics.len(), 1);
  assert!(
    output.type_facts.is_none(),
    "expected no type facts when diagnostics_only is enabled"
  );
  let diag = &output.diagnostics[0];
  assert_eq!(diag.code, 2322);
  assert_eq!(diag.file.as_deref(), Some("main.ts"));
  assert_eq!((diag.start, diag.end), (6, 11));
}

#[test]
fn resolves_relative_imports_across_files() {
  let mut runner = match common::runner_or_skip("tsc runner tests") {
    Some(runner) => runner,
    None => return,
  };

  let a_name: Arc<str> = Arc::from("a.ts");
  let b_name: Arc<str> = Arc::from("b.ts");
  let mut files = HashMap::new();
  files.insert(
    Arc::clone(&a_name),
    Arc::from("export const value: number = 1;"),
  );
  files.insert(
    Arc::clone(&b_name),
    Arc::from("import { value } from './a';\nconst str: string = value;\n"),
  );

  let request = TscRequest {
    root_names: vec![a_name, b_name],
    files,
    options: Map::new(),
    diagnostics_only: true,
    trace_resolution: false,
    type_queries: Vec::new(),
  };

  let output = runner.check(request).expect("tsc output");
  assert_eq!(output.diagnostics.len(), 1);
  assert!(
    output.type_facts.is_none(),
    "expected no type facts when diagnostics_only is enabled"
  );
  let diag = &output.diagnostics[0];
  assert_eq!(diag.code, 2322);
  assert_eq!(diag.file.as_deref(), Some("b.ts"));
  assert_eq!((diag.start, diag.end), (35, 38));
}

#[test]
fn does_not_read_arbitrary_host_fs() {
  let mut runner = match common::runner_or_skip("tsc runner tests") {
    Some(runner) => runner,
    None => return,
  };

  let tmp = tempfile::tempdir().expect("tempdir");
  let type_root = tmp.path().join("types");
  let pkg_dir = type_root.join("external");
  fs::create_dir_all(&pkg_dir).expect("create temp type root package dir");
  fs::write(
    pkg_dir.join("index.d.ts"),
    "declare const external: string;\n",
  )
  .expect("write temp type definitions");

  let name: Arc<str> = Arc::from("main.ts");
  let mut files = HashMap::new();
  files.insert(Arc::clone(&name), Arc::from("export {};"));

  let mut options = Map::new();
  options.insert(
    "types".to_string(),
    Value::Array(vec![Value::String("external".to_string())]),
  );
  options.insert(
    "typeRoots".to_string(),
    Value::Array(vec![Value::String(type_root.to_string_lossy().to_string())]),
  );

  let request = TscRequest {
    root_names: vec![name],
    files,
    options,
    diagnostics_only: true,
    trace_resolution: false,
    type_queries: Vec::new(),
  };

  let output = runner.check(request).expect("tsc output");
  assert!(
    output.diagnostics.iter().any(|diag| diag.code == 2688),
    "expected TS2688 when type roots only exist on disk outside the TypeScript install; got {:#?}",
    output.diagnostics
  );
}
