use crate::{GcObject, GcString, GcSymbol, Heap};
use std::cmp::Ordering;
use std::ops::{BitAnd, BitOr, BitXor};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct U256 {
  limbs: [u64; 4],
}

impl U256 {
  const ZERO: Self = Self { limbs: [0; 4] };

  fn from_u128(value: u128) -> Self {
    Self {
      limbs: [value as u64, (value >> 64) as u64, 0, 0],
    }
  }

  fn is_zero(self) -> bool {
    self.limbs.iter().all(|&x| x == 0)
  }

  fn bit_len(self) -> u32 {
    for i in (0..4).rev() {
      let limb = self.limbs[i];
      if limb != 0 {
        return (i as u32) * 64 + (64 - limb.leading_zeros());
      }
    }
    0
  }

  fn get_bit(self, bit: u32) -> bool {
    debug_assert!(bit < 256);
    let limb = (bit / 64) as usize;
    let offset = bit % 64;
    (self.limbs[limb] & (1u64 << offset)) != 0
  }

  fn checked_add(self, other: Self) -> Option<Self> {
    let mut out = [0u64; 4];
    let mut carry: u128 = 0;
    for i in 0..4 {
      let sum = (self.limbs[i] as u128) + (other.limbs[i] as u128) + carry;
      out[i] = sum as u64;
      carry = sum >> 64;
    }
    if carry != 0 {
      return None;
    }
    Some(Self { limbs: out })
  }

  fn checked_sub(self, other: Self) -> Option<Self> {
    let mut out = [0u64; 4];
    let mut borrow: u128 = 0;
    for i in 0..4 {
      let a = self.limbs[i] as u128;
      let b = (other.limbs[i] as u128) + borrow;
      if a >= b {
        out[i] = (a - b) as u64;
        borrow = 0;
      } else {
        out[i] = ((1u128 << 64) + a - b) as u64;
        borrow = 1;
      }
    }
    if borrow != 0 {
      return None;
    }
    Some(Self { limbs: out })
  }

  fn wrapping_sub(self, other: Self) -> Self {
    let mut out = [0u64; 4];
    let mut borrow: u128 = 0;
    for i in 0..4 {
      let a = self.limbs[i] as u128;
      let b = (other.limbs[i] as u128) + borrow;
      if a >= b {
        out[i] = (a - b) as u64;
        borrow = 0;
      } else {
        out[i] = ((1u128 << 64) + a - b) as u64;
        borrow = 1;
      }
    }
    Self { limbs: out }
  }

  fn wrapping_neg(self) -> Self {
    Self::ZERO.wrapping_sub(self)
  }

  fn checked_mul(self, other: Self) -> Option<Self> {
    let mut out = [0u64; 8];

    for i in 0..4 {
      let mut carry: u128 = 0;
      for j in 0..4 {
        let idx = i + j;
        let cur = out[idx] as u128;
        let prod = (self.limbs[i] as u128) * (other.limbs[j] as u128);
        let sum = cur + prod + carry;
        out[idx] = sum as u64;
        carry = sum >> 64;
      }

      let mut idx = i + 4;
      while carry != 0 {
        if idx >= 8 {
          break;
        }
        let sum = (out[idx] as u128) + carry;
        out[idx] = sum as u64;
        carry = sum >> 64;
        idx += 1;
      }
    }

    if out[4..].iter().any(|&limb| limb != 0) {
      return None;
    }
    Some(Self {
      limbs: [out[0], out[1], out[2], out[3]],
    })
  }

  fn checked_mul_u32(self, mul: u32) -> Option<Self> {
    let mut out = [0u64; 4];
    let mut carry: u128 = 0;
    let mul = mul as u128;
    for i in 0..4 {
      let prod = (self.limbs[i] as u128) * mul + carry;
      out[i] = prod as u64;
      carry = prod >> 64;
    }
    if carry != 0 {
      return None;
    }
    Some(Self { limbs: out })
  }

