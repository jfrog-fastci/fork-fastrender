//! WebIDL -> JavaScript conversions (return values).
//!
//! WebIDL defines conversions from IDL types to ECMAScript values, which bindings use when
//! returning results back into JS. This module provides a small, runtime-agnostic implementation
//! that operates on [`webidl_ir::WebIdlValue`] (a host-side representation of IDL values).

use crate::runtime::WebIdlJsRuntime;

use webidl_ir::{DictionaryMemberSchema, IdlType, NamedType, NamedTypeKind, NumericType, TypeContext, WebIdlValue};

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
  match ty {
    IdlType::Annotated { inner, .. } => to_js_with_limits(rt, ctx, inner, value, limits),
    IdlType::Nullable(inner) => match value {
      WebIdlValue::Null => Ok(rt.js_null()),
      _ => to_js_with_limits(rt, ctx, inner, value, limits),
    },
    IdlType::Union(_) => {
      let WebIdlValue::Union { member_ty, value } = value else {
        return Err(rt.throw_type_error("union return value must include a selected member type"));
      };
      to_js_with_limits(rt, ctx, member_ty, value, limits)
    }

    IdlType::Any => to_js_any(rt, ctx, value, limits),
    IdlType::Undefined => match value {
      WebIdlValue::Undefined => Ok(rt.js_undefined()),
      _ => Err(rt.throw_type_error("expected `undefined`")),
    },
    IdlType::Boolean => match value {
      WebIdlValue::Boolean(b) => Ok(rt.js_boolean(*b)),
      _ => Err(rt.throw_type_error("expected a boolean value")),
    },
    IdlType::Numeric(numeric_type) => to_js_numeric(rt, *numeric_type, value),
    IdlType::BigInt => Err(rt.throw_type_error("BigInt return values are not supported yet")),
    IdlType::String(_) => match value {
      WebIdlValue::String(s) => to_js_string(rt, s, limits),
      _ => Err(rt.throw_type_error("expected a string value")),
    },
    IdlType::Object => match value {
      WebIdlValue::PlatformObject(obj) => rt
        .platform_object_to_js_value(obj)
        .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime")),
      _ => Err(rt.throw_type_error(
        "`object` return values require a platform object handle",
      )),
    },
    IdlType::Symbol => Err(rt.throw_type_error("symbol return values are not supported")),
    IdlType::Named(named) => to_js_named(rt, ctx, named, value, limits),

    IdlType::Sequence(elem) | IdlType::FrozenArray(elem) => {
      let WebIdlValue::Sequence { values, .. } = value else {
        return Err(rt.throw_type_error("expected a sequence value"));
      };
      to_js_sequence(rt, ctx, elem, values, limits)
    }

    IdlType::AsyncSequence(_) => Err(rt.throw_type_error("async sequence return values are not supported")),
    IdlType::Record(_, _) => Err(rt.throw_type_error("record return values are not supported yet")),
    IdlType::Promise(_) => Err(rt.throw_type_error("promise return values are not supported yet")),
  }
}

fn to_js_any<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  value: &WebIdlValue,
  limits: ToJsLimits,
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
    WebIdlValue::Sequence { elem_ty, values } => to_js_sequence(rt, ctx, elem_ty, values, limits),
    WebIdlValue::Dictionary { name, members } => {
      // Convert as if the return type was that dictionary.
      to_js_dictionary(rt, ctx, name, members, limits)
    }
    WebIdlValue::Union { member_ty, value } => to_js_with_limits(rt, ctx, member_ty, value, limits),
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

fn to_js_sequence<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  elem_ty: &IdlType,
  values: &[WebIdlValue],
  limits: ToJsLimits,
) -> Result<R::JsValue, R::Error> {
  if values.len() > limits.max_sequence_length {
    return Err(rt.throw_range_error("sequence exceeds maximum length"));
  }

  let array = rt.alloc_array()?;
  for (idx, item) in values.iter().enumerate() {
    let idx_u32: u32 = idx
      .try_into()
      .map_err(|_| rt.throw_range_error("sequence index exceeds u32"))?;
    let js_value = to_js_with_limits(rt, ctx, elem_ty, item, limits)?;
    let key = rt.property_key_from_u32(idx_u32)?;
    rt.define_data_property(array, key, js_value, true)?;
  }
  Ok(array)
}

fn to_js_named<R: WebIdlJsRuntime>(
  rt: &mut R,
  ctx: &TypeContext,
  named: &NamedType,
  value: &WebIdlValue,
  limits: ToJsLimits,
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
      to_js_dictionary(rt, ctx, &named.name, members, limits)
    }
    NamedTypeKind::Typedef => {
      let Some(inner) = ctx.typedefs.get(&named.name) else {
        return Err(rt.throw_type_error("unknown typedef"));
      };
      to_js_with_limits(rt, ctx, inner, value, limits)
    }
    NamedTypeKind::Interface => match value {
      WebIdlValue::PlatformObject(obj) => rt
        .platform_object_to_js_value(obj)
        .ok_or_else(|| rt.throw_type_error("platform object does not belong to this runtime")),
      _ => Err(rt.throw_type_error(
        "interface return values are not supported yet (expected platform object)",
      )),
    },
    NamedTypeKind::CallbackFunction | NamedTypeKind::CallbackInterface => Err(rt.throw_type_error(
      "callback types are not supported as return values yet",
    )),
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
) -> Result<R::JsValue, R::Error> {
  if members.len() > limits.max_dictionary_entries {
    return Err(rt.throw_range_error("dictionary exceeds maximum entry count"));
  }

  let Some(schema) = ctx.flattened_dictionary_members(name) else {
    return Err(rt.throw_type_error("unknown dictionary type"));
  };

  let obj = rt.alloc_object()?;
  for (key, v) in members {
    let member_ty = dictionary_member_type(&schema, key).ok_or_else(|| {
      rt.throw_type_error("dictionary member name does not exist in the schema")
    })?;
    let js_value = to_js_with_limits(rt, ctx, member_ty, v, limits)?;
    let prop_key = rt.property_key_from_str(key)?;
    rt.define_data_property(obj, prop_key, js_value, true)?;
  }

  Ok(obj)
}

fn dictionary_member_type<'a>(schema: &'a [DictionaryMemberSchema], name: &str) -> Option<&'a IdlType> {
  for DictionaryMemberSchema { name: member_name, ty, .. } in schema {
    if member_name == name {
      return Some(ty);
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::runtime::JsRuntime;
  use crate::VmJsRuntime;
  use vm_js::Value;
  use webidl_ir::{parse_idl_type_complete, DictionarySchema};

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
}
