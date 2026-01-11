use hir_js::{BodyId, ExprId};
use std::fmt;
use std::fmt::Formatter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truthiness {
  AlwaysTruthy,
  AlwaysFalsy,
}

/// Best-effort runtime value-type summary used by downstream analyses.
///
/// This is intentionally a *lightweight* abstraction that `optimize-js` can
/// preserve in the IR even when the full TypeScript type checker is not
/// available at analysis time. In untyped builds most values default to
/// `Unknown`, but trivially-known values (e.g. literal constants) may still carry
/// a precise summary.
///
/// The representation is a bitmask so we can cheaply represent union types.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ValueTypeSummary(pub(crate) u16);

impl ValueTypeSummary {
  pub const UNKNOWN: Self = Self(0);

  pub const NULL: Self = Self(1 << 0);
  pub const UNDEFINED: Self = Self(1 << 1);
  pub const BOOLEAN: Self = Self(1 << 2);
  pub const NUMBER: Self = Self(1 << 3);
  pub const STRING: Self = Self(1 << 4);
  pub const BIGINT: Self = Self(1 << 5);
  pub const SYMBOL: Self = Self(1 << 6);
  pub const FUNCTION: Self = Self(1 << 7);
  pub const OBJECT: Self = Self(1 << 8);

  pub const NULLISH: Self = Self(Self::NULL.0 | Self::UNDEFINED.0);

  pub fn is_unknown(self) -> bool {
    self == Self::UNKNOWN
  }

  pub fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }

  pub fn excludes_nullish(self) -> bool {
    // `NULLISH` is a union of {null, undefined}. `contains()` checks for a superset, so it would
    // only return true if *both* bits are set. For our purposes we care about whether the summary
    // may include *either* nullish value, so we check for any overlap.
    !self.is_unknown() && (self.0 & Self::NULLISH.0) == 0
  }

  pub fn is_definitely_string(self) -> bool {
    self == Self::STRING
  }

  pub fn is_definitely_number(self) -> bool {
    self == Self::NUMBER
  }

  pub fn is_definitely_bigint(self) -> bool {
    self == Self::BIGINT
  }
}

impl fmt::Debug for ValueTypeSummary {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    if self.is_unknown() {
      return write!(f, "Unknown");
    }
    let mut first = true;
    let mut write_flag = |name: &str| {
      if !first {
        let _ = write!(f, "|");
      }
      first = false;
      write!(f, "{name}")
    };

    if self.contains(Self::NULL) {
      write_flag("Null")?;
    }
    if self.contains(Self::UNDEFINED) {
      write_flag("Undefined")?;
    }
    if self.contains(Self::BOOLEAN) {
      write_flag("Boolean")?;
    }
    if self.contains(Self::NUMBER) {
      write_flag("Number")?;
    }
    if self.contains(Self::STRING) {
      write_flag("String")?;
    }
    if self.contains(Self::BIGINT) {
      write_flag("BigInt")?;
    }
    if self.contains(Self::SYMBOL) {
      write_flag("Symbol")?;
    }
    if self.contains(Self::FUNCTION) {
      write_flag("Function")?;
    }
    if self.contains(Self::OBJECT) {
      write_flag("Object")?;
    }
    Ok(())
  }
}

impl std::ops::BitOr for ValueTypeSummary {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

impl std::ops::BitOrAssign for ValueTypeSummary {
  fn bitor_assign(&mut self, rhs: Self) {
    self.0 |= rhs.0;
  }
}

impl std::ops::BitAnd for ValueTypeSummary {
  type Output = Self;

  fn bitand(self, rhs: Self) -> Self::Output {
    Self(self.0 & rhs.0)
  }
}

impl std::ops::BitAndAssign for ValueTypeSummary {
  fn bitand_assign(&mut self, rhs: Self) {
    self.0 &= rhs.0;
  }
}

#[cfg(test)]
mod tests {
  use super::ValueTypeSummary;

