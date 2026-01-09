//! WHATWG WebIDL overload-set computation + validation, based on `webidl-ir`.
//!
//! This module implements the algorithms in WebIDL's "Overloading" section:
//! - compute the effective overload set
//! - distinguishability (table + recursion)
//! - distinguishing argument index
//! - overload-set validation (including the BigInt/numeric restriction)
//!
//! It is intended for bindings/codegen (compile-time), not runtime JS execution.

use std::collections::BTreeMap;
use std::fmt;

use webidl_ir::{DefaultValue, DistinguishabilityCategory, IdlType, NamedType, NamedTypeKind, TypeAnnotation};

use super::resolve::ResolvedWebIdlWorld;

/// Minimal context required for interface-like distinguishability.
///
/// WebIDL's definition is: "no single platform object implements both interface-like types".
///
/// In practice (and for most DOMs), multiple inheritance is rare; we approximate by using the
/// single-inheritance graph:
///
/// - If `A` is in `B`'s inherited-interface chain, then an object implementing `B` can also
///   implement `A`, so the two are **not** distinguishable.
/// - Otherwise we assume they are distinguishable.
pub trait WorldContext {
  /// Returns the immediate inherited interface of `interface`, if any.
  fn interface_inherits(&self, interface: &str) -> Option<&str>;
}

impl WorldContext for ResolvedWebIdlWorld {
  fn interface_inherits(&self, interface: &str) -> Option<&str> {
    self
      .interfaces
      .get(interface)
      .and_then(|i| i.inherits.as_deref())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
  pub interface: String,
  pub raw_member: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
  pub message: String,
  /// Optional per-overload origins attached to this diagnostic.
  pub origins: Vec<Origin>,
}

impl Diagnostic {
  fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      origins: Vec::new(),
    }
  }

  fn with_origins(mut self, origins: Vec<Origin>) -> Self {
    self.origins = origins;
    self
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Optionality {
  Required,
  Optional,
  Variadic,
}

impl Optionality {
  fn as_str(self) -> &'static str {
    match self {
      Optionality::Required => "required",
      Optionality::Optional => "optional",
      Optionality::Variadic => "variadic",
    }
  }
}

impl fmt::Display for Optionality {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadArgument {
  pub name: Option<String>,
  pub ty: IdlType,
  pub optionality: Optionality,
  pub default: Option<DefaultValue>,
}

impl OverloadArgument {
  pub fn required(ty: IdlType) -> Self {
    Self {
      name: None,
      ty,
      optionality: Optionality::Required,
      default: None,
    }
  }

  pub fn optional(ty: IdlType) -> Self {
    Self {
      name: None,
      ty,
      optionality: Optionality::Optional,
      default: None,
    }
  }

