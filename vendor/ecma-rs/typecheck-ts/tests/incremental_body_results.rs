use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program, QueryKind};

#[test]
fn ids_stable_across_unrelated_edits() {
  let mut host = MemoryHost::new();
  let file0 = FileKey::new("file0.ts");
  let file1 = FileKey::new("file1.ts");
  host.insert(file0.clone(), "export function foo() { return 1; }\n");
  host.insert(file1.clone(), "export function bar() { return 2; }\n");

  let mut program = Program::new(host, vec![file0.clone(), file1.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");

  let file1_id = program.file_id(&file1).expect("file1 id");
  let bar_def_before = program
    .definitions_in_file(file1_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("bar"))
    .expect("bar def");
  let bodies_before = program.bodies_in_file(file1_id);
  assert!(
    !bodies_before.is_empty(),
    "expected at least one body in file1"
  );

  let file0_id = program.file_id(&file0).expect("file0 id");
  program.set_file_text(file0_id, Arc::from("export function foo() { return 1; }\n\n"));
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected diagnostics after edit: {diags:?}");

  assert_eq!(
    program.file_id(&file1).expect("file1 id after edit"),
    file1_id,
    "file ids must remain stable across edits"
  );
  let bar_def_after = program
    .definitions_in_file(file1_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("bar"))
    .expect("bar def after edit");
  assert_eq!(
    bar_def_after, bar_def_before,
    "definition ids in untouched files must remain stable across edits"
  );
  assert_eq!(
    program.bodies_in_file(file1_id),
    bodies_before,
    "body ids in untouched files must remain stable across edits"
  );
}

#[test]
fn check_reuses_cached_body_results() {
  let mut host = MemoryHost::new();
  let file0 = FileKey::new("file0.ts");
  let file1 = FileKey::new("file1.ts");
  host.insert(file0.clone(), "export function a() { return 1; }\n");
  host.insert(
    file1.clone(),
    "export function b() { return 2; }\nexport function c() { return 3; }\n",
  );

  let mut program = Program::new(host, vec![file0.clone(), file1.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
  let stats_after_first = program.query_stats();
  let misses_after_first = stats_after_first
    .queries
    .get(&QueryKind::CheckBody)
    .map(|stat| stat.cache_misses)
    .unwrap_or(0);
  assert!(
    misses_after_first > 0,
    "expected initial check to typecheck at least one body"
  );

  let diags = program.check();
  assert!(diags.is_empty(), "unexpected diagnostics on second check: {diags:?}");
  let stats_after_second = program.query_stats();
  let misses_after_second = stats_after_second
    .queries
    .get(&QueryKind::CheckBody)
    .map(|stat| stat.cache_misses)
    .unwrap_or(0);
  assert_eq!(
    misses_after_second, misses_after_first,
    "expected unchanged re-check to reuse cached body results"
  );

  let file0_id = program.file_id(&file0).expect("file0 id");
  let bodies_in_file0 = program.bodies_in_file(file0_id);
  assert!(
    !bodies_in_file0.is_empty(),
    "expected at least one body in edited file"
  );

  let file1_id = program.file_id(&file1).expect("file1 id");
  let bodies_in_file1 = program.bodies_in_file(file1_id);
  assert!(
    bodies_in_file1.len() > bodies_in_file0.len(),
    "expected non-edited file to have more bodies so recheck counts are distinguishable"
  );

  // Whitespace-only edit that should not affect declaration types.
  program.set_file_text(file0_id, Arc::from("export function a() { return 1; }\n\n"));
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected diagnostics after whitespace edit: {diags:?}");
  let stats_after_edit = program.query_stats();
  let misses_after_edit = stats_after_edit
    .queries
    .get(&QueryKind::CheckBody)
    .map(|stat| stat.cache_misses)
    .unwrap_or(0);
  let delta = misses_after_edit.saturating_sub(misses_after_second);
  assert!(
    delta <= (bodies_in_file0.len() as u64) * 2,
    "expected bodies in non-edited files to avoid re-checking (delta={delta}, file0_bodies={})",
    bodies_in_file0.len()
  );
}
