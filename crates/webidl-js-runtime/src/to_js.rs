//! WebIDL -> JavaScript conversions (return values).
//!
//! WebIDL defines conversions from IDL types to ECMAScript values, which bindings use when
//! returning results back into JS. This module provides a small, runtime-agnostic implementation
//! that operates on [`webidl_ir::WebIdlValue`] (a host-side representation of IDL values).

use crate::runtime::WebIdlJsRuntime;

use webidl_ir::{
  eval_default_value, IdlType, NamedType, NamedTypeKind, NumericType, StringType, TypeContext,
  WebIdlException, WebIdlValue,
};

const BYTESTRING_INVALID_CODE_UNITS: &str =
  "ByteString value must only contain code units in range 0..=255";

/// Limits applied while converting WebIDL values back into JavaScript.
///
/// These are defensive bounds: return conversions can allocate JS strings/objects/arrays and should
/// not allow an unbounded amount of work.
#[derive(Debug, Clone, Copy)]
pub struct ToJsLimits {
  /// Maximum number of UTF-8 bytes accepted when allocating a JS string.
  pub max_string_bytes: usize,
  /// Maximum number of elements accepted when allocating an array for `sequence<T>` / `FrozenArray<T>`.
  pub max_sequence_length: usize,
  /// Maximum number of own properties defined when converting dictionaries/records.
  pub max_dictionary_entries: usize,
}

impl Default for ToJsLimits {
  fn default() -> Self {
    Self {
      // 1 MiB matches other defensive limits in the renderer and is large enough for typical web API
      // return values while preventing pathological allocations.
      max_string_bytes: 1024 * 1024,
      // Allow moderately sized arrays (e.g. NodeLists) while still bounding work/allocations.
      max_sequence_length: 64 * 1024,
      // Dictionaries have a fixed schema and are usually small; keep this conservative.
      max_dictionary_entries: 4096,
    }
  }
}

pub fn to_js<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  ty: &IdlType,
  value: &WebIdlValue,
) -> Result<R::JsValue, R::Error> {
  to_js_with_limits(rt, ctx, ty, value, ToJsLimits::default())
}

pub fn to_js_with_limits<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  ty: &IdlType,
  value: &WebIdlValue,
  limits: ToJsLimits,
) -> Result<R::JsValue, R::Error> {
  let mut typedef_stack = Vec::<String>::new();
  to_js_with_limits_inner(rt, ctx, ty, value, limits, &mut typedef_stack)
}

fn to_js_with_limits_inner<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  ty: &IdlType,
  value: &WebIdlValue,
  limits: ToJsLimits,
  typedef_stack: &mut Vec<String>,
) -> Result<R::JsValue, R::Error> {
  match ty {
    IdlType::Annotated { inner, .. } => to_js_with_limits_inner(rt, ctx, inner, value, limits, typedef_stack),
    IdlType::Nullable(inner) => match value {
      WebIdlValue::Null => Ok(rt.js_null()),
      _ => to_js_with_limits_inner(rt, ctx, inner, value, limits, typedef_stack),
    },
    IdlType::Union(_members) => {
      let WebIdlValue::Union { member_ty, value } = value else {
        return Err(rt.throw_type_error("union return value must include a selected member type"));
      };
      let flattened = ty.flattened_union_member_types();
      if !flattened
        .iter()
        .any(|m| union_member_matches(m, member_ty.innermost_type()))
      {
        return Err(rt.throw_type_error("union return value member type is not part of the union"));
      }
      to_js_with_limits_inner(rt, ctx, member_ty, value, limits, typedef_stack)
    }

    IdlType::Any => to_js_any(rt, ctx, value, limits, typedef_stack),
    IdlType::Undefined => match value {
      WebIdlValue::Undefined => Ok(rt.js_undefined()),
      _ => Err(rt.throw_type_error("expected `undefined`")),
    },
    IdlType::Boolean => match value {
      WebIdlValue::Boolean(b) => Ok(rt.js_boolean(*b)),
      _ => Err(rt.throw_type_error("expected a boolean value")),
    },
    IdlType::Numeric(numeric_type) => to_js_numeric(rt, *numeric_type, value),
    IdlType::BigInt => match value {
      WebIdlValue::PlatformObject(obj) => {
        let v = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_bigint(v) {
          return Err(rt.throw_type_error("platform object is not a BigInt"));
        }
        Ok(v)
      }
      _ => Err(rt.throw_type_error("bigint return values require a platform object handle")),
    },
    IdlType::String(string_ty) => match value {
      WebIdlValue::String(s) => to_js_string_type(rt, *string_ty, s, limits),
      _ => Err(rt.throw_type_error("expected a string value")),
    },
    IdlType::Object => match value {
      WebIdlValue::PlatformObject(obj) => {
        let v = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_object(v) {
          return Err(rt.throw_type_error("platform object is not an object"));
        }
        Ok(v)
      }
      _ => {
        // `object` return values must be JavaScript objects, but allow callers to provide any
        // structured WebIDL value that converts to an object (e.g. dictionaries/records/sequences).
        let v = to_js_any(rt, ctx, value, limits, typedef_stack)?;
        if !rt.is_object(v) {
          return Err(rt.throw_type_error("expected an object value"));
        }
        Ok(v)
      }
    },
    IdlType::Symbol => match value {
      WebIdlValue::PlatformObject(obj) => {
        let v = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_symbol(v) {
          return Err(rt.throw_type_error("platform object is not a Symbol"));
        }
        Ok(v)
      }
      _ => Err(rt.throw_type_error("symbol return values require a platform object handle")),
    },
    IdlType::Named(named) => to_js_named(rt, ctx, named, value, limits, typedef_stack),

    IdlType::Sequence(elem) | IdlType::FrozenArray(elem) => {
      let WebIdlValue::Sequence { values, .. } = value else {
        return Err(rt.throw_type_error("expected a sequence value"));
      };
      to_js_sequence(rt, ctx, elem, values, limits, typedef_stack)
    }

    IdlType::AsyncSequence(_) => Err(rt.throw_type_error("async sequence return values are not supported")),
    IdlType::Record(key_ty, value_ty) => {
      let WebIdlValue::Record { entries, .. } = value else {
        return Err(rt.throw_type_error("expected a record value"));
      };
      to_js_record(rt, ctx, key_ty, value_ty, entries, limits, typedef_stack)
    }
    IdlType::Promise(_inner) => match value {
      WebIdlValue::PlatformObject(obj) => {
        let v = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_object(v) {
          return Err(rt.throw_type_error("platform object is not an object"));
        }
        Ok(v)
      }
      _ => Err(rt.throw_type_error(
        "promise return values require a platform object handle",
      )),
    },
  }
}

