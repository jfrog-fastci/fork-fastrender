//! WHATWG WebIDL overload-set computation + validation.
//!
//! This module implements the algorithms in WebIDL's "Overloading" section:
//! - compute the effective overload set
//! - distinguishability
//! - distinguishing argument index
//! - overload-set validation (including the BigInt/numeric restriction)
//!
//! This is *compile-time* logic intended for bindings/codegen (not runtime JS execution).

use std::collections::BTreeMap;
use std::fmt;

use super::resolve::ResolvedWebIdlWorld;
use webidl_ir::{IdlType as IrIdlType, NamedType as IrNamedType, NamedTypeKind as IrNamedTypeKind};

/// Minimal context required for interface-like distinguishability.
///
/// WebIDL's definition is: "no single platform object implements both interface-like types".
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
  pub file: Option<String>,
  pub line: Option<u32>,
  pub raw_member: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumericType {
  Long,
  Double,
}

impl fmt::Display for NumericType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      NumericType::Long => f.write_str("long"),
      NumericType::Double => f.write_str("double"),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringType {
  DOMString,
  ByteString,
  USVString,
}

impl fmt::Display for StringType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      StringType::DOMString => f.write_str("DOMString"),
      StringType::ByteString => f.write_str("ByteString"),
      StringType::USVString => f.write_str("USVString"),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BufferSourceType {
  ArrayBuffer,
  SharedArrayBuffer,
  DataView,
  TypedArray(String),
}

impl fmt::Display for BufferSourceType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      BufferSourceType::ArrayBuffer => f.write_str("ArrayBuffer"),
      BufferSourceType::SharedArrayBuffer => f.write_str("SharedArrayBuffer"),
      BufferSourceType::DataView => f.write_str("DataView"),
      BufferSourceType::TypedArray(name) => f.write_str(name),
    }
  }
}

/// WebIDL type model sufficient for overload resolution.
///
/// Note: This is intentionally small and binding-oriented; it can be replaced by a richer WebIDL IR
/// crate once the bindings generator lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdlType {
  Undefined,
  Boolean,
  Numeric(NumericType),
  BigInt,
  String(StringType),
  Object,
  Symbol,

  Interface(String),
  /// `callback interface Foo { ... }`
  CallbackInterface(String),
  Dictionary(String),
  /// `enum Foo { ... }`
  Enum(String),
  /// `record<K, V>` (dictionary-like for distinguishability purposes).
  Record(Box<IdlType>, Box<IdlType>),

  /// `callback Foo = ...` / `FooCallback` types.
  CallbackFunction {
    legacy_treat_non_object_as_null: bool,
  },

  Sequence(Box<IdlType>),
  FrozenArray(Box<IdlType>),
  AsyncSequence(Box<IdlType>),

  BufferSource(BufferSourceType),

  Promise(Box<IdlType>),
  Any,

  Union(Vec<IdlType>),
  Nullable(Box<IdlType>),
  Annotated(Box<IdlType>),
}

impl IdlType {
  fn strip_annotated(&self) -> &IdlType {
    let mut t = self;
    while let IdlType::Annotated(inner) = t {
      t = inner;
    }
    t
  }

  /// WebIDL "innermost type": strip a single layer of annotation, then a single layer of
  /// nullability.
  fn innermost(&self) -> &IdlType {
    let t = self.strip_annotated();
    if let IdlType::Nullable(inner) = t {
      inner
    } else {
      t
    }
  }

  fn is_dictionary_type(&self) -> bool {
    matches!(self.strip_annotated(), IdlType::Dictionary(_))
  }

  fn is_numeric_type(&self) -> bool {
    matches!(self.innermost(), IdlType::Numeric(_))
  }

  fn is_bigint_type(&self) -> bool {
    matches!(self.innermost(), IdlType::BigInt)
  }

  fn union_members_for_distinguishability(&self) -> Option<Vec<&IdlType>> {
    let t = self.strip_annotated();
    match t {
      IdlType::Union(members) => Some(flatten_union_members(members)),
      IdlType::Nullable(inner) => match inner.strip_annotated() {
        IdlType::Union(members) => Some(flatten_union_members(members)),
        _ => None,
      },
      _ => None,
    }
  }

