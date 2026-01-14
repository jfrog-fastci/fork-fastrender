use crate::VmError;
use std::cmp::Ordering;

/// Default tick cadence for tight internal loops that process attacker-controlled input.
///
/// The VM's AST interpreter already calls `Vm::tick()` once per statement / loop iteration.
/// However, many built-ins and internal helpers can still perform O(n) work in pure Rust (string
/// parsing, JSON parsing, bulk copies, etc). Those routines should periodically call the supplied
/// `tick` closure to ensure fuel/deadline/interrupt budgets are observed.
pub(crate) const DEFAULT_TICK_EVERY: usize = 1024;

/// Unstable sorting with periodic ticks.
///
/// Rust's slice sorting APIs are infallible, but module loading paths sometimes need to sort
/// attacker-controlled lists (import attributes, module namespace exports, etc). Those sorts can do
/// `O(n log n)` comparisons; without periodically calling `tick()`, they can bypass fuel/deadline/
/// interrupt budgets.
///
/// This helper makes `sort_unstable_by` *cooperatively interruptible* by:
/// - counting comparator invocations, and
/// - calling the supplied `tick` closure every [`DEFAULT_TICK_EVERY`] comparisons.
///
/// If `tick` returns an error (e.g. out-of-fuel), sorting is aborted and the error is returned.
///
/// # Panic safety
///
/// This routine uses a sentinel panic + `catch_unwind` internally to escape Rust's infallible sort
/// implementation. All panics are caught and surfaced as a `VmError` (either the tick error or an
/// `InvariantViolation`), so no panics escape to callers.
pub(crate) fn sort_unstable_by_with_ticks<T>(
  slice: &mut [T],
  mut compare: impl FnMut(&T, &T) -> Ordering,
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<(), VmError> {
  // Ensure very small sorts still observe VM budgets. Without this pre-tick, a sort that performs
  // fewer than `DEFAULT_TICK_EVERY` comparisons could run without *any* budget/interrupt checks.
  tick()?;

  // Use a sentinel to abort `sort_unstable_by` early when `tick()` fails.
  struct TickAbort;

  let mut comparisons: usize = 0;
  let mut tick_err: Option<VmError> = None;

  let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    slice.sort_unstable_by(|a, b| {
      comparisons = comparisons.wrapping_add(1);
      // Avoid ticking on the first comparison so small sorts don't effectively double-charge fuel
      // too aggressively (we already tick once before entering this helper).
      if (comparisons & (DEFAULT_TICK_EVERY - 1)) == 0 {
        if let Err(err) = tick() {
          tick_err = Some(err);
          std::panic::panic_any(TickAbort);
        }
      }
      compare(a, b)
    });
  }));

  match res {
    Ok(()) => Ok(()),
    Err(panic) => {
      if panic.is::<TickAbort>() {
        return Err(tick_err.unwrap_or(VmError::InvariantViolation(
          "sort aborted without a captured tick error",
        )));
      }
      Err(VmError::InvariantViolation("sort panicked"))
    }
  }
}

/// Call `tick()` every `every` iterations (including iteration 0).
///
/// `every` should be a power-of-two so the check can be compiled down to a fast bitmask.
#[inline]
pub(crate) fn tick_every(
  i: usize,
  every: usize,
  tick: &mut (impl FnMut() -> Result<(), VmError> + ?Sized),
) -> Result<(), VmError> {
  debug_assert!(every.is_power_of_two(), "tick interval must be a power-of-two");
  if (i & (every - 1)) == 0 {
    tick()?;
  }
  Ok(())
}

/// `Vec::extend_from_slice` with periodic ticks for large slices.
///
/// This avoids long stretches of uninterruptible work when copying attacker-controlled data
/// (notably UTF-16 code units) into fresh buffers.
pub(crate) fn vec_try_extend_from_slice_with_ticks<T: Copy>(
  out: &mut Vec<T>,
  slice: &[T],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<(), VmError> {
  let needed = slice
    .len()
    .saturating_sub(out.capacity().saturating_sub(out.len()));
  if needed > 0 {
    out.try_reserve(needed).map_err(|_| VmError::OutOfMemory)?;
  }

  let mut start = 0;
  while start < slice.len() {
    let end = slice
      .len()
      .min(start.saturating_add(DEFAULT_TICK_EVERY));
    out.extend_from_slice(&slice[start..end]);
    start = end;
    if start < slice.len() {
      tick()?;
    }
  }

  Ok(())
}

/// Compare two UTF-16 code unit slices for equality with periodic ticks.
pub(crate) fn code_units_eq_with_ticks(
  a: &[u16],
  b: &[u16],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<bool, VmError> {
  if a.len() != b.len() {
    return Ok(false);
  }
  for (i, (&au, &bu)) in a.iter().zip(b.iter()).enumerate() {
    // Avoid ticking on the first iteration so short string comparisons don't effectively
    // double-charge fuel (the surrounding expression evaluation already ticks).
    if i != 0 {
      tick_every(i, DEFAULT_TICK_EVERY, &mut tick)?;
    }
    if au != bu {
      return Ok(false);
    }
  }
  Ok(true)
}

/// Lexicographically compare two UTF-16 code unit slices with periodic ticks.
pub(crate) fn code_units_cmp_with_ticks(
  a: &[u16],
  b: &[u16],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<Ordering, VmError> {
  let min_len = a.len().min(b.len());
  for i in 0..min_len {
    if i != 0 {
      tick_every(i, DEFAULT_TICK_EVERY, &mut tick)?;
    }
    let au = a[i];
    let bu = b[i];
    if au != bu {
      return Ok(au.cmp(&bu));
    }
  }
  Ok(a.len().cmp(&b.len()))
}
