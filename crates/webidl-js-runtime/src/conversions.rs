//! WebIDL <-> JavaScript value conversion algorithms.
//!
//! WebIDL conversion algorithms often need to distinguish between `TypeError` and `RangeError`
//! failures. The pure conversion logic in this module returns [`webidl_ir::WebIdlException`], which
//! is then mapped to the embedded engine's throw type via [`WebIdlJsRuntime`].
//!
//! This module implements a subset of the WHATWG WebIDL "type mapping" algorithms used by generated
//! bindings. It intentionally focuses on spec-shaped conversions and deterministic error messages
//! rather than being maximally permissive.
//!
//! This module also provides callback type conversions (callback functions and callback
//! interfaces). These conversions return the underlying engine value so bindings can store and
//! invoke callbacks later.

use crate::{JsRuntime, WebIdlJsRuntime};
use std::collections::BTreeMap;
use webidl_ir::{
  eval_default_value, DefaultValue, DictionaryMemberSchema, IdlType, NamedType, NamedTypeKind,
  NumericType, PlatformObject, StringType, TypeAnnotation, TypeContext, WebIdlException, WebIdlValue,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ConvertedValue<V> {
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

  String(String),
  Enum(String),

  Any(V),
  Object(V),
  PlatformObject(PlatformObject),

  Sequence {
    elem_ty: Box<IdlType>,
    values: Vec<ConvertedValue<V>>,
  },

  Record {
    key_ty: Box<IdlType>,
    value_ty: Box<IdlType>,
    entries: BTreeMap<String, ConvertedValue<V>>,
  },

  Dictionary {
    name: String,
    members: BTreeMap<String, ConvertedValue<V>>,
  },

  Union {
    member_ty: Box<IdlType>,
    value: Box<ConvertedValue<V>>,
  },
}

#[derive(Debug, Clone)]
pub struct ArgumentSchema {
  pub name: &'static str,
  pub ty: IdlType,
  pub optional: bool,
  pub default: Option<DefaultValue>,
}

pub fn convert_arguments<R: WebIdlJsRuntime>(
  rt: &mut R,
  args: &[R::JsValue],
  params: &[ArgumentSchema],
  ctx: &TypeContext,
) -> Result<Vec<ConvertedValue<R::JsValue>>, R::Error> {
  let required_len = params.iter().take_while(|p| !p.optional).count();
  if args.len() < required_len {
    return Err(rt.throw_type_error("Not enough arguments"));
  }

  let mut out = Vec::with_capacity(params.len());
  for (idx, param) in params.iter().enumerate() {
    let v = args.get(idx).copied().unwrap_or_else(|| rt.js_undefined());
    if rt.is_undefined(v) {
      if let Some(default) = &param.default {
        let evaluated =
          eval_default_value(&param.ty, default, ctx).map_err(|e| throw_webidl_exception(rt, e))?;
        out.push(converted_from_webidl_value::<R::JsValue>(evaluated));
        continue;
      }
    }
    out.push(convert_to_idl(rt, v, &param.ty, ctx)?);
  }
  Ok(out)
}

pub fn convert_to_idl<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  ty: &IdlType,
  ctx: &TypeContext,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  let mut typedef_stack = Vec::<String>::new();
  convert_to_idl_inner(rt, v, ty, ctx, &mut typedef_stack, ConversionState::default())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IntegerConversionAttrs {
  pub clamp: bool,
  pub enforce_range: bool,
}

impl IntegerConversionAttrs {
  pub fn is_empty(self) -> bool {
    !self.clamp && !self.enforce_range
  }
}

fn throw_webidl_exception<R: WebIdlJsRuntime>(rt: &mut R, err: WebIdlException) -> R::Error {
  match err {
    WebIdlException::TypeError { message } => rt.throw_type_error(&message),
    WebIdlException::RangeError { message } => rt.throw_range_error(&message),
  }
}

/// Convert an ECMAScript value to an IDL `byte`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-byte>
pub fn to_byte<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i8, R::Error> {
  let n = rt.to_number(value)?;
  let v = convert_to_int(n, 8, true, attrs).map_err(|e| throw_webidl_exception(rt, e))?;
  Ok(v as i8)
}

#[derive(Debug, Clone, Copy, Default)]
struct ConversionState {
  int_attrs: IntegerConversionAttrs,
  legacy_null_to_empty_string: bool,
  legacy_treat_non_object_as_null: bool,
}

fn convert_to_idl_inner<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  ty: &IdlType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
  state: ConversionState,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  match ty {
    IdlType::Annotated { annotations, inner } => {
      let mut out_state = state;
      for a in annotations {
        match a {
          TypeAnnotation::Clamp => out_state.int_attrs.clamp = true,
          TypeAnnotation::EnforceRange => out_state.int_attrs.enforce_range = true,
          TypeAnnotation::LegacyNullToEmptyString => out_state.legacy_null_to_empty_string = true,
          TypeAnnotation::LegacyTreatNonObjectAsNull => {
            out_state.legacy_treat_non_object_as_null = true;
          }
          _ => {}
        }
      }
      if out_state.int_attrs.clamp && out_state.int_attrs.enforce_range {
        return Err(rt.throw_type_error(
          "[Clamp] and [EnforceRange] cannot both apply to the same type",
        ));
      }
      convert_to_idl_inner(rt, v, inner, ctx, typedef_stack, out_state)
    }

    // <https://webidl.spec.whatwg.org/#es-nullable-type>
    IdlType::Nullable(inner) => {
      let inner_includes_undefined = includes_undefined(rt, inner, ctx, typedef_stack)?;
      if rt.is_undefined(v) && inner_includes_undefined {
        return Ok(ConvertedValue::Undefined);
      }
      if rt.is_null(v) || rt.is_undefined(v) {
        return Ok(ConvertedValue::Null);
      }
      convert_to_idl_inner(rt, v, inner, ctx, typedef_stack, state)
    }

    IdlType::Union(members) => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to a union type",
        ));
      }
      convert_to_union(rt, v, members, ctx, typedef_stack)
    }

    IdlType::Any => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `any`",
        ));
      }
      Ok(ConvertedValue::Any(v))
    }

    IdlType::Undefined => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `undefined`",
        ));
      }
      if rt.is_undefined(v) {
        Ok(ConvertedValue::Undefined)
      } else {
        Err(rt.throw_type_error("Value is not undefined"))
      }
    }

    IdlType::Boolean => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `boolean`",
        ));
      }
      Ok(ConvertedValue::Boolean(rt.to_boolean(v)?))
    }

    IdlType::Numeric(numeric_type) => convert_to_numeric(rt, v, *numeric_type, state.int_attrs),

    IdlType::String(string_type) => convert_to_string(rt, v, *string_type, state),

    IdlType::Object => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `object`",
        ));
      }
      if !rt.is_object(v) {
        return Err(rt.throw_type_error("Value is not an object"));
      }
      Ok(ConvertedValue::Object(v))
    }

    IdlType::Symbol => Err(rt.throw_type_error("`symbol` conversions are not supported yet")),

    IdlType::BigInt => Err(rt.throw_type_error("`bigint` conversions are not supported yet")),

    IdlType::Named(named) => convert_to_named_type(rt, v, named, ctx, typedef_stack, state),

    IdlType::Sequence(elem_ty) => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `sequence`",
        ));
      }
      convert_to_sequence(rt, v, elem_ty, ctx, typedef_stack)
    }

    IdlType::Record(key_ty, value_ty) => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `record`",
        ));
      }
      convert_to_record(rt, v, key_ty, value_ty, ctx, typedef_stack)
    }

    // Non-MVP types.
    IdlType::FrozenArray(_)
    | IdlType::AsyncSequence(_)
    | IdlType::Promise(_) => Err(rt.throw_type_error("WebIDL type conversion is not supported yet")),
  }
}