  fn includes_nullable_type(&self) -> bool {
    fn rec(t: &IdlType) -> bool {
      let t = t.strip_annotated();
      match t {
        IdlType::Nullable(_) => true,
        IdlType::Union(members) => members.iter().any(rec),
        _ => false,
      }
    }
    rec(self)
  }

  fn is_union_type_with_dictionary_in_flattened_members(&self) -> bool {
    let t = self.strip_annotated();
    let IdlType::Union(members) = t else {
      return false;
    };
    flatten_union_members(members)
      .into_iter()
      .any(|t| matches!(t.strip_annotated(), IdlType::Dictionary(_)))
  }
}

impl TryFrom<&IrIdlType> for IdlType {
  type Error = String;

  fn try_from(value: &IrIdlType) -> Result<Self, Self::Error> {
    match value {
      IrIdlType::Any => Ok(IdlType::Any),
      IrIdlType::Undefined => Ok(IdlType::Undefined),
      IrIdlType::Boolean => Ok(IdlType::Boolean),
      IrIdlType::Numeric(_) => {
        // The overload algorithm cares about the *category* (numeric), not the exact numeric type.
        // Use `double` as a canonical representative.
        Ok(IdlType::Numeric(NumericType::Double))
      }
      IrIdlType::BigInt => Ok(IdlType::BigInt),
      IrIdlType::String(s) => match s {
        webidl_ir::StringType::DomString => Ok(IdlType::String(StringType::DOMString)),
        webidl_ir::StringType::ByteString => Ok(IdlType::String(StringType::ByteString)),
        webidl_ir::StringType::UsvString => Ok(IdlType::String(StringType::USVString)),
      },
      IrIdlType::Object => Ok(IdlType::Object),
      IrIdlType::Symbol => Ok(IdlType::Symbol),
      IrIdlType::Named(IrNamedType { name, kind }) => match kind {
        IrNamedTypeKind::Interface => Ok(IdlType::Interface(name.clone())),
        IrNamedTypeKind::Unresolved => Err(format!(
          "cannot convert unresolved named type `{name}` to overload type"
        )),
        IrNamedTypeKind::CallbackInterface => Ok(IdlType::CallbackInterface(name.clone())),
        IrNamedTypeKind::Dictionary => Ok(IdlType::Dictionary(name.clone())),
        IrNamedTypeKind::Enum => Ok(IdlType::Enum(name.clone())),
        IrNamedTypeKind::CallbackFunction => Ok(IdlType::CallbackFunction {
          legacy_treat_non_object_as_null: false,
        }),
        IrNamedTypeKind::Typedef => Err(format!(
          "cannot convert unresolved typedef `{name}` to overload type; expand typedefs first"
        )),
      },

      IrIdlType::Nullable(inner) => Ok(IdlType::Nullable(Box::new(IdlType::try_from(
        inner.as_ref(),
      )?))),
      IrIdlType::Union(members) => {
        let mut out = Vec::with_capacity(members.len());
        for m in members {
          out.push(IdlType::try_from(m)?);
        }
        Ok(IdlType::Union(out))
      }
      IrIdlType::Sequence(inner) => Ok(IdlType::Sequence(Box::new(IdlType::try_from(
        inner.as_ref(),
      )?))),
      IrIdlType::FrozenArray(inner) => Ok(IdlType::FrozenArray(Box::new(IdlType::try_from(
        inner.as_ref(),
      )?))),
      IrIdlType::AsyncSequence(inner) => Ok(IdlType::AsyncSequence(Box::new(IdlType::try_from(
        inner.as_ref(),
      )?))),
      IrIdlType::Record(key, value) => Ok(IdlType::Record(
        Box::new(IdlType::try_from(key.as_ref())?),
        Box::new(IdlType::try_from(value.as_ref())?),
      )),
      IrIdlType::Promise(inner) => Ok(IdlType::Promise(Box::new(IdlType::try_from(
        inner.as_ref(),
      )?))),
      IrIdlType::Annotated { annotations, inner } => {
        let mut inner = IdlType::try_from(inner.as_ref())?;

        let legacy_treat_non_object_as_null = annotations.iter().any(|a| {
          matches!(a, webidl_ir::TypeAnnotation::LegacyTreatNonObjectAsNull)
        });
        if legacy_treat_non_object_as_null {
          if let IdlType::CallbackFunction {
            legacy_treat_non_object_as_null: flag,
          } = &mut inner
          {
            *flag = true;
          }
        }

        Ok(IdlType::Annotated(Box::new(inner)))
      }
    }
  }
}

