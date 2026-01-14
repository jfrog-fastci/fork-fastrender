//! ECMAScript `RegExp` parsing + execution (partial).
//!
//! This is a small backtracking VM designed for `vm-js`:
//! - Operates over UTF-16 code units (JS string model).
//! - Provides explicit `vm.tick()` hooks in the backtracking loop so hostile patterns can be
//!   interrupted via fuel/deadline/interrupt budgets.
//! - Focuses on the subset needed for baseline real-world behaviour: literals, character classes,
//!   groups, alternation, quantifiers, `^`/`$`, `.`/dotAll, `\\b`/`\\B`, and basic lookahead.
//!
//! This module intentionally does **not** attempt to be a full spec implementation yet.
//! Unicode-heavy features in particular are still evolving (Unicode property escapes of strings,
//! complete `/v` UnicodeSets syntax, etc).
//! Call sites must treat compilation failures as `SyntaxError`.

use crate::fallible_alloc::box_try_new_vm;
use crate::regexp_unicode_property_strings::{match_property_at, MAX_MATCHES_PER_POSITION};
use crate::regexp_unicode_resolver::{resolve_unicode_property_value_expression, ResolvedUnicodeProperty};
use crate::regexp_unicode_tables::{
  contains_code_point as unicode_table_contains_code_point, BinaryProp, ResolvedCodePointProperty,
};
use crate::tick::{tick_every, DEFAULT_TICK_EVERY};
use crate::{Heap, HeapLimits, VmError};
use core::alloc::Layout;
use core::cell::Cell;
use core::mem;
use core::ptr;
use icu_casemap::{CaseMapperBorrowed, ClosureSink};
use std::alloc::alloc;

pub(crate) use crate::regexp_unicode_property_strings::UnicodeStringProperty;

#[cfg(test)]
mod unicode_string_property;
#[cfg(test)]
pub(crate) use unicode_string_property::resolve_unicode_string_property;

#[derive(Debug, Clone)]
pub(crate) struct RegExpSyntaxError {
  pub(crate) message: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) enum RegExpCompileError {
  Syntax(RegExpSyntaxError),
  OutOfMemory,
  /// VM termination / budget error observed during compilation.
  Vm(VmError),
}

impl From<RegExpSyntaxError> for RegExpCompileError {
  fn from(value: RegExpSyntaxError) -> Self {
    Self::Syntax(value)
  }
}

impl From<VmError> for RegExpCompileError {
  fn from(value: VmError) -> Self {
    match value {
      VmError::OutOfMemory => Self::OutOfMemory,
      other => Self::Vm(other),
    }
  }
}

/// Minimum non-zero capacity for compilation vectors that can grow due to hostile patterns.
const MIN_VEC_CAPACITY: usize = 1;

fn grown_capacity(current_capacity: usize, required_len: usize) -> usize {
  if required_len <= current_capacity {
    return current_capacity;
  }
  let mut cap = current_capacity.max(MIN_VEC_CAPACITY);
  while cap < required_len {
    cap = match cap.checked_mul(2) {
      Some(next) => next,
      None => return usize::MAX,
    };
  }
  cap
}

fn vec_capacity_growth_bytes<T>(current_capacity: usize, required_len: usize) -> usize {
  let elem_size = mem::size_of::<T>();
  if elem_size == 0 {
    return 0;
  }
  let new_capacity = grown_capacity(current_capacity, required_len);
  if new_capacity == usize::MAX {
    return usize::MAX;
  }
  new_capacity
    .saturating_sub(current_capacity)
    .saturating_mul(elem_size)
}

/// Shared compilation context for heap-limit accounting and fuel/deadline budgeting.
struct CompileCtx<'a> {
  heap_limits: HeapLimits,
  heap_base_bytes: usize,
  compiled_bytes: usize,
  tick: &'a mut dyn FnMut() -> Result<(), VmError>,
}

impl<'a> CompileCtx<'a> {
  fn new(heap: &Heap, tick: &'a mut dyn FnMut() -> Result<(), VmError>) -> Self {
    Self {
      heap_limits: heap.limits(),
      heap_base_bytes: heap.estimated_total_bytes(),
      compiled_bytes: 0,
      tick,
    }
  }

  #[inline]
  fn tick(&mut self) -> Result<(), RegExpCompileError> {
    (*self.tick)().map_err(RegExpCompileError::from)
  }

  #[inline]
  fn tick_every(&mut self, i: usize) -> Result<(), RegExpCompileError> {
    tick_every(i, DEFAULT_TICK_EVERY, &mut *self.tick).map_err(RegExpCompileError::from)
  }

  fn charge(&mut self, bytes: usize) -> Result<(), RegExpCompileError> {
    let after = self
      .heap_base_bytes
      .saturating_add(self.compiled_bytes)
      .saturating_add(bytes);
    if after > self.heap_limits.max_bytes {
      return Err(RegExpCompileError::OutOfMemory);
    }
    self.compiled_bytes = self.compiled_bytes.saturating_add(bytes);
    Ok(())
  }

  fn reserve_vec_to_len<T>(
    &mut self,
    vec: &mut Vec<T>,
    required_len: usize,
  ) -> Result<(), RegExpCompileError> {
    if required_len <= vec.capacity() {
      return Ok(());
    }
    let desired_capacity = grown_capacity(vec.capacity(), required_len);
    if desired_capacity == usize::MAX {
      return Err(RegExpCompileError::OutOfMemory);
    }

    let growth_bytes = vec_capacity_growth_bytes::<T>(vec.capacity(), desired_capacity);
    if growth_bytes == usize::MAX {
      return Err(RegExpCompileError::OutOfMemory);
    }
    if growth_bytes != 0 {
      self.charge(growth_bytes)?;
    }

    let additional = desired_capacity
      .checked_sub(vec.len())
      .ok_or(RegExpCompileError::OutOfMemory)?;
    vec
      .try_reserve_exact(additional)
      .map_err(|_| RegExpCompileError::OutOfMemory)?;
    Ok(())
  }

  fn vec_try_push<T>(&mut self, vec: &mut Vec<T>, value: T) -> Result<(), RegExpCompileError> {
    let required_len = vec
      .len()
      .checked_add(1)
      .ok_or(RegExpCompileError::OutOfMemory)?;
    self.reserve_vec_to_len(vec, required_len)?;
    vec.push(value);
    Ok(())
  }

  fn box_try_new<T>(&mut self, value: T) -> Result<Box<T>, RegExpCompileError> {
    let size = mem::size_of::<T>();
    if size == 0 {
      // `Box::new` does not allocate for ZSTs, so this cannot fail with OOM.
      return Ok(Box::new(value));
    }
    self.charge(size)?;

    let layout = Layout::new::<T>();
    // SAFETY: We allocate enough space for `T` and immediately initialise it before converting it
    // into a `Box<T>`.
    unsafe {
      let raw = alloc(layout) as *mut T;
      if raw.is_null() {
        return Err(RegExpCompileError::OutOfMemory);
      }
      ptr::write(raw, value);
      Ok(Box::from_raw(raw))
    }
  }
}

// --- RegExp `/v` (UnicodeSets mode) data model ---
//
// ECMAScript RegExp UnicodeSets mode extends character classes to support:
// - set operations (union / intersection / subtraction), and
// - string elements (e.g. `\q{...}`).
//
// The main engine currently operates over UTF-16 code units, so the “character” universe here is
// `u16` (0..=0xFFFF). This data model is intended for RegExp compilation and must therefore charge
// all dynamic allocations against `CompileCtx` to respect heap limits.

const CHARSET_WORDS: usize = 0x10000 / 64;

/// A set of UTF-16 code units (0..=0xFFFF).
#[derive(Clone, PartialEq, Eq)]
struct CharSet {
  bits: [u64; CHARSET_WORDS],
}

impl Default for CharSet {
  fn default() -> Self {
    Self {
      bits: [0u64; CHARSET_WORDS],
    }
  }
}

impl CharSet {
  #[inline]
  fn is_empty(&self) -> bool {
    self.bits.iter().all(|&w| w == 0)
  }

  #[inline]
  fn contains(&self, u: u16) -> bool {
    let idx = (u as usize) / 64;
    let bit = (u as usize) % 64;
    (self.bits[idx] & (1u64 << bit)) != 0
  }

  #[inline]
  fn insert(&mut self, u: u16) {
    let idx = (u as usize) / 64;
    let bit = (u as usize) % 64;
    self.bits[idx] |= 1u64 << bit;
  }

  #[inline]
  fn insert_range(&mut self, start: u16, end: u16) {
    if start > end {
      return;
    }
    let start = start as usize;
    let end = end as usize;
    let start_idx = start / 64;
    let start_bit = start % 64;
    let end_idx = end / 64;
    let end_bit = end % 64;
    if start_idx == end_idx {
      let mask = (!0u64 << start_bit) & (!0u64 >> (63 - end_bit));
      self.bits[start_idx] |= mask;
      return;
    }

    self.bits[start_idx] |= !0u64 << start_bit;
    for idx in (start_idx + 1)..end_idx {
      self.bits[idx] = !0u64;
    }
    self.bits[end_idx] |= !0u64 >> (63 - end_bit);
  }

  #[inline]
  fn complement(&self) -> Self {
    let mut out = Self::default();
    for i in 0..CHARSET_WORDS {
      out.bits[i] = !self.bits[i];
    }
    out
  }

  #[inline]
  fn union(&self, other: &Self) -> Self {
    let mut out = Self::default();
    for i in 0..CHARSET_WORDS {
      out.bits[i] = self.bits[i] | other.bits[i];
    }
    out
  }

  #[inline]
  fn intersection(&self, other: &Self) -> Self {
    let mut out = Self::default();
    for i in 0..CHARSET_WORDS {
      out.bits[i] = self.bits[i] & other.bits[i];
    }
    out
  }

  #[inline]
  fn difference(&self, other: &Self) -> Self {
    let mut out = Self::default();
    for i in 0..CHARSET_WORDS {
      out.bits[i] = self.bits[i] & !other.bits[i];
    }
    out
  }
}

/// A RegExp UnicodeSets-mode set containing Unicode code points and string elements.
///
/// The RegExp engine executes over UTF-16, but in UnicodeMode (`u`/`v`) character-class semantics
/// are defined in terms of Unicode code points:
/// - BMP code points (including unpaired surrogates) are stored in `chars` as a bitset over
///   `u16` (0..=0xFFFF).
/// - Supplementary code points (>= 0x10000) are stored in `supplementary`.
///
/// Invariant: `strings` contains **no** strings that encode a single Unicode code point (either a
/// single code unit or a UTF-16 surrogate pair). Those are canonicalized into `chars` /
/// `supplementary`.
/// Invariant: `strings` are stored in **descending length** order, stable for equal lengths.
#[derive(Clone, Default, PartialEq, Eq)]
struct UnicodeSet {
  chars: CharSet,
  supplementary: Vec<u32>,
  strings: Vec<Vec<u16>>,
}

impl UnicodeSet {
  #[inline]
  fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn is_empty(&self) -> bool {
    self.chars.is_empty() && self.supplementary.is_empty() && self.strings.is_empty()
  }

  #[inline]
  fn insert_char(&mut self, u: u16) {
    self.chars.insert(u);
  }

  fn insert_code_point(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    cp: u32,
  ) -> Result<(), RegExpCompileError> {
    if cp <= 0xFFFF {
      self.insert_char(cp as u16);
      return Ok(());
    }
    self.insert_supplementary(ctx, cp)
  }

  fn insert_supplementary(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    cp: u32,
  ) -> Result<(), RegExpCompileError> {
    debug_assert!(cp > 0xFFFF);
    // Keep `supplementary` sorted ascending and deduplicated for cheap set operations.
    match self.supplementary.binary_search(&cp) {
      Ok(_) => Ok(()),
      Err(insert_at) => {
        let required_len = self
          .supplementary
          .len()
          .checked_add(1)
          .ok_or(RegExpCompileError::OutOfMemory)?;
        ctx.reserve_vec_to_len(&mut self.supplementary, required_len)?;
        self.supplementary.insert(insert_at, cp);
        Ok(())
      }
    }
  }

  /// Adds a string element to this set, canonicalizing length-1 strings into `chars`.
  fn insert_string(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    units: &[u16],
  ) -> Result<(), RegExpCompileError> {
    match units.len() {
      0 => self.insert_string_non1(ctx, units),
      1 => {
        self.insert_char(units[0]);
        Ok(())
      }
      2 if is_utf16_high_surrogate(units[0]) && is_utf16_low_surrogate(units[1]) => {
        let cp = utf16_decode_surrogate_pair(units[0], units[1]);
        self.insert_supplementary(ctx, cp)
      }
      _ => self.insert_string_non1(ctx, units),
    }
  }

  fn insert_string_non1(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    units: &[u16],
  ) -> Result<(), RegExpCompileError> {
    debug_assert!(
      !(units.len() == 1
        || (units.len() == 2 && is_utf16_high_surrogate(units[0]) && is_utf16_low_surrogate(units[1]))),
      "single-code-point strings must be canonicalized"
    );

    let len = units.len();

    // Find the insertion point so `strings` stays sorted by descending length (stable for equal
    // lengths). While scanning the equal-length group we can also deduplicate.
    let mut insert_at = 0usize;
    while insert_at < self.strings.len() {
      let cur_len = self.strings[insert_at].len();
      if cur_len > len {
        insert_at += 1;
        continue;
      }
      if cur_len < len {
        break;
      }
      // Equal-length group: scan until end, checking for duplicates.
      let mut i = insert_at;
      while i < self.strings.len() && self.strings[i].len() == len {
        if self.strings[i].as_slice() == units {
          return Ok(());
        }
        i += 1;
      }
      insert_at = i;
      break;
    }

    let mut owned: Vec<u16> = Vec::new();
    ctx.reserve_vec_to_len(&mut owned, units.len())?;
    owned.extend_from_slice(units);

    let required_len = self
      .strings
      .len()
      .checked_add(1)
      .ok_or(RegExpCompileError::OutOfMemory)?;
    ctx.reserve_vec_to_len(&mut self.strings, required_len)?;
    self.strings.insert(insert_at, owned);
    Ok(())
  }

  /// Mirrors the intent of spec `MayContainStrings`:
  /// `true` if the set contains the empty string or any string element longer than 1 code unit.
  #[inline]
  fn may_contain_strings(&self) -> bool {
    self
      .strings
      .iter()
      .any(|s| s.is_empty() || s.len() > 1)
  }

  /// Iterates string elements in descending length order (stable for equal lengths).
  #[inline]
  fn iter_strings_desc_len(&self) -> impl Iterator<Item = &[u16]> {
    // `strings` is stored in the required order as an invariant.
    self.strings.iter().map(|s| s.as_slice())
  }

  #[inline]
  fn contains_char(&self, u: u16) -> bool {
    self.chars.contains(u)
  }

  #[inline]
  fn contains_code_point(&self, cp: u32) -> bool {
    if cp <= 0xFFFF {
      return self.contains_char(cp as u16);
    }
    self.supplementary.binary_search(&cp).is_ok()
  }

  fn contains_string(&self, units: &[u16]) -> bool {
    match units.len() {
      0 => self.strings.iter().any(|s| s.is_empty()),
      1 => self.contains_char(units[0]),
      2 if is_utf16_high_surrogate(units[0]) && is_utf16_low_surrogate(units[1]) => {
        self.contains_code_point(utf16_decode_surrogate_pair(units[0], units[1]))
      }
      _ => self.strings.iter().any(|s| s.as_slice() == units),
    }
  }

  fn union(
    &self,
    ctx: &mut CompileCtx<'_>,
    other: &Self,
  ) -> Result<Self, RegExpCompileError> {
    let chars = self.chars.union(&other.chars);

    let mut supplementary: Vec<u32> = Vec::new();
    let supp_reserve = self
      .supplementary
      .len()
      .checked_add(other.supplementary.len())
      .ok_or(RegExpCompileError::OutOfMemory)?;
    ctx.reserve_vec_to_len(&mut supplementary, supp_reserve)?;
    let mut si = 0usize;
    let mut sj = 0usize;
    while si < self.supplementary.len() && sj < other.supplementary.len() {
      let a = self.supplementary[si];
      let b = other.supplementary[sj];
      if a < b {
        supplementary.push(a);
        si += 1;
      } else if a > b {
        supplementary.push(b);
        sj += 1;
      } else {
        supplementary.push(a);
        si += 1;
        sj += 1;
      }
    }
    while si < self.supplementary.len() {
      supplementary.push(self.supplementary[si]);
      si += 1;
    }
    while sj < other.supplementary.len() {
      supplementary.push(other.supplementary[sj]);
      sj += 1;
    }

    let mut out = Self {
      chars,
      supplementary,
      strings: Vec::new(),
    };

    let reserve = self
      .strings
      .len()
      .checked_add(other.strings.len())
      .ok_or(RegExpCompileError::OutOfMemory)?;
    ctx.reserve_vec_to_len(&mut out.strings, reserve)?;

    let mut i = 0usize;
    let mut j = 0usize;
    while i < self.strings.len() && j < other.strings.len() {
      let len_a = self.strings[i].len();
      let len_b = other.strings[j].len();
      if len_a > len_b {
        out.push_string_clone(ctx, &self.strings[i])?;
        i += 1;
        continue;
      }
      if len_a < len_b {
        out.push_string_clone(ctx, &other.strings[j])?;
        j += 1;
        continue;
      }

      // Equal-length groups: emit all from `self`, then the unique ones from `other`.
      let len = len_a;
      let i_start = i;
      while i < self.strings.len() && self.strings[i].len() == len {
        i += 1;
      }
      let i_end = i;

      let j_start = j;
      while j < other.strings.len() && other.strings[j].len() == len {
        j += 1;
      }
      let j_end = j;

      for s in &self.strings[i_start..i_end] {
        out.push_string_clone(ctx, s)?;
      }
      for s in &other.strings[j_start..j_end] {
        if !self.strings[i_start..i_end]
          .iter()
          .any(|a| a.as_slice() == s.as_slice())
        {
          out.push_string_clone(ctx, s)?;
        }
      }
    }

    while i < self.strings.len() {
      out.push_string_clone(ctx, &self.strings[i])?;
      i += 1;
    }
    while j < other.strings.len() {
      out.push_string_clone(ctx, &other.strings[j])?;
      j += 1;
    }

    Ok(out)
  }

  fn intersection(
    &self,
    ctx: &mut CompileCtx<'_>,
    other: &Self,
  ) -> Result<Self, RegExpCompileError> {
    let chars = self.chars.intersection(&other.chars);

    let mut supplementary: Vec<u32> = Vec::new();
    let supp_reserve = self.supplementary.len().min(other.supplementary.len());
    ctx.reserve_vec_to_len(&mut supplementary, supp_reserve)?;
    let mut si = 0usize;
    let mut sj = 0usize;
    while si < self.supplementary.len() && sj < other.supplementary.len() {
      let a = self.supplementary[si];
      let b = other.supplementary[sj];
      if a < b {
        si += 1;
      } else if a > b {
        sj += 1;
      } else {
        supplementary.push(a);
        si += 1;
        sj += 1;
      }
    }
    let mut out = Self {
      chars,
      supplementary,
      strings: Vec::new(),
    };

    let reserve = self.strings.len().min(other.strings.len());
    ctx.reserve_vec_to_len(&mut out.strings, reserve)?;

    let mut i = 0usize;
    let mut j = 0usize;
    while i < self.strings.len() && j < other.strings.len() {
      let len_a = self.strings[i].len();
      let len_b = other.strings[j].len();
      if len_a > len_b {
        i = Self::group_end(&self.strings, i);
        continue;
      }
      if len_a < len_b {
        j = Self::group_end(&other.strings, j);
        continue;
      }

      let i_start = i;
      i = Self::group_end(&self.strings, i_start);
      let i_end = i;

      let j_start = j;
      j = Self::group_end(&other.strings, j_start);
      let j_end = j;

      for s in &self.strings[i_start..i_end] {
        if other.strings[j_start..j_end]
          .iter()
          .any(|b| b.as_slice() == s.as_slice())
        {
          out.push_string_clone(ctx, s)?;
        }
      }
    }

    Ok(out)
  }

  fn difference(
    &self,
    ctx: &mut CompileCtx<'_>,
    other: &Self,
  ) -> Result<Self, RegExpCompileError> {
    let chars = self.chars.difference(&other.chars);

    let mut supplementary: Vec<u32> = Vec::new();
    ctx.reserve_vec_to_len(&mut supplementary, self.supplementary.len())?;
    let mut si = 0usize;
    let mut sj = 0usize;
    while si < self.supplementary.len() {
      let a = self.supplementary[si];
      if sj >= other.supplementary.len() {
        supplementary.extend_from_slice(&self.supplementary[si..]);
        break;
      }
      let b = other.supplementary[sj];
      if a < b {
        supplementary.push(a);
        si += 1;
      } else if a > b {
        sj += 1;
      } else {
        // Equal: remove.
        si += 1;
        sj += 1;
      }
    }
    let mut out = Self {
      chars,
      supplementary,
      strings: Vec::new(),
    };
    ctx.reserve_vec_to_len(&mut out.strings, self.strings.len())?;

    let mut j = 0usize;
    let mut i = 0usize;
    while i < self.strings.len() {
      let len = self.strings[i].len();
      let i_start = i;
      i = Self::group_end(&self.strings, i_start);
      let i_end = i;

      while j < other.strings.len() && other.strings[j].len() > len {
        j = Self::group_end(&other.strings, j);
      }

      let (j_start, j_end) = if j < other.strings.len() && other.strings[j].len() == len {
        let start = j;
        let end = Self::group_end(&other.strings, start);
        (start, end)
      } else {
        (0usize, 0usize)
      };

      for s in &self.strings[i_start..i_end] {
        let in_other = j_end != 0
          && other.strings[j_start..j_end]
            .iter()
            .any(|b| b.as_slice() == s.as_slice());
        if !in_other {
          out.push_string_clone(ctx, s)?;
        }
      }
    }

    Ok(out)
  }

  /// Computes the complement of this set against an explicit universe.
  ///
  /// This is useful for future support of negated UnicodeSets-mode character classes (`[^...]`),
  /// where the universe is the full UTF-16 code unit range.
  #[inline]
  fn complement_in(
    &self,
    ctx: &mut CompileCtx<'_>,
    universe: &Self,
  ) -> Result<Self, RegExpCompileError> {
    universe.difference(ctx, self)
  }

  fn group_end(strings: &[Vec<u16>], start: usize) -> usize {
    let len = strings[start].len();
    let mut end = start;
    while end < strings.len() && strings[end].len() == len {
      end += 1;
    }
    end
  }

  fn push_string_clone(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    units: &[u16],
  ) -> Result<(), RegExpCompileError> {
    debug_assert!(
      !(units.len() == 1
        || (units.len() == 2 && is_utf16_high_surrogate(units[0]) && is_utf16_low_surrogate(units[1]))),
      "single-code-point strings must be canonicalized"
    );
    let mut owned: Vec<u16> = Vec::new();
    ctx.reserve_vec_to_len(&mut owned, units.len())?;
    owned.extend_from_slice(units);
    ctx.vec_try_push(&mut self.strings, owned)?;
    Ok(())
  }
}

fn char_set_to_char_class(
  ctx: &mut CompileCtx<'_>,
  chars: &CharSet,
  supplementary: &[u32],
  negated: bool,
) -> Result<CharClass, RegExpCompileError> {
  let mut items: Vec<CharClassItem> = Vec::new();

  // Convert the bitset into a compact list of ranges.
  let mut u: u32 = 0;
  let mut i: usize = 0;
  while u <= 0xFFFF {
    if i != 0 {
      ctx.tick_every(i)?;
    }
    i = i.wrapping_add(1);

    let cu = u as u16;
    if !chars.contains(cu) {
      u += 1;
      continue;
    }

    let start = cu;
    let mut end = cu;
    u += 1;
    while u <= 0xFFFF {
      let cu = u as u16;
      if !chars.contains(cu) {
        break;
      }
      end = cu;
      u += 1;
    }

    if start == end {
      ctx.vec_try_push(&mut items, CharClassItem::Char(start as u32))?;
    } else {
      ctx.vec_try_push(&mut items, CharClassItem::Range(start as u32, end as u32))?;
    }
  }

  for &cp in supplementary {
    // Supplementary code points cannot be represented in the BMP bitset.
    debug_assert!(cp > 0xFFFF);
    ctx.vec_try_push(&mut items, CharClassItem::Char(cp))?;
  }

  Ok(CharClass { negated, items })
}

#[inline]
fn is_class_set_escape_class_operand_start(u: Option<u16>) -> bool {
  let Some(u) = u else { return false };
  u == (b'd' as u16)
    || u == (b'D' as u16)
    || u == (b's' as u16)
    || u == (b'S' as u16)
    || u == (b'w' as u16)
    || u == (b'W' as u16)
    || u == (b'p' as u16)
    || u == (b'P' as u16)
}

#[inline]
fn is_class_set_reserved_punctuator(u: u16) -> bool {
  u == (b'&' as u16)
    || u == (b'-' as u16)
    || u == (b'!' as u16)
    || u == (b'#' as u16)
    || u == (b'%' as u16)
    || u == (b',' as u16)
    || u == (b':' as u16)
    || u == (b';' as u16)
    || u == (b'<' as u16)
    || u == (b'=' as u16)
    || u == (b'>' as u16)
    || u == (b'@' as u16)
    || u == (b'`' as u16)
    || u == (b'~' as u16)
}

#[inline]
fn is_class_set_syntax_character(u: u16) -> bool {
  u == (b'(' as u16)
    || u == (b')' as u16)
    || u == (b'[' as u16)
    || u == (b']' as u16)
    || u == (b'{' as u16)
    || u == (b'}' as u16)
    || u == (b'/' as u16)
    || u == (b'-' as u16)
    || u == (b'\\' as u16)
    || u == (b'|' as u16)
}