fn to_js_any<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  value: &WebIdlValue,
  limits: ToJsLimits,
  typedef_stack: &mut Vec<String>,
) -> Result<R::JsValue, R::Error> {
  match value {
    WebIdlValue::Undefined => Ok(rt.js_undefined()),
    WebIdlValue::Null => Ok(rt.js_null()),
    WebIdlValue::Boolean(b) => Ok(rt.js_boolean(*b)),
    WebIdlValue::Byte(_)
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
    | WebIdlValue::UnrestrictedDouble(_) => {
      let Some(n) = numeric_value_to_f64(value) else {
        return Err(rt.throw_type_error("expected numeric WebIDL value"));
      };
      Ok(rt.js_number(n))
    }
    WebIdlValue::String(s) | WebIdlValue::Enum(s) => to_js_string(rt, s, limits),
    WebIdlValue::Sequence { elem_ty, values } => {
      to_js_sequence(rt, ctx, elem_ty, values, limits, typedef_stack)
    }
    WebIdlValue::Record {
      key_ty,
      value_ty,
      entries,
    } => to_js_record(rt, ctx, key_ty, value_ty, entries, limits, typedef_stack),
    WebIdlValue::Dictionary { name, members } => {
      // Convert as if the return type was that dictionary.
      to_js_dictionary(rt, ctx, name, members, limits, typedef_stack)
    }
    WebIdlValue::Union { member_ty, value } => {
      to_js_with_limits_inner(rt, ctx, member_ty, value, limits, typedef_stack)
    }
    WebIdlValue::PlatformObject(obj) => rt
      .platform_object_to_js_value(obj)
      .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime")),
  }
}

fn to_js_numeric<R: WebIdlJsRuntime>(
  rt: &mut R,
  numeric_type: NumericType,
  value: &WebIdlValue,
) -> Result<R::JsValue, R::Error> {
  let n = match (numeric_type, value) {
    (NumericType::Byte, WebIdlValue::Byte(v)) => *v as f64,
    (NumericType::Octet, WebIdlValue::Octet(v)) => *v as f64,
    (NumericType::Short, WebIdlValue::Short(v)) => *v as f64,
    (NumericType::UnsignedShort, WebIdlValue::UnsignedShort(v)) => *v as f64,
    (NumericType::Long, WebIdlValue::Long(v)) => *v as f64,
    (NumericType::UnsignedLong, WebIdlValue::UnsignedLong(v)) => *v as f64,
    (NumericType::LongLong, WebIdlValue::LongLong(v)) => *v as f64,
    (NumericType::UnsignedLongLong, WebIdlValue::UnsignedLongLong(v)) => *v as f64,
    (NumericType::Float, WebIdlValue::Float(v)) => *v as f64,
    (NumericType::UnrestrictedFloat, WebIdlValue::UnrestrictedFloat(v)) => *v as f64,
    (NumericType::Double, WebIdlValue::Double(v)) => *v,
    (NumericType::UnrestrictedDouble, WebIdlValue::UnrestrictedDouble(v)) => *v,
    _ => return Err(rt.throw_type_error("numeric WebIDL value does not match its declared type")),
  };
  Ok(rt.js_number(n))
}

