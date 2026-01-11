#![cfg(feature = "typed")]

use effect_js::typed::TypedProgram;
use effect_js::{load_default_api_database, recognize_patterns_typed, ArrayChainOp, ArrayTerminal, RecognizedPattern};
use hir_js::{ExprId, ExprKind, Literal};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
const xs: number[] = [1, 2, 3];
const f = (x: number) => x + 1;
const g = (x: number) => x * 2;
const h = (a: number, b: number) => a + b;

xs.map(f).map(g);
xs.map(f).filter(g).reduce(h, 0);
xs.filter(g);
"#;

fn assert_ident(body: &hir_js::Body, lowered: &hir_js::LowerResult, expr: ExprId, expected: &str) {
  let expr = &body.exprs[expr.0 as usize];
  match &expr.kind {
    ExprKind::Ident(name) => assert_eq!(
      lowered.names.resolve(*name),
      Some(expected),
      "expected Ident({expected})"
    ),
    other => panic!("expected Ident({expected}), got {other:?}"),
  }
}

#[test]
fn detects_array_chains_typed() {
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
  let kb = load_default_api_database();
  let patterns = recognize_patterns_typed(&kb, &lowered, root_body, &types);

  let array_chains: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::ArrayChain { base, ops, terminal } => {
        Some((*base, ops.clone(), terminal.clone()))
      }
      _ => None,
    })
    .collect();
  assert_eq!(
    array_chains.len(),
    2,
    "expected exactly two ArrayChain patterns"
  );

  for (base, _ops, _terminal) in array_chains.iter() {
    assert_ident(body, &lowered, *base, "xs");
  }

  let map_map = array_chains
    .iter()
    .find(|(_base, ops, terminal)| {
      terminal.is_none()
        && ops.len() == 2
        && matches!(ops[0], ArrayChainOp::Map { .. })
        && matches!(ops[1], ArrayChainOp::Map { .. })
    })
    .expect("expected xs.map(f).map(g) ArrayChain");

  if let [ArrayChainOp::Map { callback: cb0 }, ArrayChainOp::Map { callback: cb1 }] =
    &map_map.1[..]
  {
    assert_ident(body, &lowered, *cb0, "f");
    assert_ident(body, &lowered, *cb1, "g");
  } else {
    panic!("expected ops=[Map, Map], got {:?}", map_map.1);
  }

  let map_filter_reduce = array_chains
    .iter()
    .find(|(_base, ops, terminal)| {
      ops.len() == 2
        && matches!(ops[0], ArrayChainOp::Map { .. })
        && matches!(ops[1], ArrayChainOp::Filter { .. })
        && matches!(terminal, Some(ArrayTerminal::Reduce { .. }))
    })
    .expect("expected xs.map(f).filter(g).reduce(h, 0) ArrayChain");

  if let [ArrayChainOp::Map { callback: map_cb }, ArrayChainOp::Filter { callback: filter_cb }] =
    &map_filter_reduce.1[..]
  {
    assert_ident(body, &lowered, *map_cb, "f");
    assert_ident(body, &lowered, *filter_cb, "g");
  } else {
    panic!(
      "expected ops=[Map, Filter], got {:?}",
      map_filter_reduce.1
    );
  }

  match &map_filter_reduce.2 {
    Some(ArrayTerminal::Reduce {
      callback,
      init: Some(init),
    }) => {
      assert_ident(body, &lowered, *callback, "h");
      match &body.exprs[init.0 as usize].kind {
        ExprKind::Literal(Literal::Number(n)) => assert_eq!(n, "0"),
        other => panic!("expected init to be numeric literal, got {other:?}"),
      }
    }
    Some(ArrayTerminal::Reduce { init: None, .. }) => {
      panic!("expected reduce init argument to be captured");
    }
    other => panic!("expected terminal=Reduce, got {other:?}"),
  }

  // `xs.filter(g)` is too short to form an ArrayChain.
  assert!(
    !array_chains
      .iter()
      .any(|(_base, ops, terminal)| ops.len() == 1 && terminal.is_none()),
    "expected single-step filter chain to not be recognized"
  );
}
