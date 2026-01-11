use hir_js::{BodyId, DefId, ExprId};
use std::fmt;

use types_ts_interned::{Store, TypeId};

/// Abstract source of `types-ts-interned` types for HIR expressions.
///
/// This keeps `effect-js` logic generic so it can be fed by `typecheck-ts` or
/// other typing frontends.
pub trait TypeProvider {
  fn type_of_expr(&self, body: BodyId, expr: ExprId) -> TypeId;

  fn store(&self) -> &Store;

  /// Optional hook to resolve a `DefId` referenced by `TypeKind::Ref`.
  ///
  /// Some APIs (e.g. `Promise.prototype.then`) need nominal type identity, which
  /// requires checking the referenced definition name. When the provider cannot
  /// resolve `DefId`s (e.g. it only has a type store), it can return `None` and
  /// `effect-js` will conservatively treat the type as unknown.
  fn def_name(&self, _def: DefId) -> Option<String> {
    None
  }
}

/// `TypeProvider` adapter for `typecheck-ts`.
///
/// `typecheck-ts::Program` owns the interned store behind a mutex, so we clone
/// the `Arc<TypeStore>` once and retain it for cheap access in `store()`.
pub struct TypecheckProgram<'a> {
  program: &'a typecheck_ts::Program,
  store: std::sync::Arc<Store>,
}

impl fmt::Debug for TypecheckProgram<'_> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Avoid requiring `typecheck_ts::Program: Debug`.
    f.debug_struct("TypecheckProgram").finish_non_exhaustive()
  }
}

impl<'a> TypecheckProgram<'a> {
  pub fn new(program: &'a typecheck_ts::Program) -> Self {
    Self {
      program,
      store: program.interned_type_store(),
    }
  }
}

impl TypeProvider for TypecheckProgram<'_> {
  fn type_of_expr(&self, body: BodyId, expr: ExprId) -> TypeId {
    let res = self.program.check_body(body);
    res
      .expr_type(expr)
      .unwrap_or_else(|| self.store.primitive_ids().unknown)
  }

  fn store(&self) -> &Store {
    &self.store
  }

  fn def_name(&self, def: DefId) -> Option<String> {
    self.program.def_name(def)
  }
}
