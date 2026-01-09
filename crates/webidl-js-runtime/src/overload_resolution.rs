//! WebIDL overload resolution and helpers.
//!
//! This module implements the runtime portion of the WHATWG WebIDL
//! [overload resolution algorithm](https://webidl.spec.whatwg.org/#js-overloads).
//! Overload resolution is used heavily by DOM APIs (e.g. `CanvasRenderingContext2D.drawImage`,
//! `CSS.supports`, various `Document` methods).
//!
//! The goal is for generated bindings to be able to share one correct implementation, rather than
//! emitting bespoke dispatch code per overloaded operation.

use crate::runtime::{IteratorRecord, WebIdlJsRuntime};
use std::collections::BTreeMap;
use webidl_ir::{
  DistinguishabilityCategory, IdlType, NamedTypeKind, NumericType, TypeAnnotation,
  WebIdlValue as IrWebIdlValue,
};

/// Create and return the engine error for an overload resolution failure.
///
/// Callers typically use this as:
///
/// ```ignore
/// return Err(throw_no_matching_overload(rt, "op", args.len(), &candidates));
/// ```
pub fn throw_no_matching_overload<R: WebIdlJsRuntime>(
  rt: &mut R,
  operation_name: &str,
  provided_argc: usize,
  candidate_signatures: &[&str],
) -> R::Error {
  let mut candidates = candidate_signatures
    .iter()
    .copied()
    .map(str::to_string)
    .collect::<Vec<_>>();
  // Deterministic output for golden tests regardless of how codegen collects candidates.
  candidates.sort();
  candidates.dedup();

  let mut message = format!(
    "No matching overload for {operation_name} with {provided_argc} arguments."
  );
  if !candidates.is_empty() {
    message.push_str("\nCandidates:");
    for cand in candidates {
      message.push_str("\n  - ");
      message.push_str(&cand);
    }
  }

  rt.throw_type_error(&message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Optionality {
  Required,
  Optional,
  Variadic,
}

impl Optionality {
  fn is_optional_for_trimming(self) -> bool {
    matches!(self, Optionality::Optional | Optionality::Variadic)
  }

  fn is_optional(self) -> bool {
    matches!(self, Optionality::Optional)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverloadArg {
  pub ty: IdlType,
  pub optionality: Optionality,
  pub default: Option<IrWebIdlValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverloadSig {
  /// Arguments as declared in the WebIDL signature.
  pub args: Vec<OverloadArg>,
  /// Declaration order index (used for stable tie-breaking / diagnostics).
  pub decl_index: usize,
  /// Optional precomputed distinguishing argument indices, keyed by effective argument count.
  ///
  /// The overload resolution algorithm computes the distinguishing argument index over the
  /// effective overload set entries with a given type-list length (see WebIDL "distinguishing
  /// argument index"). Bindings generators can precompute this per argument count group and provide
  /// it here to avoid doing distinguishability checks at runtime.
  pub distinguishing_arg_index_by_arg_count: Option<Vec<(usize, usize)>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConvertedArgument<V: Copy> {
  Value(WebIdlValue<V>),
  Missing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOverload<V: Copy> {
  pub overload_index: usize,
  /// Converted IDL values (and "missing" markers) per WebIDL overload resolution algorithm.
  pub values: Vec<ConvertedArgument<V>>,
}

/// Runtime representation of WebIDL values produced by conversions.
///
/// This is intentionally small and currently only covers the types needed by overload resolution
/// and early bindings scaffolding. As the bindings generator grows, this can be expanded (or
/// replaced) to carry richer host-side types.
#[derive(Debug, Clone, PartialEq)]
pub enum WebIdlValue<V: Copy> {
  Undefined,
  Null,
  Boolean(bool),

  Byte(i8),
  Octet(u8),
  Short(i16),
  UnsignedShort(u16),
  Long(i32),
  UnsignedLong(u32),
  LongLong(i64),
  UnsignedLongLong(u64),
  Float(f32),
  UnrestrictedFloat(f32),
  Double(f64),
  UnrestrictedDouble(f64),

  /// A JavaScript String value (not a String object).
  String(V),
  Enum(V),

  /// A JavaScript value reference used for `any`, interface types, callback types, `object`, etc.
  JsValue(V),

  Sequence {
    elem_ty: Box<IdlType>,
    values: Vec<WebIdlValue<V>>,
  },

  Dictionary {
    name: String,
    members: BTreeMap<String, WebIdlValue<V>>,
  },

  Union {
    member_ty: Box<IdlType>,
    value: Box<WebIdlValue<V>>,
  },
}

#[derive(Debug, Clone)]
struct EffectiveEntry {
  overload_index: usize,
  type_list: Vec<IdlType>,
  optionality_list: Vec<Optionality>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NullOrUndefined {
  Null,
  Undefined,
}

fn null_or_undefined<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
) -> Result<Option<NullOrUndefined>, R::Error> {
  if rt.is_object(value)
    || rt.is_boolean(value)
    || rt.is_number(value)
    || rt.is_bigint(value)
    || rt.is_string(value)
    || rt.is_symbol(value)
  {
    return Ok(None);
  }

  // The only remaining ECMAScript types are Undefined and Null.
  // Use `ToNumber` to distinguish them in a runtime-agnostic way.
  let n = rt.to_number(value)?;
  Ok(Some(if n.is_nan() {
    NullOrUndefined::Undefined
  } else {
    NullOrUndefined::Null
  }))
}

fn is_undefined<R: WebIdlJsRuntime>(rt: &mut R, value: R::JsValue) -> Result<bool, R::Error> {
  Ok(matches!(
    null_or_undefined(rt, value)?,
    Some(NullOrUndefined::Undefined)
  ))
}

pub fn resolve_overload<R: WebIdlJsRuntime>(
  rt: &mut R,
  overloads: &[OverloadSig],
  args: &[R::JsValue],
) -> Result<ResolvedOverload<R::JsValue>, R::Error> {
  // Spec: https://webidl.spec.whatwg.org/#dfn-overload-resolution-algorithm

  // 1. Let maxarg be the length of the longest type list of the entries in S.
  // Here we compute maxarg without materializing the full effective overload set:
  // - without variadics, the longest entry has the maximum declared argument length
  // - with a variadic overload present, the effective overload set expands up to max(maxarg, n)
  let n = args.len();
  let max_declared = overloads.iter().map(|o| o.args.len()).max().unwrap_or(0);
  let has_variadic = overloads
    .iter()
    .any(|o| o.args.last().is_some_and(|a| a.optionality == Optionality::Variadic));
  let maxarg = if has_variadic {
    std::cmp::max(max_declared, n)
  } else {
    max_declared
  };

  // 2. Let n be the size of args. (already `n`)
  // 3. Initialize argcount to be min(maxarg, n).
  let argcount = std::cmp::min(maxarg, n);

  // 4. Remove from S all entries whose type list is not of length argcount.
  // We build the filtered effective overload set entries directly.
  let mut entries = Vec::<EffectiveEntry>::new();
  for (overload_index, overload) in overloads.iter().enumerate() {
    if let Some(entry) = effective_entry_for_argcount(overload_index, overload, argcount) {
      entries.push(entry);
    }
  }

  // 5. If S is empty, throw TypeError.
  if entries.is_empty() {
    return Err(rt.throw_type_error(&format!(
      "No matching overload for {argcount} argument(s). Candidates:\n{}",
      format_overload_candidates(overloads)
    )));
  }

  // 6. Initialize d to -1.
  let mut d: isize = -1;
  // 7. Initialize method to undefined.
  let mut method: Option<R::JsValue> = None;
  // 8. If there is more than one entry in S, set d to distinguishing argument index.
  if entries.len() > 1 {
    d = match precomputed_distinguishing_index(overloads, argcount) {
      Some(v) => v as isize,
      None => distinguishing_argument_index(&entries)
        .map(|v| v as isize)
        .unwrap_or(-2),
    };
    if d < 0 {
      return Err(rt.throw_type_error(&format!(
        "Ambiguous overload set for {argcount} argument(s); no distinguishing argument index. Candidates:\n{}",
        format_effective_entries(&entries)
      )));
    }
  }

  // 9. values = empty list
  let mut values = Vec::<ConvertedArgument<R::JsValue>>::new();
  // 10. i = 0
  let mut i: usize = 0;

  // 11. While i < d: convert args left of distinguishing index.
  while (i as isize) < d {
    let v = args[i];
    let ty = entries[0].type_list[i].clone();
    let optionality = entries[0].optionality_list[i];

    if optionality.is_optional() && is_undefined(rt, v)? {
      if let Some(default) = overloads[entries[0].overload_index].args[i].default.as_ref() {
        values.push(ConvertedArgument::Value(convert_default(rt, default)?));
      } else {
        values.push(ConvertedArgument::Missing);
      }
    } else {
      values.push(ConvertedArgument::Value(convert_js_to_idl(rt, &ty, v)?));
    }
    i += 1;
  }

  // 12. If i = d, then resolve the overload using args[i].
  if (i as isize) == d {
    let v = args[i];

    // 12.2 optional undefined special-case.
    if is_undefined(rt, v)?
      && entries
        .iter()
        .any(|e| e.optionality_list.get(i).copied() == Some(Optionality::Optional))
    {
      entries.retain(|e| e.optionality_list.get(i).copied() == Some(Optionality::Optional));
    }
    // 12.3 nullable/dictionary special-case.
    else if matches!(null_or_undefined(rt, v)?, Some(_))
      && entries.iter().any(|e| type_matches_nullable_dictionary(&e.type_list[i]))
    {
      entries.retain(|e| type_matches_nullable_dictionary(&e.type_list[i]));
    }
    // 12.4 platform object special-case.
    else if rt.is_platform_object(v)
      && entries
        .iter()
        .any(|e| type_matches_platform_object(rt, &e.type_list[i], v))
    {
      entries.retain(|e| type_matches_platform_object(rt, &e.type_list[i], v));
    }
    // 12.5 ArrayBuffer special-case.
    else if rt.is_object(v)
      && rt.is_array_buffer(v)
      && entries
        .iter()
        .any(|e| type_matches_array_buffer(rt, &e.type_list[i], v))
    {
      entries.retain(|e| type_matches_array_buffer(rt, &e.type_list[i], v));
    }
    // 12.6 DataView special-case.
    else if rt.is_object(v)
      && rt.is_data_view(v)
      && entries
        .iter()
        .any(|e| type_matches_data_view(rt, &e.type_list[i], v))
    {
      entries.retain(|e| type_matches_data_view(rt, &e.type_list[i], v));
    }
    // 12.7 TypedArray special-case.
    else if rt.is_object(v)
      && rt.typed_array_name(v).is_some()
      && entries
        .iter()
        .any(|e| type_matches_typed_array(rt, &e.type_list[i], v))
    {
      entries.retain(|e| type_matches_typed_array(rt, &e.type_list[i], v));
    }
    // 12.8 callback function special-case.
    else if rt.is_callable(v)
      && entries
        .iter()
        .any(|e| type_matches_callable(rt, &e.type_list[i], v))
    {
      entries.retain(|e| type_matches_callable(rt, &e.type_list[i], v));
    }
    // 12.9 async sequence special-case.
    else if rt.is_object(v) && entries.iter().any(|e| type_matches_async_sequence(&e.type_list[i])) {
      let has_string_type = entries.iter().any(|e| type_matches_string(&e.type_list[i]));
      let skip_async_sequence = rt.is_string_object(v) && has_string_type;

      if !skip_async_sequence {
        let async_iter_key = rt.symbol_async_iterator()?;
        let iter_key = rt.symbol_iterator()?;
        let mut m = rt.get_method(v, async_iter_key)?;
        if m.is_none() {
          m = rt.get_method(v, iter_key)?;
        }
        if m.is_some() {
          entries.retain(|e| type_matches_async_sequence(&e.type_list[i]));
        }
      }
    }
    // 12.10 sequence special-case.
    else if rt.is_object(v) && entries.iter().any(|e| type_matches_sequence(&e.type_list[i])) {
      let iter_key = rt.symbol_iterator()?;
      let m = rt.get_method(v, iter_key)?;
      if let Some(method_value) = m {
        method = Some(method_value);
        entries.retain(|e| type_matches_sequence(&e.type_list[i]));
      }
    }
    // 12.11 object/dictionary-like special-case.
    else if rt.is_object(v)
      && entries
        .iter()
        .any(|e| type_matches_object_or_dictionary_like(&e.type_list[i]))
    {
      entries.retain(|e| type_matches_object_or_dictionary_like(&e.type_list[i]));
    }
    // 12.12 boolean special-case.
    else if rt.is_boolean(v) && entries.iter().any(|e| type_matches_boolean(&e.type_list[i])) {
      entries.retain(|e| type_matches_boolean(&e.type_list[i]));
    }
    // 12.13 number special-case.
    else if rt.is_number(v) && entries.iter().any(|e| type_matches_numeric(&e.type_list[i])) {
      entries.retain(|e| type_matches_numeric(&e.type_list[i]));
    }
    // 12.14 bigint special-case.
    else if rt.is_bigint(v) && entries.iter().any(|e| type_matches_bigint(&e.type_list[i])) {
      entries.retain(|e| type_matches_bigint(&e.type_list[i]));
    }
    // 12.15 string fallthrough.
    else if entries.iter().any(|e| type_matches_string(&e.type_list[i])) {
      entries.retain(|e| type_matches_string(&e.type_list[i]));
    }
    // 12.16 numeric fallthrough.
    else if entries.iter().any(|e| type_matches_numeric(&e.type_list[i])) {
      entries.retain(|e| type_matches_numeric(&e.type_list[i]));
    }
    // 12.17 boolean fallthrough.
    else if entries.iter().any(|e| type_matches_boolean(&e.type_list[i])) {
      entries.retain(|e| type_matches_boolean(&e.type_list[i]));
    }
    // 12.18 bigint fallthrough.
    else if entries.iter().any(|e| type_matches_bigint(&e.type_list[i])) {
      entries.retain(|e| type_matches_bigint(&e.type_list[i]));
    }
    // 12.19 any fallthrough.
    else if entries
      .iter()
      .any(|e| matches!(e.type_list[i].innermost_type(), IdlType::Any))
    {
      let any_entry = entries
        .iter()
        .find(|e| matches!(e.type_list[i].innermost_type(), IdlType::Any))
        .cloned();
      if let Some(e) = any_entry {
        entries.clear();
        entries.push(e);
      }
    } else {
      return Err(rt.throw_type_error(&format!(
        "No matching overload for argument {i}. Candidates:\n{}",
        format_effective_entries(&entries)
      )));
    }

    if entries.len() != 1 {
      return Err(rt.throw_type_error(&format!(
        "Ambiguous overload for argument {i}. Candidates:\n{}",
        format_effective_entries(&entries)
      )));
    }
  }

  let Some(selected_entry) = entries.first().cloned() else {
    return Err(rt.throw_type_error("overload resolution failed: empty overload set"));
  };
  let selected_overload = &overloads[selected_entry.overload_index];

  // 13. If i = d and method is not undefined, convert distinguishing argument as iterable.
  //
  // Note: `method` is only used to avoid calling `GetMethod(V, @@iterator)` twice when the selected
  // type is (or unwraps to) an actual sequence type. If the selected type is a union containing
  // sequence-like members, conversion will happen in step 14 via the union conversion algorithm.
  if (i as isize) == d {
    if let Some(method_value) = method {
      if let Some(elem_ty) = selected_entry.type_list.get(i).and_then(sequence_element_type) {
        let v = args[i];
        let seq = create_sequence_from_iterable(rt, elem_ty, v, method_value)?;
        values.push(ConvertedArgument::Value(seq));
        i += 1;
      }
    }
  }

  // 14. While i < argcount: convert remaining arguments.
  while i < argcount {
    let v = args[i];
    let ty = selected_entry.type_list[i].clone();
    let optionality = selected_entry.optionality_list[i];

    if optionality.is_optional() && is_undefined(rt, v)? {
      if let Some(default) = selected_overload.args[i].default.as_ref() {
        values.push(ConvertedArgument::Value(convert_default(rt, default)?));
      } else {
        values.push(ConvertedArgument::Missing);
      }
    } else {
      values.push(ConvertedArgument::Value(convert_js_to_idl(rt, &ty, v)?));
    }
    i += 1;
  }

  // 15. Fill remaining declared arguments with default/missing (variadics contribute nothing).
  while i < selected_overload.args.len() {
    let arg = &selected_overload.args[i];
    if let Some(default) = arg.default.as_ref() {
      values.push(ConvertedArgument::Value(convert_default(rt, default)?));
    } else if arg.optionality != Optionality::Variadic {
      values.push(ConvertedArgument::Missing);
    }
    i += 1;
  }

  Ok(ResolvedOverload {
    overload_index: selected_entry.overload_index,
    values,
  })
}

fn sequence_element_type(t: &IdlType) -> Option<&IdlType> {
  match t {
    IdlType::Annotated { inner, .. } => sequence_element_type(inner),
    IdlType::Nullable(inner) => sequence_element_type(inner),
    IdlType::Sequence(elem) => Some(elem.as_ref()),
    _ => None,
  }
}

fn effective_entry_for_argcount(
  overload_index: usize,
  overload: &OverloadSig,
  argcount: usize,
) -> Option<EffectiveEntry> {
  let declared = &overload.args;
  let declared_len = declared.len();

  if argcount == 0 {
    // Only include the empty effective entry if the overload can be called with 0 arguments (all
    // arguments are optional/variadic and thus trimmed).
    if declared.is_empty() || declared.iter().all(|a| a.optionality.is_optional_for_trimming()) {
      return Some(EffectiveEntry {
        overload_index,
        type_list: Vec::new(),
        optionality_list: Vec::new(),
      });
    }
    return None;
  }

  // Non-empty argcount.
  if declared_len == 0 {
    return None;
  }

  let is_variadic = declared
    .last()
    .is_some_and(|a| a.optionality == Optionality::Variadic);

  if argcount > declared_len {
    if !is_variadic {
      return None;
    }

    let mut type_list = Vec::with_capacity(argcount);
    let mut optionality_list = Vec::with_capacity(argcount);

    // Copy all but the final variadic argument.
    for arg in &declared[..(declared_len - 1)] {
      type_list.push(arg.ty.clone());
      optionality_list.push(arg.optionality);
    }

    // Repeat variadic arg type until argcount.
    let var_ty = declared[declared_len - 1].ty.clone();
    let repeats = argcount - (declared_len - 1);
    for _ in 0..repeats {
      type_list.push(var_ty.clone());
      optionality_list.push(Optionality::Variadic);
    }

    return Some(EffectiveEntry {
      overload_index,
      type_list,
      optionality_list,
    });
  }

  // argcount <= declared_len: ensure dropped tail arguments are all optional/variadic.
  for arg in &declared[argcount..] {
    if !arg.optionality.is_optional_for_trimming() {
      return None;
    }
  }

  let mut type_list = Vec::with_capacity(argcount);
  let mut optionality_list = Vec::with_capacity(argcount);
  for arg in &declared[..argcount] {
    type_list.push(arg.ty.clone());
    optionality_list.push(arg.optionality);
  }

  Some(EffectiveEntry {
    overload_index,
    type_list,
    optionality_list,
  })
}

fn precomputed_distinguishing_index(overloads: &[OverloadSig], argcount: usize) -> Option<usize> {
  let first = overloads.first()?.distinguishing_arg_index_by_arg_count.as_ref()?;
  let (_, d) = first.iter().find(|(n, _d)| *n == argcount)?;
  Some(*d)
}

fn distinguishing_argument_index(entries: &[EffectiveEntry]) -> Option<usize> {
  if entries.len() <= 1 {
    return None;
  }
  let len = entries[0].type_list.len();
  if entries.iter().any(|e| e.type_list.len() != len) {
    return None;
  }

  'idx: for i in 0..len {
    for a in 0..entries.len() {
      for b in (a + 1)..entries.len() {
        if !are_distinguishable(&entries[a].type_list[i], &entries[b].type_list[i]) {
          continue 'idx;
        }
      }
    }
    return Some(i);
  }

  None
}

fn are_distinguishable(a: &IdlType, b: &IdlType) -> bool {
  // Spec: https://webidl.spec.whatwg.org/#dfn-distinguishable

  // 1. Nullable/dictionary special-case.
  if a.includes_nullable_type()
    && (b.includes_nullable_type() || union_has_dictionary_member(b) || is_dictionary_type(b))
  {
    return false;
  }
  if b.includes_nullable_type()
    && (a.includes_nullable_type() || union_has_dictionary_member(a) || is_dictionary_type(a))
  {
    return false;
  }

  // 2/3. Union recursion.
  let a_members = union_members_for_distinguishability(a);
  let b_members = union_members_for_distinguishability(b);
  if let (Some(ma), Some(mb)) = (&a_members, &b_members) {
    for ta in ma {
      for tb in mb {
        if !are_distinguishable(ta, tb) {
          return false;
        }
      }
    }
    return true;
  }
  if let Some(ma) = &a_members {
    return ma.iter().all(|m| are_distinguishable(m, b));
  }
  if let Some(mb) = &b_members {
    return mb.iter().all(|m| are_distinguishable(a, m));
  }

  // 4. Table-based.
  let Some(ca) = a.category_for_distinguishability() else {
    return false;
  };
  let Some(cb) = b.category_for_distinguishability() else {
    return false;
  };

  if ca == cb {
    return match ca {
      DistinguishabilityCategory::InterfaceLike => {
        // Requirement (a): types are not the same, and no object implements both.
        //
        // Runtime dispatch only has access to the actual JS value and cannot cheaply answer "does
        // any object implement both". For valid overload sets, different interface names are
        // assumed to be non-overlapping.
        a.innermost_type() != b.innermost_type()
      }
      _ => false,
    };
  }

  // Special conditional cells.
  if (ca == DistinguishabilityCategory::Numeric && cb == DistinguishabilityCategory::BigInt)
    || (ca == DistinguishabilityCategory::BigInt && cb == DistinguishabilityCategory::Numeric)
  {
    return true;
  }

  if (ca == DistinguishabilityCategory::CallbackFunction
    && cb == DistinguishabilityCategory::DictionaryLike)
    || (ca == DistinguishabilityCategory::DictionaryLike
      && cb == DistinguishabilityCategory::CallbackFunction)
  {
    // Requirement (c): callback function distinguishable from dictionary-like unless it has
    // [LegacyTreatNonObjectAsNull].
    let callback = if ca == DistinguishabilityCategory::CallbackFunction {
      a
    } else {
      b
    };
    return !has_annotation(callback, TypeAnnotation::LegacyTreatNonObjectAsNull);
  }

  if (ca == DistinguishabilityCategory::AsyncSequence && cb == DistinguishabilityCategory::SequenceLike)
    || (ca == DistinguishabilityCategory::SequenceLike && cb == DistinguishabilityCategory::AsyncSequence)
  {
    return false;
  }

  if ca == DistinguishabilityCategory::Object || cb == DistinguishabilityCategory::Object {
    let other = if ca == DistinguishabilityCategory::Object { cb } else { ca };
    return matches!(
      other,
      DistinguishabilityCategory::Undefined
        | DistinguishabilityCategory::Boolean
        | DistinguishabilityCategory::Numeric
        | DistinguishabilityCategory::BigInt
        | DistinguishabilityCategory::String
        | DistinguishabilityCategory::Symbol
    );
  }

  // Undefined vs dictionary-like is blank in the table.
  if (ca == DistinguishabilityCategory::Undefined && cb == DistinguishabilityCategory::DictionaryLike)
    || (ca == DistinguishabilityCategory::DictionaryLike && cb == DistinguishabilityCategory::Undefined)
  {
    return false;
  }

  // Everything else is a ● mark.
  true
}

fn union_members_for_distinguishability(t: &IdlType) -> Option<Vec<IdlType>> {
  // Union or nullable-union, after stripping annotated.
  match t {
    IdlType::Annotated { inner, .. } => union_members_for_distinguishability(inner),
    IdlType::Union(_) => Some(t.flattened_union_member_types()),
    IdlType::Nullable(inner) => match inner.as_ref() {
      IdlType::Annotated { .. } => union_members_for_distinguishability(inner),
      IdlType::Union(_) => Some(inner.flattened_union_member_types()),
      _ => None,
    },
    _ => None,
  }
}

fn union_has_dictionary_member(t: &IdlType) -> bool {
  match t.innermost_type() {
    IdlType::Union(members) => members
      .iter()
      .flat_map(|m| m.flattened_union_member_types())
      .any(|m| is_dictionary_type(&m)),
    _ => false,
  }
}

fn is_dictionary_type(t: &IdlType) -> bool {
  matches!(
    t.innermost_type(),
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Dictionary,
      ..
    })
  )
}

fn has_annotation(t: &IdlType, needle: TypeAnnotation) -> bool {
  let mut cur = t;
  loop {
    match cur {
      IdlType::Annotated { annotations, inner } => {
        if annotations.iter().any(|a| *a == needle) {
          return true;
        }
        cur = inner;
      }
      _ => return false,
    }
  }
}

fn type_matches_nullable_dictionary(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_nullable_dictionary(inner),
    IdlType::Nullable(_) => true,
    IdlType::Union(_) => t.includes_nullable_type() || union_has_dictionary_member(t),
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Dictionary,
      ..
    }) => true,
    _ => false,
  }
}

fn type_matches_platform_object<R: WebIdlJsRuntime>(rt: &R, t: &IdlType, v: R::JsValue) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_platform_object(rt, inner, v),
    IdlType::Nullable(inner) => type_matches_platform_object(rt, inner, v),
    IdlType::Union(members) => members
      .iter()
      .any(|m| type_matches_platform_object(rt, m, v)),
    IdlType::Object => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Interface,
      name,
    }) => rt.implements_interface(v, name),
    _ => false,
  }
}

