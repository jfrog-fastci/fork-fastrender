#[cfg(feature = "hir-semantic-ops")]
use hir_js::ArrayChainOp;
use hir_js::{
  Body, BodyId, ExprId, ExprKind, LowerResult, NameId, NameInterner, ObjectKey, ObjectProperty,
  PatId, PatKind, StmtId, StmtKind, TypeExprId, UnaryOp, VarDeclarator,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizedPattern {
  MapFilterReduceChain {
    body: BodyId,
    root: ExprId,
    array: ExprId,
    map_callback: ExprId,
    filter_callback: ExprId,
    reduce_callback: ExprId,
    reduce_init: Option<ExprId>,
  },
  PromiseAllFetch {
    body: BodyId,
    root: ExprId,
    urls: ExprId,
    map_call: ExprId,
    callback: ExprId,
    callback_body: BodyId,
    fetch_call: ExprId,
  },
  AsyncIteratorForAwait {
    body: BodyId,
    stmt: StmtId,
    iterable: ExprId,
  },
  JsonParseTypedVar {
    body: BodyId,
    stmt: StmtId,
    parse_call: ExprId,
    type_annotation: TypeExprId,
  },
  JsonParseTypedAssertion {
    body: BodyId,
    assertion: ExprId,
    parse_call: ExprId,
    type_annotation: TypeExprId,
  },
  StringTemplate {
    body: BodyId,
    expr: ExprId,
    span_count: usize,
  },
  ObjectSpread {
    body: BodyId,
    expr: ExprId,
    spread_count: usize,
  },
  ArrayDestructure {
    body: BodyId,
    stmt: StmtId,
    pat: PatId,
    arity: usize,
    init: Option<ExprId>,
  },
  MapGetOrDefault {
    body: BodyId,
    expr: ExprId,
    map_has_call: ExprId,
    map_get_call: ExprId,
    map_ident: NameId,
    key_in_has: ExprId,
    key_in_get: ExprId,
    default_expr: ExprId,
  },
  GuardClause {
    body: BodyId,
    stmt: StmtId,
    guarded_expr: ExprId,
  },
}

pub fn recognize_patterns(lowered: &LowerResult) -> Vec<RecognizedPattern> {
  let mut out = Vec::new();
  let names = lowered.names.as_ref();

  for (&body_id, &idx) in &lowered.body_index {
    let body = lowered.bodies[idx].as_ref();
    for &stmt in &body.root_stmts {
      visit_stmt(lowered, names, body_id, body, stmt, &mut out);
    }
  }

  out
}

fn visit_stmt(
  lowered: &LowerResult,
  names: &NameInterner,
  body_id: BodyId,
  body: &Body,
  stmt_id: StmtId,
  out: &mut Vec<RecognizedPattern>,
) {
  let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
    return;
  };
  match &stmt.kind {
    StmtKind::Expr(expr) => visit_expr(lowered, names, body_id, body, *expr, out),
    StmtKind::ExportDefaultExpr(expr) => visit_expr(lowered, names, body_id, body, *expr, out),
    StmtKind::Return(expr) => {
      if let Some(expr) = expr {
        visit_expr(lowered, names, body_id, body, *expr, out);
      }
    }
    StmtKind::Block(stmts) => {
      for stmt in stmts {
        visit_stmt(lowered, names, body_id, body, *stmt, out);
      }
    }
    StmtKind::If {
      test,
      consequent,
      alternate,
    } => {
      if alternate.is_none() && is_guard_clause(body, *test, *consequent) {
        if let Some(guarded) = extract_negated_expr(body, *test) {
          out.push(RecognizedPattern::GuardClause {
            body: body_id,
            stmt: stmt_id,
            guarded_expr: guarded,
          });
        }
      }

      visit_expr(lowered, names, body_id, body, *test, out);
      visit_stmt(lowered, names, body_id, body, *consequent, out);
      if let Some(alt) = alternate {
        visit_stmt(lowered, names, body_id, body, *alt, out);
      }
    }
    StmtKind::ForIn {
      left,
      right,
      body: loop_body,
      is_for_of,
      await_,
    } => {
      if *is_for_of && *await_ {
        out.push(RecognizedPattern::AsyncIteratorForAwait {
          body: body_id,
          stmt: stmt_id,
          iterable: *right,
        });
      }

      visit_expr(lowered, names, body_id, body, *right, out);
      visit_stmt(lowered, names, body_id, body, *loop_body, out);

      match left {
        hir_js::ForHead::Pat(pat) => visit_pat(lowered, names, body_id, body, *pat, out),
        hir_js::ForHead::Var(var) => {
          for decl in &var.declarators {
            visit_var_declarator(lowered, names, body_id, body, stmt_id, decl, out);
          }
        }
      }
    }
    StmtKind::Var(var) => {
      for decl in &var.declarators {
        visit_var_declarator(lowered, names, body_id, body, stmt_id, decl, out);
      }
    }
    StmtKind::While { test, body: inner } | StmtKind::DoWhile { test, body: inner } => {
      visit_expr(lowered, names, body_id, body, *test, out);
      visit_stmt(lowered, names, body_id, body, *inner, out);
    }
    StmtKind::For {
      init,
      test,
      update,
      body: inner,
    } => {
      if let Some(init) = init {
        match init {
          hir_js::ForInit::Expr(expr) => visit_expr(lowered, names, body_id, body, *expr, out),
          hir_js::ForInit::Var(var) => {
            for decl in &var.declarators {
              visit_var_declarator(lowered, names, body_id, body, stmt_id, decl, out);
            }
          }
        }
      }
      if let Some(test) = test {
        visit_expr(lowered, names, body_id, body, *test, out);
      }
      if let Some(update) = update {
        visit_expr(lowered, names, body_id, body, *update, out);
      }
      visit_stmt(lowered, names, body_id, body, *inner, out);
    }
    StmtKind::Switch {
      discriminant,
      cases,
    } => {
      visit_expr(lowered, names, body_id, body, *discriminant, out);
      for case in cases {
        if let Some(test) = case.test {
          visit_expr(lowered, names, body_id, body, test, out);
        }
        for stmt in &case.consequent {
          visit_stmt(lowered, names, body_id, body, *stmt, out);
        }
      }
    }
    StmtKind::Try {
      block,
      catch,
      finally_block,
    } => {
      visit_stmt(lowered, names, body_id, body, *block, out);
      if let Some(catch) = catch {
        if let Some(pat) = catch.param {
          visit_pat(lowered, names, body_id, body, pat, out);
        }
        visit_stmt(lowered, names, body_id, body, catch.body, out);
      }
      if let Some(finally_block) = finally_block {
        visit_stmt(lowered, names, body_id, body, *finally_block, out);
      }
    }
    StmtKind::Throw(expr) => visit_expr(lowered, names, body_id, body, *expr, out),

    // Nothing to traverse.
    StmtKind::Decl(_)
    | StmtKind::Break(_)
    | StmtKind::Continue(_)
    | StmtKind::Labeled { .. }
    | StmtKind::With { .. }
    | StmtKind::Debugger
    | StmtKind::Empty => {}
  }
}

