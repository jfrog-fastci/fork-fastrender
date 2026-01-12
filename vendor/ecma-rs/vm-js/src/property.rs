use crate::heap::{Trace, Tracer};
use crate::{GcString, GcSymbol, Heap, Value, VmError};

/// A JavaScript property key (ECMAScript `PropertyKey`).
///
/// This mirrors the spec's `PropertyKey` union: `String | Symbol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyKey {
  String(GcString),
  Symbol(GcSymbol),
}

impl PropertyKey {
  pub fn from_string(value: GcString) -> Self {
    Self::String(value)
  }

  pub fn from_symbol(value: GcSymbol) -> Self {
    Self::Symbol(value)
  }
}

impl Trace for PropertyKey {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    match self {
      PropertyKey::String(s) => tracer.trace_value(Value::String(*s)),
      PropertyKey::Symbol(s) => tracer.trace_value(Value::Symbol(*s)),
    }
  }
}

/// A concrete property descriptor.
#[derive(Debug, Clone, Copy)]
pub struct PropertyDescriptor {
  pub enumerable: bool,
  pub configurable: bool,
  pub kind: PropertyKind,
}

impl PropertyDescriptor {
  pub fn is_data_descriptor(&self) -> bool {
    matches!(self.kind, PropertyKind::Data { .. })
  }

  pub fn is_accessor_descriptor(&self) -> bool {
    matches!(self.kind, PropertyKind::Accessor { .. })
  }

  pub fn is_generic_descriptor(&self) -> bool {
    false
  }
}

impl Trace for PropertyDescriptor {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.kind.trace(tracer);
  }
}

/// The kind of property described by a [`PropertyDescriptor`].
#[derive(Debug, Clone, Copy)]
pub enum PropertyKind {
  Data { value: Value, writable: bool },
  Accessor { get: Value, set: Value },
}

impl Trace for PropertyKind {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    match self {
      PropertyKind::Data { value, .. } => tracer.trace_value(*value),
      PropertyKind::Accessor { get, set } => {
        tracer.trace_value(*get);
        tracer.trace_value(*set);
      }
    }
  }
}

/// A "partial" property descriptor patch used by `DefineProperty`-style operations.
#[derive(Debug, Default, Clone, Copy)]
pub struct PropertyDescriptorPatch {
  pub enumerable: Option<bool>,
  pub configurable: Option<bool>,
  pub value: Option<Value>,
  pub writable: Option<bool>,
  pub get: Option<Value>,
  pub set: Option<Value>,
}

impl PropertyDescriptorPatch {
  pub fn is_empty(&self) -> bool {
    self.enumerable.is_none()
      && self.configurable.is_none()
      && self.value.is_none()
      && self.writable.is_none()
      && self.get.is_none()
      && self.set.is_none()
  }

  pub fn is_data_descriptor(&self) -> bool {
    self.value.is_some() || self.writable.is_some()
  }

  pub fn is_accessor_descriptor(&self) -> bool {
    self.get.is_some() || self.set.is_some()
  }

  pub fn is_generic_descriptor(&self) -> bool {
    !self.is_data_descriptor() && !self.is_accessor_descriptor()
  }
  /// Validates that this patch does not mix data and accessor descriptor fields.
  ///
  /// Per ECMAScript, a descriptor cannot be both a Data Descriptor and an Accessor Descriptor.
  pub fn validate(&self) -> Result<(), VmError> {
    let has_data = self.value.is_some() || self.writable.is_some();
    let has_accessor = self.get.is_some() || self.set.is_some();
    if has_data && has_accessor {
      return Err(VmError::InvalidPropertyDescriptorPatch);
    }
    Ok(())
  }
}

impl Trace for PropertyDescriptorPatch {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    if let Some(v) = self.value {
      tracer.trace_value(v);
    }
    if let Some(v) = self.get {
      tracer.trace_value(v);
    }
    if let Some(v) = self.set {
      tracer.trace_value(v);
    }
  }
}

