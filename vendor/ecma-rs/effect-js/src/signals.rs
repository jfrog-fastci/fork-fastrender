use hir_js::ids::BodyPath;
use hir_js::{
  Body, BodyId, BodyKind, ExprId, ExprKind, ForHead, ForInit, HirFile, NameInterner, ObjectKey,
  StmtId, StmtKind, TypeExprId, TypeExprKind, TypeMemberKind, VarDeclKind,
};

/// Semantic cues surfaced from the user's source code.
///
/// These are intentionally *not* "optimization patterns": they represent explicit
/// developer intent (TypeScript assertions, `for await (...)`, etc.) that later
/// phases can exploit without needing to change the HIR itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SemanticSignal {
  /// `Promise.all(...)` indicates that promises are intended to be independent.
  PromiseAll { expr: ExprId },

  /// An `async` function body that contains no await-like constructs.
  AsyncFunctionWithoutAwait { def: hir_js::DefId, body: BodyId },

  /// `expr as const`.
  AsConstAssertion { expr: ExprId },

  /// `expr as T` / `<T>expr`.
  TypeAssertion { expr: ExprId },

  /// `expr!`.
  NonNullAssertion { expr: ExprId },

  /// A `const` variable binding (`const a = 1, b = 2;` yields one signal per declarator).
  ConstBinding {
    stmt: StmtId,
    declarator_index: usize,
  },

  /// `readonly` type position (readonly arrays/properties).
  ReadonlyTypePosition { type_expr: TypeExprId },

  /// Access to a private field (e.g. `this.#x`).
  PrivateFieldAccess { expr: ExprId },

  /// `for await (... of ...)`.
  ForAwaitOf { stmt: StmtId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalTables {
  /// Signals attached to each [`ExprId`] in the body (`len == body.exprs.len()`).
  pub expr_signals: Vec<Vec<SemanticSignal>>,
  /// Signals attached to each [`StmtId`] in the body (`len == body.stmts.len()`).
  pub stmt_signals: Vec<Vec<SemanticSignal>>,
  /// Signals attached to the body as a whole.
  pub body_signals: Vec<SemanticSignal>,
}

pub fn collect_signals(body: &Body) -> SignalTables {
  let mut tables = SignalTables {
    expr_signals: vec![Vec::new(); body.exprs.len()],
    stmt_signals: vec![Vec::new(); body.stmts.len()],
    body_signals: Vec::new(),
  };

  // Track whether this body contains any await-like construct; used by
  // `AsyncFunctionWithoutAwait`.
  let mut has_await_like = false;

  // Expression signals (deterministic: increasing `ExprId`).
  for (idx, expr) in body.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    match &expr.kind {
      ExprKind::TypeAssertion {
        const_assertion, ..
      } => {
        tables.expr_signals[idx].push(SemanticSignal::TypeAssertion { expr: expr_id });
        if *const_assertion {
          tables.expr_signals[idx].push(SemanticSignal::AsConstAssertion { expr: expr_id });
        }
      }
      ExprKind::NonNull { .. } => {
        tables.expr_signals[idx].push(SemanticSignal::NonNullAssertion { expr: expr_id });
      }
      ExprKind::Await { .. } => {
        has_await_like = true;
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::AwaitExpr { .. } => {
        has_await_like = true;
      }
      _ => {}
    }
  }

  // Statement signals (deterministic: increasing `StmtId`).
  for (idx, stmt) in body.stmts.iter().enumerate() {
    let stmt_id = StmtId(idx as u32);
    match &stmt.kind {
      StmtKind::Var(var) => {
        if var.kind == VarDeclKind::Const {
          for declarator_index in 0..var.declarators.len() {
            tables.stmt_signals[idx].push(SemanticSignal::ConstBinding {
              stmt: stmt_id,
              declarator_index,
            });
          }
        }
        if var.kind == VarDeclKind::AwaitUsing {
          has_await_like = true;
        }
      }
      StmtKind::For { init, .. } => {
        if let Some(ForInit::Var(var)) = init {
          if var.kind == VarDeclKind::AwaitUsing {
            has_await_like = true;
          }
        }
      }
      StmtKind::ForIn {
        left,
        is_for_of: true,
        await_: true,
        ..
      } => {
        tables.stmt_signals[idx].push(SemanticSignal::ForAwaitOf { stmt: stmt_id });
        has_await_like = true;

        if let ForHead::Var(var) = left {
          if var.kind == VarDeclKind::AwaitUsing {
            has_await_like = true;
          }
        }
      }
      StmtKind::ForIn { left, .. } => {
        if let ForHead::Var(var) = left {
          if var.kind == VarDeclKind::AwaitUsing {
            has_await_like = true;
          }
        }
      }
      _ => {}
    }
  }

  // Body-level signals.
  if body.kind == BodyKind::Function {
    if body.function.as_ref().is_some_and(|func| func.async_) && !has_await_like {
      tables
        .body_signals
        .push(SemanticSignal::AsyncFunctionWithoutAwait {
          def: body.owner,
          body: stable_body_id(body),
        });
    }
  }

  tables
}

fn stable_body_id(body: &Body) -> BodyId {
  // `SignalTables` are collected from a `&Body` alone (without the owning `HirFile`
  // or the original `BodyId`). We reconstruct the stable-ish identifier using the
  // same hashing scheme as `hir-js` (see `hir_js::ids::BodyPath`).
  //
  // Note: `hir-js` salts hashes on collision. We intentionally do not attempt to
  // reproduce collision handling here; collisions are vanishingly unlikely and
  // callers that need the exact `BodyId` should carry it alongside the `Body`.
  let path = BodyPath::new(body.owner, body.kind, 0);
  BodyId::new(body.owner.file(), path.stable_hash_u32())
}

/// Detect semantic signals for a body, including ones that require file/name context
/// (e.g. `Promise.all`, readonly type syntax).
///
/// Output ordering is deterministic: by span start, then by signal kind.
pub fn detect_signals(file: &HirFile, body: &Body, names: &NameInterner) -> Vec<SemanticSignal> {
  let tables = collect_signals(body);
  let mut signals = Vec::new();

  for bucket in tables.expr_signals.iter() {
    signals.extend(bucket.iter().cloned());
  }
  for bucket in tables.stmt_signals.iter() {
    signals.extend(bucket.iter().cloned());
  }
  signals.extend(tables.body_signals.iter().cloned());

  // Promise.all(...) signals.
  for (idx, expr) in body.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    match &expr.kind {
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::PromiseAll { .. } => {
        // When `hir-js/semantic-ops` is enabled, `Promise.all([..])` may be lowered into a
        // dedicated node rather than a `Call` expression.
        signals.push(SemanticSignal::PromiseAll { expr: expr_id });
      }
      ExprKind::Call(call) => {
        if !call.is_new && is_promise_all_call(body, names, call.callee) {
          signals.push(SemanticSignal::PromiseAll { expr: expr_id });
        }
      }
      ExprKind::Member(member) => {
        if is_private_name(&member.property, names) {
          signals.push(SemanticSignal::PrivateFieldAccess { expr: expr_id });
        }
      }
      _ => {}
    }
  }

  // readonly types (TypeScript syntax).
  if let Some(arenas) = file.types.get(&body.owner) {
    for (idx, ty) in arenas.type_exprs.iter().enumerate() {
      let id = TypeExprId(idx as u32);
      if matches!(&ty.kind, TypeExprKind::Array(arr) if arr.readonly) {
        signals.push(SemanticSignal::ReadonlyTypePosition { type_expr: id });
      }
    }

    for member in arenas.type_members.iter() {
      if let TypeMemberKind::Property(sig) = &member.kind {
        if sig.readonly {
          if let Some(type_expr) = sig.type_annotation {
            signals.push(SemanticSignal::ReadonlyTypePosition { type_expr });
          }
        }
      }
    }
  }

  signals.sort_by_key(|sig| {
    (
      signal_span_start(sig, file, body),
      signal_kind_rank(sig),
      signal_tiebreak(sig),
    )
  });
  signals
}