#[inline]
fn is_class_set_reserved_double_punctuator(u: u16) -> bool {
  u == (b'&' as u16)
    || u == (b'!' as u16)
    || u == (b'#' as u16)
    || u == (b'$' as u16)
    || u == (b'%' as u16)
    || u == (b'*' as u16)
    || u == (b'+' as u16)
    || u == (b',' as u16)
    || u == (b'.' as u16)
    || u == (b':' as u16)
    || u == (b';' as u16)
    || u == (b'<' as u16)
    || u == (b'=' as u16)
    || u == (b'>' as u16)
    || u == (b'?' as u16)
    || u == (b'@' as u16)
    || u == (b'^' as u16)
    || u == (b'`' as u16)
    || u == (b'~' as u16)
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegExpFlags {
  /// The `d` / `hasIndices` flag.
  ///
  /// Note: `vm-js` currently accepts and stores this flag, but does not yet implement
  /// `regexp-match-indices` result semantics (e.g. `match.indices`).
  pub(crate) has_indices: bool,
  pub(crate) global: bool,
  pub(crate) ignore_case: bool,
  pub(crate) multiline: bool,
  pub(crate) dot_all: bool,
  pub(crate) unicode: bool,
  /// Enables ECMAScript "Unicode sets" (`/v`) mode.
  ///
  /// Note: `/v` is mutually exclusive with `/u` in the spec. The RegExp parser uses
  /// [`RegExpFlags::has_either_unicode_flag`] for "UnicodeMode" behaviour.
  pub(crate) unicode_sets: bool,
  pub(crate) sticky: bool,
}

impl RegExpFlags {
  pub(crate) fn parse(
    units: &[u16],
    tick: &mut dyn FnMut() -> Result<(), VmError>,
  ) -> Result<Self, RegExpCompileError> {
    let mut flags = RegExpFlags::default();
    for (i, &u) in units.iter().enumerate() {
      // Avoid ticking on the first iteration so short flag strings don't effectively double-charge
      // fuel (the surrounding expression evaluation already ticks).
      if i != 0 {
        tick_every(i, DEFAULT_TICK_EVERY, tick)?;
      }
      let b = u as u32;
      if b > 0x7F {
        return Err(RegExpSyntaxError {
          message: "Invalid flags supplied to RegExp constructor",
        }
        .into());
      }
      match b as u8 {
        b'd' => {
          if flags.has_indices {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.has_indices = true;
        }
        b'g' => {
          if flags.global {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.global = true;
        }
        b'i' => {
          if flags.ignore_case {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.ignore_case = true;
        }
        b'm' => {
          if flags.multiline {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.multiline = true;
        }
        b's' => {
          if flags.dot_all {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.dot_all = true;
        }
        b'u' => {
          if flags.unicode || flags.unicode_sets {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.unicode = true;
        }
        b'v' => {
          if flags.unicode_sets || flags.unicode {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.unicode_sets = true;
        }
        b'y' => {
          if flags.sticky {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.sticky = true;
        }
        _ => {
          return Err(RegExpSyntaxError {
            message: "Invalid flags supplied to RegExp constructor",
          }
          .into())
        }
      }
    }
    Ok(flags)
  }

  /// Returns the canonical flags string order used by `RegExp.prototype.flags`.
  pub(crate) fn to_canonical_string(self) -> String {
    debug_assert!(
      !(self.unicode && self.unicode_sets),
      "RegExpFlags cannot contain both `u` and `v`"
    );
    let mut out = String::new();
    if self.has_indices {
      out.push('d');
    }
    if self.global {
      out.push('g');
    }
    if self.ignore_case {
      out.push('i');
    }
    if self.multiline {
      out.push('m');
    }
    if self.dot_all {
      out.push('s');
    }
    if self.unicode {
      out.push('u');
    }
    if self.unicode_sets {
      out.push('v');
    }
    if self.sticky {
      out.push('y');
    }
    out
  }
  /// True when either the Unicode (`u`) or Unicode sets (`v`) flags are enabled.
  ///
  /// The RegExp parser has a handful of early-error restrictions that apply in "UnicodeMode",
  /// which is defined as either `u` or `v` being present.
  #[inline]
  pub(crate) fn has_either_unicode_flag(self) -> bool {
    debug_assert!(
      !(self.unicode && self.unicode_sets),
      "RegExpFlags cannot contain both `u` and `v`"
    );
    self.unicode || self.unicode_sets
  }
}

#[cfg(test)]
mod flags_d_tests {
  use super::RegExpFlags;

  #[test]
  fn regexp_flags_parse_accepts_d_and_canonicalizes() {
    let mut tick = || Ok(());
    let flags = RegExpFlags::parse(&[b'd' as u16, b'g' as u16, b'i' as u16], &mut tick).unwrap();
    assert_eq!(flags.to_canonical_string(), "dgi");
  }

  #[test]
  fn regexp_flags_parse_rejects_duplicate_d() {
    let mut tick = || Ok(());
    assert!(RegExpFlags::parse(&[b'd' as u16, b'd' as u16], &mut tick).is_err());
  }
}

#[derive(Debug, Clone)]
pub struct RegExpProgram {
  insts: Vec<Inst>,
  pub(crate) capture_count: usize,
  pub(crate) repeat_count: usize,
  pub(crate) named_capture_groups: Vec<NamedCaptureGroup>,
}

#[derive(Debug, Clone)]
pub(crate) struct NamedCaptureGroup {
  pub(crate) name: Vec<u16>,
  pub(crate) capture_indices: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchDir {
  Forward,
  Backward,
}

impl MatchDir {
  #[inline]
  fn is_forward(self) -> bool {
    matches!(self, MatchDir::Forward)
  }
}

/// Execution-time memory budget for the RegExp backtracking VM.
///
/// RegExp execution allocates per-backtracking-state `captures`/`repeats` buffers and grows a
/// backtracking stack. These allocations live outside the GC heap, so they must be explicitly
/// bounded to avoid bypassing [`crate::HeapLimits`].
#[derive(Debug)]
pub(crate) struct RegExpExecMemoryBudget {
  max_bytes: usize,
  used_bytes: Cell<usize>,
}

impl RegExpExecMemoryBudget {
  #[inline]
  pub(crate) fn new(max_bytes: usize) -> Self {
    Self {
      max_bytes,
      used_bytes: Cell::new(0),
    }
  }

  #[inline]
  fn try_charge(&self, bytes: usize) -> Result<RegExpExecMemoryToken<'_>, VmError> {
    let new_used = self
      .used_bytes
      .get()
      .checked_add(bytes)
      .ok_or(VmError::OutOfMemory)?;
    if new_used > self.max_bytes {
      return Err(VmError::OutOfMemory);
    }
    self.used_bytes.set(new_used);
    Ok(RegExpExecMemoryToken {
      budget: self,
      bytes,
    })
  }
}

#[derive(Debug)]
struct RegExpExecMemoryToken<'a> {
  budget: &'a RegExpExecMemoryBudget,
  bytes: usize,
}

impl Drop for RegExpExecMemoryToken<'_> {
  fn drop(&mut self) {
    if self.bytes == 0 {
      return;
    }
    // Never panic in a destructor path; be conservative and saturate.
    let used = self.budget.used_bytes.get();
    self.budget.used_bytes.set(used.saturating_sub(self.bytes));
  }
}

impl RegExpProgram {
  pub(crate) fn heap_size_bytes(&self) -> usize {
    let mut total = self.insts.capacity().saturating_mul(mem::size_of::<Inst>());
    total = total.saturating_add(
      self
        .named_capture_groups
        .capacity()
        .saturating_mul(mem::size_of::<NamedCaptureGroup>()),
    );
    for group in self.named_capture_groups.iter() {
      total = total.saturating_add(group.name.capacity().saturating_mul(mem::size_of::<u16>()));
      total = total.saturating_add(
        group
          .capture_indices
          .capacity()
          .saturating_mul(mem::size_of::<u32>()),
      );
    }
    for inst in self.insts.iter() {
      match inst {
        Inst::Class(cls) => {
          total = total.saturating_add(cls.heap_size_bytes());
        }
        Inst::UnicodeSet(cls) => {
          total = total.saturating_add(cls.heap_size_bytes());
        }
        Inst::LookAhead { program, .. } => {
          total = total.saturating_add(mem::size_of::<RegExpProgram>());
          total = total.saturating_add(program.heap_size_bytes());
        }
        Inst::LookBehind { program, .. } => {
          total = total.saturating_add(mem::size_of::<RegExpProgram>());
          total = total.saturating_add(program.heap_size_bytes());
        }
        _ => {}
      }
    }
    total
  }

  pub(crate) fn exec_at<'a>(
    &self,
    input: &[u16],
    start: usize,
    flags: RegExpFlags,
    tick: &mut dyn FnMut() -> Result<(), VmError>,
    exec_mem: &'a RegExpExecMemoryBudget,
    initial_captures: Option<&[usize]>,
  ) -> Result<Option<RegExpMatch>, VmError> {
    self.exec_at_dir(
      input,
      start,
      flags,
      MatchDir::Forward,
      tick,
      exec_mem,
      initial_captures,
    )
  }

  fn exec_at_dir<'a>(
    &self,
    input: &[u16],
    start: usize,
    flags: RegExpFlags,
    dir: MatchDir,
    tick: &mut dyn FnMut() -> Result<(), VmError>,
    exec_mem: &'a RegExpExecMemoryBudget,
    initial_captures: Option<&[usize]>,
  ) -> Result<Option<RegExpMatch>, VmError> {
    let unicode_mode = flags.has_either_unicode_flag();
    let mut stack: Vec<ExecState<'a>> = Vec::new();
    let mut stack_mem: Vec<RegExpExecMemoryToken<'a>> = Vec::new();

    fn stack_try_push<'a>(
      stack: &mut Vec<ExecState<'a>>,
      stack_mem: &mut Vec<RegExpExecMemoryToken<'a>>,
      exec_mem: &'a RegExpExecMemoryBudget,
      value: ExecState<'a>,
    ) -> Result<(), VmError> {
      if stack.len() == stack.capacity() {
        let old_cap = stack.capacity();
        let new_cap = if old_cap == 0 { 8 } else { old_cap.saturating_mul(2) };
        let additional = new_cap.saturating_sub(old_cap);
        let bytes = additional
          .checked_mul(mem::size_of::<ExecState<'a>>())
          .ok_or(VmError::OutOfMemory)?;
        if stack_mem.len() == stack_mem.capacity() {
          stack_mem
            .try_reserve(1)
            .map_err(|_| VmError::OutOfMemory)?;
        }
        let token = exec_mem.try_charge(bytes)?;
        stack
          .try_reserve_exact(additional)
          .map_err(|_| VmError::OutOfMemory)?;
        stack_mem.push(token);
      }
      stack.push(value);
      Ok(())
    }

    fn clear_capture_slots(
      state: &mut ExecState<'_>,
      from_slot: usize,
      to_slot: usize,
      tick: &mut dyn FnMut() -> Result<(), VmError>,
    ) -> Result<(), VmError> {
      let end = to_slot.min(state.captures.len());
      if from_slot >= end {
        return Ok(());
      }
      for (i, slot) in (from_slot..end).enumerate() {
        // Avoid ticking on the first iteration so small capture ranges don't double-charge fuel;
        // the surrounding VM loop already ticks once per instruction.
        if i != 0 {
          tick_every(i, DEFAULT_TICK_EVERY, tick)?;
        }
        state.captures[slot] = UNSET;
      }
      Ok(())
    }

    #[inline]
    fn reset_repeat_state(state: &mut ExecState<'_>, id: usize) {
      if let Some(rep) = state.repeats.get_mut(id) {
        rep.count = 0;
        rep.last_pos = UNSET;
      }
    }

    let init = ExecState::new(self, start, initial_captures, exec_mem)?;
    stack_try_push(&mut stack, &mut stack_mem, exec_mem, init)?;

    while let Some(mut state) = stack.pop() {
      loop {
        tick()?;

        let inst = match self.insts.get(state.pc) {
          Some(i) => i,
          None => break,
        };
        match inst {
          Inst::Char(ch) => {
            if dir.is_forward() {
              let Some((cp, len)) =
                decode_code_point(input, state.pos, unicode_mode)
              else {
                break;
              };
              if canonicalize(flags, *ch) != canonicalize(flags, cp) {
                break;
              }
              state.pos = state.pos.saturating_add(len);
            } else {
              let Some((cp, len)) =
                decode_prev_code_point(input, state.pos, unicode_mode)
              else {
                break;
              };
              if canonicalize(flags, *ch) != canonicalize(flags, cp) {
                break;
              }
              state.pos = state.pos.saturating_sub(len);
            }
            state.pc += 1;
          }
          Inst::Any => {
            let Some((cp, len)) = (if dir.is_forward() {
              decode_code_point(input, state.pos, unicode_mode)
            } else {
              decode_prev_code_point(input, state.pos, unicode_mode)
            }) else {
              break;
            };
            if !flags.dot_all && is_line_terminator(cp) {
              break;
            }
            if dir.is_forward() {
              state.pos = state.pos.saturating_add(len);
            } else {
              state.pos = state.pos.saturating_sub(len);
            }
            state.pc += 1;
          }
          Inst::Class(cls) => {
            let Some((cp, len)) = (if dir.is_forward() {
              decode_code_point(input, state.pos, unicode_mode)
            } else {
              decode_prev_code_point(input, state.pos, unicode_mode)
            }) else {
              break;
            };
            if !cls.matches(cp, flags) {
              break;
            }
            if dir.is_forward() {
              state.pos = state.pos.saturating_add(len);
            } else {
              state.pos = state.pos.saturating_sub(len);
            }
            state.pc += 1;
          }
          Inst::UnicodeSet(cls) => {
            if !dir.is_forward() {
              // `/v` UnicodeSets-mode class matching currently supports only single-code-point and
              // empty-string elements in lookbehind (backward direction). Multi-unit string
              // elements require suffix matching against the trie and are not yet implemented.
              let next_pc = state.pc.saturating_add(1);
              let end_pos = state.pos;

              // --- 2) Single-code-point elements ---
              if let Some((cp, len)) =
                decode_prev_code_point(input, end_pos, flags.has_either_unicode_flag())
              {
                let prev_pos = end_pos.saturating_sub(len);
                if cls.single.matches(cp, flags) {
                  // Keep empty as a lower-priority alternative.
                  if cls.has_empty {
                    let mut empty_state = state.try_clone(exec_mem)?;
                    empty_state.pc = next_pc;
                    stack_try_push(&mut stack, &mut stack_mem, exec_mem, empty_state)?;
                  }
                  state.pos = prev_pos;
                  state.pc = next_pc;
                  continue;
                }
              }

              // --- 3) Empty string element ---
              if cls.has_empty {
                state.pc = next_pc;
                continue;
              }

              break;
            }

            let next_pc = state.pc.saturating_add(1);
            let start_pos = state.pos;

            // --- 1) Try multi-code-unit string elements (length > 1), longest first ---
            let mut best_len: usize = 0;
            if !cls.strings.is_empty() {
              let mut node = cls.strings.root();
              let mut pos = start_pos;
              let mut depth: usize = 0;
              let mut step_i: usize = 0;
              while let Some(&u_raw) = input.get(pos) {
                // Tick within the trie traversal to bound hostile long strings.
                if step_i != 0 {
                  tick_every(step_i, DEFAULT_TICK_EVERY, tick)?;
                }
                step_i = step_i.wrapping_add(1);

                let u = if flags.ignore_case {
                  ascii_lower(u_raw)
                } else {
                  u_raw
                };
                let Some(next_node) = cls.strings.step(node, u) else {
                  break;
                };
                node = next_node;
                pos += 1;
                depth = depth.wrapping_add(1);
                // `/v` ordering prefers string elements with length > 1 over single-code-unit
                // elements. The trie is constructed to only contain strings of length > 1.
                if depth > 1 && cls.strings.node_is_terminal(node) {
                  best_len = depth;
                }
              }
            }

            if best_len != 0 {
              // Push lower-priority alternatives in *reverse* order so the VM pops them in the
              // correct spec order:
              //   strings (longest..shortest) -> single -> empty
              if cls.has_empty {
                let mut empty_state = state.try_clone(exec_mem)?;
                empty_state.pc = next_pc;
                stack_try_push(&mut stack, &mut stack_mem, exec_mem, empty_state)?;
              }

              if let Some((cp, len)) =
                decode_code_point(input, start_pos, flags.has_either_unicode_flag())
              {
                if cls.single.matches(cp, flags) {
                  let mut char_state = state.try_clone(exec_mem)?;
                  char_state.pc = next_pc;
                  char_state.pos = start_pos.saturating_add(len);
                  stack_try_push(&mut stack, &mut stack_mem, exec_mem, char_state)?;
                }
              }

              // Push the other matching string lengths (excluding the longest) so they are tried
              // after the current branch.
              if !cls.strings.is_empty() {
                let mut node = cls.strings.root();
                let mut pos = start_pos;
                let mut depth: usize = 0;
                let mut step_i: usize = 0;
                while let Some(&u_raw) = input.get(pos) {
                  if step_i != 0 {
                    tick_every(step_i, DEFAULT_TICK_EVERY, tick)?;
                  }
                  step_i = step_i.wrapping_add(1);

                  let u = if flags.ignore_case {
                    ascii_lower(u_raw)
                  } else {
                    u_raw
                  };
                  let Some(next_node) = cls.strings.step(node, u) else {
                    break;
                  };
                  node = next_node;
                  pos += 1;
                  depth = depth.wrapping_add(1);
                  if depth > 1 && cls.strings.node_is_terminal(node) && depth != best_len {
                    let mut alt = state.try_clone(exec_mem)?;
                    alt.pc = next_pc;
                    alt.pos = start_pos.saturating_add(depth);
                    stack_try_push(&mut stack, &mut stack_mem, exec_mem, alt)?;
                  }
                }
              }

              state.pos = start_pos.saturating_add(best_len);
              state.pc = next_pc;
              continue;
            }

            // --- 2) Single-code-unit elements ---
            if let Some((cp, len)) =
              decode_code_point(input, start_pos, flags.has_either_unicode_flag())
            {
              if cls.single.matches(cp, flags) {
                // Keep empty as a lower-priority alternative.
                if cls.has_empty {
                  let mut empty_state = state.try_clone(exec_mem)?;
                  empty_state.pc = next_pc;
                  stack_try_push(&mut stack, &mut stack_mem, exec_mem, empty_state)?;
                }
                state.pos = start_pos.saturating_add(len);
                state.pc = next_pc;
                continue;
              }
            }

            // --- 3) Empty string element ---
            if cls.has_empty {
              state.pc = next_pc;
              continue;
            }

            break;
          }
          Inst::UnicodeProperty(prop) => {
            let Some((cp, len)) = (if dir.is_forward() {
              decode_code_point(input, state.pos, flags.has_either_unicode_flag())
            } else {
              decode_prev_code_point(input, state.pos, flags.has_either_unicode_flag())
            }) else {
              break;
            };
            if !prop.matches(cp, flags) {
              break;
            }
            if dir.is_forward() {
              state.pos = state.pos.saturating_add(len);
            } else {
              state.pos = state.pos.saturating_sub(len);
            }
            state.pc += 1;
          }
          Inst::UnicodeStringProperty(prop) => {
            // String properties match only in forward direction. Backward matching would require
            // suffix matching against the property trie, which is not implemented yet.
            if !dir.is_forward() {
              break;
            }
            let next_pc = state.pc.saturating_add(1);
            let start_pos = state.pos;

            // Compute all matching string lengths (prefix matches) with a bounded, allocation-free
            // buffer.
            let mut lens = [0usize; MAX_MATCHES_PER_POSITION];
            let n = match_property_at(*prop, input, start_pos, &mut lens);
            if n == 0 {
              break;
            }

            let best_len = lens[n - 1];
            // Push shorter alternatives so the VM tries them after the current (longest) branch.
            // `lens` is sorted by increasing length, so pushing in forward order makes the stack
            // pop in descending length order.
            for &len in &lens[..n - 1] {
              let mut alt = state.try_clone(exec_mem)?;
              alt.pc = next_pc;
              alt.pos = start_pos.saturating_add(len);
              stack_try_push(&mut stack, &mut stack_mem, exec_mem, alt)?;
            }

            state.pos = start_pos.saturating_add(best_len);
            state.pc = next_pc;
            continue;
          }
          Inst::AssertStart => {
            if state.pos == 0 {
              state.pc += 1;
              continue;
            }
            if flags.multiline {
              if let Some(&prev) = input.get(state.pos.saturating_sub(1)) {
                if is_line_terminator_unit(prev) {
                  state.pc += 1;
                  continue;
                }
              }
            }
            break;
          }
          Inst::AssertEnd => {
            let len = input.len();
            if state.pos == len {
              state.pc += 1;
              continue;
            }
            // `$` matches before a final line terminator even without multiline.
            if state.pos + 1 == len {
              if let Some(&next) = input.get(state.pos) {
                if is_line_terminator_unit(next) {
                  state.pc += 1;
                  continue;
                }
              }
            }
            if flags.multiline {
              if let Some(&next) = input.get(state.pos) {
                if is_line_terminator_unit(next) {
                  state.pc += 1;
                  continue;
                }
              }
            }
            break;
          }
          Inst::WordBoundary { negated } => {
            let at = is_word_boundary(input, state.pos, flags);
            if *negated {
              if at {
                break;
              }
            } else if !at {
              break;
            }
            state.pc += 1;
          }
          Inst::Save(slot) => {
            let slot = if dir.is_forward() { *slot } else { *slot ^ 1 };
            if let Some(dst) = state.captures.get_mut(slot) {
              *dst = state.pos;
            }
            state.pc += 1;
          }
          Inst::BackRef(group) => {
            let idx = *group as usize;
            // Group 0 is not addressable via backreferences; treat it as empty.
            if idx == 0 {
              state.pc += 1;
              continue;
            }
            let start_slot = idx.saturating_mul(2);
            let end_slot = start_slot.saturating_add(1);
            let (Some(&cap_start), Some(&cap_end)) =
              (state.captures.get(start_slot), state.captures.get(end_slot))
            else {
              // Out-of-range group index: treat as empty (approximation).
              state.pc += 1;
              continue;
            };
            if cap_start == UNSET || cap_end == UNSET || cap_end < cap_start {
              // Unmatched capture => empty backreference.
              state.pc += 1;
              continue;
            }
            if cap_end > input.len() || cap_start > input.len() {
              // Defensive: capture indices should always be within-bounds.
              break;
            }
            if !flags.has_either_unicode_flag() {
              // Legacy (no `/u` or `/v`) behaviour: compare UTF-16 code units directly.
              let slice = &input[cap_start..cap_end];
              if dir.is_forward() {
                if state.pos + slice.len() > input.len() {
                  break;
                }
                let mut ok = true;
                for (i, (&a, &b)) in slice
                  .iter()
                  .zip(input[state.pos..state.pos + slice.len()].iter())
                  .enumerate()
                {
                  // Avoid ticking on the first iteration so small captures don't double-charge fuel;
                  // the surrounding VM loop already ticks once per instruction.
                  if i != 0 {
                    tick_every(i, DEFAULT_TICK_EVERY, tick)?;
                  }
                  if !canonical_eq(a, b, flags) {
                    ok = false;
                    break;
                  }
                }
                if !ok {
                  break;
                }
                state.pos += slice.len();
              } else {
                if slice.len() > state.pos {
                  break;
                }
                let start_pos = state.pos - slice.len();
                let mut ok = true;
                for (i, (&a, &b)) in slice.iter().zip(input[start_pos..state.pos].iter()).enumerate()
                {
                  if i != 0 {
                    tick_every(i, DEFAULT_TICK_EVERY, tick)?;
                  }
                  if !canonical_eq(a, b, flags) {
                    ok = false;
                    break;
                  }
                }
                if !ok {
                  break;
                }
                state.pos = start_pos;
              }
              state.pc += 1;
              continue;
            }

            // FullUnicode (/u or /v) semantics:
            // - Compare the capture and the input as sequences of decoded code points.
            // - Do not allow matching to stop in the middle of a surrogate pair.
            if !is_utf16_code_point_boundary(input, state.pos)
              || !is_utf16_code_point_boundary(input, cap_start)
              || !is_utf16_code_point_boundary(input, cap_end)
            {
              break;
            }

            if dir.is_forward() {
              let mut cap_i = cap_start;
              let mut target_i = state.pos;
              let mut cmp_i: usize = 0;
              while cap_i < cap_end {
                // Avoid ticking on the first iteration so short backreferences don't effectively
                // double-charge fuel (the surrounding VM loop already ticks per instruction).
                if cmp_i != 0 {
                  tick_every(cmp_i, DEFAULT_TICK_EVERY, tick)?;
                }
                cmp_i = cmp_i.wrapping_add(1);

                let (cap_cp, cap_len) = utf16_code_point_at_bounded(input, cap_i, cap_end);
                if cap_len == 0 {
                  break;
                }
                if target_i >= input.len() {
                  break;
                }
                debug_assert!(is_utf16_code_point_boundary(input, target_i));
                let Some((target_cp, target_len)) =
                  decode_code_point(input, target_i, /*unicode=*/ true)
                else {
                  break;
                };

                if canonicalize(flags, cap_cp) != canonicalize(flags, target_cp) {
                  break;
                }

                cap_i = cap_i.saturating_add(cap_len);
                target_i = target_i.saturating_add(target_len);
              }

              if cap_i != cap_end {
                // Mismatch, ran out of input, or capture boundaries not aligned to code points.
                break;
              }
              debug_assert!(is_utf16_code_point_boundary(input, target_i));
              state.pos = target_i;
            } else {
              let mut cap_i = cap_end;
              let mut target_i = state.pos;
              let mut cmp_i: usize = 0;
              while cap_i > cap_start {
                if cmp_i != 0 {
                  tick_every(cmp_i, DEFAULT_TICK_EVERY, tick)?;
                }
                cmp_i = cmp_i.wrapping_add(1);

                let Some((cap_cp, cap_len)) =
                  decode_prev_code_point(input, cap_i, /*unicode=*/ true)
                else {
                  break;
                };
                let Some(new_cap_i) = cap_i.checked_sub(cap_len) else {
                  break;
                };
                if new_cap_i < cap_start {
                  break;
                }

                let Some((target_cp, target_len)) =
                  decode_prev_code_point(input, target_i, /*unicode=*/ true)
                else {
                  break;
                };
                let Some(new_target_i) = target_i.checked_sub(target_len) else {
                  break;
                };

                if canonicalize(flags, cap_cp) != canonicalize(flags, target_cp) {
                  break;
                }

                cap_i = new_cap_i;
                target_i = new_target_i;
              }

              if cap_i != cap_start {
                break;
              }
              debug_assert!(is_utf16_code_point_boundary(input, target_i));
              state.pos = target_i;
            }
            state.pc += 1;
          }
          Inst::NamedBackRef(name_id) => {
            let Some(group) = self.named_capture_groups.get(*name_id as usize) else {
              // Should not happen (compile-time validated); treat as empty.
              state.pc += 1;
              continue;
            };

            let mut found: Option<(usize, usize)> = None;
            for (i, &cap_idx) in group.capture_indices.iter().rev().enumerate() {
              if i % 64 == 0 {
                tick()?;
              }
              let idx = cap_idx as usize;
              let start_slot = idx.saturating_mul(2);
              let end_slot = start_slot.saturating_add(1);
              let (Some(&cap_start), Some(&cap_end)) =
                (state.captures.get(start_slot), state.captures.get(end_slot))
              else {
                continue;
              };
              if cap_start == UNSET || cap_end == UNSET || cap_end < cap_start {
                continue;
              }
              found = Some((cap_start, cap_end));
              break;
            }

            let Some((cap_start, cap_end)) = found else {
              // Unmatched capture => empty backreference.
              state.pc += 1;
              continue;
            };

            if cap_end > input.len() || cap_start > input.len() {
              // Defensive: capture indices should always be within-bounds.
              break;
            }

            if !flags.has_either_unicode_flag() {
              // Legacy (no `/u` or `/v`) behaviour: compare UTF-16 code units directly.
              let slice = &input[cap_start..cap_end];
              if dir.is_forward() {
                if state.pos + slice.len() > input.len() {
                  break;
                }

                let mut ok = true;
                for (i, (&a, &b)) in slice
                  .iter()
                  .zip(input[state.pos..state.pos + slice.len()].iter())
                  .enumerate()
                {
                  if i != 0 {
                    tick_every(i, DEFAULT_TICK_EVERY, tick)?;
                  }
                  if !canonical_eq(a, b, flags) {
                    ok = false;
                    break;
                  }
                }
                if !ok {
                  break;
                }
                state.pos += slice.len();
              } else {
                if slice.len() > state.pos {
                  break;
                }
                let start_pos = state.pos - slice.len();

                let mut ok = true;
                for (i, (&a, &b)) in slice.iter().zip(input[start_pos..state.pos].iter()).enumerate()
                {
                  if i != 0 {
                    tick_every(i, DEFAULT_TICK_EVERY, tick)?;
                  }
                  if !canonical_eq(a, b, flags) {
                    ok = false;
                    break;
                  }
                }
                if !ok {
                  break;
                }
                state.pos = start_pos;
              }
              state.pc += 1;
              continue;
            }

            // FullUnicode (/u or /v): compare decoded code points, forbidding partial surrogate-pair
            // matches.
            if !is_utf16_code_point_boundary(input, state.pos)
              || !is_utf16_code_point_boundary(input, cap_start)
              || !is_utf16_code_point_boundary(input, cap_end)
            {
              break;
            }

            if dir.is_forward() {
              let mut cap_i = cap_start;
              let mut target_i = state.pos;
              let mut cmp_i: usize = 0;
              while cap_i < cap_end {
                if cmp_i != 0 {
                  tick_every(cmp_i, DEFAULT_TICK_EVERY, tick)?;
                }
                cmp_i = cmp_i.wrapping_add(1);

                let (cap_cp, cap_len) = utf16_code_point_at_bounded(input, cap_i, cap_end);
                if cap_len == 0 {
                  break;
                }
                if target_i >= input.len() {
                  break;
                }
                debug_assert!(is_utf16_code_point_boundary(input, target_i));
                let Some((target_cp, target_len)) =
                  decode_code_point(input, target_i, /*unicode=*/ true)
                else {
                  break;
                };

                if canonicalize(flags, cap_cp) != canonicalize(flags, target_cp) {
                  break;
                }

                cap_i = cap_i.saturating_add(cap_len);
                target_i = target_i.saturating_add(target_len);
              }

              if cap_i != cap_end {
                break;
              }
              debug_assert!(is_utf16_code_point_boundary(input, target_i));
              state.pos = target_i;
            } else {
              let mut cap_i = cap_end;
              let mut target_i = state.pos;
              let mut cmp_i: usize = 0;
              while cap_i > cap_start {
                if cmp_i != 0 {
                  tick_every(cmp_i, DEFAULT_TICK_EVERY, tick)?;
                }
                cmp_i = cmp_i.wrapping_add(1);

                let Some((cap_cp, cap_len)) =
                  decode_prev_code_point(input, cap_i, /*unicode=*/ true)
                else {
                  break;
                };
                let Some(new_cap_i) = cap_i.checked_sub(cap_len) else {
                  break;
                };
                if new_cap_i < cap_start {
                  break;
                }

                let Some((target_cp, target_len)) =
                  decode_prev_code_point(input, target_i, /*unicode=*/ true)
                else {
                  break;
                };
                let Some(new_target_i) = target_i.checked_sub(target_len) else {
                  break;
                };

                if canonicalize(flags, cap_cp) != canonicalize(flags, target_cp) {
                  break;
                }

                cap_i = new_cap_i;
                target_i = new_target_i;
              }

              if cap_i != cap_start {
                break;
              }
              debug_assert!(is_utf16_code_point_boundary(input, target_i));
              state.pos = target_i;
            }
            state.pc += 1;
          }
          Inst::Split(a, b) => {
            let mut other = state.try_clone(exec_mem)?;
            other.pc = *b;
            stack_try_push(&mut stack, &mut stack_mem, exec_mem, other)?;
            state.pc = *a;
          }
          Inst::Jump(target) => {
            state.pc = *target;
          }
          Inst::RepeatStart {
            id,
            min,
            max,
            greedy,
            exit,
            clear_from_slot,
            clear_to_slot,
          } => {
            let id = *id;
            // Reset this quantifier's runtime state when entering from outside its own loop (i.e.
            // not from the `RepeatEnd { start }` jump). This prevents nested quantifiers from
            // leaking their `count`/`last_pos` across outer iterations.
            let is_continuation = state.repeat_from_end_pc == Some(state.pc);
            state.repeat_from_end_pc = None;
            if !is_continuation {
              if let Some(rep) = state.repeats.get_mut(id) {
                rep.count = 0;
                rep.last_pos = UNSET;
              }
            }
            let Some(rep) = state.repeats.get(id).copied() else {
              break;
            };
            let count = rep.count;
            let last_pos = rep.last_pos;

            // Empty-match guard: if the previous iteration started at this same input position and
            // we've satisfied the minimum, don't enter the body again (avoids infinite loops for
            // patterns like `(?:)*` and `(a*)*`).
            if count >= *min && last_pos == state.pos && count != 0 {
              // Leaving the quantifier: reset per-quantifier runtime state so a later entry to the
              // same `RepeatStart` (e.g. due to an enclosing quantifier) starts fresh.
              reset_repeat_state(&mut state, id);
              state.pc = *exit;
              continue;
            }

            if count < *min {
              // Capture groups in quantified expressions are reset for each iteration (ECMA-262
              // `RepeatMatcher` / `UpdateS` semantics).
              if let Some(rep) = state.repeats.get_mut(id) {
                rep.last_pos = state.pos;
                rep.count = rep.count.saturating_add(1);
              }
              clear_capture_slots(&mut state, *clear_from_slot, *clear_to_slot, tick)?;
              state.pc += 1;
              continue;
            }
            if let Some(max) = max {
              if count >= *max {
                reset_repeat_state(&mut state, id);
                state.pc = *exit;
                continue;
              }
            }

            if *greedy {
              // Try the body first, but keep the "stop" continuation on the backtracking stack.
              let mut stop = state.try_clone(exec_mem)?;
              stop.pc = *exit;
              reset_repeat_state(&mut stop, id);
              stack_try_push(&mut stack, &mut stack_mem, exec_mem, stop)?;
              if let Some(rep) = state.repeats.get_mut(id) {
                rep.last_pos = state.pos;
                rep.count = rep.count.saturating_add(1);
              }
              clear_capture_slots(&mut state, *clear_from_slot, *clear_to_slot, tick)?;
              state.pc += 1;
            } else {
              // Lazy: try stopping first, but keep the "take body" continuation on the stack.
              let mut body = state.try_clone(exec_mem)?;
              if let Some(body_rep) = body.repeats.get_mut(id) {
                body_rep.last_pos = body.pos;
                body_rep.count = body_rep.count.saturating_add(1);
              }
              clear_capture_slots(&mut body, *clear_from_slot, *clear_to_slot, tick)?;
              body.pc += 1;
              stack_try_push(&mut stack, &mut stack_mem, exec_mem, body)?;
              reset_repeat_state(&mut state, id);
              state.pc = *exit;
            }
          }
          Inst::RepeatEnd { start } => {
            state.pc = *start;
            state.repeat_from_end_pc = Some(*start);
          }
          Inst::RepeatReset { id } => {
            let Some(rep) = state.repeats.get_mut(*id) else {
              break;
            };
            rep.count = 0;
            rep.last_pos = UNSET;
            state.pc += 1;
          }
          Inst::LookAhead { program, negative } => {
            // Run the nested program anchored at the current position.
            let sub = program.exec_at(
              input,
              state.pos,
              flags,
              tick,
              exec_mem,
              Some(&state.captures),
            )?;
            match (sub, *negative) {
              (Some(_), true) => {
                // Negative lookahead matched => fail this branch.
                break;
              }
              (None, false) => {
                // Positive lookahead failed.
                break;
              }
              (None, true) => {
                // Negative lookahead failed => success, consume nothing.
                state.pc += 1;
              }
              (Some(matched), false) => {
                // Positive lookahead matched => merge captures (excluding group 0).
                state.merge_captures_from(&matched);
                state.pc += 1;
              }
            }
          }
          Inst::LookBehind { program, negative } => {
            // Run the nested program anchored at the current position with -1 direction.
            let sub = program.exec_at_dir(
              input,
              state.pos,
              flags,
              MatchDir::Backward,
              tick,
              exec_mem,
              Some(&state.captures),
            )?;
            match (sub, *negative) {
              (Some(_), true) => {
                // Negative lookbehind matched => fail this branch.
                break;
              }
              (None, false) => {
                // Positive lookbehind failed.
                break;
              }
              (None, true) => {
                // Negative lookbehind failed => success, consume nothing.
                state.pc += 1;
              }
              (Some(matched), false) => {
                // Positive lookbehind matched => merge captures (excluding group 0).
                state.merge_captures_from(&matched);
                state.pc += 1;
              }
            }
          }
          Inst::Match => {
            // Success: fill group 0 end.
            if let Some(end) = state.captures.get_mut(1) {
              *end = state.pos;
            }
            return Ok(Some(RegExpMatch {
              end: state.pos,
              captures: state.captures,
            }));
          }
        }
      }
    }

    Ok(None)
  }

  /// Search for the first match at or after `start` (unanchored).
  ///
  /// This is primarily used by `RegExpBuiltinExec` for non-sticky regexes.
  ///
  /// For common patterns like `/\\s/`, calling `exec_at` at every candidate index is extremely
  /// expensive (it allocates per-attempt backtracking state and ticks once per instruction). This
  /// helper provides a fast-path for "single atom" programs that can be evaluated with a simple
  /// linear scan.
  pub(crate) fn exec_search<'a>(
    &self,
    input: &[u16],
    start: usize,
    flags: RegExpFlags,
    tick: &mut dyn FnMut() -> Result<(), VmError>,
    exec_mem: &'a RegExpExecMemoryBudget,
  ) -> Result<Option<RegExpMatch>, VmError> {
    // Fast path: program is a single instruction followed by `Match`, with no captures/repeats.
    if self.capture_count == 1
      && self.repeat_count == 0
      && self.named_capture_groups.is_empty()
      && self.insts.len() == 2
      && matches!(self.insts[1], Inst::Match)
    {
      let unicode = flags.has_either_unicode_flag();

      let try_return_match = |start: usize, end: usize| -> Result<Option<RegExpMatch>, VmError> {
        let mut captures: Vec<usize> = Vec::new();
        captures
          .try_reserve_exact(2)
          .map_err(|_| VmError::OutOfMemory)?;
        captures.push(start);
        captures.push(end);
        Ok(Some(RegExpMatch { end, captures }))
      };

      match &self.insts[0] {
        Inst::Char(ch) => {
          let target = canonicalize(flags, *ch);
          let mut k = start;
          let mut i: usize = 0;
          while k < input.len() {
            if i != 0 {
              tick_every(i, DEFAULT_TICK_EVERY, tick)?;
            }
            let Some((cp, len)) = decode_code_point(input, k, unicode) else {
              break;
            };
            if canonicalize(flags, cp) == target {
              let end = k.saturating_add(len);
              return try_return_match(k, end);
            }
            k = advance_string_index(input, k, unicode);
            i = i.wrapping_add(1);
          }
          return Ok(None);
        }
        Inst::Any => {
          let mut k = start;
          let mut i: usize = 0;
          while k < input.len() {
            if i != 0 {
              tick_every(i, DEFAULT_TICK_EVERY, tick)?;
            }
            let Some((cp, len)) = decode_code_point(input, k, unicode) else {
              break;
            };
            if flags.dot_all || !(cp <= 0xFFFF && is_line_terminator_unit(cp as u16)) {
              let end = k.saturating_add(len);
              return try_return_match(k, end);
            }
            k = advance_string_index(input, k, unicode);
            i = i.wrapping_add(1);
          }
          return Ok(None);
        }
        Inst::Class(cls) => {
          let mut k = start;
          let mut i: usize = 0;
          while k < input.len() {
            if i != 0 {
              tick_every(i, DEFAULT_TICK_EVERY, tick)?;
            }
            let Some((cp, len)) = decode_code_point(input, k, unicode) else {
              break;
            };
            if cls.matches(cp, flags) {
              let end = k.saturating_add(len);
              return try_return_match(k, end);
            }
            k = advance_string_index(input, k, unicode);
            i = i.wrapping_add(1);
          }
          return Ok(None);
        }
        _ => {
          // Not eligible for the fast path.
        }
      }
    }

    // Fallback: try to match at each candidate index.
    let mut k = start;
    let s_len = input.len();
    loop {
      if k > s_len {
        break;
      }
      // Run the VM at this candidate index (anchored).
      let m = self.exec_at(input, k, flags, tick, exec_mem, None)?;
      if m.is_some() {
        return Ok(m);
      }
      k = advance_string_index(input, k, flags.has_either_unicode_flag());
      if k > s_len {
        break;
      }
      // Ensure long scans still observe termination budgets even for patterns that fail quickly.
      if k % DEFAULT_TICK_EVERY == 0 {
        tick()?;
      }
    }

    Ok(None)
  }

  /// Fallibly clones this program.
  ///
  /// Note: `RegExpProgram` also implements `Clone`, but the derived `Clone` implementation may
  /// allocate infallibly. Embeddings that want to avoid abort-on-OOM should prefer this method.
  pub fn try_clone(&self) -> Result<Self, VmError> {
    let mut insts: Vec<Inst> = Vec::new();
    insts
      .try_reserve_exact(self.insts.len())
      .map_err(|_| VmError::OutOfMemory)?;

    for inst in self.insts.iter() {
      let cloned = match inst {
        Inst::Char(u) => Inst::Char(*u),
        Inst::Any => Inst::Any,
        Inst::Class(cls) => Inst::Class(cls.try_clone().map_err(|e| match e {
          RegExpCompileError::OutOfMemory => VmError::OutOfMemory,
          // Cloning an already-compiled class should never fail with a syntax error.
          RegExpCompileError::Syntax(_) => {
            VmError::InvariantViolation("RegExpProgram clone syntax error")
          }
          RegExpCompileError::Vm(err) => err,
        })?),
        Inst::UnicodeSet(cls) => Inst::UnicodeSet(cls.try_clone().map_err(|e| match e {
          RegExpCompileError::OutOfMemory => VmError::OutOfMemory,
          // Cloning an already-compiled class should never fail with a syntax error.
          RegExpCompileError::Syntax(_) => {
            VmError::InvariantViolation("RegExpProgram clone syntax error")
          }
          RegExpCompileError::Vm(err) => err,
        })?),
        Inst::UnicodeProperty(prop) => Inst::UnicodeProperty(*prop),
        Inst::UnicodeStringProperty(prop) => Inst::UnicodeStringProperty(*prop),
        Inst::AssertStart => Inst::AssertStart,
        Inst::AssertEnd => Inst::AssertEnd,
        Inst::WordBoundary { negated } => Inst::WordBoundary { negated: *negated },
        Inst::Save(slot) => Inst::Save(*slot),
        Inst::BackRef(group) => Inst::BackRef(*group),
        Inst::NamedBackRef(name_id) => Inst::NamedBackRef(*name_id),
        Inst::Split(a, b) => Inst::Split(*a, *b),
        Inst::Jump(target) => Inst::Jump(*target),
        Inst::RepeatReset { id } => Inst::RepeatReset { id: *id },
        Inst::RepeatStart {
          id,
          min,
          max,
          greedy,
          exit,
          clear_from_slot,
          clear_to_slot,
        } => Inst::RepeatStart {
          id: *id,
          min: *min,
          max: *max,
          greedy: *greedy,
          exit: *exit,
          clear_from_slot: *clear_from_slot,
          clear_to_slot: *clear_to_slot,
        },
        Inst::RepeatEnd { start } => Inst::RepeatEnd { start: *start },
        Inst::LookAhead { program, negative } => Inst::LookAhead {
          program: box_try_new_vm(program.try_clone()?)?,
          negative: *negative,
        },
        Inst::LookBehind { program, negative } => Inst::LookBehind {
          program: box_try_new_vm(program.try_clone()?)?,
          negative: *negative,
        },
        Inst::Match => Inst::Match,
      };
      // `insts` was reserved to `self.insts.len()` above; pushing within that bound is infallible.
      insts.push(cloned);
    }

    let mut named_capture_groups: Vec<NamedCaptureGroup> = Vec::new();
    named_capture_groups
      .try_reserve_exact(self.named_capture_groups.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for group in self.named_capture_groups.iter() {
      let mut name: Vec<u16> = Vec::new();
      name
        .try_reserve_exact(group.name.len())
        .map_err(|_| VmError::OutOfMemory)?;
      name.extend_from_slice(&group.name);

      let mut capture_indices: Vec<u32> = Vec::new();
      capture_indices
        .try_reserve_exact(group.capture_indices.len())
        .map_err(|_| VmError::OutOfMemory)?;
      capture_indices.extend_from_slice(&group.capture_indices);

      named_capture_groups.push(NamedCaptureGroup {
        name,
        capture_indices,
      });
    }

    Ok(Self {
      insts,
      capture_count: self.capture_count,
      repeat_count: self.repeat_count,
      named_capture_groups,
    })
  }
}

#[derive(Debug, Clone)]
pub(crate) struct RegExpMatch {
  pub(crate) end: usize,
  /// Capture slots: index `2*i` is the start, `2*i+1` is the end. `usize::MAX` means "unset".
  pub(crate) captures: Vec<usize>,
}

const UNSET: usize = usize::MAX;

#[derive(Debug, Clone, Copy, Default)]
struct RepeatRuntime {
  count: u32,
  last_pos: usize,
}

#[derive(Debug)]
struct ExecState<'a> {
  pc: usize,
  pos: usize,
  captures: Vec<usize>,
  captures_mem: RegExpExecMemoryToken<'a>,
  repeats: Vec<RepeatRuntime>,
  repeats_mem: RegExpExecMemoryToken<'a>,
  /// Marker used to distinguish entering a `RepeatStart` from its corresponding `RepeatEnd`
  /// (continuation of the same quantifier loop) vs entering it "fresh" from some outer control
  /// flow (e.g. an enclosing quantifier iteration).
  ///
  /// Without this, nested quantifiers can leak their `RepeatRuntime` state across outer iterations
  /// (e.g. `(?<=((?:b\\d{2})+))`), causing inner `RepeatStart` counts to be reused and short-circuit
  /// subsequent iterations.
  repeat_from_end_pc: Option<usize>,
}

impl<'a> ExecState<'a> {
  fn new(
    program: &RegExpProgram,
    start: usize,
    initial_captures: Option<&[usize]>,
    exec_mem: &'a RegExpExecMemoryBudget,
  ) -> Result<Self, VmError> {
    let capture_len = program
      .capture_count
      .checked_mul(2)
      .ok_or(VmError::OutOfMemory)?;
    let capture_bytes = capture_len
      .checked_mul(mem::size_of::<usize>())
      .ok_or(VmError::OutOfMemory)?;
    let captures_mem = exec_mem.try_charge(capture_bytes)?;

    let mut captures: Vec<usize> = Vec::new();
    captures
      .try_reserve_exact(capture_len)
      .map_err(|_| VmError::OutOfMemory)?;
    captures.resize(capture_len, UNSET);

    if let Some(src) = initial_captures {
      let len = captures.len().min(src.len());
      captures[..len].copy_from_slice(&src[..len]);
    }

    // Group 0 start is always the start position for the currently-executing program.
    if let Some(slot0) = captures.get_mut(0) {
      *slot0 = start;
    }

    let repeats_len = program.repeat_count;
    let repeats_bytes = repeats_len
      .checked_mul(mem::size_of::<RepeatRuntime>())
      .ok_or(VmError::OutOfMemory)?;
    let repeats_mem = exec_mem.try_charge(repeats_bytes)?;

    let mut repeats: Vec<RepeatRuntime> = Vec::new();
    repeats
      .try_reserve_exact(repeats_len)
      .map_err(|_| VmError::OutOfMemory)?;
    repeats.resize(repeats_len, RepeatRuntime { count: 0, last_pos: UNSET });

    Ok(Self {
      pc: 0,
      pos: start,
      captures,
      captures_mem,
      repeats,
      repeats_mem,
      repeat_from_end_pc: None,
    })
  }

  fn try_clone(&self, exec_mem: &'a RegExpExecMemoryBudget) -> Result<Self, VmError> {
    let capture_bytes = self
      .captures
      .len()
      .checked_mul(mem::size_of::<usize>())
      .ok_or(VmError::OutOfMemory)?;
    let captures_mem = exec_mem.try_charge(capture_bytes)?;

    let mut captures: Vec<usize> = Vec::new();
    captures
      .try_reserve_exact(self.captures.len())
      .map_err(|_| VmError::OutOfMemory)?;
    captures.extend_from_slice(&self.captures);

    let repeats_bytes = self
      .repeats
      .len()
      .checked_mul(mem::size_of::<RepeatRuntime>())
      .ok_or(VmError::OutOfMemory)?;
    let repeats_mem = exec_mem.try_charge(repeats_bytes)?;

    let mut repeats: Vec<RepeatRuntime> = Vec::new();
    repeats
      .try_reserve_exact(self.repeats.len())
      .map_err(|_| VmError::OutOfMemory)?;
    repeats.extend_from_slice(&self.repeats);
    Ok(Self {
      pc: self.pc,
      pos: self.pos,
      captures,
      captures_mem,
      repeats,
      repeats_mem,
      repeat_from_end_pc: self.repeat_from_end_pc,
    })
  }

  fn merge_captures_from(&mut self, other: &RegExpMatch) {
    // Preserve group 0 slots (0..2) from the outer match attempt.
    for i in 2..self.captures.len().min(other.captures.len()) {
      self.captures[i] = other.captures[i];
    }
  }
}

#[derive(Debug, Clone)]
enum Inst {
  /// Matches a single pattern character.
  ///
  /// In non-UnicodeMode this matches a single UTF-16 code unit.
  /// In UnicodeMode (`/u` or `/v`), this matches a single Unicode code point (consuming 1 or 2
  /// UTF-16 code units).
  Char(u32),
  Any,
  Class(CharClass),
  /// UnicodeSets-mode (`/v`) character class that can match either a single code unit or a string
  /// of multiple code units.
  ///
  /// This is intentionally a single VM instruction to avoid exploding large properties-of-strings
  /// (e.g. `\p{RGI_Emoji}`) into tens of thousands of `Split` + `Char` instructions.
  UnicodeSet(UnicodeSetClass),
  UnicodeProperty(UnicodeProperty),
  /// RegExp `v` flag Unicode properties-of-strings (`\p{RGI_Emoji}` etc).
  UnicodeStringProperty(UnicodeStringProperty),
  AssertStart,
  AssertEnd,
  WordBoundary { negated: bool },
  Save(usize),
  BackRef(u32),
  NamedBackRef(u32),
  Split(usize, usize),
  Jump(usize),
  /// Resets a quantifier's runtime state (`count`/`last_pos`) for a fresh entry into the quantified
  /// expression.
  ///
  /// Quantifier repetition counts are stored in the executing [`ExecState`]. When the same
  /// quantifier is re-entered (e.g. due to an enclosing quantified group), those counters must be
  /// cleared so the quantifier can match again from scratch. The `RepeatEnd` loop backedge jumps
  /// directly to [`Inst::RepeatStart`] and intentionally bypasses this instruction so individual
  /// iterations do not reset the counter.
  RepeatReset { id: usize },
  RepeatStart {
    id: usize,
    min: u32,
    max: Option<u32>,
    greedy: bool,
    exit: usize,
    clear_from_slot: usize,
    clear_to_slot: usize,
  },
  RepeatEnd {
    start: usize,
  },
  LookAhead {
    program: Box<RegExpProgram>,
    negative: bool,
  },
  LookBehind {
    program: Box<RegExpProgram>,
    negative: bool,
  },
  Match,
}

#[derive(Debug, Clone, Copy)]
struct UnicodeProperty {
  prop: ResolvedCodePointProperty,
  negated: bool,
}

#[derive(Debug, Clone)]
struct CharClass {
  negated: bool,
  items: Vec<CharClassItem>,
}

impl CharClass {
  fn heap_size_bytes(&self) -> usize {
    self
      .items
      .capacity()
      .saturating_mul(mem::size_of::<CharClassItem>())
  }

  fn try_clone(&self) -> Result<Self, RegExpCompileError> {
    let mut items: Vec<CharClassItem> = Vec::new();
    items
      .try_reserve_exact(self.items.len())
      .map_err(|_| RegExpCompileError::OutOfMemory)?;
    items.extend_from_slice(&self.items);
    Ok(Self {
      negated: self.negated,
      items,
    })
  }

  fn matches(&self, u: u32, flags: RegExpFlags) -> bool {
    let mut any = false;
    for item in self.items.iter() {
      if item.matches(u, flags) {
        any = true;
        break;
      }
    }
    if self.negated { !any } else { any }
  }
}

#[derive(Debug, Clone)]
struct UnicodeSetClass {
  /// String elements with length > 1 (UTF-16 code units), compiled into a trie for fast prefix
  /// matching.
  strings: StringTrie,
  /// Single-code-unit elements (including `\q{...}` entries of length 1).
  single: CharClass,
  /// Whether the class contains the empty-string element.
  has_empty: bool,
}

impl UnicodeSetClass {
  fn heap_size_bytes(&self) -> usize {
    self
      .strings
      .heap_size_bytes()
      .saturating_add(self.single.heap_size_bytes())
  }

  fn try_clone(&self) -> Result<Self, RegExpCompileError> {
    Ok(Self {
      strings: self.strings.try_clone()?,
      single: self.single.try_clone()?,
      has_empty: self.has_empty,
    })
  }
}

/// A compact trie over UTF-16 code units used to match string elements inside `/v` character
/// classes.
#[derive(Debug, Clone)]
struct StringTrie {
  nodes: Vec<StringTrieNode>,
  edges: Vec<StringTrieEdge>,
}

impl StringTrie {
  /// Builds a trie from the provided string elements.
  ///
  /// `strings` should contain only elements with length > 1 (empty and single-unit strings are
  /// handled separately by the `/v` class wrapper).
  fn try_build_from_slices<'s>(
    ctx: &mut CompileCtx<'_>,
    strings: impl IntoIterator<Item = &'s [u16]>,
    ignore_case: bool,
  ) -> Result<Self, RegExpCompileError> {
    #[derive(Debug)]
    struct BuildNode {
      terminal: bool,
      // Sorted by `unit`.
      edges: Vec<(u16, usize)>,
    }

    let mut nodes: Vec<BuildNode> = Vec::new();
    ctx.vec_try_push(
      &mut nodes,
      BuildNode {
        terminal: false,
        edges: Vec::new(),
      },
    )?;

    for (s_i, s) in strings.into_iter().enumerate() {
      if s_i != 0 {
        ctx.tick_every(s_i)?;
      }

      let mut node_idx: usize = 0;
      for (u_i, &u_raw) in s.iter().enumerate() {
        if u_i != 0 {
          ctx.tick_every(u_i)?;
        }

        let u = if ignore_case {
          ascii_lower(u_raw)
        } else {
          u_raw
        };

        let (edge_pos, existing_target) = {
          let node = nodes.get(node_idx).ok_or(RegExpCompileError::OutOfMemory)?;
          match node.edges.binary_search_by_key(&u, |(unit, _)| *unit) {
            Ok(pos) => (pos, Some(node.edges[pos].1)),
            Err(pos) => (pos, None),
          }
        };

        if let Some(target) = existing_target {
          node_idx = target;
          continue;
        }

        let new_idx = nodes.len();
        ctx.vec_try_push(
          &mut nodes,
          BuildNode {
            terminal: false,
            edges: Vec::new(),
          },
        )?;

        let node = nodes.get_mut(node_idx).ok_or(RegExpCompileError::OutOfMemory)?;
        let required_len = node
          .edges
          .len()
          .checked_add(1)
          .ok_or(RegExpCompileError::OutOfMemory)?;
        ctx.reserve_vec_to_len(&mut node.edges, required_len)?;
        node.edges.insert(edge_pos, (u, new_idx));
        node_idx = new_idx;
      }

      if let Some(node) = nodes.get_mut(node_idx) {
        node.terminal = true;
      }
    }

    // Flatten the builder nodes into the compact `nodes` + `edges` representation.
    let mut total_edges: usize = 0;
    for (i, n) in nodes.iter().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }
      total_edges = total_edges.saturating_add(n.edges.len());
    }

    let mut out_nodes: Vec<StringTrieNode> = Vec::new();
    ctx.reserve_vec_to_len(&mut out_nodes, nodes.len())?;
    let mut out_edges: Vec<StringTrieEdge> = Vec::new();
    ctx.reserve_vec_to_len(&mut out_edges, total_edges)?;

    for (n_i, n) in nodes.into_iter().enumerate() {
      if n_i != 0 {
        ctx.tick_every(n_i)?;
      }

      let edge_start = out_edges.len();
      let edge_len = n.edges.len();
      for (e_i, (unit, target)) in n.edges.into_iter().enumerate() {
        if e_i != 0 {
          ctx.tick_every(e_i)?;
        }
        out_edges.push(StringTrieEdge { unit, target });
      }

      out_nodes.push(StringTrieNode {
        edge_start,
        edge_len,
        terminal: n.terminal,
      });
    }

    Ok(Self {
      nodes: out_nodes,
      edges: out_edges,
    })
  }

  fn heap_size_bytes(&self) -> usize {
    self
      .nodes
      .capacity()
      .saturating_mul(mem::size_of::<StringTrieNode>())
      .saturating_add(self.edges.capacity().saturating_mul(mem::size_of::<StringTrieEdge>()))
  }

  fn try_clone(&self) -> Result<Self, RegExpCompileError> {
    let mut nodes: Vec<StringTrieNode> = Vec::new();
    nodes
      .try_reserve_exact(self.nodes.len())
      .map_err(|_| RegExpCompileError::OutOfMemory)?;
    nodes.extend_from_slice(&self.nodes);

    let mut edges: Vec<StringTrieEdge> = Vec::new();
    edges
      .try_reserve_exact(self.edges.len())
      .map_err(|_| RegExpCompileError::OutOfMemory)?;
    edges.extend_from_slice(&self.edges);

    Ok(Self { nodes, edges })
  }

  #[inline]
  fn is_empty(&self) -> bool {
    self.edges.is_empty()
  }

  #[inline]
  fn root(&self) -> usize {
    0
  }

  #[inline]
  fn node_is_terminal(&self, node: usize) -> bool {
    self.nodes.get(node).is_some_and(|n| n.terminal)
  }

  #[inline]
  fn step(&self, node: usize, unit: u16) -> Option<usize> {
    let node = self.nodes.get(node)?;
    let start = node.edge_start;
    let end = start.saturating_add(node.edge_len);
    let slice = self.edges.get(start..end)?;
    match slice.binary_search_by_key(&unit, |e| e.unit) {
      Ok(i) => Some(slice[i].target),
      Err(_) => None,
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct StringTrieNode {
  edge_start: usize,
  edge_len: usize,
  terminal: bool,
}

#[derive(Debug, Clone, Copy)]
struct StringTrieEdge {
  unit: u16,
  target: usize,
}

#[derive(Debug, Clone, Copy)]
enum CharClassItem {
  Char(u32),
  Range(u32, u32),
  Digit { negated: bool },
  Word { negated: bool },
  Space { negated: bool },
  UnicodeProperty(UnicodeProperty),
}

impl CharClassItem {
  fn matches(self, u: u32, flags: RegExpFlags) -> bool {
    match self {
      CharClassItem::Char(c) => canonicalize(flags, c) == canonicalize(flags, u),
      CharClassItem::Range(a, b) => {
        if a <= b {
          if !flags.ignore_case {
            return u >= a && u <= b;
          }

          // Approximation of the spec `CharacterRange` matching behaviour:
          // canonicalize the input and endpoints before comparing.
          let cu = canonicalize(flags, u);
          let ca = canonicalize(flags, a);
          let cb = canonicalize(flags, b);
          cu >= ca && cu <= cb
        } else {
          false
        }
      }
      CharClassItem::Digit { negated } => {
        let is_digit = (b'0' as u32..=b'9' as u32).contains(&u);
        if negated { !is_digit } else { is_digit }
      }
      CharClassItem::Word { negated } => {
        let is_word = u <= 0xFFFF && is_word_unit(u as u16, flags);
        if negated { !is_word } else { is_word }
      }
      CharClassItem::Space { negated } => {
        // `\s` in ECMAScript RegExp matches the union of WhiteSpace and LineTerminator
        // (https://tc39.es/ecma262/#sec-characterclassescape).
        let is_space = u <= 0xFFFF && crate::ops::is_ecma_whitespace_unit(u as u16);
        if negated { !is_space } else { is_space }
      }
      CharClassItem::UnicodeProperty(prop) => prop.matches(u, flags),
    }
  }
}

/// ECMAScript `Canonicalize` abstract operation (Runtime Semantics: Canonicalize).
///
/// This implementation follows ECMA-262 `Canonicalize ( rer, ch )`:
/// - With either Unicode flag (`u` or `v`) and ignoreCase, use CaseFolding.txt
///   **simple/common** mappings (no full case folding).
/// - With ignoreCase and *no* Unicode flag, use `toUppercase` with the spec's
///   single-code-unit + ASCII-guard rules.
#[inline]
fn canonicalize(flags: RegExpFlags, ch: u32) -> u32 {
  // 1. If IgnoreCase is false, return ch.
  if !flags.ignore_case {
    return ch;
  }

  // 2. If HasEitherUnicodeFlag(rer) is true, apply simple/common case folding.
  if flags.has_either_unicode_flag() {
    // Canonicalize operates on Unicode code points. Surrogate code points are not
    // valid Unicode scalar values and canonicalize to themselves.
    if (0xD800..=0xDFFF).contains(&ch) || ch > 0x10FFFF {
      return ch;
    }
    return crate::regexp_case_fold(ch);
  }

  // 3. Otherwise, ignoreCase is true with no Unicode flags; treat `ch` as a UTF-16 code unit.
  if ch > 0xFFFF {
    // Should be unreachable (we canonicalize UTF-16 units), but keep the function total.
    return ch;
  }
  let cu = ch as u16;

  // Surrogate code units are not Unicode scalar values; canonicalize to themselves.
  let Some(scalar) = char::from_u32(cu as u32) else {
    return ch;
  };

  // Compute toUppercase (Unicode Default Case Conversion).
  let mut upper_iter = scalar.to_uppercase();
  let Some(upper0) = upper_iter.next() else {
    return ch;
  };
  if upper_iter.next().is_some() {
    // Not exactly one code unit.
    return ch;
  }
  let upper_cp = upper0 as u32;
  if upper_cp > 0xFFFF {
    // Uppercase expands to a surrogate pair (two code units).
    return ch;
  }
  let upper_cu = upper_cp as u16;

  // Spec guard: if original >= 128 and the mapping is ASCII, keep original.
  if cu >= 128 && upper_cu < 128 {
    return ch;
  }
  upper_cu as u32
}

#[inline]
fn canonicalize_utf16_unit(flags: RegExpFlags, unit: u16) -> u32 {
  canonicalize(flags, unit as u32)
}

#[inline]
fn canonical_eq(a: u16, b: u16, flags: RegExpFlags) -> bool {
  canonicalize_utf16_unit(flags, a) == canonicalize_utf16_unit(flags, b)
}

#[inline]
fn ascii_lower(u: u16) -> u16 {
  if (b'A' as u16..=b'Z' as u16).contains(&u) {
    u + 32
  } else {
    u
  }
}

fn eq_ascii_ignore_case(units: &[u16], ascii: &[u8]) -> bool {
  if units.len() != ascii.len() {
    return false;
  }
  units
    .iter()
    .copied()
    .zip(ascii.iter().copied())
    .all(|(u, b)| {
      if u > 0x7F {
        return false;
      }
      ascii_lower(u) == ascii_lower(b as u16)
    })
}

fn is_regexp_identifier_start_ascii(u: u16) -> bool {
  u == (b'$' as u16)
    || u == (b'_' as u16)
    || (b'A' as u16..=b'Z' as u16).contains(&u)
    || (b'a' as u16..=b'z' as u16).contains(&u)
}

fn is_regexp_identifier_continue_ascii(u: u16) -> bool {
  is_regexp_identifier_start_ascii(u) || (b'0' as u16..=b'9' as u16).contains(&u)
}

fn decode_code_point(input: &[u16], pos: usize, unicode: bool) -> Option<(u32, usize)> {
  let u = *input.get(pos)? as u32;
  if !unicode {
    return Some((u, 1));
  }
  // UnicodeMode: treat surrogate pairs as a single code point.
  if (0xD800..=0xDBFF).contains(&u) && pos + 1 < input.len() {
    let u2 = input[pos + 1] as u32;
    if (0xDC00..=0xDFFF).contains(&u2) {
      let lead = u - 0xD800;
      let trail = u2 - 0xDC00;
      let cp = 0x10000 + (lead << 10) + trail;
      return Some((cp, 2));
    }
  }
  Some((u, 1))
}

fn decode_prev_code_point(input: &[u16], pos: usize, unicode: bool) -> Option<(u32, usize)> {
  let end = pos.checked_sub(1)?;
  let u = input[end] as u32;
  if !unicode {
    return Some((u, 1));
  }
  // UnicodeMode: treat surrogate pairs as a single code point.
  if (0xDC00..=0xDFFF).contains(&u) && end >= 1 {
    let lead_u = input[end - 1] as u32;
    if (0xD800..=0xDBFF).contains(&lead_u) {
      let lead = lead_u - 0xD800;
      let trail = u - 0xDC00;
      let cp = 0x10000 + (lead << 10) + trail;
      return Some((cp, 2));
    }
  }
  Some((u, 1))
}

#[inline]
fn is_utf16_high_surrogate(u: u16) -> bool {
  (0xD800..=0xDBFF).contains(&u)
}

#[inline]
fn is_utf16_low_surrogate(u: u16) -> bool {
  (0xDC00..=0xDFFF).contains(&u)
}

/// Returns true if `index` is a valid UTF-16 code point boundary, i.e. it does not point to the
/// second code unit of a surrogate pair.
#[inline]
fn is_utf16_code_point_boundary(input: &[u16], index: usize) -> bool {
  if index == 0 || index >= input.len() {
    return true;
  }
  // Disallow indices that point at a trailing surrogate that has a corresponding leading surrogate.
  !(is_utf16_high_surrogate(input[index - 1]) && is_utf16_low_surrogate(input[index]))
}

/// Decode the UTF-16 code point starting at `index`, reading at most until `end` (exclusive).
///
/// This follows ECMA-262 `CodePointAt`/`StringToCodePoints` semantics:
/// - If the current code unit is a leading surrogate and the next code unit (within bounds) is a
///   trailing surrogate, decode as a single supplementary code point and consume 2 code units.
/// - Otherwise decode the single code unit as a code point and consume 1 code unit.
#[inline]
fn utf16_code_point_at_bounded(units: &[u16], index: usize, end: usize) -> (u32, usize) {
  debug_assert!(index <= end);
  if index >= end {
    return (0, 0);
  }
  let u = units[index];
  if is_utf16_high_surrogate(u) && index + 1 < end {
    let u2 = units[index + 1];
    if is_utf16_low_surrogate(u2) {
      let high = (u as u32) - 0xD800;
      let low = (u2 as u32) - 0xDC00;
      let cp = 0x10000 + ((high << 10) | low);
      return (cp, 2);
    }
  }
  (u as u32, 1)
}

fn is_line_terminator_unit(u: u16) -> bool {
  matches!(u, 0x000A | 0x000D | 0x2028 | 0x2029)
}

#[inline]
fn is_line_terminator(cp: u32) -> bool {
  matches!(cp, 0x000A | 0x000D | 0x2028 | 0x2029)
}

#[inline]
fn utf16_decode_surrogate_pair(high: u16, low: u16) -> u32 {
  debug_assert!((0xD800..=0xDBFF).contains(&high));
  debug_assert!((0xDC00..=0xDFFF).contains(&low));
  let high = high as u32;
  let low = low as u32;
  0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
}

#[inline]
fn utf16_encode_code_point(cp: u32, out: &mut [u16; 2]) -> usize {
  debug_assert!(cp <= 0x10FFFF);
  if cp <= 0xFFFF {
    out[0] = cp as u16;
    1
  } else {
    let cp = cp - 0x10000;
    out[0] = 0xD800 + ((cp >> 10) as u16);
    out[1] = 0xDC00 + ((cp & 0x3FF) as u16);
    2
  }
}
fn is_ascii_letter(u: u16) -> bool {
  (b'a' as u16..=b'z' as u16).contains(&u) || (b'A' as u16..=b'Z' as u16).contains(&u)
}

fn is_syntax_character(u: u16) -> bool {
  matches!(
    u,
    x if x == (b'^' as u16)
      || x == (b'$' as u16)
      || x == (b'\\' as u16)
      || x == (b'.' as u16)
      || x == (b'*' as u16)
      || x == (b'+' as u16)
      || x == (b'?' as u16)
      || x == (b'(' as u16)
      || x == (b')' as u16)
      || x == (b'[' as u16)
      || x == (b']' as u16)
      || x == (b'{' as u16)
      || x == (b'}' as u16)
      || x == (b'|' as u16)
  )
}

#[inline]
fn is_basic_word_unit(u: u16) -> bool {
  matches!(u, 0x0030..=0x0039)
    || matches!(u, 0x0061..=0x007A)
    || matches!(u, 0x0041..=0x005A)
    || u == (b'_' as u16)
}

#[inline]
fn is_word_unit(u: u16, flags: RegExpFlags) -> bool {
  if is_basic_word_unit(u) {
    return true;
  }
  // WordCharacters(rer) adds `extraWordChars` only when ignoreCase and either
  // Unicode flag are present.
  if !flags.ignore_case || !flags.has_either_unicode_flag() {
    return false;
  }

  let cu = canonicalize_utf16_unit(flags, u);
  matches!(cu, 0x0030..=0x0039)
    || matches!(cu, 0x0061..=0x007A)
    || matches!(cu, 0x0041..=0x005A)
    || cu == 0x005F
}

fn is_word_boundary(input: &[u16], pos: usize, flags: RegExpFlags) -> bool {
  let left = pos.checked_sub(1).and_then(|i| input.get(i)).copied();
  let right = input.get(pos).copied();
  let left_word = left.is_some_and(|u| is_word_unit(u, flags));
  let right_word = right.is_some_and(|u| is_word_unit(u, flags));
  left_word != right_word
}

pub(crate) fn advance_string_index(input: &[u16], index: usize, unicode: bool) -> usize {
  if index >= input.len() {
    return index.saturating_add(1);
  }
  if !unicode {
    return index.saturating_add(1);
  }
  let (_, len) = utf16_code_point_at_bounded(input, index, input.len());
  index.saturating_add(len)
}

impl UnicodeProperty {
  #[inline]
  fn matches(self, cp: u32, flags: RegExpFlags) -> bool {
    let mut ok = unicode_property_matches(self.prop, cp, flags);
    if self.negated {
      ok = !ok;
    }
    ok
  }
}

#[inline]
fn unicode_property_contains(prop: ResolvedCodePointProperty, cp: u32) -> bool {
  crate::regexp_unicode_tables::contains_code_point(prop, cp)
}

fn unicode_property_matches(prop: ResolvedCodePointProperty, cp: u32, flags: RegExpFlags) -> bool {
  if unicode_property_contains(prop, cp) {
    return true;
  }
  if !flags.ignore_case || !flags.has_either_unicode_flag() {
    return false;
  }

  let canonical = canonicalize(flags, cp);
  if unicode_property_contains(prop, canonical) {
    return true;
  }

  let Some(canonical_ch) = char::from_u32(canonical) else {
    // Surrogates and out-of-range code points have no case mappings.
    return false;
  };

  struct CharSink {
    buf: [char; 64],
    len: usize,
  }

  impl CharSink {
    fn new() -> Self {
      Self {
        buf: ['\0'; 64],
        len: 0,
      }
    }

    fn iter(&self) -> impl Iterator<Item = char> + '_ {
      self.buf[..self.len].iter().copied()
    }
  }

  impl ClosureSink for CharSink {
    #[inline]
    fn add_char(&mut self, c: char) {
      if self.len < self.buf.len() {
        self.buf[self.len] = c;
        self.len += 1;
      }
    }

    #[inline]
    fn add_string(&mut self, _string: &str) {
      // Ignore multi-code-point expansions; the RegExp VM matches one code point at a time.
    }
  }

  let cm = CaseMapperBorrowed::new();
  let mut sink = CharSink::new();
  cm.add_case_closure_to(canonical_ch, &mut sink);
  for other in sink.iter() {
    let other_cp = other as u32;
    if canonicalize(flags, other_cp) != canonical {
      continue;
    }
    if unicode_property_contains(prop, other_cp) {
      return true;
    }
  }

  false
}
// --- Parser + compiler ---

#[derive(Debug, Clone)]
struct Disjunction {
  alts: Vec<Alternative>,
}

#[derive(Debug, Clone)]
struct Alternative {
  terms: Vec<Term>,
}

#[derive(Debug, Clone)]
enum Term {
  Assertion(Assertion),
  Atom(Atom, Option<Quantifier>),
}

#[derive(Debug, Clone)]
enum Assertion {
  Start,
  End,
  WordBoundary,
  NotWordBoundary,
  LookAhead { negative: bool, disj: Disjunction },
  LookBehind { negative: bool, disj: Disjunction },
}

#[derive(Debug, Clone)]
enum Atom {
  /// A literal pattern character.
  ///
  /// In non-UnicodeMode this represents a single UTF-16 code unit.
  /// In UnicodeMode (`/u` or `/v`), this represents a single Unicode code point (which may require
  /// a surrogate pair in the input string).
  Literal(u32),
  Any,
  Class(CharClass),
  UnicodeSet(UnicodeSetClass),
  UnicodeProperty(UnicodeProperty),
  UnicodeStringProperty(UnicodeStringProperty),
  /// Lookahead assertion that can be quantified in non-UnicodeMode (Annex B).
  ///
  /// This is a zero-width matcher (does not advance the input position), but it can contain
  /// capturing groups whose slots must be reset between iterations when quantified.
  LookAhead {
    negative: bool,
    /// Inclusive range of capture-group indices contained within this assertion.
    ///
    /// A value of `0` means "no capture groups" (capture-group indices are 1-based).
    capture_range_start: u32,
    capture_range_end: u32,
    disj: Disjunction,
  },
  Group {
    capture: Option<u32>,
    /// Inclusive range of capture-group indices contained within this group.
    ///
    /// Stored so quantified groups can clear their own capture slots on each iteration without
    /// rescanning the full sub-AST.
    ///
    /// A value of `0` means "no capture groups" (capture-group indices are 1-based).
    capture_range_start: u32,
    capture_range_end: u32,
    disj: Disjunction,
  },
  BackRef(u32),
  NamedBackRef(Vec<u16>),
}

#[derive(Debug, Clone, Copy)]
struct Quantifier {
  min: u32,
  max: Option<u32>,
  greedy: bool,
}

/// Conservative upper bound estimate for memory allocated while compiling a RegExp of
/// `pattern_len` UTF-16 code units.
///
/// This is used by call sites to consult `HeapLimits` **before** allocating potentially-large
/// off-heap buffers during RegExp compilation, preventing heap-limit bypass via large patterns.
pub(crate) fn estimated_regexp_compilation_bytes(pattern_len: usize) -> usize {
  // The current compiler is linear in the input length; each code unit can contribute at most a
  // small constant number of AST nodes and VM instructions (plus character-class items). Use a
  // conservative estimate so this remains correct even if the compiler gains new features.
  const INSTS_PER_UNIT: usize = 4;
  const TERMS_PER_UNIT: usize = 2;
  const ALTS_PER_UNIT: usize = 2;
  const END_JUMPS_PER_UNIT: usize = 2;
  const CLASS_ITEMS_PER_UNIT: usize = 2;
  const PROGRAMS_PER_UNIT: usize = 1;
  // Named groups bookkeeping (approximate).
  const NAMED_GROUPS_PER_UNIT: usize = 1;
  const NAME_UNITS_PER_UNIT: usize = 1;
  const CAPTURE_INDEX_PER_UNIT: usize = 1;
  let per_unit = INSTS_PER_UNIT
    .saturating_mul(mem::size_of::<Inst>())
    .saturating_add(TERMS_PER_UNIT.saturating_mul(mem::size_of::<Term>()))
    .saturating_add(ALTS_PER_UNIT.saturating_mul(mem::size_of::<Alternative>()))
    .saturating_add(END_JUMPS_PER_UNIT.saturating_mul(mem::size_of::<usize>()))
    .saturating_add(CLASS_ITEMS_PER_UNIT.saturating_mul(mem::size_of::<CharClassItem>()))
    .saturating_add(PROGRAMS_PER_UNIT.saturating_mul(mem::size_of::<RegExpProgram>()))
    .saturating_add(NAMED_GROUPS_PER_UNIT.saturating_mul(mem::size_of::<NamedCaptureGroup>()))
    .saturating_add(NAME_UNITS_PER_UNIT.saturating_mul(mem::size_of::<u16>()))
    .saturating_add(CAPTURE_INDEX_PER_UNIT.saturating_mul(mem::size_of::<u32>()));

  // Fixed overhead for vector headers, builder state, etc.
  const OVERHEAD_BYTES: usize = 8 * 1024;

  pattern_len.saturating_mul(per_unit).saturating_add(OVERHEAD_BYTES)
}

pub(crate) fn compile_regexp_with_budget(
  pattern: &[u16],
  flags: RegExpFlags,
  heap: &Heap,
  tick: &mut dyn FnMut() -> Result<(), VmError>,
) -> Result<RegExpProgram, RegExpCompileError> {
  let mut ctx = CompileCtx::new(heap, tick);
  // Ensure fuel/deadline/interrupt budgets apply during RegExp compilation as well as during
  // execution.
  ctx.tick()?;

  // Annex B disambiguation for DecimalEscape/backreferences in non-UnicodeMode depends on the total
  // number of capturing groups in the pattern (forward references are allowed), so compute it
  // up-front.
  let total_capture_count = count_total_capturing_groups(&mut ctx, pattern, flags)?;
  let mut parser = Parser::new(pattern, flags, total_capture_count);
  let disj = parser.parse_disjunction(&mut ctx, None)?;
  if parser.peek().is_some() {
    return Err(RegExpSyntaxError {
      message: "Invalid regular expression",
    }
    .into());
  }

  // UnicodeMode (`u`/`v`) early errors for DecimalEscape/backreferences.
  // The total capture count is only known after the full parse, so validate here.
  if flags.has_either_unicode_flag() {
    for (i, &backref) in parser.backrefs.iter().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }
      if backref > parser.capture_count {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into());
      }
    }
  }
  let capture_count = parser.capture_count as usize + 1;
  let named_capture_groups = mem::take(&mut parser.named_capture_groups);
  let mut builder = ProgramBuilder::new(capture_count, named_capture_groups, flags.has_either_unicode_flag());
  builder.compile_disjunction(&mut ctx, disj)?;
  builder.emit(&mut ctx, Inst::Match)?;
  Ok(builder.finish())
}