fn numeric_value_to_f64(value: &WebIdlValue) -> Option<f64> {
  Some(match value {
    WebIdlValue::Byte(v) => *v as f64,
    WebIdlValue::Octet(v) => *v as f64,
    WebIdlValue::Short(v) => *v as f64,
    WebIdlValue::UnsignedShort(v) => *v as f64,
    WebIdlValue::Long(v) => *v as f64,
    WebIdlValue::UnsignedLong(v) => *v as f64,
    WebIdlValue::LongLong(v) => *v as f64,
    WebIdlValue::UnsignedLongLong(v) => *v as f64,
    WebIdlValue::Float(v) => *v as f64,
    WebIdlValue::UnrestrictedFloat(v) => *v as f64,
    WebIdlValue::Double(v) | WebIdlValue::UnrestrictedDouble(v) => *v,
    _ => return None,
  })
}

fn union_member_matches(union_member: &IdlType, selected: &IdlType) -> bool {
  let union_member = union_member.innermost_type();
  let selected = selected.innermost_type();
  match (union_member, selected) {
    (IdlType::Any, IdlType::Any)
    | (IdlType::Undefined, IdlType::Undefined)
    | (IdlType::Boolean, IdlType::Boolean)
    | (IdlType::BigInt, IdlType::BigInt)
    | (IdlType::Object, IdlType::Object)
    | (IdlType::Symbol, IdlType::Symbol) => true,
    (IdlType::Numeric(a), IdlType::Numeric(b)) => a == b,
    (IdlType::String(a), IdlType::String(b)) => a == b,
    (IdlType::Named(a), IdlType::Named(b)) => a.name == b.name,
    (IdlType::Sequence(a), IdlType::Sequence(b))
    | (IdlType::FrozenArray(a), IdlType::FrozenArray(b))
    | (IdlType::AsyncSequence(a), IdlType::AsyncSequence(b))
    | (IdlType::Promise(a), IdlType::Promise(b)) => union_member_matches(a, b),
    (IdlType::Record(ak, av), IdlType::Record(bk, bv)) => {
      union_member_matches(ak, bk) && union_member_matches(av, bv)
    }
    _ => false,
  }
}

fn to_js_string<R: WebIdlJsRuntime>(
  rt: &mut R,
  s: &str,
  limits: ToJsLimits,
) -> Result<R::JsValue, R::Error> {
  if s.len() > limits.max_string_bytes {
    return Err(rt.throw_range_error("string exceeds maximum length"));
  }
  rt.alloc_string(s)
}

fn to_js_string_type<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: StringType,
  s: &str,
  limits: ToJsLimits,
) -> Result<R::JsValue, R::Error> {
  if ty == StringType::ByteString && s.chars().any(|c| (c as u32) > 0xFF) {
    return Err(rt.throw_type_error(BYTESTRING_INVALID_CODE_UNITS));
  }
  to_js_string(rt, s, limits)
}

fn to_js_sequence<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  elem_ty: &IdlType,
  values: &[WebIdlValue],
  limits: ToJsLimits,
  typedef_stack: &mut Vec<String>,
) -> Result<R::JsValue, R::Error> {
  if values.len() > limits.max_sequence_length {
    return Err(rt.throw_range_error("sequence exceeds maximum length"));
  }

  let array = rt.alloc_array()?;
  for (idx, item) in values.iter().enumerate() {
    let idx_u32: u32 = idx
      .try_into()
      .map_err(|_| rt.throw_range_error("sequence index exceeds u32"))?;
    let js_value = to_js_with_limits_inner(rt, ctx, elem_ty, item, limits, typedef_stack)?;
    let key = rt.property_key_from_u32(idx_u32)?;
    rt.define_data_property(array, key, js_value, true)?;
  }

  // Arrays expose a non-enumerable `length` data property.
  //
  // Some runtimes may choose to return an Array exotic object from `alloc_array` (preferred), in
  // which case `length` already exists with the correct attributes. If `alloc_array` returns an
  // ordinary object, define `length` manually.
  let length_key = rt.property_key_from_str("length")?;
  if rt.get_own_property(array, length_key)?.is_none() {
    rt.define_data_property(array, length_key, rt.js_number(values.len() as f64), false)?;
  }
  Ok(array)
}