fn type_matches_array_buffer<R: WebIdlJsRuntime>(rt: &R, t: &IdlType, v: R::JsValue) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_array_buffer(rt, inner, v),
    IdlType::Nullable(inner) => type_matches_array_buffer(rt, inner, v),
    IdlType::Union(members) => members.iter().any(|m| type_matches_array_buffer(rt, m, v)),
    IdlType::Object => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Interface,
      name,
    }) => match name.as_str() {
      "ArrayBuffer" => rt.is_array_buffer(v) && !rt.is_shared_array_buffer(v),
      "SharedArrayBuffer" => rt.is_shared_array_buffer(v),
      _ => false,
    },
    _ => false,
  }
}

fn type_matches_data_view<R: WebIdlJsRuntime>(rt: &R, t: &IdlType, v: R::JsValue) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_data_view(rt, inner, v),
    IdlType::Nullable(inner) => type_matches_data_view(rt, inner, v),
    IdlType::Union(members) => members.iter().any(|m| type_matches_data_view(rt, m, v)),
    IdlType::Object => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Interface,
      name,
    }) => name == "DataView" && rt.is_data_view(v),
    _ => false,
  }
}

fn type_matches_typed_array<R: WebIdlJsRuntime>(rt: &R, t: &IdlType, v: R::JsValue) -> bool {
  let Some(actual) = rt.typed_array_name(v) else {
    return false;
  };
  match t {
    IdlType::Annotated { inner, .. } => type_matches_typed_array(rt, inner, v),
    IdlType::Nullable(inner) => type_matches_typed_array(rt, inner, v),
    IdlType::Union(members) => members.iter().any(|m| type_matches_typed_array(rt, m, v)),
    IdlType::Object => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Interface,
      name,
    }) => name == actual,
    _ => false,
  }
}

