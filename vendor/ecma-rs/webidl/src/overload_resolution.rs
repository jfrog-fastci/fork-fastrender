//! WebIDL overload resolution and helpers.
//!
//! This module implements the runtime portion of the WHATWG WebIDL
//! [overload resolution algorithm](https://webidl.spec.whatwg.org/#js-overloads).
//! Overload resolution is used heavily by DOM APIs (e.g. `CanvasRenderingContext2D.drawImage`,
//! `CSS.supports`, various `Document` methods).
//!
//! The goal is for generated bindings to be able to share one correct implementation, rather than
//! emitting bespoke dispatch code per overloaded operation.

use crate::conversions_shared;
use crate::runtime::WebIdlJsRuntime;
use std::collections::BTreeMap;
use crate::ir::{
  DefaultValue, DistinguishabilityCategory, IdlType, NamedType, NamedTypeKind, NumericLiteral,
  NumericType, TypeAnnotation,
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

  let mut message =
    format!("No matching overload for {operation_name} with {provided_argc} arguments.");
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
  pub default: Option<DefaultValue>,
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

  Record {
    key_ty: Box<IdlType>,
    value_ty: Box<IdlType>,
    entries: Vec<(String, WebIdlValue<V>)>,
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
  let has_variadic = overloads.iter().any(|o| {
    o.args
      .last()
      .is_some_and(|a| a.optionality == Optionality::Variadic)
  });
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

  // Root `args` and any JS values produced by conversions for the duration of later conversions.
  let mut roots: Vec<R::JsValue> = Vec::new();
  roots.extend_from_slice(args);
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

    let converted = rt.with_stack_roots(&roots, |rt| {
      if optionality.is_optional() && is_undefined(rt, v)? {
        if let Some(default) = overloads[entries[0].overload_index].args[i]
          .default
          .as_ref()
        {
          Ok(ConvertedArgument::Value(convert_default(rt, &ty, default)?))
        } else {
          Ok(ConvertedArgument::Missing)
        }
      } else {
        Ok(ConvertedArgument::Value(convert_js_to_idl(rt, &ty, v)?))
      }
    })?;

    if let ConvertedArgument::Value(v) = &converted {
      append_webidl_value_roots(&mut roots, v);
    }

    values.push(converted);
    i += 1;
  }

  // 12. If i = d, then resolve the overload using args[i].
  if (i as isize) == d {
    let v = args[i];
    let mut selected_method: Option<R::JsValue> = None;

    rt.with_stack_roots(&roots, |rt| {
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
        && entries
          .iter()
          .any(|e| type_matches_nullable_dictionary(&e.type_list[i]))
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
      // Distinguishability requirement (d):
      // If V is a String object and the overload set includes a string type at position i, then it
      // must be treated as a string and must not be probed for iterator-based conversions (e.g.
      // sequence/FrozenArray).
      else if rt.is_object(v)
        && rt.is_string_object(v)
        && entries.iter().any(|e| type_matches_string(&e.type_list[i]))
      {
        entries.retain(|e| type_matches_string(&e.type_list[i]));
      }
      // 12.9 async sequence special-case.
      else if rt.is_object(v)
        && entries
          .iter()
          .any(|e| type_matches_async_sequence(&e.type_list[i]))
      {
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
      else if rt.is_object(v)
        && entries
          .iter()
          .any(|e| type_matches_sequence(&e.type_list[i]))
      {
        let iter_key = rt.symbol_iterator()?;
        let m = rt.get_method(v, iter_key)?;
        if let Some(method_value) = m {
          selected_method = Some(method_value);
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
      else if rt.is_boolean(v)
        && entries
          .iter()
          .any(|e| type_matches_boolean(&e.type_list[i]))
      {
        entries.retain(|e| type_matches_boolean(&e.type_list[i]));
      }
      // 12.13 number special-case.
      else if rt.is_number(v)
        && entries
          .iter()
          .any(|e| type_matches_numeric(&e.type_list[i]))
      {
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
      else if entries
        .iter()
        .any(|e| type_matches_numeric(&e.type_list[i]))
      {
        entries.retain(|e| type_matches_numeric(&e.type_list[i]));
      }
      // 12.17 boolean fallthrough.
      else if entries
        .iter()
        .any(|e| type_matches_boolean(&e.type_list[i]))
      {
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
      Ok(())
    })?;

    if let Some(method_value) = selected_method {
      method = Some(method_value);
      roots.push(method_value);
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
      if let Some(elem_ty) = selected_entry
        .type_list
        .get(i)
        .and_then(sequence_element_type)
      {
        let v = args[i];
        let seq = rt.with_stack_roots(&roots, |rt| {
          create_sequence_from_iterable(rt, elem_ty, v, method_value)
        })?;
        append_webidl_value_roots(&mut roots, &seq);
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

    let converted = rt.with_stack_roots(&roots, |rt| {
      if optionality.is_optional() && is_undefined(rt, v)? {
        if let Some(default) = selected_overload.args[i].default.as_ref() {
          Ok(ConvertedArgument::Value(convert_default(rt, &ty, default)?))
        } else {
          Ok(ConvertedArgument::Missing)
        }
      } else {
        Ok(ConvertedArgument::Value(convert_js_to_idl(rt, &ty, v)?))
      }
    })?;

    if let ConvertedArgument::Value(v) = &converted {
      append_webidl_value_roots(&mut roots, v);
    }

    values.push(converted);
    i += 1;
  }

  // 15. Fill remaining declared arguments with default/missing (variadics contribute nothing).
  while i < selected_overload.args.len() {
    let arg = &selected_overload.args[i];
    if let Some(default) = arg.default.as_ref() {
      let v = rt.with_stack_roots(&roots, |rt| convert_default(rt, &arg.ty, default))?;
      append_webidl_value_roots(&mut roots, &v);
      values.push(ConvertedArgument::Value(v));
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
    IdlType::FrozenArray(elem) => Some(elem.as_ref()),
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
    if declared.is_empty()
      || declared
        .iter()
        .all(|a| a.optionality.is_optional_for_trimming())
    {
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
  let first = overloads
    .first()?
    .distinguishing_arg_index_by_arg_count
    .as_ref()?;
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

  if (ca == DistinguishabilityCategory::AsyncSequence
    && cb == DistinguishabilityCategory::SequenceLike)
    || (ca == DistinguishabilityCategory::SequenceLike
      && cb == DistinguishabilityCategory::AsyncSequence)
  {
    return false;
  }

  if ca == DistinguishabilityCategory::Object || cb == DistinguishabilityCategory::Object {
    let other = if ca == DistinguishabilityCategory::Object {
      cb
    } else {
      ca
    };
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
  if (ca == DistinguishabilityCategory::Undefined
    && cb == DistinguishabilityCategory::DictionaryLike)
    || (ca == DistinguishabilityCategory::DictionaryLike
      && cb == DistinguishabilityCategory::Undefined)
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
    IdlType::Named(NamedType {
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
    IdlType::Named(NamedType {
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
    IdlType::Named(NamedType {
      kind: NamedTypeKind::Interface,
      name,
    }) => rt.implements_interface(v, crate::interface_id_from_name(name)),
    _ => false,
  }
}

fn type_matches_array_buffer<R: WebIdlJsRuntime>(rt: &R, t: &IdlType, v: R::JsValue) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_array_buffer(rt, inner, v),
    IdlType::Nullable(inner) => type_matches_array_buffer(rt, inner, v),
    IdlType::Union(members) => members.iter().any(|m| type_matches_array_buffer(rt, m, v)),
    IdlType::Object => true,
    IdlType::Named(NamedType {
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
    IdlType::Named(NamedType {
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
    IdlType::Named(NamedType {
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
    IdlType::Named(NamedType {
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
    IdlType::Sequence(_) | IdlType::FrozenArray(_) => true,
    _ => false,
  }
}

fn type_matches_object_or_dictionary_like(t: &IdlType) -> bool {
  match t {
    IdlType::Annotated { inner, .. } => type_matches_object_or_dictionary_like(inner),
    IdlType::Nullable(inner) => type_matches_object_or_dictionary_like(inner),
    IdlType::Union(members) => members.iter().any(type_matches_object_or_dictionary_like),
    IdlType::Object | IdlType::Record(_, _) => true,
    IdlType::Named(NamedType {
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
    IdlType::Named(NamedType {
      kind: NamedTypeKind::Enum,
      ..
    }) => true,
    _ => false,
  }
}

fn append_webidl_value_roots<V: Copy>(roots: &mut Vec<V>, value: &WebIdlValue<V>) {
  match value {
    WebIdlValue::String(v) | WebIdlValue::Enum(v) | WebIdlValue::JsValue(v) => roots.push(*v),
    WebIdlValue::Sequence { values, .. } => {
      for item in values {
        append_webidl_value_roots(roots, item);
      }
    }
    WebIdlValue::Record { entries, .. } => {
      for (_k, v) in entries {
        append_webidl_value_roots(roots, v);
      }
    }
    WebIdlValue::Dictionary { members, .. } => {
      for (_k, v) in members {
        append_webidl_value_roots(roots, v);
      }
    }
    WebIdlValue::Union { value, .. } => append_webidl_value_roots(roots, value),
    WebIdlValue::Undefined
    | WebIdlValue::Null
    | WebIdlValue::Boolean(_)
    | WebIdlValue::Byte(_)
    | WebIdlValue::Octet(_)
    | WebIdlValue::Short(_)
    | WebIdlValue::UnsignedShort(_)
    | WebIdlValue::Long(_)
    | WebIdlValue::UnsignedLong(_)
    | WebIdlValue::LongLong(_)
    | WebIdlValue::UnsignedLongLong(_)
    | WebIdlValue::Float(_)
    | WebIdlValue::UnrestrictedFloat(_)
    | WebIdlValue::Double(_)
    | WebIdlValue::UnrestrictedDouble(_) => {}
  }
}

fn create_sequence_from_iterable<R: WebIdlJsRuntime>(
  rt: &mut R,
  elem_ty: &IdlType,
  iterable: R::JsValue,
  method: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  let out = conversions_shared::materialize_iterable(
    rt,
    iterable,
    method,
    |rt, next| convert_js_to_idl(rt, elem_ty, next),
    |roots, v| append_webidl_value_roots(roots, v),
  )?;
  Ok(WebIdlValue::Sequence {
    elem_ty: Box::new(elem_ty.clone()),
    values: out,
  })
}

fn convert_record<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  key_ty: &IdlType,
  value_ty: &IdlType,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  // WebIDL "js-to-record" begins by rejecting non-Object values. Record conversion does *not*
  // apply `ToObject` to accept primitives.
  if !rt.is_object(value) {
    return Err(rt.throw_type_error(conversions_shared::VALUE_IS_NOT_OBJECT));
  }
  let obj = value;

  let entries = conversions_shared::materialize_record_entries(
    rt,
    obj,
    |rt, obj, key, key_value| {
      let typed_key_idl = convert_js_to_idl(rt, key_ty, key_value)?;
      let WebIdlValue::String(typed_key_value) = typed_key_idl else {
        return Err(rt.throw_type_error("record key did not convert to a string"));
      };
      let typed_key = rt.string_to_utf8_lossy(typed_key_value)?;

      let prop_value = rt.get(obj, key)?;
      let typed_value = rt.with_stack_roots(&[prop_value], |rt| {
        convert_js_to_idl(rt, value_ty, prop_value)
      })?;
      Ok((typed_key, typed_value))
    },
    |roots, v| append_webidl_value_roots(roots, v),
  )?;

  Ok(WebIdlValue::Record {
    key_ty: Box::new(key_ty.clone()),
    value_ty: Box::new(value_ty.clone()),
    entries,
  })
}

fn convert_default<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: &IdlType,
  v: &DefaultValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  fn numeric_literal_to_f64(lit: &NumericLiteral) -> Result<f64, ()> {
    match lit {
      NumericLiteral::Infinity { negative } => Ok(if *negative {
        f64::NEG_INFINITY
      } else {
        f64::INFINITY
      }),
      NumericLiteral::NaN => Ok(f64::NAN),
      NumericLiteral::Decimal(s) => s.trim().parse::<f64>().map_err(|_| ()),
      NumericLiteral::Integer(s) => parse_integer_literal_to_f64(s),
    }
  }

  fn parse_integer_literal_to_f64(token: &str) -> Result<f64, ()> {
    let token = token.trim();
    if token.is_empty() {
      return Err(());
    }

    let mut sign = 1.0f64;
    let mut rest = token;
    if let Some(after) = rest.strip_prefix('-') {
      sign = -1.0;
      rest = after;
    } else if rest.starts_with('+') {
      return Err(());
    }

    // WebIDL integer token semantics:
    // - if it begins with `0x`/`0X`, the base is 16
    // - else if it begins with `0`, the base is 8 (digits after the leading 0 may be empty)
    // - otherwise the base is 10
    //
    // <https://webidl.spec.whatwg.org/#dfn-value-of-integer-tokens>
    let (base, digits, allow_empty_digits) =
      if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (16u32, hex, false)
      } else if let Some(after_0) = rest.strip_prefix('0') {
        (8u32, after_0, true)
      } else {
        (10u32, rest, false)
      };

    if digits.is_empty() && !allow_empty_digits {
      return Err(());
    }

    let mut v = 0f64;
    for ch in digits.chars() {
      let d = ch.to_digit(base).ok_or(())?;
      v = v * (base as f64) + (d as f64);
    }
    Ok(sign * v)
  }

  let js_value = match v {
    DefaultValue::Undefined => rt.js_undefined(),
    DefaultValue::Null => rt.js_null(),
    DefaultValue::Boolean(b) => rt.js_boolean(*b),
    DefaultValue::Number(lit) => {
      let n = numeric_literal_to_f64(lit)
        .map_err(|_| rt.throw_type_error("invalid numeric literal default value"))?;
      rt.js_number(n)
    }
    DefaultValue::String(s) => rt.alloc_string(s)?,
    DefaultValue::EmptySequence => rt.alloc_array()?,
    DefaultValue::EmptyDictionary => rt.alloc_object()?,
  };

  rt.with_stack_roots(&[js_value], |rt| convert_js_to_idl(rt, ty, js_value))
}

fn to_js_string_with_limits<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
) -> Result<R::JsValue, R::Error> {
  let s = rt.to_string(value)?;
  conversions_shared::enforce_string_code_units_limit(rt, s)?;
  Ok(s)
}

fn convert_js_to_idl<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: &IdlType,
  value: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  convert_js_to_idl_inner(rt, ty, value, crate::IntegerConversionAttrs::default())
}

fn convert_js_to_idl_inner<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: &IdlType,
  value: R::JsValue,
  int_attrs: crate::IntegerConversionAttrs,
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
        return Err(
          rt.throw_type_error("[Clamp] and [EnforceRange] cannot both apply to the same type"),
        );
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
    IdlType::String(_) => Ok(WebIdlValue::String(to_js_string_with_limits(rt, value)?)),
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
    IdlType::Sequence(elem_ty) | IdlType::FrozenArray(elem_ty) => {
      // Spec: <https://webidl.spec.whatwg.org/#js-to-sequence>
      //
      // 1. If V is not an Object, throw a TypeError.
      if !rt.is_object(value) {
        return Err(rt.throw_type_error(conversions_shared::VALUE_IS_NOT_OBJECT));
      }
      rt.with_stack_roots(&[value], |rt| {
        let iter_key = rt.symbol_iterator()?;
        let method = rt.get_method(value, iter_key)?;
        let Some(method_value) = method else {
          return Err(rt.throw_type_error("value is not iterable"));
        };
        create_sequence_from_iterable(rt, elem_ty, value, method_value)
      })
    }
    IdlType::Record(key_ty, value_ty) => convert_record(rt, value, key_ty, value_ty),
    IdlType::AsyncSequence(_elem_ty) => {
      // MVP: validate the value is async-iterable (or at least iterable) and preserve the object
      // reference so bindings can consume it.
      // Spec: <https://webidl.spec.whatwg.org/#js-to-async-iterable>
      //
      // 1. If V is not an Object, throw a TypeError.
      if !rt.is_object(value) {
        return Err(rt.throw_type_error(conversions_shared::VALUE_IS_NOT_OBJECT));
      }
      rt.with_stack_roots(&[value], |rt| {
        let async_iter_key = rt.symbol_async_iterator()?;
        let iter_key = rt.symbol_iterator()?;
        let mut method = rt.get_method(value, async_iter_key)?;
        if method.is_none() {
          method = rt.get_method(value, iter_key)?;
        }
        if method.is_none() {
          return Err(rt.throw_type_error("value is not async iterable"));
        }
        Ok(WebIdlValue::JsValue(value))
      })
    }
    IdlType::Promise(_inner_ty) => {
      if !int_attrs.is_empty() {
        return Err(
          rt.throw_type_error("[Clamp]/[EnforceRange] annotations cannot apply to `Promise`"),
        );
      }
      // Spec: https://webidl.spec.whatwg.org/#es-promise
      //
      // WebIDL `Promise<T>` conversion:
      // 1. Let `promise` be ? PromiseResolve(%Promise%, V).
      let promise = rt.with_stack_roots(&[value], |rt| rt.promise_resolve(value))?;
      Ok(WebIdlValue::JsValue(promise))
    }
  }
}

fn convert_named<R: WebIdlJsRuntime>(
  rt: &mut R,
  named: &NamedType,
  value: R::JsValue,
) -> Result<WebIdlValue<R::JsValue>, R::Error> {
  match named.kind {
    NamedTypeKind::Interface => {
      if !rt.is_platform_object(value) {
        return Err(rt.throw_type_error("value is not a platform object"));
      }
      if !rt.implements_interface(value, crate::interface_id_from_name(&named.name)) {
        return Err(
          rt.throw_type_error("platform object does not implement the required interface"),
        );
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
    NamedTypeKind::Enum => Ok(WebIdlValue::Enum(to_js_string_with_limits(rt, value)?)),
    NamedTypeKind::Typedef | NamedTypeKind::Unresolved => {
      Err(rt.throw_type_error("typedefs must be resolved before conversion"))
    }
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
  if rt.is_object(value) {
    // Interfaces / platform objects.
    if rt.is_platform_object(value) {
      if let Some(iface) = types.iter().find_map(|t| match t.innermost_type() {
        IdlType::Named(NamedType {
          kind: NamedTypeKind::Interface,
          name,
        }) if rt.implements_interface(value, crate::interface_id_from_name(name)) => Some(t.clone()),
        _ => None,
      }) {
        return Ok(WebIdlValue::Union {
          member_ty: Box::new(iface),
          value: Box::new(WebIdlValue::JsValue(value)),
        });
      }
    }

    // Distinguishability requirement (d):
    // If the union includes a string type, then String objects must be treated as strings and must
    // not be probed for iterator methods (e.g. @@iterator for sequence/FrozenArray).
    if rt.is_string_object(value) {
      if let Some(string_ty) = types.iter().find_map(|t| match t.innermost_type() {
        IdlType::String(_)
        | IdlType::Named(NamedType {
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
    }

    // Async sequence.
    if let Some(async_ty) = types
      .iter()
      .find(|t| matches!(t.innermost_type(), IdlType::AsyncSequence(_)))
      .cloned()
    {
      let has_string_type = types.iter().any(|t| type_matches_string(t));
      let skip_async_sequence = rt.is_string_object(value) && has_string_type;

      if !skip_async_sequence {
        let async_iter_key = rt.symbol_async_iterator()?;
        let iter_key = rt.symbol_iterator()?;
        let mut m = rt.get_method(value, async_iter_key)?;
        if m.is_none() {
          m = rt.get_method(value, iter_key)?;
        }
        if m.is_some() {
          let out = convert_js_to_idl(rt, &async_ty, value)?;
          return Ok(WebIdlValue::Union {
            member_ty: Box::new(async_ty),
            value: Box::new(out),
          });
        }
      }
    }

    // Sequence / FrozenArray.
    if let Some(seq_ty) = types
      .iter()
      .find(|t| matches!(t.innermost_type(), IdlType::Sequence(_) | IdlType::FrozenArray(_)))
      .cloned()
    {
      let iter_key = rt.symbol_iterator()?;
      let method = rt.get_method(value, iter_key)?;
      if let Some(method_value) = method {
        let Some(elem_ty) = sequence_element_type(&seq_ty) else {
          return Err(rt.throw_type_error("unexpected sequence type shape"));
        };
        let seq = create_sequence_from_iterable(rt, elem_ty, value, method_value)?;
        return Ok(WebIdlValue::Union {
          member_ty: Box::new(seq_ty),
          value: Box::new(seq),
        });
      }
    }

    // Dictionary.
    if let Some(dict_ty) = types.iter().find(|t| is_dictionary_type(t)).cloned() {
      let out = convert_js_to_idl(rt, &dict_ty, value)?;
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(dict_ty),
        value: Box::new(out),
      });
    }

    // Record.
    if let Some(record_ty) = types
      .iter()
      .find(|t| matches!(t.innermost_type(), IdlType::Record(_, _)))
      .cloned()
    {
      let out = convert_js_to_idl(rt, &record_ty, value)?;
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(record_ty),
        value: Box::new(out),
      });
    }

    // object
    if types
      .iter()
      .any(|t| matches!(t.innermost_type(), IdlType::Object))
    {
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(IdlType::Object),
        value: Box::new(WebIdlValue::JsValue(value)),
      });
    }
  }

  // Numbers/bool/bigint dispatch.
  if rt.is_boolean(value) {
    if types
      .iter()
      .any(|t| matches!(t.innermost_type(), IdlType::Boolean))
    {
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
      let out = convert_numeric(rt, numeric, value, crate::IntegerConversionAttrs::default())?;
      return Ok(WebIdlValue::Union {
        member_ty: Box::new(member_ty),
        value: Box::new(out),
      });
    }
  }
  if rt.is_bigint(value) {
    if types
      .iter()
      .any(|t| matches!(t.innermost_type(), IdlType::BigInt))
    {
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
    | IdlType::Named(NamedType {
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
  attrs: crate::IntegerConversionAttrs,
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
    NumericType::UnsignedShort => Ok(WebIdlValue::UnsignedShort(convert_to_int(
      rt, value, 16, false, attrs,
    )? as u16)),
    NumericType::Long => Ok(WebIdlValue::Long(
      convert_to_int(rt, value, 32, true, attrs)? as i32,
    )),
    NumericType::UnsignedLong => Ok(WebIdlValue::UnsignedLong(convert_to_int(
      rt, value, 32, false, attrs,
    )? as u32)),
    NumericType::LongLong => Ok(WebIdlValue::LongLong(
      convert_to_int(rt, value, 64, true, attrs)? as i64,
    )),
    NumericType::UnsignedLongLong => Ok(WebIdlValue::UnsignedLongLong(convert_to_int(
      rt, value, 64, false, attrs,
    )? as u64)),
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
  attrs: crate::IntegerConversionAttrs,
) -> Result<i128, R::Error> {
  let n = rt.to_number(value)?;
  crate::convert_to_int(n, bit_length, signed, attrs)
    .map_err(|e| conversions_shared::numeric_conversion_error_to_js_error(rt, e))
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
          TypeAnnotation::LegacyTreatNonObjectAsNull => {
            out.push_str("[LegacyTreatNonObjectAsNull] ")
          }
          _ => {}
        }
      }
      out.push_str(&format_idl_type(inner));
      out
    }
  }
}