fn count_total_capturing_groups(
  ctx: &mut CompileCtx<'_>,
  units: &[u16],
  flags: RegExpFlags,
) -> Result<u32, RegExpCompileError> {
  let mut i: usize = 0;
  let mut scan_i: usize = 0;
  let mut count: u32 = 0;
  #[derive(Clone, Copy)]
  struct ClassScanState {
    /// Whether this class is negated (`[^...]`).
    negated: bool,
    /// True immediately after `[` (before any class atom), so `^` may still be a negation marker.
    negation_possible: bool,
    /// True before consuming the first class atom (after optional `^`), so an unescaped `]` is a
    /// literal character, not the end of the class.
    first_atom: bool,
  }
  let mut class_stack: Vec<ClassScanState> = Vec::new();
  while i < units.len() {
    // Budget large patterns explicitly: this is an `O(N)` scan over attacker-controlled input.
    if scan_i != 0 {
      ctx.tick_every(scan_i)?;
    }
    scan_i = scan_i.wrapping_add(1);

    let u = units[i];
    if !class_stack.is_empty() {
      // Inside a character class: ignore `(`/`)` entirely, but track `[`/`]` balancing (nested only
      // in `/v` UnicodeSets mode) and the special-case "first `]` is literal" rule.
      let Some(top) = class_stack.last_mut() else {
        return Err(VmError::InvariantViolation("RegExp class stack unexpectedly empty").into());
      };

      if u == (b'\\' as u16) {
        // Escapes inside classes consume one ClassAtom.
        top.negation_possible = false;
        if top.first_atom {
          top.first_atom = false;
        }
        i = i.saturating_add(2);
        continue;
      }

      if top.negation_possible && u == (b'^' as u16) {
        // `^` as the first code unit after `[` is the negation marker; it does not count as an
        // atom, so `]` may still be literal immediately after it.
        top.negated = true;
        top.negation_possible = false;
        i = i.saturating_add(1);
        continue;
      }

      if flags.unicode_sets && u == (b'[' as u16) {
        // Nested character classes are only allowed in `/v` mode. Opening a nested class consumes
        // the first atom in the parent class, if any.
        top.negation_possible = false;
        if top.first_atom {
          top.first_atom = false;
        }
        class_stack
          .try_reserve(1)
          .map_err(|_| RegExpCompileError::OutOfMemory)?;
        class_stack.push(ClassScanState {
          negated: false,
          negation_possible: true,
          first_atom: true,
        });
        i = i.saturating_add(1);
        continue;
      }

      if u == (b']' as u16) {
        if top.first_atom {
          // ECMAScript permits `[^]` (a negated empty character class) by treating the `]`
          // immediately after `^` as the class terminator, *unless* it is followed by another `]`
          // (in which case the first `]` is intended to be a literal: `[^]]`).
          if top.negated && units.get(i + 1).copied() != Some(b']' as u16) {
            class_stack.pop();
            i = i.saturating_add(1);
            continue;
          }
          // Unescaped `]` as the first atom is a literal `]`, not the end of the class.
          top.negation_possible = false;
          top.first_atom = false;
          i = i.saturating_add(1);
          continue;
        }
        // End of this class nesting level.
        class_stack.pop();
        i = i.saturating_add(1);
        continue;
      }

      // Any other code unit inside the class consumes the first atom if we haven't yet.
      top.negation_possible = false;
      if top.first_atom {
        top.first_atom = false;
      }
      i = i.saturating_add(1);
      continue;
    }

    if u == (b'\\' as u16) {
      // Skip the escaped code unit.
      i = i.saturating_add(2);
      continue;
    }

    if u == (b'[' as u16) {
      class_stack
        .try_reserve(1)
        .map_err(|_| RegExpCompileError::OutOfMemory)?;
      class_stack.push(ClassScanState {
        negated: false,
        negation_possible: true,
        first_atom: true,
      });
      i = i.saturating_add(1);
      continue;
    }

    if u == (b'(' as u16) {
      let is_capture = if units.get(i + 1) == Some(&(b'?' as u16)) {
        match units.get(i + 2).copied() {
          // Non-capturing group and lookahead assertions.
          Some(x) if x == (b':' as u16) || x == (b'=' as u16) || x == (b'!' as u16) => false,
          // Lookbehind `(?<=` / `(?<!` and named captures `(?<name>...)`.
          Some(x) if x == (b'<' as u16) => match units.get(i + 3).copied() {
            Some(y) if y == (b'=' as u16) || y == (b'!' as u16) => false,
            // Treat any other `(?<...` as a capture group (best-effort; invalid names will be
            // rejected by the real parser).
            Some(_) => true,
            None => false,
          },
          _ => false,
        }
      } else {
        true
      };
      if is_capture {
        count = count.saturating_add(1);
      }
    }

    i = i.saturating_add(1);
  }
  Ok(count)
}

