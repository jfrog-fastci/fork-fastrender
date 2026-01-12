//! IL-level analysis metadata containers.
//!
//! These types are intended to be lightweight and deterministic so analyses can
//! attach results directly onto IL instructions without affecting existing
//! serialization/debug output.
//!
//! IMPORTANT: `Inst.meta` is currently skipped under the `serde` feature to
//! avoid schema changes for `optimize-js-debugger`.

use crate::symbol::semantics::SymbolId;
use crate::types::{TypeId, ValueTypeSummary};
use effect_model::{EffectFlags, EffectSummary, ThrowBehavior};
use hir_js::ExprId;
use std::cmp::Ordering;
use std::collections::BTreeSet;

#[cfg(feature = "serde")]
fn serialize_type_id<S>(value: &Option<TypeId>, serializer: S) -> Result<S::Ok, S::Error>
where
  S: serde::Serializer,
{
  use serde::Serialize;
  #[cfg(feature = "typed")]
  {
    value.map(|id| id.0).serialize(serializer)
  }
  #[cfg(not(feature = "typed"))]
  {
    value.serialize(serializer)
  }
}

#[cfg(feature = "serde")]
fn serialize_expr_id<S>(value: &Option<ExprId>, serializer: S) -> Result<S::Ok, S::Error>
where
  S: serde::Serializer,
{
  use serde::Serialize;
  value.map(|id| id.0).serialize(serializer)
}

#[cfg(feature = "serde")]
fn is_false(value: &bool) -> bool {
  !*value
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub enum EffectLocation {
  /// Heap memory reachable via object/array properties.
  Heap,
  /// Variables captured from an ancestor scope (including the global scope when in module/script
  /// mode).
  Foreign(SymbolId),
  /// Unknown/global identifier accesses in global mode.
  Unknown(String),
  /// Field-level heap effect for strict-native typed programs.
  ///
  /// This is only sound when the strict-native checker has ruled out JavaScript
  /// dynamic property behavior (notably prototype mutation and computed
  /// property access with non-constant keys). See `analysis/effect.rs` for the
  /// full set of assumptions.
  #[cfg(feature = "typed")]
  Field {
    shape: types_ts_interned::ShapeId,
    key: types_ts_interned::PropKey,
  },
  /// Effects on element storage for typed layouts (e.g. native arrays).
  ///
  /// Currently unused by `optimize-js`, but reserved for strict-native effect
  /// modeling.
  #[cfg(feature = "typed")]
  ArrayElements { elem: types_ts_interned::LayoutId },
}

impl Default for EffectLocation {
  fn default() -> Self {
    Self::Unknown(String::new())
  }
}

impl EffectLocation {
  fn discriminant(&self) -> u8 {
    match self {
      EffectLocation::Heap => 0,
      EffectLocation::Foreign(_) => 1,
      EffectLocation::Unknown(_) => 2,
      #[cfg(feature = "typed")]
      EffectLocation::Field { .. } => 3,
      #[cfg(feature = "typed")]
      EffectLocation::ArrayElements { .. } => 4,
    }
  }
}

#[cfg(feature = "typed")]
fn cmp_prop_key(a: &types_ts_interned::PropKey, b: &types_ts_interned::PropKey) -> Ordering {
  use types_ts_interned::PropKey;

  fn prop_key_discriminant(key: &PropKey) -> u8 {
    match key {
      PropKey::String(_) => 0,
      PropKey::Number(_) => 1,
      PropKey::Symbol(_) => 2,
    }
  }

  let discr = prop_key_discriminant(a).cmp(&prop_key_discriminant(b));
  if discr != Ordering::Equal {
    return discr;
  }
  match (a, b) {
    (PropKey::String(a), PropKey::String(b)) | (PropKey::Symbol(a), PropKey::Symbol(b)) => a.cmp(b),
    (PropKey::Number(a), PropKey::Number(b)) => a.cmp(b),
    _ => Ordering::Equal,
  }
}

impl Ord for EffectLocation {
  fn cmp(&self, other: &Self) -> Ordering {
    let discr = self.discriminant().cmp(&other.discriminant());
    if discr != Ordering::Equal {
      return discr;
    }

    match (self, other) {
      (EffectLocation::Heap, EffectLocation::Heap) => Ordering::Equal,
      (EffectLocation::Foreign(a), EffectLocation::Foreign(b)) => a.cmp(b),
      (EffectLocation::Unknown(a), EffectLocation::Unknown(b)) => a.cmp(b),
      #[cfg(feature = "typed")]
      (
        EffectLocation::Field {
          shape: a_shape,
          key: a_key,
        },
        EffectLocation::Field {
          shape: b_shape,
          key: b_key,
        },
      ) => a_shape
        .cmp(b_shape)
        .then_with(|| cmp_prop_key(a_key, b_key)),
      #[cfg(feature = "typed")]
      (
        EffectLocation::ArrayElements { elem: a_elem },
        EffectLocation::ArrayElements { elem: b_elem },
      ) => a_elem.cmp(b_elem),
      _ => Ordering::Equal,
    }
  }
}

impl PartialOrd for EffectLocation {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct EffectSet {
  pub reads: BTreeSet<EffectLocation>,
  pub writes: BTreeSet<EffectLocation>,
  pub summary: EffectSummary,
  pub unknown: bool,
}

impl EffectSet {
  pub fn is_default(&self) -> bool {
    self.reads.is_empty() && self.writes.is_empty() && self.summary.is_pure() && !self.unknown
  }

  pub fn is_pure(&self) -> bool {
    self.is_default()
  }

  pub fn mark_unknown(&mut self) {
    self.unknown = true;
    self.summary = EffectSummary::UNKNOWN;
  }

  pub fn merge(&mut self, other: &Self) {
    self.reads.extend(other.reads.iter().cloned());
    self.writes.extend(other.writes.iter().cloned());
    self.summary = EffectSummary::join(self.summary, other.summary);
    if other.unknown {
      self.mark_unknown();
    }
  }

  pub fn union(mut self, other: Self) -> Self {
    self.merge(&other);
    self
  }
}

/// Conservative purity lattice for JS operations.
///
/// Ordering is deterministic (via `Ord`) and reflects "less pure" moving upwards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Purity {
  /// No reads, writes, allocation, or unknown effects.
  Pure,
  /// May read program state but does not write/allocate/unknown.
  ReadOnly,
  /// Pure except for allocation.
  Allocating,
  /// May write state or has unknown effects.
  #[default]
  Impure,
}

impl Purity {
  pub fn is_default(&self) -> bool {
    matches!(self, Self::Impure)
  }

  pub fn from_effects(effects: &EffectSet) -> Self {
    if effects.unknown
      || !effects.writes.is_empty()
      || !matches!(effects.summary.throws, ThrowBehavior::Never)
    {
      return Self::Impure;
    }

    if !effects.summary.flags.is_empty() {
      // Allocating is only used when allocation is the *only* tracked effect, and only when we
      // don't also read observable state.
      if effects.summary.flags == EffectFlags::ALLOCATES && effects.reads.is_empty() {
        return Self::Allocating;
      }
      return Self::Impure;
    }

    if !effects.reads.is_empty() {
      return Self::ReadOnly;
    }

    Self::Pure
  }
}

/// Placeholder escape analysis result for a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum EscapeState {
  Unknown,
  NoEscape,
  Escapes,
}