fn is_promise_all_call(body: &Body, names: &NameInterner, callee: ExprId) -> bool {
  let Some(callee_expr) = body.exprs.get(callee.0 as usize) else {
    return false;
  };
  let ExprKind::Member(member) = &callee_expr.kind else {
    return false;
  };

  let Some(obj_expr) = body.exprs.get(member.object.0 as usize) else {
    return false;
  };
  let ExprKind::Ident(obj) = &obj_expr.kind else {
    return false;
  };
  let ObjectKey::Ident(prop) = &member.property else {
    return false;
  };

  names.resolve(*obj) == Some("Promise") && names.resolve(*prop) == Some("all")
}

fn is_private_name(key: &ObjectKey, names: &NameInterner) -> bool {
  match key {
    ObjectKey::Ident(id) => names.resolve(*id).is_some_and(|name| name.starts_with('#')),
    _ => false,
  }
}

fn signal_span_start(signal: &SemanticSignal, file: &HirFile, body: &Body) -> u32 {
  match *signal {
    SemanticSignal::PromiseAll { expr }
    | SemanticSignal::AsConstAssertion { expr }
    | SemanticSignal::TypeAssertion { expr }
    | SemanticSignal::NonNullAssertion { expr }
    | SemanticSignal::PrivateFieldAccess { expr } => body.exprs[expr.0 as usize].span.start,
    SemanticSignal::ConstBinding { stmt, .. } | SemanticSignal::ForAwaitOf { stmt } => {
      body.stmts[stmt.0 as usize].span.start
    }
    SemanticSignal::AsyncFunctionWithoutAwait { body: body_id, .. } => file
      .span_map
      .body_span(body_id)
      .map(|range| range.start)
      .unwrap_or(body.span.start),
    SemanticSignal::ReadonlyTypePosition { type_expr } => file
      .span_map
      .type_expr_span(body.owner, type_expr)
      .map(|range| range.start)
      .unwrap_or(body.span.start),
  }
}

