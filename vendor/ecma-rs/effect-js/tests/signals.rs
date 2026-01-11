use effect_js::{collect_signals, SemanticSignal};
use hir_js::{lower_from_source_with_kind, DefKind, ExprId, ExprKind, FileKind, StmtId, StmtKind, VarDeclKind};

#[test]
fn collects_semantic_signals() {
  let source = r#"
async function f(x: number) { return x + 1; }
const a = 1; let b = 2;
const t = { a: 1 } as const;
const y = (z as number)!;
async function g(it: any) { for await (const x of it) { } }
"#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");

  // --- Root / top-level signals ---
  let root_body_id = lowered.root_body();
  let root_body = lowered.body(root_body_id).expect("root body");
  let root_signals = collect_signals(root_body);
  assert_eq!(root_signals.expr_signals.len(), root_body.exprs.len());
  assert_eq!(root_signals.stmt_signals.len(), root_body.stmts.len());

  // `let b = 2;` should not trigger `VarDeclConst`.
  let let_stmt = root_body
    .stmts
    .iter()
    .enumerate()
    .find_map(|(idx, stmt)| match &stmt.kind {
      StmtKind::Var(var) if var.kind == VarDeclKind::Let => Some(StmtId(idx as u32)),
      _ => None,
    })
    .expect("expected a `let` var statement in the root body");
  assert!(
    !root_signals.stmt_signals[let_stmt.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::VarDeclConst { .. })),
    "did not expect VarDeclConst for the `let` statement"
  );

  // `const t = ... as const;` should surface a const assertion (and *not* a type assertion).
  let as_const_expr = root_body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      ExprKind::TypeAssertion {
        const_assertion: true,
        ..
      } => Some(ExprId(idx as u32)),
      _ => None,
    })
    .expect("expected `as const` type assertion expression in the root body");
  assert!(
    root_signals.expr_signals[as_const_expr.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::ConstAssertion { expr } if *expr == as_const_expr)),
    "expected ConstAssertion on the `as const` expr"
  );
  assert!(
    !root_signals.expr_signals[as_const_expr.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::TypeAssertion { .. })),
    "did not expect TypeAssertion on the `as const` expr"
  );

  // `const y = (z as number)!;` should surface both a type assertion and a non-null assertion.
  let type_assert_expr = root_body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      ExprKind::TypeAssertion {
        const_assertion: false,
        ..
      } => Some(ExprId(idx as u32)),
      _ => None,
    })
    .expect("expected `z as number` type assertion expression in the root body");
  assert!(
    root_signals.expr_signals[type_assert_expr.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::TypeAssertion { expr } if *expr == type_assert_expr)),
    "expected TypeAssertion on the `z as number` expr"
  );

  let non_null_expr = root_body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| matches!(&expr.kind, ExprKind::NonNull { .. }).then_some(ExprId(idx as u32)))
    .expect("expected non-null assertion expression in the root body");
  assert!(
    root_signals.expr_signals[non_null_expr.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::NonNullAssertion { expr } if *expr == non_null_expr)),
    "expected NonNullAssertion on the `(...)!` expr"
  );

  // --- Async function signals ---
  let def_for_name = |name: &str| {
    lowered
      .defs
      .iter()
      .find(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some(name))
      .unwrap_or_else(|| panic!("expected function {name}"))
  };

  // `async function f` never awaits -> signal.
  let f_def = def_for_name("f");
  let f_body_id = f_def.body.expect("expected body for function f");
  let f_body = lowered.body(f_body_id).expect("f body exists");
  let f_signals = collect_signals(f_body);
  assert!(
    f_signals
      .body_signals
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::AsyncFunctionNeverAwaits { body } if *body == f_body_id)),
    "expected AsyncFunctionNeverAwaits for function f"
  );

  // `async function g` contains `for await` -> should NOT signal.
  let g_def = def_for_name("g");
  let g_body_id = g_def.body.expect("expected body for function g");
  let g_body = lowered.body(g_body_id).expect("g body exists");
  let g_signals = collect_signals(g_body);
  assert!(
    !g_signals
      .body_signals
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::AsyncFunctionNeverAwaits { .. })),
    "did not expect AsyncFunctionNeverAwaits for function g"
  );

  let for_await_stmt = g_body
    .stmts
    .iter()
    .enumerate()
    .find_map(|(idx, stmt)| match &stmt.kind {
      StmtKind::ForIn {
        is_for_of: true,
        await_: true,
        ..
      } => Some(StmtId(idx as u32)),
      _ => None,
    })
    .expect("expected `for await (... of ...)` statement in g");
  assert!(
    g_signals.stmt_signals[for_await_stmt.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::ForAwaitOf { stmt } if *stmt == for_await_stmt)),
    "expected ForAwaitOf signal on the loop statement"
  );
}