fn visit_var_declarator(
  lowered: &LowerResult,
  names: &NameInterner,
  body_id: BodyId,
  body: &Body,
  stmt_id: StmtId,
  decl: &VarDeclarator,
  out: &mut Vec<RecognizedPattern>,
) {
  if let Some(init) = decl.init {
    visit_expr(lowered, names, body_id, body, init, out);
  }
  visit_pat(lowered, names, body_id, body, decl.pat, out);

  if let Some(arity) = array_pat_fixed_arity(body, decl.pat) {
    out.push(RecognizedPattern::ArrayDestructure {
      body: body_id,
      stmt: stmt_id,
      pat: decl.pat,
      arity,
      init: decl.init,
    });
  }

  if let (Some(type_annotation), Some(init)) = (decl.type_annotation, decl.init) {
    if is_json_parse_call(body, names, init) {
      out.push(RecognizedPattern::JsonParseTypedVar {
        body: body_id,
        stmt: stmt_id,
        parse_call: init,
        type_annotation,
      });
    }
  }
}

fn visit_pat(
  lowered: &LowerResult,
  names: &NameInterner,
  body_id: BodyId,
  body: &Body,
  pat_id: PatId,
  out: &mut Vec<RecognizedPattern>,
) {
  let Some(pat) = body.pats.get(pat_id.0 as usize) else {
    return;
  };
  match &pat.kind {
    PatKind::Ident(_) => {}
    PatKind::Array(arr) => {
      for element in &arr.elements {
        if let Some(element) = element {
          visit_pat(lowered, names, body_id, body, element.pat, out);
          if let Some(default_value) = element.default_value {
            visit_expr(lowered, names, body_id, body, default_value, out);
          }
        }
      }
      if let Some(rest) = arr.rest {
        visit_pat(lowered, names, body_id, body, rest, out);
      }
    }
    PatKind::Object(obj) => {
      for prop in &obj.props {
        visit_pat(lowered, names, body_id, body, prop.value, out);
        if let Some(default_value) = prop.default_value {
          visit_expr(lowered, names, body_id, body, default_value, out);
        }
        if let ObjectKey::Computed(expr) = prop.key {
          visit_expr(lowered, names, body_id, body, expr, out);
        }
      }
      if let Some(rest) = obj.rest {
        visit_pat(lowered, names, body_id, body, rest, out);
      }
    }
    PatKind::Rest(inner) => visit_pat(lowered, names, body_id, body, **inner, out),
    PatKind::Assign {
      target,
      default_value,
    } => {
      visit_pat(lowered, names, body_id, body, *target, out);
      visit_expr(lowered, names, body_id, body, *default_value, out);
    }
    PatKind::AssignTarget(expr) => visit_expr(lowered, names, body_id, body, *expr, out),
  }
}