fn convert_to_named_type<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  named: &NamedType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
  state: ConversionState,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  let name = named.name.as_str();
  if let Some(ty) = ctx.typedefs.get(name) {
    if typedef_stack.contains(&name.to_string()) {
      return Err(rt.throw_type_error(&format!(
        "typedef cycle detected: {} -> {name}",
        typedef_stack.join(" -> ")
      )));
    }
    typedef_stack.push(name.to_string());
    let out = convert_to_idl_inner(rt, v, ty, ctx, typedef_stack, state);
    typedef_stack.pop();
    return out;
  }

  if ctx.enums.contains_key(name) {
    if !state.int_attrs.is_empty() {
      return Err(rt.throw_type_error(
        "[Clamp]/[EnforceRange] annotations cannot apply to an enum type",
      ));
    }
    return convert_to_enum(rt, v, name, ctx);
  }

  if ctx.dictionaries.contains_key(name) {
    if !state.int_attrs.is_empty() {
      return Err(rt.throw_type_error(
        "[Clamp]/[EnforceRange] annotations cannot apply to a dictionary type",
      ));
    }
    return convert_to_dictionary(rt, v, name, ctx, typedef_stack);
  }

  match named.kind {
    NamedTypeKind::Interface => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to an interface type",
        ));
      }
      // WebIDL interface conversions are specified in terms of platform objects owned by the
      // embedding. We validate the brand via `implements_interface` and then store the opaque host
      // id (if available) for downstream bindings to map back to Rust objects.
      let opaque = to_interface_opaque(rt, v, name)?;
      Ok(ConvertedValue::PlatformObject(PlatformObject::new(opaque)))
    }
    NamedTypeKind::CallbackFunction | NamedTypeKind::CallbackInterface => {
      if !state.int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to a callback type",
        ));
      }
      let cb = convert_to_callback_internal(rt, v, &IdlType::Named(named.clone()), state.legacy_treat_non_object_as_null)?;
      if rt.is_null(cb) {
        Ok(ConvertedValue::Null)
      } else {
        Ok(ConvertedValue::Any(cb))
      }
    }
    _ => Err(rt.throw_type_error(&format!("Unknown named type `{name}`"))),
  }
}