fn type_matches_callable<R: WebIdlJsRuntime>(rt: &R, t: &IdlType, v: R::JsValue) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_callable(rt, inner, v),
    IdlType::Nullable(inner) => type_matches_callable(rt, inner, v),
    IdlType::Union(members) => members.iter().any(|m| type_matches_callable(rt, m, v)),
    IdlType::Object => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::CallbackFunction,
      ..
    }) => rt.is_callable(v),
    _ => false,
  }
}

fn type_matches_async_sequence(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_async_sequence(inner),
    IdlType::Nullable(inner) => type_matches_async_sequence(inner),
    IdlType::Union(members) => members.iter().any(type_matches_async_sequence),
    IdlType::AsyncSequence(_) => true,
    _ => false,
  }
}

fn type_matches_sequence(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_sequence(inner),
    IdlType::Nullable(inner) => type_matches_sequence(inner),
    IdlType::Union(members) => members.iter().any(type_matches_sequence),
    IdlType::Sequence(_) => true,
    _ => false,
  }
}

fn type_matches_object_or_dictionary_like(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_object_or_dictionary_like(inner),
    IdlType::Nullable(inner) => type_matches_object_or_dictionary_like(inner),
    IdlType::Union(members) => members.iter().any(type_matches_object_or_dictionary_like),
    IdlType::Object | IdlType::Record(_, _) => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::CallbackInterface | NamedTypeKind::Dictionary,
      ..
    }) => true,
    _ => false,
  }
}

