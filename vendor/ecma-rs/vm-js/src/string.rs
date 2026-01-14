use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem;
use std::slice;

use unicode_normalization::UnicodeNormalization;

use crate::VmError;

/// A JavaScript String value.
///
/// Per ECMAScript, strings are sequences of UTF-16 code units and may contain
/// unpaired surrogate code units.
#[derive(Clone)]
pub struct JsString {
  units: Box<[u16]>,
  hash64: u64,
}

impl JsString {
  /// Creates a `JsString` by encoding a Rust UTF-8 `str` into UTF-16 code units.
  ///
  /// This is fallible (returns [`VmError::OutOfMemory`]) and should be preferred over
  /// infallible `String`/`Vec` allocation paths for attacker-controlled inputs.
  pub fn from_str(s: &str) -> Result<Self, VmError> {
    // `encode_utf16().count()` is O(n) but avoids an intermediate `Vec` allocation when
    // pre-sizing the buffer with fallible `try_reserve_exact`.
    let len = s.encode_utf16().count();
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(len)
      .map_err(|_| VmError::OutOfMemory)?;
    units.extend(s.encode_utf16());
    Self::from_u16_vec(units)
  }

  pub fn from_code_units(units: &[u16]) -> Result<Self, VmError> {
    // Avoid `units.to_vec()`, which allocates infallibly and can abort the host process on OOM.
    let mut buf: Vec<u16> = Vec::new();
    buf
      .try_reserve_exact(units.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend_from_slice(units);
    Self::from_u16_vec(buf)
  }

  pub fn from_u16_vec(mut units: Vec<u16>) -> Result<Self, VmError> {
    // Converting a `Vec<T>` into a `Box<[T]>` requires the backing allocation to be sized exactly
    // for `len` elements. `Vec::shrink_to_fit` / `Vec::into_boxed_slice` will perform this
    // reallocation infallibly (and abort the process on allocator OOM), so we need to handle spare
    // capacity explicitly using fallible allocations.

    let len = units.len();
    let cap = units.capacity();

    // Special-case empty strings: we can always represent these without any heap allocation.
    if len == 0 {
      units = Vec::new();
    } else if cap != len {
      // Allocate a trimmed buffer of exactly `len` code units using fallible `try_reserve_exact`.
      // This avoids infallible reallocations in `into_boxed_slice` under attacker-controlled sizes.
      let mut trimmed: Vec<u16> = Vec::new();
      trimmed
        .try_reserve_exact(len)
        .map_err(|_| VmError::OutOfMemory)?;
      trimmed.extend_from_slice(&units);
      units = trimmed;
    }

    debug_assert_eq!(
      units.len(),
      units.capacity(),
      "JsString::from_u16_vec must only box exact-capacity Vecs"
    );

    // Convert to `Box<[u16]>` without any further allocation.
    let units = vec_into_boxed_slice_exact(units);
    let hash64 = stable_hash64(units.as_ref());
    Ok(Self { units, hash64 })
  }

  pub fn len_code_units(&self) -> usize {
    self.units.len()
  }

  pub fn is_empty(&self) -> bool {
    self.units.is_empty()
  }

  pub fn as_code_units(&self) -> &[u16] {
    self.units.as_ref()
  }

  pub fn to_utf8_lossy(&self) -> String {
    // Avoid `String::from_utf16_lossy`: it may allocate infallibly and abort the host process on
    // allocator OOM. This is an infallible API surface (used primarily in tests / debug helpers),
    // so we fall back to an empty string if the fallible conversion cannot allocate.
    utf16_to_utf8_lossy(self.as_code_units()).unwrap_or_default()
  }

  pub fn stable_hash64(&self) -> u64 {
    self.hash64
  }

  pub(crate) fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_len(self.units.len())
  }

  pub(crate) fn heap_size_bytes_for_len(units_len: usize) -> usize {
    // Payload bytes owned by this string allocation.
    //
    // Note: `JsString` headers are stored inline in the heap slot table, so this size intentionally
    // excludes `mem::size_of::<JsString>()` and only counts the backing UTF-16 buffer.
    units_len.checked_mul(2).unwrap_or(usize::MAX)
  }
}