fn convert_to_string<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  string_type: StringType,
  state: ConversionState,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  // DOMString conversion has a LegacyNullToEmptyString special case.
  if matches!(string_type, StringType::DomString) && state.legacy_null_to_empty_string && rt.is_null(v) {
    return Ok(ConvertedValue::String(String::new()));
  }

  let string_val = rt.to_string(v)?;
  let s = rt.string_to_utf8_lossy(string_val)?;

  match string_type {
    StringType::DomString => Ok(ConvertedValue::String(s)),
    StringType::UsvString => {
      // Our runtime string extraction already produces Unicode scalar values, so this is already a
      // USVString (surrogates are replaced with U+FFFD by construction).
      Ok(ConvertedValue::String(s))
    }
    StringType::ByteString => {
      if s.chars().any(|c| (c as u32) > 0xFF) {
        return Err(rt.throw_type_error("ByteString contains code point > 0xFF"));
      }
      Ok(ConvertedValue::String(s))
    }
  }
}

fn convert_to_enum<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  enum_name: &str,
  ctx: &TypeContext,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  let values = ctx
    .enums
    .get(enum_name)
    .ok_or_else(|| rt.throw_type_error(&format!("Unknown enum `{enum_name}`")))?;
  let s = rt.to_string(v)?;
  let s = rt.string_to_utf8_lossy(s)?;
  if !values.contains(&s) {
    return Err(rt.throw_type_error(&format!(
      "Value is not a valid member of the `{enum_name}` enum"
    )));
  }
  Ok(ConvertedValue::Enum(s))
}

fn convert_to_numeric<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  numeric_type: NumericType,
  int_attrs: IntegerConversionAttrs,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  match numeric_type {
    NumericType::Float => {
      if !int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations only apply to integer types",
        ));
      }
      let x = rt.to_number(v)?;
      if x.is_nan() || x.is_infinite() {
        return Err(rt.throw_type_error("float must be a finite number"));
      }
      let mut y = x as f32;
      if y.is_infinite() {
        return Err(rt.throw_type_error("float is out of range"));
      }
      if y == 0.0 && x.is_sign_negative() {
        y = -0.0;
      }
      Ok(ConvertedValue::Float(y))
    }
    NumericType::UnrestrictedFloat => {
      if !int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations only apply to integer types",
        ));
      }
      let x = rt.to_number(v)?;
      if x.is_nan() {
        return Ok(ConvertedValue::UnrestrictedFloat(f32::from_bits(0x7fc0_0000)));
      }
      let mut y = x as f32;
      if y == 0.0 && x.is_sign_negative() {
        y = -0.0;
      }
      Ok(ConvertedValue::UnrestrictedFloat(y))
    }
    NumericType::Double => {
      if !int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations only apply to integer types",
        ));
      }
      let x = rt.to_number(v)?;
      if x.is_nan() || x.is_infinite() {
        return Err(rt.throw_type_error("double must be a finite number"));
      }
      Ok(ConvertedValue::Double(x))
    }
    NumericType::UnrestrictedDouble => {
      if !int_attrs.is_empty() {
        return Err(rt.throw_type_error(
          "[Clamp]/[EnforceRange] annotations only apply to integer types",
        ));
      }
      let x = rt.to_number(v)?;
      if x.is_nan() {
        return Ok(ConvertedValue::UnrestrictedDouble(f64::from_bits(
          0x7ff8_0000_0000_0000,
        )));
      }
      Ok(ConvertedValue::UnrestrictedDouble(x))
    }

    // Integer types.
    NumericType::Byte => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 8, true, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::Byte(v as i8))
    }
    NumericType::Octet => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 8, false, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::Octet(v as u8))
    }
    NumericType::Short => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 16, true, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::Short(v as i16))
    }
    NumericType::UnsignedShort => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 16, false, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::UnsignedShort(v as u16))
    }
    NumericType::Long => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 32, true, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::Long(v as i32))
    }
    NumericType::UnsignedLong => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 32, false, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::UnsignedLong(v as u32))
    }
    NumericType::LongLong => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 64, true, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::LongLong(v as i64))
    }
    NumericType::UnsignedLongLong => {
      let n = rt.to_number(v)?;
      let v = convert_to_int(n, 64, false, int_attrs).map_err(|e| throw_webidl_exception(rt, e))?;
      Ok(ConvertedValue::UnsignedLongLong(v as u64))
    }
  }
}

