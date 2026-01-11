use effect_js::signals::detect_signals;
use effect_js::{collect_signals, SemanticSignal};
use hir_js::{
  lower_from_source_with_kind, DefKind, ExprId, ExprKind, FileKind, ObjectKey, StmtId, StmtKind,
  TypeExprId, TypeExprKind, TypeMemberKind, VarDeclKind,
};

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

  // `let b = 2;` should not trigger `ConstBinding`.
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
      .any(|sig| matches!(sig, SemanticSignal::ConstBinding { .. })),
    "did not expect ConstBinding for the `let` statement"
  );

  // `const t = ... as const;` should surface both a const assertion and a type assertion.
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
      .any(
        |sig| matches!(sig, SemanticSignal::AsConstAssertion { expr } if *expr == as_const_expr)
      ),
    "expected AsConstAssertion on the `as const` expr"
  );
  assert!(
    root_signals.expr_signals[as_const_expr.0 as usize]
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::TypeAssertion { expr } if *expr == as_const_expr)),
    "expected TypeAssertion on the `as const` expr"
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
      .any(
        |sig| matches!(sig, SemanticSignal::TypeAssertion { expr } if *expr == type_assert_expr)
      ),
    "expected TypeAssertion on the `z as number` expr"
  );

  let non_null_expr = root_body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| {
      matches!(&expr.kind, ExprKind::NonNull { .. }).then_some(ExprId(idx as u32))
    })
    .expect("expected non-null assertion expression in the root body");
  assert!(
    root_signals.expr_signals[non_null_expr.0 as usize]
      .iter()
      .any(
        |sig| matches!(sig, SemanticSignal::NonNullAssertion { expr } if *expr == non_null_expr)
      ),
    "expected NonNullAssertion on the `(...)!` expr"
  );

  // --- Async function signals ---
  let def_for_name = |name: &str| {
    lowered
      .defs
      .iter()
      .find(|def| {
        def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some(name)
      })
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
      .any(|sig| matches!(sig, SemanticSignal::AsyncFunctionWithoutAwait { def, body } if *body == f_body_id && *def == f_def.id)),
    "expected AsyncFunctionWithoutAwait for function f"
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
      .any(|sig| matches!(sig, SemanticSignal::AsyncFunctionWithoutAwait { .. })),
    "did not expect AsyncFunctionWithoutAwait for function g"
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

fn find_def<'a>(
  lowered: &'a hir_js::LowerResult,
  kind: DefKind,
  name: &str,
) -> &'a hir_js::DefData {
  lowered
    .defs
    .iter()
    .find(|def| def.path.kind == kind && lowered.names.resolve(def.name) == Some(name))
    .unwrap_or_else(|| panic!("expected {kind:?} def named {name}"))
}

fn find_expr<'a>(
  body: &'a hir_js::Body,
  pred: impl Fn(ExprId, &'a hir_js::ExprKind) -> bool,
) -> ExprId {
  body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| {
      let expr_id = ExprId(idx as u32);
      pred(expr_id, &expr.kind).then_some(expr_id)
    })
    .expect("expected to find matching expression")
}

fn find_stmt<'a>(
  body: &'a hir_js::Body,
  pred: impl Fn(StmtId, &'a hir_js::StmtKind) -> bool,
) -> StmtId {
  body
    .stmts
    .iter()
    .enumerate()
    .find_map(|(idx, stmt)| {
      let stmt_id = StmtId(idx as u32);
      pred(stmt_id, &stmt.kind).then_some(stmt_id)
    })
    .expect("expected to find matching statement")
}

fn find_type_expr(
  arenas: &hir_js::TypeArenas,
  pred: impl Fn(TypeExprId, &hir_js::TypeExprKind) -> bool,
) -> TypeExprId {
  arenas
    .type_exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| {
      let id = TypeExprId(idx as u32);
      pred(id, &expr.kind).then_some(id)
    })
    .expect("expected to find matching type expression")
}

