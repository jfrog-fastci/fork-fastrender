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

  /// Returns `true` when the expression is a known array/readonly-array/tuple.
  #[cfg(feature = "typed")]
  fn expr_is_array(&self, body: BodyId, expr: ExprId) -> bool {
    let Some(ty) = self.expr_type(body, expr) else {
      return false;
    };
    matches!(
      self.type_kind(ty),
      Some(TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. })
    )
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
}