fn signal_kind_rank(signal: &SemanticSignal) -> u8 {
  match signal {
    SemanticSignal::PromiseAll { .. } => 0,
    SemanticSignal::AsyncFunctionWithoutAwait { .. } => 1,
    SemanticSignal::ConstBinding { .. } => 2,
    SemanticSignal::ReadonlyTypePosition { .. } => 3,
    SemanticSignal::AsConstAssertion { .. } => 4,
    SemanticSignal::PrivateFieldAccess { .. } => 5,
    SemanticSignal::TypeAssertion { .. } => 6,
    SemanticSignal::NonNullAssertion { .. } => 7,
    SemanticSignal::ForAwaitOf { .. } => 8,
  }
}

fn signal_tiebreak(signal: &SemanticSignal) -> u64 {
  match *signal {
    SemanticSignal::PromiseAll { expr }
    | SemanticSignal::AsConstAssertion { expr }
    | SemanticSignal::TypeAssertion { expr }
    | SemanticSignal::NonNullAssertion { expr }
    | SemanticSignal::PrivateFieldAccess { expr } => expr.0 as u64,
    SemanticSignal::AsyncFunctionWithoutAwait { def, body } => def.0 ^ body.0,
    SemanticSignal::ConstBinding {
      stmt,
      declarator_index,
    } => ((stmt.0 as u64) << 32) | (declarator_index as u64),
    SemanticSignal::ReadonlyTypePosition { type_expr } => type_expr.0 as u64,
    SemanticSignal::ForAwaitOf { stmt } => stmt.0 as u64,
  }
}