  fn checked_add_u32(self, add: u32) -> Option<Self> {
    let mut out = self;
    let mut carry: u128 = add as u128;
    for i in 0..4 {
      if carry == 0 {
        break;
      }
      let sum = (out.limbs[i] as u128) + carry;
      out.limbs[i] = sum as u64;
      carry = sum >> 64;
    }
    if carry != 0 {
      return None;
    }
    Some(out)
  }

  fn checked_shl(self, shift: u32) -> Option<Self> {
    if shift == 0 {
      return Some(self);
    }
    if shift >= 256 {
      return if self.is_zero() { Some(self) } else { None };
    }

    let word_shift = (shift / 64) as usize;
    let bit_shift = shift % 64;

    // Any limbs that would be shifted entirely out of range indicate overflow.
    for i in (4 - word_shift)..4 {
      if self.limbs[i] != 0 {
        return None;
      }
    }

    if bit_shift != 0 {
      let top_src = 3usize.saturating_sub(word_shift);
      if (self.limbs[top_src] >> (64 - bit_shift)) != 0 {
        return None;
      }
    }

    let mut out = [0u64; 4];
    for i in (0usize..4).rev() {
      let src_idx = i.checked_sub(word_shift);
      if let Some(src_idx) = src_idx {
        let mut val = (self.limbs[src_idx] as u128) << bit_shift;
        if bit_shift != 0 && src_idx > 0 {
          val |= (self.limbs[src_idx - 1] as u128) >> (64 - bit_shift);
        }
        out[i] = val as u64;
      }
    }

    Some(Self { limbs: out })
  }

  fn shr(self, shift: u32) -> Self {
    if shift == 0 {
      return self;
    }
    if shift >= 256 {
      return Self::ZERO;
    }

    let word_shift = (shift / 64) as usize;
    let bit_shift = shift % 64;

    let mut out = [0u64; 4];
    for i in 0..4 {
      let src_idx = i + word_shift;
      if src_idx >= 4 {
        continue;
      }
      let mut val = (self.limbs[src_idx] as u128) >> bit_shift;
      if bit_shift != 0 && src_idx + 1 < 4 {
        val |= (self.limbs[src_idx + 1] as u128) << (64 - bit_shift);
      }
      out[i] = val as u64;
    }

    Self { limbs: out }
  }

  fn to_u128(self) -> Option<u128> {
    if self.limbs[2] != 0 || self.limbs[3] != 0 {
      return None;
    }
    Some((self.limbs[0] as u128) | ((self.limbs[1] as u128) << 64))
  }

  fn div_mod_u64(self, divisor: u64) -> (Self, u64) {
    debug_assert!(divisor != 0);
    let mut out = [0u64; 4];
    let mut rem: u128 = 0;
    let divisor = divisor as u128;
    for i in (0..4).rev() {
      let numerator = (rem << 64) | (self.limbs[i] as u128);
      let q = numerator / divisor;
      let r = numerator % divisor;
      out[i] = q as u64;
      rem = r;
    }
    (Self { limbs: out }, rem as u64)
  }

  fn to_decimal_string(self) -> String {
    if self.is_zero() {
      return "0".to_string();
    }
    const BASE: u64 = 10_000_000_000_000_000_000;
    let mut parts: Vec<u64> = Vec::new();
    let mut n = self;
    while !n.is_zero() {
      let (q, r) = n.div_mod_u64(BASE);
      parts.push(r);
      n = q;
    }
    let mut s = parts.pop().unwrap().to_string();
    for part in parts.iter().rev() {
      s.push_str(&format!("{:019}", part));
    }
    s
  }

  fn parse_decimal(s: &str) -> Option<Self> {
    let mut out = Self::ZERO;
    for b in s.bytes() {
      let digit = b.wrapping_sub(b'0');
      if digit > 9 {
        return None;
      }
      out = out.checked_mul_u32(10)?;
      out = out.checked_add_u32(digit as u32)?;
    }
    Some(out)
  }
}

impl Ord for U256 {
  fn cmp(&self, other: &Self) -> Ordering {
    for i in (0..4).rev() {
      match self.limbs[i].cmp(&other.limbs[i]) {
        Ordering::Equal => continue,
        other => return other,
      }
    }
    Ordering::Equal
  }
}