pub(crate) fn compile_regexp(
  pattern: &[u16],
  flags: RegExpFlags,
  heap: &Heap,
) -> Result<RegExpProgram, RegExpCompileError> {
  let mut tick = || Ok(());
  compile_regexp_with_budget(pattern, flags, heap, &mut tick)
}

struct Parser<'a> {
  units: &'a [u16],
  idx: usize,
  flags: RegExpFlags,
  /// Total number of capturing groups in the entire pattern.
  ///
  /// In non-UnicodeMode (no `/u` or `/v`), DecimalEscape parsing uses this to decide whether `\1`
  /// etc is a backreference (including forward references) or an Annex B legacy octal / identity
  /// escape.
  total_capture_count: u32,
  capture_count: u32,
  named_capture_groups: Vec<NamedCaptureGroup>,
  backrefs: Vec<u32>,
}

#[inline]
fn is_decimal_digit(u: u16) -> bool {
  (b'0' as u16..=b'9' as u16).contains(&u)
}

#[inline]
fn is_octal_digit(u: u16) -> bool {
  (b'0' as u16..=b'7' as u16).contains(&u)
}

impl<'a> Parser<'a> {
  fn new(units: &'a [u16], flags: RegExpFlags, total_capture_count: u32) -> Self {
    Self {
      units,
      idx: 0,
      flags,
      total_capture_count,
      capture_count: 0,
      named_capture_groups: Vec::new(),
      backrefs: Vec::new(),
    }
  }