fn convert_to_int(
  n: f64,
  bit_length: u32,
  signed: bool,
  ext: IntegerConversionAttrs,
) -> Result<f64, WebIdlException> {
  let (lower_bound, upper_bound) = if bit_length == 64 {
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

  // `ToNumber(V)` is done by the caller; normalize -0 to +0.
  let mut x = n;
  if x == 0.0 && x.is_sign_negative() {
    x = 0.0;
  }

  if ext.enforce_range {
    if x.is_nan() || x.is_infinite() {
      return Err(WebIdlException::range_error(
        "EnforceRange integer conversion cannot be NaN/Infinity",
      ));
    }
    x = integer_part(x);
    if x < lower_bound || x > upper_bound {
      return Err(WebIdlException::range_error(
        "integer value is outside EnforceRange bounds",
      ));
    }
    return Ok(x);
  }

  if ext.clamp && !x.is_nan() {
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
  if n < 0.0 {
    -r
  } else {
    r
  }
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
  let floor_int = floor as i64;
  if floor_int % 2 == 0 {
    floor
  } else {
    floor + 1.0
  }
}

fn is_null_or_undefined<R: JsRuntime>(rt: &R, value: R::JsValue) -> bool {
  // ECMAScript values are:
  // `Undefined | Null | Boolean | Number | BigInt | String | Symbol | Object`.
  //
  // WebIDL algorithms frequently treat `undefined` and `null` as a single bucket; we derive
  // "null or undefined" by excluding every other type.
  !rt.is_object(value)
    && !rt.is_boolean(value)
    && !rt.is_number(value)
    && !rt.is_bigint(value)
    && !rt.is_string(value)
    && !rt.is_symbol(value)
}

/// Convert an ECMAScript value to a WebIDL callback function value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-function>
pub fn to_callback_function<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  legacy_treat_non_object_as_null: bool,
) -> Result<R::JsValue, R::Error> {
  if legacy_treat_non_object_as_null && !rt.is_object(value) {
    return Ok(rt.js_null());
  }
  if rt.is_callable(value) {
    return Ok(value);
  }
  Err(rt.throw_type_error("Value is not a callable callback function"))
}

/// Convert an ECMAScript value to a WebIDL callback interface value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-interface>
pub fn to_callback_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
) -> Result<R::JsValue, R::Error> {
  if rt.is_callable(value) {
    return Ok(value);
  }
  if !rt.is_object(value) {
    return Err(rt.throw_type_error("Value is not a callable callback interface"));
  }

  let handle_event_key = rt.property_key_from_str("handleEvent")?;
  match rt.get_method(value, handle_event_key)? {
    Some(_) => Ok(value),
    None => Err(rt.throw_type_error(
      "Callback interface object is missing a callable handleEvent method",
    )),
  }
}

/// Convert an ECMAScript value to a WebIDL interface type.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-interface>
pub fn to_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  interface: &str,
) -> Result<R::JsValue, R::Error> {
  if rt.implements_interface(value, interface) {
    return Ok(value);
  }
  Err(rt.throw_type_error(&format!(
    "Value is not a platform object implementing interface `{interface}`"
  )))
}

/// Convert an ECMAScript value to an opaque platform object id for a given WebIDL interface.
///
/// This is a convenience wrapper around [`WebIdlJsRuntime::implements_interface`] and
/// [`WebIdlJsRuntime::platform_object_opaque`].
pub fn to_interface_opaque<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  interface: &str,
) -> Result<u64, R::Error> {
  if !rt.implements_interface(value, interface) {
    return Err(rt.throw_type_error(&format!(
      "Value is not a platform object implementing interface `{interface}`"
    )));
  }
  rt
    .platform_object_opaque(value)
    .ok_or_else(|| {
      rt.throw_type_error(&format!(
        "Platform object implementing interface `{interface}` does not expose an opaque id"
      ))
    })
}

fn convert_to_callback_internal<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
  legacy_treat_non_object_as_null: bool,
) -> Result<R::JsValue, R::Error> {
  match ty {
    IdlType::Annotated { annotations, inner } => {
      let legacy = legacy_treat_non_object_as_null
        || annotations
          .iter()
          .any(|ann| matches!(ann, TypeAnnotation::LegacyTreatNonObjectAsNull));
      convert_to_callback_internal(rt, value, inner, legacy)
    }
    IdlType::Nullable(inner) => {
      if is_null_or_undefined(rt, value) {
        return Ok(rt.js_null());
      }
      convert_to_callback_internal(rt, value, inner, legacy_treat_non_object_as_null)
    }
    IdlType::Named(named) => match named.kind {
      NamedTypeKind::CallbackFunction => {
        to_callback_function(rt, value, legacy_treat_non_object_as_null)
      }
      NamedTypeKind::CallbackInterface => {
        // Callback interfaces are normally structural (`handleEvent`) or callable. However, bindings
        // generators may also treat embedding-defined platform objects as implementing callback
        // interfaces, so accept those first.
        if rt.implements_interface(value, &named.name) {
          return Ok(value);
        }
        to_callback_interface(rt, value)
      }
      _ => Err(rt.throw_type_error("Expected a callback function or callback interface type")),
    },
    _ => Err(rt.throw_type_error("Expected a callback function or callback interface type")),
  }
}

/// Convert an ECMAScript value to a callback type described by `ty`.
///
/// This helper only supports callback functions and callback interfaces (with optional `nullable`
/// and `[LegacyTreatNonObjectAsNull]` wrappers). It returns the underlying engine value (function
/// or object) so the host can store it and invoke it later.
pub fn convert_to_callback<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<R::JsValue, R::Error> {
  convert_to_callback_internal(rt, value, ty, false)
}

fn convert_to_interface_internal<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<R::JsValue, R::Error> {
  match ty {
    IdlType::Annotated { inner, .. } => convert_to_interface_internal(rt, value, inner),
    IdlType::Nullable(inner) => {
      if is_null_or_undefined(rt, value) {
        return Ok(rt.js_null());
      }
      convert_to_interface_internal(rt, value, inner)
    }
    IdlType::Named(named) => match named.kind {
      NamedTypeKind::Interface => to_interface(rt, value, &named.name),
      _ => Err(rt.throw_type_error("Expected an interface type")),
    },
    _ => Err(rt.throw_type_error("Expected an interface type")),
  }
}