impl Default for EscapeState {
  fn default() -> Self {
    Self::Unknown
  }
}

/// Best-effort string encoding classification for compile-time-known string values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum StringEncoding {
  Ascii,
  Latin1,
  Utf8,
  Unknown,
}

impl Default for StringEncoding {
  fn default() -> Self {
    Self::Unknown
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct TypeInfo {
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub string_encoding: Option<StringEncoding>,
}

impl TypeInfo {
  pub fn is_default(&self) -> bool {
    self.string_encoding.is_none()
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum OwnershipState {
  Owned,
  Borrowed,
  Shared,
  Unknown,
}

/// Hints for downstream lowering that can implement certain ownership transfers
/// without a clone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum InPlaceHint {
  /// This `VarAssign` is a move of an owned value and can be implemented as a transfer/no-clone in
  /// downstream lowering.
  MoveNoClone { src: u32, tgt: u32 },
}

impl Default for InPlaceHint {
  fn default() -> Self {
    Self::MoveNoClone { src: 0, tgt: 0 }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ArgUseMode {
  Borrow,
  Consume,
}

/// Parallelization metadata for semantic operations.
///
/// This is attached to semantic ops (e.g. `Array.prototype.map`, `Promise.all`) so downstream
/// backends can make a single local decision about whether to emit parallel lowering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum ParallelPlan {
  /// The operation may be lowered using CPU data-parallel execution (e.g. parallel map/filter).
  Parallelizable,
  /// The operation should spawn all async tasks concurrently and await completion (e.g.
  /// `Promise.all`).
  SpawnAll,
  /// The operation should spawn all async tasks concurrently, but the overall result is the first
  /// settled promise (e.g. `Promise.race`).
  SpawnAllButRaceResult,
  /// The operation should be lowered sequentially.
  NotParallelizable(ParallelReason),
}

impl ParallelPlan {
  pub fn is_parallelizable(&self) -> bool {
    matches!(
      self,
      ParallelPlan::Parallelizable | ParallelPlan::SpawnAll | ParallelPlan::SpawnAllButRaceResult
    )
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum ParallelReason {
  UnknownCallback,
  ImpureCallback,
  CallbackUnknownEffects,
  CallbackReadsHeap,
  CallbackWrites,
  CallbackUsesIndex,
  ReduceNotAssociative,
  Await,
}

impl Default for ArgUseMode {
  fn default() -> Self {
    Self::Borrow
  }
}

/// Whether an `await` is required to yield to the microtask queue.
///
/// In ECMAScript, `await` always yields (even when awaiting a non-promise value).
/// Native AOT backends may opt into a relaxed semantics where `await` on a value
/// proven to be non-promise and non-thenable is allowed to not yield.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum AwaitBehavior {
  /// Strict ECMAScript semantics: always yield.
  MustYield,
  /// Relaxed semantics: may not yield when the awaited value is already "ready".
  MayNotYield,
}

impl Default for AwaitBehavior {
  fn default() -> Self {
    Self::MustYield
  }
}

fn is_default_arg_use_modes(modes: &[ArgUseMode]) -> bool {
  modes.is_empty() || modes.iter().all(|m| matches!(m, ArgUseMode::Borrow))
}

impl Default for OwnershipState {
  fn default() -> Self {
    Self::Unknown
  }
}

impl OwnershipState {
  pub fn is_default(&self) -> bool {
    matches!(self, Self::Unknown)
  }

  pub fn join(self, other: Self) -> Self {
    use OwnershipState::*;
    // Deterministic "worse wins" join.
    match (self, other) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (Shared, _) | (_, Shared) => Shared,
      (Borrowed, _) | (_, Borrowed) => Borrowed,
      (Owned, Owned) => Owned,
    }
  }
}

/// Alias for downstream APIs that prefer `Ownership` over `OwnershipState`.
pub type Ownership = OwnershipState;

/// Branch-local nullability information derived from comparisons against `null`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Nullability {
  #[default]
  Unknown,
  Nullish,
  NonNullish,
}

impl Nullability {
  pub fn join(self, other: Self) -> Self {
    match (self, other) {
      (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
      (Self::Nullish, Self::Nullish) => Self::Nullish,
      (Self::NonNullish, Self::NonNullish) => Self::NonNullish,
      _ => Self::Unknown,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NullabilityNarrowing {
  pub var: u32,
  pub when_true: Nullability,
  pub when_false: Nullability,
}

impl Default for NullabilityNarrowing {
  fn default() -> Self {
    Self {
      var: 0,
      when_true: Nullability::Unknown,
      when_false: Nullability::Unknown,
    }
  }
}

/// Conservative integer range information.
///
/// `None` for a bound means unbounded/unknown in that direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct IntRange {
  pub min: Option<i64>,
  pub max: Option<i64>,
}

impl IntRange {
  pub fn join(&self, other: &Self) -> Self {
    Self {
      min: match (self.min, other.min) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => None,
      },
      max: match (self.max, other.max) {
        (Some(a), Some(b)) => Some(a.max(b)),
        _ => None,
      },
    }
  }
}

/// Per-value analysis facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct ValueFacts {
  pub purity: Option<Purity>,
  pub escape: Option<EscapeState>,
  pub ownership: Option<OwnershipState>,
  pub encoding: Option<StringEncoding>,
  pub int_range: Option<IntRange>,
  pub nullability: Option<Nullability>,
}

/// Per-instruction metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct InstMeta {
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "EffectSet::is_default")
  )]
  pub effects: EffectSet,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "TypeInfo::is_default")
  )]
  pub result_type: TypeInfo,
  #[cfg_attr(
    feature = "serde",
    serde(
      default,
      skip_serializing_if = "Option::is_none",
      serialize_with = "serialize_type_id"
    )
  )]
  pub type_id: Option<TypeId>,
  #[cfg(feature = "typed")]
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub native_layout: Option<types_ts_interned::LayoutId>,
  #[cfg_attr(
    feature = "serde",
    serde(
      default,
      skip_serializing_if = "Option::is_none",
      serialize_with = "serialize_expr_id"
    )
  )]
  pub hir_expr: Option<ExprId>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub type_summary: Option<ValueTypeSummary>,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
  pub excludes_nullish: bool,
  /// Preserve this instruction through copy-propagation passes.
  ///
  /// `optimize-js`'s SSA cleanup passes aggressively remove `VarAssign`
  /// instructions (`%t = %x`) as redundant copies. In typed builds we sometimes
  /// intentionally materialize such copies to attach per-expression type
  /// metadata (e.g. identifier reads can be flow-narrowed and parameters have no
  /// explicit defining instruction).
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
  pub preserve_var_assign: bool,
  /// For `InstTyp::Await`, records whether the awaited value is known to be
  /// already resolved (i.e. the await point is guaranteed not to suspend).
  ///
  /// This is currently only populated by the `native-async-ops` lowering; most
  /// analyses treat `Await` conservatively regardless of this flag.
  #[cfg(feature = "native-async-ops")]
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
  pub await_known_resolved: bool,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "OwnershipState::is_default")
  )]
  pub ownership: OwnershipState,
  /// Per-argument ownership transfer information.
  ///
  /// When non-empty, this is aligned 1:1 with the instruction's `args` (i.e.
  /// `arg_use_modes[i]` describes how `args[i]` is used).
  ///
  /// To keep IL metadata lightweight, analyses may leave this empty to mean
  /// "all arguments are borrowed".
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "is_default_arg_use_modes")
  )]
  pub arg_use_modes: Vec<ArgUseMode>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub in_place_hint: Option<InPlaceHint>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub result_escape: Option<crate::analysis::escape::EscapeState>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Purity::is_default")
  )]
  pub callee_purity: Purity,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub nullability_narrowing: Option<NullabilityNarrowing>,
  /// Opt-in `await` semantics relaxation for internal await ops.
  ///
  /// This field is currently consumed by native backends that choose to elide
  /// yielding for "ready" values (see [`AwaitBehavior`]).
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub await_behavior: Option<AwaitBehavior>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub value: Option<ValueFacts>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub parallel: Option<ParallelPlan>,
}

