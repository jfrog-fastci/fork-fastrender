#![cfg(feature = "typed")]

use effect_js::{resolve_call, ApiDatabase};
use effect_js::typed::TypedProgram;
use hir_js::{ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::{DefKind, FileKey, ImportTarget, MemoryHost, Program};

fn resolve_single_call(source: &str, link_fs: bool) -> (Arc<Program>, typecheck_ts::FileId, String) {
  let index_key = FileKey::new("index.ts");

  // Use a different `FileKey` than the import specifier to ensure `resolve_call`
  // relies on the preserved import specifier string.
  let fs_key = FileKey::new("node_fs.ts");

  let mut host = MemoryHost::new();
  host.insert(index_key.clone(), source);
  host.insert(
    fs_key.clone(),
    r#"
      export function readFile(path: string): Promise<string> {
        return Promise.resolve("");
      }
    "#,
  );

  if link_fs {
    host.link(index_key.clone(), "node:fs", fs_key.clone());
  }

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  if link_fs {
    assert!(diagnostics.is_empty(), "typecheck diagnostics: {diagnostics:#?}");
  }

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let lower = lowered.as_ref();
  let root_body = lowered.root_body();
  let body = lower.body(root_body).expect("root body exists");

  let call_expr = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| matches!(expr.kind, ExprKind::Call(_)).then_some(ExprId(idx as u32)))
    .expect("expected a call expression in the input");

  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  assert!(db.get("node:fs.readFile").is_some(), "missing node:fs.readFile");

  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let api = resolve_call(lower, root_body, body, call_expr, &db, Some(&types))
    .expect("call resolves")
    .api;

  (program, file, api)
}

#[test]
fn resolves_named_import_call() {
  let (_program, _file, api) =
    resolve_single_call(r#"import { readFile } from "node:fs"; readFile("x");"#, true);
  assert_eq!(api, "node:fs.readFile");
}

#[test]
fn resolves_namespace_import_call() {
  let (_program, _file, api) =
    resolve_single_call(r#"import * as fs from "node:fs"; fs.readFile("x");"#, true);
  assert_eq!(api, "node:fs.readFile");
}

#[test]
fn resolves_import_call_when_module_unresolved() {
  let (program, file, api) = resolve_single_call(
    r#"import { readFile } from "node:fs"; (readFile as any)("x");"#,
    false,
  );
  assert_eq!(api, "node:fs.readFile");

  let import_def = program
    .definitions_in_file(file)
    .into_iter()
    .find(|def| matches!(program.def_kind(*def), Some(DefKind::Import(_))))
    .expect("import def");

  match program.def_kind(import_def) {
    Some(DefKind::Import(import)) => assert!(matches!(import.target, ImportTarget::Unresolved { .. })),
    other => panic!("expected import def kind, got {other:?}"),
  }
}

#[test]
fn resolves_namespace_import_call_when_module_unresolved() {
  let (_program, _file, api) = resolve_single_call(
    r#"import * as fs from "node:fs"; fs.readFile("x");"#,
    false,
  );
  assert_eq!(api, "node:fs.readFile");
}