  #[test]
  fn value_type_summary_excludes_nullish_semantics() {
    assert!(!ValueTypeSummary::UNKNOWN.excludes_nullish());
    assert!(!ValueTypeSummary::NULL.excludes_nullish());
    assert!(!ValueTypeSummary::UNDEFINED.excludes_nullish());
    assert!(!ValueTypeSummary::NULLISH.excludes_nullish());
    assert!(ValueTypeSummary::STRING.excludes_nullish());
    assert!(!(ValueTypeSummary::STRING | ValueTypeSummary::NULL).excludes_nullish());
  }
}

/// Optional TypeScript type information for the optimizer.
///
/// The optimizer is designed to compile without a dependency on `typecheck-ts`.
/// When the `typed` feature is enabled callers can populate this context with a
/// `typecheck_ts::Program` and per-body expression type tables.
#[derive(Clone, Default)]
pub struct TypeContext {
  #[cfg(feature = "typed")]
  pub(crate) program: Option<std::sync::Arc<typecheck_ts::Program>>,
  #[cfg(feature = "typed")]
  pub(crate) expr_types: ahash::HashMap<BodyId, Vec<Option<typecheck_ts::TypeId>>>,
}

impl fmt::Debug for TypeContext {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut s = f.debug_struct("TypeContext");
    #[cfg(feature = "typed")]
    {
      s.field("has_program", &self.program.is_some());
      s.field("body_count", &self.expr_types.len());
    }
    s.finish()
  }
}

impl TypeContext {
  /// Type identifier for a HIR expression, if available.
  pub fn expr_type(&self, body: BodyId, expr: ExprId) -> Option<TypeId> {
    #[cfg(feature = "typed")]
    {
      self
        .expr_types
        .get(&body)
        .and_then(|types| types.get(expr.0 as usize).copied())
        .flatten()
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      None
    }
  }

  /// If `expr` is statically typed as a boolean literal, return that literal value.
  pub fn bool_literal_expr(&self, body: BodyId, expr: ExprId) -> Option<bool> {
    #[cfg(feature = "typed")]
    {
      let program = self.program.as_ref()?;
      let ty = self.expr_type(body, expr)?;
      match program.type_kind(ty) {
        typecheck_ts::TypeKindSummary::BooleanLiteral(value) => Some(value),
        _ => None,
      }
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      None
    }
  }

  /// If `expr` is statically typed as always truthy or always falsy, return that truthiness.
  pub fn expr_truthiness(&self, body: BodyId, expr: ExprId) -> Option<Truthiness> {
    #[cfg(feature = "typed")]
    {
      let program = self.program.as_ref()?;
      if !program.compiler_options().strict_null_checks {
        return None;
      }
      let ty = self.expr_type(body, expr)?;
      type_truthiness(program, ty, 0)
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      None
    }
  }

  /// Returns `true` if the expression type is known to exclude `null | undefined`.
  pub fn expr_excludes_nullish(&self, body: BodyId, expr: ExprId) -> bool {
    #[cfg(feature = "typed")]
    {
      let Some(program) = self.program.as_ref() else {
        return false;
      };
      let Some(ty) = self.expr_type(body, expr) else {
        return false;
      };
      type_excludes_nullish(program, ty, 0)
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      false
    }
  }

  /// Returns the JavaScript `typeof` tag for the expression when it is known.
  ///
  /// This is intentionally conservative; if we cannot reliably map the
  /// TypeScript type to a single runtime `typeof` string we return `None` and
  /// callers should fall back to untyped behaviour.
  pub fn expr_typeof_string(&self, body: BodyId, expr: ExprId) -> Option<&'static str> {
    #[cfg(feature = "typed")]
    {
      let program = self.program.as_ref()?;
      let ty = self.expr_type(body, expr)?;
      type_to_typeof_string(program, ty, 0)
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      None
    }
  }

  /// Returns `true` when the expression is known to evaluate to a primitive boolean.
  ///
  /// This is conservative; if the type information is unavailable or the type
  /// cannot be proven to be `boolean` (including boolean literals), returns
  /// `false`.
  pub fn expr_is_boolean(&self, body: BodyId, expr: ExprId) -> bool {
    #[cfg(feature = "typed")]
    {
      let Some(program) = self.program.as_ref() else {
        return false;
      };
      let Some(ty) = self.expr_type(body, expr) else {
        return false;
      };
      type_is_boolean(program, ty, 0)
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      false
    }
  }

  /// Best-effort runtime type summary for the expression, when available.
  ///
  /// When `typed` is disabled or the type information cannot be mapped into a
  /// stable runtime category, returns [`ValueTypeSummary::UNKNOWN`].
  pub fn expr_value_type_summary(&self, body: BodyId, expr: ExprId) -> ValueTypeSummary {
    #[cfg(feature = "typed")]
    {
      let Some(program) = self.program.as_ref() else {
        return ValueTypeSummary::UNKNOWN;
      };
      let Some(ty) = self.expr_type(body, expr) else {
        return ValueTypeSummary::UNKNOWN;
      };
      type_value_summary(program, ty, 0)
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = (body, expr);
      ValueTypeSummary::UNKNOWN
    }
  }
}