  pub fn variadic(ty: IdlType) -> Self {
    Self {
      name: None,
      ty,
      optionality: Optionality::Variadic,
      default: None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overload {
  pub name: String,
  pub arguments: Vec<OverloadArgument>,
  pub origin: Option<Origin>,
}

/// An item in the effective overload set (a WebIDL "tuple").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveOverloadEntry {
  pub callable_id: usize,
  pub type_list: Vec<IdlType>,
  pub optionality_list: Vec<Optionality>,
}

impl EffectiveOverloadEntry {
  fn fmt_tuple(&self) -> String {
    let types = self
      .type_list
      .iter()
      .map(|t| t.to_string())
      .collect::<Vec<_>>()
      .join(", ");
    let opts = self
      .optionality_list
      .iter()
      .map(|o| o.as_str())
      .collect::<Vec<_>>()
      .join(", ");
    format!(
      "(overload #{}, types=[{}], optionality=[{}])",
      self.callable_id, types, opts
    )
  }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EffectiveOverloadSet {
  pub items: Vec<EffectiveOverloadEntry>,
}

/// Deterministic dispatch metadata for bindings/codegen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadDispatchPlan {
  /// The full effective overload set (for `N = maxarg`).
  pub effective: EffectiveOverloadSet,
  /// Effective entries grouped by argument count (= type list size), sorted by argument count.
  pub groups: Vec<OverloadDispatchGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadDispatchGroup {
  pub argument_count: usize,
  pub entries: Vec<EffectiveOverloadEntry>,
  pub distinguishing_argument_index: Option<usize>,
  /// For each entry (in `entries` order), derived type-category info for the distinguishing argument.
  pub distinguishing_argument_types: Vec<TypeCategoryFastPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeCategoryFastPath {
  pub category: Option<DistinguishabilityCategory>,
  /// If `ty` is `IdlType::Named`, this records the resolved kind/name.
  pub innermost_named_type: Option<NamedType>,
  pub includes_nullable_type: bool,
  pub includes_undefined: bool,
  /// Flattened union member types (for union types). For non-unions this is the innermost type.
  pub flattened_union_member_types: Vec<IdlType>,
  pub flattened_union_member_categories: Vec<Option<DistinguishabilityCategory>>,
}

impl TypeCategoryFastPath {
  fn for_idl_type(ty: &IdlType) -> Self {
    let flattened = ty.flattened_union_member_types();
    Self {
      category: ty.category_for_distinguishability(),
      innermost_named_type: match ty.innermost_type() {
        IdlType::Named(named) => Some(named.clone()),
        _ => None,
      },
      includes_nullable_type: ty.includes_nullable_type(),
      includes_undefined: ty.includes_undefined(),
      flattened_union_member_categories: flattened
        .iter()
        .map(|t| t.category_for_distinguishability())
        .collect(),
      flattened_union_member_types: flattened,
    }
  }
}

/// Compute the WebIDL effective overload set for `overloads` given an argument count `n`.
///
/// Ordering: for deterministic codegen we preserve overload declaration order, then emit all
/// effective entries for each overload in increasing argument-count order.
pub fn compute_effective_overload_set(overloads: &[Overload], n: usize) -> EffectiveOverloadSet {
  let mut out = Vec::<EffectiveOverloadEntry>::new();

  let maxarg = overloads
    .iter()
    .map(|o| o.arguments.len())
    .max()
    .unwrap_or(0);
  let max = std::cmp::max(maxarg, n);

  for (callable_id, overload) in overloads.iter().enumerate() {
    let arguments = &overload.arguments;
    let declared_n = arguments.len();

    let mut declared_types = Vec::<IdlType>::with_capacity(declared_n);
    let mut declared_opts = Vec::<Optionality>::with_capacity(declared_n);
    for (idx, arg) in arguments.iter().enumerate() {
      declared_types.push(arg.ty.clone());
      let opt = match arg.optionality {
        Optionality::Variadic if idx + 1 == declared_n => Optionality::Variadic,
        Optionality::Optional => Optionality::Optional,
        _ => Optionality::Required,
      };
      declared_opts.push(opt);
    }

    let mut per_overload = Vec::<EffectiveOverloadEntry>::new();

    // Base entry.
    per_overload.push(EffectiveOverloadEntry {
      callable_id,
      type_list: declared_types.clone(),
      optionality_list: declared_opts.clone(),
    });

    // Variadic expansion up to `max`.
    let is_variadic = arguments
      .last()
      .is_some_and(|a| a.optionality == Optionality::Variadic);
    if is_variadic && declared_n > 0 {
      for i in declared_n..max {
        let mut t = declared_types.clone();
        let mut o = declared_opts.clone();
        for _ in declared_n..=i {
          t.push(declared_types[declared_n - 1].clone());
          o.push(Optionality::Variadic);
        }
        per_overload.push(EffectiveOverloadEntry {
          callable_id,
          type_list: t,
          optionality_list: o,
        });
      }
    }

    // Optional-argument trimming (includes final variadic argument).
    if declared_n > 0 {
      let mut i: isize = declared_n as isize - 1;
      while i >= 0 {
        let arg = &arguments[i as usize];
        let is_optional = arg.optionality == Optionality::Optional
          || (arg.optionality == Optionality::Variadic && i as usize == declared_n - 1);
        if !is_optional {
          break;
        }

        let mut t = Vec::new();
        let mut o = Vec::new();
        for j in 0..i as usize {
          t.push(declared_types[j].clone());
          o.push(declared_opts[j]);
        }
        per_overload.push(EffectiveOverloadEntry {
          callable_id,
          type_list: t,
          optionality_list: o,
        });

        i -= 1;
      }
    }

    // For stable output and for ease of testing/debugging we order by type-list length.
    per_overload.sort_by_key(|e| e.type_list.len());

    // Ordered-set append (no duplicates).
    for entry in per_overload {
      if !out.contains(&entry) {
        out.push(entry);
      }
    }
  }

  EffectiveOverloadSet { items: out }
}

/// Compute the distinguishing argument index for `entries_same_len`.
///
/// Returns `None` if no such index exists.
pub fn distinguishing_argument_index<C: WorldContext>(
  entries_same_len: &[EffectiveOverloadEntry],
  world_ctx: &C,
) -> Option<usize> {
  if entries_same_len.len() <= 1 {
    return None;
  }
  let Some(len) = entries_same_len.first().map(|e| e.type_list.len()) else {
    return None;
  };
  if !entries_same_len.iter().all(|e| e.type_list.len() == len) {
    return None;
  }

  'idx: for i in 0..len {
    for a in 0..entries_same_len.len() {
      for b in (a + 1)..entries_same_len.len() {
        if !are_distinguishable(
          &entries_same_len[a].type_list[i],
          &entries_same_len[b].type_list[i],
          world_ctx,
        ) {
          continue 'idx;
        }
      }
    }
    return Some(i);
  }
  None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableCell {
  False,
  True,
  ReqA,
  ReqB,
  ReqC,
  ReqD,
}

fn distinguishability_table(a: DistinguishabilityCategory, b: DistinguishabilityCategory) -> TableCell {
  use DistinguishabilityCategory::*;
  if a == b {
    return match a {
      InterfaceLike => TableCell::ReqA,
      _ => TableCell::False,
    };
  }

  // Special conditional cells.
  if (a == Numeric && b == BigInt) || (a == BigInt && b == Numeric) {
    return TableCell::ReqB;
  }
  if (a == CallbackFunction && b == DictionaryLike) || (a == DictionaryLike && b == CallbackFunction) {
    return TableCell::ReqC;
  }
  if (a == String && b == AsyncSequence) || (a == AsyncSequence && b == String) {
    return TableCell::ReqD;
  }

  // Special blank cells.
  if (a == Undefined && b == DictionaryLike) || (a == DictionaryLike && b == Undefined) {
    return TableCell::False;
  }
  if a == Object || b == Object {
    // object is only distinguishable from symbol and the primitive scalar categories.
    let other = if a == Object { b } else { a };
    return match other {
      Undefined | Boolean | Numeric | BigInt | String | Symbol => TableCell::True,
      _ => TableCell::False,
    };
  }
  if (a == AsyncSequence && b == SequenceLike) || (a == SequenceLike && b == AsyncSequence) {
    return TableCell::False;
  }

  // Everything else is a ● mark.
  TableCell::True
}

fn is_dictionary_type(t: &IdlType) -> bool {
  matches!(
    t.innermost_type(),
    IdlType::Named(NamedType {
      kind: NamedTypeKind::Dictionary,
      ..
    })
  )
}

fn is_union_type_with_dictionary_in_flattened_members(t: &IdlType) -> bool {
  matches!(t.innermost_type(), IdlType::Union(_))
    && t
      .flattened_union_member_types()
      .iter()
      .any(|t| is_dictionary_type(t))
}

fn innermost_type_for_distinguishability_table(t: &IdlType) -> &IdlType {
  let t = match t {
    IdlType::Annotated { inner, .. } => inner,
    _ => t,
  };
  match t {
    IdlType::Nullable(inner) => inner,
    _ => t,
  }
}

fn interface_chain_contains<C: WorldContext>(world_ctx: &C, start: &str, target: &str) -> bool {
  let mut cur = Some(start);
  while let Some(name) = cur {
    if name == target {
      return true;
    }
    cur = world_ctx.interface_inherits(name);
  }
  false
}

fn interface_like_distinguishable<C: WorldContext>(a: &IdlType, b: &IdlType, world_ctx: &C) -> bool {
  match (a, b) {
    (
      IdlType::Named(NamedType {
        name: a,
        kind: NamedTypeKind::Interface,
      }),
      IdlType::Named(NamedType {
        name: b,
        kind: NamedTypeKind::Interface,
      }),
    ) => {
      if a == b {
        return false;
      }
      // If either is in the other's inherited-interface chain, treat as overlapping.
      if interface_chain_contains(world_ctx, a, b) || interface_chain_contains(world_ctx, b, a) {
        return false;
      }
      true
    }
    _ => false,
  }
}

fn callback_function_has_legacy_treat_non_object_as_null(t: &IdlType) -> bool {
  let mut cur = t;
  let mut has = false;
  loop {
    match cur {
      IdlType::Annotated { annotations, inner } => {
        has |= annotations
          .iter()
          .any(|a| matches!(a, TypeAnnotation::LegacyTreatNonObjectAsNull));
        cur = inner;
      }
      IdlType::Nullable(inner) => cur = inner,
      _ => break,
    }
  }
  matches!(
    cur,
    IdlType::Named(NamedType {
      kind: NamedTypeKind::CallbackFunction,
      ..
    })
  ) && has
}

fn callback_function_vs_dictionary_like_distinguishable(a: &IdlType, b: &IdlType) -> bool {
  let a_cat = a.category_for_distinguishability();
  let b_cat = b.category_for_distinguishability();
  if a_cat == Some(DistinguishabilityCategory::CallbackFunction)
    && b_cat == Some(DistinguishabilityCategory::DictionaryLike)
  {
    return !callback_function_has_legacy_treat_non_object_as_null(a);
  }
  if a_cat == Some(DistinguishabilityCategory::DictionaryLike)
    && b_cat == Some(DistinguishabilityCategory::CallbackFunction)
  {
    return !callback_function_has_legacy_treat_non_object_as_null(b);
  }
  false
}

fn is_numeric_type(t: &IdlType) -> bool {
  matches!(t.innermost_type(), IdlType::Numeric(_))
}

fn is_bigint_type(t: &IdlType) -> bool {
  matches!(t.innermost_type(), IdlType::BigInt)
}

/// Whether two WebIDL types are distinguishable.
///
/// This implements the recursive algorithm in WHATWG WebIDL ("Overloading" section).
pub fn are_distinguishable<C: WorldContext>(a: &IdlType, b: &IdlType, world_ctx: &C) -> bool {
  // Step 1: Nullable/dictionary special-case.
  if a.includes_nullable_type()
    && (b.includes_nullable_type()
      || is_union_type_with_dictionary_in_flattened_members(b)
      || is_dictionary_type(b))
  {
    return false;
  }
  if b.includes_nullable_type()
    && (a.includes_nullable_type()
      || is_union_type_with_dictionary_in_flattened_members(a)
      || is_dictionary_type(a))
  {
    return false;
  }

  let a_is_union = matches!(a.innermost_type(), IdlType::Union(_));
  let b_is_union = matches!(b.innermost_type(), IdlType::Union(_));

  // Step 2: Union vs union.
  if a_is_union && b_is_union {
    let ma = a.flattened_union_member_types();
    let mb = b.flattened_union_member_types();
    for ta in &ma {
      for tb in &mb {
        if !are_distinguishable(ta, tb, world_ctx) {
          return false;
        }
      }
    }
    return true;
  }

  // Step 3: Union vs non-union.
  if a_is_union {
    let members = a.flattened_union_member_types();
    return members.iter().all(|m| are_distinguishable(m, b, world_ctx));
  }
  if b_is_union {
    let members = b.flattened_union_member_types();
    return members.iter().all(|m| are_distinguishable(a, m, world_ctx));
  }

  // Step 4: Table-based (strip annotated, then nullable).
  let a_inner = innermost_type_for_distinguishability_table(a);
  let b_inner = innermost_type_for_distinguishability_table(b);

  let Some(ca) = a_inner.category_for_distinguishability() else {
    return false;
  };
  let Some(cb) = b_inner.category_for_distinguishability() else {
    return false;
  };

  match distinguishability_table(ca, cb) {
    TableCell::False => false,
    TableCell::True => true,
    TableCell::ReqA => interface_like_distinguishable(a_inner, b_inner, world_ctx),
    TableCell::ReqB => true,
    TableCell::ReqC => callback_function_vs_dictionary_like_distinguishable(a, b),
    TableCell::ReqD => true,
  }
}

fn origins_for_entries(overloads: &[Overload], entries: &[EffectiveOverloadEntry]) -> Vec<Origin> {
  let mut out = Vec::<Origin>::new();
  for entry in entries {
    let Some(origin) = overloads
      .get(entry.callable_id)
      .and_then(|o| o.origin.clone())
    else {
      continue;
    };
    if !out.contains(&origin) {
      out.push(origin);
    }
  }
  out
}

/// Validate an overload set for distinguishability + BigInt/numeric restrictions.
pub fn validate_overload_set<C: WorldContext>(
  overloads: &[Overload],
  world_ctx: &C,
) -> Result<(), Vec<Diagnostic>> {
  let mut diags = Vec::<Diagnostic>::new();

  // Basic structural argument validation (helps produce saner diagnostics downstream).
  for overload in overloads {
    for (idx, arg) in overload.arguments.iter().enumerate() {
      if arg.optionality == Optionality::Variadic && idx + 1 != overload.arguments.len() {
        diags.push(
          Diagnostic::new(format!(
            "WebIDL overload `{}` has a variadic argument that is not in the final position",
            overload.name
          ))
          .with_origins(overload.origin.iter().cloned().collect()),
        );
      }
      if arg.optionality == Optionality::Variadic && arg.default.is_some() {
        diags.push(
          Diagnostic::new(format!(
            "WebIDL overload `{}` has a variadic argument with a default value (not allowed)",
            overload.name
          ))
          .with_origins(overload.origin.iter().cloned().collect()),
        );
      }
    }
  }

  let maxarg = overloads
    .iter()
    .map(|o| o.arguments.len())
    .max()
    .unwrap_or(0);

  let effective = compute_effective_overload_set(overloads, maxarg);

  let mut by_len: BTreeMap<usize, Vec<EffectiveOverloadEntry>> = BTreeMap::new();
  for entry in &effective.items {
    by_len.entry(entry.type_list.len()).or_default().push(entry.clone());
  }

  for (len, entries) in by_len {
    if entries.len() <= 1 {
      continue;
    }

    let Some(d) = distinguishing_argument_index(&entries, world_ctx) else {
      let mut msg = format!(
        "WebIDL overload set for `{}` ({} arguments) has no distinguishing argument index; \
         argument types are not pairwise distinguishable for any argument position.\n\
         Conflicting entries:",
        overloads
          .first()
          .map(|o| o.name.as_str())
          .unwrap_or("<unknown>"),
        len
      );
      for e in &entries {
        msg.push_str("\n  - ");
        msg.push_str(&e.fmt_tuple());
      }
      diags.push(Diagnostic::new(msg).with_origins(origins_for_entries(overloads, &entries)));
      continue;
    };

    // For all j < d, types and optionality must be identical.
    for j in 0..d {
      let first_type = &entries[0].type_list[j];
      let first_opt = entries[0].optionality_list[j];
      for e in &entries[1..] {
        if &e.type_list[j] != first_type || e.optionality_list[j] != first_opt {
          let mut msg = format!(
            "WebIDL overload set for `{}` ({} arguments) has distinguishing argument index {}, \
             but overloads differ before that index (argument {} must have identical type and \
             optionality across overloads).\n\
             Conflicting entries:",
            overloads
              .first()
              .map(|o| o.name.as_str())
              .unwrap_or("<unknown>"),
            len,
            d,
            j
          );
          for e in &entries {
            msg.push_str("\n  - ");
            msg.push_str(&e.fmt_tuple());
          }
          diags.push(Diagnostic::new(msg).with_origins(origins_for_entries(overloads, &entries)));
          // Only emit one diagnostic per (len,j) to avoid noise.
          break;
        }
      }
    }

    // BigInt/numeric restriction at the distinguishing argument index.
    let mut has_bigint = false;
    let mut has_numeric = false;
    for e in &entries {
      let ty = &e.type_list[d];
      has_bigint |= is_bigint_type(ty);
      has_numeric |= is_numeric_type(ty);
    }
    if has_bigint && has_numeric {
      let mut msg = format!(
        "WebIDL overload set for `{}` ({} arguments) violates the BigInt/numeric restriction: \
         distinguishing argument index {} includes both `bigint` and numeric types.\n\
         Conflicting entries:",
        overloads
          .first()
          .map(|o| o.name.as_str())
          .unwrap_or("<unknown>"),
        len,
        d
      );
      for e in &entries {
        msg.push_str("\n  - ");
        msg.push_str(&e.fmt_tuple());
      }
      diags.push(Diagnostic::new(msg).with_origins(origins_for_entries(overloads, &entries)));
    }
  }

  if diags.is_empty() {
    Ok(())
  } else {
    Err(diags)
  }
}

/// Validate `overloads` and compute a deterministic `OverloadDispatchPlan`.
pub fn compute_dispatch_plan<C: WorldContext>(
  overloads: &[Overload],
  world_ctx: &C,
) -> Result<OverloadDispatchPlan, Vec<Diagnostic>> {
  validate_overload_set(overloads, world_ctx)?;

  let maxarg = overloads
    .iter()
    .map(|o| o.arguments.len())
    .max()
    .unwrap_or(0);
  let effective = compute_effective_overload_set(overloads, maxarg);

  let mut by_len: BTreeMap<usize, Vec<EffectiveOverloadEntry>> = BTreeMap::new();
  for entry in &effective.items {
    by_len.entry(entry.type_list.len()).or_default().push(entry.clone());
  }

  let mut groups = Vec::with_capacity(by_len.len());
  for (argument_count, entries) in by_len {
    let distinguishing_argument_index = distinguishing_argument_index(&entries, world_ctx);
    let mut distinguishing_argument_types = Vec::new();
    if let Some(d) = distinguishing_argument_index {
      distinguishing_argument_types = entries
        .iter()
        .map(|e| TypeCategoryFastPath::for_idl_type(&e.type_list[d]))
        .collect();
    }
    groups.push(OverloadDispatchGroup {
      argument_count,
      entries,
      distinguishing_argument_index,
      distinguishing_argument_types,
    });
  }

  Ok(OverloadDispatchPlan { effective, groups })
}

