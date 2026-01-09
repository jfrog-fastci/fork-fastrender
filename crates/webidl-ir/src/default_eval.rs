use crate::{
  DefaultValue, DictionaryMemberSchema, IdlType, NamedType, NumericLiteral, NumericType,
  StringType, TypeAnnotation, TypeContext, WebIdlException, WebIdlValue,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default)]
struct IntegerConversionAttrs {
  clamp: bool,
  enforce_range: bool,
}

impl IntegerConversionAttrs {
  fn is_empty(self) -> bool {
    !self.clamp && !self.enforce_range
  }
}

pub fn eval_default_value(
  ty: &IdlType,
  dv: &DefaultValue,
  ctx: &TypeContext,
) -> Result<WebIdlValue, WebIdlException> {
  let mut typedef_stack = Vec::<String>::new();
  eval_default_value_inner(
    ty,
    dv,
    ctx,
    &mut typedef_stack,
    IntegerConversionAttrs::default(),
  )
}

fn eval_default_value_inner(
  ty: &IdlType,
  dv: &DefaultValue,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
  int_attrs: IntegerConversionAttrs,
) -> Result<WebIdlValue, WebIdlException> {
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
        return Err(WebIdlException::type_error(
          "[Clamp] and [EnforceRange] cannot both apply to the same type",
        ));
      }
      eval_default_value_inner(inner, dv, ctx, typedef_stack, out_attrs)
    }
    IdlType::Nullable(inner) => match dv {
      DefaultValue::Null => Ok(WebIdlValue::Null),
      _ => eval_default_value_inner(inner, dv, ctx, typedef_stack, int_attrs),
    },
    IdlType::Union(members) => {
      if !int_attrs.is_empty() {
        return Err(WebIdlException::type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to a union type",
        ));
      }
      eval_default_union(members, dv, ctx, typedef_stack)
    }

    IdlType::Any => {
      if !int_attrs.is_empty() {
        return Err(WebIdlException::type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `any`",
        ));
      }
      match dv {
        DefaultValue::Undefined => Ok(WebIdlValue::Undefined),
        DefaultValue::Boolean(b) => Ok(WebIdlValue::Boolean(*b)),
        DefaultValue::Number(lit) => eval_default_numeric_literal(
          NumericType::UnrestrictedDouble,
          lit,
          IntegerConversionAttrs::default(),
        ),
        DefaultValue::String(s) => Ok(WebIdlValue::String(s.clone())),
        DefaultValue::Null => Err(WebIdlException::type_error(
          "`null` default requires a nullable type",
        )),
        DefaultValue::EmptySequence | DefaultValue::EmptyDictionary => Err(
          WebIdlException::type_error("`[]`/`{}` defaults require a sequence/dictionary type"),
        ),
      }
    }
    IdlType::Undefined => {
      if !int_attrs.is_empty() {
        return Err(WebIdlException::type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `undefined`",
        ));
      }
      match dv {
        DefaultValue::Undefined => Ok(WebIdlValue::Undefined),
        _ => Err(WebIdlException::type_error(
          "default value is not `undefined`",
        )),
      }
    }
    IdlType::Boolean => {
      if !int_attrs.is_empty() {
        return Err(WebIdlException::type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `boolean`",
        ));
      }
      match dv {
        DefaultValue::Boolean(b) => Ok(WebIdlValue::Boolean(*b)),
        _ => Err(WebIdlException::type_error(
          "default value is not a boolean literal",
        )),
      }
    }
    IdlType::Numeric(numeric_type) => match dv {
      DefaultValue::Number(lit) => eval_default_numeric_literal(*numeric_type, lit, int_attrs),
      _ => Err(WebIdlException::type_error(
        "default value is not a numeric literal",
      )),
    },
    IdlType::BigInt => Err(WebIdlException::type_error(
      "default values for `bigint` are not supported",
    )),
    IdlType::String(string_type) => {
      if !int_attrs.is_empty() {
        return Err(WebIdlException::type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to string types",
        ));
      }
      match dv {
        DefaultValue::String(s) => eval_default_string_literal(*string_type, s, ctx),
        _ => Err(WebIdlException::type_error(
          "default value is not a string literal",
        )),
      }
    }
    IdlType::Object | IdlType::Symbol => Err(WebIdlException::type_error(
      "default value token is not supported for this type",
    )),
    IdlType::Named(NamedType { name, .. }) => {
      eval_default_named_type(name, dv, ctx, typedef_stack, int_attrs)
    }

    IdlType::Sequence(elem) => {
      if !int_attrs.is_empty() {
        return Err(WebIdlException::type_error(
          "[Clamp]/[EnforceRange] annotations cannot apply to `sequence`",
        ));
      }
      match dv {
        DefaultValue::EmptySequence => Ok(WebIdlValue::Sequence {
          elem_ty: elem.clone(),
          values: Vec::new(),
        }),
        _ => Err(WebIdlException::type_error("default value is not `[]`")),
      }
    }
    IdlType::FrozenArray(_) | IdlType::AsyncSequence(_) => Err(WebIdlException::type_error(
      "`[]` defaults are only supported for `sequence<T>` types",
    )),
    IdlType::Record(_, _) | IdlType::Promise(_) => Err(WebIdlException::type_error(
      "default value token is not supported for this type",
    )),
  }
}

