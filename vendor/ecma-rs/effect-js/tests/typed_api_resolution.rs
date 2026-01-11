#![cfg(feature = "typed")]

use effect_js::{recognize_patterns_typed, recognize_patterns_untyped, ApiId, RecognizedPattern};
use effect_js::typed::TypedProgram;
use hir_js::{ExprId, ExprKind, ObjectKey};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
const arr: number[] = [1, 2, 3];
arr.map(x => x + 1);
const total = arr.map(x => x + 1).filter(x => x > 1).reduce((a, b) => a + b, 0);

const ro: ReadonlyArray<number> = arr;
ro.map(x => x);

const str: string = "HELLO";
str.toLowerCase();

const m: Map<string, number> = new Map();
m.get("a");
const v = m.get("a") ?? 0;

const p: Promise<number> = Promise.resolve(1);
p.then(x => x + 1);

const anyVal: any = arr;
anyVal.map((x: number) => x);

const parsed: { x: number } = JSON.parse("{\"x\": 1}");
"#;

#[test]
fn typed_resolves_instance_apis_and_gates_patterns() {
  let index_key = FileKey::new("index.ts");

  let mut host = MemoryHost::new();
  host.insert(index_key.clone(), INDEX_TS);

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let root_body = lowered.root_body();
  let body = lowered.body(root_body).expect("root body exists");

  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let patterns = recognize_patterns_typed(&lowered, root_body, &types);

  let apis: Vec<ApiId> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::CanonicalCall { api, .. } => Some(*api),
      _ => None,
    })
    .collect();

  assert!(apis.contains(&ApiId::ArrayPrototypeMap));
  assert!(apis.contains(&ApiId::StringPrototypeToLowerCase));

  // Ensure we do not resolve prototype APIs when the receiver type is `any`.
  let any_val_map_call = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      ExprKind::Call(call) => {
        let callee = body.exprs.get(call.callee.0 as usize)?;
        let ExprKind::Member(member) = &callee.kind else {
          return None;
        };
        let ObjectKey::Ident(prop) = member.property else {
          return None;
        };
        let prop = lowered.names.resolve(prop)?;
        if prop != "map" {
          return None;
        }
        let recv = body.exprs.get(member.object.0 as usize)?;
        let ExprKind::Ident(name) = recv.kind else {
          return None;
        };
        let recv_name = lowered.names.resolve(name)?;
        (recv_name == "anyVal").then_some(ExprId(idx as u32))
      }
      _ => None,
    })
    .expect("found anyVal.map call");

  assert!(
    !patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::CanonicalCall { call, api: ApiId::ArrayPrototypeMap } if *call == any_val_map_call
    )),
    "anyVal.map should not resolve to Array.prototype.map"
  );

  // Typed-only patterns should be emitted only when types confirm the receiver.
  assert!(patterns
    .iter()
    .any(|pat| matches!(pat, RecognizedPattern::MapFilterReduce { .. })));

  // JsonParseTyped relies on a declared annotation and should work without typing.
  let untyped = recognize_patterns_untyped(&lowered, root_body);
  assert!(untyped
    .iter()
    .any(|pat| matches!(pat, RecognizedPattern::JsonParseTyped { .. })));
}