fn flatten_union_members<'a>(members: &'a [IdlType]) -> Vec<&'a IdlType> {
  fn rec<'a>(t: &'a IdlType, out: &mut Vec<&'a IdlType>) {
    match t.strip_annotated() {
      IdlType::Union(members) => {
        for m in members {
          rec(m, out);
        }
      }
      _ => out.push(t),
    }
  }

  let mut out = Vec::new();
  for m in members {
    rec(m, &mut out);
  }
  out
}

impl fmt::Display for IdlType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      IdlType::Undefined => f.write_str("undefined"),
      IdlType::Boolean => f.write_str("boolean"),
      IdlType::Numeric(n) => n.fmt(f),
      IdlType::BigInt => f.write_str("bigint"),
      IdlType::String(s) => s.fmt(f),
      IdlType::Object => f.write_str("object"),
      IdlType::Symbol => f.write_str("symbol"),
      IdlType::Interface(name) => f.write_str(name),
      IdlType::CallbackInterface(name) => {
        f.write_str(name)?;
        f.write_str(" (callback interface)")
      }
      IdlType::Dictionary(name) => {
        f.write_str(name)?;
        f.write_str(" (dictionary)")
      }
      IdlType::Enum(name) => {
        f.write_str(name)?;
        f.write_str(" (enum)")
      }
      IdlType::Record(key, value) => write!(f, "record<{}, {}>", key, value),
      IdlType::CallbackFunction {
        legacy_treat_non_object_as_null,
      } => {
        if *legacy_treat_non_object_as_null {
          f.write_str("callback function [LegacyTreatNonObjectAsNull]")
        } else {
          f.write_str("callback function")
        }
      }
      IdlType::Sequence(inner) => write!(f, "sequence<{}>", inner),
      IdlType::FrozenArray(inner) => write!(f, "FrozenArray<{}>", inner),
      IdlType::AsyncSequence(inner) => write!(f, "async sequence<{}>", inner),
      IdlType::BufferSource(t) => t.fmt(f),
      IdlType::Promise(inner) => write!(f, "Promise<{}>", inner),
      IdlType::Any => f.write_str("any"),
      IdlType::Union(members) => {
        f.write_str("(")?;
        for (idx, m) in members.iter().enumerate() {
          if idx != 0 {
            f.write_str(" or ")?;
          }
          write!(f, "{m}")?;
        }
        f.write_str(")")
      }
      IdlType::Nullable(inner) => write!(f, "{}?", inner),
      IdlType::Annotated(inner) => write!(f, "[annotated] {inner}"),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadArgument {
  pub name: Option<String>,
  pub ty: IdlType,
  pub optionality: Optionality,
  pub default: Option<String>,
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
  let len = entries_same_len[0].type_list.len();
  debug_assert!(entries_same_len.iter().all(|e| e.type_list.len() == len));

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
enum DistinguishabilityCategory {
  Undefined,
  Boolean,
  Numeric,
  BigInt,
  String,
  Object,
  Symbol,
  InterfaceLike,
  CallbackFunction,
  DictionaryLike,
  AsyncSequence,
  SequenceLike,
}

fn category_of(t: &IdlType) -> Option<DistinguishabilityCategory> {
  use DistinguishabilityCategory::*;
  match t {
    IdlType::Undefined => Some(Undefined),
    IdlType::Boolean => Some(Boolean),
    IdlType::Numeric(_) => Some(Numeric),
    IdlType::BigInt => Some(BigInt),
    IdlType::String(_) => Some(String),
    IdlType::Enum(_) => Some(String),
    IdlType::Object => Some(Object),
    IdlType::Symbol => Some(Symbol),
    IdlType::Interface(_) | IdlType::BufferSource(_) => Some(InterfaceLike),
    IdlType::CallbackFunction { .. } => Some(CallbackFunction),
    IdlType::Dictionary(_) | IdlType::Record(_, _) | IdlType::CallbackInterface(_) => {
      Some(DictionaryLike)
    }
    IdlType::AsyncSequence(_) => Some(AsyncSequence),
    IdlType::Sequence(_) | IdlType::FrozenArray(_) => Some(SequenceLike),
    _ => None,
  }
}

/// Whether two WebIDL types are distinguishable.
///
/// This implements the recursive algorithm in WHATWG WebIDL ("Overloading" section).
pub fn are_distinguishable<C: WorldContext>(a: &IdlType, b: &IdlType, world_ctx: &C) -> bool {
  // 1. Nullable/dictionary special-case.
  if a.includes_nullable_type()
    && (b.includes_nullable_type()
      || b.is_union_type_with_dictionary_in_flattened_members()
      || b.is_dictionary_type())
  {
    return false;
  }
  if b.includes_nullable_type()
    && (a.includes_nullable_type()
      || a.is_union_type_with_dictionary_in_flattened_members()
      || a.is_dictionary_type())
  {
    return false;
  }

  // 2. Union vs union.
  if let (Some(ma), Some(mb)) = (
    a.union_members_for_distinguishability(),
    b.union_members_for_distinguishability(),
  ) {
    for ta in &ma {
      for tb in &mb {
        if !are_distinguishable(ta, tb, world_ctx) {
          return false;
        }
      }
    }
    return true;
  }

  // 3. Union vs non-union.
  if let Some(members) = a.union_members_for_distinguishability() {
    return members
      .into_iter()
      .all(|m| are_distinguishable(m, b, world_ctx));
  }
  if let Some(members) = b.union_members_for_distinguishability() {
    return members
      .into_iter()
      .all(|m| are_distinguishable(a, m, world_ctx));
  }

  // 4. Table-based (strip annotated, then nullable).
  let a_inner = a.innermost();
  let b_inner = b.innermost();

  let Some(ca) = category_of(a_inner) else {
    return false;
  };
  let Some(cb) = category_of(b_inner) else {
    return false;
  };

  match distinguishability_table(ca, cb) {
    TableCell::False => false,
    TableCell::True => true,
    TableCell::ReqA => interface_like_distinguishable(a_inner, b_inner, world_ctx),
    TableCell::ReqB => true,
    TableCell::ReqC => callback_function_vs_dictionary_like_distinguishable(a_inner, b_inner),
    TableCell::ReqD => true,
  }
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

fn distinguishability_table(
  a: DistinguishabilityCategory,
  b: DistinguishabilityCategory,
) -> TableCell {
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
  if (a == CallbackFunction && b == DictionaryLike)
    || (a == DictionaryLike && b == CallbackFunction)
  {
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

fn interface_like_distinguishable<C: WorldContext>(
  a: &IdlType,
  b: &IdlType,
  world_ctx: &C,
) -> bool {
  // Safety: only called if both types are interface-like.
  match (a, b) {
    (IdlType::Interface(a), IdlType::Interface(b)) => {
      if a == b {
        return false;
      }
      // If either is in the other's inherited-interface chain, treat as overlapping.
      if interface_chain_contains(world_ctx, a, b) || interface_chain_contains(world_ctx, b, a) {
        return false;
      }
      true
    }
    (IdlType::BufferSource(a), IdlType::BufferSource(b)) => a != b,
    (IdlType::Interface(_), IdlType::BufferSource(_))
    | (IdlType::BufferSource(_), IdlType::Interface(_)) => true,
    _ => false,
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

fn callback_function_vs_dictionary_like_distinguishable(a: &IdlType, b: &IdlType) -> bool {
  match (a, b) {
    (
      IdlType::CallbackFunction {
        legacy_treat_non_object_as_null,
      },
      _,
    ) if matches!(
      category_of(b),
      Some(DistinguishabilityCategory::DictionaryLike)
    ) =>
    {
      !*legacy_treat_non_object_as_null
    }
    (
      _,
      IdlType::CallbackFunction {
        legacy_treat_non_object_as_null,
      },
    ) if matches!(
      category_of(a),
      Some(DistinguishabilityCategory::DictionaryLike)
    ) =>
    {
      !*legacy_treat_non_object_as_null
    }
    _ => false,
  }
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
        diags.push(Diagnostic::new(format!(
          "WebIDL overload `{}` has a variadic argument that is not in the final position",
          overload.name
        )));
      }
      if arg.optionality == Optionality::Variadic && arg.default.is_some() {
        diags.push(Diagnostic::new(format!(
          "WebIDL overload `{}` has a variadic argument with a default value (not allowed)",
          overload.name
        )));
      }
    }
  }

  let maxarg = overloads
    .iter()
    .map(|o| o.arguments.len())
    .max()
    .unwrap_or(0);

  let effective = compute_effective_overload_set(overloads, maxarg);

  let mut by_len: BTreeMap<usize, Vec<&EffectiveOverloadEntry>> = BTreeMap::new();
  for entry in &effective.items {
    by_len.entry(entry.type_list.len()).or_default().push(entry);
  }

  for (len, entries) in by_len {
    if entries.len() <= 1 {
      continue;
    }

    // Clone into a contiguous vec for easier indexing.
    let entries = entries.into_iter().cloned().collect::<Vec<_>>();

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
      diags.push(Diagnostic::new(msg));
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
          diags.push(Diagnostic::new(msg));
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
      has_bigint |= ty.is_bigint_type();
      has_numeric |= ty.is_numeric_type();
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
      diags.push(Diagnostic::new(msg));
    }
  }

  if diags.is_empty() {
    Ok(())
  } else {
    Err(diags)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeMap;

  #[derive(Default)]
  struct TestWorld {
    inherits: BTreeMap<String, String>,
  }

  impl WorldContext for TestWorld {
    fn interface_inherits(&self, interface: &str) -> Option<&str> {
      self.inherits.get(interface).map(|s| s.as_str())
    }
  }

  #[test]
  fn compute_effective_overload_set_spec_example_a_f() {
    let overloads = vec![
      Overload {
        name: "f".into(),
        arguments: vec![OverloadArgument::required(IdlType::String(
          StringType::DOMString,
        ))],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![
          OverloadArgument::required(IdlType::Interface("Node".into())),
          OverloadArgument::required(IdlType::String(StringType::DOMString)),
          OverloadArgument::variadic(IdlType::Numeric(NumericType::Double)),
        ],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![
          OverloadArgument::required(IdlType::Interface("Event".into())),
          OverloadArgument::required(IdlType::String(StringType::DOMString)),
          OverloadArgument::optional(IdlType::String(StringType::DOMString)),
          OverloadArgument::variadic(IdlType::Numeric(NumericType::Double)),
        ],
        origin: None,
      },
    ];

    let set = compute_effective_overload_set(&overloads, 4);
    let expected = EffectiveOverloadSet {
      items: vec![
        EffectiveOverloadEntry {
          callable_id: 0,
          type_list: vec![IdlType::String(StringType::DOMString)],
          optionality_list: vec![Optionality::Required],
        },
        EffectiveOverloadEntry {
          callable_id: 1,
          type_list: vec![
            IdlType::Interface("Node".into()),
            IdlType::String(StringType::DOMString),
          ],
          optionality_list: vec![Optionality::Required, Optionality::Required],
        },
        EffectiveOverloadEntry {
          callable_id: 1,
          type_list: vec![
            IdlType::Interface("Node".into()),
            IdlType::String(StringType::DOMString),
            IdlType::Numeric(NumericType::Double),
          ],
          optionality_list: vec![
            Optionality::Required,
            Optionality::Required,
            Optionality::Variadic,
          ],
        },
        EffectiveOverloadEntry {
          callable_id: 1,
          type_list: vec![
            IdlType::Interface("Node".into()),
            IdlType::String(StringType::DOMString),
            IdlType::Numeric(NumericType::Double),
            IdlType::Numeric(NumericType::Double),
          ],
          optionality_list: vec![
            Optionality::Required,
            Optionality::Required,
            Optionality::Variadic,
            Optionality::Variadic,
          ],
        },
        EffectiveOverloadEntry {
          callable_id: 2,
          type_list: vec![],
          optionality_list: vec![],
        },
        EffectiveOverloadEntry {
          callable_id: 3,
          type_list: vec![
            IdlType::Interface("Event".into()),
            IdlType::String(StringType::DOMString),
          ],
          optionality_list: vec![Optionality::Required, Optionality::Required],
        },
        EffectiveOverloadEntry {
          callable_id: 3,
          type_list: vec![
            IdlType::Interface("Event".into()),
            IdlType::String(StringType::DOMString),
            IdlType::String(StringType::DOMString),
          ],
          optionality_list: vec![
            Optionality::Required,
            Optionality::Required,
            Optionality::Optional,
          ],
        },
        EffectiveOverloadEntry {
          callable_id: 3,
          type_list: vec![
            IdlType::Interface("Event".into()),
            IdlType::String(StringType::DOMString),
            IdlType::String(StringType::DOMString),
            IdlType::Numeric(NumericType::Double),
          ],
          optionality_list: vec![
            Optionality::Required,
            Optionality::Required,
            Optionality::Optional,
            Optionality::Variadic,
          ],
        },
      ],
    };
    assert_eq!(set, expected);

    // The distinguishing argument index for the groups of size 2/3/4 should be 0 (Node vs Event).
    let world = TestWorld::default();
    for len in [2usize, 3, 4] {
      let entries = set
        .items
        .iter()
        .filter(|e| e.type_list.len() == len)
        .cloned()
        .collect::<Vec<_>>();
      assert_eq!(distinguishing_argument_index(&entries, &world), Some(0));
    }
  }

  #[test]
  fn distinguishability_nullable_dictionary_special_case() {
    let world = TestWorld::default();
    let a = IdlType::Nullable(Box::new(IdlType::Numeric(NumericType::Double)));
    let b = IdlType::Dictionary("Dictionary1".into());
    assert!(!are_distinguishable(&a, &b, &world));
  }

  #[test]
  fn distinguishability_interface_inheritance_is_not_distinguishable() {
    let mut world = TestWorld::default();
    world.inherits.insert("Event".into(), "Node".into());
    let node = IdlType::Interface("Node".into());
    let event = IdlType::Interface("Event".into());
    assert!(!are_distinguishable(&node, &event, &world));
  }

  #[test]
  fn validate_overload_set_rejects_domstring_vs_usvstring() {
    let world = TestWorld::default();
    let overloads = vec![
      Overload {
        name: "f".into(),
        arguments: vec![OverloadArgument::required(IdlType::String(
          StringType::DOMString,
        ))],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![OverloadArgument::required(IdlType::String(
          StringType::USVString,
        ))],
        origin: None,
      },
    ];

    let err = validate_overload_set(&overloads, &world).unwrap_err();
    let msg = err.iter().map(|d| d.message.as_str()).collect::<String>();
    assert!(msg.contains("DOMString"));
    assert!(msg.contains("USVString"));
  }

  #[test]
  fn validate_overload_set_rejects_mismatch_before_distinguishing_index() {
    let world = TestWorld::default();
    let overloads = vec![
      Overload {
        name: "f".into(),
        arguments: vec![OverloadArgument::required(IdlType::String(
          StringType::DOMString,
        ))],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![
          OverloadArgument::required(IdlType::Numeric(NumericType::Long)),
          OverloadArgument::required(IdlType::Numeric(NumericType::Double)),
          OverloadArgument::required(IdlType::Interface("Node".into())),
          OverloadArgument::required(IdlType::Interface("Node".into())),
        ],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![
          OverloadArgument::required(IdlType::Numeric(NumericType::Double)),
          OverloadArgument::required(IdlType::Numeric(NumericType::Double)),
          OverloadArgument::required(IdlType::String(StringType::DOMString)),
          OverloadArgument::required(IdlType::Interface("Node".into())),
        ],
        origin: None,
      },
    ];

    let err = validate_overload_set(&overloads, &world).unwrap_err();
    let msg = err.iter().map(|d| d.message.as_str()).collect::<String>();
    assert!(msg.contains("differ before that index"));
    assert!(msg.contains("argument 0"));
    assert!(msg.contains("long"));
    assert!(msg.contains("double"));
  }

  #[test]
  fn validate_overload_set_rejects_bigint_vs_numeric_at_distinguishing_index() {
    let world = TestWorld::default();
    let overloads = vec![
      Overload {
        name: "f".into(),
        arguments: vec![OverloadArgument::required(IdlType::BigInt)],
        origin: None,
      },
      Overload {
        name: "f".into(),
        arguments: vec![OverloadArgument::required(IdlType::Numeric(
          NumericType::Double,
        ))],
        origin: None,
      },
    ];

    let err = validate_overload_set(&overloads, &world).unwrap_err();
    let msg = err.iter().map(|d| d.message.as_str()).collect::<String>();
    assert!(msg.contains("BigInt/numeric restriction"));
    assert!(msg.contains("bigint"));
    assert!(msg.contains("numeric"));
  }
}