fn eval_default_union(
  members: &[IdlType],
  dv: &DefaultValue,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<WebIdlValue, WebIdlException> {
  let mut flattened = Vec::<IdlType>::new();
  for member in members {
    flatten_union_member(&mut flattened, member);
  }
  dedupe_types(&mut flattened);

  let mut matches = Vec::<(IdlType, WebIdlValue)>::new();
  for member in flattened {
    let Ok(value) = eval_default_value_inner(
      &member,
      dv,
      ctx,
      typedef_stack,
      IntegerConversionAttrs::default(),
    ) else {
      continue;
    };
    matches.push((member, value));
  }

  match matches.len() {
    0 => Err(WebIdlException::type_error(
      "default value does not match any union member type",
    )),
    1 => {
      let (member_ty, value) = matches.pop().expect("len=1");
      Ok(WebIdlValue::Union {
        member_ty: Box::new(member_ty),
        value: Box::new(value),
      })
    }
    _ => Err(WebIdlException::type_error(
      "default value is ambiguous for the union type",
    )),
  }
}

fn flatten_union_member(out: &mut Vec<IdlType>, ty: &IdlType) {
  match ty.innermost_type() {
    IdlType::Union(members) => {
      for m in members {
        flatten_union_member(out, m);
      }
    }
    _ => out.push(ty.clone()),
  }
}

fn dedupe_types(types: &mut Vec<IdlType>) {
  let mut i = 0usize;
  while i < types.len() {
    let mut j = i + 1;
    while j < types.len() {
      if types[i] == types[j] {
        types.remove(j);
      } else {
        j += 1;
      }
    }
    i += 1;
  }
}

fn eval_default_named_type(
  name: &str,
  dv: &DefaultValue,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
  int_attrs: IntegerConversionAttrs,
) -> Result<WebIdlValue, WebIdlException> {
  if let Some(ty) = ctx.typedefs.get(name) {
    if typedef_stack.contains(&name.to_string()) {
      return Err(WebIdlException::type_error(format!(
        "typedef cycle detected: {} -> {name}",
        typedef_stack.join(" -> ")
      )));
    }
    typedef_stack.push(name.to_string());
    let out = eval_default_value_inner(ty, dv, ctx, typedef_stack, int_attrs);
    typedef_stack.pop();
    return out;
  }

  if ctx.enums.contains_key(name) {
    if !int_attrs.is_empty() {
      return Err(WebIdlException::type_error(
        "[Clamp]/[EnforceRange] annotations cannot apply to an enum type",
      ));
    }
    return eval_default_enum(name, dv, ctx);
  }

  if ctx.dictionaries.contains_key(name) {
    if !int_attrs.is_empty() {
      return Err(WebIdlException::type_error(
        "[Clamp]/[EnforceRange] annotations cannot apply to a dictionary type",
      ));
    }
    return eval_default_dictionary(name, dv, ctx, typedef_stack);
  }

  Err(WebIdlException::type_error(format!(
    "unknown named type `{name}`"
  )))
}

fn eval_default_enum(
  name: &str,
  dv: &DefaultValue,
  ctx: &TypeContext,
) -> Result<WebIdlValue, WebIdlException> {
  let DefaultValue::String(value) = dv else {
    return Err(WebIdlException::type_error(
      "enum default value must be a string literal",
    ));
  };
  let Some(values) = ctx.enums.get(name) else {
    return Err(WebIdlException::type_error(format!(
      "unknown enum `{name}`"
    )));
  };
  if !values.contains(value) {
    return Err(WebIdlException::type_error(format!(
      "enum `{name}` has no value `{value}`"
    )));
  }
  Ok(WebIdlValue::Enum(value.clone()))
}

fn eval_default_dictionary(
  name: &str,
  dv: &DefaultValue,
  ctx: &TypeContext,
  typedef_stack: &mut Vec<String>,
) -> Result<WebIdlValue, WebIdlException> {
  let DefaultValue::EmptyDictionary = dv else {
    return Err(WebIdlException::type_error(
      "dictionary default value must be `{}`",
    ));
  };

  let Some(members) = ctx.flattened_dictionary_members(name) else {
    return Err(WebIdlException::type_error(format!(
      "unknown dictionary `{name}`"
    )));
  };

  if members.iter().any(|m| m.required) {
    return Err(WebIdlException::type_error(format!(
      "dictionary `{name}` has required members, so empty dictionary defaults are invalid"
    )));
  }

  let mut out = BTreeMap::<String, WebIdlValue>::new();
  for DictionaryMemberSchema {
    name: member_name,
    ty,
    default,
    ..
  } in members
  {
    let Some(default) = default else {
      continue;
    };
    let v = eval_default_value_inner(
      &ty,
      &default,
      ctx,
      typedef_stack,
      IntegerConversionAttrs::default(),
    )?;
    out.insert(member_name, v);
  }

  Ok(WebIdlValue::Dictionary {
    name: name.to_string(),
    members: out,
  })
}

fn eval_default_string_literal(
  ty: StringType,
  value: &str,
  _ctx: &TypeContext,
) -> Result<WebIdlValue, WebIdlException> {
  match ty {
    StringType::DomString | StringType::UsvString => Ok(WebIdlValue::String(value.to_string())),
    StringType::ByteString => {
      if value.chars().any(|c| c as u32 > 0xFF) {
        return Err(WebIdlException::type_error(
          "ByteString default contains code point > 0xFF",
        ));
      }
      Ok(WebIdlValue::String(value.to_string()))
    }
  }
}

fn eval_default_numeric_literal(
  ty: NumericType,
  lit: &NumericLiteral,
  int_attrs: IntegerConversionAttrs,
) -> Result<WebIdlValue, WebIdlException> {
  if !ty.is_integer() && !int_attrs.is_empty() {
    return Err(WebIdlException::type_error(
      "[Clamp]/[EnforceRange] annotations only apply to integer numeric types",
    ));
  }

  let n = numeric_literal_to_f64(lit)?;
  match ty {
    NumericType::Byte => Ok(WebIdlValue::Byte(
      convert_to_int(n, 8, true, int_attrs)? as i8
    )),
    NumericType::Octet => Ok(WebIdlValue::Octet(
      convert_to_int(n, 8, false, int_attrs)? as u8
    )),
    NumericType::Short => Ok(WebIdlValue::Short(
      convert_to_int(n, 16, true, int_attrs)? as i16
    )),
    NumericType::UnsignedShort => Ok(WebIdlValue::UnsignedShort(convert_to_int(
      n, 16, false, int_attrs,
    )? as u16)),
    NumericType::Long => Ok(WebIdlValue::Long(
      convert_to_int(n, 32, true, int_attrs)? as i32
    )),
    NumericType::UnsignedLong => Ok(WebIdlValue::UnsignedLong(convert_to_int(
      n, 32, false, int_attrs,
    )? as u32)),
    NumericType::LongLong => Ok(WebIdlValue::LongLong(
      convert_to_int(n, 64, true, int_attrs)? as i64,
    )),
    NumericType::UnsignedLongLong => Ok(WebIdlValue::UnsignedLongLong(convert_to_int(
      n, 64, false, int_attrs,
    )? as u64)),
    NumericType::Float => eval_default_float(n),
    NumericType::UnrestrictedFloat => eval_default_unrestricted_float(n),
    NumericType::Double => eval_default_double(n),
    NumericType::UnrestrictedDouble => Ok(WebIdlValue::UnrestrictedDouble(canonicalize_nan_f64(n))),
  }
}

fn eval_default_float(n: f64) -> Result<WebIdlValue, WebIdlException> {
  if n.is_nan() || n.is_infinite() {
    return Err(WebIdlException::type_error(
      "`float` default cannot be NaN or Infinity",
    ));
  }
  let as_f32 = n as f32;
  if as_f32.is_infinite() {
    return Err(WebIdlException::type_error(
      "`float` default is out of range",
    ));
  }
  Ok(WebIdlValue::Float(as_f32))
}

fn eval_default_unrestricted_float(n: f64) -> Result<WebIdlValue, WebIdlException> {
  if n.is_nan() {
    return Ok(WebIdlValue::UnrestrictedFloat(f32::from_bits(0x7fc0_0000)));
  }
  Ok(WebIdlValue::UnrestrictedFloat(n as f32))
}

fn eval_default_double(n: f64) -> Result<WebIdlValue, WebIdlException> {
  if n.is_nan() || n.is_infinite() {
    return Err(WebIdlException::type_error(
      "`double` default cannot be NaN or Infinity",
    ));
  }
  Ok(WebIdlValue::Double(n))
}

fn canonicalize_nan_f64(n: f64) -> f64 {
  if n.is_nan() {
    return f64::from_bits(0x7ff8_0000_0000_0000);
  }
  n
}

fn numeric_literal_to_f64(lit: &NumericLiteral) -> Result<f64, WebIdlException> {
  match lit {
    NumericLiteral::Infinity { negative } => Ok(if *negative {
      f64::NEG_INFINITY
    } else {
      f64::INFINITY
    }),
    NumericLiteral::NaN => Ok(f64::NAN),
    NumericLiteral::Decimal(s) => s
      .parse::<f64>()
      .map_err(|_| WebIdlException::type_error("invalid decimal literal")),
    NumericLiteral::Integer(s) => parse_integer_literal_to_f64(s),
  }
}

fn parse_integer_literal_to_f64(token: &str) -> Result<f64, WebIdlException> {
  let token = token.trim();
  if token.is_empty() {
    return Err(WebIdlException::type_error("invalid integer literal"));
  }

  let mut sign = 1.0f64;
  let mut rest = token;
  if let Some(after) = rest.strip_prefix('-') {
    sign = -1.0;
    rest = after;
  } else if rest.starts_with('+') {
    // `+` is not part of WebIDL `integer`, but keep a clear error in case an upstream parser is lax.
    return Err(WebIdlException::type_error("invalid integer literal"));
  }

  // WebIDL integer token semantics:
  // - if it begins with `0x`/`0X`, the base is 16 (hex digits after the prefix)
  // - else if it begins with `0`, the base is 8 (octal digits after the leading 0; may be empty)
  // - otherwise the base is 10 (decimal digits)
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
    return Err(WebIdlException::type_error("invalid integer literal"));
  }

  let mut v = 0f64;
  for ch in digits.chars() {
    let d = ch
      .to_digit(base)
      .ok_or_else(|| WebIdlException::type_error("invalid integer literal"))?;
    v = v * (base as f64) + (d as f64);
  }
  Ok(sign * v)
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

  // `ToNumber(V)` is identity here: numeric literals are side-effect free.
  let mut x = n;
  if x == 0.0 && x.is_sign_negative() {
    x = 0.0;
  }

  if ext.enforce_range {
    if x.is_nan() || x.is_infinite() {
      return Err(WebIdlException::range_error(
        "EnforceRange integer default cannot be NaN/Infinity",
      ));
    }
    x = integer_part(x);
    if x < lower_bound || x > upper_bound {
      return Err(WebIdlException::range_error(
        "integer default is outside EnforceRange bounds",
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
  // exactly halfway between two integers
  let floor_int = floor as i64;
  if floor_int % 2 == 0 {
    floor
  } else {
    floor + 1.0
  }
}
