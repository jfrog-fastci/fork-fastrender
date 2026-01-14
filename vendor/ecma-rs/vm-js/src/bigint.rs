use crate::VmError;
use core::cmp::Ordering;
use core::fmt;

/// An arbitrary-precision JavaScript BigInt value (ECMA-262 BigInt type).
///
/// Representation: sign + magnitude, where the magnitude is a little-endian array of 32-bit limbs
/// (base 2^32).
///
/// ## OOM safety
///
/// BigInt operations can allocate buffers whose size is controlled by hostile JS input (parsing
/// huge literals, exponentiation, division, etc). All BigInt-internal allocations must therefore be
/// fallible (`try_reserve*`) so they surface as `VmError::OutOfMemory` instead of aborting the host
/// process.
#[derive(PartialEq, Eq, Hash)]
pub struct JsBigInt {
  negative: bool,
  limbs: Vec<u32>,
}

impl fmt::Debug for JsBigInt {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("JsBigInt")
      .field("negative", &self.is_negative())
      .field("limbs_len", &self.limbs.len())
      .finish()
  }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
  if a.len() != b.len() {
    return a.len().cmp(&b.len());
  }
  for (a, b) in a.iter().rev().zip(b.iter().rev()) {
    if a != b {
      return a.cmp(b);
    }
  }
  Ordering::Equal
}

impl Ord for JsBigInt {
  fn cmp(&self, other: &Self) -> Ordering {
    match (self.is_negative(), other.is_negative()) {
      (true, false) => Ordering::Less,
      (false, true) => Ordering::Greater,
      (false, false) => cmp_mag(&self.limbs, &other.limbs),
      (true, true) => cmp_mag(&other.limbs, &self.limbs),
    }
  }
}

impl PartialOrd for JsBigInt {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl JsBigInt {
  pub fn zero() -> Self {
    Self {
      negative: false,
      limbs: Vec::new(),
    }
  }

  pub fn is_zero(&self) -> bool {
    self.limbs.is_empty()
  }

  pub fn is_negative(&self) -> bool {
    self.negative && !self.is_zero()
  }

  pub(crate) fn heap_size_bytes(&self) -> usize {
    // Only count the BigInt-owned limb buffer (header lives inline in the heap slot table).
    self
      .limbs
      .capacity()
      .checked_mul(core::mem::size_of::<u32>())
      .unwrap_or(usize::MAX)
  }

  /// Estimated number of bytes required to store this BigInt's magnitude (limb payload only).
  ///
  /// This is intended for host-side size budgeting (e.g. structured clone implementations) without
  /// exposing the internal limb vector.
  #[inline]
  pub fn estimated_byte_len(&self) -> usize {
    self
      .limbs
      .len()
      .saturating_mul(core::mem::size_of::<u32>())
  }

  /// Fallible clone of this BigInt value.
  ///
  /// BigInt values can be arbitrarily large, so cloning must use fallible allocations to surface
  /// `VmError::OutOfMemory` instead of aborting the host process.
  pub fn try_clone(&self) -> Result<Self, VmError> {
    Self::from_mag(self.is_negative(), Self::vec_from_slice(&self.limbs)?)
  }

  fn from_mag(negative: bool, mut mag: Vec<u32>) -> Result<Self, VmError> {
    while mag.last().copied() == Some(0) {
      mag.pop();
    }
    let negative = negative && !mag.is_empty();

    // Avoid retaining excessive spare capacity when an operation cancels leading limbs.
    // (E.g. `(1n << 1_000_000n) - ((1n << 1_000_000n) - 1n) == 1n`.)
    //
    // This keeps GC heap accounting closer to the true magnitude size without relying on
    // infallible `shrink_to_fit`-style reallocations.
    let len = mag.len();
    if len == 0 {
      return Ok(Self::zero());
    }
    let cap = mag.capacity();
    let double_len = len.checked_mul(2).unwrap_or(usize::MAX);
    if cap > double_len {
      let mut trimmed: Vec<u32> = Vec::new();
      trimmed
        .try_reserve_exact(len)
        .map_err(|_| VmError::OutOfMemory)?;
      trimmed.extend_from_slice(&mag);
      mag = trimmed;
    }

    Ok(Self { negative, limbs: mag })
  }

  fn vec_from_slice(slice: &[u32]) -> Result<Vec<u32>, VmError> {
    let mut out: Vec<u32> = Vec::new();
    out
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(slice);
    Ok(out)
  }

  fn vec_with_len_zeroed(len: usize) -> Result<Vec<u32>, VmError> {
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;
    out.resize(len, 0);
    Ok(out)
  }

  pub(crate) fn copy(&self) -> Result<Self, VmError> {
    self.try_clone()
  }

  pub fn from_u128(value: u128) -> Result<Self, VmError> {
    if value == 0 {
      return Ok(Self::zero());
    }
    let limb_len = ((128 - value.leading_zeros() + 31) / 32) as usize;
    let mut limbs: Vec<u32> = Vec::new();
    limbs
      .try_reserve_exact(limb_len)
      .map_err(|_| VmError::OutOfMemory)?;
    let mut v = value;
    while v != 0 {
      limbs.push(v as u32);
      v >>= 32;
    }
    Self::from_mag(false, limbs)
  }