fn to_js_named<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  named: &NamedType,
  value: &WebIdlValue,
  limits: ToJsLimits,
  typedef_stack: &mut Vec<String>,
) -> Result<R::JsValue, R::Error> {
  let kind = match &named.kind {
    NamedTypeKind::Unresolved => resolve_named_kind(ctx, &named.name).ok_or_else(|| {
      rt.throw_type_error("named type could not be resolved (missing enum/dictionary/typedef schema)")
    })?,
    other => other.clone(),
  };

  match kind {
    NamedTypeKind::Enum => {
      let WebIdlValue::Enum(s) = value else {
        return Err(rt.throw_type_error("expected an enum value"));
      };
      let Some(values) = ctx.enums.get(&named.name) else {
        return Err(rt.throw_type_error("unknown enum type"));
      };
      if !values.contains(s) {
        return Err(rt.throw_type_error("enum value is not a member of the enum"));
      }
      to_js_string(rt, s, limits)
    }
    NamedTypeKind::Dictionary => {
      let WebIdlValue::Dictionary { name, members } = value else {
        return Err(rt.throw_type_error("expected a dictionary value"));
      };
      if name != &named.name {
        return Err(rt.throw_type_error("dictionary value name does not match declared type"));
      }
      to_js_dictionary(rt, ctx, &named.name, members, limits, typedef_stack)
    }
    NamedTypeKind::Typedef => {
      let Some(inner) = ctx.typedefs.get(&named.name) else {
        return Err(rt.throw_type_error("unknown typedef"));
      };
      if typedef_stack.contains(&named.name) {
        let message = format!(
          "typedef cycle detected: {} -> {}",
          typedef_stack.join(" -> "),
          named.name
        );
        return Err(rt.throw_type_error(&message));
      }
      typedef_stack.push(named.name.clone());
      let out = to_js_with_limits_inner(rt, ctx, inner, value, limits, typedef_stack);
      typedef_stack.pop();
      out
    }
    NamedTypeKind::Interface => match value {
      WebIdlValue::PlatformObject(obj) => {
        let js_value = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_object(js_value) {
          return Err(rt.throw_type_error("platform object is not an object"));
        }
        if !rt.implements_interface(js_value, &named.name) {
          return Err(rt.throw_type_error("platform object does not implement the interface"));
        }
        Ok(js_value)
      }
      _ => Err(rt.throw_type_error(
        "interface return values are not supported yet (expected platform object)",
      )),
    },
    NamedTypeKind::CallbackFunction => match value {
      WebIdlValue::PlatformObject(obj) => {
        let js_value = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_callable(js_value) {
          return Err(rt.throw_type_error("callback function return value is not callable"));
        }
        Ok(js_value)
      }
      _ => Err(rt.throw_type_error(
        "callback function return values require a platform object handle",
      )),
    },
    NamedTypeKind::CallbackInterface => match value {
      WebIdlValue::PlatformObject(obj) => {
        let js_value = rt
          .platform_object_to_js_value(obj)
          .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime"))?;
        if !rt.is_object(js_value) {
          return Err(rt.throw_type_error("callback interface return value is not an object"));
        }
        Ok(js_value)
      }
      _ => Err(rt.throw_type_error(
        "callback interface return values require a platform object handle",
      )),
    },
    NamedTypeKind::Unresolved => Err(rt.throw_type_error(
      "named type kind is unresolved (missing enum/dictionary/typedef schema)",
    )),
  }
}

fn resolve_named_kind(ctx: &TypeContext, name: &str) -> Option<NamedTypeKind> {
  if ctx.typedefs.contains_key(name) {
    return Some(NamedTypeKind::Typedef);
  }
  if ctx.enums.contains_key(name) {
    return Some(NamedTypeKind::Enum);
  }
  if ctx.dictionaries.contains_key(name) {
    return Some(NamedTypeKind::Dictionary);
  }
  None
}

fn to_js_dictionary<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  name: &str,
  members: &std::collections::BTreeMap<String, WebIdlValue>,
  limits: ToJsLimits,
  typedef_stack: &mut Vec<String>,
) -> Result<R::JsValue, R::Error> {
  if members.len() > limits.max_dictionary_entries {
    return Err(rt.throw_range_error("dictionary exceeds maximum entry count"));
  }

  let Some(schema) = ctx.flattened_dictionary_members(name) else {
    return Err(rt.throw_type_error("unknown dictionary type"));
  };
  if schema.len() > limits.max_dictionary_entries {
    return Err(rt.throw_range_error("dictionary schema exceeds maximum entry count"));
  }

  // Validate provided member names before allocating the output object.
  // This matches WebIDL's dictionary model: only schema-declared members are allowed.
  let mut schema_names = std::collections::HashSet::<&str>::new();
  for member in &schema {
    if member.name.len() > limits.max_string_bytes {
      return Err(rt.throw_range_error("dictionary key exceeds maximum length"));
    }
    schema_names.insert(member.name.as_str());
    if member.required && !members.contains_key(&member.name) {
      let message = format!("dictionary `{name}` missing required member `{}`", member.name);
      return Err(rt.throw_type_error(&message));
    }
  }
  for key in members.keys() {
    if key.len() > limits.max_string_bytes {
      return Err(rt.throw_range_error("dictionary key exceeds maximum length"));
    }
    if !schema_names.contains(key.as_str()) {
      return Err(rt.throw_type_error("dictionary member name does not exist in the schema"));
    }
  }

  let obj = rt.alloc_object()?;
  // Define properties in WebIDL dictionary serialization order: inherited dictionaries from least
  // to most derived, and members in lexicographical order.
  for member in &schema {
    let v = if let Some(v) = members.get(&member.name) {
      Some(std::borrow::Cow::Borrowed(v))
    } else if let Some(default) = &member.default {
      let v = eval_default_value(&member.ty, default, ctx)
        .map_err(|e| throw_webidl_exception(rt, e))?;
      Some(std::borrow::Cow::Owned(v))
    } else {
      None
    };
    let Some(v) = v else {
      continue;
    };
    let js_value = to_js_with_limits_inner(rt, ctx, &member.ty, &v, limits, typedef_stack)?;
    let prop_key = rt.property_key_from_str(&member.name)?;
    rt.define_data_property(obj, prop_key, js_value, true)?;
  }

  Ok(obj)
}

