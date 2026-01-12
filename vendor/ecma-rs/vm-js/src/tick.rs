use crate::VmError;

/// Default tick cadence for tight internal loops that process attacker-controlled input.
///
/// The VM's AST interpreter already calls `Vm::tick()` once per statement / loop iteration.
/// However, many built-ins and internal helpers can still perform O(n) work in pure Rust (string
/// parsing, JSON parsing, bulk copies, etc). Those routines should periodically call the supplied
/// `tick` closure to ensure fuel/deadline/interrupt budgets are observed.
pub(crate) const DEFAULT_TICK_EVERY: usize = 1024;

/// Call `tick()` every `every` iterations (including iteration 0).
///
/// `every` should be a power-of-two so the check can be compiled down to a fast bitmask.
#[inline]
pub(crate) fn tick_every(
  i: usize,
  every: usize,
  tick: &mut impl FnMut() -> Result<(), VmError>,
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