#[cfg(feature = "typed")]
pub type TypeId = typecheck_ts::TypeId;

#[cfg(not(feature = "typed"))]
pub type TypeId = ();

#[cfg(feature = "typed")]
impl TypeContext {
  /// Build a [`TypeContext`] from a `typecheck-ts` program.
  ///
  /// When possible we prefer an ID-aligned mapping between `hir-js` bodies and
  /// `typecheck-ts` [`typecheck_ts::BodyCheckResult`] side tables (matching on
  /// `BodyId` and validating the expression counts). If that fails for a
  /// particular body, we fall back to span-based matching as a conservative
  /// best-effort.
  pub fn from_typecheck_program(
    program: std::sync::Arc<typecheck_ts::Program>,
    file: typecheck_ts::FileId,
    lower: &hir_js::LowerResult,
  ) -> Self {
    use ahash::HashMapExt;
    use diagnostics::TextRange;

    let mut checked_expr_types: ahash::HashMap<BodyId, Vec<Option<typecheck_ts::TypeId>>> =
      ahash::HashMap::new();
    let mut span_to_ty: ahash::HashMap<TextRange, Option<typecheck_ts::TypeId>> =
      ahash::HashMap::new();

    for body_id in program.bodies_in_file(file) {
      let checked = program.check_body(body_id);
      checked_expr_types.insert(
        body_id,
        checked.expr_types().iter().copied().map(Some).collect(),
      );
      for (&span, &ty) in checked.expr_spans().iter().zip(checked.expr_types().iter()) {
        span_to_ty
          .entry(span)
          .and_modify(|existing| {
            if existing.map(|existing| existing != ty).unwrap_or(false) {
              *existing = None;
            }
          })
          .or_insert(Some(ty));
      }
    }

    let mut expr_types = ahash::HashMap::new();
    for (body_id, idx) in lower.body_index.iter() {
      let body = &lower.bodies[*idx];
      if let Some(types) = checked_expr_types
        .get(body_id)
        .filter(|types| types.len() == body.exprs.len())
      {
        expr_types.insert(*body_id, types.clone());
        continue;
      }

      let mut body_types = Vec::with_capacity(body.exprs.len());
      for expr in body.exprs.iter() {
        let ty = span_to_ty.get(&expr.span).and_then(|ty| *ty);
        body_types.push(ty);
      }
      expr_types.insert(*body_id, body_types);
    }

    Self {
      program: Some(program),
      expr_types,
    }
  }

  /// Build a [`TypeContext`] from a `typecheck-ts` program assuming the
  /// optimizer is using the same `hir-js` lowering cached inside the program.
  ///
  /// When the optimizer and type checker share a [`hir_js::LowerResult`] (for
  /// example via `typecheck_ts::Program::hir_lowered`) then `BodyId`/`ExprId`
  /// values are guaranteed to line up and we can map types directly without
  /// span matching.
  pub fn from_typecheck_program_aligned(
    program: std::sync::Arc<typecheck_ts::Program>,
    file: typecheck_ts::FileId,
    lower: &hir_js::LowerResult,
  ) -> Self {
    use ahash::HashMapExt;

    let _ = file;
    let mut expr_types = ahash::HashMap::new();

    for (body_id, idx) in lower.body_index.iter() {
      let checked = program.check_body(*body_id);
      let body = &lower.bodies[*idx];
      let mut body_types = Vec::with_capacity(body.exprs.len());
      for idx in 0..body.exprs.len() {
        body_types.push(checked.expr_types().get(idx).copied());
      }
      expr_types.insert(*body_id, body_types);
    }

    Self {
      program: Some(program),
      expr_types,
    }
  }
}

#[cfg(feature = "typed")]
fn type_excludes_nullish(
  program: &typecheck_ts::Program,
  ty: typecheck_ts::TypeId,
  depth: u8,
) -> bool {
  if !program.compiler_options().strict_null_checks {
    return false;
  }
  // Avoid pathological recursion for self-referential aliases.
  if depth >= 8 {
    return false;
  }

  use types_ts_interned::TypeKind as K;
  match program.interned_type_kind(ty) {
    K::Any
    | K::Unknown
    | K::Void
    | K::Null
    | K::Undefined
    | K::This
    | K::Infer { .. }
    | K::TypeParam(_)
    | K::Predicate { .. }
    | K::Conditional { .. }
    | K::Mapped(_)
    | K::TemplateLiteral(_)
    | K::IndexedAccess { .. }
    | K::KeyOf(_) => false,
    // `never` contains no values and is trivially non-nullish.
    K::Never => true,
    K::Union(members) => members
      .into_iter()
      .all(|member| type_excludes_nullish(program, member, depth + 1)),
    K::Intersection(members) => members
      .into_iter()
      .any(|member| type_excludes_nullish(program, member, depth + 1)),
    K::Ref { def, .. } => type_excludes_nullish(
      program,
      program.declared_type_of_def_interned(def),
      depth + 1,
    ),
    K::EmptyObject => true,
    _ => true,
  }
}