fn throw_webidl_exception<R: WebIdlJsRuntime>(rt: &mut R, err: WebIdlException) -> R::Error {
  match err {
    WebIdlException::TypeError { message } => rt.throw_type_error(&message),
    WebIdlException::RangeError { message } => rt.throw_range_error(&message),
  }
}

fn to_js_record<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  key_ty: &IdlType,
  value_ty: &IdlType,
  entries: &[(String, WebIdlValue)],
  limits: ToJsLimits,
  typedef_stack: &mut Vec<String>,
) -> Result<R::JsValue, R::Error> {
  if entries.len() > limits.max_dictionary_entries {
    return Err(rt.throw_range_error("record exceeds maximum entry count"));
  }

  let string_type = record_key_string_type(ctx, key_ty).ok_or_else(|| {
    rt.throw_type_error("record key type is not supported (expected a string type)")
  })?;

  for (key, _) in entries.iter() {
    if key.len() > limits.max_string_bytes {
      return Err(rt.throw_range_error("record key exceeds maximum length"));
    }
    if string_type == StringType::ByteString && key.chars().any(|c| (c as u32) > 0xFF) {
      return Err(rt.throw_type_error(BYTESTRING_INVALID_CODE_UNITS));
    }
  }

  let obj = rt.alloc_object()?;
  for (key, v) in entries.iter() {
    let js_value = to_js_with_limits_inner(rt, ctx, value_ty, v, limits, typedef_stack)?;
    let prop_key = rt.property_key_from_str(key)?;
    rt.define_data_property(obj, prop_key, js_value, true)?;
  }
  Ok(obj)
}

fn record_key_string_type(ctx: &TypeContext, ty: &IdlType) -> Option<StringType> {
  record_key_string_type_inner(ctx, ty, &mut std::collections::BTreeSet::<String>::new())
}

