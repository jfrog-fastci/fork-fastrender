#![cfg(feature = "typed")]

use effect_js::{resolve_call, ApiDatabase};
use effect_js::typed::TypedProgram;
use hir_js::{ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn resolve_single_call(source: &str) -> String {
  let index_key = FileKey::new("index.ts");

  // For `resolve_call` we want the resolved module's `FileKey` to correspond to the
  // import specifier (`node:fs`) so we can map `ImportTarget::File` back to a
  // knowledge-base module prefix.
  let fs_key = FileKey::new("node:fs");

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
  host.link(index_key.clone(), "node:fs", fs_key.clone());

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "typecheck diagnostics: {diagnostics:#?}");

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
  resolve_call(lower, root_body, body, call_expr, &db, Some(&types))
    .expect("call resolves")
    .api
}

#[test]
fn resolves_named_import_call() {
  let api = resolve_single_call(r#"import { readFile } from "node:fs"; readFile("x");"#);
  assert_eq!(api, "node:fs.readFile");
}

#[test]
fn resolves_namespace_import_call() {
  let api = resolve_single_call(r#"import * as fs from "node:fs"; fs.readFile("x");"#);
  assert_eq!(api, "node:fs.readFile");
}