  fn peek(&self) -> Option<u16> {
    self.units.get(self.idx).copied()
  }

  fn next(&mut self) -> Option<u16> {
    let u = self.peek()?;
    self.idx += 1;
    Some(u)
  }

  fn eat(&mut self, ch: u16) -> bool {
    if self.peek() == Some(ch) {
      self.idx += 1;
      true
    } else {
      false
    }
  }

  /// Parse the Annex B `LegacyOctalEscapeSequence` value after having consumed the first octal
  /// digit (`0`-`7`).
  ///
  /// This consumes the correct number of additional octal digits:
  /// - Leading `0`-`3`: up to 2 more digits (max 3 digits total).
  /// - Leading `4`-`7`: at most 1 more digit (max 2 digits total).
  /// - 1-digit form is only used when the following code unit is not an octal digit.
  fn parse_legacy_octal_escape_after_first(
    &mut self,
    first_digit: u16,
  ) -> Result<u32, RegExpCompileError> {
    debug_assert!(is_octal_digit(first_digit));
    let mut value: u32 = (first_digit - (b'0' as u16)) as u32;

    if (b'0' as u16..=b'3' as u16).contains(&first_digit) {
      if let Some(d1) = self.peek() {
        if is_octal_digit(d1) {
          self.next();
          value = value
            .saturating_mul(8)
            .saturating_add((d1 - (b'0' as u16)) as u32);
          if let Some(d2) = self.peek() {
            if is_octal_digit(d2) {
              self.next();
              value = value
                .saturating_mul(8)
                .saturating_add((d2 - (b'0' as u16)) as u32);
            }
          }
        }
      }
    } else {
      // `4`-`7`: exactly 2 digits when there is a following octal digit.
      if let Some(d1) = self.peek() {
        if is_octal_digit(d1) {
          self.next();
          value = value
            .saturating_mul(8)
            .saturating_add((d1 - (b'0' as u16)) as u32);
        }
      }
    }

    Ok(value)
  }

  fn parse_disjunction(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    terminator: Option<u16>,
  ) -> Result<Disjunction, RegExpCompileError> {
    let mut alts: Vec<Alternative> = Vec::new();
    let first = self.parse_alternative(ctx, terminator)?;
    ctx.vec_try_push(&mut alts, first)?;
    let mut alt_i: usize = 0;
    while self.eat(b'|' as u16) {
      alt_i = alt_i.wrapping_add(1);
      if alt_i != 0 {
        ctx.tick_every(alt_i)?;
      }
      let alt = self.parse_alternative(ctx, terminator)?;
      ctx.vec_try_push(&mut alts, alt)?;
    }
    Ok(Disjunction { alts })
  }

