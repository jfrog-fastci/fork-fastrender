use hir_js::ids::BodyPath;
use hir_js::{Body, BodyId, BodyKind, ExprId, ExprKind, ForHead, ForInit, StmtId, StmtKind, VarDeclKind};

/// Semantic cues surfaced from the user's source code.
///
/// These are intentionally *not* "optimization patterns": they represent explicit
/// developer intent (TypeScript assertions, `for await (...)`, etc.) that later
/// phases can exploit without needing to change the HIR itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SemanticSignal {
  /// An `async` function body that contains no await-like constructs.
  AsyncFunctionNeverAwaits { body: BodyId },

  /// `expr as const`.
  ConstAssertion { expr: ExprId },

  /// `expr as T` / `<T>expr`.
  TypeAssertion { expr: ExprId },

  /// `expr!`.
  NonNullAssertion { expr: ExprId },

  /// `const ...`.
  VarDeclConst { stmt: StmtId },

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
  // `AsyncFunctionNeverAwaits`.
  let mut has_await_like = false;

  // Expression signals (deterministic: increasing `ExprId`).
  for (idx, expr) in body.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    match &expr.kind {
      ExprKind::TypeAssertion {
        const_assertion: true,
        ..
      } => {
        tables.expr_signals[idx].push(SemanticSignal::ConstAssertion { expr: expr_id });
      }
      ExprKind::TypeAssertion {
        const_assertion: false,
        ..
      } => {
        tables.expr_signals[idx].push(SemanticSignal::TypeAssertion { expr: expr_id });
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
          tables.stmt_signals[idx].push(SemanticSignal::VarDeclConst { stmt: stmt_id });
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
      tables.body_signals.push(SemanticSignal::AsyncFunctionNeverAwaits {
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