impl InstMeta {
  /// Clears metadata that describes the instruction's *result value* (i.e. the
  /// value stored in `Inst::tgts[0]`).
  ///
  /// This is useful when an optimization keeps an instruction for its effects
  /// (e.g. a `Call`) but removes its target variable. In that case we must
  /// reset any result-value annotations to avoid leaving stale type/ownership
  /// data attached to an instruction that no longer defines a value.
  pub fn clear_result_var_metadata(&mut self) {
    self.result_type = TypeInfo::default();
    self.type_id = None;
    #[cfg(feature = "typed")]
    {
      self.native_layout = None;
    }
    self.hir_expr = None;
    self.type_summary = None;
    self.excludes_nullish = false;
    self.ownership = OwnershipState::Unknown;
    self.result_escape = None;
    self.value = None;
  }

  /// Copies metadata that describes a result value from another instruction.
  ///
  /// This is intended for SSA/CFG rewrites that replace one value-defining
  /// instruction with another (e.g. Phi lowering).
  pub fn copy_result_var_metadata_from(&mut self, src: &Self) {
    self.result_type = src.result_type.clone();
    self.type_id = src.type_id;
    #[cfg(feature = "typed")]
    {
      self.native_layout = src.native_layout;
    }
    self.hir_expr = src.hir_expr;
    self.type_summary = src.type_summary;
    self.excludes_nullish = src.excludes_nullish;
    self.ownership = src.ownership;
    self.result_escape = src.result_escape;
    self.value = src.value.clone();
  }