fn type_matches_boolean(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_boolean(inner),
    IdlType::Nullable(inner) => type_matches_boolean(inner),
    IdlType::Union(members) => members.iter().any(type_matches_boolean),
    IdlType::Boolean => true,
    _ => false,
  }
}

fn type_matches_numeric(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_numeric(inner),
    IdlType::Nullable(inner) => type_matches_numeric(inner),
    IdlType::Union(members) => members.iter().any(type_matches_numeric),
    IdlType::Numeric(_) => true,
    _ => false,
  }
}

fn type_matches_bigint(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_bigint(inner),
    IdlType::Nullable(inner) => type_matches_bigint(inner),
    IdlType::Union(members) => members.iter().any(type_matches_bigint),
    IdlType::BigInt => true,
    _ => false,
  }
}

fn type_matches_string(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_string(inner),
    IdlType::Nullable(inner) => type_matches_string(inner),
    IdlType::Union(members) => members.iter().any(type_matches_string),
    IdlType::String(_) => true,
    IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Enum,
      ..
    }) => true,
    _ => false,
  }
}

fn create_sequence_from_iterable<R: WebIdlJsRuntime>(
  rt: &mut R,
  elem_ty: &IdlType,
  value: R::JsValue,
  method: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  let mut record: IteratorRecord<R::JsValue> = rt.get_iterator_from_method(value, method)?;
  let mut out = Vec::<WebIdlValue<R::JsValue>>::new();
  while let Some(next) = rt.iterator_step_value(&mut record)? {
    out.push(convert_js_to_idl(rt, elem_ty, next)?);
  }
  Ok(WebIdlValue::Sequence {
    elem_ty: Box::new(elem_ty.clone()),
    values: out,
  })
}

