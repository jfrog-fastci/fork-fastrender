//! ECMAScript `RegExp` parsing + execution (partial).
//!
//! This is a small backtracking VM designed for `vm-js`:
//! - Operates over UTF-16 code units (JS string model).
//! - Provides explicit `vm.tick()` hooks in the backtracking loop so hostile patterns can be
//!   interrupted via fuel/deadline/interrupt budgets.
//! - Focuses on the subset needed for baseline real-world behaviour: literals, character classes,
//!   groups, alternation, quantifiers, `^`/`$`, `.`/dotAll, `\\b`/`\\B`, and basic lookahead.
//!
//! This module intentionally does **not** attempt to be a full spec implementation yet (e.g.
//! unicode property escapes, full unicode case folding, and lookbehind are not implemented).
//! Call sites must treat compilation failures as `SyntaxError`.

use crate::tick::{tick_every, DEFAULT_TICK_EVERY};
use crate::{Heap, HeapLimits, VmError};
use core::alloc::Layout;
use core::cell::Cell;
use core::mem;
use core::ptr;
use std::alloc::alloc;

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

fn box_try_new_vm<T>(value: T) -> Result<Box<T>, VmError> {
  let size = mem::size_of::<T>();
  if size == 0 {
    return Ok(Box::new(value));
  }

  let layout = Layout::new::<T>();
  // SAFETY: We allocate enough space for `T` and immediately initialise it before converting it
  // into a `Box<T>`.
  unsafe {
    let raw = alloc(layout) as *mut T;
    if raw.is_null() {
      return Err(VmError::OutOfMemory);
    }
    ptr::write(raw, value);
    Ok(Box::from_raw(raw))
  }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegExpFlags {
  pub(crate) global: bool,
  pub(crate) ignore_case: bool,
  pub(crate) multiline: bool,
  pub(crate) dot_all: bool,
  pub(crate) unicode: bool,
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
          if flags.unicode {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            }
            .into());
          }
          flags.unicode = true;
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
    let mut out = String::new();
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
    if self.sticky {
      out.push('y');
    }
    out
  }
}