  pub fn from_i128(value: i128) -> Result<Self, VmError> {
    if value == 0 {
      return Ok(Self::zero());
    }
    let negative = value < 0;
    // `-i128::MIN` overflows, so handle it explicitly.
    let mag: u128 = if negative {
      if value == i128::MIN {
        1u128 << 127
      } else {
        (-value) as u128
      }
    } else {
      value as u128
    };
    let mut out = Self::from_u128(mag)?;
    if negative && !out.is_zero() {
      out.negative = true;
    }
    Ok(out)
  }

  pub(crate) fn try_to_i128(&self) -> Option<i128> {
    if self.is_zero() {
      return Some(0);
    }
    if self.limbs.len() > 4 {
      return None;
    }

    let mut mag: u128 = 0;
    for (i, limb) in self.limbs.iter().enumerate() {
      mag |= (*limb as u128) << (i * 32);
    }

    if self.is_negative() {
      // Allow i128::MIN explicitly.
      if mag == (1u128 << 127) {
        return Some(i128::MIN);
      }
      if mag > i128::MAX as u128 {
        return None;
      }
      Some(-(mag as i128))
    } else {
      if mag > i128::MAX as u128 {
        return None;
      }
      Some(mag as i128)
    }
  }

  pub(crate) fn bit_len(&self) -> u64 {
    let Some(&last) = self.limbs.last() else {
      return 0;
    };
    let hi = 32u32 - last.leading_zeros();
    ((self.limbs.len() - 1) as u64) * 32 + hi as u64
  }

  #[inline]
  pub(crate) fn limbs(&self) -> &[u32] {
    &self.limbs
  }

  fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    if a.len() != b.len() {
      return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
      match a[i].cmp(&b[i]) {
        Ordering::Equal => continue,
        other => return other,
      }
    }
    Ordering::Equal
  }

  pub(crate) fn cmp(&self, other: &Self) -> Ordering {
    match (self.is_negative(), other.is_negative()) {
      (false, false) => Self::cmp_mag(&self.limbs, &other.limbs),
      (true, true) => Self::cmp_mag(&other.limbs, &self.limbs),
      (true, false) => Ordering::Less,
      (false, true) => Ordering::Greater,
    }
  }

  fn add_mag(a: &[u32], b: &[u32]) -> Result<Vec<u32>, VmError> {
    let out_len = core::cmp::max(a.len(), b.len()) + 1;
    let mut out = Self::vec_with_len_zeroed(out_len)?;
    let mut carry: u64 = 0;
    for i in 0..out_len {
      let av = a.get(i).copied().unwrap_or(0) as u64;
      let bv = b.get(i).copied().unwrap_or(0) as u64;
      let sum = av + bv + carry;
      out[i] = sum as u32;
      carry = sum >> 32;
    }
    while out.last().copied() == Some(0) {
      out.pop();
    }
    Ok(out)
  }

  fn sub_mag(a: &[u32], b: &[u32]) -> Result<Vec<u32>, VmError> {
    // Computes a - b where a >= b (magnitudes).
    debug_assert!(Self::cmp_mag(a, b) != Ordering::Less);
    let mut out = Self::vec_with_len_zeroed(a.len())?;

    let mut borrow: i64 = 0;
    for i in 0..a.len() {
      let av = a[i] as i64;
      let bv = b.get(i).copied().unwrap_or(0) as i64;
      let mut v = av - bv - borrow;
      if v < 0 {
        v += 1i64 << 32;
        borrow = 1;
      } else {
        borrow = 0;
      }
      out[i] = v as u32;
    }
    debug_assert_eq!(borrow, 0);
    while out.last().copied() == Some(0) {
      out.pop();
    }
    Ok(out)
  }

  pub(crate) fn add(&self, other: &Self) -> Result<Self, VmError> {
    if self.is_negative() == other.is_negative() {
      return Self::from_mag(
        self.is_negative(),
        Self::add_mag(&self.limbs, &other.limbs)?,
      );
    }

    // Mixed signs: subtract magnitudes.
    match Self::cmp_mag(&self.limbs, &other.limbs) {
      Ordering::Greater | Ordering::Equal => Self::from_mag(
        self.is_negative(),
        Self::sub_mag(&self.limbs, &other.limbs)?,
      ),
      Ordering::Less => Self::from_mag(
        other.is_negative(),
        Self::sub_mag(&other.limbs, &self.limbs)?,
      ),
    }
  }

  pub(crate) fn sub(&self, other: &Self) -> Result<Self, VmError> {
    if self.is_negative() != other.is_negative() {
      // a - (-b) == a + b (and vice versa)
      return Self::from_mag(
        self.is_negative(),
        Self::add_mag(&self.limbs, &other.limbs)?,
      );
    }

    // Same sign: subtract magnitudes, flipping sign if the rhs magnitude is larger.
    match Self::cmp_mag(&self.limbs, &other.limbs) {
      Ordering::Greater | Ordering::Equal => Self::from_mag(
        self.is_negative(),
        Self::sub_mag(&self.limbs, &other.limbs)?,
      ),
      Ordering::Less => Self::from_mag(
        !self.is_negative(),
        Self::sub_mag(&other.limbs, &self.limbs)?,
      ),
    }
  }