fn convert_default<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: &IrWebIdlValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  Ok(match v {
    IrWebIdlValue::Undefined => WebIdlValue::Undefined,
    IrWebIdlValue::Null => WebIdlValue::Null,
    IrWebIdlValue::Boolean(b) => WebIdlValue::Boolean(*b),
    IrWebIdlValue::Byte(n) => WebIdlValue::Byte(*n),
    IrWebIdlValue::Octet(n) => WebIdlValue::Octet(*n),
    IrWebIdlValue::Short(n) => WebIdlValue::Short(*n),
    IrWebIdlValue::UnsignedShort(n) => WebIdlValue::UnsignedShort(*n),
    IrWebIdlValue::Long(n) => WebIdlValue::Long(*n),
    IrWebIdlValue::UnsignedLong(n) => WebIdlValue::UnsignedLong(*n),
    IrWebIdlValue::LongLong(n) => WebIdlValue::LongLong(*n),
    IrWebIdlValue::UnsignedLongLong(n) => WebIdlValue::UnsignedLongLong(*n),
    IrWebIdlValue::Float(n) => WebIdlValue::Float(*n),
    IrWebIdlValue::UnrestrictedFloat(n) => WebIdlValue::UnrestrictedFloat(*n),
    IrWebIdlValue::Double(n) => WebIdlValue::Double(*n),
    IrWebIdlValue::UnrestrictedDouble(n) => WebIdlValue::UnrestrictedDouble(*n),
    IrWebIdlValue::String(s) => WebIdlValue::String(rt.alloc_string(s)?),
    IrWebIdlValue::Enum(s) => WebIdlValue::Enum(rt.alloc_string(s)?),
    IrWebIdlValue::PlatformObject(obj) => {
      let js = rt
        .platform_object_to_js_value(obj)
        .ok_or_else(|| rt.throw_type_error("platform object default value does not belong to this runtime"))?;
      WebIdlValue::JsValue(js)
    }
    IrWebIdlValue::Sequence { elem_ty, values } => WebIdlValue::Sequence {
      elem_ty: elem_ty.clone(),
      values: values
        .iter()
        .map(|v| convert_default(rt, v))
        .collect::<Result<Vec<_>, _>>()?,
    },
    IrWebIdlValue::Record { .. } => {
      return Err(rt.throw_type_error(
        "record default values are not supported by overload resolution yet",
      ));
    }
    IrWebIdlValue::Dictionary { name, members } => WebIdlValue::Dictionary {
      name: name.clone(),
      members: members
        .iter()
        .map(|(k, v)| Ok((k.clone(), convert_default(rt, v)?)))
        .collect::<Result<BTreeMap<_, _>, _>>()?,
    },
    IrWebIdlValue::Union { member_ty, value } => WebIdlValue::Union {
      member_ty: member_ty.clone(),
      value: Box::new(convert_default(rt, value)?),
    },
  })
}

#[derive(Debug, Clone, Copy, Default)]
struct IntegerConversionAttrs {
  clamp: bool,
  enforce_range: bool,
}

fn convert_js_to_idl<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: &IdlType,
  value: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  convert_js_to_idl_inner(rt, ty, value, IntegerConversionAttrs::default())
}

fn convert_js_to_idl_inner<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: &IdlType,
  value: R::JsValue,
  int_attrs: IntegerConversionAttrs,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  match ty {
    IdlType::Annotated { annotations, inner } => {
      let mut out_attrs = int_attrs;
      for a in annotations {
        match a {
          TypeAnnotation::Clamp => out_attrs.clamp = true,
          TypeAnnotation::EnforceRange => out_attrs.enforce_range = true,
          _ => {}
        }
      }
      if out_attrs.clamp && out_attrs.enforce_range {
        return Err(rt.throw_type_error(
          "[Clamp] and [EnforceRange] cannot both apply to the same type",
        ));
      }
      convert_js_to_idl_inner(rt, inner, value, out_attrs)
    }
    IdlType::Any => Ok(WebIdlValue::JsValue(value)),
    IdlType::Undefined => {
      if !is_undefined(rt, value)? {
        return Err(rt.throw_type_error("expected `undefined`"));
      }
      Ok(WebIdlValue::Undefined)
    }
    IdlType::Boolean => Ok(WebIdlValue::Boolean(rt.to_boolean(value)?)),
    IdlType::Numeric(n) => convert_numeric(rt, *n, value, int_attrs),
    IdlType::BigInt => {
      if rt.is_bigint(value) {
        return Ok(WebIdlValue::JsValue(value));
      }
      Ok(WebIdlValue::JsValue(rt.to_bigint(value)?))
    }
    IdlType::String(_) => Ok(WebIdlValue::String(rt.to_string(value)?)),
    IdlType::Object => {
      if !rt.is_object(value) {
        return Err(rt.throw_type_error("value is not an object"));
      }
      Ok(WebIdlValue::JsValue(value))
    }
    IdlType::Symbol => {
      if !rt.is_symbol(value) {
        return Err(rt.throw_type_error("value is not a symbol"));
      }
      Ok(WebIdlValue::JsValue(value))
    }
    IdlType::Named(named) => convert_named(rt, named, value),
    IdlType::Nullable(inner) => {
      if matches!(null_or_undefined(rt, value)?, Some(_)) {
        return Ok(WebIdlValue::Null);
      }
      convert_js_to_idl_inner(rt, inner, value, int_attrs)
    }
    IdlType::Union(_) => convert_union(rt, ty, value),
    IdlType::Sequence(elem_ty) => {
      if !rt.is_object(value) {
        return Err(rt.throw_type_error("value is not an object"));
      }
      let iter_key = rt.symbol_iterator()?;
      let method = rt.get_method(value, iter_key)?;
      let Some(method_value) = method else {
        return Err(rt.throw_type_error("value is not iterable"));
      };
      create_sequence_from_iterable(rt, elem_ty, value, method_value)
    }
    IdlType::FrozenArray(_)
    | IdlType::AsyncSequence(_)
    | IdlType::Record(_, _)
    | IdlType::Promise(_) => Err(rt.throw_type_error(
      "conversion for this WebIDL type is not implemented yet",
    )),
  }
}

