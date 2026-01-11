#![cfg(feature = "semantic-ops")]

use diagnostics::TextRange;
use hir_js::ApiId;
use hir_js::ArrayChainOp;
use hir_js::Body;
use hir_js::BodyId;
use hir_js::BodyKind;
use hir_js::DefId;
use hir_js::Expr;
use hir_js::ExprId;
use hir_js::ExprKind;
use hir_js::SpanMap;

#[test]
fn can_construct_semantic_ops_expr_kinds() {
  let exprs = vec![
    Expr {
      span: TextRange::new(0, 1),
      kind: ExprKind::Missing,
    },
    Expr {
      span: TextRange::new(2, 3),
      kind: ExprKind::Missing,
    },
    Expr {
      span: TextRange::new(4, 5),
      kind: ExprKind::Missing,
    },
    Expr {
      span: TextRange::new(6, 7),
      kind: ExprKind::ArrayMap {
        array: ExprId(0),
        callback: ExprId(1),
      },
    },
    Expr {
      span: TextRange::new(8, 9),
      kind: ExprKind::ArrayFilter {
        array: ExprId(0),
        callback: ExprId(1),
      },
    },
    Expr {
      span: TextRange::new(10, 11),
      kind: ExprKind::ArrayReduce {
        array: ExprId(0),
        callback: ExprId(1),
        init: Some(ExprId(2)),
      },
    },
    Expr {
      span: TextRange::new(12, 13),
      kind: ExprKind::ArrayFind {
        array: ExprId(0),
        callback: ExprId(1),
      },
    },
    Expr {
      span: TextRange::new(14, 15),
      kind: ExprKind::ArrayEvery {
        array: ExprId(0),
        callback: ExprId(1),
      },
    },
    Expr {
      span: TextRange::new(16, 17),
      kind: ExprKind::ArraySome {
        array: ExprId(0),
        callback: ExprId(1),
      },
    },
    Expr {
      span: TextRange::new(18, 19),
      kind: ExprKind::ArrayChain {
        array: ExprId(0),
        ops: vec![
          ArrayChainOp::Map(ExprId(1)),
          ArrayChainOp::Filter(ExprId(1)),
          ArrayChainOp::Reduce(ExprId(1), Some(ExprId(2))),
          ArrayChainOp::Find(ExprId(1)),
          ArrayChainOp::Every(ExprId(1)),
          ArrayChainOp::Some(ExprId(1)),
        ],
      },
    },
    Expr {
      span: TextRange::new(20, 21),
      kind: ExprKind::PromiseAll {
        promises: vec![ExprId(0), ExprId(2)],
      },
    },
    Expr {
      span: TextRange::new(22, 23),
      kind: ExprKind::PromiseRace {
        promises: vec![ExprId(0)],
      },
    },
    Expr {
      span: TextRange::new(24, 25),
      kind: ExprKind::AwaitExpr {
        value: ExprId(0),
        known_resolved: true,
      },
    },
    Expr {
      span: TextRange::new(26, 27),
      kind: ExprKind::KnownApiCall {
        api: ApiId(42),
        args: vec![ExprId(0), ExprId(2)],
      },
    },
  ];

  let body = Body {
    owner: DefId(0),
    span: TextRange::new(0, 27),
    kind: BodyKind::Function,
    exprs,
    stmts: Vec::new(),
    pats: Vec::new(),
    root_stmts: Vec::new(),
    function: None,
    class: None,
    expr_types: None,
  };

  assert!(matches!(
    body.exprs[3].kind,
    ExprKind::ArrayMap { array, callback } if array == ExprId(0) && callback == ExprId(1)
  ));
  assert!(matches!(
    body.exprs[5].kind,
    ExprKind::ArrayReduce { init: Some(init), .. } if init == ExprId(2)
  ));
  assert!(matches!(
    body.exprs[9].kind,
    ExprKind::ArrayChain { .. }
  ));
  assert!(matches!(body.exprs[12].kind, ExprKind::AwaitExpr { .. }));
  assert!(matches!(body.exprs[13].kind, ExprKind::KnownApiCall { .. }));
}

#[test]
fn span_map_smoke_for_semantic_ops_exprs() {
  let exprs = vec![
    Expr {
      span: TextRange::new(0, 10),
      kind: ExprKind::Missing,
    },
    Expr {
      span: TextRange::new(20, 30),
      kind: ExprKind::Missing,
    },
    Expr {
      span: TextRange::new(0, 30),
      kind: ExprKind::ArrayMap {
        array: ExprId(0),
        callback: ExprId(1),
      },
    },
  ];

  let body = Body {
    owner: DefId(0),
    span: TextRange::new(0, 30),
    kind: BodyKind::Function,
    exprs,
    stmts: Vec::new(),
    pats: Vec::new(),
    root_stmts: Vec::new(),
    function: None,
    class: None,
    expr_types: None,
  };

  let body_id = BodyId(0);
  let mut map = SpanMap::new();
  for (idx, expr) in body.exprs.iter().enumerate() {
    map.add_expr(expr.span, body_id, ExprId(idx as u32));
  }
  map.finalize();

  assert_eq!(map.expr_at_offset(5), Some((body_id, ExprId(0))));
  assert_eq!(map.expr_at_offset(15), Some((body_id, ExprId(2))));
  assert_eq!(map.expr_at_offset(25), Some((body_id, ExprId(1))));
  assert_eq!(map.expr_span(body_id, ExprId(2)), Some(TextRange::new(0, 30)));
}