  fn parse_alternative(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    terminator: Option<u16>,
  ) -> Result<Alternative, RegExpCompileError> {
    let mut terms: Vec<Term> = Vec::new();
    let mut term_i: usize = 0;
    loop {
      let Some(u) = self.peek() else { break };
      if Some(u) == terminator || u == (b'|' as u16) {
        break;
      }
      // Special terminator-only handling: unmatched `)` is invalid at the top-level.
      if u == (b')' as u16) {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
          .into());
      }
      if term_i != 0 {
        ctx.tick_every(term_i)?;
      }
      term_i = term_i.wrapping_add(1);
      let term = self.parse_term(ctx, terminator)?;
      ctx.vec_try_push(&mut terms, term)?;
    }
    Ok(Alternative { terms })
  }

  fn parse_term(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    terminator: Option<u16>,
  ) -> Result<Term, RegExpCompileError> {
    let Some(u) = self.peek() else {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    };
    let unicode_mode = self.flags.has_either_unicode_flag();

    // Lookaround assertions: lookahead `(?=...)` / `(?!...)`, lookbehind `(?<=...)` / `(?<!...)`.
    if u == (b'(' as u16) {
      if self.units.get(self.idx + 1) == Some(&(b'?' as u16)) {
        // Lookbehind assertions: `(?<=...)` / `(?<!...)`.
        if self.units.get(self.idx + 2) == Some(&(b'<' as u16)) {
          if let Some(kind) = self.units.get(self.idx + 3).copied() {
            if kind == (b'=' as u16) || kind == (b'!' as u16) {
              // Consume "(?<=" / "(?<!".
              self.idx += 4;
              let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
              if !self.eat(b')' as u16) {
                return Err(RegExpSyntaxError {
                  message: "Unterminated group",
                }
                .into());
              }
              return Ok(Term::Assertion(Assertion::LookBehind {
                negative: kind == (b'!' as u16),
                disj,
              }));
            }
          }
        }

        if let Some(kind) = self.units.get(self.idx + 2).copied() {
          if kind == (b'=' as u16) || kind == (b'!' as u16) {
            let captures_before = self.capture_count;
            // Consume "(?=" / "(?!".
            self.idx += 3;
            let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
            if !self.eat(b')' as u16) {
              return Err(RegExpSyntaxError {
                message: "Unterminated group",
              }
              .into());
            }
            let negative = kind == (b'!' as u16);

            // Annex B: In non-UnicodeMode, lookahead assertions can be quantified.
            // Treat them as an atom so the normal quantifier parser/VM machinery applies.
            if !unicode_mode {
              let captures_after = self.capture_count;
              let (capture_range_start, capture_range_end) = if captures_after > captures_before {
                (
                  captures_before.saturating_add(1),
                  captures_after,
                )
              } else {
                (0, 0)
              };
              let atom = Atom::LookAhead {
                negative,
                capture_range_start,
                capture_range_end,
                disj,
              };
              let quant = self.parse_quantifier_if_present(ctx)?;
              return Ok(Term::Atom(atom, quant));
            }

            return Ok(Term::Assertion(Assertion::LookAhead { negative, disj }));
          }
        }
      }
    }

    // Assertions.
    match u {
      x if x == (b'^' as u16) => {
        self.next();
        return Ok(Term::Assertion(Assertion::Start));
      }
      x if x == (b'$' as u16) => {
        self.next();
        return Ok(Term::Assertion(Assertion::End));
      }
      x if x == (b'\\' as u16) => {
        // Might be a boundary assertion.
        let save = self.idx;
        self.next();
        let Some(next) = self.next() else {
          return Err(RegExpSyntaxError {
            message: "Invalid escape",
          }
          .into());
        };
        match next {
          x if x == (b'b' as u16) => return Ok(Term::Assertion(Assertion::WordBoundary)),
          x if x == (b'B' as u16) => return Ok(Term::Assertion(Assertion::NotWordBoundary)),
          _ => {
            // Not an assertion; rewind and parse as atom.
            self.idx = save;
          }
        }
      }
      _ => {}
    }

    // Atom.
    let atom = self.parse_atom(ctx, terminator)?;
    let quant = self.parse_quantifier_if_present(ctx)?;
    Ok(Term::Atom(atom, quant))
  }

  fn parse_atom(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    terminator: Option<u16>,
  ) -> Result<Atom, RegExpCompileError> {
    let Some(u) = self.next() else {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    };
    let unicode_mode = self.flags.has_either_unicode_flag();

    match u {
      x if x == (b'.' as u16) => Ok(Atom::Any),
      x if x == (b'[' as u16) => self.parse_class(ctx),
      x if x == (b'(' as u16) => self.parse_group(ctx),
      x if x == (b'\\' as u16) => self.parse_escape_atom(ctx),
      x if x == (b'*' as u16) || x == (b'+' as u16) || x == (b'?' as u16) => {
        Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into())
      }
      x if x == (b'{' as u16) => {
        // Annex B (extended pattern characters) treats `{` as a literal atom when it is not parsed
        // as a quantifier. This extension is not applied in UnicodeMode (`u` or `v`).
        if unicode_mode {
          Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into())
        } else {
          Ok(Atom::Literal(x as u32))
        }
      }
      x if x == (b']' as u16) && unicode_mode => Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into()),
      x if x == (b'}' as u16) && unicode_mode => Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into()),
      x if x == (b')' as u16) => {
        if terminator == Some(x) {
          // Caller should have stopped before consuming.
          Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into())
        } else {
          Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into())
        }
      }
      x => {
        // In UnicodeMode (`u`/`v`), pattern source text is interpreted as Unicode code points, so
        // UTF-16 surrogate pairs in the pattern represent a single atom.
        if unicode_mode
          && (0xD800..=0xDBFF).contains(&x)
          && self.peek().is_some_and(|u2| (0xDC00..=0xDFFF).contains(&u2))
        {
          let Some(low) = self.next() else {
            return Ok(Atom::Literal(x as u32));
          };
          Ok(Atom::Literal(utf16_decode_surrogate_pair(x, low)))
        } else {
          Ok(Atom::Literal(x as u32))
        }
      }
    }
  }

  fn parse_group(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Atom, RegExpCompileError> {
    // `(` has already been consumed.
    let captures_before = self.capture_count;
    if self.eat(b'?' as u16) {
      let Some(next) = self.next() else {
        return Err(RegExpSyntaxError { message: "Invalid group" }.into());
      };
      match next {
        x if x == (b':' as u16) => {
          // Non-capturing group.
          let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
          if !self.eat(b')' as u16) {
            return Err(RegExpSyntaxError {
              message: "Unterminated group",
            }
            .into());
          }
          let captures_after = self.capture_count;
          let (capture_range_start, capture_range_end) = if captures_after > captures_before {
            (
              captures_before.saturating_add(1),
              captures_after,
            )
          } else {
            (0, 0)
          };
          Ok(Atom::Group {
            capture: None,
            capture_range_start,
            capture_range_end,
            disj,
          })
        }
        x if x == (b'<' as u16) => {
          // Named capturing group: `(?<name>...)`.
          let name = self.parse_group_name(ctx)?;
          self.capture_count = self.capture_count.saturating_add(1);
          let idx = self.capture_count;
          self.register_named_capture_group(ctx, name, idx)?;
          let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
          if !self.eat(b')' as u16) {
            return Err(RegExpSyntaxError {
              message: "Unterminated group",
            }
            .into());
          }
          let captures_after = self.capture_count;
          Ok(Atom::Group {
            capture: Some(idx),
            capture_range_start: idx,
            capture_range_end: captures_after,
            disj,
          })
        }
        _ => Err(RegExpSyntaxError { message: "Invalid group" }.into()),
      }
    } else {
      // Capturing group.
      self.capture_count = self.capture_count.saturating_add(1);
      let idx = self.capture_count;
      let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
      if !self.eat(b')' as u16) {
        return Err(RegExpSyntaxError {
          message: "Unterminated group",
        }
        .into());
      }
      let captures_after = self.capture_count;
      Ok(Atom::Group {
        capture: Some(idx),
        capture_range_start: idx,
        capture_range_end: captures_after,
        disj,
      })
    }
  }

  fn parse_group_name(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Vec<u16>, RegExpCompileError> {
    let mut name: Vec<u16> = Vec::new();
    let mut i: usize = 0;
    loop {
      let Some(u) = self.peek() else {
        return Err(RegExpSyntaxError { message: "Invalid group" }.into());
      };
      if u == (b'>' as u16) {
        // Consume `>`.
        self.next();
        break;
      }
      if i != 0 {
        ctx.tick_every(i)?;
      }
      i = i.wrapping_add(1);
      self.next();
      ctx.vec_try_push(&mut name, u)?;
    }

    if name.is_empty() {
      return Err(RegExpSyntaxError { message: "Invalid group" }.into());
    }
    if !is_regexp_identifier_start_ascii(name[0]) {
      return Err(RegExpSyntaxError { message: "Invalid group" }.into());
    }
    for (j, &u) in name.iter().enumerate().skip(1) {
      if j % 32 == 0 {
        ctx.tick()?;
      }
      if !is_regexp_identifier_continue_ascii(u) {
        return Err(RegExpSyntaxError { message: "Invalid group" }.into());
      }
    }
    Ok(name)
  }

  fn register_named_capture_group(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    name: Vec<u16>,
    capture_idx: u32,
  ) -> Result<(), RegExpCompileError> {
    for (i, group) in self.named_capture_groups.iter_mut().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }
      if group.name == name {
        ctx.vec_try_push(&mut group.capture_indices, capture_idx)?;
        return Ok(());
      }
    }

    let mut capture_indices: Vec<u32> = Vec::new();
    ctx.vec_try_push(&mut capture_indices, capture_idx)?;
    ctx.vec_try_push(
      &mut self.named_capture_groups,
      NamedCaptureGroup {
        name,
        capture_indices,
      },
    )?;
    Ok(())
  }

  fn parse_class(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Atom, RegExpCompileError> {
    // `[` has already been consumed.
    if self.flags.unicode_sets {
      return self.parse_class_unicode_sets(ctx);
    }
    let mut negated = false;
    if self.eat(b'^' as u16) {
      negated = true;
    }
    let mut items: Vec<CharClassItem> = Vec::new();

    let mut item_i: usize = 0;
    loop {
      let Some(u) = self.peek() else {
        return Err(RegExpSyntaxError {
          message: "Unterminated character class",
        }
        .into());
      };
      if u == (b']' as u16) {
        if item_i == 0 && !negated {
          // Special case: in ECMAScript, `]` can appear unescaped as the first class atom (to match
          // a literal `]`). This must not be confused with the *empty* class `[]` / `[^]`.
          //
          // We disambiguate by scanning for another (unescaped) `]` later in the class. When none
          // exists, this `]` terminates an empty class.
          let mut j = self.idx.saturating_add(1);
          let mut escaped = false;
          let mut has_terminator = false;
          while j < self.units.len() {
            let u2 = self.units[j];
            if escaped {
              escaped = false;
              j = j.saturating_add(1);
              continue;
            }
            if u2 == (b'\\' as u16) {
              escaped = true;
              j = j.saturating_add(1);
              continue;
            }
            if u2 == (b']' as u16) {
              has_terminator = true;
              break;
            }
            j = j.saturating_add(1);
          }
          if has_terminator {
            self.next();
            ctx.vec_try_push(&mut items, CharClassItem::Char(b']' as u32))?;
            item_i = item_i.wrapping_add(1);
            continue;
          }
        }

        // Empty character classes like `[]` and `[^]` are valid in ECMAScript:
        // - `[]` matches nothing.
        // - `[^]` matches any UTF-16 code unit (commonly used as a dotAll workaround).
        self.next();
        break;
      }

      if item_i != 0 {
        ctx.tick_every(item_i)?;
      }
      item_i = item_i.wrapping_add(1);

      let atom = self.parse_class_atom(ctx)?;
      // Range?
      if self.peek() == Some(b'-' as u16) {
        // Only treat as range when there's a following atom before `]`.
        let save = self.idx;
        self.next(); // consume '-'
        if self.peek() == Some(b']' as u16) {
          // Literal '-' at end.
          self.idx = save;
        } else {
          let atom2 = self.parse_class_atom(ctx)?;
          if let (CharClassItem::Char(a), CharClassItem::Char(b)) = (atom, atom2) {
            if a > b {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            ctx.vec_try_push(&mut items, CharClassItem::Range(a, b))?;
            continue;
          } else {
            // In UnicodeMode (`u`/`v`), Annex B forbids class ranges where either endpoint is a
            // character class escape (e.g. `\s`, `\d`, `\w`) or any other multi-character atom.
            //
            // Non-Unicode mode keeps the web-compatible legacy behaviour of treating the `-` as a
            // literal when the range is not well-formed.
            if self.flags.has_either_unicode_flag() {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            // Not a valid range; treat '-' literally and keep both atoms.
            self.idx = save;
          }
        }
      }
      ctx.vec_try_push(&mut items, atom)?;
    }

    Ok(Atom::Class(CharClass { negated, items }))
  }

  fn parse_class_unicode_sets(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Atom, RegExpCompileError> {
    // `[` has already been consumed.
    let mut negated = false;
    if self.eat(b'^' as u16) {
      negated = true;
    }

    // Fast-path for a single character class escape (`[\d]`, `[\S]`, etc).
    //
    // These should behave the same as the corresponding escape outside character classes. In
    // particular, the negated forms (`\D`, `\S`, `\W`) must match non-BMP code points in UnicodeMode
    // (which would otherwise be missed by the current `/v` UnicodeSet `CharSet` representation).
    if self.peek() == Some(b'\\' as u16)
      && matches!(
        self.units.get(self.idx + 1).copied(),
        Some(x)
          if x == (b'd' as u16)
            || x == (b'D' as u16)
            || x == (b'w' as u16)
            || x == (b'W' as u16)
            || x == (b's' as u16)
            || x == (b'S' as u16)
      )
      && self.units.get(self.idx + 2).copied() == Some(b']' as u16)
    {
      self.next(); // consume '\'
      let Some(esc) = self.next() else {
        return Err(RegExpSyntaxError {
          message: "Unterminated character class",
        }
        .into());
      };
      let item = match esc {
        x if x == (b'd' as u16) => CharClassItem::Digit { negated: false },
        x if x == (b'D' as u16) => CharClassItem::Digit { negated: true },
        x if x == (b'w' as u16) => CharClassItem::Word { negated: false },
        x if x == (b'W' as u16) => CharClassItem::Word { negated: true },
        x if x == (b's' as u16) => CharClassItem::Space { negated: false },
        x if x == (b'S' as u16) => CharClassItem::Space { negated: true },
        _ => {
          return Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into());
        }
      };
      let mut items: Vec<CharClassItem> = Vec::new();
      ctx.vec_try_push(&mut items, item)?;
      if !self.eat(b']' as u16) {
        return Err(RegExpSyntaxError {
          message: "Unterminated character class",
        }
        .into());
      }
      return Ok(Atom::Class(CharClass { negated, items }));
    }

    // UnicodeSetsMode allows empty class contents (`[]` / `[^]`).
    let set = if self.peek() == Some(b']' as u16) {
      UnicodeSet::new()
    } else {
      self.parse_class_set_expression(ctx)?
    };

    if !self.eat(b']' as u16) {
      return Err(RegExpSyntaxError {
        message: "Unterminated character class",
      }
      .into());
    }

    // Early error: negated UnicodeSets-mode classes must not contain strings.
    if negated && set.may_contain_strings() {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    }

    self.unicode_set_to_class_atom(ctx, set, negated)
  }

  fn unicode_set_to_class_atom(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    set: UnicodeSet,
    negated: bool,
  ) -> Result<Atom, RegExpCompileError> {
    if set.strings.is_empty() {
      return Ok(Atom::Class(char_set_to_char_class(
        ctx,
        &set.chars,
        &set.supplementary,
        negated,
      )?));
    }

    // Negated classes with strings are an early error and should have been rejected above.
    debug_assert!(!negated, "negated UnicodeSets-mode classes cannot contain strings");

    let has_empty = set.contains_string(&[]);
    let strings = StringTrie::try_build_from_slices(
      ctx,
      set.iter_strings_desc_len().filter(|s| s.len() > 1),
      self.flags.ignore_case,
    )?;
    let single = char_set_to_char_class(ctx, &set.chars, &set.supplementary, false)?;
    Ok(Atom::UnicodeSet(UnicodeSetClass {
      strings,
      single,
      has_empty,
    }))
  }

  fn parse_class_set_expression(&mut self, ctx: &mut CompileCtx<'_>) -> Result<UnicodeSet, RegExpCompileError> {
    // Implements the core of ECMA-262 `ClassSetExpression` for UnicodeSetsMode (`/v`).
    //
    // We follow the grammar:
    //   ClassSetExpression :: ClassUnion | ClassIntersection | ClassSubtraction
    //
    // Disambiguation strategy:
    // - Parse a leading `ClassSetOperand`, then look for `&&` / `--` to decide whether this is an
    //   intersection/subtraction expression.
    // - Otherwise, fall back to parsing a union from the start (so `a-b` parses as a range).
    let start_idx = self.idx;
    let first = self.parse_class_set_operand(ctx)?;

    let is_and_and = self.peek() == Some(b'&' as u16)
      && self.units.get(self.idx + 1).copied() == Some(b'&' as u16)
      && self.units.get(self.idx + 2).copied() != Some(b'&' as u16);
    if is_and_and {
      // ClassIntersection
      self.idx = self.idx.saturating_add(2); // consume `&&`
      let mut out = first;
      let rhs = self.parse_class_set_operand(ctx)?;
      out = out.intersection(ctx, &rhs)?;
      while self.peek() == Some(b'&' as u16)
        && self.units.get(self.idx + 1).copied() == Some(b'&' as u16)
        && self.units.get(self.idx + 2).copied() != Some(b'&' as u16)
      {
        self.idx = self.idx.saturating_add(2);
        let rhs = self.parse_class_set_operand(ctx)?;
        out = out.intersection(ctx, &rhs)?;
      }
      return Ok(out);
    }

    let is_dash_dash =
      self.peek() == Some(b'-' as u16) && self.units.get(self.idx + 1).copied() == Some(b'-' as u16);
    if is_dash_dash {
      // ClassSubtraction
      self.idx = self.idx.saturating_add(2); // consume `--`
      let mut out = first;
      let rhs = self.parse_class_set_operand(ctx)?;
      out = out.difference(ctx, &rhs)?;
      while self.peek() == Some(b'-' as u16) && self.units.get(self.idx + 1).copied() == Some(b'-' as u16) {
        self.idx = self.idx.saturating_add(2);
        let rhs = self.parse_class_set_operand(ctx)?;
        out = out.difference(ctx, &rhs)?;
      }
      return Ok(out);
    }

    // Otherwise: ClassUnion (which includes ranges).
    self.idx = start_idx;
    self.parse_class_union(ctx)
  }

  fn parse_class_union(&mut self, ctx: &mut CompileCtx<'_>) -> Result<UnicodeSet, RegExpCompileError> {
    let mut out = UnicodeSet::new();
    let mut i: usize = 0;
    while let Some(u) = self.peek() {
      if u == (b']' as u16) {
        break;
      }
      if i != 0 {
        ctx.tick_every(i)?;
      }
      i = i.wrapping_add(1);
      let item = self.parse_class_union_item(ctx)?;
      out = out.union(ctx, &item)?;
    }
    Ok(out)
  }

  fn parse_class_union_item(&mut self, ctx: &mut CompileCtx<'_>) -> Result<UnicodeSet, RegExpCompileError> {
    // If this starts with an operand that cannot be a range start, parse it directly.
    match self.peek() {
      Some(x) if x == (b'[' as u16) => return self.parse_nested_class_operand(ctx),
      Some(x) if x == (b'\\' as u16) => {
        let n1 = self.units.get(self.idx + 1).copied();
        let n2 = self.units.get(self.idx + 2).copied();
        if n1 == Some(b'q' as u16) && n2 == Some(b'{' as u16) {
          return self.parse_class_string_disjunction(ctx);
        }
        if is_class_set_escape_class_operand_start(n1) {
          return self.parse_nested_class_operand(ctx);
        }
      }
      _ => {}
    }

    // Range: ClassSetCharacter '-' ClassSetCharacter
    //
    // In UnicodeSets mode we must treat a UTF-16 surrogate pair literal as a single class-set
    // character (a single non-BMP code point). If we parsed the high/low surrogates as separate
    // elements, `/v` matching (which runs in UnicodeMode) would never match non-BMP literals.
    if let (Some(hi), Some(lo)) = (self.peek(), self.units.get(self.idx + 1).copied()) {
      if is_utf16_high_surrogate(hi) && is_utf16_low_surrogate(lo) {
        // Consume both code units and treat them as a single class element.
        self.next();
        self.next();
        let mut set = UnicodeSet::new();
        set.insert_string(ctx, &[hi, lo])?;
        return Ok(set);
      }
    }
    let start = self.parse_class_set_character(ctx)?;
    if self.peek() == Some(b'-' as u16) && self.units.get(self.idx + 1).copied() != Some(b'-' as u16) {
      self.next(); // consume '-'
      let end = self.parse_class_set_character(ctx)?;
      // Ranges involving non-BMP code points are not supported yet; reject them so we don't try to
      // approximate them by UTF-16 code unit ranges.
      if start > 0xFFFF || end > 0xFFFF {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into());
      }
      let start = start as u16;
      let end = end as u16;
      if start > end {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into());
      }
      let mut set = UnicodeSet::new();
      set.chars.insert_range(start, end);
      return Ok(set);
    }

    let mut set = UnicodeSet::new();
    let mut buf = [0u16; 2];
    let len = utf16_encode_code_point(start, &mut buf);
    set.insert_string(ctx, &buf[..len])?;
    Ok(set)
  }

  fn parse_class_set_operand(&mut self, ctx: &mut CompileCtx<'_>) -> Result<UnicodeSet, RegExpCompileError> {
    match self.peek() {
      Some(x) if x == (b'[' as u16) => self.parse_nested_class_operand(ctx),
      Some(x) if x == (b'\\' as u16) => {
        let n1 = self.units.get(self.idx + 1).copied();
        let n2 = self.units.get(self.idx + 2).copied();
        if n1 == Some(b'q' as u16) && n2 == Some(b'{' as u16) {
          return self.parse_class_string_disjunction(ctx);
        }
        if is_class_set_escape_class_operand_start(n1) {
          return self.parse_nested_class_operand(ctx);
        }
        let ch = self.parse_class_set_character(ctx)?;
        let mut set = UnicodeSet::new();
        let mut buf = [0u16; 2];
        let len = utf16_encode_code_point(ch, &mut buf);
        set.insert_string(ctx, &buf[..len])?;
        Ok(set)
      }
      Some(_) => {
        // Same surrogate-pair handling as `parse_class_union_item`.
        if let (Some(hi), Some(lo)) = (self.peek(), self.units.get(self.idx + 1).copied()) {
          if is_utf16_high_surrogate(hi) && is_utf16_low_surrogate(lo) {
            self.next();
            self.next();
            let mut set = UnicodeSet::new();
            set.insert_string(ctx, &[hi, lo])?;
            return Ok(set);
          }
        }
        let ch = self.parse_class_set_character(ctx)?;
        let mut set = UnicodeSet::new();
        let mut buf = [0u16; 2];
        let len = utf16_encode_code_point(ch, &mut buf);
        set.insert_string(ctx, &buf[..len])?;
        Ok(set)
      }
      None => Err(RegExpSyntaxError {
        message: "Unterminated character class",
      }
      .into()),
    }
  }

  fn parse_nested_class_operand(&mut self, ctx: &mut CompileCtx<'_>) -> Result<UnicodeSet, RegExpCompileError> {
    match self.peek() {
      Some(x) if x == (b'[' as u16) => {
        self.next(); // consume '['
        let mut negated = false;
        if self.eat(b'^' as u16) {
          negated = true;
        }

        let mut inner = if self.peek() == Some(b']' as u16) {
          UnicodeSet::new()
        } else {
          self.parse_class_set_expression(ctx)?
        };

        if !self.eat(b']' as u16) {
          return Err(RegExpSyntaxError {
            message: "Unterminated character class",
          }
          .into());
        }

        if negated {
          // Negated nested classes compute a complement set. We currently only implement
          // complement over the BMP `CharSet` universe; reject string elements and supplementary
          // code points to avoid silently producing incorrect results.
          if inner.may_contain_strings() || !inner.supplementary.is_empty() {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          inner.chars = inner.chars.complement();
        }
        Ok(inner)
      }
      Some(x) if x == (b'\\' as u16) => {
        self.next(); // consume '\'
        let Some(e) = self.next() else {
          return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
        };
        let mut set = UnicodeSet::new();

        match e {
          x if x == (b'd' as u16) || x == (b'D' as u16) => {
            for u in b'0'..=b'9' {
              set.insert_char(u as u16);
            }
            if x == (b'D' as u16) {
              set.chars = set.chars.complement();
            }
          }
          x if x == (b'w' as u16) || x == (b'W' as u16) => {
            for u in b'0'..=b'9' {
              set.insert_char(u as u16);
            }
            for u in b'a'..=b'z' {
              set.insert_char(u as u16);
            }
            for u in b'A'..=b'Z' {
              set.insert_char(u as u16);
            }
            set.insert_char(b'_' as u16);
            if x == (b'W' as u16) {
              set.chars = set.chars.complement();
            }
          }
          x if x == (b's' as u16) || x == (b'S' as u16) => {
            // Match the same code units as `CharClassItem::Space`:
            // ECMA-262 `WhiteSpace ∪ LineTerminator`
            // (https://tc39.es/ecma262/#sec-characterclassescape).
            for u in [
              // WhiteSpace
              0x0009u16, // Tab
              0x000Bu16, // VT
              0x000Cu16, // FF
              0x0020u16, // Space
              0x00A0u16, // NBSP
              0x1680u16, // Ogham space mark
              0x202Fu16, // Narrow NBSP
              0x205Fu16, // Medium mathematical space
              0x3000u16, // Ideographic space
              0xFEFFu16, // BOM
              // LineTerminator
              0x000Au16, // LF
              0x000Du16, // CR
              0x2028u16, // Line separator
              0x2029u16, // Paragraph separator
            ] {
              set.insert_char(u);
            }
            for u in 0x2000u16..=0x200Au16 {
              // En quad..Hair space
              set.insert_char(u);
            }
            if x == (b'S' as u16) {
              set.chars = set.chars.complement();
            }
          }
          x if x == (b'p' as u16) || x == (b'P' as u16) => {
            let prop_negated = x == (b'P' as u16);
            if !self.eat(b'{' as u16) {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            let resolved = self.parse_unicode_property_value_expression(ctx)?;
            match resolved {
              ResolvedUnicodeProperty::CodePoint(prop) => {
                match prop {
                  // Common fast-path used heavily by unicodeSets tests.
                  ResolvedCodePointProperty::Binary(BinaryProp::ASCII_Hex_Digit) => {
                    for u in b'0'..=b'9' {
                      set.insert_char(u as u16);
                    }
                    for u in b'A'..=b'F' {
                      set.insert_char(u as u16);
                    }
                    for u in b'a'..=b'f' {
                      set.insert_char(u as u16);
                    }
                  }
                  other => {
                    // Fallback: scan all UTF-16 code units.
                    let mut cu: u32 = 0;
                    let mut i: usize = 0;
                    while cu <= 0xFFFF {
                      if i != 0 {
                        ctx.tick_every(i)?;
                      }
                      i = i.wrapping_add(1);
                      if unicode_table_contains_code_point(other, cu) {
                        set.insert_char(cu as u16);
                      }
                      cu += 1;
                    }
                  }
                }

                if prop_negated {
                  set.chars = set.chars.complement();
                }
              }
              ResolvedUnicodeProperty::String(prop) => {
                // String properties cannot be negated.
                if prop_negated {
                  return Err(RegExpSyntaxError {
                    message: "Invalid regular expression",
                  }
                  .into());
                }

                // For now, only support the subset needed by unicodeSets tests.
                match prop {
                  UnicodeStringProperty::EmojiKeycapSequence => {
                    for &base in &[b'#' as u16, b'*' as u16] {
                      let units = [base, 0xFE0F, 0x20E3];
                      set.insert_string(ctx, &units)?;
                    }
                    for base in b'0'..=b'9' {
                      let units = [base as u16, 0xFE0F, 0x20E3];
                      set.insert_string(ctx, &units)?;
                    }
                  }
                  _ => {
                    return Err(RegExpSyntaxError {
                      message: "Invalid regular expression",
                    }
                    .into());
                  }
                }
              }
            }
          }
          _ => {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
        }

        Ok(set)
      }
      _ => Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into()),
    }
  }

  fn parse_class_string_disjunction(&mut self, ctx: &mut CompileCtx<'_>) -> Result<UnicodeSet, RegExpCompileError> {
    // Grammar: `\q{` ClassStringDisjunctionContents `}`
    // `\` has not been consumed yet.
    if self.next() != Some(b'\\' as u16) || self.next() != Some(b'q' as u16) || self.next() != Some(b'{' as u16) {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    }

    let mut out = UnicodeSet::new();
    let mut cur: Vec<u16> = Vec::new();

    let mut i: usize = 0;
    loop {
      let Some(u) = self.peek() else {
        return Err(RegExpSyntaxError {
          message: "Unterminated character class",
        }
        .into());
      };

      if u == (b'}' as u16) {
        self.next();
        out.insert_string(ctx, &cur)?;
        break;
      }
      if u == (b'|' as u16) {
        self.next();
        out.insert_string(ctx, &cur)?;
        cur.clear();
        continue;
      }

      if i != 0 {
        ctx.tick_every(i)?;
      }
      i = i.wrapping_add(1);
      let ch = self.parse_class_set_character(ctx)?;
      let mut buf = [0u16; 2];
      let len = utf16_encode_code_point(ch, &mut buf);
      for u in buf.into_iter().take(len) {
        ctx.vec_try_push(&mut cur, u)?;
      }
    }

    Ok(out)
  }

  fn parse_class_set_character(&mut self, ctx: &mut CompileCtx<'_>) -> Result<u32, RegExpCompileError> {
    let Some(u) = self.next() else {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    };

    // ClassSetSyntaxCharacter :: one of
    //   `(` `)` `[` `]` `{` `}` `/` `-` `\` `|`
    match u {
      x if x == (b'\\' as u16) => {
        let Some(e) = self.next() else {
          return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
        };
        match e {
          x if x == (b'b' as u16) => Ok(0x0008), // backspace
          x if x == (b'c' as u16) => {
            let Some(next) = self.peek() else {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            };
            if !is_ascii_letter(next) {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            self.next();
            Ok(((next as u8) & 0x1F) as u32)
          }
          x if x == (b'n' as u16) => Ok(0x000A),
          x if x == (b'r' as u16) => Ok(0x000D),
          x if x == (b't' as u16) => Ok(0x0009),
          x if x == (b'v' as u16) => Ok(0x000B),
          x if x == (b'f' as u16) => Ok(0x000C),
          x if x == (b'0' as u16) => {
            // `\0` in UnicodeMode is only valid when not followed by a decimal digit.
            if self.peek().is_some_and(is_decimal_digit) {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            Ok(0x0000)
          }
          x if (b'1' as u16..=b'7' as u16).contains(&x) => Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into()),
          x if x == (b'8' as u16) || x == (b'9' as u16) => Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into()),
          x if x == (b'x' as u16) => {
            let v = self.parse_hex_escape_2(ctx)?;
            Ok(v)
          }
          x if x == (b'u' as u16) => {
            self.parse_unicode_escape(ctx)
          }
          // ClassSetReservedPunctuator (treat as identity escapes).
          x if is_class_set_reserved_punctuator(x) => Ok(x as u32),
          // In UnicodeMode (`/v` implies UnicodeMode), identity escapes are restricted to
          // `SyntaxCharacter` or `/` (IdentityEscape[+UnicodeMode]).
          other => {
            if is_syntax_character(other) || other == (b'/' as u16) {
              Ok(other as u32)
            } else {
              Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into())
            }
          }
        }
      }
      // Syntax characters must be escaped in UnicodeSetsMode.
      x if is_class_set_syntax_character(x) => {
        Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into())
      }
      other => {
        // Reject unescaped reserved double punctuators (e.g. `&&`, `==`, `<<`), which must be
        // escaped in UnicodeSetsMode.
        let next = self.peek();
        let is_reserved_double = next == Some(other) && is_class_set_reserved_double_punctuator(other);
        if is_reserved_double {
          return Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into());
        }
        // In UnicodeMode (`u`/`v`), pattern source text is interpreted as Unicode code points, so
        // UTF-16 surrogate pairs in the pattern represent a single character.
        if self.flags.has_either_unicode_flag()
          && is_utf16_high_surrogate(other)
          && self.peek().is_some_and(is_utf16_low_surrogate)
        {
          let Some(low) = self.next() else {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          };
          return Ok(utf16_decode_surrogate_pair(other, low));
        }
        Ok(other as u32)
      }
    }
  }

  fn parse_class_atom(
    &mut self,
    ctx: &mut CompileCtx<'_>,
  ) -> Result<CharClassItem, RegExpCompileError> {
    let Some(u) = self.next() else {
      return Err(RegExpSyntaxError {
        message: "Invalid character class",
      }
      .into());
    };
    match u {
      x if x == (b'\\' as u16) => {
        let Some(e) = self.next() else {
          return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
        };
        match e {
          x if x == (b'd' as u16) => Ok(CharClassItem::Digit { negated: false }),
          x if x == (b'D' as u16) => Ok(CharClassItem::Digit { negated: true }),
          x if x == (b'w' as u16) => Ok(CharClassItem::Word { negated: false }),
          x if x == (b'W' as u16) => Ok(CharClassItem::Word { negated: true }),
          x if x == (b's' as u16) => Ok(CharClassItem::Space { negated: false }),
          x if x == (b'S' as u16) => Ok(CharClassItem::Space { negated: true }),
          x if x == (b'b' as u16) => Ok(CharClassItem::Char(0x0008)), // backspace
          x if x == (b'c' as u16) => {
            let Some(next) = self.peek() else {
              if self.flags.has_either_unicode_flag() {
                return Err(RegExpSyntaxError {
                  message: "Invalid regular expression",
                }
                .into());
              }
              // Annex B: in non-UnicodeMode, an invalid `\c` escape in a character class causes
              // the backslash itself to be treated as a literal class atom and leaves the `c` to
              // be parsed as a normal class character.
              //
              // Example: `/[\\c]/` matches both `\\` and `c`.
              self.idx = self.idx.saturating_sub(1);
              return Ok(CharClassItem::Char(b'\\' as u32));
            };

            // In non-UnicodeMode, `\c` control escapes accept both ASCII letters and the Annex B
            // `ClassControlLetter` set (decimal digit or `_`).
            let legacy_class_control =
              !self.flags.has_either_unicode_flag() && (is_decimal_digit(next) || next == (b'_' as u16));

            if is_ascii_letter(next) || legacy_class_control {
              self.next();
              return Ok(CharClassItem::Char(((next as u8) & 0x1F) as u32));
            }

            if self.flags.has_either_unicode_flag() {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }

            // Annex B fallback: treat as a literal backslash and leave `c` for the next atom.
            self.idx = self.idx.saturating_sub(1);
            Ok(CharClassItem::Char(b'\\' as u32))
          }
          x if x == (b'n' as u16) => Ok(CharClassItem::Char(0x000A)),
          x if x == (b'r' as u16) => Ok(CharClassItem::Char(0x000D)),
          x if x == (b't' as u16) => Ok(CharClassItem::Char(0x0009)),
          x if x == (b'v' as u16) => Ok(CharClassItem::Char(0x000B)),
          x if x == (b'f' as u16) => Ok(CharClassItem::Char(0x000C)),
          x if x == (b'0' as u16) => {
            // `\0` in a character class.
            if self.flags.has_either_unicode_flag() {
              if self.peek().is_some_and(is_decimal_digit) {
                return Err(RegExpSyntaxError {
                  message: "Invalid regular expression",
                }
                .into());
              }
              return Ok(CharClassItem::Char(0x0000));
            }

            if self.peek().is_some_and(is_octal_digit) {
              let v = self.parse_legacy_octal_escape_after_first(x)?;
              return Ok(CharClassItem::Char(v));
            }
            Ok(CharClassItem::Char(0x0000))
            }
          x if (b'1' as u16..=b'7' as u16).contains(&x) => {
            // Annex B legacy octal escapes in character classes.
            if self.flags.has_either_unicode_flag() {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            let v = self.parse_legacy_octal_escape_after_first(x)?;
            Ok(CharClassItem::Char(v))
          }
          x if x == (b'8' as u16) || x == (b'9' as u16) => {
            // `\8` / `\9` are identity escapes in non-unicode mode.
            if self.flags.has_either_unicode_flag() {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
                .into());
            }
            Ok(CharClassItem::Char(x as u32))
          }
          x if x == (b'-' as u16) => {
            if self.flags.has_either_unicode_flag() {
              // `ClassEscape[+UnicodeMode]` allows escaping `-` inside a character class.
              Ok(CharClassItem::Char(x as u32))
            } else {
              Ok(CharClassItem::Char(x as u32))
            }
          }
          x if x == (b'x' as u16) => Ok(CharClassItem::Char(self.parse_hex_escape_2(ctx)?)),
          x if x == (b'u' as u16) => Ok(CharClassItem::Char(self.parse_unicode_escape(ctx)?)),
          x if (x == (b'p' as u16) || x == (b'P' as u16))
            && self.flags.has_either_unicode_flag()
            && self.peek() == Some(b'{' as u16) =>
          {
            // Consume `{`.
            self.next();
            let negated = x == (b'P' as u16);
            let prop = self.parse_unicode_property_escape(ctx, negated)?;
            Ok(CharClassItem::UnicodeProperty(prop))
          }
          other => {
            if self.flags.has_either_unicode_flag() {
              if is_syntax_character(other) || other == (b'/' as u16) {
                Ok(CharClassItem::Char(other as u32))
              } else {
                Err(RegExpSyntaxError {
                  message: "Invalid regular expression",
                }
                .into())
              }
            } else {
              Ok(CharClassItem::Char(other as u32))
            }
          }
        }
      }
      other => {
        // In UnicodeMode (`u`/`v`), the RegExp grammar operates over Unicode code points. Literal
        // non-BMP characters therefore appear in the pattern as UTF-16 surrogate pairs and must be
        // decoded into a single `u32` code point.
        //
        // This is especially important for `/v` (UnicodeSets) patterns like `[👨‍👩‍👧‍👦]/v`, which
        // must match the full code point `👨` rather than a single surrogate half.
        if self.flags.has_either_unicode_flag()
          && (0xD800..=0xDBFF).contains(&other)
          && self.peek().is_some_and(|u2| (0xDC00..=0xDFFF).contains(&u2))
        {
          let Some(low) = self.next() else {
            return Ok(CharClassItem::Char(other as u32));
          };
          Ok(CharClassItem::Char(utf16_decode_surrogate_pair(other, low)))
        } else {
          Ok(CharClassItem::Char(other as u32))
        }
      }
    }
  }

  fn parse_escape_atom(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Atom, RegExpCompileError> {
    let Some(e) = self.next() else {
      return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
    };
    match e {
      x if x == (b'd' as u16) => {
        let mut items: Vec<CharClassItem> = Vec::new();
        ctx.vec_try_push(&mut items, CharClassItem::Digit { negated: false })?;
        Ok(Atom::Class(CharClass { negated: false, items }))
      }
      x if x == (b'D' as u16) => {
        let mut items: Vec<CharClassItem> = Vec::new();
        ctx.vec_try_push(&mut items, CharClassItem::Digit { negated: true })?;
        Ok(Atom::Class(CharClass { negated: false, items }))
      }
      x if x == (b'w' as u16) => {
        let mut items: Vec<CharClassItem> = Vec::new();
        ctx.vec_try_push(&mut items, CharClassItem::Word { negated: false })?;
        Ok(Atom::Class(CharClass { negated: false, items }))
      }
      x if x == (b'W' as u16) => {
        let mut items: Vec<CharClassItem> = Vec::new();
        ctx.vec_try_push(&mut items, CharClassItem::Word { negated: true })?;
        Ok(Atom::Class(CharClass { negated: false, items }))
      }
      x if x == (b's' as u16) => {
        let mut items: Vec<CharClassItem> = Vec::new();
        ctx.vec_try_push(&mut items, CharClassItem::Space { negated: false })?;
        Ok(Atom::Class(CharClass { negated: false, items }))
      }
      x if x == (b'S' as u16) => {
        let mut items: Vec<CharClassItem> = Vec::new();
        ctx.vec_try_push(&mut items, CharClassItem::Space { negated: true })?;
        Ok(Atom::Class(CharClass { negated: false, items }))
      }
      x if x == (b'n' as u16) => Ok(Atom::Literal(0x000A)),
      x if x == (b'r' as u16) => Ok(Atom::Literal(0x000D)),
      x if x == (b't' as u16) => Ok(Atom::Literal(0x0009)),
      x if x == (b'v' as u16) => Ok(Atom::Literal(0x000B)),
      x if x == (b'f' as u16) => Ok(Atom::Literal(0x000C)),
      x if x == (b'c' as u16) => {
        let Some(next) = self.peek() else {
          if self.flags.has_either_unicode_flag() {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          // Annex B: when `\c` is not followed by an ASCII letter, treat the `\` as a literal
          // backslash and leave the `c` to be parsed as a normal pattern character.
          //
          // This matches JS behaviour: `new RegExp("\\c")` matches the two-character string `\c`.
          self.idx = self.idx.saturating_sub(1);
          return Ok(Atom::Literal(b'\\' as u32));
        };
        if !is_ascii_letter(next) {
          if self.flags.has_either_unicode_flag() {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          // Same Annex B rule as above: `\c0` / `\c_` etc become a literal backslash followed by
          // `c...`.
          self.idx = self.idx.saturating_sub(1);
          return Ok(Atom::Literal(b'\\' as u32));
        }
        self.next();
        Ok(Atom::Literal(((next as u8) & 0x1F) as u32))
      }
      x if x == (b'0' as u16) => {
        // `\0` has special semantics: it's either a NUL escape or an Annex B legacy octal escape.
        if self.flags.has_either_unicode_flag() {
          // In unicode mode, `\0` may not be followed by a decimal digit.
          if self.peek().is_some_and(is_decimal_digit) {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          return Ok(Atom::Literal(0x0000));
        }

        if self.peek().is_some_and(is_octal_digit) {
          let v = self.parse_legacy_octal_escape_after_first(x)?;
          return Ok(Atom::Literal(v));
        }
        Ok(Atom::Literal(0x0000))
      }
      x if x == (b'k' as u16) => {
        // `\k<name>` is a named backreference.
        //
        // - In UnicodeMode (`/u` or `/v`), `\k` must be followed by `<name>`; otherwise it's a
        //   syntax error.
        // - In non-UnicodeMode, `\k` is an identity escape for `k` *unless* it is immediately
        //   followed by `<name>`, in which case it is parsed as a named backreference and later
        //   resolves to:
        //   - a real backreference when `name` is bound, or
        //   - the literal sequence `k<name>` when `name` is unbound (Annex B).
        if self.eat(b'<' as u16) {
          let name = self.parse_group_name(ctx)?;
          Ok(Atom::NamedBackRef(name))
        } else if self.flags.has_either_unicode_flag() {
          Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into())
        } else {
          Ok(Atom::Literal(x as u32))
        }
      }
      x if x == (b'p' as u16) || x == (b'P' as u16) => {
        if !self.flags.has_either_unicode_flag() {
          // Without Unicode mode, `\p` / `\P` are treated as identity escapes.
          return Ok(Atom::Literal(x as u32));
        }
        // Unicode property escapes require `{...}` in Unicode mode.
        if !self.eat(b'{' as u16) {
          return Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into());
        }
        let negated = x == (b'P' as u16);
        let resolved = self.parse_unicode_property_value_expression(ctx)?;
        match resolved {
          ResolvedUnicodeProperty::CodePoint(prop) => Ok(Atom::UnicodeProperty(UnicodeProperty { prop, negated })),
          ResolvedUnicodeProperty::String(prop) => {
            // String properties cannot be negated.
            if negated {
              return Err(RegExpSyntaxError {
                message: "Invalid regular expression",
              }
              .into());
            }
            Ok(Atom::UnicodeStringProperty(prop))
          }
        }
      }
      x if (b'1' as u16..=b'9' as u16).contains(&x) => {
        // `\1`-`\9` is either a DecimalEscape/backreference or (in non-unicode mode) an Annex B
        // legacy octal escape / identity escape.
        let digit_start = self.idx.saturating_sub(1);

        // Parse as a decimal integer literal first (DecimalEscape).
        let mut n: u32 = (x - (b'0' as u16)) as u32;
        let mut digit_i: usize = 0;
        while let Some(d) = self.peek() {
          if !is_decimal_digit(d) {
            break;
          }
          if digit_i != 0 {
            ctx.tick_every(digit_i)?;
          }
          digit_i = digit_i.wrapping_add(1);
          self.next();
          n = n
            .saturating_mul(10)
            .saturating_add((d - (b'0' as u16)) as u32);
        }

        if self.flags.has_either_unicode_flag() {
          // UnicodeMode: treat as a DecimalEscape/backreference. Bounds are validated after the
          // full parse so forward references (e.g. `/\\1(a)/u`) work correctly.
          ctx.vec_try_push(&mut self.backrefs, n)?;
          return Ok(Atom::BackRef(n));
        }

        if n <= self.total_capture_count {
          return Ok(Atom::BackRef(n));
        }

        // Non-unicode: invalid backreference => legacy octal escape (if possible) or identity
        // escape (`\8`/`\9`). Rewind to immediately after the first digit so the remaining digits
        // can be re-parsed according to the Annex B legacy-octal length rules (or left as literal
        // pattern characters for identity escapes).
        if x == (b'8' as u16) || x == (b'9' as u16) {
          self.idx = digit_start.saturating_add(1);
          return Ok(Atom::Literal(x as u32));
        }
        self.idx = digit_start.saturating_add(1);
        let v = self.parse_legacy_octal_escape_after_first(x)?;
        Ok(Atom::Literal(v))
      }
      x if x == (b'x' as u16) => Ok(Atom::Literal(self.parse_hex_escape_2(ctx)?)),
      x if x == (b'u' as u16) => Ok(Atom::Literal(self.parse_unicode_escape(ctx)?)),
      other => {
        if self.flags.has_either_unicode_flag() {
          if is_syntax_character(other) || other == (b'/' as u16) {
            Ok(Atom::Literal(other as u32))
          } else {
            Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into())
          }
        } else {
          Ok(Atom::Literal(other as u32))
        }
      }
    }
  }

  fn parse_unicode_property_value_expression(
    &mut self,
    ctx: &mut CompileCtx<'_>,
  ) -> Result<ResolvedUnicodeProperty, RegExpCompileError> {
    // `{` has already been consumed.
    let mut bytes: Vec<u8> = Vec::new();
    let mut i: usize = 0;
    loop {
      let Some(u) = self.next() else {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into());
      };
      if u == (b'}' as u16) {
        break;
      }
      if u > 0x7F {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into());
      }
      if i != 0 {
        ctx.tick_every(i)?;
      }
      i = i.wrapping_add(1);
      ctx.vec_try_push(&mut bytes, u as u8)?;
    }

    if bytes.is_empty() {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    }

    let Ok(expr) = core::str::from_utf8(&bytes) else {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    };

    Ok(resolve_unicode_property_value_expression(expr, self.flags.unicode_sets)
      .map_err(RegExpCompileError::Syntax)?)
  }

  fn parse_unicode_property_escape(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    negated: bool,
  ) -> Result<UnicodeProperty, RegExpCompileError> {
    let resolved = self.parse_unicode_property_value_expression(ctx)?;
    match resolved {
      ResolvedUnicodeProperty::CodePoint(prop) => Ok(UnicodeProperty { prop, negated }),
      ResolvedUnicodeProperty::String(_prop) => Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into()),
    }
  }

  fn parse_hex_escape_2(&mut self, _ctx: &mut CompileCtx<'_>) -> Result<u32, RegExpCompileError> {
    // In non-UnicodeMode, `\x` is only a hex escape when followed by two hex digits; otherwise it
    // is an identity escape for `x`.
    if !self.flags.has_either_unicode_flag() {
      let Some(&h1) = self.units.get(self.idx) else {
        return Ok(b'x' as u32);
      };
      let Some(&h2) = self.units.get(self.idx + 1) else {
        return Ok(b'x' as u32);
      };
      let (Some(v1), Some(v2)) = (hex_value(h1), hex_value(h2)) else {
        return Ok(b'x' as u32);
      };
      // Consume digits.
      self.idx = self.idx.saturating_add(2);
      return Ok((v1 << 4) | v2);
    }

    // UnicodeMode: must be exactly `\xHH`.
    let h1 = self.next().ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
      message: "Invalid escape",
    }))?;
    let h2 = self.next().ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
      message: "Invalid escape",
    }))?;
    let v1 = hex_value(h1).ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
      message: "Invalid escape",
    }))?;
    let v2 = hex_value(h2).ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
      message: "Invalid escape",
    }))?;
    Ok((v1 << 4) | v2)
  }

  fn parse_unicode_escape(&mut self, ctx: &mut CompileCtx<'_>) -> Result<u32, RegExpCompileError> {
    if self.flags.has_either_unicode_flag() && self.peek() == Some(b'{' as u16) {
      // `\u{...}` (UnicodeMode only).
      self.next(); // consume '{'
      let mut value: u32 = 0;
      let mut saw_digit = false;
      let mut digit_i: usize = 0;
      loop {
        let Some(u) = self.peek() else {
          // Unterminated `\u{...`.
          return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
        };
        if u == (b'}' as u16) {
          self.next(); // consume '}'
          break;
        }
        if digit_i != 0 {
          ctx.tick_every(digit_i)?;
        }
        digit_i = digit_i.wrapping_add(1);
        let d = hex_value(u).ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
          message: "Invalid escape",
        }))?;
        self.next();
        saw_digit = true;
        value = value.saturating_mul(16).saturating_add(d);
        if value > 0x10FFFF {
          return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
        }
      }
      if !saw_digit {
        return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
      }
      return Ok(value);
    }

    // `\uXXXX` (always).
    if !self.flags.has_either_unicode_flag() {
      // Non-UnicodeMode: only treat as a Unicode escape sequence when followed by 4 hex digits;
      // otherwise it is an identity escape for `u` (and leaves the input untouched so `{...}` can
      // form a quantifier).
      if self.idx + 4 <= self.units.len()
        && self.units[self.idx..self.idx + 4]
          .iter()
          .all(|&u| hex_value(u).is_some())
      {
        let save = self.idx;
        let mut value: u32 = 0;
        for _ in 0..4 {
          let Some(u) = self.next() else {
            self.idx = save;
            return Ok(b'u' as u32);
          };
          let Some(d) = hex_value(u) else {
            self.idx = save;
            return Ok(b'u' as u32);
          };
          value = (value << 4) | d;
        }
        return Ok(value);
      }
      return Ok(b'u' as u32);
    }

    // UnicodeMode: must be exactly `\uXXXX`.
    let mut value: u32 = 0;
    for _ in 0..4 {
      let u = self.next().ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
        message: "Invalid escape",
      }))?;
      let d = hex_value(u).ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
        message: "Invalid escape",
      }))?;
      value = (value << 4) | d;
    }

    // Surrogate-pair merge: `\uD83D\uDC38` => U+1F438 (UnicodeMode only), but only for the
    // non-braced `\uXXXX` form. This matches ECMAScript ParsePattern semantics.
    if (0xD800..=0xDBFF).contains(&(value as u16)) {
      let save = self.idx;
      if self.units.get(save) == Some(&(b'\\' as u16))
        && self.units.get(save + 1) == Some(&(b'u' as u16))
      {
        // Do not merge braced escapes (`\u{...}`).
        if self.units.get(save + 2) != Some(&(b'{' as u16)) && save + 6 <= self.units.len() {
          let digits = &self.units[save + 2..save + 6];
          if digits.iter().all(|&u| hex_value(u).is_some()) {
            let mut trail: u32 = 0;
            for &u in digits {
              let Some(d) = hex_value(u) else {
                trail = 0;
                break;
              };
              trail = (trail << 4) | d;
            }
            if (0xDC00..=0xDFFF).contains(&(trail as u16)) {
              // Consume the second escape (`\u` + 4 hex digits).
              self.idx = save + 6;
              let lead = value - 0xD800;
              let trail = trail - 0xDC00;
              return Ok(0x10000 + (lead << 10) + trail);
            }
          }
        }
      }
    }

    Ok(value)
  }

  fn parse_quantifier_if_present(
    &mut self,
    ctx: &mut CompileCtx<'_>,
  ) -> Result<Option<Quantifier>, RegExpCompileError> {
    let Some(u) = self.peek() else {
      return Ok(None);
    };
    let unicode_mode = self.flags.has_either_unicode_flag();
    let (mut min, max): (u32, Option<u32>) = match u {
      x if x == (b'*' as u16) => {
        self.next();
        (0, None)
      }
      x if x == (b'+' as u16) => {
        self.next();
        (1, None)
      }
      x if x == (b'?' as u16) => {
        self.next();
        (0, Some(1))
      }
      x if x == (b'{' as u16) => {
        let save = self.idx;
        self.next();
        let Some(first) = self.peek() else {
          if unicode_mode {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          self.idx = save;
          return Ok(None);
        };
        if !(b'0' as u16..=b'9' as u16).contains(&first) {
          // Not a quantifier; treat `{` as a literal.
          if unicode_mode {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          self.idx = save;
          return Ok(None);
        }
        let m = self.parse_decimal_u32(ctx)?;
        let mut n: Option<u32> = None;
        if self.eat(b',' as u16) {
          if let Some(d) = self.peek() {
            if (b'0' as u16..=b'9' as u16).contains(&d) {
              n = Some(self.parse_decimal_u32(ctx)?);
            } else {
              n = None;
            }
          }
        } else {
          n = Some(m);
        }
        if !self.eat(b'}' as u16) {
          if unicode_mode {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }
          self.idx = save;
          return Ok(None);
        }
        (m, n)
      }
      x if x == (b'}' as u16) && unicode_mode => {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into())
      }
      _ => return Ok(None),
    };

    if let Some(max) = max {
      if max < min {
        return Err(RegExpSyntaxError {
          message: "Invalid regular expression",
        }
        .into());
      }
    }

    // Lazy quantifier suffix `?`.
    let mut greedy = true;
    if self.peek() == Some(b'?' as u16) {
      self.next();
      greedy = false;
    }

    // Special-case: `{0,}` should be treated as `*`.
    if max.is_none() && min == 0 {
      min = 0;
    }

    Ok(Some(Quantifier { min, max, greedy }))
  }

  fn parse_decimal_u32(&mut self, ctx: &mut CompileCtx<'_>) -> Result<u32, RegExpCompileError> {
    let mut n: u32 = 0;
    let mut digit_i: usize = 0;
    while let Some(u) = self.peek() {
      if !(b'0' as u16..=b'9' as u16).contains(&u) {
        break;
      }
      if digit_i != 0 {
        ctx.tick_every(digit_i)?;
      }
      digit_i = digit_i.wrapping_add(1);
      self.next();
      n = n.saturating_mul(10).saturating_add((u - (b'0' as u16)) as u32);
    }
    Ok(n)
  }
}