fn visit_expr(
  lowered: &LowerResult,
  names: &NameInterner,
  body_id: BodyId,
  body: &Body,
  expr_id: ExprId,
  out: &mut Vec<RecognizedPattern>,
) {
  let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
    return;
  };

  match &expr.kind {
    ExprKind::Call(_) => {
      if let Some(pattern) = match_map_filter_reduce(body, names, expr_id) {
        out.push(pattern.with_body(body_id));
      }
      if let Some(pattern) = match_promise_all_fetch(lowered, names, body_id, body, expr_id) {
        out.push(pattern);
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayChain { .. } => {
      if let Some(pattern) = match_map_filter_reduce(body, names, expr_id) {
        out.push(pattern.with_body(body_id));
      }
    }
    ExprKind::Conditional {
      test,
      consequent,
      alternate,
    } => {
      if let Some(pattern) =
        match_map_get_or_default(body, names, expr_id, *test, *consequent, *alternate)
      {
        out.push(pattern.with_body(body_id));
      }
    }
    ExprKind::TypeAssertion {
      expr,
      type_annotation: Some(type_annotation),
      ..
    } => {
      if is_json_parse_call(body, names, *expr) {
        out.push(RecognizedPattern::JsonParseTypedAssertion {
          body: body_id,
          assertion: expr_id,
          parse_call: *expr,
          type_annotation: *type_annotation,
        });
      }
    }
    ExprKind::Template(template) => {
      if template.spans.len() >= 2 {
        out.push(RecognizedPattern::StringTemplate {
          body: body_id,
          expr: expr_id,
          span_count: template.spans.len(),
        });
      }
    }
    ExprKind::Object(object) => {
      let spread_count = object
        .properties
        .iter()
        .filter(|prop| matches!(prop, ObjectProperty::Spread(_)))
        .count();
      if spread_count >= 1 {
        out.push(RecognizedPattern::ObjectSpread {
          body: body_id,
          expr: expr_id,
          spread_count,
        });
      }
    }
    _ => {}
  }

  match &expr.kind {
    ExprKind::Missing
    | ExprKind::Ident(_)
    | ExprKind::This
    | ExprKind::Super
    | ExprKind::Literal(_)
    | ExprKind::ImportMeta
    | ExprKind::NewTarget => {}
    ExprKind::Unary { expr, .. } => visit_expr(lowered, names, body_id, body, *expr, out),
    ExprKind::Update { expr, .. } => visit_expr(lowered, names, body_id, body, *expr, out),
    ExprKind::Binary { left, right, .. } => {
      visit_expr(lowered, names, body_id, body, *left, out);
      visit_expr(lowered, names, body_id, body, *right, out);
    }
    ExprKind::Assignment { target, value, .. } => {
      visit_pat(lowered, names, body_id, body, *target, out);
      visit_expr(lowered, names, body_id, body, *value, out);
    }
    ExprKind::Call(call) => {
      visit_expr(lowered, names, body_id, body, call.callee, out);
      for arg in &call.args {
        visit_expr(lowered, names, body_id, body, arg.expr, out);
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { array, callback }
    | ExprKind::ArrayFilter { array, callback }
    | ExprKind::ArrayFind { array, callback }
    | ExprKind::ArrayEvery { array, callback }
    | ExprKind::ArraySome { array, callback } => {
      visit_expr(lowered, names, body_id, body, *array, out);
      visit_expr(lowered, names, body_id, body, *callback, out);
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayReduce {
      array,
      callback,
      init,
    } => {
      visit_expr(lowered, names, body_id, body, *array, out);
      visit_expr(lowered, names, body_id, body, *callback, out);
      if let Some(init) = init {
        visit_expr(lowered, names, body_id, body, *init, out);
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayChain { array, ops } => {
      visit_expr(lowered, names, body_id, body, *array, out);
      for op in ops {
        match *op {
          ArrayChainOp::Map(callback)
          | ArrayChainOp::Filter(callback)
          | ArrayChainOp::Find(callback)
          | ArrayChainOp::Every(callback)
          | ArrayChainOp::Some(callback) => {
            visit_expr(lowered, names, body_id, body, callback, out)
          }
          ArrayChainOp::Reduce(callback, init) => {
            visit_expr(lowered, names, body_id, body, callback, out);
            if let Some(init) = init {
              visit_expr(lowered, names, body_id, body, init, out);
            }
          }
        }
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
      for promise in promises {
        visit_expr(lowered, names, body_id, body, *promise, out);
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::AwaitExpr { value, .. } => visit_expr(lowered, names, body_id, body, *value, out),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::KnownApiCall { args, .. } => {
      for arg in args {
        visit_expr(lowered, names, body_id, body, *arg, out);
      }
    }
    ExprKind::Member(member) => {
      visit_expr(lowered, names, body_id, body, member.object, out);
      if let ObjectKey::Computed(expr) = member.property {
        visit_expr(lowered, names, body_id, body, expr, out);
      }
    }
    ExprKind::Conditional {
      test,
      consequent,
      alternate,
    } => {
      visit_expr(lowered, names, body_id, body, *test, out);
      visit_expr(lowered, names, body_id, body, *consequent, out);
      visit_expr(lowered, names, body_id, body, *alternate, out);
    }
    ExprKind::Array(array) => {
      for element in &array.elements {
        match element {
          hir_js::ArrayElement::Expr(expr) | hir_js::ArrayElement::Spread(expr) => {
            visit_expr(lowered, names, body_id, body, *expr, out);
          }
          hir_js::ArrayElement::Empty => {}
        }
      }
    }
    ExprKind::Object(object) => {
      for prop in &object.properties {
        match prop {
          ObjectProperty::KeyValue { key, value, .. } => {
            if let ObjectKey::Computed(expr) = key {
              visit_expr(lowered, names, body_id, body, *expr, out);
            }
            visit_expr(lowered, names, body_id, body, *value, out);
          }
          ObjectProperty::Getter { key, .. } | ObjectProperty::Setter { key, .. } => {
            if let ObjectKey::Computed(expr) = key {
              visit_expr(lowered, names, body_id, body, *expr, out);
            }
          }
          ObjectProperty::Spread(expr) => visit_expr(lowered, names, body_id, body, *expr, out),
        }
      }
    }
    ExprKind::FunctionExpr { .. } | ExprKind::ClassExpr { .. } => {}
    ExprKind::Template(template) => {
      for span in &template.spans {
        visit_expr(lowered, names, body_id, body, span.expr, out);
      }
    }
    ExprKind::TaggedTemplate { tag, template } => {
      visit_expr(lowered, names, body_id, body, *tag, out);
      for span in &template.spans {
        visit_expr(lowered, names, body_id, body, span.expr, out);
      }
    }
    ExprKind::Await { expr } | ExprKind::NonNull { expr } => {
      visit_expr(lowered, names, body_id, body, *expr, out)
    }
    ExprKind::Yield { expr: Some(expr), .. } => visit_expr(lowered, names, body_id, body, *expr, out),
    ExprKind::Yield { expr: None, .. } => {}
    ExprKind::Instantiation { expr, .. } => visit_expr(lowered, names, body_id, body, *expr, out),
    ExprKind::TypeAssertion { expr, .. } => visit_expr(lowered, names, body_id, body, *expr, out),
    ExprKind::Satisfies { expr, .. } => visit_expr(lowered, names, body_id, body, *expr, out),
    ExprKind::ImportCall {
      argument,
      attributes,
    } => {
      visit_expr(lowered, names, body_id, body, *argument, out);
      if let Some(attributes) = attributes {
        visit_expr(lowered, names, body_id, body, *attributes, out);
      }
    }
    ExprKind::Jsx(_) => {}
  }
}

fn extract_negated_expr(body: &Body, test: ExprId) -> Option<ExprId> {
  let ExprKind::Unary {
    op: UnaryOp::Not,
    expr,
  } = body.exprs.get(test.0 as usize)?.kind
  else {
    return None;
  };
  Some(expr)
}

fn is_guard_clause(body: &Body, test: ExprId, consequent: StmtId) -> bool {
  extract_negated_expr(body, test).is_some() && is_return_or_throw(body, consequent)
}

fn is_return_or_throw(body: &Body, stmt_id: StmtId) -> bool {
  let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
    return false;
  };
  match &stmt.kind {
    StmtKind::Return(_) | StmtKind::Throw(_) => true,
    StmtKind::Block(stmts) if stmts.len() == 1 => is_return_or_throw(body, stmts[0]),
    _ => false,
  }
}

fn array_pat_fixed_arity(body: &Body, pat: PatId) -> Option<usize> {
  let PatKind::Array(arr) = &body.pats.get(pat.0 as usize)?.kind else {
    return None;
  };
  if arr.rest.is_some() {
    return None;
  }
  if arr.elements.is_empty() {
    return None;
  }
  if arr.elements.iter().any(|el| el.is_none()) {
    return None;
  }
  Some(arr.elements.len())
}

fn object_key_matches(key: &ObjectKey, names: &NameInterner, expected: &str) -> bool {
  match key {
    ObjectKey::Ident(id) => names.resolve(*id) == Some(expected),
    ObjectKey::String(s) => s == expected,
    _ => false,
  }
}

fn is_json_parse_call(body: &Body, names: &NameInterner, expr: ExprId) -> bool {
  let ExprKind::Call(call) = &body.exprs[expr.0 as usize].kind else {
    return false;
  };
  let ExprKind::Member(member) = &body.exprs[call.callee.0 as usize].kind else {
    return false;
  };
  object_key_matches(&member.property, names, "parse")
    && matches!(&body.exprs[member.object.0 as usize].kind, ExprKind::Ident(name) if names.resolve(*name) == Some("JSON"))
}

struct PartialMapFilterReduce {
  root: ExprId,
  array: ExprId,
  map_callback: ExprId,
  filter_callback: ExprId,
  reduce_callback: ExprId,
  reduce_init: Option<ExprId>,
}

impl PartialMapFilterReduce {
  fn with_body(self, body: BodyId) -> RecognizedPattern {
    RecognizedPattern::MapFilterReduceChain {
      body,
      root: self.root,
      array: self.array,
      map_callback: self.map_callback,
      filter_callback: self.filter_callback,
      reduce_callback: self.reduce_callback,
      reduce_init: self.reduce_init,
    }
  }
}

fn match_map_filter_reduce(
  body: &Body,
  names: &NameInterner,
  root: ExprId,
) -> Option<PartialMapFilterReduce> {
  #[cfg(feature = "hir-semantic-ops")]
  if let Some(pattern) = match_map_filter_reduce_semantic_ops(body, root) {
    return Some(pattern);
  }

  let ExprKind::Call(reduce_call) = &body.exprs.get(root.0 as usize)?.kind else {
    return None;
  };
  if reduce_call.optional || reduce_call.is_new {
    return None;
  }
  let (filter_expr, reduce_name) = match &body.exprs.get(reduce_call.callee.0 as usize)?.kind {
    ExprKind::Member(member) => (member.object, &member.property),
    _ => return None,
  };
  if !object_key_matches(reduce_name, names, "reduce") {
    return None;
  }
  if reduce_call.args.iter().any(|arg| arg.spread) {
    return None;
  }
  let reduce_callback = reduce_call.args.get(0)?.expr;
  let reduce_init = reduce_call.args.get(1).map(|arg| arg.expr);

  let ExprKind::Call(filter_call) = &body.exprs.get(filter_expr.0 as usize)?.kind else {
    return None;
  };
  if filter_call.optional || filter_call.is_new {
    return None;
  }
  if filter_call.args.len() != 1 || filter_call.args[0].spread {
    return None;
  }
  let filter_callback = filter_call.args[0].expr;
  let (map_expr, filter_name) = match &body.exprs.get(filter_call.callee.0 as usize)?.kind {
    ExprKind::Member(member) => (member.object, &member.property),
    _ => return None,
  };
  if !object_key_matches(filter_name, names, "filter") {
    return None;
  }

  let ExprKind::Call(map_call) = &body.exprs.get(map_expr.0 as usize)?.kind else {
    return None;
  };
  if map_call.optional || map_call.is_new {
    return None;
  }
  if map_call.args.len() != 1 || map_call.args[0].spread {
    return None;
  }
  let map_callback = map_call.args[0].expr;
  let (array, map_name) = match &body.exprs.get(map_call.callee.0 as usize)?.kind {
    ExprKind::Member(member) => (member.object, &member.property),
    _ => return None,
  };
  if !object_key_matches(map_name, names, "map") {
    return None;
  }

  Some(PartialMapFilterReduce {
    root,
    array,
    map_callback,
    filter_callback,
    reduce_callback,
    reduce_init,
  })
}

#[cfg(feature = "hir-semantic-ops")]
fn match_map_filter_reduce_semantic_ops(
  body: &Body,
  root: ExprId,
) -> Option<PartialMapFilterReduce> {
  let ExprKind::ArrayChain { array, ops } = &body.exprs.get(root.0 as usize)?.kind else {
    return None;
  };
  if ops.len() != 3 {
    return None;
  }

  let map_callback = match ops.get(0)? {
    hir_js::ArrayChainOp::Map(callback) => *callback,
    _ => return None,
  };
  let filter_callback = match ops.get(1)? {
    hir_js::ArrayChainOp::Filter(callback) => *callback,
    _ => return None,
  };
  let (reduce_callback, reduce_init) = match ops.get(2)? {
    hir_js::ArrayChainOp::Reduce(callback, init) => (*callback, *init),
    _ => return None,
  };

  Some(PartialMapFilterReduce {
    root,
    array: *array,
    map_callback,
    filter_callback,
    reduce_callback,
    reduce_init,
  })
}

fn match_promise_all_fetch(
  lowered: &LowerResult,
  names: &NameInterner,
  body_id: BodyId,
  body: &Body,
  root: ExprId,
) -> Option<RecognizedPattern> {
  let ExprKind::Call(all_call) = &body.exprs.get(root.0 as usize)?.kind else {
    return None;
  };
  if all_call.optional || all_call.is_new {
    return None;
  }
  if all_call.args.len() != 1 || all_call.args[0].spread {
    return None;
  }

  let ExprKind::Member(all_member) = &body.exprs.get(all_call.callee.0 as usize)?.kind else {
    return None;
  };
  if !object_key_matches(&all_member.property, names, "all") {
    return None;
  }
  let ExprKind::Ident(promise_ident) = &body.exprs.get(all_member.object.0 as usize)?.kind else {
    return None;
  };
  if names.resolve(*promise_ident) != Some("Promise") {
    return None;
  }

  let map_call_expr = all_call.args[0].expr;
  let (urls, callback_expr) = match &body.exprs.get(map_call_expr.0 as usize)?.kind {
    ExprKind::Call(map_call) => {
      if map_call.optional || map_call.is_new {
        return None;
      }
      if map_call.args.len() != 1 || map_call.args[0].spread {
        return None;
      }
      let callback_expr = map_call.args[0].expr;
      let ExprKind::Member(map_member) = &body.exprs.get(map_call.callee.0 as usize)?.kind else {
        return None;
      };
      if !object_key_matches(&map_member.property, names, "map") {
        return None;
      }
      (map_member.object, callback_expr)
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { array, callback } => (*array, *callback),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayChain { array, ops } => {
      let [ArrayChainOp::Map(callback)] = ops.as_slice() else {
        return None;
      };
      (*array, *callback)
    }
    _ => return None,
  };

  let ExprKind::FunctionExpr {
    body: callback_body_id,
    ..
  } = &body.exprs.get(callback_expr.0 as usize)?.kind
  else {
    return None;
  };

  let callback_body = lowered.body(*callback_body_id)?;
  let returned = returned_expr(callback_body)?;

  let ExprKind::Call(fetch_call) = &callback_body.exprs.get(returned.0 as usize)?.kind else {
    return None;
  };
  let ExprKind::Ident(fetch_ident) = &callback_body.exprs.get(fetch_call.callee.0 as usize)?.kind
  else {
    return None;
  };
  if names.resolve(*fetch_ident) != Some("fetch") {
    return None;
  }

  Some(RecognizedPattern::PromiseAllFetch {
    body: body_id,
    root,
    urls,
    map_call: map_call_expr,
    callback: callback_expr,
    callback_body: *callback_body_id,
    fetch_call: returned,
  })
}

fn returned_expr(body: &Body) -> Option<ExprId> {
  let function = body.function.as_ref()?;
  match &function.body {
    hir_js::FunctionBody::Expr(expr) => Some(*expr),
    hir_js::FunctionBody::Block(stmts) => {
      let last = stmts.last().copied()?;
      let StmtKind::Return(Some(expr)) = &body.stmts.get(last.0 as usize)?.kind else {
        return None;
      };
      Some(*expr)
    }
  }
}

struct PartialMapGetOrDefault {
  expr: ExprId,
  map_has_call: ExprId,
  map_get_call: ExprId,
  map_ident: NameId,
  key_in_has: ExprId,
  key_in_get: ExprId,
  default_expr: ExprId,
}

impl PartialMapGetOrDefault {
  fn with_body(self, body: BodyId) -> RecognizedPattern {
    RecognizedPattern::MapGetOrDefault {
      body,
      expr: self.expr,
      map_has_call: self.map_has_call,
      map_get_call: self.map_get_call,
      map_ident: self.map_ident,
      key_in_has: self.key_in_has,
      key_in_get: self.key_in_get,
      default_expr: self.default_expr,
    }
  }
}

fn match_map_get_or_default(
  body: &Body,
  names: &NameInterner,
  expr: ExprId,
  test: ExprId,
  consequent: ExprId,
  alternate: ExprId,
) -> Option<PartialMapGetOrDefault> {
  let (map_has_call, map_ident, key_in_has) =
    match_member_call_with_single_ident_arg(body, names, test, "has")?;
  let (map_get_call, map_ident2, key_in_get) =
    match_member_call_with_single_ident_arg(body, names, consequent, "get")?;
  if map_ident != map_ident2 {
    return None;
  }

  // Ensure the key is the same identifier on both branches.
  let ExprKind::Ident(key1) = &body.exprs.get(key_in_has.0 as usize)?.kind else {
    return None;
  };
  let ExprKind::Ident(key2) = &body.exprs.get(key_in_get.0 as usize)?.kind else {
    return None;
  };
  if key1 != key2 {
    return None;
  }

  Some(PartialMapGetOrDefault {
    expr,
    map_has_call,
    map_get_call,
    map_ident,
    key_in_has,
    key_in_get,
    default_expr: alternate,
  })
}

fn match_member_call_with_single_ident_arg(
  body: &Body,
  names: &NameInterner,
  expr: ExprId,
  method: &str,
) -> Option<(ExprId, NameId, ExprId)> {
  let ExprKind::Call(call) = &body.exprs.get(expr.0 as usize)?.kind else {
    return None;
  };
  if call.optional || call.is_new || call.args.len() != 1 || call.args[0].spread {
    return None;
  }
  let key_expr = call.args[0].expr;

  let ExprKind::Member(member) = &body.exprs.get(call.callee.0 as usize)?.kind else {
    return None;
  };
  if !object_key_matches(&member.property, names, method) {
    return None;
  }
  let ExprKind::Ident(map_ident) = &body.exprs.get(member.object.0 as usize)?.kind else {
    return None;
  };

  Some((expr, *map_ident, key_expr))
}

#[cfg(test)]
mod tests {
  use super::{recognize_patterns, RecognizedPattern};
  use hir_js::FileKind;

  fn patterns(source: &str) -> Vec<RecognizedPattern> {
    let lowered = hir_js::lower_from_source_with_kind(FileKind::Ts, source).unwrap();
    recognize_patterns(&lowered)
  }

  #[test]
  fn recognizes_map_filter_reduce_chain() {
    let pats = patterns("const r = arr.map(f).filter(g).reduce(h, 0);");
    assert!(pats.iter().any(|p| matches!(
      p,
      RecognizedPattern::MapFilterReduceChain {
        reduce_init: Some(_),
        ..
      }
    )));
  }

  #[test]
  fn recognizes_promise_all_fetch() {
    let pats = patterns(
      r#"
async function main(urls: string[]) {
  return Promise.all(urls.map(url => fetch(url)));
}
"#,
    );
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::PromiseAllFetch { .. })));
  }

  #[test]
  fn recognizes_async_iterator_for_await() {
    let pats = patterns(
      r#"
async function main(iter: AsyncIterable<number>) {
  for await (const x of iter) {
    x;
  }
}
"#,
    );
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::AsyncIteratorForAwait { .. })));
  }

  #[test]
  fn recognizes_json_parse_typed() {
    let pats = patterns(
      r#"
type T = { x: number };
const a: T = JSON.parse("{}");
const b = JSON.parse("{}") as T;
"#,
    );
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::JsonParseTypedVar { .. })));
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::JsonParseTypedAssertion { .. })));
  }

  #[test]
  fn recognizes_string_template() {
    let pats = patterns("const s = `${a} ${b}`;");
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::StringTemplate { span_count: 2, .. })));
  }

  #[test]
  fn recognizes_object_spread() {
    let pats = patterns("const o = { ...a, b: 1 };");
    assert!(pats.iter().any(|p| matches!(
      p,
      RecognizedPattern::ObjectSpread {
        spread_count: 1,
        ..
      }
    )));
  }

  #[test]
  fn recognizes_array_destructure() {
    let pats = patterns("const [a, b] = arr;");
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::ArrayDestructure { arity: 2, .. })));
  }

  #[test]
  fn recognizes_map_get_or_default() {
    let pats = patterns("const v = map.has(k) ? map.get(k) : 0;");
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::MapGetOrDefault { .. })));
  }

  #[test]
  fn recognizes_guard_clause() {
    let pats = patterns(
      r#"
function f(x?: number) {
  if (!x) return;
  x;
}
"#,
    );
    assert!(pats
      .iter()
      .any(|p| matches!(p, RecognizedPattern::GuardClause { .. })));
  }
}
