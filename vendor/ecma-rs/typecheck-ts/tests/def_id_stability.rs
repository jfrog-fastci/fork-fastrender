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

