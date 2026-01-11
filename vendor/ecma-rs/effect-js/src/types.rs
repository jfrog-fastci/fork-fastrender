use hir_js::{BodyId, ExprId, PatId};

#[cfg(feature = "typed")]
pub type TypeId = typecheck_ts::TypeId;
#[cfg(not(feature = "typed"))]
pub type TypeId = ();

#[cfg(feature = "typed")]
pub use typecheck_ts::TypeKindSummary;
#[cfg(not(feature = "typed"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKindSummary {}

/// Abstract source of coarse type information for HIR nodes.
///
/// When the `typed` feature is disabled, all methods are expected to return
/// `None` and the derived helpers are absent.
pub trait TypeProvider {
  fn expr_type(&self, body: BodyId, expr: ExprId) -> Option<TypeId>;

  fn pat_type(&self, body: BodyId, pat: PatId) -> Option<TypeId>;

  /// Human-readable (best-effort) display string for a type.
  ///
  /// This is optional and intended for conservative heuristics that need
  /// nominal names (e.g. `Promise<T>`). Implementations should return `None`
  /// when rendering is not available or is too expensive.
  ///
  /// Typed implementations may forward to `typecheck_ts::Program::display_type`.
  fn display_type(&self, _ty: TypeId) -> Option<String> {
    None
  }

  /// Convenience wrapper to display an expression's type.
  #[cfg(feature = "typed")]
  fn expr_type_display(&self, body: BodyId, expr: ExprId) -> Option<String> {
    let ty = self.expr_type(body, expr)?;
    self.display_type(ty)
  }

  /// Top-level kind summary for a type.
  ///
  /// In untyped builds this always returns `None`.
  fn type_kind(&self, _ty: TypeId) -> Option<TypeKindSummary> {
    None
  }

  /// Downcast hook for typed-only resolvers that need `typecheck-ts` semantic APIs.
  #[cfg(feature = "typed")]
  fn as_typed_program(&self) -> Option<&crate::typed::TypedProgram> {
    None
  }

  /// Friendly name for a referenced definition.
  ///
  /// Typed-only helper used for coarse instance-method resolution (e.g. proving
  /// that `m.get(...)` is `Map.prototype.get`).
  #[cfg(feature = "typed")]
  fn def_name(&self, def: typecheck_ts::DefId) -> Option<String> {
    let _ = def;
    None
  }

  /// Returns `true` when the expression is a known array/readonly-array/tuple.
  #[cfg(feature = "typed")]
  fn expr_is_array(&self, body: BodyId, expr: ExprId) -> bool {
    let Some(mut ty) = self.expr_type(body, expr) else {
      return false;
    };

    // `typecheck-ts` models many type aliases as `Ref` nodes. Since `effect-js`
    // uses unexpanded kinds (to preserve names like `Map`/`Promise`), we need to
    // peel through type aliases to reliably detect arrays.
    const MAX_ALIAS_DEPTH: usize = 8;
    for _ in 0..MAX_ALIAS_DEPTH {
      match self.type_kind(ty) {
        Some(TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. }) => return true,
        Some(TypeKindSummary::Ref { def, .. }) => {
          if matches!(self.def_name(def).as_deref(), Some("Array" | "ReadonlyArray")) {
            return true;
          }

          let Some(typed) = self.as_typed_program() else {
            return false;
          };
          let Some(typecheck_ts::DefKind::TypeAlias(_)) = typed.def_kind(def) else {
            return false;
          };
          ty = typed.program().declared_type_of_def_interned(def);
        }
        _ => return false,
      }
    }
    false
  }

  /// Returns `true` when the expression is a known string (including literal/template types).
  #[cfg(feature = "typed")]
  fn expr_is_string(&self, body: BodyId, expr: ExprId) -> bool {
    let Some(mut ty) = self.expr_type(body, expr) else {
      return false;
    };

    const MAX_ALIAS_DEPTH: usize = 8;
    for _ in 0..MAX_ALIAS_DEPTH {
      match self.type_kind(ty) {
        Some(
          TypeKindSummary::String | TypeKindSummary::StringLiteral(_) | TypeKindSummary::TemplateLiteral,
        ) => return true,
        Some(TypeKindSummary::Ref { def, .. }) => {
          let Some(typed) = self.as_typed_program() else {
            return false;
          };
          let Some(typecheck_ts::DefKind::TypeAlias(_)) = typed.def_kind(def) else {
            return false;
          };
          ty = typed.program().declared_type_of_def_interned(def);
        }
        _ => return false,
      }
    }
    false
  }

  /// Returns `true` when the expression's top-level type is a reference to a
  /// definition with the given name (e.g. `"Map"` or `"Promise"`).
  #[cfg(feature = "typed")]
  fn expr_is_named_ref(&self, body: BodyId, expr: ExprId, expected: &str) -> bool {
    let Some(mut ty) = self.expr_type(body, expr) else {
      return false;
    };

    const MAX_ALIAS_DEPTH: usize = 8;
    for _ in 0..MAX_ALIAS_DEPTH {
      let Some(TypeKindSummary::Ref { def, .. }) = self.type_kind(ty) else {
        return false;
      };

      if self.def_name(def).as_deref() == Some(expected) {
        return true;
      }

      let Some(typed) = self.as_typed_program() else {
        return false;
      };
      let Some(typecheck_ts::DefKind::TypeAlias(_)) = typed.def_kind(def) else {
        return false;
      };
      ty = typed.program().declared_type_of_def_interned(def);
    }
    false
  }
}
