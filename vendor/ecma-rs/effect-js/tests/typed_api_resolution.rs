#![cfg(feature = "typed")]

use effect_js::{recognize_patterns_typed, recognize_patterns_untyped, ApiId, RecognizedPattern};
use effect_js::typed::TypedProgram;
use hir_js::{ExprId, ExprKind, Literal, ObjectKey};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
const arr: number[] = [1, 2, 3];
arr.map(x => x + 1);
arr.forEach(x => x + 1);
const total = arr.map(x => x + 1).filter(x => x > 1).reduce((a, b) => a + b, 0);

type NumArray = number[];
const aliasArr: NumArray = arr;
aliasArr.map(x => x);

const ro: ReadonlyArray<number> = arr;
ro.map(x => x);

const str: string = "HELLO";
str.toLowerCase();
str.split("");

type StrAlias = string;
const aliasStr: StrAlias = str;
aliasStr.toLowerCase();

const m: Map<string, number> = new Map();
m.has("a");
m.get("a");
const v = m.get("a") ?? 0;
const w = m.has("__effect_js_key__") ? m.get("__effect_js_key__")! : 12345;

type MapAlias = Map<string, number>;
const aliasMap: MapAlias = m;
aliasMap.has("a");
aliasMap.get("a");
const v2 = aliasMap.get("a") ?? 0;
const v3 = aliasMap.get("__effect_js_alias_key__")! ?? 54321;

const p: Promise<number> = Promise.resolve(1);
p.then(x => x + 1);

type PromiseAlias<T> = Promise<T>;
const aliasPromise: PromiseAlias<number> = p;
aliasPromise.then(x => x + 1);

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
  assert!(apis.contains(&ApiId::ArrayPrototypeForEach));
  assert!(apis.contains(&ApiId::StringPrototypeToLowerCase));
  assert!(apis.contains(&ApiId::StringPrototypeSplit));
  assert!(apis.contains(&ApiId::MapPrototypeGet));
  assert!(apis.contains(&ApiId::MapPrototypeHas));
  assert!(apis.contains(&ApiId::StringPrototypeSplit));
  assert!(apis.contains(&ApiId::MapPrototypeGet));
  assert!(apis.contains(&ApiId::MapPrototypeHas));
  assert!(apis.contains(&ApiId::PromisePrototypeThen));

  let find_member_call = |recv_expected: &str, prop_expected: &str| -> ExprId {
    body
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
          if prop != prop_expected {
            return None;
          }
          let recv = body.exprs.get(member.object.0 as usize)?;
          let ExprKind::Ident(name) = recv.kind else {
            return None;
          };
          let recv_name = lowered.names.resolve(name)?;
          (recv_name == recv_expected).then_some(ExprId(idx as u32))
        }
        _ => None,
      })
      .unwrap_or_else(|| panic!("found {recv_expected}.{prop_expected} call"))
  };

  // Ensure we do not resolve prototype APIs when the receiver type is `any`.
  let any_val_map_call = find_member_call("anyVal", "map");

  assert!(
    !patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::CanonicalCall { call, api: ApiId::ArrayPrototypeMap } if *call == any_val_map_call
    )),
    "anyVal.map should not resolve to Array.prototype.map"
  );

  // Aliased types should still resolve through type aliases.
  let alias_arr_map_call = find_member_call("aliasArr", "map");
  assert!(patterns.iter().any(|pat| matches!(
    pat,
    RecognizedPattern::CanonicalCall { call, api: ApiId::ArrayPrototypeMap } if *call == alias_arr_map_call
  )));

  let alias_str_lower_call = find_member_call("aliasStr", "toLowerCase");
  assert!(patterns.iter().any(|pat| matches!(
    pat,
    RecognizedPattern::CanonicalCall { call, api: ApiId::StringPrototypeToLowerCase } if *call == alias_str_lower_call
  )));

  let alias_map_get_call = find_member_call("aliasMap", "get");
  assert!(patterns.iter().any(|pat| matches!(
    pat,
    RecognizedPattern::CanonicalCall { call, api: ApiId::MapPrototypeGet } if *call == alias_map_get_call
  )));

  let alias_map_has_call = find_member_call("aliasMap", "has");
  assert!(patterns.iter().any(|pat| matches!(
    pat,
    RecognizedPattern::CanonicalCall { call, api: ApiId::MapPrototypeHas } if *call == alias_map_has_call
  )));

  let alias_promise_then_call = find_member_call("aliasPromise", "then");
  assert!(patterns.iter().any(|pat| matches!(
    pat,
    RecognizedPattern::CanonicalCall { call, api: ApiId::PromisePrototypeThen } if *call == alias_promise_then_call
  )));

  // Typed-only patterns should be emitted only when types confirm the receiver.
  assert!(patterns
    .iter()
    .any(|pat| matches!(pat, RecognizedPattern::MapFilterReduce { .. })));
  assert!(
    patterns.iter().any(|pat| matches!(pat, RecognizedPattern::MapGetOrDefault { .. })),
    "MapGetOrDefault should be emitted for m.get(...)/aliasMap.get(...) patterns"
  );
  assert!(patterns.iter().any(|pat| match pat {
    RecognizedPattern::MapGetOrDefault { map, .. } => {
      matches!(
        body.exprs.get(map.0 as usize).map(|expr| &expr.kind),
        Some(ExprKind::Ident(name)) if lowered.names.resolve(*name) == Some("aliasMap")
      )
    }
    _ => false,
  }));

  let nullish_default = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      ExprKind::Literal(Literal::Number(n)) if n == "54321" => Some(ExprId(idx as u32)),
      _ => None,
    })
    .expect("found default literal used in nullish MapGetOrDefault expression");
  assert!(
    patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::MapGetOrDefault { default, .. } if *default == nullish_default
    )),
    "expected MapGetOrDefault for `map.get(key)! ?? default`"
  );

  let conditional_default = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      ExprKind::Literal(Literal::Number(n)) if n == "12345" => Some(ExprId(idx as u32)),
      _ => None,
    })
    .expect("found default literal used in conditional MapGetOrDefault expression");
  assert!(
    patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::MapGetOrDefault { default, .. } if *default == conditional_default
    )),
    "expected MapGetOrDefault for `m.has(k) ? m.get(k) : default` conditional"
  );

  // JsonParseTyped relies on a declared annotation and should work without typing.
  let untyped = recognize_patterns_untyped(&lowered, root_body);
  assert!(untyped
    .iter()
    .any(|pat| matches!(pat, RecognizedPattern::JsonParseTyped { .. })));
}
