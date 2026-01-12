use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn default_export_expr_def_id_is_stable_across_unrelated_edits() {
  // `export default <expr>` does not have a `hir-js` `DefId`, so `typecheck-ts`
  // synthesizes a definition to represent the exported value.
  //
  // That synthetic `DefId` must remain stable across unrelated edits (such as
  // inserting a new top-level declaration earlier in the file).
  let mut host = MemoryHost::new();
  let file = FileKey::new("file1.ts");
  host.insert(file.clone(), "export default 123;\n");

  let mut program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let def_before = program
    .exports_of(file_id)
    .get("default")
    .and_then(|entry| entry.def)
    .expect("default export def");

  program.set_file_text(
    file_id,
    Arc::from(
      r#"
export const added = 1;
export default 123;
"#,
    ),
  );

  let def_after = program
    .exports_of(file_id)
    .get("default")
    .and_then(|entry| entry.def)
    .expect("default export def after edit");

  assert_eq!(
    def_before, def_after,
    "default export expression DefId should be stable across unrelated edits"
  );
  assert_eq!(def_after.file(), file_id, "def should be file-scoped");
}

#[test]
fn default_export_expr_def_id_is_stable_across_other_file_edits() {
  // Regression: `ProgramState::alloc_def()` used a single global counter, so edits
  // that introduce new synthetic defs in *other* files could shift the `DefId`
  // observed for `export default <expr>`.
  let mut host = MemoryHost::new();
  let file0 = FileKey::new("file0.ts");
  let file1 = FileKey::new("file1.ts");
  host.insert(file0.clone(), "export const baseline = 0;\n");
  host.insert(file1.clone(), "export default 123;\n");

  let mut program = Program::new(host, vec![file0.clone(), file1.clone()]);
  let file0_id = program.file_id(&file0).expect("file0 id");
  let file1_id = program.file_id(&file1).expect("file1 id");

  let def_before = program
    .exports_of(file1_id)
    .get("default")
    .and_then(|entry| entry.def)
    .expect("default export def");

  program.set_file_text(
    file0_id,
    Arc::from(
      r#"
export const added = 1;
export const baseline = 0;
"#,
    ),
  );

  let def_after = program
    .exports_of(file1_id)
    .get("default")
    .and_then(|entry| entry.def)
    .expect("default export def after edit");

  assert_eq!(
    def_before, def_after,
    "default export expression DefId should remain stable across unrelated edits in other files"
  );
  assert_eq!(def_after.file(), file1_id, "def should be file-scoped");
}