#[cfg(feature = "typed")]
fn type_to_typeof_string(
  program: &typecheck_ts::Program,
  ty: typecheck_ts::TypeId,
  depth: u8,
) -> Option<&'static str> {
  if depth >= 8 {
    return None;
  }

  use types_ts_interned::TypeKind as K;
  match program.interned_type_kind(ty) {
    K::Boolean | K::BooleanLiteral(_) => Some("boolean"),
    K::Number | K::NumberLiteral(_) => Some("number"),
    K::String | K::StringLiteral(_) => Some("string"),
    K::BigInt | K::BigIntLiteral(_) => Some("bigint"),
    K::Symbol | K::UniqueSymbol => Some("symbol"),
    K::Undefined | K::Void => Some("undefined"),
    K::Null => Some("object"),
    K::Callable { .. } => Some("function"),
    // We can only return a `typeof` result when it is uniquely determined by
    // the type. Note that the TypeScript `{}`/`object` supertypes can include
    // callable values, so they do *not* map to a single `typeof` tag.
    K::Tuple(_) | K::Array { .. } => Some("object"),
    K::Ref { def, .. } => type_to_typeof_string(
      program,
      program.declared_type_of_def_interned(def),
      depth + 1,
    ),
    K::Union(members) => {
      let mut tag: Option<&'static str> = None;
      for member in members {
        if matches!(program.interned_type_kind(member), K::Never) {
          continue;
        }
        let member_tag = type_to_typeof_string(program, member, depth + 1)?;
        match tag {
          None => tag = Some(member_tag),
          Some(existing) if existing == member_tag => {}
          _ => return None,
        }
      }
      tag
    }
    K::Intersection(members) => {
      let mut tag: Option<&'static str> = None;
      for member in members {
        if matches!(program.interned_type_kind(member), K::Never) {
          continue;
        }
        let Some(member_tag) = type_to_typeof_string(program, member, depth + 1) else {
          continue;
        };
        match tag {
          None => tag = Some(member_tag),
          Some(existing) if existing == member_tag => {}
          _ => return None,
        }
      }
      tag
    }
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn type_truthiness(
  program: &typecheck_ts::Program,
  ty: typecheck_ts::TypeId,
  depth: u8,
) -> Option<Truthiness> {
  if depth >= 8 {
    return None;
  }

  use types_ts_interned::TypeKind as K;
  match program.interned_type_kind(ty) {
    K::Null | K::Undefined | K::Void => Some(Truthiness::AlwaysFalsy),
    K::BooleanLiteral(value) => Some(if value {
      Truthiness::AlwaysTruthy
    } else {
      Truthiness::AlwaysFalsy
    }),
    K::StringLiteral(_) => match program.type_kind(ty) {
      typecheck_ts::TypeKindSummary::StringLiteral(value) => Some(if value.is_empty() {
        Truthiness::AlwaysFalsy
      } else {
        Truthiness::AlwaysTruthy
      }),
      _ => None,
    },
    K::NumberLiteral(value) => {
      let value = value.0;
      Some(if value == 0.0 || value.is_nan() {
        Truthiness::AlwaysFalsy
      } else {
        Truthiness::AlwaysTruthy
      })
    }
    K::BigIntLiteral(value) => Some(if value == num_bigint::BigInt::from(0) {
      Truthiness::AlwaysFalsy
    } else {
      Truthiness::AlwaysTruthy
    }),
    K::Tuple(_) | K::Array { .. } | K::Callable { .. } | K::Object(_) | K::EmptyObject => {
      Some(Truthiness::AlwaysTruthy)
    }
    K::Symbol | K::UniqueSymbol => Some(Truthiness::AlwaysTruthy),
    K::Union(members) => {
      let mut acc: Option<Truthiness> = None;
      for member in members {
        if matches!(program.interned_type_kind(member), K::Never) {
          continue;
        }
        let member_truthiness = type_truthiness(program, member, depth + 1)?;
        match acc {
          None => acc = Some(member_truthiness),
          Some(existing) if existing == member_truthiness => {}
          _ => return None,
        }
      }
      acc
    }
    K::Intersection(members) => {
      let mut has_truthy = false;
      let mut has_falsy = false;
      for member in members {
        let Some(member_truthiness) = type_truthiness(program, member, depth + 1) else {
          continue;
        };
        match member_truthiness {
          Truthiness::AlwaysTruthy => has_truthy = true,
          Truthiness::AlwaysFalsy => has_falsy = true,
        }
      }
      match (has_truthy, has_falsy) {
        (true, false) => Some(Truthiness::AlwaysTruthy),
        (false, true) => Some(Truthiness::AlwaysFalsy),
        _ => None,
      }
    }
    K::Ref { def, .. } => type_truthiness(
      program,
      program.declared_type_of_def_interned(def),
      depth + 1,
    ),
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn type_is_boolean(program: &typecheck_ts::Program, ty: typecheck_ts::TypeId, depth: u8) -> bool {
  if depth >= 8 {
    return false;
  }

  use types_ts_interned::IntrinsicKind;
  use types_ts_interned::TypeKind as K;

  match program.interned_type_kind(ty) {
    K::Boolean | K::BooleanLiteral(_) => true,
    // `never` contains no runtime values, so it is vacuously a boolean.
    K::Never => true,
    K::Union(members) => members
      .into_iter()
      .all(|member| type_is_boolean(program, member, depth + 1)),
    // Intersection types narrow; if any constituent is boolean the result is a subset of boolean.
    K::Intersection(members) => members
      .into_iter()
      .any(|member| type_is_boolean(program, member, depth + 1)),
    K::Ref { def, .. } => type_is_boolean(
      program,
      program.declared_type_of_def_interned(def),
      depth + 1,
    ),
    K::Intrinsic { kind, ty } => match kind {
      IntrinsicKind::NoInfer => type_is_boolean(program, ty, depth + 1),
      _ => false,
    },
    _ => false,
  }
}

#[cfg(feature = "typed")]
fn type_value_summary(
  program: &typecheck_ts::Program,
  ty: typecheck_ts::TypeId,
  depth: u8,
) -> ValueTypeSummary {
  if depth >= 8 {
    return ValueTypeSummary::UNKNOWN;
  }

  use types_ts_interned::IntrinsicKind;
  use types_ts_interned::TypeKind as K;

  match program.interned_type_kind(ty) {
    K::Any | K::Unknown | K::This | K::Infer { .. } | K::TypeParam(_) | K::Predicate { .. } => {
      ValueTypeSummary::UNKNOWN
    }
    K::Never => ValueTypeSummary::UNKNOWN,
    K::Null => ValueTypeSummary::NULL,
    K::Undefined | K::Void => ValueTypeSummary::UNDEFINED,
    K::Boolean | K::BooleanLiteral(_) => ValueTypeSummary::BOOLEAN,
    K::Number | K::NumberLiteral(_) => ValueTypeSummary::NUMBER,
    K::String | K::StringLiteral(_) | K::TemplateLiteral(_) => ValueTypeSummary::STRING,
    K::BigInt | K::BigIntLiteral(_) => ValueTypeSummary::BIGINT,
    K::Symbol | K::UniqueSymbol => ValueTypeSummary::SYMBOL,
    K::Callable { .. } => ValueTypeSummary::FUNCTION,
    K::Tuple(_) | K::Array { .. } | K::Object(_) | K::EmptyObject => ValueTypeSummary::OBJECT,
    K::Union(members) => {
      let mut acc = ValueTypeSummary::UNKNOWN;
      for member in members {
        if matches!(program.interned_type_kind(member), K::Never) {
          continue;
        }
        let member_summary = type_value_summary(program, member, depth + 1);
        if member_summary.is_unknown() {
          return ValueTypeSummary::UNKNOWN;
        }
        acc |= member_summary;
      }
      acc
    }
    K::Intersection(members) => {
      let mut acc: Option<ValueTypeSummary> = None;
      for member in members {
        let member_summary = type_value_summary(program, member, depth + 1);
        if member_summary.is_unknown() {
          continue;
        }
        acc = Some(match acc {
          None => member_summary,
          Some(existing) => existing & member_summary,
        });
      }
      acc.unwrap_or(ValueTypeSummary::UNKNOWN)
    }
    K::Ref { def, .. } => type_value_summary(
      program,
      program.declared_type_of_def_interned(def),
      depth + 1,
    ),
    K::Intrinsic { kind, ty } => match kind {
      IntrinsicKind::NoInfer => type_value_summary(program, ty, depth + 1),
      _ => ValueTypeSummary::UNKNOWN,
    },
    _ => ValueTypeSummary::UNKNOWN,
  }
}