#[test]
fn detects_exec_plan_semantic_signals() {
  let source = r#"
class C {
  #x = 1;
  m() { return this.#x; }
}

async function noAwait() { return 1; }

function takesReadonly(arr: readonly string[], obj: { readonly a: number }) { }

const a = 1, b = 2;
const t = { a: 1 } as const;
Promise.all([t]);
"#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");

  // --- Root body signals ---
  let root_body_id = lowered.root_body();
  let root_body = lowered.body(root_body_id).expect("root body");
  let root_signals = detect_signals(&lowered.hir, root_body, &lowered.names);

  // Promise.all(...)
  let promise_all_call = find_expr(root_body, |_, kind| {
    match kind {
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::PromiseAll { .. } => true,
      ExprKind::Call(call) => {
        if call.is_new {
          return false;
        }
        let Some(callee) = root_body.exprs.get(call.callee.0 as usize) else {
          return false;
        };
        let ExprKind::Member(member) = &callee.kind else {
          return false;
        };
        let Some(obj) = root_body.exprs.get(member.object.0 as usize) else {
          return false;
        };
        let ExprKind::Ident(obj_name) = &obj.kind else {
          return false;
        };
        let ObjectKey::Ident(prop_name) = &member.property else {
          return false;
        };
        lowered.names.resolve(*obj_name) == Some("Promise")
          && lowered.names.resolve(*prop_name) == Some("all")
      }
      _ => false,
    }
  });
  assert!(
    root_signals
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::PromiseAll { expr } if *expr == promise_all_call)),
    "expected PromiseAll signal"
  );

  // const a = 1, b = 2;
  let const_multi_stmt = find_stmt(root_body, |_, kind| match kind {
    StmtKind::Var(var) => var.kind == VarDeclKind::Const && var.declarators.len() == 2,
    _ => false,
  });
  let first_const = root_signals
    .iter()
    .position(|sig| matches!(sig, SemanticSignal::ConstBinding { stmt, declarator_index: 0 } if *stmt == const_multi_stmt))
    .expect("expected first ConstBinding");
  let second_const = root_signals
    .iter()
    .position(|sig| matches!(sig, SemanticSignal::ConstBinding { stmt, declarator_index: 1 } if *stmt == const_multi_stmt))
    .expect("expected second ConstBinding");
  assert!(
    first_const < second_const,
    "expected ConstBinding signals to be ordered by declarator_index"
  );

  // as const => both AsConstAssertion and TypeAssertion, in that order.
  let as_const_expr = find_expr(root_body, |_, kind| {
    matches!(
      kind,
      ExprKind::TypeAssertion {
        const_assertion: true,
        ..
      }
    )
  });
  let as_const_pos = root_signals
    .iter()
    .position(
      |sig| matches!(sig, SemanticSignal::AsConstAssertion { expr } if *expr == as_const_expr),
    )
    .expect("expected AsConstAssertion");
  let type_assert_pos = root_signals
    .iter()
    .position(|sig| matches!(sig, SemanticSignal::TypeAssertion { expr } if *expr == as_const_expr))
    .expect("expected TypeAssertion for as const");
  assert!(
    as_const_pos < type_assert_pos,
    "expected AsConstAssertion to sort before TypeAssertion for the same expr"
  );

  // --- Async function without await ---
  let no_await_def = find_def(&lowered, DefKind::Function, "noAwait");
  let no_await_body_id = no_await_def.body.expect("noAwait body");
  let no_await_body = lowered.body(no_await_body_id).expect("noAwait body exists");
  let no_await_signals = detect_signals(&lowered.hir, no_await_body, &lowered.names);
  assert!(
    no_await_signals.iter().any(|sig| matches!(sig, SemanticSignal::AsyncFunctionWithoutAwait { def, body } if *def == no_await_def.id && *body == no_await_body_id)),
    "expected AsyncFunctionWithoutAwait for noAwait"
  );

  // --- readonly type syntax ---
  let takes_def = find_def(&lowered, DefKind::Function, "takesReadonly");
  let takes_body_id = takes_def.body.expect("takesReadonly body");
  let takes_body = lowered
    .body(takes_body_id)
    .expect("takesReadonly body exists");
  let takes_signals = detect_signals(&lowered.hir, takes_body, &lowered.names);

  let arenas = lowered
    .hir
    .types
    .get(&takes_def.id)
    .expect("type arenas for takesReadonly");

  let readonly_array = find_type_expr(
    arenas,
    |_, kind| matches!(kind, TypeExprKind::Array(arr) if arr.readonly),
  );
  assert!(
    takes_signals
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::ReadonlyTypePosition { type_expr } if *type_expr == readonly_array)),
    "expected ReadonlyTypePosition for readonly array"
  );

  let readonly_prop_type = arenas
    .type_members
    .iter()
    .find_map(|member| match &member.kind {
      TypeMemberKind::Property(prop) if prop.readonly => prop.type_annotation,
      _ => None,
    })
    .expect("expected readonly property signature with type annotation");
  assert!(
    takes_signals
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::ReadonlyTypePosition { type_expr } if *type_expr == readonly_prop_type)),
    "expected ReadonlyTypePosition for readonly property signature"
  );

  // --- private field access ---
  let method_def = find_def(&lowered, DefKind::Method, "m");
  let method_body_id = method_def.body.expect("method body");
  let method_body = lowered.body(method_body_id).expect("method body exists");
  let method_signals = detect_signals(&lowered.hir, method_body, &lowered.names);

  let private_access_expr = find_expr(method_body, |_, kind| match kind {
    ExprKind::Member(member) => match &member.property {
      ObjectKey::Ident(id) => lowered
        .names
        .resolve(*id)
        .is_some_and(|name| name.starts_with('#')),
      _ => false,
    },
    _ => false,
  });

  assert!(
    method_signals
      .iter()
      .any(|sig| matches!(sig, SemanticSignal::PrivateFieldAccess { expr } if *expr == private_access_expr)),
    "expected PrivateFieldAccess for this.#x"
  );
}