fn hex_value(u: u16) -> Option<u32> {
  match u {
    x if (b'0' as u16..=b'9' as u16).contains(&x) => Some((x - (b'0' as u16)) as u32),
    x if (b'a' as u16..=b'f' as u16).contains(&x) => Some((x - (b'a' as u16) + 10) as u32),
    x if (b'A' as u16..=b'F' as u16).contains(&x) => Some((x - (b'A' as u16) + 10) as u32),
    _ => None,
  }
}

struct ProgramBuilder {
  insts: Vec<Inst>,
  repeat_count: usize,
  capture_count: usize,
  named_capture_groups: Vec<NamedCaptureGroup>,
  unicode_mode: bool,
}

impl ProgramBuilder {
  fn new(
    capture_count: usize,
    named_capture_groups: Vec<NamedCaptureGroup>,
    unicode_mode: bool,
  ) -> Self {
    Self {
      insts: Vec::new(),
      repeat_count: 0,
      capture_count,
      named_capture_groups,
      unicode_mode,
    }
  }

  fn finish(self) -> RegExpProgram {
    RegExpProgram {
      insts: self.insts,
      capture_count: self.capture_count,
      repeat_count: self.repeat_count,
      named_capture_groups: self.named_capture_groups,
    }
  }

  fn try_clone_named_capture_groups(
    ctx: &mut CompileCtx<'_>,
    groups: &[NamedCaptureGroup],
  ) -> Result<Vec<NamedCaptureGroup>, RegExpCompileError> {
    let mut out: Vec<NamedCaptureGroup> = Vec::new();
    ctx.reserve_vec_to_len(&mut out, groups.len())?;
    for (i, group) in groups.iter().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }

      let mut name: Vec<u16> = Vec::new();
      ctx.reserve_vec_to_len(&mut name, group.name.len())?;
      name.extend_from_slice(&group.name);

      let mut capture_indices: Vec<u32> = Vec::new();
      ctx.reserve_vec_to_len(&mut capture_indices, group.capture_indices.len())?;
      capture_indices.extend_from_slice(&group.capture_indices);