impl Heap {
  /// Compare two property keys.
  ///
  /// - String keys compare by UTF-16 code units.
  /// - Symbol keys compare by identity (handle equality).
  pub fn property_key_eq(&self, a: &PropertyKey, b: &PropertyKey) -> bool {
    match (a, b) {
      (PropertyKey::String(a), PropertyKey::String(b)) => {
        let Ok(a) = self.get_string(*a) else {
          return false;
        };
        let Ok(b) = self.get_string(*b) else {
          return false;
        };
        a.as_code_units() == b.as_code_units()
      }
      (PropertyKey::Symbol(a), PropertyKey::Symbol(b)) => a == b,
      _ => false,
    }
  }

  /// If `key` is a String that is an ECMAScript array index, returns its numeric value.
  ///
  /// An "array index" is a canonical `uint32` string `P` such that:
  /// - `ToString(ToUint32(P)) === P`, and
  /// - `ToUint32(P) !== 2^32 - 1`.
  ///
  /// This matches the ordering requirements for `OrdinaryOwnPropertyKeys`.
  pub(crate) fn array_index(&self, key: &PropertyKey) -> Option<u32> {
    let PropertyKey::String(s) = key else {
      return None;
    };
    let s = self.get_string(*s).ok()?;
    let units = s.as_code_units();
    if units.is_empty() {
      return None;
    }

    const U0: u16 = b'0' as u16;
    const U9: u16 = b'9' as u16;

    // `ToString(ToUint32(P)) === P` implies no leading zeros (except the single "0").
    if units.len() > 1 && units[0] == U0 {
      return None;
    }

    let mut value: u64 = 0;
    for &u in units {
      if !(U0..=U9).contains(&u) {
        return None;
      }
      value = value.checked_mul(10)?;
      value = value.checked_add((u - U0) as u64)?;
      if value > u32::MAX as u64 {
        return None;
      }
    }

    // Exclude 2^32-1.
    if value == u32::MAX as u64 {
      return None;
    }

    Some(value as u32)
  }

  /// Convert a value to a property key.
  ///
  /// This is a minimal implementation of the `ToPropertyKey` shape from ECMA-262:
  /// - `String`/`Symbol` values are returned directly.
  /// - `Object` values are not supported here because full `ToPropertyKey` requires `ToPrimitive`,
  ///   which can invoke user code. Use [`Scope::to_property_key`] for the spec-shaped operation.
  /// - All other values go through `ToString`.
  pub fn to_property_key(&mut self, value: Value) -> Result<PropertyKey, VmError> {
    match value {
      Value::String(s) => Ok(PropertyKey::String(s)),
      Value::Symbol(s) => Ok(PropertyKey::Symbol(s)),
      Value::Object(_) => Err(VmError::Unimplemented(
        "ToPropertyKey on objects requires ToPrimitive (use Scope::to_property_key)",
      )),
      other => Ok(PropertyKey::String(self.to_string(other)?)),
    }
  }

  /// ECMAScript `ToString` (minimal).
  ///
  /// This covers the primitive cases needed by WebIDL conversions:
  /// - `undefined`, `null`, booleans, numbers, strings.
  ///
  /// For `Object`, this is currently unimplemented (requires `ToPrimitive` and built-ins).
  ///
  /// For `Symbol`, this throws a TypeError.
  pub fn to_string(&mut self, value: Value) -> Result<GcString, VmError> {
    // Fast path: no allocation.
    if let Value::String(s) = value {
      return Ok(s);
    }

    // Allocate via a scope so we can root `value` across a GC triggered by the string allocation.
    let mut scope = self.scope();
    scope.push_root(value)?;

    match value {
      Value::Undefined => scope.alloc_string("undefined"),
      Value::Null => scope.alloc_string("null"),
      Value::Bool(true) => scope.alloc_string("true"),
      Value::Bool(false) => scope.alloc_string("false"),
      Value::Number(n) => {
        let s = number_to_string(n)?;
        scope.alloc_string(&s)
      }
      Value::BigInt(b) => {
        let s = {
          let bi = scope.heap().get_bigint(b)?;
          bi.to_string_radix_with_tick(10, &mut || Ok(()))?
        };
        scope.alloc_string(&s)
      }
      Value::String(s) => Ok(s),
      Value::Symbol(_) => Err(VmError::TypeError("Cannot convert a Symbol value to a string")),
      Value::Object(_) => Err(VmError::Unimplemented(
        "ToString on objects requires ToPrimitive/built-ins",
      )),
    }
  }

