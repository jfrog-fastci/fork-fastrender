use crate::{GcObject, GcString, GcSymbol, Heap};

/// A JavaScript BigInt primitive value.
///
/// This implementation intentionally keeps BigInts inline (no GC allocation) because the
/// test262-smoke suite exercises only values that fit within 128 bits. The representation is
/// sign+magnitude so we can represent the full `u128` range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JsBigInt {
  negative: bool,
  magnitude: u128,
}

impl JsBigInt {
  pub fn zero() -> Self {
    Self {
      negative: false,
      magnitude: 0,
    }
  }

  pub fn from_i128(value: i128) -> Self {
    if value == 0 {
      return Self::zero();
    }
    if value < 0 {
      // `-i128::MIN` overflows, so handle it explicitly.
      let magnitude = if value == i128::MIN {
        1u128 << 127
      } else {
        (-value) as u128
      };
      Self {
        negative: true,
        magnitude,
      }
    } else {
      Self {
        negative: false,
        magnitude: value as u128,
      }
    }
  }

  pub fn from_u128(value: u128) -> Self {
    Self {
      negative: false,
      magnitude: value,
    }
  }

  pub fn is_zero(self) -> bool {
    self.magnitude == 0
  }

  pub fn is_negative(self) -> bool {
    self.negative && !self.is_zero()
  }

  pub fn negate(self) -> Self {
    if self.is_zero() {
      self
    } else {
      Self {
        negative: !self.negative,
        magnitude: self.magnitude,
      }
    }
  }

  pub fn checked_add(self, other: Self) -> Option<Self> {
    match (self.is_negative(), other.is_negative()) {
      (false, false) => self
        .magnitude
        .checked_add(other.magnitude)
        .map(Self::from_u128),
      (true, true) => self
        .magnitude
        .checked_add(other.magnitude)
        .map(|mag| Self {
          negative: true,
          magnitude: mag,
        }),
      // Mixed signs: subtraction of magnitudes.
      _ => {
        let (larger, smaller, larger_negative) = if self.magnitude >= other.magnitude {
          (self.magnitude, other.magnitude, self.is_negative())
        } else {
          (other.magnitude, self.magnitude, other.is_negative())
        };
        let mag = larger - smaller;
        Some(if mag == 0 {
          Self::zero()
        } else {
          Self {
            negative: larger_negative,
            magnitude: mag,
          }
        })
      }
    }
  }

  pub fn checked_mul(self, other: Self) -> Option<Self> {
    let mag = self.magnitude.checked_mul(other.magnitude)?;
    if mag == 0 {
      return Some(Self::zero());
    }
    Some(Self {
      negative: self.is_negative() ^ other.is_negative(),
      magnitude: mag,
    })
  }

  fn magnitude_bit_len(self) -> u32 {
    if self.magnitude == 0 {
      0
    } else {
      128 - self.magnitude.leading_zeros()
    }
  }

  fn twos_complement_mask(width: u32) -> u128 {
    debug_assert!((1..=128).contains(&width));
    if width == 128 {
      u128::MAX
    } else {
      (1u128 << width) - 1
    }
  }

  fn to_twos_complement_u128(self, width: u32) -> u128 {
    debug_assert!((1..=128).contains(&width));
    let mask = Self::twos_complement_mask(width);
    let raw = if self.is_negative() {
      if width == 128 {
        (0u128).wrapping_sub(self.magnitude)
      } else {
        (1u128 << width) - self.magnitude
      }
    } else {
      self.magnitude
    };
    raw & mask
  }

  fn from_twos_complement_u128(value: u128, width: u32) -> Self {
    debug_assert!((1..=128).contains(&width));
    let mask = Self::twos_complement_mask(width);
    let value = value & mask;
    let sign_bit = 1u128 << (width - 1);
    if (value & sign_bit) == 0 {
      Self {
        negative: false,
        magnitude: value,
      }
    } else {
      let magnitude = if width == 128 {
        (0u128).wrapping_sub(value)
      } else {
        (1u128 << width) - value
      };
      debug_assert!(magnitude != 0);
      Self {
        negative: true,
        magnitude,
      }
    }
  }

  fn to_twos_complement_129(self) -> (bool, u128) {
    if self.is_negative() {
      (true, (0u128).wrapping_sub(self.magnitude))
    } else {
      (false, self.magnitude)
    }
  }

  fn from_twos_complement_129(high: bool, low: u128) -> Option<Self> {
    if !high {
      return Some(Self {
        negative: false,
        magnitude: low,
      });
    }
    let magnitude = (0u128).wrapping_sub(low);
    if magnitude == 0 {
      return None;
    }
    Some(Self {
      negative: true,
      magnitude,
    })
  }

