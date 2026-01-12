use hir_js::DefKind;
use hir_js::ExprKind;
use hir_js::FileKind;
use hir_js::StmtKind;
use hir_js::TypeExprKind;
use hir_js::lower_from_source_with_kind;

#[test]
fn lowers_instantiation_expr_in_call_callee() {
  let source = "const x = f<string>(1);";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");

  let declarator = lowered
    .defs
    .iter()
    .find(|def| def.path.kind == DefKind::VarDeclarator && lowered.names.resolve(def.name) == Some("x"))
    .expect("x declarator def");
  let init_body_id = declarator.body.expect("x initializer body id");
  let init_body = lowered.body(init_body_id).expect("initializer body");

  let stmt_id = *init_body.root_stmts.first().expect("initializer root stmt");
  let stmt = &init_body.stmts[stmt_id.0 as usize];
  let init_expr = match &stmt.kind {
    StmtKind::Var(var_decl) => var_decl.declarators[0].init.expect("initializer expr id"),
    other => panic!("expected var decl statement, got {other:?}"),
  };

  let call = match &init_body.exprs[init_expr.0 as usize].kind {
    ExprKind::Call(call) => call,
    other => panic!("expected call expression, got {other:?}"),
  };

  let callee = match &init_body.exprs[call.callee.0 as usize].kind {
    ExprKind::Instantiation { expr, type_args } => {
      assert_eq!(type_args.len(), 1, "expected exactly one explicit type argument");
      (*expr, type_args[0])
    }
    other => panic!("expected instantiation callee, got {other:?}"),
  };

  match &init_body.exprs[callee.0.0 as usize].kind {
    ExprKind::Ident(name) => {
      assert_eq!(lowered.names.resolve(*name), Some("f"));
    }
    other => panic!("expected ident callee, got {other:?}"),
  }

  let arenas = lowered.type_arenas(declarator.id).expect("type arenas for x declarator");
  assert!(
    matches!(arenas.type_exprs[callee.1.0 as usize].kind, TypeExprKind::String),
    "expected lowered type argument to be `string`"
  );

  // Span map should be able to land on the instantiation expression for offsets
  // inside `<...>`.
  let offset = source.find('<').expect("< in source") as u32;
  let (mapped_body, mapped_expr) = lowered
    .hir
    .span_map
    .expr_at_offset(offset)
    .expect("expr at instantiation offset");
  let body = lowered.body(mapped_body).expect("mapped body");
  assert!(
    matches!(body.exprs[mapped_expr.0 as usize].kind, ExprKind::Instantiation { .. }),
    "expected span map to return instantiation expr at `<` offset"
  );

  // Type argument nodes should also be indexed in the span map.
  let type_offset = source.find("string").expect("string in source") as u32;
  let (owner, type_expr) = lowered
    .hir
    .span_map
    .type_expr_at_offset(type_offset)
    .expect("type expr at offset");
  let type_arenas = lowered.type_arenas(owner).expect("type arenas for type expr owner");
  assert!(
    matches!(
      type_arenas.type_exprs[type_expr.0 as usize].kind,
      TypeExprKind::String
    ),
    "expected span map type expr at `string` offset"
  );
}

#[test]
fn lowers_instantiation_expr_under_new_call() {
  let source = "new C<number>();";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");

  let root = lowered.root_body();
  let body = lowered.body(root).expect("root body");
  let stmt_id = *body.root_stmts.first().expect("root stmt");
  let stmt = &body.stmts[stmt_id.0 as usize];
  let expr_id = match &stmt.kind {
    StmtKind::Expr(expr) => *expr,
    other => panic!("expected expr statement, got {other:?}"),
  };

  let call = match &body.exprs[expr_id.0 as usize].kind {
    ExprKind::Call(call) => call,
    other => panic!("expected lowered new expression as call, got {other:?}"),
  };
  assert!(call.is_new, "expected is_new for `new C<number>()`");

  let (inner_expr, type_arg) = match &body.exprs[call.callee.0 as usize].kind {
    ExprKind::Instantiation { expr, type_args } => {
      assert_eq!(type_args.len(), 1, "expected exactly one explicit type argument");
      (*expr, type_args[0])
    }
    other => panic!("expected instantiation callee, got {other:?}"),
  };

  match &body.exprs[inner_expr.0 as usize].kind {
    ExprKind::Ident(name) => {
      assert_eq!(lowered.names.resolve(*name), Some("C"));
    }
    other => panic!("expected ident callee, got {other:?}"),
  }

  let arenas = lowered.type_arenas(body.owner).expect("type arenas for root body owner");
  assert!(
    matches!(arenas.type_exprs[type_arg.0 as usize].kind, TypeExprKind::Number),
    "expected lowered type argument to be `number`"
  );
}

#[test]
fn lowers_bare_instantiation_expr() {
  let source = "const g = f<string>;";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");

  let declarator = lowered
    .defs
    .iter()
    .find(|def| def.path.kind == DefKind::VarDeclarator && lowered.names.resolve(def.name) == Some("g"))
    .expect("g declarator def");
  let init_body_id = declarator.body.expect("g initializer body id");
  let init_body = lowered.body(init_body_id).expect("initializer body");

  let stmt_id = *init_body.root_stmts.first().expect("initializer root stmt");
  let stmt = &init_body.stmts[stmt_id.0 as usize];
  let init_expr = match &stmt.kind {
    StmtKind::Var(var_decl) => var_decl.declarators[0].init.expect("initializer expr id"),
    other => panic!("expected var decl statement, got {other:?}"),
  };

  let (inner_expr, type_arg) = match &init_body.exprs[init_expr.0 as usize].kind {
    ExprKind::Instantiation { expr, type_args } => {
      assert_eq!(type_args.len(), 1, "expected exactly one explicit type argument");
      (*expr, type_args[0])
    }
    other => panic!("expected instantiation expression, got {other:?}"),
  };

  match &init_body.exprs[inner_expr.0 as usize].kind {
    ExprKind::Ident(name) => {
      assert_eq!(lowered.names.resolve(*name), Some("f"));
    }
    other => panic!("expected ident inner expression, got {other:?}"),
  }

  let arenas = lowered.type_arenas(declarator.id).expect("type arenas for g declarator");
  assert!(
    matches!(arenas.type_exprs[type_arg.0 as usize].kind, TypeExprKind::String),
    "expected lowered type argument to be `string`"
  );

  // Span map should be able to land on the instantiation expression for offsets
  // inside `<...>`.
  let offset = source.find('<').expect("< in source") as u32;
  let (mapped_body, mapped_expr) = lowered
    .hir
    .span_map
    .expr_at_offset(offset)
    .expect("expr at instantiation offset");
  let body = lowered.body(mapped_body).expect("mapped body");
  assert!(
    matches!(body.exprs[mapped_expr.0 as usize].kind, ExprKind::Instantiation { .. }),
    "expected span map to return instantiation expr at `<` offset"
  );

  // Type argument nodes should also be indexed in the span map.
  let type_offset = source.find("string").expect("string in source") as u32;
  let (owner, type_expr) = lowered
    .hir
    .span_map
    .type_expr_at_offset(type_offset)
    .expect("type expr at offset");
  let type_arenas = lowered.type_arenas(owner).expect("type arenas for type expr owner");
  assert!(
    matches!(type_arenas.type_exprs[type_expr.0 as usize].kind, TypeExprKind::String),
    "expected span map type expr at `string` offset"
  );
}