  /// Minimal ECMAScript `ToBoolean`.
  pub fn to_boolean(&self, value: Value) -> Result<bool, VmError> {
    Ok(match value {
      Value::Undefined | Value::Null => false,
      Value::Bool(b) => b,
      Value::Number(n) => n != 0.0 && !n.is_nan(),
      Value::BigInt(b) => !self.get_bigint(b)?.is_zero(),
      Value::String(s) => !self.get_string(s)?.as_code_units().is_empty(),
      Value::Symbol(_) | Value::Object(_) => true,
    })
  }

  /// ECMAScript `ToNumber` (minimal).
  ///
  /// This covers the primitive cases needed by WebIDL conversions:
  /// - `undefined`, `null`, booleans, numbers, strings.
  ///
  /// For `Object`, this returns [`VmError::Unimplemented`]. Full spec `ToNumber` requires
  /// `ToPrimitive`, which can invoke user code (`@@toPrimitive`, `valueOf`, `toString`) and
  /// therefore requires a [`Vm`] + host context. Use [`Scope::to_number`] for the spec-shaped
  /// operation.
  ///
  /// For `Symbol`, this throws a TypeError.
  pub fn to_number(&mut self, value: Value) -> Result<f64, VmError> {
    crate::ops::to_number(self, value)
  }

