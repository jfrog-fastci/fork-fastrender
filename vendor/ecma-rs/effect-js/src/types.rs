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
    let Some(ty) = self.expr_type(body, expr) else {
      return false;
    };
    match self.type_kind(ty) {
      Some(TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. }) => true,
      Some(TypeKindSummary::Ref { def, .. }) => matches!(
        self.def_name(def).as_deref(),
        Some("Array" | "ReadonlyArray")
      ),
      _ => false,
    }
  }

  /// Returns `true` when the expression is a known string (including literal/template types).
  #[cfg(feature = "typed")]
  fn expr_is_string(&self, body: BodyId, expr: ExprId) -> bool {
    let Some(ty) = self.expr_type(body, expr) else {
      return false;
    };
    matches!(
      self.type_kind(ty),
      Some(
        TypeKindSummary::String | TypeKindSummary::StringLiteral(_) | TypeKindSummary::TemplateLiteral
      )
    )
  }

  /// Returns `true` when the expression's top-level type is a reference to a
  /// definition with the given name (e.g. `"Map"` or `"Promise"`).
  #[cfg(feature = "typed")]
  fn expr_is_named_ref(&self, body: BodyId, expr: ExprId, expected: &str) -> bool {
    let Some(ty) = self.expr_type(body, expr) else {
      return false;
    };
    let Some(TypeKindSummary::Ref { def, .. }) = self.type_kind(ty) else {
      return false;
    };
    self.def_name(def).as_deref() == Some(expected)
  }
}