  pub(crate) fn neg(&self) -> Result<Self, VmError> {
    Self::from_mag(!self.is_negative(), Self::vec_from_slice(&self.limbs)?)
  }

  /// Negates this BigInt value without allocating.
  ///
  /// This consumes the value, mirroring the older `vm-js` `JsBigInt::negate()` API used by some
  /// host-side conversion code.
  pub fn negate(mut self) -> Self {
    if !self.is_zero() {
      self.negative = !self.negative;
    }
    self
  }

  fn mul_mag_with_tick(
    a: &[u32],
    b: &[u32],
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Vec<u32>, VmError> {
    if a.is_empty() || b.is_empty() {
      return Ok(Vec::new());
    }
    let out_len = a
      .len()
      .checked_add(b.len())
      .ok_or(VmError::OutOfMemory)?;
    let mut out = Self::vec_with_len_zeroed(out_len)?;

    const TICK_EVERY_OUTER: usize = 32;
    const TICK_EVERY_INNER: usize = 1024;
    for i in 0..a.len() {
      if i % TICK_EVERY_OUTER == 0 {
        tick()?;
      }
      let av = a[i] as u64;
      let mut carry: u64 = 0;
      for j in 0..b.len() {
        if j % TICK_EVERY_INNER == 0 {
          tick()?;
        }
        let idx = i + j;
        let cur = out[idx] as u64;
        let prod = av * (b[j] as u64) + cur + carry;
        out[idx] = prod as u32;
        carry = prod >> 32;
      }
      let mut idx = i + b.len();
      while carry != 0 {
        let cur = out[idx] as u64;
        let sum = cur + carry;
        out[idx] = sum as u32;
        carry = sum >> 32;
        idx += 1;
      }
    }

    while out.last().copied() == Some(0) {
      out.pop();
    }
    Ok(out)
  }

  pub(crate) fn mul_with_tick(
    &self,
    other: &Self,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Self, VmError> {
    let mag = Self::mul_mag_with_tick(&self.limbs, &other.limbs, tick)?;
    Self::from_mag(self.is_negative() ^ other.is_negative(), mag)
  }

  fn shl_mag(a: &[u32], shift: u64) -> Result<Vec<u32>, VmError> {
    if a.is_empty() {
      return Ok(Vec::new());
    }
    if shift == 0 {
      return Self::vec_from_slice(a);
    }

    let word_shift = (shift / 32) as usize;
    let bit_shift = (shift % 32) as u32;
    let out_len = a
      .len()
      .checked_add(word_shift)
      .and_then(|n| if bit_shift == 0 { Some(n) } else { n.checked_add(1) })
      .ok_or(VmError::OutOfMemory)?;
    let mut out = Self::vec_with_len_zeroed(out_len)?;

    let mut carry: u64 = 0;
    for i in 0..a.len() {
      let v = ((a[i] as u64) << bit_shift) | carry;
      out[i + word_shift] = v as u32;
      carry = v >> 32;
    }
    if bit_shift != 0 {
      out[a.len() + word_shift] = carry as u32;
    }

    while out.last().copied() == Some(0) {
      out.pop();
    }
    Ok(out)
  }

  fn shr_mag(a: &[u32], shift: u64) -> Result<Vec<u32>, VmError> {
    if a.is_empty() {
      return Ok(Vec::new());
    }
    if shift == 0 {
      return Self::vec_from_slice(a);
    }

    let word_shift = (shift / 32) as usize;
    let bit_shift = (shift % 32) as u32;
    if word_shift >= a.len() {
      return Ok(Vec::new());
    }
    let out_len = a.len() - word_shift;
    let mut out = Self::vec_with_len_zeroed(out_len)?;

    if bit_shift == 0 {
      out.copy_from_slice(&a[word_shift..]);
    } else {
      let mask = (1u32 << bit_shift) - 1;
      let mut carry: u32 = 0;
      for i in (word_shift..a.len()).rev() {
        let limb = a[i];
        let v = (limb >> bit_shift) | (carry << (32 - bit_shift));
        out[i - word_shift] = v;
        carry = limb & mask;
      }
    }

    while out.last().copied() == Some(0) {
      out.pop();
    }
    Ok(out)
  }

  pub(crate) fn shl(&self, shift: u64) -> Result<Self, VmError> {
    Self::from_mag(self.is_negative(), Self::shl_mag(&self.limbs, shift)?)
  }

  pub(crate) fn shr(&self, shift: u64) -> Result<Self, VmError> {
    if self.is_zero() {
      return Ok(Self::zero());
    }
    if shift == 0 {
      return self.copy();
    }

    if !self.is_negative() {
      return Self::from_mag(false, Self::shr_mag(&self.limbs, shift)?);
    }

    // Arithmetic shift for negatives:
    // floor(-m / 2^k) == -ceil(m / 2^k).
    let mut q = Self::shr_mag(&self.limbs, shift)?;

    // Determine whether any discarded bits were non-zero (i.e. whether `m % 2^k != 0`).
    let mut has_remainder = false;
    let word_shift = (shift / 32) as usize;
    let bit_shift = (shift % 32) as u32;
    if word_shift >= self.limbs.len() {
      has_remainder = true;
    } else {
      for &limb in &self.limbs[..word_shift] {
        if limb != 0 {
          has_remainder = true;
          break;
        }
      }
      if !has_remainder && bit_shift != 0 {
        let mask = (1u32 << bit_shift) - 1;
        if (self.limbs[word_shift] & mask) != 0 {
          has_remainder = true;
        }
      }
    }

    if has_remainder {
      q = Self::add_mag(&q, &[1])?;
    }
    Self::from_mag(true, q)
  }

  fn get_bit_mag(mag: &[u32], bit: u64) -> bool {
    let limb = (bit / 32) as usize;
    let offset = (bit % 32) as u32;
    mag.get(limb).map_or(false, |v| (*v & (1u32 << offset)) != 0)
  }

  fn set_bit_mag(mag: &mut [u32], bit: u64) {
    let limb = (bit / 32) as usize;
    let offset = (bit % 32) as u32;
    if let Some(v) = mag.get_mut(limb) {
      *v |= 1u32 << offset;
    }
  }

  fn div_mod_mag_with_tick(
    dividend: &[u32],
    divisor: &[u32],
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(Vec<u32>, Vec<u32>), VmError> {
    if divisor.is_empty() {
      return Err(VmError::InvariantViolation("BigInt division by zero"));
    }
    if dividend.is_empty() {
      return Ok((Vec::new(), Vec::new()));
    }
    if Self::cmp_mag(dividend, divisor) == Ordering::Less {
      return Ok((Vec::new(), Self::vec_from_slice(dividend)?));
    }

    let n_bits = {
      let Some(&last) = dividend.last() else {
        return Ok((Vec::new(), Vec::new()));
      };
      let hi = 32u32 - last.leading_zeros();
      ((dividend.len() - 1) as u64) * 32 + hi as u64
    };

    let q_len = ((n_bits + 31) / 32) as usize;
    let mut quotient = Self::vec_with_len_zeroed(q_len)?;

    // Remainder is always < divisor; allocate proportional to divisor size.
    let mut rem: Vec<u32> = Vec::new();
    rem
      .try_reserve_exact(divisor.len().saturating_add(1))
      .map_err(|_| VmError::OutOfMemory)?;

    const TICK_EVERY: u64 = 256;
    for bit in (0..n_bits).rev() {
      if bit % TICK_EVERY == 0 {
        tick()?;
      }

      // rem <<= 1
      if !rem.is_empty() {
        let mut carry: u64 = 0;
        for limb in rem.iter_mut() {
          let v = ((*limb as u64) << 1) | carry;
          *limb = v as u32;
          carry = v >> 32;
        }
        if carry != 0 {
          if rem.len() == rem.capacity() {
            rem
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
          }
          rem.push(carry as u32);
        }
      }

      // Add next dividend bit into remainder.
      if Self::get_bit_mag(dividend, bit) {
        if rem.is_empty() {
          if rem.len() == rem.capacity() {
            rem
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
          }
          rem.push(1);
        } else {
          rem[0] |= 1;
        }
      }

      while rem.last().copied() == Some(0) {
        rem.pop();
      }

      if !rem.is_empty() && Self::cmp_mag(&rem, divisor) != Ordering::Less {
        rem = Self::sub_mag(&rem, divisor)?;
        Self::set_bit_mag(&mut quotient, bit);
      }
    }

    while quotient.last().copied() == Some(0) {
      quotient.pop();
    }

    Ok((quotient, rem))
  }

  pub(crate) fn div_mod_with_tick(
    &self,
    other: &Self,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(Self, Self), VmError> {
    if other.is_zero() {
      return Err(VmError::InvariantViolation("BigInt division by zero"));
    }
    if self.is_zero() {
      return Ok((Self::zero(), Self::zero()));
    }

    let (q_mag, r_mag) = Self::div_mod_mag_with_tick(&self.limbs, &other.limbs, tick)?;

    let q = Self::from_mag(self.is_negative() ^ other.is_negative(), q_mag)?;
    let r = Self::from_mag(self.is_negative(), r_mag)?;
    Ok((q, r))
  }

  pub(crate) fn pow_with_tick(
    &self,
    exponent: &Self,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Self, VmError> {
    debug_assert!(!exponent.is_negative());

    if exponent.is_zero() {
      return Self::from_u128(1);
    }
    if self.is_zero() {
      return Ok(Self::zero());
    }

    // Special-cases that avoid allocating enormous intermediate values for huge exponents.
    if self.limbs.len() == 1 && self.limbs[0] == 1 && !self.is_negative() {
      return Self::from_u128(1);
    }
    if self.limbs.len() == 1 && self.limbs[0] == 1 && self.is_negative() {
      // (-1n) ** e is +/-1 depending on parity.
      let is_odd = (exponent.limbs.first().copied().unwrap_or(0) & 1) != 0;
      return if is_odd {
        Self::from_i128(-1)
      } else {
        Self::from_u128(1)
      };
    }

    // Exponentiation by squaring.
    let mut exp_mag = Self::vec_from_slice(&exponent.limbs)?;
    let mut base = self.copy()?;
    let mut result = Self::from_u128(1)?;

    const TICK_EVERY: usize = 32;
    let mut steps: usize = 0;
    while !exp_mag.is_empty() {
      if steps % TICK_EVERY == 0 {
        tick()?;
      }
      steps += 1;

      // If exponent is odd, result *= base.
      if (exp_mag[0] & 1) != 0 {
        result = result.mul_with_tick(&base, tick)?;
      }

      // exp_mag >>= 1
      let mut carry: u32 = 0;
      for limb in exp_mag.iter_mut().rev() {
        let new_carry = (*limb & 1) << 31;
        *limb = (*limb >> 1) | carry;
        carry = new_carry;
      }
      while exp_mag.last().copied() == Some(0) {
        exp_mag.pop();
      }

      if exp_mag.is_empty() {
        break;
      }
      base = base.mul_with_tick(&base, tick)?;
    }

    Ok(result)
  }

  fn div_mod_u32(mut n: Vec<u32>, divisor: u32) -> Result<(Vec<u32>, u32), VmError> {
    debug_assert!(divisor != 0);
    if n.is_empty() {
      return Ok((Vec::new(), 0));
    }
    let mut rem: u64 = 0;
    for limb in n.iter_mut().rev() {
      let cur = (rem << 32) | (*limb as u64);
      let q = cur / (divisor as u64);
      let r = cur % (divisor as u64);
      *limb = q as u32;
      rem = r;
    }
    while n.last().copied() == Some(0) {
      n.pop();
    }
    Ok((n, rem as u32))
  }

  pub fn to_string_radix_with_tick(
    &self,
    radix: u32,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<String, VmError> {
    debug_assert!((2..=36).contains(&radix));

    if self.is_zero() {
      let mut out = String::new();
      out.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
      out.push('0');
      return Ok(out);
    }

    // Copy magnitude.
    let mut mag = Self::vec_from_slice(&self.limbs)?;

    let mut digits: Vec<u8> = Vec::new();

    const TICK_EVERY: usize = 256;
    let mut steps: usize = 0;
    while !mag.is_empty() {
      if steps % TICK_EVERY == 0 {
        tick()?;
      }
      steps += 1;

      let (q, r) = Self::div_mod_u32(mag, radix)?;
      mag = q;
      let ch = match r {
        0..=9 => b'0' + (r as u8),
        10..=35 => b'a' + ((r - 10) as u8),
        _ => return Err(VmError::InvariantViolation("BigInt toString digit out of range")),
      };
      if digits.len() == digits.capacity() {
        digits
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
      }
      digits.push(ch);
    }

    digits.reverse();
    if self.is_negative() {
      if digits.len() == digits.capacity() {
        digits
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
      }
      digits.insert(0, b'-');
    }

    // `digits` contains only ASCII [0-9a-z] plus an optional leading '-', so it must always be
    // valid UTF-8. Avoid panicking on invariant violations.
    String::from_utf8(digits)
      .map_err(|_| VmError::InvariantViolation("BigInt toString produced non-UTF-8 digits"))
  }

  fn pow2_mag(bits: u64) -> Result<Vec<u32>, VmError> {
    // Produces magnitude for 2^bits.
    let limb = (bits / 32) as usize;
    let offset = (bits % 32) as u32;
    let len = limb.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let mut out = Self::vec_with_len_zeroed(len)?;
    out[limb] = 1u32 << offset;
    Ok(out)
  }

  pub(crate) fn as_uint_n(&self, bits: u64) -> Result<Self, VmError> {
    if bits == 0 {
      return Ok(Self::zero());
    }

    let limb_bits = 32u64;
    let full_limbs = (bits / limb_bits) as usize;
    let rem_bits = (bits % limb_bits) as u32;
    let out_len = if rem_bits == 0 {
      full_limbs
    } else {
      full_limbs + 1
    };

    // Compute m = abs(self) mod 2^bits by truncating limbs.
    let take = core::cmp::min(out_len, self.limbs.len());
    let mut m = Self::vec_from_slice(&self.limbs[..take])?;
    if rem_bits != 0 && !m.is_empty() {
      let mask = (1u32 << rem_bits) - 1;
      let idx = out_len - 1;
      if let Some(limb) = m.get_mut(idx) {
        *limb &= mask;
      }
    }
    while m.last().copied() == Some(0) {
      m.pop();
    }

    if !self.is_negative() {
      return Self::from_mag(false, m);
    }

    // Negative: result = 0 if m == 0 else 2^bits - m.
    if m.is_empty() {
      return Ok(Self::zero());
    }

    let pow = Self::pow2_mag(bits)?;
    Self::from_mag(false, Self::sub_mag(&pow, &m)?)
  }

  pub(crate) fn as_int_n(&self, bits: u64) -> Result<Self, VmError> {
    if bits == 0 {
      return Ok(Self::zero());
    }

    let unsigned = self.as_uint_n(bits)?;
    let sign_bit = bits - 1;
    if !Self::get_bit_mag(&unsigned.limbs, sign_bit) {
      return Ok(unsigned);
    }

    // Negative: unsigned - 2^bits.
    let pow = Self::from_mag(false, Self::pow2_mag(bits)?)?;
    unsigned.sub(&pow)
  }

  pub(crate) fn bitwise_not(&self) -> Result<Self, VmError> {
    // ~x == -(x + 1)
    let one = Self::from_u128(1)?;
    let tmp = self.add(&one)?;
    tmp.neg()
  }

  fn to_twos_complement(&self, width: u64) -> Result<Vec<u32>, VmError> {
    debug_assert!(width >= 1);
    let limbs_len = ((width + 31) / 32) as usize;
    let mut out = Self::vec_with_len_zeroed(limbs_len)?;

    let take = core::cmp::min(limbs_len, self.limbs.len());
    out[..take].copy_from_slice(&self.limbs[..take]);

    if self.is_negative() {
      for limb in out.iter_mut() {
        *limb = !*limb;
      }
      // +1
      let mut carry: u64 = 1;
      for limb in out.iter_mut() {
        if carry == 0 {
          break;
        }
        let sum = (*limb as u64) + carry;
        *limb = sum as u32;
        carry = sum >> 32;
      }
    }

    // Mask off unused high bits.
    let rem_bits = (width % 32) as u32;
    if rem_bits != 0 {
      let mask = (1u32 << rem_bits) - 1;
      if let Some(last) = out.last_mut() {
        *last &= mask;
      }
    }

    Ok(out)
  }

  fn from_twos_complement(mut value: Vec<u32>, width: u64) -> Result<Self, VmError> {
    debug_assert!(width >= 1);
    let sign_bit = width - 1;
    let is_negative = Self::get_bit_mag(&value, sign_bit);
    if !is_negative {
      while value.last().copied() == Some(0) {
        value.pop();
      }
      return Self::from_mag(false, value);
    }

    // Convert back: magnitude = (~value + 1) masked to width bits.
    for limb in value.iter_mut() {
      *limb = !*limb;
    }
    let mut carry: u64 = 1;
    for limb in value.iter_mut() {
      if carry == 0 {
        break;
      }
      let sum = (*limb as u64) + carry;
      *limb = sum as u32;
      carry = sum >> 32;
    }

    let rem_bits = (width % 32) as u32;
    if rem_bits != 0 {
      let mask = (1u32 << rem_bits) - 1;
      if let Some(last) = value.last_mut() {
        *last &= mask;
      }
    }
    while value.last().copied() == Some(0) {
      value.pop();
    }
    Self::from_mag(true, value)
  }

  pub(crate) fn bitwise_and(&self, other: &Self) -> Result<Self, VmError> {
    self.bitwise_binary_op(other, |a, b| a & b)
  }

  pub(crate) fn bitwise_or(&self, other: &Self) -> Result<Self, VmError> {
    self.bitwise_binary_op(other, |a, b| a | b)
  }

  pub(crate) fn bitwise_xor(&self, other: &Self) -> Result<Self, VmError> {
    self.bitwise_binary_op(other, |a, b| a ^ b)
  }

  fn bitwise_binary_op(
    &self,
    other: &Self,
    op: fn(u32, u32) -> u32,
  ) -> Result<Self, VmError> {
    let width = core::cmp::max(self.bit_len(), other.bit_len())
      .checked_add(1)
      .ok_or(VmError::OutOfMemory)?;
    let a = self.to_twos_complement(width)?;
    let b = other.to_twos_complement(width)?;
    debug_assert_eq!(a.len(), b.len());

    let mut out = a;
    for (o, b) in out.iter_mut().zip(b.iter()) {
      *o = op(*o, *b);
    }
    Self::from_twos_complement(out, width)
  }

  /// Converts this BigInt to an IEEE-754 binary64 (`f64`) using round-to-nearest, ties-to-even.
  ///
  /// This matches ECMAScript `BigInt::toNumber` semantics used by `Number(x)` / `new Number(x)`.
  ///
  /// Spec: <https://tc39.es/ecma262/#sec-bigint::tonumber>
  pub(crate) fn to_f64_round_ties_to_even(&self) -> f64 {
    if self.is_zero() {
      // BigInt has no -0; `-0n` canonicalizes to `0n`.
      return 0.0;
    }

    let negative = self.is_negative();
    let mag = &self.limbs;

    // `bit_len` is the minimal k such that 2^(k-1) <= |x| < 2^k.
    let bit_len = self.bit_len();

    // Any BigInt with |x| >= 2^1024 overflows the finite range of f64 and converts to Infinity.
    if bit_len > 1024 {
      return if negative { f64::NEG_INFINITY } else { f64::INFINITY };
    }

    // Integers up to 53 bits are exactly representable in binary64.
    if bit_len <= 53 {
      let mut v: u64 = 0;
      for (i, limb) in mag.iter().enumerate() {
        v |= (*limb as u64) << (i * 32);
      }
      let out = v as f64;
      return if negative { -out } else { out };
    }

    // Extract the top 53 bits (the implicit leading 1 + 52-bit mantissa payload).
    let shift = bit_len - 53;
    debug_assert!(shift >= 1);
    let word_shift = (shift / 32) as usize;
    let bit_shift = (shift % 32) as u32;

    // Read enough limbs to cover the shifted window (up to 53 bits).
    let mut chunk: u128 = 0;
    for i in 0..3usize {
      if let Some(&limb) = mag.get(word_shift + i) {
        chunk |= (limb as u128) << (i * 32);
      }
    }
    let mut mantissa: u64 = if bit_shift == 0 {
      chunk as u64
    } else {
      (chunk >> bit_shift) as u64
    };

    // Determine the rounding direction using the "guard + sticky" technique.
    //
    // - guard: the most significant discarded bit.
    // - sticky: OR of all less significant discarded bits.
    let guard = Self::get_bit_mag(mag, shift - 1);
    let mut sticky = false;
    if shift > 1 {
      let sticky_bits = shift - 1;
      let full_limbs = (sticky_bits / 32) as usize;
      let rem_bits = (sticky_bits % 32) as u32;

      // Any non-zero limb below the partial limb makes the remainder non-zero.
      for &limb in mag.iter().take(full_limbs) {
        if limb != 0 {
          sticky = true;
          break;
        }
      }

      if !sticky && rem_bits != 0 {
        if let Some(&limb) = mag.get(full_limbs) {
          let mask = (1u32 << rem_bits) - 1;
          if (limb & mask) != 0 {
            sticky = true;
          }
        }
      }
    }

    // Round to nearest, ties to even.
    if guard && (sticky || (mantissa & 1) == 1) {
      mantissa = mantissa.wrapping_add(1);
    }

    // Normalize if rounding overflowed the 53-bit mantissa window.
    let mut exponent = bit_len - 1;
    if mantissa == (1u64 << 53) {
      mantissa >>= 1;
      exponent += 1;
      // Exponent overflow produces Infinity.
      if exponent > 1023 {
        return if negative { f64::NEG_INFINITY } else { f64::INFINITY };
      }
    }

    debug_assert!((1u64 << 52) <= mantissa && mantissa < (1u64 << 53));
    debug_assert!(exponent <= 1023);

    let sign_bit = if negative { 1u64 << 63 } else { 0 };
    let exp_bits = (exponent + 1023) << 52;
    let frac_bits = mantissa & ((1u64 << 52) - 1);
    f64::from_bits(sign_bit | exp_bits | frac_bits)
  }

  pub fn from_f64_exact(n: f64) -> Result<Option<Self>, VmError> {
    if !n.is_finite() {
      return Ok(None);
    }
    if n == 0.0 {
      return Ok(Some(Self::zero()));
    }
    if n.fract() != 0.0 {
      return Ok(None);
    }

    let bits = n.to_bits();
    let sign = (bits >> 63) != 0;
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);

    if exp_bits == 0 {
      // Subnormal non-zero numbers are not integral.
      return Ok(None);
    }

    let exp = exp_bits - 1023;
    let mantissa = (1u64 << 52) | frac;
    let shift = exp - 52;

    let mut mag = Self::from_u128(mantissa as u128)?;
    if shift >= 0 {
      mag = mag.shl(shift as u64)?;
    } else {
      // Right shift, exact because n is integral.
      let r = (-shift) as u64;
      mag = mag.shr(r)?;
    }

    if sign {
      mag = mag.neg()?;
    }
    Ok(Some(mag))
  }

  pub(crate) fn parse_ascii_radix_with_tick(
    s: &str,
    radix: u32,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Self, VmError> {
    debug_assert!((2..=36).contains(&radix));
    if s.is_empty() {
      return Ok(Self::zero());
    }

    let mut mag: Vec<u32> = Vec::new();
    // Pre-reserve an estimate: bits ~= len * log2(radix).
    let est_bits = (s.len() as u64)
      .saturating_mul((radix as f64).log2().ceil() as u64)
      .saturating_add(1);
    let est_limbs = ((est_bits + 31) / 32) as usize;
    if est_limbs != 0 {
      mag
        .try_reserve_exact(est_limbs)
        .map_err(|_| VmError::OutOfMemory)?;
    }

    const TICK_EVERY: usize = 1024;
    for (i, b) in s.bytes().enumerate() {
      if i % TICK_EVERY == 0 {
        tick()?;
      }
      let digit = match b {
        b'0'..=b'9' => (b - b'0') as u32,
        b'a'..=b'z' => (b - b'a' + 10) as u32,
        b'A'..=b'Z' => (b - b'A' + 10) as u32,
        _ => return Err(VmError::TypeError("invalid BigInt digit")),
      };
      if digit >= radix {
        return Err(VmError::TypeError("invalid BigInt digit"));
      }

      // mag = mag * radix + digit
      let mut carry: u64 = digit as u64;
      for limb in mag.iter_mut() {
        let prod = (*limb as u64) * (radix as u64) + carry;
        *limb = prod as u32;
        carry = prod >> 32;
      }
      while carry != 0 {
        if mag.len() == mag.capacity() {
          mag.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        }
        mag.push(carry as u32);
        carry >>= 32;
      }
    }

    Self::from_mag(false, mag)
  }

  /// ECMAScript `StringToBigInt` parsing for a JS UTF-16 string.
  ///
  /// This is intended for `BigInt(value)` and other built-ins that accept string inputs.
  ///
  /// Returns `Ok(None)` if the input is not a valid BigInt string.
  pub fn parse_utf16_string_with_tick(
    units: &[u16],
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<Self>, VmError> {
    fn is_ecma_whitespace_u16(u: u16) -> bool {
      // Matches `ops::is_ecma_whitespace` but operates on BMP code points.
      matches!(
        u,
        0x0009 // Tab
        | 0x000A // LF
        | 0x000B // VT
        | 0x000C // FF
        | 0x000D // CR
        | 0x0020 // Space
        | 0x00A0 // No-break space
        | 0x1680 // Ogham space mark
        | 0x2000..=0x200A // En quad..hair space
        | 0x2028 // Line separator
        | 0x2029 // Paragraph separator
        | 0x202F // Narrow no-break space
        | 0x205F // Medium mathematical space
        | 0x3000 // Ideographic space
        | 0xFEFF // BOM
      )
    }

    // 1. Trim ECMAScript whitespace.
    let mut start = 0usize;
    let mut end = units.len();
    while start < end && is_ecma_whitespace_u16(units[start]) {
      start += 1;
    }
    while start < end && is_ecma_whitespace_u16(units[end - 1]) {
      end -= 1;
    }
    let units = &units[start..end];
    if units.is_empty() {
      // Per ECMA-262 `StringToBigInt`, an empty (or whitespace-only) string is treated as `0n`.
      return Ok(Some(Self::zero()));
    }

    // 2. Optional sign.
    let mut idx = 0usize;
    let mut negative = false;
    let mut has_sign = false;
    if units[0] == b'+' as u16 {
      idx = 1;
      has_sign = true;
    } else if units[0] == b'-' as u16 {
      idx = 1;
      negative = true;
      has_sign = true;
    }
    if idx >= units.len() {
      return Ok(None);
    }

    // 3. Radix prefix.
    let mut radix: u32 = 10;
    // Per ECMA-262 (mirroring `StringToNumber`), signed hex/binary/octal forms are not accepted:
    // `BigInt("-0x10")` throws.
    if !has_sign && units[idx] == b'0' as u16 && idx + 1 < units.len() {
      let prefix = units[idx + 1];
      if prefix == b'x' as u16 || prefix == b'X' as u16 {
        radix = 16;
        idx += 2;
      } else if prefix == b'o' as u16 || prefix == b'O' as u16 {
        radix = 8;
        idx += 2;
      } else if prefix == b'b' as u16 || prefix == b'B' as u16 {
        radix = 2;
        idx += 2;
      }
    }
    if idx >= units.len() {
      return Ok(None);
    }

    let digits = &units[idx..];

    // 4. Parse digits into magnitude.
    let mut mag: Vec<u32> = Vec::new();
    // Pre-reserve an estimate: bits ~= len * log2(radix).
    let est_bits = (digits.len() as u64)
      .saturating_mul((radix as f64).log2().ceil() as u64)
      .saturating_add(1);
    let est_limbs = ((est_bits + 31) / 32) as usize;
    if est_limbs != 0 {
      mag
        .try_reserve_exact(est_limbs)
        .map_err(|_| VmError::OutOfMemory)?;
    }

    const TICK_EVERY: usize = 1024;
    for (i, &u) in digits.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        tick()?;
      }
      let digit = match u {
        u if (b'0' as u16..=b'9' as u16).contains(&u) => (u - b'0' as u16) as u32,
        u if (b'a' as u16..=b'z' as u16).contains(&u) => (u - b'a' as u16 + 10) as u32,
        u if (b'A' as u16..=b'Z' as u16).contains(&u) => (u - b'A' as u16 + 10) as u32,
        _ => return Ok(None),
      };
      if digit >= radix {
        return Ok(None);
      }

      // mag = mag * radix + digit
      let mut carry: u64 = digit as u64;
      for limb in mag.iter_mut() {
        let prod = (*limb as u64) * (radix as u64) + carry;
        *limb = prod as u32;
        carry = prod >> 32;
      }
      while carry != 0 {
        if mag.len() == mag.capacity() {
          mag.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        }
        mag.push(carry as u32);
        carry >>= 32;
      }
    }

    Ok(Some(Self::from_mag(negative, mag)?))
  }
}