  /// Budget-aware variant of [`Heap::to_number`].
  pub fn to_number_with_tick(
    &mut self,
    value: Value,
    tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<f64, VmError> {
    let mut tick = tick;
    crate::ops::to_number_with_tick(self, value, &mut tick)
  }
}

// https://tc39.es/ecma262/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-tostring
pub(crate) fn number_to_string(n: f64) -> Result<String, VmError> {
  fn static_str(s: &'static str) -> Result<String, VmError> {
    let mut out = String::new();
    out
      .try_reserve_exact(s.len())
      .map_err(|_| VmError::OutOfMemory)?;
    out.push_str(s);
    Ok(out)
  }

  if n.is_nan() {
    return static_str("NaN");
  }
  if n == 0.0 {
    // Covers both +0 and -0.
    return static_str("0");
  }
  if n.is_infinite() {
    if n.is_sign_negative() {
      return static_str("-Infinity");
    } else {
      return static_str("Infinity");
    }
  }

  let sign = if n.is_sign_negative() { "-" } else { "" };
  let abs = n.abs();

  // Use `ryu` only to get the digit + exponent decomposition; the final formatting rules match
  // ECMAScript `Number::toString()` (not Rust's float formatting).
  let mut buffer = ryu::Buffer::new();
  let raw = buffer.format_finite(abs);
  // `ryu` formats `1.0` as `"1.0"`, but ECMAScript `ToString(1)` is `"1"`.
  let raw = raw.strip_suffix(".0").unwrap_or(raw);
  let (digits, exp) = parse_ryu_to_decimal(raw)?;
  let k = exp + digits.len() as i32;

  let sign_len = sign.len();
  let digits_len = digits.len();

  if k > 0 && k <= 21 {
    let k = k as usize;
    let total_len = sign_len
      .checked_add(if k >= digits_len { k } else { digits_len + 1 })
      .ok_or(VmError::OutOfMemory)?;
    let mut out = String::new();
    out
      .try_reserve_exact(total_len)
      .map_err(|_| VmError::OutOfMemory)?;
    out.push_str(sign);
    if k >= digits.len() {
      out.push_str(&digits);
      out.extend(std::iter::repeat('0').take(k - digits.len()));
    } else {
      out.push_str(&digits[..k]);
      out.push('.');
      out.push_str(&digits[k..]);
    }
    return Ok(out);
  }

  if k <= 0 && k > -6 {
    let total_len = sign_len
      .checked_add(2)
      .and_then(|v| v.checked_add((-k) as usize))
      .and_then(|v| v.checked_add(digits_len))
      .ok_or(VmError::OutOfMemory)?;
    let mut out = String::new();
    out
      .try_reserve_exact(total_len)
      .map_err(|_| VmError::OutOfMemory)?;
    out.push_str(sign);
    out.push_str("0.");
    out.extend(std::iter::repeat('0').take((-k) as usize));
    out.push_str(&digits);
    return Ok(out);
  }

  // Exponential form.
  fn decimal_len_u32(mut n: u32) -> usize {
    let mut len = 1usize;
    while n >= 10 {
      n /= 10;
      len += 1;
    }
    len
  }

  let exp_part = k - 1;
  let exp_mag = if exp_part >= 0 {
    exp_part as u32
  } else {
    // `exp_part` is `i32`, so its magnitude always fits in `u32`.
    (-i64::from(exp_part)) as u32
  };
  let exp_len = decimal_len_u32(exp_mag);
  let digits_part_len = digits_len + if digits_len > 1 { 1 } else { 0 };
  let total_len = sign_len
    .checked_add(digits_part_len)
    .and_then(|v| v.checked_add(1 /* e */ + 1 /* exp sign */ + exp_len))
    .ok_or(VmError::OutOfMemory)?;
  let mut out = String::new();
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;
  out.push_str(sign);

  let first = digits.as_bytes()[0] as char;
  out.push(first);
  if digits.len() > 1 {
    out.push('.');
    out.push_str(&digits[1..]);
  }
  out.push('e');
  fn push_u32_decimal(out: &mut String, mut value: u32) {
    // `u32::MAX` has 10 decimal digits.
    let mut buf = [0u8; 10];
    let mut pos = buf.len();
    if value == 0 {
      pos -= 1;
      buf[pos] = b'0';
    } else {
      while value != 0 {
        pos -= 1;
        buf[pos] = b'0' + (value % 10) as u8;
        value /= 10;
      }
    }
    // Safe by construction: ASCII digits.
    let s = std::str::from_utf8(&buf[pos..]).unwrap_or("0");
    out.push_str(s);
  }
  if exp_part >= 0 {
    out.push('+');
    push_u32_decimal(&mut out, exp_part as u32);
  } else {
    out.push('-');
    push_u32_decimal(&mut out, exp_mag);
  }
  Ok(out)
}

fn parse_ryu_to_decimal(raw: &str) -> Result<(String, i32), VmError> {
  // `raw` is expected to be ASCII and contain either:
  // - digits with optional decimal point
  // - digits with optional decimal point and a trailing `e[+-]?\d+`
  //
  // Returns `(digits, exp)` such that `value = digits × 10^exp` and `digits`
  // contains no leading zeros.
  let (mantissa, exp_part) = match raw.split_once('e') {
    Some((mantissa, exp)) => (mantissa, Some(exp)),
    None => (raw, None),
  };

  let mut exp: i32 = exp_part.map_or(0, |e| e.parse().unwrap_or(0));

  let mut digits = String::new();
  digits
    .try_reserve_exact(mantissa.len())
    .map_err(|_| VmError::OutOfMemory)?;
  let mut in_frac = false;
  let mut frac_len = 0usize;
  let mut started = false;

  for &b in mantissa.as_bytes() {
    if b == b'.' {
      in_frac = true;
      continue;
    }
    // `mantissa` is expected to be ASCII digits and `.` only.
    if in_frac {
      frac_len += 1;
    }
    if !started {
      if b == b'0' {
        continue;
      }
      started = true;
    }
    digits.push(b as char);
  }

  exp -= frac_len as i32;

  // `raw` comes from a non-zero number, but keep this robust against internal bugs.
  if digits.is_empty() {
    digits.push('0');
  }

  Ok((digits, exp))
}