/// Converts a `Vec<u16>` into `Box<[u16]>` without allocation.
///
/// # Safety contract
///
/// This requires `v.len() == v.capacity()` so the slice layout used by `Box<[T]>` deallocation
/// matches the original `Vec` allocation layout. Callers must uphold this invariant.
fn vec_into_boxed_slice_exact(mut v: Vec<u16>) -> Box<[u16]> {
  debug_assert_eq!(v.len(), v.capacity());
  let len = v.len();
  let ptr = v.as_mut_ptr();
  mem::forget(v);
  // Safety: `ptr` came from a `Vec<u16>` allocation, and `len == capacity` ensures the allocation
  // layout matches `Box<[u16]>`'s slice layout.
  unsafe { Box::from_raw(slice::from_raw_parts_mut(ptr, len)) }
}

/// Fallible UTF-16→UTF-8 conversion for VM internals.
///
/// JavaScript strings are UTF-16 code units and may contain unpaired surrogates. Rust `String`
/// cannot represent surrogate code points, so this conversion mirrors `String::from_utf16_lossy`
/// by replacing invalid sequences with `U+FFFD`.
///
/// This helper must be used for attacker-controlled inputs that could be extremely large, since
/// the standard library's UTF-16 conversion routines may allocate infallibly and abort the
/// process on OOM.
#[allow(dead_code)]
pub(crate) fn utf16_to_utf8_lossy(units: &[u16]) -> Result<String, VmError> {
  utf16_to_utf8_lossy_with_tick(units, || Ok(()))
}

/// Fallible UTF-16→UTF-8 conversion with a hard cap on the number of UTF-16 code units converted.
///
/// This is intended for *host-facing* formatting paths (error messages, stack traces, etc), where a
/// script can surface attacker-controlled strings that are extremely large.
///
/// If `units` exceeds `max_code_units`, the output is truncated and an ellipsis (`…`) is appended.
pub(crate) fn utf16_to_utf8_lossy_truncated(
  units: &[u16],
  max_code_units: usize,
) -> Result<String, VmError> {
  utf16_to_utf8_lossy_truncated_with_tick(units, max_code_units, || Ok(()))
}

/// [`utf16_to_utf8_lossy_truncated`] with an optional `tick` hook.
pub(crate) fn utf16_to_utf8_lossy_truncated_with_tick(
  units: &[u16],
  max_code_units: usize,
  tick: impl FnMut() -> Result<(), VmError>,
) -> Result<String, VmError> {
  let truncated = units.len() > max_code_units;
  let units = if truncated {
    &units[..max_code_units]
  } else {
    units
  };

  let mut out = utf16_to_utf8_lossy_with_tick(units, tick)?;

  if truncated {
    let mut buf = [0u8; 4];
    let needed = '…'.encode_utf8(&mut buf).len();
    out.try_reserve(needed).map_err(|_| VmError::OutOfMemory)?;
    out.push('…');
  }

  Ok(out)
}