/// Convert an ECMAScript value to a WebIDL interface type described by `ty`.
///
/// This helper supports `interface` types with optional `nullable` and `Annotated` wrappers. It
/// returns the underlying engine value so bindings can retain the original JS wrapper (and then
/// use [`WebIdlJsRuntime::platform_object_opaque`] to map it back to host data if needed).
pub fn convert_to_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<R::JsValue, R::Error> {
  convert_to_interface_internal(rt, value, ty)
}

fn convert_to_interface_opaque_internal<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<Option<u64>, R::Error> {
  match ty {
    IdlType::Annotated { inner, .. } => convert_to_interface_opaque_internal(rt, value, inner),
    IdlType::Nullable(inner) => {
      if is_null_or_undefined(rt, value) {
        return Ok(None);
      }
      convert_to_interface_opaque_internal(rt, value, inner)
    }
    IdlType::Named(named) => match named.kind {
      NamedTypeKind::Interface => Ok(Some(to_interface_opaque(rt, value, &named.name)?)),
      _ => Err(rt.throw_type_error("Expected an interface type")),
    },
    _ => Err(rt.throw_type_error("Expected an interface type")),
  }
}

/// Convert an ECMAScript value to an opaque platform object id described by `ty`.
///
/// - For `Interface` types, returns `Ok(Some(id))`.
/// - For `nullable interface` types, returns `Ok(None)` when `value` is `null` or `undefined`.
pub fn convert_to_interface_opaque<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<Option<u64>, R::Error> {
  convert_to_interface_opaque_internal(rt, value, ty)
}

/// Invoke a previously-converted callback interface value.
///
/// - If `callback` is callable, it is called with `this = undefined`.
/// - Otherwise, it is treated as an object and `callback.handleEvent(...args)` is invoked.
pub fn invoke_callback_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  callback: R::JsValue,
  args: &[R::JsValue],
) -> Result<R::JsValue, R::Error> {
  if rt.is_callable(callback) {
    return rt.call(callback, rt.js_undefined(), args);
  }
  if !rt.is_object(callback) {
    return Err(rt.throw_type_error("Callback interface value is not callable or an object"));
  }

  let handle_event_key = rt.property_key_from_str("handleEvent")?;
  let Some(handle_event) = rt.get_method(callback, handle_event_key)? else {
    return Err(rt.throw_type_error(
      "Callback interface object is missing a callable handleEvent method",
    ));
  };

  rt.call(handle_event, callback, args)
}

fn convert_to_sequence<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  elem_ty: &IdlType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  if !rt.is_object(v) {
    return Err(rt.throw_type_error("Value is not an object"));
  }
  let iterator_key = rt.symbol_iterator()?;
  let Some(method) = rt.get_method(v, iterator_key)? else {
    return Err(rt.throw_type_error("Value is not iterable"));
  };
  create_sequence_from_iterable(rt, v, method, elem_ty, ctx, typedef_stack)
}