      ctx.vec_try_push(
        &mut out,
        NamedCaptureGroup {
          name,
          capture_indices,
        },
      )?;
    }
    Ok(out)
  }

  fn emit(&mut self, ctx: &mut CompileCtx<'_>, inst: Inst) -> Result<usize, RegExpCompileError> {
    let pc = self.insts.len();
    if pc != 0 {
      ctx.tick_every(pc)?;
    }
    let required_len = pc
      .checked_add(1)
      .ok_or(RegExpCompileError::OutOfMemory)?;
    ctx.reserve_vec_to_len(&mut self.insts, required_len)?;
    self.insts.push(inst);
    Ok(pc)
  }

  fn compile_disjunction(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    disj: Disjunction,
  ) -> Result<(), RegExpCompileError> {
    self.compile_disjunction_dir(ctx, disj, MatchDir::Forward)
  }

  fn compile_disjunction_dir(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    disj: Disjunction,
    dir: MatchDir,
  ) -> Result<(), RegExpCompileError> {
    if disj.alts.is_empty() {
      return Ok(());
    }
    let mut alts = disj.alts;
    if alts.len() == 1 {
      let Some(alt) = alts.pop() else {
        return Ok(());
      };
      return self.compile_alternative_dir(ctx, alt, dir);
    }

    let last_idx = alts.len().saturating_sub(1);
    let mut end_jumps: Vec<usize> = Vec::new();
    for (i, alt) in alts.into_iter().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }
      if i == last_idx {
        self.compile_alternative_dir(ctx, alt, dir)?;
        break;
      }
      // Split to this alternative (fallthrough) or the next one (patched).
      let fallthrough = self
        .insts
        .len()
        .checked_add(1)
        .ok_or(RegExpCompileError::OutOfMemory)?;
      let split_pc = self.emit(ctx, Inst::Split(fallthrough, 0))?;
      self.compile_alternative_dir(ctx, alt, dir)?;
      let jmp_pc = self.emit(ctx, Inst::Jump(0))?;
      ctx.vec_try_push(&mut end_jumps, jmp_pc)?;
      // Patch the split's second branch to the start of the next alternative.
      let next_pc = self.insts.len();
      let Inst::Split(_, ref mut b) = self.insts[split_pc] else {
        return Err(VmError::InvariantViolation("expected Split instruction for alternation").into());
      };
      *b = next_pc;
    }

    let end = self.insts.len();
    for pc in end_jumps {
      let Inst::Jump(ref mut target) = self.insts[pc] else {
        return Err(VmError::InvariantViolation("expected Jump instruction for alternation").into());
      };
      *target = end;
    }
    Ok(())
  }

  fn compile_alternative_dir(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    alt: Alternative,
    dir: MatchDir,
  ) -> Result<(), RegExpCompileError> {
    if dir.is_forward() {
      for (i, term) in alt.terms.into_iter().enumerate() {
        if i != 0 {
          ctx.tick_every(i)?;
        }
        self.compile_term_dir(ctx, term, dir)?;
      }
    } else {
      for (i, term) in alt.terms.into_iter().rev().enumerate() {
        if i != 0 {
          ctx.tick_every(i)?;
        }
        self.compile_term_dir(ctx, term, dir)?;
      }
    }
    Ok(())
  }

  fn compile_term_dir(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    term: Term,
    dir: MatchDir,
  ) -> Result<(), RegExpCompileError> {
    match term {
      Term::Assertion(a) => self.compile_assertion(ctx, a),
      Term::Atom(atom, quant) => match quant {
        Some(q) => self.compile_quantified_dir(ctx, atom, q, dir),
        None => self.compile_atom_dir(ctx, atom, dir),
      },
    }
  }

  fn compile_assertion(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    a: Assertion,
  ) -> Result<(), RegExpCompileError> {
    match a {
      Assertion::Start => {
        self.emit(ctx, Inst::AssertStart)?;
      }
      Assertion::End => {
        self.emit(ctx, Inst::AssertEnd)?;
      }
      Assertion::WordBoundary => {
        self.emit(ctx, Inst::WordBoundary { negated: false })?;
      }
      Assertion::NotWordBoundary => {
        self.emit(ctx, Inst::WordBoundary { negated: true })?;
      }
      Assertion::LookAhead { negative, disj } => {
        // Compile lookahead into a nested program that shares the outer capture slot numbering.
        let cloned_named =
          ProgramBuilder::try_clone_named_capture_groups(ctx, &self.named_capture_groups)?;
        let mut nested = ProgramBuilder::new(self.capture_count, cloned_named, self.unicode_mode);
        nested.compile_disjunction(ctx, disj)?;
        nested.emit(ctx, Inst::Match)?;
        let nested_prog = nested.finish();
        let boxed = ctx.box_try_new(nested_prog)?;
        self.emit(
          ctx,
          Inst::LookAhead {
            program: boxed,
            negative,
          },
        )?;
      }
      Assertion::LookBehind { negative, disj } => {
        // Compile lookbehind into a nested program that shares the outer capture slot numbering.
        // The nested program is compiled and executed with -1 direction semantics.
        let cloned_named =
          ProgramBuilder::try_clone_named_capture_groups(ctx, &self.named_capture_groups)?;
        let mut nested = ProgramBuilder::new(self.capture_count, cloned_named, self.unicode_mode);
        nested.compile_disjunction_dir(ctx, disj, MatchDir::Backward)?;
        nested.emit(ctx, Inst::Match)?;
        let nested_prog = nested.finish();
        let boxed = ctx.box_try_new(nested_prog)?;
        self.emit(
          ctx,
          Inst::LookBehind {
            program: boxed,
            negative,
          },
        )?;
      }
    }
    Ok(())
  }

  fn compile_quantified_dir(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    atom: Atom,
    q: Quantifier,
    dir: MatchDir,
  ) -> Result<(), RegExpCompileError> {
    let (clear_from_slot, clear_to_slot) = match &atom {
      Atom::Group {
        capture_range_start,
        capture_range_end,
        ..
      } if *capture_range_start != 0 && *capture_range_end >= *capture_range_start => {
        let start = *capture_range_start as usize;
        let end = *capture_range_end as usize;
        let clear_from_slot = start
          .checked_mul(2)
          .ok_or(RegExpCompileError::OutOfMemory)?;
        let clear_to_slot = end
          .checked_add(1)
          .ok_or(RegExpCompileError::OutOfMemory)?
          .checked_mul(2)
          .ok_or(RegExpCompileError::OutOfMemory)?;
        (clear_from_slot, clear_to_slot)
      }
      Atom::LookAhead {
        capture_range_start,
        capture_range_end,
        ..
      } if *capture_range_start != 0 && *capture_range_end >= *capture_range_start => {
        let start = *capture_range_start as usize;
        let end = *capture_range_end as usize;
        let clear_from_slot = start
          .checked_mul(2)
          .ok_or(RegExpCompileError::OutOfMemory)?;
        let clear_to_slot = end
          .checked_add(1)
          .ok_or(RegExpCompileError::OutOfMemory)?
          .checked_mul(2)
          .ok_or(RegExpCompileError::OutOfMemory)?;
        (clear_from_slot, clear_to_slot)
      }
      _ => (0, 0),
    };

    let id = self.repeat_count;
    self.repeat_count = self.repeat_count.saturating_add(1);

    // Quantifier runtime counters live in `ExecState.repeats` and must be reset each time this
    // quantifier is re-entered (e.g. when an enclosing quantified group repeats its body). The loop
    // backedge (`RepeatEnd`) jumps directly to `RepeatStart` and intentionally bypasses this reset.
    self.emit(ctx, Inst::RepeatReset { id })?;

    let start_pc = self.emit(ctx, Inst::RepeatStart {
      id,
      min: q.min,
      max: q.max,
      greedy: q.greedy,
      exit: 0, // patch
      clear_from_slot,
      clear_to_slot,
    })?;
    self.compile_atom_dir(ctx, atom, dir)?;
    self.emit(ctx, Inst::RepeatEnd { start: start_pc })?;
    let exit = self.insts.len();
    let Inst::RepeatStart { exit: ref mut e, .. } = self.insts[start_pc] else {
      return Err(VmError::InvariantViolation("expected RepeatStart instruction for quantifier").into());
    };
    *e = exit;
    Ok(())
  }

  fn compile_atom_dir(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    atom: Atom,
    dir: MatchDir,
  ) -> Result<(), RegExpCompileError> {
    match atom {
      Atom::Literal(u) => {
        self.emit(ctx, Inst::Char(u))?;
      }
      Atom::Any => {
        self.emit(ctx, Inst::Any)?;
      }
      Atom::Class(cls) => {
        self.emit(ctx, Inst::Class(cls))?;
      }
      Atom::UnicodeSet(cls) => {
        self.emit(ctx, Inst::UnicodeSet(cls))?;
      }
      Atom::UnicodeProperty(prop) => {
        self.emit(ctx, Inst::UnicodeProperty(prop))?;
      }
      Atom::UnicodeStringProperty(prop) => {
        self.emit(ctx, Inst::UnicodeStringProperty(prop))?;
      }
      Atom::LookAhead { negative, disj, .. } => {
        // Same instruction as a normal lookahead assertion; this atom form exists so Annex B
        // quantifiers can apply to lookahead in non-UnicodeMode.
        self.compile_assertion(ctx, Assertion::LookAhead { negative, disj })?;
      }
      Atom::BackRef(n) => {
        self.emit(ctx, Inst::BackRef(n))?;
      }
      Atom::NamedBackRef(name) => {
        let mut found: Option<u32> = None;
        for (i, group) in self.named_capture_groups.iter().enumerate() {
          if i != 0 {
            ctx.tick_every(i)?;
          }
          if group.name == name {
            found = Some(u32::try_from(i).map_err(|_| RegExpCompileError::OutOfMemory)?);
            break;
          }
        }
        let Some(name_id) = found else {
          if self.unicode_mode {
            return Err(RegExpSyntaxError {
              message: "Invalid regular expression",
            }
            .into());
          }

          // Annex B: in non-UnicodeMode, a named backreference to an unresolved group name falls
          // back to `IdentityEscape` (`\k`) and literal `<name>`.
          //
          // The parser has already consumed the full `\k<name>` sequence as a `NamedBackRef` atom,
          // so emit the literal equivalent `k<name>` here.
          if dir.is_forward() {
            self.emit(ctx, Inst::Char(b'k' as u32))?;
            self.emit(ctx, Inst::Char(b'<' as u32))?;
            for (i, &u) in name.iter().enumerate() {
              if i != 0 {
                ctx.tick_every(i)?;
              }
              self.emit(ctx, Inst::Char(u as u32))?;
            }
            self.emit(ctx, Inst::Char(b'>' as u32))?;
          } else {
            self.emit(ctx, Inst::Char(b'>' as u32))?;
            for (i, &u) in name.iter().rev().enumerate() {
              if i != 0 {
                ctx.tick_every(i)?;
              }
              self.emit(ctx, Inst::Char(u as u32))?;
            }
            self.emit(ctx, Inst::Char(b'<' as u32))?;
            self.emit(ctx, Inst::Char(b'k' as u32))?;
          }
          return Ok(());
        };
        self.emit(ctx, Inst::NamedBackRef(name_id))?;
      }
      Atom::Group { capture, disj, .. } => {
        if let Some(idx) = capture {
          let start_slot = (idx as usize).saturating_mul(2);
          self.emit(ctx, Inst::Save(start_slot))?;
          self.compile_disjunction_dir(ctx, disj, dir)?;
          self.emit(ctx, Inst::Save(start_slot.saturating_add(1)))?;
        } else {
          self.compile_disjunction_dir(ctx, disj, dir)?;
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod regexp_unicode_sets_tests {
  use super::*;
  use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

  fn eval_bool(rt: &mut JsRuntime, script: &str) -> Result<bool, VmError> {
    match rt.exec_script(script)? {
      Value::Bool(b) => Ok(b),
      _other => Err(VmError::InvariantViolation("expected boolean result from test script")),
    }
  }

  #[test]
  fn regexp_space_escape_matches_ecma_whitespace_and_line_terminators() -> Result<(), VmError> {
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let exec_mem = RegExpExecMemoryBudget::new(1024 * 1024);
    fn matches_units(
      prog: &RegExpProgram,
      flags: RegExpFlags,
      exec_mem: &RegExpExecMemoryBudget,
      input: &[u16],
    ) -> Result<bool, VmError> {
      let mut tick = || Ok(());
      Ok(
        prog
          .exec_at(input, 0, flags, &mut tick, exec_mem, None)?
          .is_some(),
      )
    }

    let none = RegExpFlags::default();
    let v = RegExpFlags {
      unicode_sets: true,
      ..RegExpFlags::default()
    };

    // `\s` / `\S` as plain character class escapes.
    let re_s = compile_regexp(&[b'\\' as u16, b's' as u16], none, &heap).expect("compile \\s");
    let re_not_s =
      compile_regexp(&[b'\\' as u16, b'S' as u16], none, &heap).expect("compile \\S");
    // `\s` / `\S` inside `/v` UnicodeSets mode character classes.
    let re_v_s =
      compile_regexp(&[b'[' as u16, b'\\' as u16, b's' as u16, b']' as u16], v, &heap)
        .expect("compile [\\s] with /v");
    let re_v_not_s =
      compile_regexp(&[b'[' as u16, b'\\' as u16, b'S' as u16, b']' as u16], v, &heap)
        .expect("compile [\\S] with /v");

    // WhiteSpace ∪ LineTerminator (ECMA-262):
    for u in [
      // WhiteSpace
      0x0009u16, // Tab
      0x000Bu16, // VT
      0x000Cu16, // FF
      0x0020u16, // Space
      0x00A0u16, // NBSP
      0x1680u16, // Ogham space mark
      0x202Fu16, // Narrow NBSP
      0x205Fu16, // Medium mathematical space
      0x3000u16, // Ideographic space
      0xFEFFu16, // BOM
      // LineTerminator
      0x000Au16, // LF
      0x000Du16, // CR
      0x2028u16, // Line separator
      0x2029u16, // Paragraph separator
    ] {
      assert!(matches_units(&re_s, none, &exec_mem, &[u])?);
      assert!(!matches_units(&re_not_s, none, &exec_mem, &[u])?);
      assert!(matches_units(&re_v_s, v, &exec_mem, &[u])?);
      assert!(!matches_units(&re_v_not_s, v, &exec_mem, &[u])?);
    }
    for u in 0x2000u16..=0x200Au16 {
      assert!(matches_units(&re_s, none, &exec_mem, &[u])?);
      assert!(!matches_units(&re_not_s, none, &exec_mem, &[u])?);
      assert!(matches_units(&re_v_s, v, &exec_mem, &[u])?);
      assert!(!matches_units(&re_v_not_s, v, &exec_mem, &[u])?);
    }

    // Negation is the complement of the `\s` set.
    assert!(!matches_units(&re_s, none, &exec_mem, &[b'a' as u16])?);
    assert!(matches_units(&re_not_s, none, &exec_mem, &[b'a' as u16])?);
    assert!(!matches_units(&re_v_s, v, &exec_mem, &[b'a' as u16])?);
    assert!(matches_units(&re_v_not_s, v, &exec_mem, &[b'a' as u16])?);

    // A common "Unicode whitespace" code point that is *not* in the ECMAScript `\s` set.
    assert!(!matches_units(&re_s, none, &exec_mem, &[0x200B])?);
    assert!(matches_units(&re_not_s, none, &exec_mem, &[0x200B])?);
    assert!(!matches_units(&re_v_s, v, &exec_mem, &[0x200B])?);
    assert!(matches_units(&re_v_not_s, v, &exec_mem, &[0x200B])?);
    // Mongolian vowel separator: historically treated as whitespace in some contexts, but **not**
    // in the ECMAScript `\s` set.
    assert!(!matches_units(&re_s, none, &exec_mem, &[0x180E])?);
    assert!(matches_units(&re_not_s, none, &exec_mem, &[0x180E])?);
    assert!(!matches_units(&re_v_s, v, &exec_mem, &[0x180E])?);
    assert!(matches_units(&re_v_not_s, v, &exec_mem, &[0x180E])?);

    // Non-BMP code points must also follow the same `\s`/`\S` semantics. (ECMAScript `\s` contains
    // no non-BMP characters.)
    let grinning_face: [u16; 2] = [0xD83D, 0xDE00]; // U+1F600
    assert!(!matches_units(&re_s, none, &exec_mem, &grinning_face)?);
    assert!(matches_units(&re_not_s, none, &exec_mem, &grinning_face)?);
    assert!(!matches_units(&re_v_s, v, &exec_mem, &grinning_face)?);
    assert!(matches_units(&re_v_not_s, v, &exec_mem, &grinning_face)?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_restricted_patterns_throw() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // Unescaped `]` is only tolerated in non-Unicode mode (Annex B). In UnicodeMode (`/u` or `/v`)
    // it is a SyntaxError both via the constructor and via RegExp literals.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("]", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { eval("/]/u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { eval("/]/v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    // In UnicodeMode, `{` must only appear as part of a valid quantifier after an atom.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("{", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{1", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{1,", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{1,2", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("{", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{1", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{1,", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a{1,2", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    // Legacy (non-unicode) mode should continue treating these as literal PatternCharacters.
    assert!(eval_bool(&mut rt, r#"(new RegExp("]").test("]"))"#)?);
    assert!(eval_bool(&mut rt, r#"(new RegExp("{").test("{"))"#)?);
    assert!(eval_bool(&mut rt, r#"(new RegExp("a{").test("a{"))"#)?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_property_of_strings_match_and_backtrack() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // Basic matches for properties of strings.
    assert!(eval_bool(
      &mut rt,
      r#"( /^\p{Emoji_Keycap_Sequence}+$/v.test("0\uFE0F\u20E3") )"#,
    )?);
    assert!(!eval_bool(
      &mut rt,
      r#"( /^\p{Emoji_Keycap_Sequence}+$/v.test("0") )"#,
    )?);

    // Properties of strings inside `/v` character classes.
    assert!(eval_bool(
      &mut rt,
      r#"( /^[\p{Emoji_Keycap_Sequence}_]+$/v.test("_") )"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"( /^[\p{Emoji_Keycap_Sequence}_]+$/v.test("9\uFE0F\u20E3") )"#,
    )?);
    assert!(!eval_bool(
      &mut rt,
      r#"( /^[\p{Emoji_Keycap_Sequence}_]+$/v.test("9") )"#,
    )?);

    // Backtracking between multiple prefix matches of a string property:
    // "🏳️" is a Basic_Emoji and "🏳️‍🌈" is an RGI_Emoji_ZWJ_Sequence. `\p{RGI_Emoji}` must
    // surface both lengths so the VM can backtrack from the longest to the shorter match.
    assert!(eval_bool(
      &mut rt,
      r#"( /^\p{RGI_Emoji}\u200D\u{1F308}$/v.test("\u{1F3F3}\u{FE0F}\u200D\u{1F308}") )"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_flags_v_is_accepted_and_mutually_exclusive_with_u() {
    let mut tick = || Ok(());
    let v = RegExpFlags::parse(&[b'v' as u16], &mut tick).expect("v should parse");
    assert!(v.unicode_sets);
    assert!(!v.unicode);
    assert_eq!(v.to_canonical_string(), "v");
    assert!(v.has_either_unicode_flag());

    let mut tick = || Ok(());
    let err = RegExpFlags::parse(&[b'u' as u16, b'v' as u16], &mut tick).unwrap_err();
    match err {
      RegExpCompileError::Syntax(e) => {
        assert_eq!(e.message, "Invalid flags supplied to RegExp constructor")
      }
      other => panic!("expected syntax error, got {other:?}"),
    }

    let mut tick = || Ok(());
    let err = RegExpFlags::parse(&[b'v' as u16, b'u' as u16], &mut tick).unwrap_err();
    match err {
      RegExpCompileError::Syntax(e) => {
        assert_eq!(e.message, "Invalid flags supplied to RegExp constructor")
      }
      other => panic!("expected syntax error, got {other:?}"),
    }

    // Duplicate `v` should also be rejected.
    let mut tick = || Ok(());
    let err = RegExpFlags::parse(&[b'v' as u16, b'v' as u16], &mut tick).unwrap_err();
    match err {
      RegExpCompileError::Syntax(e) => {
        assert_eq!(e.message, "Invalid flags supplied to RegExp constructor")
      }
      other => panic!("expected syntax error, got {other:?}"),
    }
  }

  #[test]
  fn regexp_flags_d_is_accepted_and_reports_in_canonical_order() {
    let mut tick = || Ok(());
    let d = RegExpFlags::parse(&[b'd' as u16], &mut tick).expect("d should parse");
    assert!(d.has_indices);
    assert_eq!(d.to_canonical_string(), "d");

    let mut tick = || Ok(());
    let vd = RegExpFlags::parse(&[b'v' as u16, b'd' as u16], &mut tick).expect("vd should parse");
    assert!(vd.has_indices);
    assert!(vd.unicode_sets);
    assert_eq!(vd.to_canonical_string(), "dv");

    // Duplicates are rejected.
    let mut tick = || Ok(());
    let err = RegExpFlags::parse(&[b'd' as u16, b'd' as u16], &mut tick).unwrap_err();
    match err {
      RegExpCompileError::Syntax(e) => {
        assert_eq!(e.message, "Invalid flags supplied to RegExp constructor")
      }
      other => panic!("expected syntax error, got {other:?}"),
    }
  }

  #[test]
  fn regexp_v_flag_is_accepted_by_regexp_literals_and_constructor() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // RegExp literal with `/v`.
    assert!(eval_bool(&mut rt, r#"(/a/v.test("a"))"#)?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = /a/v; return r.unicode === false && r.unicodeSets === true && r.flags === "v"; })()"#,
    )?);
    // Canonical `flags` ordering should include `v`.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = /a/vig; return r.flags === "giv"; })()"#,
    )?);

    // RegExp constructor with `v`.
    assert!(eval_bool(&mut rt, r#"(new RegExp("a", "v").test("a"))"#)?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = new RegExp("a", "v"); return r.unicode === false && r.unicodeSets === true && r.flags === "v"; })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = new RegExp("a", "vig"); return r.flags === "giv"; })()"#,
    )?);

    // `u` and `v` are mutually exclusive.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a", "uv"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid flags supplied to RegExp constructor"; } })()"#,
    )?);
    // Duplicate `v` is invalid.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("a", "vv"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid flags supplied to RegExp constructor"; } })()"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_d_flag_is_accepted_by_regexp_literals_and_constructor() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // RegExp literal with `/d`.
    assert!(eval_bool(&mut rt, r#"(/a/d.test("a"))"#)?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = /a/d; return r.hasIndices === true && r.flags === "d"; })()"#,
    )?);

    // RegExp constructor with `d`.
    assert!(eval_bool(&mut rt, r#"(new RegExp("a", "d").test("a"))"#)?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = new RegExp("a", "d"); return r.hasIndices === true && r.flags === "d"; })()"#,
    )?);

    // `d` composes with `/v` and canonicalizes to `"dv"`.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = /a/vd; return r.unicode === false && r.unicodeSets === true && r.hasIndices === true && r.flags === "dv"; })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const r = new RegExp("a", "vd"); return r.unicode === false && r.unicodeSets === true && r.hasIndices === true && r.flags === "dv"; })()"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_mode_rejects_invalid_control_escapes() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // `\c` is a legacy control escape. In UnicodeMode (either `u` or `v`), it must be followed by
    // an ASCII letter, otherwise it is a syntax error (unicode-restricted identity escape).
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\c", "u"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\c", "v"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\c1", "u"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\c1", "v"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);

    // Same restriction inside character classes.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[\\c]", "u"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[\\c]", "v"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[\\c1]", "u"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[\\c1]", "v"); return false; } catch (e) { return e instanceof SyntaxError && e.message === "Invalid regular expression"; } })()"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_escape_sequence_parsing_matches_ecma262() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // In non-unicode mode, `\u{...}` is parsed as an identity escape (`u`) followed by `{...}`,
    // which can be interpreted as a quantifier.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const m = /\u{41}/.exec("ABC" + "u".repeat(41)); return m !== null && m[0] === "u".repeat(41); })()"#,
    )?);

    // If the `{...}` does not form a valid quantifier, it is treated as literals in non-unicode
    // mode (Annex B behavior).
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const m = /\u{4A}/.exec("JKLu{4A}"); return m !== null && m[0] === "u{4A}"; })()"#,
    )?);

    // In unicode mode (`u` or `v`), braced escapes are parsed as full Unicode code points.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const s = "\uD83D\uDC38"; const m = /\u{1F438}/u.exec(s); return m !== null && m[0] === s; })()"#,
    )?);

    // In unicode mode, surrogate pair escapes are merged into a single code point.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { const s = "\uD83D\uDC38"; const m = /\uD83D\uDC38/u.exec(s); return m !== null && m[0] === s; })()"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_property_escapes_parse_and_match() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // Binary property.
    assert!(eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{ASCII}", "u").test("A"))"#,
    )?);
    assert!(!eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{ASCII}", "u").test("é"))"#,
    )?);
    assert!(!eval_bool(
      &mut rt,
      r#"(new RegExp("\\P{ASCII}", "u").test("A"))"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(new RegExp("\\P{ASCII}", "u").test("é"))"#,
    )?);

    // General_Category lone-value precedence (`Lu` => `General_Category=Uppercase_Letter`).
    assert!(eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{Lu}", "u").test("A"))"#,
    )?);
    assert!(!eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{Lu}", "u").test("a"))"#,
    )?);
    // ignoreCase should apply (case folding), so `Lu` matches "a" under `/iu`.
    assert!(eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{Lu}", "iu").test("a"))"#,
    )?);

    // Non-binary `Script=...`.
    assert!(eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{Script=Greek}", "u").test("Ω"))"#,
    )?);
    assert!(!eval_bool(
      &mut rt,
      r#"(new RegExp("\\p{Script=Greek}", "u").test("A"))"#,
    )?);

    // Invalid / unsupported names.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\p{ascii}", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\p{Block=Basic_Latin}", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    // `\p`/`\P` require `{...}` in UnicodeMode.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("\\p", "u"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_sets_mode_class_string_disjunction_matches_longest_first() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let ok = eval_bool(
      &mut rt,
      r#"
      (function () {
        const re = new RegExp("[\\q{ab|a}]", "v");
        const m = re.exec("ab");
        return m !== null && m[0] === "ab" && re.test("a");
      })()
    "#,
    )?;
    assert!(ok);
    Ok(())
  }

  #[test]
  fn regexp_unicode_sets_mode_class_set_intersection_can_be_empty() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let ok = eval_bool(
      &mut rt,
      r#"
      (function () {
        const re = new RegExp("[_&&\\q{0|9\\uFE0F\\u20E3}]", "v");
        return !re.test("_") && !re.test("0") && !re.test("9\uFE0F\u20E3");
      })()
    "#,
    )?;
    assert!(ok);
    Ok(())
  }

  #[test]
  fn regexp_unicode_sets_mode_class_set_subtraction_removes_string_alternative() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let ok = eval_bool(
      &mut rt,
      r#"
      (function () {
        const re = new RegExp("[\\q{ab|a}--\\q{ab}]", "v");
        const m = re.exec("ab");
        return m !== null && m[0] === "a";
      })()
    "#,
    )?;
    assert!(ok);
    Ok(())
  }

  #[test]
  fn regexp_unicode_sets_mode_negated_class_with_strings_is_syntax_error() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let ok = eval_bool(
      &mut rt,
      r#"
      (function () {
        try {
          new RegExp("[^\\q{ab}]", "v");
          return false;
        } catch (e) {
          return e instanceof SyntaxError;
        }
      })()
    "#,
    )?;
    assert!(ok);
    Ok(())
  }

  #[test]
  fn regexp_unicode_sets_mode_accepts_class_set_expression_shape_patterns() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // Union by adjacency with a nested class: `[[0-9]_]`.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { return new RegExp("^[[0-9]_]+$", "v").test("0_"); } catch (e) { return false; } })()"#,
    )?);

    // Intersection: `[[0-9]&&[0-9]]`.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { return new RegExp("^[[0-9]&&[0-9]]+$", "v").test("012"); } catch (e) { return false; } })()"#,
    )?);

    // Subtraction: `[[0-9]--_]`.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { return new RegExp("^[[0-9]--_]+$", "v").test("012"); } catch (e) { return false; } })()"#,
    )?);

    // ClassStringDisjunction: `\q{...}`.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("^[\\q{0|2|4|9\\uFE0F\\u20E3}_]+$", "v"); return true; } catch (e) { return false; } })()"#,
    )?);

    Ok(())
  }

  #[test]
  fn regexp_unicode_sets_mode_class_set_expression_early_errors() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    // Unescaped ClassSetSyntaxCharacter.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[(]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[)]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    // Reserved double punctuators.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[**]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[@@]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[^^^]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    // Range order: start must not be greater than end.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[b-a]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    // Negated nested classes may not contain strings.
    assert!(eval_bool(
      &mut rt,
      r#"(function () { try { new RegExp("[[^\\q{ab}]]", "v"); return false; } catch (e) { return e instanceof SyntaxError; } })()"#,
    )?);

    Ok(())
  }
}

#[cfg(test)]
mod unicode_set_tests {
  use super::*;

  fn test_ctx() -> (Heap, CompileCtx<'static>) {
    let heap = Heap::new(HeapLimits::new(1024 * 1024 * 1024, 1024 * 1024 * 1024));
    // Leak the closure so `CompileCtx` can borrow it with a `'static` lifetime in tests.
    //
    // Tests are short-lived and this keeps the helper ergonomic.
    let tick: &'static mut dyn FnMut() -> Result<(), VmError> = {
      // Avoid `Box::new`, which aborts the process on allocator OOM.
      let boxed: Box<dyn FnMut() -> Result<(), VmError>> =
        crate::fallible_alloc::box_try_new_vm(|| Ok(())).expect("alloc tick");
      Box::leak(boxed)
    };
    let ctx = CompileCtx::new(&heap, tick);
    (heap, ctx)
  }

  #[test]
  fn unicode_set_canonicalizes_len1_strings_into_chars() {
    let (_heap, mut ctx) = test_ctx();

    let mut set = UnicodeSet::new();
    set
      .insert_string(&mut ctx, &[b'x' as u16])
      .expect("insert");
    assert!(set.contains_char(b'x' as u16));
    assert!(!set.may_contain_strings());
    assert!(set.iter_strings_desc_len().next().is_none());
  }

  #[test]
  fn unicode_set_set_ops_chars_vs_strings() {
    let (_heap, mut ctx) = test_ctx();

    // Mirrors a common test262-style case:
    // `_ && \q{0|9\uFE0F\u20E3}` => empty.
    let mut left = UnicodeSet::new();
    left.insert_char(b'_' as u16);

    let mut right = UnicodeSet::new();
    right
      .insert_string(&mut ctx, &[b'0' as u16])
      .expect("insert"); // canonicalized into chars
    right
      .insert_string(&mut ctx, &[b'9' as u16, 0xFE0F, 0x20E3])
      .expect("insert");

    let inter = left.intersection(&mut ctx, &right).expect("intersection");
    assert!(inter.is_empty());

    // Union should contain both the character and the multi-unit string.
    let uni = left.union(&mut ctx, &right).expect("union");
    assert!(uni.contains_char(b'_' as u16));
    assert!(uni.contains_char(b'0' as u16));
    assert!(uni.contains_string(&[b'9' as u16, 0xFE0F, 0x20E3]));

    // Subtraction should remove only matching element kinds.
    let diff = uni.difference(&mut ctx, &right).expect("difference");
    assert!(diff.contains_char(b'_' as u16));
    assert!(!diff.contains_char(b'0' as u16));
    assert!(!diff.contains_string(&[b'9' as u16, 0xFE0F, 0x20E3]));
  }

  #[test]
  fn unicode_set_string_iteration_is_descending_length_stable() {
    let (_heap, mut ctx) = test_ctx();

    let mut set = UnicodeSet::new();
    // len2
    set
      .insert_string(&mut ctx, &[b'b' as u16, b'b' as u16])
      .expect("insert");
    // len3
    set
      .insert_string(&mut ctx, &[b'c' as u16, b'c' as u16, b'c' as u16])
      .expect("insert");
    // empty string
    set.insert_string(&mut ctx, &[]).expect("insert");
    // len2 (should come after "bb", stable for equal lengths)
    set
      .insert_string(&mut ctx, &[b'd' as u16, b'd' as u16])
      .expect("insert");
    // len1 (canonicalized, not a string element)
    set
      .insert_string(&mut ctx, &[b'a' as u16])
      .expect("insert");

    let got: Vec<Vec<u16>> = set.iter_strings_desc_len().map(|s| s.to_vec()).collect();
    let want: Vec<Vec<u16>> = vec![
      vec![b'c' as u16, b'c' as u16, b'c' as u16],
      vec![b'b' as u16, b'b' as u16],
      vec![b'd' as u16, b'd' as u16],
      vec![],
    ];
    assert_eq!(got, want);
  }

  #[test]
  fn unicode_set_complement_against_universe() {
    let (_heap, mut ctx) = test_ctx();

    let mut universe = UnicodeSet::new();
    universe.insert_char(b'a' as u16);
    universe.insert_char(b'b' as u16);
    universe.insert_char(b'c' as u16);
    universe
      .insert_string(&mut ctx, &[b'x' as u16, b'y' as u16])
      .expect("insert");
    universe.insert_string(&mut ctx, &[]).expect("insert");

    let mut subset = UnicodeSet::new();
    subset.insert_char(b'b' as u16);
    subset
      .insert_string(&mut ctx, &[b'x' as u16, b'y' as u16])
      .expect("insert");

    let comp = subset
      .complement_in(&mut ctx, &universe)
      .expect("complement");
    assert!(comp.contains_char(b'a' as u16));
    assert!(!comp.contains_char(b'b' as u16));
    assert!(comp.contains_char(b'c' as u16));
    assert!(comp.contains_string(&[]));
    assert!(!comp.contains_string(&[b'x' as u16, b'y' as u16]));
  }
}

#[cfg(test)]
mod unicode_set_vm_tests {
  use super::*;

  fn build_trie(strings: &[&[u16]]) -> StringTrie {
    let heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 32 * 1024 * 1024));
    let mut tick = || Ok(());
    let mut ctx = CompileCtx::new(&heap, &mut tick);
    StringTrie::try_build_from_slices(&mut ctx, strings.iter().copied(), false).unwrap()
  }

  #[test]
  fn unicode_set_matches_strings_longest_first_then_char_then_empty() {
    // Class elements: "ab" (string), "a" (single), "" (empty).
    // Then literal 'b' must match.
    //
    // On input "ab":
    // - Try "ab" first (consumes 2) => next 'b' fails (OOB)
    // - Backtrack to "a" (consumes 1) => 'b' matches
    // - Empty is last resort.
    let ab: [u16; 2] = [b'a' as u16, b'b' as u16];
    let trie = build_trie(&[&ab]);
    let single = CharClass {
      negated: false,
      items: vec![CharClassItem::Char(b'a' as u32)],
    };
    let cls = UnicodeSetClass {
      strings: trie,
      single,
      has_empty: true,
    };
    let program = RegExpProgram {
      insts: vec![Inst::UnicodeSet(cls), Inst::Char(b'b' as u32), Inst::Match],
      capture_count: 1,
      repeat_count: 0,
      named_capture_groups: vec![],
    };

    let exec_mem = RegExpExecMemoryBudget::new(1024 * 1024);
    let mut tick = || Ok(());

    let flags = RegExpFlags::default();

    // Backtracking should find the single-character alternative.
    let input_ab: Vec<u16> = [b'a' as u16, b'b' as u16].to_vec();
    let m = program
      .exec_at(&input_ab, 0, flags, &mut tick, &exec_mem, None)
      .unwrap()
      .unwrap();
    assert_eq!(m.end, 2);

    // Empty-string alternative should still work as a last resort.
    let input_b: Vec<u16> = [b'b' as u16].to_vec();
    let m = program
      .exec_at(&input_b, 0, flags, &mut tick, &exec_mem, None)
      .unwrap()
      .unwrap();
    assert_eq!(m.end, 1);
  }

  #[test]
  fn unicode_set_pushes_multiple_string_alternatives() {
    // Class elements: "abc" | "ab".
    // Then literal 'c' must match.
    //
    // On input "abc":
    // - Try "abc" first (longer) => fails at trailing 'c' (OOB)
    // - Backtrack to "ab" => 'c' matches
    let ab: [u16; 2] = [b'a' as u16, b'b' as u16];
    let abc: [u16; 3] = [b'a' as u16, b'b' as u16, b'c' as u16];
    let trie = build_trie(&[&ab, &abc]);
    let single = CharClass {
      negated: false,
      items: vec![],
    };
    let cls = UnicodeSetClass {
      strings: trie,
      single,
      has_empty: false,
    };
    let program = RegExpProgram {
      insts: vec![Inst::UnicodeSet(cls), Inst::Char(b'c' as u32), Inst::Match],
      capture_count: 1,
      repeat_count: 0,
      named_capture_groups: vec![],
    };

    let exec_mem = RegExpExecMemoryBudget::new(1024 * 1024);
    let mut tick = || Ok(());
    let flags = RegExpFlags::default();

    let input: Vec<u16> = [b'a' as u16, b'b' as u16, b'c' as u16].to_vec();
    let m = program
      .exec_at(&input, 0, flags, &mut tick, &exec_mem, None)
      .unwrap()
      .unwrap();
    assert_eq!(m.end, 3);
  }
}