/// [`utf16_to_utf8_lossy`] with an optional `tick` hook.
///
/// The `tick` hook allows long conversions to observe VM budgets and host interrupts. Callers
/// should pass something like `|| vm.tick()` when converting attacker-controlled strings.
pub(crate) fn utf16_to_utf8_lossy_with_tick(
  units: &[u16],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<String, VmError> {
  const TICK_EVERY: usize = 1024;
  const RESERVE_CHUNK: usize = 8 * 1024;

  let mut out = String::new();

  let mut i = 0usize;
  while i < units.len() {
    if i % TICK_EVERY == 0 {
      tick()?;
    }

    let u = units[i];

    // Decode UTF-16, replacing invalid surrogate sequences with U+FFFD.
    let (code_point, consumed) = if (0xD800..=0xDBFF).contains(&u) {
      // High surrogate.
      if i + 1 < units.len() {
        let u2 = units[i + 1];
        if (0xDC00..=0xDFFF).contains(&u2) {
          let high = (u as u32) - 0xD800;
          let low = (u2 as u32) - 0xDC00;
          (0x10000 + ((high << 10) | low), 2)
        } else {
          (0xFFFD, 1)
        }
      } else {
        (0xFFFD, 1)
      }
    } else if (0xDC00..=0xDFFF).contains(&u) {
      // Unpaired low surrogate.
      (0xFFFD, 1)
    } else {
      (u as u32, 1)
    };
    i = i.saturating_add(consumed);

    let ch = char::from_u32(code_point).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    let needed = encoded.len();

    if out.capacity().saturating_sub(out.len()) < needed {
      out
        .try_reserve(RESERVE_CHUNK.max(needed))
        .map_err(|_| VmError::OutOfMemory)?;
    }
    out.push_str(encoded);
  }

  Ok(out)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NormalizationForm {
  Nfc,
  Nfd,
  Nfkc,
  Nfkd,
}

pub(crate) fn normalize_utf16_to_utf16_with_tick(
  units: &[u16],
  form: NormalizationForm,
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<Vec<u16>, VmError> {
  const TICK_EVERY: usize = 1024;
  const RESERVE_CHUNK: usize = 8 * 1024;

  fn normalize_segment(
    units: &[u16],
    form: NormalizationForm,
    out: &mut Vec<u16>,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    if units.is_empty() {
      return Ok(());
    }

    let utf8 = utf16_to_utf8_lossy_with_tick(units, || tick())?;

    macro_rules! normalize_iter {
      ($iter:expr) => {
        for (idx, ch) in ($iter).enumerate() {
          if idx % TICK_EVERY == 0 {
            tick()?;
          }

          let mut buf = [0u16; 2];
          let encoded = ch.encode_utf16(&mut buf);
          let needed = encoded.len();

          if out.capacity().saturating_sub(out.len()) < needed {
            out
              .try_reserve(RESERVE_CHUNK.max(needed))
              .map_err(|_| VmError::OutOfMemory)?;
          }
          out.extend_from_slice(encoded);
        }
      };
    }

    match form {
      NormalizationForm::Nfc => normalize_iter!(utf8.nfc()),
      NormalizationForm::Nfd => normalize_iter!(utf8.nfd()),
      NormalizationForm::Nfkc => normalize_iter!(utf8.nfkc()),
      NormalizationForm::Nfkd => normalize_iter!(utf8.nfkd()),
    }

    Ok(())
  }

  // Start with a conservative reservation: normalization can expand the string.
  let mut out: Vec<u16> = Vec::new();
  out.try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  // Normalize only scalar-value segments. Unpaired surrogate code points are left unchanged.
  let mut seg_start = 0usize;
  let mut i = 0usize;
  while i < units.len() {
    if i % TICK_EVERY == 0 {
      tick()?;
    }

    let u = units[i];

    // Surrogates: keep well-formed pairs inside the segment; flush on unpaired surrogate code points.
    let is_unpaired_surrogate = if (0xD800..=0xDBFF).contains(&u) {
      // High surrogate.
      if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
        i += 2;
        continue;
      }
      true
    } else if (0xDC00..=0xDFFF).contains(&u) {
      // Low surrogate without a preceding high surrogate.
      true
    } else {
      false
    };

    if is_unpaired_surrogate {
      normalize_segment(&units[seg_start..i], form, &mut out, &mut tick)?;
      // Preserve the unpaired surrogate code unit unchanged.
      if out.capacity().saturating_sub(out.len()) < 1 {
        out.try_reserve(RESERVE_CHUNK).map_err(|_| VmError::OutOfMemory)?;
      }
      out.push(u);
      i += 1;
      seg_start = i;
    } else {
      i += 1;
    }
  }
  normalize_segment(&units[seg_start..], form, &mut out, &mut tick)?;

  Ok(out)
}

/// [`utf16_to_utf8_lossy_with_tick`] with a hard UTF-8 output cap.
///
/// This is intended for best-effort formatting paths (error messages, stack traces) where:
/// - inputs are attacker-controlled and may be extremely large,
/// - host allocations must be fallible (no abort on OOM), and
/// - the output should be bounded to avoid allocating arbitrarily large host `String`s.
///
/// Returns `(string, truncated)` where `truncated` indicates whether the input was longer than the
/// cap and was therefore cut off early.
pub(crate) fn utf16_to_utf8_lossy_bounded_with_tick(
  units: &[u16],
  max_bytes: usize,
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<(String, bool), VmError> {
  const TICK_EVERY: usize = 1024;
  const RESERVE_CHUNK: usize = 8 * 1024;

  if max_bytes == 0 {
    return Ok((String::new(), !units.is_empty()));
  }

  let mut out = String::new();
  let mut truncated = false;

  let mut i = 0usize;
  while i < units.len() {
    if i % TICK_EVERY == 0 {
      tick()?;
    }

    let u = units[i];

    // Decode UTF-16, replacing invalid surrogate sequences with U+FFFD.
    let (code_point, consumed) = if (0xD800..=0xDBFF).contains(&u) {
      // High surrogate.
      if i + 1 < units.len() {
        let u2 = units[i + 1];
        if (0xDC00..=0xDFFF).contains(&u2) {
          let high = (u as u32) - 0xD800;
          let low = (u2 as u32) - 0xDC00;
          (0x10000 + ((high << 10) | low), 2)
        } else {
          (0xFFFD, 1)
        }
      } else {
        (0xFFFD, 1)
      }
    } else if (0xDC00..=0xDFFF).contains(&u) {
      // Unpaired low surrogate.
      (0xFFFD, 1)
    } else {
      (u as u32, 1)
    };
    i = i.saturating_add(consumed);

    let ch = char::from_u32(code_point).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    let needed = encoded.len();

    // Ensure we never exceed the cap; if the next code point doesn't fit, truncate.
    if out.len().saturating_add(needed) > max_bytes {
      truncated = true;
      break;
    }

    if out.capacity().saturating_sub(out.len()) < needed {
      // Reserve at most the remaining cap to avoid attempting huge allocations.
      let remaining = max_bytes.saturating_sub(out.len());
      let reserve = RESERVE_CHUNK.min(remaining).max(needed);
      out
        .try_reserve(reserve)
        .map_err(|_| VmError::OutOfMemory)?;
    }
    out.push_str(encoded);
  }

  Ok((out, truncated))
}

#[allow(dead_code)]
#[inline]
pub(crate) fn utf16_to_utf8_lossy_bounded(
  units: &[u16],
  max_bytes: usize,
) -> Result<(String, bool), VmError> {
  utf16_to_utf8_lossy_bounded_with_tick(units, max_bytes, || Ok(()))
}

impl PartialEq for JsString {
  fn eq(&self, other: &Self) -> bool {
    self.units == other.units
  }
}

impl Eq for JsString {}

impl Hash for JsString {
  fn hash<H: Hasher>(&self, state: &mut H) {
    // Hash the length and the code units. This keeps hashing:
    // - compatible with `Eq` (code-unit equality),
    // - resistant to trivial collision attacks (uses the map's keyed hasher),
    // - and deterministic across platforms.
    self.units.len().hash(state);
    for unit in self.units.iter() {
      unit.hash(state);
    }
  }
}

impl PartialOrd for JsString {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for JsString {
  fn cmp(&self, other: &Self) -> Ordering {
    self.units.as_ref().cmp(other.units.as_ref())
  }
}

impl fmt::Debug for JsString {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Rust `String` cannot represent lone surrogates; use a lossy conversion so
    // Debug never panics.
    f.debug_struct("JsString")
      .field("len_code_units", &self.len_code_units())
      .field("utf8_lossy", &self.to_utf8_lossy())
      .finish()
  }
}

const FNV_OFFSET_BASIS_64: u64 = 0xcbf29ce484222325;
const FNV_PRIME_64: u64 = 0x00000100000001B3;

fn stable_hash64(units: &[u16]) -> u64 {
  let mut hash = FNV_OFFSET_BASIS_64;
  for unit in units {
    for byte in unit.to_le_bytes() {
      hash ^= byte as u64;
      hash = hash.wrapping_mul(FNV_PRIME_64);
    }
  }
  hash
}