  fn checked_bitwise_binary_op(
    self,
    other: Self,
    op_low: fn(u128, u128) -> u128,
    op_high: fn(bool, bool) -> bool,
  ) -> Option<Self> {
    let width = self.magnitude_bit_len().max(other.magnitude_bit_len()) + 1;
    if width <= 128 {
      let a = self.to_twos_complement_u128(width);
      let b = other.to_twos_complement_u128(width);
      Some(Self::from_twos_complement_u128(op_low(a, b), width))
    } else {
      debug_assert_eq!(width, 129);
      let (a_high, a_low) = self.to_twos_complement_129();
      let (b_high, b_low) = other.to_twos_complement_129();
      let out_high = op_high(a_high, b_high);
      let out_low = op_low(a_low, b_low);
      Self::from_twos_complement_129(out_high, out_low)
    }
  }

  pub fn checked_bitwise_not(self) -> Option<Self> {
    self
      .negate()
      .checked_add(Self {
        negative: true,
        magnitude: 1,
      })
  }

  pub fn checked_bitwise_and(self, other: Self) -> Option<Self> {
    self.checked_bitwise_binary_op(other, |a, b| a & b, |a, b| a & b)
  }

  pub fn checked_bitwise_or(self, other: Self) -> Option<Self> {
    self.checked_bitwise_binary_op(other, |a, b| a | b, |a, b| a | b)
  }

  pub fn checked_bitwise_xor(self, other: Self) -> Option<Self> {
    self.checked_bitwise_binary_op(other, |a, b| a ^ b, |a, b| a ^ b)
  }

  /// Converts this BigInt to `i128` if it fits.
  ///
  /// This is used by `vm-js` for shift operators (`<<`, `>>`) while its BigInt implementation
  /// remains intentionally bounded to 128 bits.
  pub fn try_to_i128(self) -> Option<i128> {
    if self.is_zero() {
      return Some(0);
    }
    if self.is_negative() {
      let min_mag = 1u128 << 127;
      if self.magnitude > min_mag {
        return None;
      }
      if self.magnitude == min_mag {
        return Some(i128::MIN);
      }
      Some(-(self.magnitude as i128))
    } else {
      if self.magnitude > i128::MAX as u128 {
        return None;
      }
      Some(self.magnitude as i128)
    }
  }

  pub fn to_decimal_string(self) -> String {
    let mag_str = self.magnitude.to_string();
    if self.is_negative() {
      format!("-{mag_str}")
    } else {
      mag_str
    }
  }
}

/// A JavaScript value.
///
/// This is the VM's canonical value representation. Heap-allocated values are represented using
/// GC-managed handles (e.g. [`GcString`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
  /// The JavaScript `undefined` value.
  Undefined,
  /// The JavaScript `null` value.
  Null,
  /// A JavaScript boolean.
  Bool(bool),
  /// A JavaScript number (IEEE-754 double).
  Number(f64),
  /// A JavaScript BigInt.
  BigInt(JsBigInt),
  /// A GC-managed JavaScript string.
  String(GcString),
  /// A GC-managed JavaScript symbol.
  Symbol(GcSymbol),
  /// A GC-managed JavaScript object.
  Object(GcObject),
}

impl Value {
  /// ECMAScript `SameValue(x, y)`.
  ///
  /// This differs from `==`/`===` for Numbers:
  /// - `NaN` is the same as `NaN`
  /// - `+0` and `-0` are distinct
  pub fn same_value(self, other: Self, heap: &Heap) -> bool {
    match (self, other) {
      (Value::Undefined, Value::Undefined) => true,
      (Value::Null, Value::Null) => true,
      (Value::Bool(a), Value::Bool(b)) => a == b,
      (Value::Number(a), Value::Number(b)) => {
        if a.is_nan() && b.is_nan() {
          return true;
        }
        if a == 0.0 && b == 0.0 {
          // Distinguish +0 and -0.
          return a.to_bits() == b.to_bits();
        }
        a == b
      }
      (Value::BigInt(a), Value::BigInt(b)) => a == b,
      (Value::String(a), Value::String(b)) => {
        let Ok(a) = heap.get_string(a) else {
          return false;
        };
        let Ok(b) = heap.get_string(b) else {
          return false;
        };
        a.as_code_units() == b.as_code_units()
      }
      (Value::Symbol(a), Value::Symbol(b)) => a == b,
      (Value::Object(a), Value::Object(b)) => a == b,
      _ => false,
    }
  }
}

impl From<GcString> for Value {
  fn from(value: GcString) -> Self {
    Self::String(value)
  }
}

impl From<GcSymbol> for Value {
  fn from(value: GcSymbol) -> Self {
    Self::Symbol(value)
  }
}

impl From<GcObject> for Value {
  fn from(value: GcObject) -> Self {
    Self::Object(value)
  }
}