fn record_key_string_type_inner(
  ctx: &TypeContext,
  ty: &IdlType,
  visited_typedefs: &mut std::collections::BTreeSet<String>,
) -> Option<StringType> {
  match ty.innermost_type() {
    IdlType::String(s) => Some(*s),
    IdlType::Named(NamedType { name, kind }) => {
      let resolved = match kind {
        NamedTypeKind::Unresolved => resolve_named_kind(ctx, name)?,
        other => other.clone(),
      };
      match resolved {
        NamedTypeKind::Typedef => {
          if !visited_typedefs.insert(name.clone()) {
            return None;
          }
          let inner = ctx.typedefs.get(name)?;
          record_key_string_type_inner(ctx, inner, visited_typedefs)
        }
        _ => None,
      }
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::JsRuntime;
  use crate::VmJsRuntime;
  use std::collections::BTreeMap;
  use vm_js::Value;
  use webidl_ir::{
    parse_default_value, parse_idl_type_complete, DictionaryMemberSchema, DictionarySchema, NamedType,
    NamedTypeKind, PlatformObject,
  };

  #[test]
  fn primitives_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let boolean_ty = parse_idl_type_complete("boolean")?;
    let out = to_js(&mut rt, &ctx, &boolean_ty, &WebIdlValue::Boolean(true))?;
    assert!(rt.is_boolean(out));
    assert_eq!(rt.to_boolean(out)?, true);

    let long_ty = parse_idl_type_complete("long")?;
    let out = to_js(&mut rt, &ctx, &long_ty, &WebIdlValue::Long(42))?;
    assert!(rt.is_number(out));
    assert_eq!(rt.to_number(out)?, 42.0);

    let string_ty = parse_idl_type_complete("DOMString")?;
    let out = to_js(
      &mut rt,
      &ctx,
      &string_ty,
      &WebIdlValue::String("hello".to_string()),
    )?;
    let Value::String(handle) = out else {
      return Err("expected JS string".into());
    };
    assert_eq!(rt.heap().get_string(handle)?.to_utf8_lossy(), "hello");

    Ok(())
  }

  #[test]
  fn sequence_to_array_defines_indexed_enumerable_properties() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();
    let ty = parse_idl_type_complete("sequence<long>")?;
    let elem_ty = parse_idl_type_complete("long")?;
    let value = WebIdlValue::Sequence {
      elem_ty: Box::new(elem_ty),
      values: vec![WebIdlValue::Long(1), WebIdlValue::Long(2)],
    };

    let array = to_js(&mut rt, &ctx, &ty, &value)?;
    assert!(rt.is_object(array));

    for (idx, expected) in [(0u32, 1.0), (1u32, 2.0)] {
      let key = rt.property_key_from_u32(idx)?;
      let got = rt.get(array, key)?;
      assert_eq!(rt.to_number(got)?, expected);

      let desc = rt
        .get_own_property(array, key)?
        .ok_or_else(|| "missing indexed property")?;
      assert!(desc.enumerable);
    }

    let length_key = rt.property_key_from_str("length")?;
    let length_value = rt.get(array, length_key)?;
    assert_eq!(rt.to_number(length_value)?, 2.0);
    let desc = rt
      .get_own_property(array, length_key)?
      .ok_or_else(|| "missing length property")?;
    assert!(!desc.enumerable);

    Ok(())
  }

  #[test]
  fn dictionary_to_object_defines_enumerable_own_properties() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = TypeContext::default();
    ctx.add_dictionary(DictionarySchema {
      name: "Options".to_string(),
      inherits: None,
      members: vec![
        DictionaryMemberSchema {
          name: "count".to_string(),
          required: false,
          ty: parse_idl_type_complete("long")?,
          default: None,
        },
        DictionaryMemberSchema {
          name: "label".to_string(),
          required: false,
          ty: parse_idl_type_complete("DOMString")?,
          default: None,
        },
      ],
    });

    let ty = parse_idl_type_complete("Options")?;
    let mut members = std::collections::BTreeMap::new();
    members.insert("count".to_string(), WebIdlValue::Long(3));
    members.insert("label".to_string(), WebIdlValue::String("ok".to_string()));
    let value = WebIdlValue::Dictionary {
      name: "Options".to_string(),
      members,
    };

    let mut rt = VmJsRuntime::new();
    let obj = to_js(&mut rt, &ctx, &ty, &value)?;
    assert!(rt.is_object(obj));

    for (name, expected) in [("count", Value::Number(3.0)), ("label", Value::Undefined)] {
      let key = rt.property_key_from_str(name)?;
      let got = rt.get(obj, key)?;
      if name == "count" {
        assert_eq!(got, expected);
      } else {
        assert!(matches!(got, Value::String(_)));
      }
      let desc = rt
        .get_own_property(obj, key)?
        .ok_or_else(|| "missing dictionary property")?;
      assert!(desc.enumerable);
    }

    let label_key = rt.property_key_from_str("label")?;
    let label_value = rt.get(obj, label_key)?;
    let Value::String(handle) = label_value else {
      return Err("expected label to be a JS string".into());
    };
    assert_eq!(rt.heap().get_string(handle)?.to_utf8_lossy(), "ok");

    Ok(())
  }

  #[test]
  fn dictionary_to_object_inserts_members_in_lexicographic_order(
  ) -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = TypeContext::default();
    // Declare members out of order; WebIDL dictionary algorithms require lexicographical ordering
    // when converting to/from JS.
    ctx.add_dictionary(DictionarySchema {
      name: "Order".to_string(),
      inherits: None,
      members: vec![
        DictionaryMemberSchema {
          name: "b".to_string(),
          required: false,
          ty: parse_idl_type_complete("long")?,
          default: None,
        },
        DictionaryMemberSchema {
          name: "a".to_string(),
          required: false,
          ty: parse_idl_type_complete("long")?,
          default: None,
        },
      ],
    });

    let ty = parse_idl_type_complete("Order")?;
    let value = WebIdlValue::Dictionary {
      name: "Order".to_string(),
      members: BTreeMap::from([
        ("a".to_string(), WebIdlValue::Long(1)),
        ("b".to_string(), WebIdlValue::Long(2)),
      ]),
    };

    let mut rt = VmJsRuntime::new();
    let obj = to_js(&mut rt, &ctx, &ty, &value)?;

    let keys = rt.own_property_keys(obj)?;
    let mut out: Vec<String> = Vec::new();
    for key in keys {
      let s = rt.property_key_to_js_string(key)?;
      let Value::String(handle) = s else {
        return Err("expected string key".into());
      };
      out.push(rt.heap().get_string(handle)?.to_utf8_lossy());
    }
    assert_eq!(out, vec!["a", "b"]);
    Ok(())
  }

  #[test]
  fn dictionary_applies_member_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = TypeContext::default();
    ctx.add_dictionary(DictionarySchema {
      name: "Opts".to_string(),
      inherits: None,
      members: vec![DictionaryMemberSchema {
        name: "flag".to_string(),
        required: false,
        ty: parse_idl_type_complete("boolean")?,
        default: Some(parse_default_value("true")?),
      }],
    });

    let ty = parse_idl_type_complete("Opts")?;
    let value = WebIdlValue::Dictionary {
      name: "Opts".to_string(),
      members: std::collections::BTreeMap::new(),
    };

    let mut rt = VmJsRuntime::new();
    let obj = to_js(&mut rt, &ctx, &ty, &value)?;

    let key = rt.property_key_from_str("flag")?;
    let got = rt.get(obj, key)?;
    assert_eq!(rt.to_boolean(got)?, true);
    let desc = rt
      .get_own_property(obj, key)?
      .ok_or_else(|| "missing defaulted dictionary property")?;
    assert!(desc.enumerable);
    Ok(())
  }

  #[test]
  fn dictionary_key_length_limit_throws_range_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = TypeContext::default();
    ctx.add_dictionary(DictionarySchema {
      name: "Options".to_string(),
      inherits: None,
      members: vec![DictionaryMemberSchema {
        name: "longkey".to_string(),
        required: false,
        ty: parse_idl_type_complete("long")?,
        default: None,
      }],
    });
    let ty = parse_idl_type_complete("Options")?;

    let mut members = std::collections::BTreeMap::new();
    members.insert("longkey".to_string(), WebIdlValue::Long(1));
    let value = WebIdlValue::Dictionary {
      name: "Options".to_string(),
      members,
    };

    let mut rt = VmJsRuntime::new();
    let limits = ToJsLimits {
      max_string_bytes: 1,
      ..ToJsLimits::default()
    };
    let err = to_js_with_limits(&mut rt, &ctx, &ty, &value, limits).unwrap_err();

    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(
      msg.starts_with("RangeError:"),
      "expected RangeError, got {msg:?}"
    );

    Ok(())
  }

  #[test]
  fn dictionary_missing_required_member_throws_type_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = TypeContext::default();
    ctx.add_dictionary(DictionarySchema {
      name: "RequiredDict".to_string(),
      inherits: None,
      members: vec![DictionaryMemberSchema {
        name: "must".to_string(),
        required: true,
        ty: parse_idl_type_complete("long")?,
        default: None,
      }],
    });

    let ty = parse_idl_type_complete("RequiredDict")?;
    let value = WebIdlValue::Dictionary {
      name: "RequiredDict".to_string(),
      members: std::collections::BTreeMap::new(),
    };

    let mut rt = VmJsRuntime::new();
    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();
    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
    assert!(
      msg.contains("missing required member"),
      "expected required-member message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn record_to_object_defines_enumerable_own_properties() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("record<DOMString, long>")?;
    let key_ty = parse_idl_type_complete("DOMString")?;
    let value_ty = parse_idl_type_complete("long")?;

    let entries = vec![
      ("a".to_string(), WebIdlValue::Long(1)),
      ("b".to_string(), WebIdlValue::Long(2)),
    ];
    let value = WebIdlValue::Record {
      key_ty: Box::new(key_ty),
      value_ty: Box::new(value_ty),
      entries,
    };

    let obj = to_js(&mut rt, &ctx, &ty, &value)?;
    assert!(rt.is_object(obj));

    for (name, expected) in [("a", 1.0), ("b", 2.0)] {
      let key = rt.property_key_from_str(name)?;
      let got = rt.get(obj, key)?;
      assert_eq!(rt.to_number(got)?, expected);
      let desc = rt
        .get_own_property(obj, key)?
        .ok_or_else(|| "missing record property")?;
      assert!(desc.enumerable);
    }

    Ok(())
  }

  #[test]
  fn interface_return_requires_branded_platform_object() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let node_ty = IdlType::Named(NamedType {
      name: "Node".to_string(),
      kind: NamedTypeKind::Interface,
    });

    let obj = rt.alloc_platform_object_value("Node", &["EventTarget"], 123)?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(obj));
    let out = to_js(&mut rt, &ctx, &node_ty, &value)?;
    assert_eq!(out, obj);

    let other_ty = IdlType::Named(NamedType {
      name: "Document".to_string(),
      kind: NamedTypeKind::Interface,
    });
    let err = to_js(&mut rt, &ctx, &other_ty, &value).unwrap_err();
    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );

    Ok(())
  }

  #[test]
  fn callback_function_return_allows_callable_platform_object() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = IdlType::Named(NamedType {
      name: "Callback".to_string(),
      kind: NamedTypeKind::CallbackFunction,
    });

    let func = rt.alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(func));
    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert_eq!(out, func);
    Ok(())
  }

  #[test]
  fn callback_function_return_rejects_non_callable_platform_object(
  ) -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = IdlType::Named(NamedType {
      name: "Callback".to_string(),
      kind: NamedTypeKind::CallbackFunction,
    });

    let obj = rt.alloc_object_value()?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(obj));
    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();

    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
    assert!(
      msg.contains("not callable"),
      "expected not-callable message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn callback_interface_return_allows_object_platform_object() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = IdlType::Named(NamedType {
      name: "Listener".to_string(),
      kind: NamedTypeKind::CallbackInterface,
    });

    let obj = rt.alloc_object_value()?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(obj));
    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert_eq!(out, obj);
    Ok(())
  }

  #[test]
  fn callback_interface_return_rejects_non_object_platform_object(
  ) -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = IdlType::Named(NamedType {
      name: "Listener".to_string(),
      kind: NamedTypeKind::CallbackInterface,
    });

    let value = WebIdlValue::PlatformObject(PlatformObject::new(Value::Number(1.0)));
    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();

    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
    assert!(
      msg.contains("not an object"),
      "expected not-an-object message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn union_return_rejects_member_not_in_union() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("(long or DOMString)")?;
    let member_ty = parse_idl_type_complete("boolean")?;
    let value = WebIdlValue::Union {
      member_ty: Box::new(member_ty),
      value: Box::new(WebIdlValue::Boolean(true)),
    };

    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();
    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
    assert!(
      msg.contains("member type is not part of the union"),
      "expected union member error message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn union_member_matches_named_kind_inside_sequence() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("(sequence<Foo> or long)")?;
    let member_ty = IdlType::Sequence(Box::new(IdlType::Named(NamedType {
      name: "Foo".to_string(),
      kind: NamedTypeKind::Dictionary,
    })));
    let value = WebIdlValue::Union {
      member_ty: Box::new(member_ty),
      value: Box::new(WebIdlValue::Sequence {
        elem_ty: Box::new(IdlType::Named(NamedType {
          name: "Foo".to_string(),
          kind: NamedTypeKind::Dictionary,
        })),
        values: Vec::new(),
      }),
    };

    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert!(rt.is_object(out));
    Ok(())
  }

  #[test]
  fn typedef_cycles_throw_type_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = TypeContext::default();
    // `A` is a typedef to itself; this should not recurse forever.
    ctx.add_typedef("A", parse_idl_type_complete("A")?);

    let ty = parse_idl_type_complete("A")?;
    let value = WebIdlValue::Long(1);
    let mut rt = VmJsRuntime::new();
    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();
    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
    assert!(
      msg.contains("typedef cycle detected"),
      "expected typedef cycle message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn object_return_requires_object_platform_value() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("object")?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(Value::Number(1.0)));
    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();

    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
    assert!(
      msg.contains("platform object is not an object"),
      "expected objectness error message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn object_return_accepts_dictionary_value() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let mut ctx = TypeContext::default();
    ctx.add_dictionary(DictionarySchema {
      name: "Foo".to_string(),
      inherits: None,
      members: vec![DictionaryMemberSchema {
        name: "x".to_string(),
        required: false,
        ty: parse_idl_type_complete("long")?,
        default: None,
      }],
    });

    let ty = parse_idl_type_complete("object")?;
    let value = WebIdlValue::Dictionary {
      name: "Foo".to_string(),
      members: BTreeMap::from([("x".to_string(), WebIdlValue::Long(1))]),
    };

    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert!(rt.is_object(out));
    let key = rt.property_key_from_str("x")?;
    let got = rt.get(out, key)?;
    assert_eq!(rt.to_number(got)?, 1.0);
    Ok(())
  }

  #[test]
  fn object_return_rejects_primitive_values() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("object")?;
    let err = to_js(&mut rt, &ctx, &ty, &WebIdlValue::Long(1)).unwrap_err();

    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
    assert!(
      msg.contains("expected an object"),
      "expected objectness error message, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn any_return_allows_platform_non_object_values() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("any")?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(Value::Number(42.0)));
    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert_eq!(out, Value::Number(42.0));
    Ok(())
  }

  #[test]
  fn symbol_return_allows_platform_symbol_values() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("symbol")?;
    let sym = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_symbol(Some("s"))?
    };
    let value = WebIdlValue::PlatformObject(PlatformObject::new(Value::Symbol(sym)));
    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert!(rt.is_symbol(out));
    Ok(())
  }

  #[test]
  fn promise_return_allows_platform_object() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("Promise<DOMString>")?;
    let promise_obj = rt.alloc_object_value()?;
    let value = WebIdlValue::PlatformObject(PlatformObject::new(promise_obj));
    let out = to_js(&mut rt, &ctx, &ty, &value)?;
    assert_eq!(out, promise_obj);
    Ok(())
  }

  #[test]
  fn bytestring_return_validates_code_units() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("ByteString")?;
    let ok = WebIdlValue::String("abc".to_string());
    let out = to_js(&mut rt, &ctx, &ty, &ok)?;
    let Value::String(handle) = out else {
      return Err("expected JS string".into());
    };
    assert_eq!(rt.heap().get_string(handle)?.to_utf8_lossy(), "abc");

    let bad = WebIdlValue::String("\u{0100}".to_string());
    let err = to_js(&mut rt, &ctx, &ty, &bad).unwrap_err();
    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(
      msg.contains(BYTESTRING_INVALID_CODE_UNITS),
      "expected ByteString error message, got {msg:?}"
    );

    Ok(())
  }

  #[test]
  fn record_bytestring_keys_validate_code_points() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = VmJsRuntime::new();
    let ctx = TypeContext::default();

    let ty = parse_idl_type_complete("record<ByteString, long>")?;
    let key_ty = parse_idl_type_complete("ByteString")?;
    let value_ty = parse_idl_type_complete("long")?;

    let entries = vec![("\u{0100}".to_string(), WebIdlValue::Long(1))];
    let value = WebIdlValue::Record {
      key_ty: Box::new(key_ty),
      value_ty: Box::new(value_ty),
      entries,
    };

    let err = to_js(&mut rt, &ctx, &ty, &value).unwrap_err();
    let vm_js::VmError::Throw(thrown) = err else {
      return Err(format!("expected VmError::Throw, got {err:?}").into());
    };
    let s = rt.to_string(thrown)?;
    let Value::String(handle) = s else {
      return Err("expected thrown error to stringify to a JS string".into());
    };
    let msg = rt.heap().get_string(handle)?.to_utf8_lossy();
    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
    Ok(())
  }
}