fn convert_named<R: WebIdlJsRuntime>(
  rt: &mut R,
  named: &webidl_ir::NamedType,
  value: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  match named.kind {
    NamedTypeKind::Interface => {
      if !rt.is_platform_object(value) {
        return Err(rt.throw_type_error("value is not a platform object"));
      }
      if !rt.implements_interface(value, &named.name) {
        return Err(rt.throw_type_error(
          "platform object does not implement the required interface",
        ));
      }
      Ok(WebIdlValue::JsValue(value))
    }
    NamedTypeKind::Dictionary => {
      if matches!(null_or_undefined(rt, value)?, Some(_)) {
        return Ok(WebIdlValue::Dictionary {
          name: named.name.clone(),
          members: BTreeMap::new(),
        });
      }
      if !rt.is_object(value) {
        return Err(rt.throw_type_error("value is not an object"));
      }
      Ok(WebIdlValue::Dictionary {
        name: named.name.clone(),
        members: BTreeMap::new(),
      })
    }
    NamedTypeKind::CallbackFunction => {
      if !rt.is_callable(value) {
        return Err(rt.throw_type_error("value is not callable"));
      }
      Ok(WebIdlValue::JsValue(value))
    }
    NamedTypeKind::CallbackInterface => {
      if !rt.is_object(value) {
        return Err(rt.throw_type_error("value is not an object"));
      }
      Ok(WebIdlValue::JsValue(value))
    }
    NamedTypeKind::Enum => Ok(WebIdlValue::Enum(rt.to_string(value)?)),
    NamedTypeKind::Typedef | NamedTypeKind::Unresolved => Err(rt.throw_type_error(
      "typedefs must be resolved before conversion",
    )),
  }
}

fn convert_union<R: WebIdlJsRuntime>(
  rt: &mut R,
  union_ty: &IdlType,
  value: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  // Spec: https://webidl.spec.whatwg.org/#js-to-union

  if union_ty.includes_undefined() && is_undefined(rt, value)? {
    return Ok(WebIdlValue::Union {
      member_ty: Box::new(IdlType::Undefined),
      value: Box::new(WebIdlValue::Undefined),
    });
  }

  if union_ty.includes_nullable_type() && matches!(null_or_undefined(rt, value)?, Some(_)) {
    // Union nullable members are flattened, so we don't preserve the exact nullable member type.
    return Ok(WebIdlValue::Null);
  }

  let types = union_ty.flattened_union_member_types();

  // null/undefined with dictionary member.
  if matches!(null_or_undefined(rt, value)?, Some(_)) {
    if let Some(dict) = types.iter().find(|t| is_dictionary_type(t)) {
      let IdlType::Named(named) = dict.innermost_type() else {
        return Err(rt.throw_type_error("unexpected dictionary type shape"));
      };
      return Ok(WebIdlValue::Dictionary {
        name: named.name.clone(),
        members: BTreeMap::new(),
      });
    }
  }

  // platform object members.
  if rt.is_platform_object(value) {
    if let Some(iface) = types.iter().find_map(|t| match t.innermost_type() {
      IdlType::Named(webidl_ir::NamedType {
        kind: NamedTypeKind::Interface,
        name,
      }) if rt.implements_interface(value, name) => Some(t.clone()),
      _ => None,
    }) {
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(iface),
        value: Box::new(WebIdlValue::JsValue(value)),
      });
    }
    if types.iter().any(|t| matches!(t.innermost_type(), IdlType::Object)) {
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(IdlType::Object),
        value: Box::new(WebIdlValue::JsValue(value)),
      });
    }
  }

  // Numbers/bool/bigint dispatch.
  if rt.is_boolean(value) {
    if types.iter().any(|t| matches!(t.innermost_type(), IdlType::Boolean)) {
      let out = convert_js_to_idl(rt, &IdlType::Boolean, value)?;
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(IdlType::Boolean),
        value: Box::new(out),
      });
    }
  }
  if rt.is_number(value) {
    if let Some((numeric, member_ty)) = types.iter().find_map(|t| match t.innermost_type() {
      IdlType::Numeric(n) => Some((*n, t.clone())),
      _ => None,
    }) {
      let out = convert_numeric(rt, numeric, value, IntegerConversionAttrs::default())?;
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(member_ty),
        value: Box::new(out),
      });
    }
  }
  if rt.is_bigint(value) {
    if types.iter().any(|t| matches!(t.innermost_type(), IdlType::BigInt)) {
      let out = convert_js_to_idl(rt, &IdlType::BigInt, value)?;
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(IdlType::BigInt),
        value: Box::new(out),
      });
    }
  }

  // String dispatch.
  if let Some(string_ty) = types.iter().find_map(|t| match t.innermost_type() {
    IdlType::String(_)
    | IdlType::Named(webidl_ir::NamedType {
      kind: NamedTypeKind::Enum,
      ..
    }) => Some(t.clone()),
    _ => None,
  }) {
    let out = convert_js_to_idl(rt, &string_ty, value)?;
    return Ok(WebIdlValue::Union {
      member_ty: Box::new(string_ty),
      value: Box::new(out),
    });
  }

  Err(rt.throw_type_error("value does not match any union member type"))
}

fn convert_numeric<R: WebIdlJsRuntime>(
  rt: &mut R,
  numeric_ty: NumericType,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  match numeric_ty {
    NumericType::Byte => Ok(WebIdlValue::Byte(
      convert_to_int(rt, value, 8, true, attrs)? as i8,
    )),
    NumericType::Octet => Ok(WebIdlValue::Octet(
      convert_to_int(rt, value, 8, false, attrs)? as u8,
    )),
    NumericType::Short => Ok(WebIdlValue::Short(
      convert_to_int(rt, value, 16, true, attrs)? as i16,
    )),
    NumericType::UnsignedShort => Ok(WebIdlValue::UnsignedShort(
      convert_to_int(rt, value, 16, false, attrs)? as u16,
    )),
    NumericType::Long => Ok(WebIdlValue::Long(
      convert_to_int(rt, value, 32, true, attrs)? as i32,
    )),
    NumericType::UnsignedLong => Ok(WebIdlValue::UnsignedLong(
      convert_to_int(rt, value, 32, false, attrs)? as u32,
    )),
    NumericType::LongLong => Ok(WebIdlValue::LongLong(
      convert_to_int(rt, value, 64, true, attrs)? as i64,
    )),
    NumericType::UnsignedLongLong => Ok(WebIdlValue::UnsignedLongLong(
      convert_to_int(rt, value, 64, false, attrs)? as u64,
    )),
    NumericType::Float => convert_float(rt, value, false),
    NumericType::UnrestrictedFloat => convert_float(rt, value, true),
    NumericType::Double => convert_double(rt, value, false),
    NumericType::UnrestrictedDouble => convert_double(rt, value, true),
  }
}