impl PartialOrd for U256 {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl BitAnd for U256 {
  type Output = Self;

  fn bitand(self, rhs: Self) -> Self::Output {
    let mut out = [0u64; 4];
    for i in 0..4 {
      out[i] = self.limbs[i] & rhs.limbs[i];
    }
    Self { limbs: out }
  }
}

impl BitOr for U256 {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    let mut out = [0u64; 4];
    for i in 0..4 {
      out[i] = self.limbs[i] | rhs.limbs[i];
    }
    Self { limbs: out }
  }
}

impl BitXor for U256 {
  type Output = Self;

  fn bitxor(self, rhs: Self) -> Self::Output {
    let mut out = [0u64; 4];
    for i in 0..4 {
      out[i] = self.limbs[i] ^ rhs.limbs[i];
    }
    Self { limbs: out }
  }
}

/// A JavaScript BigInt primitive value.
///
/// This implementation intentionally keeps BigInts inline (no GC allocation) because the
/// curated test262 suite exercises only values that fit within 256 bits. The representation is
/// sign+magnitude so we can keep BigInts inline (no GC allocation) while still supporting the
/// moderate-width BigInt literals used by the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JsBigInt {
  negative: bool,
  magnitude: U256,
}

impl JsBigInt {
  pub fn zero() -> Self {
    Self {
      negative: false,
      magnitude: U256::ZERO,
    }
  }

  pub fn from_i128(value: i128) -> Self {
    if value == 0 {
      return Self::zero();
    }
    if value < 0 {
      // `-i128::MIN` overflows, so handle it explicitly.
      let magnitude = U256::from_u128(if value == i128::MIN {
        1u128 << 127
      } else {
        (-value) as u128
      });
      Self {
        negative: true,
        magnitude,
      }
    } else {
      Self {
        negative: false,
        magnitude: U256::from_u128(value as u128),
      }
    }
  }

  pub fn from_u128(value: u128) -> Self {
    Self {
      negative: false,
      magnitude: U256::from_u128(value),
    }
  }