fn create_sequence_from_iterable<R: WebIdlJsRuntime>(
  rt: &mut R,
  iterable: R::JsValue,
  method: R::JsValue,
  elem_ty: &IdlType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  let mut iterator_record = rt.get_iterator_from_method(iterable, method)?;
  let mut values = Vec::<ConvertedValue<R::JsValue>>::new();
  while let Some(next) = rt.iterator_step_value(&mut iterator_record)? {
    let converted = convert_to_idl_inner(
      rt,
      next,
      elem_ty,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    values.push(converted);
  }
  Ok(ConvertedValue::Sequence {
    elem_ty: Box::new(elem_ty.clone()),
    values,
  })
}

fn convert_to_record<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  key_ty: &IdlType,
  value_ty: &IdlType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  if !rt.is_object(v) {
    return Err(rt.throw_type_error("Value is not an object"));
  }

  let keys = rt.own_property_keys(v)?;
  let mut entries = BTreeMap::<String, ConvertedValue<R::JsValue>>::new();

  for key in keys {
    let Some(desc) = rt.get_own_property(v, key)? else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    let key_value = rt.property_key_to_js_string(key)?;
    let typed_key = convert_to_idl_inner(
      rt,
      key_value,
      key_ty,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    let ConvertedValue::String(typed_key) = typed_key else {
      return Err(rt.throw_type_error("Record key did not convert to a string"));
    };

    let value = rt.get(v, key)?;
    let typed_value = convert_to_idl_inner(
      rt,
      value,
      value_ty,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    entries.insert(typed_key, typed_value);
  }

  Ok(ConvertedValue::Record {
    key_ty: Box::new(key_ty.clone()),
    value_ty: Box::new(value_ty.clone()),
    entries,
  })
}

fn convert_to_dictionary<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  dict_name: &str,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  if !rt.is_object(v) && !rt.is_undefined(v) && !rt.is_null(v) {
    return Err(rt.throw_type_error("Value is not an object"));
  }

  let Some(members) = ctx.flattened_dictionary_members(dict_name) else {
    return Err(rt.throw_type_error(&format!("Unknown dictionary `{dict_name}`")));
  };

  let mut out = BTreeMap::<String, ConvertedValue<R::JsValue>>::new();
  for DictionaryMemberSchema {
    name,
    required,
    ty,
    default,
  } in members
  {
    let js_member_value = if rt.is_undefined(v) || rt.is_null(v) {
      rt.js_undefined()
    } else {
      let key = rt.property_key_from_str(&name)?;
      rt.get(v, key)?
    };

    if !rt.is_undefined(js_member_value) {
      let converted = convert_to_idl_inner(
        rt,
        js_member_value,
        &ty,
        ctx,
        typedef_stack,
        ConversionState::default(),
      )?;
      out.insert(name, converted);
      continue;
    }

    if let Some(default) = default {
      let evaluated = eval_default_value(&ty, &default, ctx).map_err(|e| throw_webidl_exception(rt, e))?;
      out.insert(name, converted_from_webidl_value::<R::JsValue>(evaluated));
      continue;
    }

    if required {
      return Err(rt.throw_type_error("Missing required dictionary member"));
    }
  }

  Ok(ConvertedValue::Dictionary {
    name: dict_name.to_string(),
    members: out,
  })
}

fn convert_to_union<R: WebIdlJsRuntime>(
  rt: &mut R,
  v: R::JsValue,
  members: &[IdlType],
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<ConvertedValue<R::JsValue>, R::Error> {
  // Spec: <https://webidl.spec.whatwg.org/#es-union>
  let flattened = flattened_union_member_types_with_typedefs(rt, members, ctx, typedef_stack)?;

  // 1. includes undefined
  if rt.is_undefined(v) && flattened.iter().any(|t| matches!(t, IdlType::Undefined)) {
    return Ok(ConvertedValue::Union {
      member_ty: Box::new(IdlType::Undefined),
      value: Box::new(ConvertedValue::Undefined),
    });
  }

  // 2. includes nullable type
  if (rt.is_null(v) || rt.is_undefined(v))
    && union_includes_nullable_type(rt, members, ctx, typedef_stack)?
  {
    // WebIDL uses the nullable member as the "specific type" for null.
    let nullable_inner = find_nullable_union_inner_type(rt, members, ctx, typedef_stack)?
      .unwrap_or(IdlType::Any);
    return Ok(ConvertedValue::Union {
      member_ty: Box::new(IdlType::Nullable(Box::new(nullable_inner))),
      value: Box::new(ConvertedValue::Null),
    });
  }

  // 4. null/undefined + dictionary
  if rt.is_null(v) || rt.is_undefined(v) {
    if let Some(dict_ty) = flattened.iter().find(|t| is_dictionary_type(t, ctx)) {
      let converted = convert_to_idl_inner(
        rt,
        v,
        dict_ty,
        ctx,
        typedef_stack,
        ConversionState::default(),
      )?;
      return Ok(ConvertedValue::Union {
        member_ty: Box::new(dict_ty.clone()),
        value: Box::new(converted),
      });
    }
  }

  // 11..16: object-based conversions
  if rt.is_object(v) {
    // sequence
    if let Some(seq_ty) = flattened.iter().find(|t| matches!(t, IdlType::Sequence(_))) {
      let iterator_key = rt.symbol_iterator()?;
      let method = rt.get_method(v, iterator_key)?;
      if let Some(method) = method {
        let IdlType::Sequence(elem_ty) = seq_ty else {
          unreachable!();
        };
        let seq = create_sequence_from_iterable(rt, v, method, elem_ty, ctx, typedef_stack)?;
        return Ok(ConvertedValue::Union {
          member_ty: Box::new(seq_ty.clone()),
          value: Box::new(seq),
        });
      }
    }

    // dictionary
    if let Some(dict_ty) = flattened.iter().find(|t| is_dictionary_type(t, ctx)) {
      let converted = convert_to_idl_inner(
        rt,
        v,
        dict_ty,
        ctx,
        typedef_stack,
        ConversionState::default(),
      )?;
      return Ok(ConvertedValue::Union {
        member_ty: Box::new(dict_ty.clone()),
        value: Box::new(converted),
      });
    }

    // record
    if let Some(record_ty) = flattened.iter().find(|t| matches!(t, IdlType::Record(_, _))) {
      let converted = convert_to_idl_inner(
        rt,
        v,
        record_ty,
        ctx,
        typedef_stack,
        ConversionState::default(),
      )?;
      return Ok(ConvertedValue::Union {
        member_ty: Box::new(record_ty.clone()),
        value: Box::new(converted),
      });
    }

    // object
    if flattened.iter().any(|t| matches!(t, IdlType::Object)) {
      return Ok(ConvertedValue::Union {
        member_ty: Box::new(IdlType::Object),
        value: Box::new(ConvertedValue::Object(v)),
      });
    }
  }

  // boolean
  if rt.is_boolean(v) && flattened.iter().any(|t| matches!(t, IdlType::Boolean)) {
    let converted = convert_to_idl_inner(
      rt,
      v,
      &IdlType::Boolean,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    return Ok(ConvertedValue::Union {
      member_ty: Box::new(IdlType::Boolean),
      value: Box::new(converted),
    });
  }

  // numeric
  if rt.is_number(v) {
    if let Some(num_ty) = flattened.iter().find(|t| matches!(t, IdlType::Numeric(_))) {
      let converted = convert_to_idl_inner(
        rt,
        v,
        num_ty,
        ctx,
        typedef_stack,
        ConversionState::default(),
      )?;
      return Ok(ConvertedValue::Union {
        member_ty: Box::new(num_ty.clone()),
        value: Box::new(converted),
      });
    }
  }

  // string (including enums)
  if let Some(str_ty) = flattened.iter().find(|t| is_string_type(t, ctx)) {
    let converted = convert_to_idl_inner(
      rt,
      v,
      str_ty,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    return Ok(ConvertedValue::Union {
      member_ty: Box::new(str_ty.clone()),
      value: Box::new(converted),
    });
  }

  // Fallback conversions.
  if let Some(num_ty) = flattened.iter().find(|t| matches!(t, IdlType::Numeric(_))) {
    let converted = convert_to_idl_inner(
      rt,
      v,
      num_ty,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    return Ok(ConvertedValue::Union {
      member_ty: Box::new(num_ty.clone()),
      value: Box::new(converted),
    });
  }
  if flattened.iter().any(|t| matches!(t, IdlType::Boolean)) {
    let converted = convert_to_idl_inner(
      rt,
      v,
      &IdlType::Boolean,
      ctx,
      typedef_stack,
      ConversionState::default(),
    )?;
    return Ok(ConvertedValue::Union {
      member_ty: Box::new(IdlType::Boolean),
      value: Box::new(converted),
    });
  }

  Err(rt.throw_type_error("Value does not match any union member type"))
}

fn is_string_type(ty: &IdlType, ctx: &TypeContext) -> bool {
  match ty {
    IdlType::String(_) => true,
    IdlType::Named(NamedType { name, .. }) => ctx.enums.contains_key(name),
    _ => false,
  }
}

fn is_dictionary_type(ty: &IdlType, ctx: &TypeContext) -> bool {
  match ty {
    IdlType::Named(NamedType { name, .. }) => ctx.dictionaries.contains_key(name),
    _ => false,
  }
}

fn includes_undefined<R: WebIdlJsRuntime>(
  rt: &mut R,
  ty: &IdlType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<bool, R::Error> {
  Ok(match ty {
    IdlType::Undefined => true,
    IdlType::Nullable(inner) => includes_undefined(rt, inner, ctx, typedef_stack)?,
    IdlType::Annotated { inner, .. } => includes_undefined(rt, inner, ctx, typedef_stack)?,
    IdlType::Union(members) => {
      let mut any = false;
      for m in members {
        if includes_undefined(rt, m, ctx, typedef_stack)? {
          any = true;
          break;
        }
      }
      any
    }
    IdlType::Named(NamedType { name, .. }) => {
      let Some(ty) = ctx.typedefs.get(name) else {
        return Ok(false);
      };
      if typedef_stack.contains(name) {
        return Err(rt.throw_type_error(&format!(
          "typedef cycle detected: {} -> {name}",
          typedef_stack.join(" -> ")
        )));
      }
      typedef_stack.push(name.clone());
      let out = includes_undefined(rt, ty, ctx, typedef_stack)?;
      typedef_stack.pop();
      out
    }
    _ => false,
  })
}

fn union_includes_nullable_type<R: WebIdlJsRuntime>(
  rt: &mut R,
  members: &[IdlType],
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<bool, R::Error> {
  Ok(number_of_nullable_member_types(rt, members, ctx, typedef_stack)? == 1)
}

fn number_of_nullable_member_types<R: WebIdlJsRuntime>(
  rt: &mut R,
  members: &[IdlType],
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<usize, R::Error> {
  let mut n = 0usize;
  for m in members {
    let mut u = m;
    if let IdlType::Annotated { inner, .. } = u {
      u = inner;
    }

    if let IdlType::Named(NamedType { name, .. }) = u {
      if let Some(td) = ctx.typedefs.get(name) {
        if typedef_stack.contains(name) {
          return Err(rt.throw_type_error(&format!(
            "typedef cycle detected: {} -> {name}",
            typedef_stack.join(" -> ")
          )));
        }
        typedef_stack.push(name.clone());
        n += match td {
          IdlType::Union(inner_members) => number_of_nullable_member_types(rt, inner_members, ctx, typedef_stack)?,
          other => number_of_nullable_member_types(rt, std::slice::from_ref(other), ctx, typedef_stack)?,
        };
        typedef_stack.pop();
        continue;
      }
    }

    if let IdlType::Nullable(inner) = u {
      n += 1;
      u = inner;
    }

    if let IdlType::Union(inner_members) = u {
      n += number_of_nullable_member_types(rt, inner_members, ctx, typedef_stack)?;
    }
  }
  Ok(n)
}

fn find_nullable_union_inner_type<R: WebIdlJsRuntime>(
  rt: &mut R,
  members: &[IdlType],
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<Option<IdlType>, R::Error> {
  for m in members {
    let mut u = m;
    if let IdlType::Annotated { inner, .. } = u {
      u = inner;
    }

    if let IdlType::Named(NamedType { name, .. }) = u {
      if let Some(td) = ctx.typedefs.get(name) {
        if typedef_stack.contains(name) {
          return Err(rt.throw_type_error(&format!(
            "typedef cycle detected: {} -> {name}",
            typedef_stack.join(" -> ")
          )));
        }
        typedef_stack.push(name.clone());
        let out = match td {
          IdlType::Union(inner_members) => find_nullable_union_inner_type(rt, inner_members, ctx, typedef_stack)?,
          other => find_nullable_union_inner_type(rt, std::slice::from_ref(other), ctx, typedef_stack)?,
        };
        typedef_stack.pop();
        if out.is_some() {
          return Ok(out);
        }
        continue;
      }
    }

    if let IdlType::Nullable(inner) = u {
      return Ok(Some((**inner).clone()));
    }

    if let IdlType::Union(inner_members) = u {
      if let Some(found) = find_nullable_union_inner_type(rt, inner_members, ctx, typedef_stack)? {
        return Ok(Some(found));
      }
    }
  }
  Ok(None)
}

fn flattened_union_member_types_with_typedefs<R: WebIdlJsRuntime>(
  rt: &mut R,
  members: &[IdlType],
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<Vec<IdlType>, R::Error> {
  let mut out: Vec<IdlType> = Vec::new();
  for member in members {
    flatten_union_member_types_into(rt, &mut out, member, ctx, typedef_stack)?;
  }
  Ok(out)
}

fn flatten_union_member_types_into<R: WebIdlJsRuntime>(
  rt: &mut R,
  out: &mut Vec<IdlType>,
  ty: &IdlType,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<(), R::Error> {
  let mut u = ty;
  if let IdlType::Annotated { inner, .. } = u {
    u = inner;
  }
  if let IdlType::Nullable(inner) = u {
    u = inner;
  }

  if let IdlType::Named(NamedType { name, .. }) = u {
    if let Some(td) = ctx.typedefs.get(name) {
      if typedef_stack.contains(name) {
        return Err(rt.throw_type_error(&format!(
          "typedef cycle detected: {} -> {name}",
          typedef_stack.join(" -> ")
        )));
      }
      typedef_stack.push(name.clone());
      flatten_union_member_types_into(rt, out, td, ctx, typedef_stack)?;
      typedef_stack.pop();
      return Ok(());
    }
  }

  match u {
    IdlType::Union(inner_members) => {
      for m in inner_members {
        flatten_union_member_types_into(rt, out, m, ctx, typedef_stack)?;
      }
    }
    other => {
      if !out.contains(other) {
        out.push(other.clone());
      }
    }
  }
  Ok(())
}

fn converted_from_webidl_value<V: Copy>(value: WebIdlValue) -> ConvertedValue<V> {
  match value {
    WebIdlValue::Undefined => ConvertedValue::Undefined,
    WebIdlValue::Null => ConvertedValue::Null,
    WebIdlValue::Boolean(b) => ConvertedValue::Boolean(b),

    WebIdlValue::Byte(v) => ConvertedValue::Byte(v),
    WebIdlValue::Octet(v) => ConvertedValue::Octet(v),
    WebIdlValue::Short(v) => ConvertedValue::Short(v),
    WebIdlValue::UnsignedShort(v) => ConvertedValue::UnsignedShort(v),
    WebIdlValue::Long(v) => ConvertedValue::Long(v),
    WebIdlValue::UnsignedLong(v) => ConvertedValue::UnsignedLong(v),
    WebIdlValue::LongLong(v) => ConvertedValue::LongLong(v),
    WebIdlValue::UnsignedLongLong(v) => ConvertedValue::UnsignedLongLong(v),
    WebIdlValue::Float(v) => ConvertedValue::Float(v),
    WebIdlValue::UnrestrictedFloat(v) => ConvertedValue::UnrestrictedFloat(v),
    WebIdlValue::Double(v) => ConvertedValue::Double(v),
    WebIdlValue::UnrestrictedDouble(v) => ConvertedValue::UnrestrictedDouble(v),

    WebIdlValue::String(s) => ConvertedValue::String(s),
    WebIdlValue::Enum(s) => ConvertedValue::Enum(s),

    WebIdlValue::Sequence { elem_ty, values } => ConvertedValue::Sequence {
      elem_ty,
      values: values
        .into_iter()
        .map(converted_from_webidl_value::<V>)
        .collect(),
    },
    WebIdlValue::Record {
      key_ty,
      value_ty,
      entries,
    } => ConvertedValue::Record {
      key_ty,
      value_ty,
      entries: entries
        .into_iter()
        .map(|(k, v)| (k, converted_from_webidl_value::<V>(v)))
        .collect(),
    },
    WebIdlValue::Dictionary { name, members } => ConvertedValue::Dictionary {
      name,
      members: members
        .into_iter()
        .map(|(k, v)| (k, converted_from_webidl_value::<V>(v)))
        .collect(),
    },
    WebIdlValue::Union { member_ty, value } => ConvertedValue::Union {
      member_ty,
      value: Box::new(converted_from_webidl_value::<V>(*value)),
    },
    WebIdlValue::PlatformObject(obj) => ConvertedValue::PlatformObject(obj),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::VmJsRuntime;
  use vm_js::{Value, VmError};

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn enforce_range_integer_conversion_throws_range_error() {
    let mut rt = VmJsRuntime::new();

    let err = to_byte(
      &mut rt,
      Value::Number(200.0),
      IntegerConversionAttrs {
        enforce_range: true,
        clamp: false,
      },
    )
    .expect_err("out-of-range enforce-range conversion should throw");

    let VmError::Throw(thrown) = err else {
      panic!("expected VmError::Throw, got {err:?}");
    };

    let s = rt.to_string(thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);
    assert!(
      msg.starts_with("RangeError:"),
      "expected RangeError, got {msg:?}"
    );
  }
}