fn convert_to_int<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  bit_length: u32,
  signed: bool,
  attrs: IntegerConversionAttrs,
) -> Result<f64, R::Error> {
  // Spec: https://webidl.spec.whatwg.org/#abstract-opdef-converttoint

  let (lower_bound, upper_bound) = if bit_length == 64 {
    // WebIDL defines `long long`/`unsigned long long` conversion bounds using the "safe integer"
    // range because ECMAScript Numbers cannot precisely represent all 64-bit integers.
    let upper_bound = (1u64 << 53) as f64 - 1.0;
    let lower_bound = if signed {
      -((1u64 << 53) as f64) + 1.0
    } else {
      0.0
    };
    (lower_bound, upper_bound)
  } else if signed {
    let lower_bound = -((1u64 << (bit_length - 1)) as f64);
    let upper_bound = ((1u64 << (bit_length - 1)) as f64) - 1.0;
    (lower_bound, upper_bound)
  } else {
    let lower_bound = 0.0;
    let upper_bound = ((1u64 << bit_length) as f64) - 1.0;
    (lower_bound, upper_bound)
  };

  let mut x = rt.to_number(value)?;
  // Normalize -0 to +0.
  if x == 0.0 && x.is_sign_negative() {
    x = 0.0;
  }

  if attrs.enforce_range {
    if x.is_nan() || x.is_infinite() {
      return Err(rt.throw_range_error(
        "EnforceRange integer conversion cannot be NaN/Infinity",
      ));
    }
    x = integer_part(x);
    if x < lower_bound || x > upper_bound {
      return Err(rt.throw_range_error(
        "integer value is outside EnforceRange bounds",
      ));
    }
    return Ok(x);
  }

  if !x.is_nan() && attrs.clamp {
    x = x.clamp(lower_bound, upper_bound);
    x = round_ties_even(x);
    if x == 0.0 && x.is_sign_negative() {
      x = 0.0;
    }
    return Ok(x);
  }

  if x.is_nan() || x == 0.0 || x.is_infinite() {
    return Ok(0.0);
  }

  x = integer_part(x);
  let modulo = 2f64.powi(bit_length as i32);
  x = x.rem_euclid(modulo);
  if signed {
    let threshold = 2f64.powi((bit_length - 1) as i32);
    if x >= threshold {
      return Ok(x - modulo);
    }
  }
  Ok(x)
}

fn integer_part(n: f64) -> f64 {
  let r = n.abs().floor();
  if n < 0.0 { -r } else { r }
}

fn round_ties_even(n: f64) -> f64 {
  let floor = n.floor();
  let frac = n - floor;
  if frac < 0.5 {
    return floor;
  }
  if frac > 0.5 {
    return floor + 1.0;
  }
  // Exactly halfway between two integers.
  let floor_int = floor as i64;
  if floor_int % 2 == 0 { floor } else { floor + 1.0 }
}

fn convert_float<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  unrestricted: bool,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  let x = rt.to_number(value)?;
  if x.is_nan() {
    if unrestricted {
      return Ok(WebIdlValue::UnrestrictedFloat(f32::from_bits(0x7fc0_0000)));
    }
    return Err(rt.throw_type_error("float value cannot be NaN"));
  }
  if x.is_infinite() {
    if unrestricted {
      return Ok(WebIdlValue::UnrestrictedFloat(x as f32));
    }
    return Err(rt.throw_type_error("float value cannot be Infinity"));
  }
  let y = x as f32;
  if y.is_infinite() {
    if unrestricted {
      return Ok(WebIdlValue::UnrestrictedFloat(y));
    }
    return Err(rt.throw_type_error("float value is out of range"));
  }
  if unrestricted {
    Ok(WebIdlValue::UnrestrictedFloat(y))
  } else {
    Ok(WebIdlValue::Float(y))
  }
}

fn convert_double<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  unrestricted: bool,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  let x = rt.to_number(value)?;
  if x.is_nan() {
    if unrestricted {
      return Ok(WebIdlValue::UnrestrictedDouble(f64::from_bits(
        0x7ff8_0000_0000_0000,
      )));
    }
    return Err(rt.throw_type_error("double value cannot be NaN"));
  }
  if x.is_infinite() {
    if unrestricted {
      return Ok(WebIdlValue::UnrestrictedDouble(x));
    }
    return Err(rt.throw_type_error("double value cannot be Infinity"));
  }
  if unrestricted {
    Ok(WebIdlValue::UnrestrictedDouble(x))
  } else {
    Ok(WebIdlValue::Double(x))
  }
}

fn format_overload_candidates(overloads: &[OverloadSig]) -> String {
  let mut out = String::new();
  for (idx, o) in overloads.iter().enumerate() {
    if idx != 0 {
      out.push('\n');
    }
    out.push_str("  - ");
    out.push_str(&format_overload_sig(o));
  }
  out
}

fn format_effective_entries(entries: &[EffectiveEntry]) -> String {
  let mut out = String::new();
  for (idx, e) in entries.iter().enumerate() {
    if idx != 0 {
      out.push('\n');
    }
    out.push_str("  - ");
    out.push_str(&format_effective_entry(e));
  }
  out
}

fn format_overload_sig(o: &OverloadSig) -> String {
  let args = o
    .args
    .iter()
    .map(format_idl_arg)
    .collect::<Vec<_>>()
    .join(", ");
  format!("#{}({})", o.decl_index, args)
}

fn format_effective_entry(e: &EffectiveEntry) -> String {
  let args = e
    .type_list
    .iter()
    .zip(e.optionality_list.iter())
    .map(|(t, o)| match o {
      Optionality::Required => format_idl_type(t),
      Optionality::Optional => format!("optional {}", format_idl_type(t)),
      Optionality::Variadic => format!("{}...", format_idl_type(t)),
    })
    .collect::<Vec<_>>()
    .join(", ");
  format!("overload #{} ({})", e.overload_index, args)
}

fn format_idl_arg(a: &OverloadArg) -> String {
  let mut out = String::new();
  match a.optionality {
    Optionality::Optional => out.push_str("optional "),
    Optionality::Variadic | Optionality::Required => {}
  }
  out.push_str(&format_idl_type(&a.ty));
  if a.optionality == Optionality::Variadic {
    out.push_str("...");
  }
  if a.optionality == Optionality::Optional && a.default.is_some() {
    out.push_str(" = <default>");
  }
  out
}