  pub fn set_type_id(&mut self, type_id: Option<TypeId>) {
    self.type_id = type_id;
    #[cfg(feature = "typed")]
    {
      self.native_layout = None;
    }
  }

  pub fn clear_type_id(&mut self) {
    self.type_id = None;
    #[cfg(feature = "typed")]
    {
      self.native_layout = None;
    }
  }

  pub fn is_default(&self) -> bool {
    let base = self.effects.is_default()
      && self.result_type.is_default()
      && self.type_id.is_none()
      && {
        #[cfg(feature = "typed")]
        {
          self.native_layout.is_none()
        }
        #[cfg(not(feature = "typed"))]
        {
          true
        }
      }
      && self.hir_expr.is_none()
      && self.type_summary.is_none()
      && !self.excludes_nullish
      && !self.preserve_var_assign
      && self.ownership.is_default()
      && is_default_arg_use_modes(&self.arg_use_modes)
      && self.in_place_hint.is_none()
      && self.result_escape.is_none()
      && self.callee_purity.is_default()
      && self.nullability_narrowing.is_none()
      && self.await_behavior.is_none()
      && self.value.is_none()
      && self.parallel.is_none()
      && self.parallel.is_none();

    #[cfg(feature = "native-async-ops")]
    {
      return base && !self.await_known_resolved;
    }

    #[cfg(not(feature = "native-async-ops"))]
    {
      return base;
    }
  }

  pub fn is_pure(&self) -> bool {
    self.effects.is_pure()
  }

  /// Returns the use mode for argument `idx`.
  ///
  /// Note: `arg_use_modes` may be empty to represent "all args are borrowed".
  /// This helper provides a stable query API for downstream backends.
  pub fn arg_use_mode(&self, idx: usize) -> ArgUseMode {
    self
      .arg_use_modes
      .get(idx)
      .copied()
      .unwrap_or(ArgUseMode::Borrow)
  }
}