#[derive(Debug, Clone)]
pub struct RegExpProgram {
  insts: Vec<Inst>,
  pub(crate) capture_count: usize,
  pub(crate) repeat_count: usize,
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
    for inst in self.insts.iter() {
      match inst {
        Inst::Class(cls) => {
          total = total.saturating_add(cls.heap_size_bytes());
        }
        Inst::LookAhead { program, .. } => {
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
            let Some(&u) = input.get(state.pos) else {
              break;
            };
            if !char_eq(*ch, u, flags.ignore_case) {
              break;
            }
            state.pos += 1;
            state.pc += 1;
          }
          Inst::Any => {
            let Some(&u) = input.get(state.pos) else {
              break;
            };
            if !flags.dot_all && is_line_terminator_unit(u) {
              break;
            }
            state.pos += 1;
            state.pc += 1;
          }
          Inst::Class(cls) => {
            let Some(&u) = input.get(state.pos) else {
              break;
            };
            if !cls.matches(u, flags.ignore_case) {
              break;
            }
            state.pos += 1;
            state.pc += 1;
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
            let at = is_word_boundary(input, state.pos);
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
            if let Some(dst) = state.captures.get_mut(*slot) {
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
            let slice = &input[cap_start..cap_end];
            if state.pos + slice.len() > input.len() {
              break;
            }
            if !slice
              .iter()
              .copied()
              .zip(input[state.pos..state.pos + slice.len()].iter().copied())
              .all(|(a, b)| char_eq(a, b, flags.ignore_case))
            {
              break;
            }
            state.pos += slice.len();
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
          } => {
            let id = *id;
            let Some(rep) = state.repeats.get(id).copied() else {
              break;
            };
            let count = rep.count;
            let last_pos = rep.last_pos;

            // Empty-match guard: if the previous iteration started at this same input position and
            // we've satisfied the minimum, don't enter the body again (avoids infinite loops for
            // patterns like `(?:)*` and `(a*)*`).
            if count >= *min && last_pos == state.pos && count != 0 {
              state.pc = *exit;
              continue;
            }

            if count < *min {
              if let Some(rep) = state.repeats.get_mut(id) {
                rep.last_pos = state.pos;
                rep.count = rep.count.saturating_add(1);
              }
              state.pc += 1;
              continue;
            }
            if let Some(max) = max {
              if count >= *max {
                state.pc = *exit;
                continue;
              }
            }

            if *greedy {
              // Try the body first, but keep the "stop" continuation on the backtracking stack.
              let mut stop = state.try_clone(exec_mem)?;
              stop.pc = *exit;
              stack_try_push(&mut stack, &mut stack_mem, exec_mem, stop)?;
              if let Some(rep) = state.repeats.get_mut(id) {
                rep.last_pos = state.pos;
                rep.count = rep.count.saturating_add(1);
              }
              state.pc += 1;
            } else {
              // Lazy: try stopping first, but keep the "take body" continuation on the stack.
              let mut body = state.try_clone(exec_mem)?;
              if let Some(body_rep) = body.repeats.get_mut(id) {
                body_rep.last_pos = body.pos;
                body_rep.count = body_rep.count.saturating_add(1);
              }
              body.pc += 1;
              stack_try_push(&mut stack, &mut stack_mem, exec_mem, body)?;
              state.pc = *exit;
            }
          }
          Inst::RepeatEnd { start } => {
            state.pc = *start;
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
            match (sub.is_some(), *negative) {
              (true, true) => {
                // Negative lookahead matched => fail this branch.
                break;
              }
              (false, false) => {
                // Positive lookahead failed.
                break;
              }
              (false, true) => {
                // Negative lookahead failed => success, consume nothing.
                state.pc += 1;
              }
              (true, false) => {
                // Positive lookahead matched => merge captures (excluding group 0).
                let matched = sub.unwrap();
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
        Inst::AssertStart => Inst::AssertStart,
        Inst::AssertEnd => Inst::AssertEnd,
        Inst::WordBoundary { negated } => Inst::WordBoundary { negated: *negated },
        Inst::Save(slot) => Inst::Save(*slot),
        Inst::BackRef(group) => Inst::BackRef(*group),
        Inst::Split(a, b) => Inst::Split(*a, *b),
        Inst::Jump(target) => Inst::Jump(*target),
        Inst::RepeatStart {
          id,
          min,
          max,
          greedy,
          exit,
        } => Inst::RepeatStart {
          id: *id,
          min: *min,
          max: *max,
          greedy: *greedy,
          exit: *exit,
        },
        Inst::RepeatEnd { start } => Inst::RepeatEnd { start: *start },
        Inst::LookAhead { program, negative } => Inst::LookAhead {
          program: box_try_new_vm(program.try_clone()?)?,
          negative: *negative,
        },
        Inst::Match => Inst::Match,
      };
      // `insts` was reserved to `self.insts.len()` above; pushing within that bound is infallible.
      insts.push(cloned);
    }

    Ok(Self {
      insts,
      capture_count: self.capture_count,
      repeat_count: self.repeat_count,
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
  Char(u16),
  Any,
  Class(CharClass),
  AssertStart,
  AssertEnd,
  WordBoundary { negated: bool },
  Save(usize),
  BackRef(u32),
  Split(usize, usize),
  Jump(usize),
  RepeatStart {
    id: usize,
    min: u32,
    max: Option<u32>,
    greedy: bool,
    exit: usize,
  },
  RepeatEnd {
    start: usize,
  },
  LookAhead {
    program: Box<RegExpProgram>,
    negative: bool,
  },
  Match,
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

  fn matches(&self, u: u16, ignore_case: bool) -> bool {
    let mut any = false;
    for item in self.items.iter() {
      if item.matches(u, ignore_case) {
        any = true;
        break;
      }
    }
    if self.negated { !any } else { any }
  }
}

#[derive(Debug, Clone, Copy)]
enum CharClassItem {
  Char(u16),
  Range(u16, u16),
  Digit { negated: bool },
  Word { negated: bool },
  Space { negated: bool },
}

impl CharClassItem {
  fn matches(self, u: u16, ignore_case: bool) -> bool {
    match self {
      CharClassItem::Char(c) => char_eq(c, u, ignore_case),
      CharClassItem::Range(a, b) => {
        if a <= b {
          if !ignore_case {
            return u >= a && u <= b;
          }
          // Minimal ASCII-only case-folding for common `[a-z]` / `[A-Z]` ranges.
          if (b'a' as u16..=b'z' as u16).contains(&a) && (b'a' as u16..=b'z' as u16).contains(&b)
          {
            let u = ascii_lower(u);
            return u >= a && u <= b;
          }
          if (b'A' as u16..=b'Z' as u16).contains(&a) && (b'A' as u16..=b'Z' as u16).contains(&b)
          {
            let u = ascii_lower(u);
            return u >= ascii_lower(a) && u <= ascii_lower(b);
          }
          u >= a && u <= b
        } else {
          false
        }
      }
      CharClassItem::Digit { negated } => {
        let is_digit = (b'0' as u16..=b'9' as u16).contains(&u);
        if negated { !is_digit } else { is_digit }
      }
      CharClassItem::Word { negated } => {
        let is_word = is_word_unit(u);
        if negated { !is_word } else { is_word }
      }
      CharClassItem::Space { negated } => {
        // `\s` in ECMAScript RegExp matches the union of WhiteSpace and LineTerminator
        // (https://tc39.es/ecma262/#sec-characterclassescape).
        //
        // Keep this set in sync with `builtins::is_trim_whitespace_unit`.
        let is_space = matches!(
          u,
          // WhiteSpace (ECMA-262)
          0x0009
            | 0x000B
            | 0x000C
            | 0x0020
            | 0x00A0
            | 0x1680
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
            // LineTerminator (ECMA-262)
            | 0x000A
            | 0x000D
            | 0x2028
            | 0x2029
        ) || matches!(u, 0x2000..=0x200A);
        if negated { !is_space } else { is_space }
      }
    }
  }
}

fn ascii_lower(u: u16) -> u16 {
  if (b'A' as u16..=b'Z' as u16).contains(&u) {
    u + 32
  } else {
    u
  }
}

fn char_eq(a: u16, b: u16, ignore_case: bool) -> bool {
  if !ignore_case {
    return a == b;
  }
  ascii_lower(a) == ascii_lower(b)
}

fn is_line_terminator_unit(u: u16) -> bool {
  matches!(u, 0x000A | 0x000D | 0x2028 | 0x2029)
}

fn is_word_unit(u: u16) -> bool {
  matches!(u, 0x0030..=0x0039)
    || matches!(u, 0x0061..=0x007A)
    || matches!(u, 0x0041..=0x005A)
    || u == (b'_' as u16)
}

fn is_word_boundary(input: &[u16], pos: usize) -> bool {
  let left = pos.checked_sub(1).and_then(|i| input.get(i)).copied();
  let right = input.get(pos).copied();
  let left_word = left.is_some_and(is_word_unit);
  let right_word = right.is_some_and(is_word_unit);
  left_word != right_word
}

pub(crate) fn advance_string_index(input: &[u16], index: usize, unicode: bool) -> usize {
  if index >= input.len() {
    return index.saturating_add(1);
  }
  if !unicode {
    return index.saturating_add(1);
  }
  let u = input[index];
  if (0xD800..=0xDBFF).contains(&u) && index + 1 < input.len() {
    let u2 = input[index + 1];
    if (0xDC00..=0xDFFF).contains(&u2) {
      return index + 2;
    }
  }
  index + 1
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
}

#[derive(Debug, Clone)]
enum Atom {
  Literal(u16),
  Any,
  Class(CharClass),
  Group { capture: Option<u32>, disj: Disjunction },
  BackRef(u32),
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
  let per_unit = INSTS_PER_UNIT
    .saturating_mul(mem::size_of::<Inst>())
    .saturating_add(TERMS_PER_UNIT.saturating_mul(mem::size_of::<Term>()))
    .saturating_add(ALTS_PER_UNIT.saturating_mul(mem::size_of::<Alternative>()))
    .saturating_add(END_JUMPS_PER_UNIT.saturating_mul(mem::size_of::<usize>()))
    .saturating_add(CLASS_ITEMS_PER_UNIT.saturating_mul(mem::size_of::<CharClassItem>()))
    .saturating_add(PROGRAMS_PER_UNIT.saturating_mul(mem::size_of::<RegExpProgram>()));

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

  let mut parser = Parser::new(pattern, flags);
  let disj = parser.parse_disjunction(&mut ctx, None)?;
  if parser.peek().is_some() {
    return Err(RegExpSyntaxError {
      message: "Invalid regular expression",
    }
    .into());
  }
  let capture_count = parser.capture_count as usize + 1;
  let mut builder = ProgramBuilder::new(capture_count);
  builder.compile_disjunction(&mut ctx, disj)?;
  builder.emit(&mut ctx, Inst::Match)?;
  Ok(builder.finish())
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
  capture_count: u32,
}

impl<'a> Parser<'a> {
  fn new(units: &'a [u16], flags: RegExpFlags) -> Self {
    Self {
      units,
      idx: 0,
      flags,
      capture_count: 0,
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

    // Lookahead assertions: `(?=...)` / `(?!...)`.
    if u == (b'(' as u16) {
      if self.units.get(self.idx + 1) == Some(&(b'?' as u16)) {
        if let Some(kind) = self.units.get(self.idx + 2).copied() {
          if kind == (b'=' as u16) || kind == (b'!' as u16) {
            // Consume "(?=" / "(?!".
            self.idx += 3;
            let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
            if !self.eat(b')' as u16) {
              return Err(RegExpSyntaxError {
                message: "Unterminated group",
              }
              .into());
            }
            return Ok(Term::Assertion(Assertion::LookAhead {
              negative: kind == (b'!' as u16),
              disj,
            }));
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
        // `{` is only a quantifier delimiter when it follows an atom; here it's an atom itself.
        Ok(Atom::Literal(x))
      }
      x if x == (b'}' as u16) && self.flags.unicode => Err(RegExpSyntaxError {
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
        if is_line_terminator_unit(x) {
          return Err(RegExpSyntaxError {
            message: "Invalid regular expression",
          }
          .into());
        }
        Ok(Atom::Literal(x))
      }
    }
  }

  fn parse_group(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Atom, RegExpCompileError> {
    // `(` has already been consumed.
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
          Ok(Atom::Group { capture: None, disj })
        }
        x if x == (b'<' as u16) => {
          // Named capturing group: `(?<name>...)`.
          let mut name_i: usize = 0;
          while let Some(u) = self.peek() {
            if name_i != 0 {
              ctx.tick_every(name_i)?;
            }
            name_i = name_i.wrapping_add(1);
            self.next();
            if u == (b'>' as u16) {
              break;
            }
          }
          if self.peek().is_none() {
            return Err(RegExpSyntaxError { message: "Invalid group" }.into());
          }
          self.capture_count = self.capture_count.saturating_add(1);
          let idx = self.capture_count;
          let disj = self.parse_disjunction(ctx, Some(b')' as u16))?;
          if !self.eat(b')' as u16) {
            return Err(RegExpSyntaxError {
              message: "Unterminated group",
            }
            .into());
          }
          Ok(Atom::Group {
            capture: Some(idx),
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
      Ok(Atom::Group {
        capture: Some(idx),
        disj,
      })
    }
  }

  fn parse_class(&mut self, ctx: &mut CompileCtx<'_>) -> Result<Atom, RegExpCompileError> {
    // `[` has already been consumed.
    let mut negated = false;
    if self.eat(b'^' as u16) {
      negated = true;
    }
    let mut items: Vec<CharClassItem> = Vec::new();

    let mut first = true;
    let mut item_i: usize = 0;
    loop {
      let Some(u) = self.peek() else {
        return Err(RegExpSyntaxError {
          message: "Unterminated character class",
        }
        .into());
      };
      if u == (b']' as u16) {
        // In most cases, `]` terminates the character class unless it is the first class character,
        // in which case it is treated as a literal `]` and a later `]` terminates the class.
        //
        // ECMAScript additionally allows `[^]` (a negated empty character class) to be parsed by
        // treating the `]` immediately after `^` as the class terminator. This is widely used as a
        // "dotAll" workaround that matches any UTF-16 code unit including line terminators.
        //
        // Ensure we do not break `[^]]`: in that pattern, the first `]` is intended to be a
        // literal and the second `]` closes the class. A simple rule that matches JS engines:
        // - If the class is negated, and the first char after `^` is `]`, and the following char is
        //   NOT `]`, treat it as the terminator (empty class).
        // - If it *is* followed by another `]`, treat the first as a literal and let the second
        //   terminate.
        if !first {
          self.next();
          break;
        }
        if negated && self.units.get(self.idx + 1).copied() != Some(b']' as u16) {
          self.next();
          break;
        }
      }
      first = false;

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
            ctx.vec_try_push(&mut items, CharClassItem::Range(a, b))?;
            continue;
          } else {
            // Not a valid range; treat '-' literally and keep both atoms.
            self.idx = save;
          }
        }
      }
      ctx.vec_try_push(&mut items, atom)?;
    }

    Ok(Atom::Class(CharClass { negated, items }))
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
          x if x == (b'n' as u16) => Ok(CharClassItem::Char(0x000A)),
          x if x == (b'r' as u16) => Ok(CharClassItem::Char(0x000D)),
          x if x == (b't' as u16) => Ok(CharClassItem::Char(0x0009)),
          x if x == (b'v' as u16) => Ok(CharClassItem::Char(0x000B)),
          x if x == (b'f' as u16) => Ok(CharClassItem::Char(0x000C)),
          x if x == (b'x' as u16) => Ok(CharClassItem::Char(self.parse_hex_escape_2(ctx)?)),
          x if x == (b'u' as u16) => Ok(CharClassItem::Char(self.parse_unicode_escape(ctx)?)),
          other => Ok(CharClassItem::Char(other)),
        }
      }
      other => Ok(CharClassItem::Char(other)),
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
      x if x == (b'0' as u16) => Ok(Atom::Literal(0x0000)),
      x if (b'1' as u16..=b'9' as u16).contains(&x) => {
        // Decimal escape => backreference (approximation).
        let mut n: u32 = (x - (b'0' as u16)) as u32;
        let mut digit_i: usize = 0;
        while let Some(d) = self.peek() {
          if !(b'0' as u16..=b'9' as u16).contains(&d) {
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
        Ok(Atom::BackRef(n))
      }
      x if x == (b'x' as u16) => Ok(Atom::Literal(self.parse_hex_escape_2(ctx)?)),
      x if x == (b'u' as u16) => Ok(Atom::Literal(self.parse_unicode_escape(ctx)?)),
      other => Ok(Atom::Literal(other)),
    }
  }

  fn parse_hex_escape_2(&mut self, _ctx: &mut CompileCtx<'_>) -> Result<u16, RegExpCompileError> {
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
    Ok(((v1 << 4) | v2) as u16)
  }

  fn parse_unicode_escape(&mut self, ctx: &mut CompileCtx<'_>) -> Result<u16, RegExpCompileError> {
    if self.flags.unicode && self.peek() == Some(b'{' as u16) {
      // \u{...}
      self.next();
      let mut value: u32 = 0;
      let mut saw_digit = false;
      let mut digit_i: usize = 0;
      while let Some(u) = self.peek() {
        if u == (b'}' as u16) {
          self.next();
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
      // Encode as UTF-16 code units; for now, only return the first unit for non-BMP code points.
      // This is an approximation that matches the common BMP cases used in scripts.
      if value <= 0xFFFF {
        return Ok(value as u16);
      }
      // Non-BMP: return high surrogate; matching will see the low surrogate as a literal unit in
      // the pattern only when it is explicitly written.
      let cp = value - 0x10000;
      let high = 0xD800 + ((cp >> 10) as u16);
      Ok(high)
    } else {
      // \uXXXX
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
      Ok(value as u16)
    }
  }

  fn parse_quantifier_if_present(
    &mut self,
    ctx: &mut CompileCtx<'_>,
  ) -> Result<Option<Quantifier>, RegExpCompileError> {
    let Some(u) = self.peek() else {
      return Ok(None);
    };
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
          self.idx = save;
          return Ok(None);
        };
        if !(b'0' as u16..=b'9' as u16).contains(&first) {
          // Not a quantifier; treat `{` as a literal.
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
          self.idx = save;
          return Ok(None);
        }
        (m, n)
      }
      x if x == (b'}' as u16) && self.flags.unicode => {
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
}

impl ProgramBuilder {
  fn new(capture_count: usize) -> Self {
    Self {
      insts: Vec::new(),
      repeat_count: 0,
      capture_count,
    }
  }

  fn finish(self) -> RegExpProgram {
    RegExpProgram {
      insts: self.insts,
      capture_count: self.capture_count,
      repeat_count: self.repeat_count,
    }
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
    if disj.alts.is_empty() {
      return Ok(());
    }
    let mut alts = disj.alts;
    if alts.len() == 1 {
      return self.compile_alternative(ctx, alts.pop().unwrap());
    }

    let last_idx = alts.len().saturating_sub(1);
    let mut end_jumps: Vec<usize> = Vec::new();
    for (i, alt) in alts.into_iter().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }
      if i == last_idx {
        self.compile_alternative(ctx, alt)?;
        break;
      }
      // Split to this alternative (fallthrough) or the next one (patched).
      let fallthrough = self
        .insts
        .len()
        .checked_add(1)
        .ok_or(RegExpCompileError::OutOfMemory)?;
      let split_pc = self.emit(ctx, Inst::Split(fallthrough, 0))?;
      self.compile_alternative(ctx, alt)?;
      let jmp_pc = self.emit(ctx, Inst::Jump(0))?;
      ctx.vec_try_push(&mut end_jumps, jmp_pc)?;
      // Patch the split's second branch to the start of the next alternative.
      let next_pc = self.insts.len();
      let Inst::Split(_, ref mut b) = self.insts[split_pc] else {
        unreachable!();
      };
      *b = next_pc;
    }

    let end = self.insts.len();
    for pc in end_jumps {
      let Inst::Jump(ref mut target) = self.insts[pc] else {
        unreachable!();
      };
      *target = end;
    }
    Ok(())
  }

  fn compile_alternative(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    alt: Alternative,
  ) -> Result<(), RegExpCompileError> {
    for (i, term) in alt.terms.into_iter().enumerate() {
      if i != 0 {
        ctx.tick_every(i)?;
      }
      self.compile_term(ctx, term)?;
    }
    Ok(())
  }

  fn compile_term(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    term: Term,
  ) -> Result<(), RegExpCompileError> {
    match term {
      Term::Assertion(a) => self.compile_assertion(ctx, a),
      Term::Atom(atom, quant) => match quant {
        Some(q) => self.compile_quantified(ctx, atom, q),
        None => self.compile_atom(ctx, atom),
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
        let mut nested = ProgramBuilder::new(self.capture_count);
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
    }
    Ok(())
  }

  fn compile_quantified(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    atom: Atom,
    q: Quantifier,
  ) -> Result<(), RegExpCompileError> {
    let id = self.repeat_count;
    self.repeat_count = self.repeat_count.saturating_add(1);
    let start_pc = self.emit(ctx, Inst::RepeatStart {
      id,
      min: q.min,
      max: q.max,
      greedy: q.greedy,
      exit: 0, // patch
    })?;
    self.compile_atom(ctx, atom)?;
    self.emit(ctx, Inst::RepeatEnd { start: start_pc })?;
    let exit = self.insts.len();
    let Inst::RepeatStart { exit: ref mut e, .. } = self.insts[start_pc] else {
      unreachable!();
    };
    *e = exit;
    Ok(())
  }

  fn compile_atom(
    &mut self,
    ctx: &mut CompileCtx<'_>,
    atom: Atom,
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
      Atom::BackRef(n) => {
        self.emit(ctx, Inst::BackRef(n))?;
      }
      Atom::Group { capture, disj } => {
        if let Some(idx) = capture {
          let start_slot = (idx as usize).saturating_mul(2);
          self.emit(ctx, Inst::Save(start_slot))?;
          self.compile_disjunction(ctx, disj)?;
          self.emit(ctx, Inst::Save(start_slot.saturating_add(1)))?;
        } else {
          self.compile_disjunction(ctx, disj)?;
        }
      }
    }
    Ok(())
  }
}