fn format_idl_type(ty: &IdlType) -> String {
  match ty {
    IdlType::Any => "any".into(),
    IdlType::Undefined => "undefined".into(),
    IdlType::Boolean => "boolean".into(),
    IdlType::Numeric(n) => format!("{:?}", n).to_lowercase(),
    IdlType::BigInt => "bigint".into(),
    IdlType::String(s) => format!("{:?}", s),
    IdlType::Object => "object".into(),
    IdlType::Symbol => "symbol".into(),
    IdlType::Named(named) => named.name.clone(),
    IdlType::Nullable(inner) => format!("{}?", format_idl_type(inner)),
    IdlType::Union(members) => {
      let inner = members
        .iter()
        .map(format_idl_type)
        .collect::<Vec<_>>()
        .join(" or ");
      format!("({inner})")
    }
    IdlType::Sequence(inner) => format!("sequence<{}>", format_idl_type(inner)),
    IdlType::FrozenArray(inner) => format!("FrozenArray<{}>", format_idl_type(inner)),
    IdlType::AsyncSequence(inner) => format!("async sequence<{}>", format_idl_type(inner)),
    IdlType::Record(k, v) => format!("record<{}, {}>", format_idl_type(k), format_idl_type(v)),
    IdlType::Promise(inner) => format!("Promise<{}>", format_idl_type(inner)),
    IdlType::Annotated { annotations, inner } => {
      let mut out = String::new();
      for a in annotations {
        match a {
          TypeAnnotation::Clamp => out.push_str("[Clamp] "),
          TypeAnnotation::EnforceRange => out.push_str("[EnforceRange] "),
          TypeAnnotation::LegacyNullToEmptyString => out.push_str("[LegacyNullToEmptyString] "),
          TypeAnnotation::LegacyTreatNonObjectAsNull => out.push_str("[LegacyTreatNonObjectAsNull] "),
          _ => {}
        }
      }
      out.push_str(&format_idl_type(inner));
      out
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::JsRuntime;
  use crate::VmJsRuntime;
  use vm_js::{PropertyKey, Value, VmError};

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  fn thrown_message(rt: &mut VmJsRuntime, err: VmError) -> String {
    let VmError::Throw(v) = err else {
      panic!("expected throw");
    };
    let Value::Object(obj) = v else {
      panic!("expected object");
    };
    let key_value = rt.alloc_string_value("message").unwrap();
    let Value::String(key) = key_value else {
      panic!("expected string value for key");
    };
    let msg = rt.get(Value::Object(obj), PropertyKey::String(key)).unwrap();
    let msg = rt.to_string(msg).unwrap();
    let Value::String(msg) = msg else {
      panic!("expected string message");
    };
    rt.heap().get_string(msg).unwrap().to_utf8_lossy()
  }

  #[test]
  fn overload_mismatch_error_message_includes_candidates() {
    let mut rt = VmJsRuntime::new();

    let err = throw_no_matching_overload(
      &mut rt,
      "doThing",
      2,
      &["doThing(DOMString)", "doThing()", "doThing(long, long)"],
    );

    let VmError::Throw(thrown) = err else {
      panic!("expected VmError::Throw, got {err:?}");
    };

    let s = rt.to_string(thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);

    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
    assert!(msg.contains("doThing"));
    assert!(msg.contains("2"));
    assert!(msg.contains("Candidates:"));

    let idx_empty = msg.find("doThing()").expect("missing doThing() signature");
    let idx_dom = msg
      .find("doThing(DOMString)")
      .expect("missing doThing(DOMString) signature");
    let idx_ll = msg
      .find("doThing(long, long)")
      .expect("missing doThing(long, long) signature");

    assert!(
      idx_empty < idx_dom && idx_dom < idx_ll,
      "expected lexicographically sorted candidates, got {msg:?}"
    );
  }

  #[test]
  fn spec_overload_set_example_selects_correct_overload() {
    let mut rt = VmJsRuntime::new();

    let overloads = vec![
      // f1: f(DOMString a)
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(webidl_ir::StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      // f2: f(Node a, DOMString b, double... c)
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::Named(webidl_ir::NamedType {
              name: "Node".into(),
              kind: NamedTypeKind::Interface,
            }),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(webidl_ir::StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::Numeric(NumericType::Double),
            optionality: Optionality::Variadic,
            default: None,
          },
        ],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
      // f3: f()
      OverloadSig {
        args: vec![],
        decl_index: 2,
        distinguishing_arg_index_by_arg_count: None,
      },
      // f4: f(Event a, DOMString b, optional DOMString c, double... d)
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::Named(webidl_ir::NamedType {
              name: "Event".into(),
              kind: NamedTypeKind::Interface,
            }),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(webidl_ir::StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(webidl_ir::StringType::DomString),
            optionality: Optionality::Optional,
            default: None,
          },
          OverloadArg {
            ty: IdlType::Numeric(NumericType::Double),
            optionality: Optionality::Variadic,
            default: None,
          },
        ],
        decl_index: 3,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    // f() selects f3.
    let out = resolve_overload(&mut rt, &overloads, &[]).unwrap();
    assert_eq!(out.overload_index, 2);
    assert!(out.values.is_empty());

    // f("x") selects f1.
    let x = rt.alloc_string_value("x").unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[x]).unwrap();
    assert_eq!(out.overload_index, 0);
    assert_eq!(out.values.len(), 1);

    // f(Node, "b") selects f2.
    let node = rt
      .alloc_platform_object_value("Node", &[], 1)
      .unwrap();
    let b = rt.alloc_string_value("b").unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[node, b]).unwrap();
    assert_eq!(out.overload_index, 1);

    // f(Event, "b", undefined) selects f4 and marks optional c as missing.
    let event = rt
      .alloc_platform_object_value("Event", &[], 2)
      .unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[event, b, Value::Undefined]).unwrap();
    assert_eq!(out.overload_index, 3);
    assert_eq!(
      out.values,
      vec![
        ConvertedArgument::Value(WebIdlValue::JsValue(event)),
        ConvertedArgument::Value(WebIdlValue::String(b)),
        ConvertedArgument::Missing,
      ]
    );
  }

  #[test]
  fn url_constructor_like_overloads_select_by_argument_count() {
    let mut rt = VmJsRuntime::new();

    // Real-world-ish: URL(url) vs URL(url, base)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(webidl_ir::StringType::UsvString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::String(webidl_ir::StringType::UsvString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(webidl_ir::StringType::UsvString),
            optionality: Optionality::Required,
            default: None,
          },
        ],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let url = rt.alloc_string_value("https://example.com/").unwrap();
    let base = rt.alloc_string_value("https://base.example/").unwrap();

    let out = resolve_overload(&mut rt, &overloads, &[url]).unwrap();
    assert_eq!(out.overload_index, 0);

    let out = resolve_overload(&mut rt, &overloads, &[url, base]).unwrap();
    assert_eq!(out.overload_index, 1);
  }

  #[test]
  fn overload_resolution_no_match_throws_type_error() {
    let mut rt = VmJsRuntime::new();
    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Boolean,
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let err = resolve_overload(&mut rt, &overloads, &[]).unwrap_err();
    let msg = thrown_message(&mut rt, err);
    assert!(msg.contains("No matching overload"));
  }

  #[test]
  fn overload_resolution_ambiguous_overload_set_throws_type_error() {
    let mut rt = VmJsRuntime::new();
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(webidl_ir::StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(webidl_ir::StringType::UsvString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let x = rt.alloc_string_value("x").unwrap();
    let err = resolve_overload(&mut rt, &overloads, &[x]).unwrap_err();
    let msg = thrown_message(&mut rt, err);
    assert!(msg.contains("Ambiguous"));
  }

  #[test]
  fn overload_resolution_getter_throw_propagates() {
    let mut rt = VmJsRuntime::new();

    // Overloads: f(sequence<any>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::Sequence(Box::new(IdlType::Any)),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(webidl_ir::StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let getter = rt
      .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("boom")))
      .unwrap();
    let obj = rt.alloc_object_value().unwrap();

    let iter_key = rt.symbol_iterator().unwrap();
    rt.define_accessor_property(obj, iter_key, getter, Value::Undefined, true)
      .unwrap();

    let err = resolve_overload(&mut rt, &overloads, &[obj]).unwrap_err();
    let msg = thrown_message(&mut rt, err);
    assert!(msg.contains("boom"));
  }

  #[test]
  fn optional_argument_default_is_used_when_undefined() {
    let mut rt = VmJsRuntime::new();
    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::String(webidl_ir::StringType::DomString),
        optionality: Optionality::Optional,
        default: Some(IrWebIdlValue::String("foo".to_string())),
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let out = resolve_overload(&mut rt, &overloads, &[Value::Undefined]).unwrap();
    assert_eq!(out.overload_index, 0);
    let [ConvertedArgument::Value(WebIdlValue::String(v))] = out.values.as_slice() else {
      panic!("expected exactly one converted string argument");
    };
    let Value::String(handle) = *v else {
      panic!("expected JS string value");
    };
    assert_eq!(rt.heap().get_string(handle).unwrap().to_utf8_lossy(), "foo");
  }
}
