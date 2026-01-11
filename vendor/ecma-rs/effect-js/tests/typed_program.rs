#![cfg(feature = "typed")]

use effect_js::typed::TypedProgram;
use effect_js::types::TypeProvider;
use hir_js::{ExprId, ExprKind, ObjectKey};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
const nums: number[] = [1,2,3];
const out = nums.map(x => x + 1);
"#;

#[test]
fn typed_program_can_query_expr_types() {
  let file_key = FileKey::new("index.ts");
  let mut host = MemoryHost::new();
  host.insert(file_key.clone(), INDEX_TS);

  let program = Arc::new(Program::new(host, vec![file_key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file = program.file_id(&file_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let body_id = lowered.root_body();
  let body = lowered.body(body_id).expect("root body exists");

  let nums_recv = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(_idx, expr)| {
      let ExprKind::Member(member) = &expr.kind else {
        return None;
      };
      let ObjectKey::Ident(prop) = member.property else {
        return None;
      };
      if lowered.names.resolve(prop)? != "map" {
        return None;
      }
      let recv_expr = body.exprs.get(member.object.0 as usize)?;
      let ExprKind::Ident(name) = recv_expr.kind else {
        return None;
      };
      (lowered.names.resolve(name)? == "nums").then_some(member.object)
    })
    .expect("found receiver for nums.map");

  let map_call = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { array, .. } => (*array == nums_recv).then_some(ExprId(idx as u32)),
      ExprKind::Call(call) => {
        let callee = body.exprs.get(call.callee.0 as usize)?;
        let ExprKind::Member(member) = &callee.kind else {
          return None;
        };
        let ObjectKey::Ident(prop) = member.property else {
          return None;
        };
        if lowered.names.resolve(prop)? != "map" {
          return None;
        }
        (member.object == nums_recv).then_some(ExprId(idx as u32))
      }
      // When `hir-js` semantic-ops lowering is enabled, the `.map(...)` call is
      // represented directly as an `ArrayMap` node.
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { array, .. } => (*array == nums_recv).then_some(ExprId(idx as u32)),
      _ => None,
    })
    .expect("found nums.map(...) call");

  let types = TypedProgram::from_program(Arc::clone(&program), file);

  assert!(types.expr_is_array(body_id, nums_recv));
  assert!(types.expr_type(body_id, nums_recv).is_some());
  assert!(types.expr_type(body_id, map_call).is_some());
}