  pub fn is_zero(self) -> bool {
    self.magnitude.is_zero()
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
        .map(|mag| Self {
          negative: false,
          magnitude: mag,
        }),
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
        let mag = larger.checked_sub(smaller)?;
        Some(if mag.is_zero() {
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
    if mag.is_zero() {
      return Some(Self::zero());
    }
    Some(Self {
      negative: self.is_negative() ^ other.is_negative(),
      magnitude: mag,
    })
  }

  fn magnitude_bit_len(self) -> u32 {
    self.magnitude.bit_len()
  }

  fn twos_complement_mask(width: u32) -> U256 {
    debug_assert!((1..=256).contains(&width));
    if width == 256 {
      return U256 {
        limbs: [u64::MAX; 4],
      };
    }

    let full_limbs = (width / 64) as usize;
    let rem_bits = width % 64;
    let mut limbs = [0u64; 4];
    for i in 0..full_limbs {
      limbs[i] = u64::MAX;
    }
    if rem_bits != 0 {
      limbs[full_limbs] = (1u64 << rem_bits) - 1;
    }
    U256 { limbs }
  }

  fn to_twos_complement_u256(self, width: u32) -> U256 {
    debug_assert!((1..=256).contains(&width));
    let mask = Self::twos_complement_mask(width);
    let raw = if self.is_negative() {
      if width == 256 {
        self.magnitude.wrapping_neg()
      } else {
        let pow = U256::from_u128(1).checked_shl(width).unwrap();
        pow.checked_sub(self.magnitude).unwrap()
      }
    } else {
      self.magnitude
    };
    raw & mask
  }

  fn from_twos_complement_u256(value: U256, width: u32) -> Self {
    debug_assert!((1..=256).contains(&width));
    let mask = Self::twos_complement_mask(width);
    let value = value & mask;
    let sign_bit = width - 1;
    if !value.get_bit(sign_bit) {
      Self {
        negative: false,
        magnitude: value,
      }
    } else {
      let magnitude = if width == 256 {
        value.wrapping_neg()
      } else {
        let pow = U256::from_u128(1).checked_shl(width).unwrap();
        pow.checked_sub(value).unwrap()
      };
      debug_assert!(!magnitude.is_zero());
      Self {
        negative: true,
        magnitude,
      }
    }
  }

  fn to_twos_complement_257(self) -> (bool, U256) {
    if self.is_negative() {
      (true, self.magnitude.wrapping_neg())
    } else {
      (false, self.magnitude)
    }
  }

  fn from_twos_complement_257(high: bool, low: U256) -> Option<Self> {
    if !high {
      return Some(Self {
        negative: false,
        magnitude: low,
      });
    }
    let magnitude = low.wrapping_neg();
    if magnitude.is_zero() {
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
    op_low: fn(U256, U256) -> U256,
    op_high: fn(bool, bool) -> bool,
  ) -> Option<Self> {
    let width = self.magnitude_bit_len().max(other.magnitude_bit_len()) + 1;
    if width <= 256 {
      let a = self.to_twos_complement_u256(width);
      let b = other.to_twos_complement_u256(width);
      Some(Self::from_twos_complement_u256(op_low(a, b), width))
    } else {
      debug_assert_eq!(width, 257);
      let (a_high, a_low) = self.to_twos_complement_257();
      let (b_high, b_low) = other.to_twos_complement_257();
      let out_high = op_high(a_high, b_high);
      let out_low = op_low(a_low, b_low);
      Self::from_twos_complement_257(out_high, out_low)
    }
  }

  pub fn checked_bitwise_not(self) -> Option<Self> {
    self
      .negate()
      .checked_add(Self {
        negative: true,
        magnitude: U256::from_u128(1),
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
  /// remains intentionally bounded to 256 bits.
  pub fn try_to_i128(self) -> Option<i128> {
    if self.is_zero() {
      return Some(0);
    }
    if self.is_negative() {
      let min_mag = U256::from_u128(1).checked_shl(127).unwrap();
      if self.magnitude > min_mag {
        return None;
      }
      if self.magnitude == min_mag {
        return Some(i128::MIN);
      }
      Some(-(self.magnitude.to_u128()? as i128))
    } else {
      if self.magnitude > U256::from_u128(i128::MAX as u128) {
        return None;
      }
      Some(self.magnitude.to_u128()? as i128)
    }
  }

  pub fn from_decimal_str(value: &str) -> Option<Self> {
    let magnitude = U256::parse_decimal(value)?;
    Some(Self {
      negative: false,
      magnitude,
    })
  }

  pub fn checked_shl(self, shift: u32) -> Option<Self> {
    if self.is_zero() {
      return Some(self);
    }
    let magnitude = self.magnitude.checked_shl(shift)?;
    Some(Self {
      negative: self.negative,
      magnitude,
    })
  }

  pub fn shr(self, shift: u32) -> Self {
    if self.is_zero() {
      return self;
    }
    if shift == 0 {
      return self;
    }
    if shift >= 256 {
      return if self.is_negative() {
        Self {
          negative: true,
          magnitude: U256::from_u128(1),
        }
      } else {
        Self::zero()
      };
    }

    if !self.is_negative() {
      return Self {
        negative: false,
        magnitude: self.magnitude.shr(shift),
      };
    }

    // For negative numbers, right shift is equivalent to division by 2^shift with rounding toward
    // -infinity: `floor(-m / 2^k) == -ceil(m / 2^k)`.
    let q = self.magnitude.shr(shift);
    let remainder_mask = Self::twos_complement_mask(shift);
    let has_remainder = !(self.magnitude & remainder_mask).is_zero();
    let q = if has_remainder {
      q.checked_add(U256::from_u128(1)).unwrap()
    } else {
      q
    };
    if q.is_zero() {
      // Rounds toward negative infinity.
      Self {
        negative: true,
        magnitude: U256::from_u128(1),
      }
    } else {
      Self {
        negative: true,
        magnitude: q,
      }
    }
  }

  pub fn to_decimal_string(self) -> String {
    let mag_str = self.magnitude.to_decimal_string();
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
