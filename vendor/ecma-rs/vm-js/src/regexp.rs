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

use crate::VmError;
use core::cell::Cell;
use core::mem;

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

fn vec_try_push<T>(buf: &mut Vec<T>, value: T) -> Result<(), RegExpCompileError> {
  if buf.len() == buf.capacity() {
    buf
      .try_reserve(1)
      .map_err(|_| RegExpCompileError::OutOfMemory)?;
  }
  buf.push(value);
  Ok(())
}

fn boxed_slice_one<T>(value: T) -> Result<Box<[T]>, RegExpCompileError> {
  let mut buf = Vec::new();
  buf
    .try_reserve_exact(1)
    .map_err(|_| RegExpCompileError::OutOfMemory)?;
  buf.push(value);
  Ok(buf.into_boxed_slice())
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
  pub(crate) fn parse(units: &[u16]) -> Result<Self, RegExpSyntaxError> {
    let mut flags = RegExpFlags::default();
    for &u in units {
      let b = u as u32;
      if b > 0x7F {
        return Err(RegExpSyntaxError {
          message: "Invalid flags supplied to RegExp constructor",
        });
      }
      match b as u8 {
        b'g' => {
          if flags.global {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            });
          }
          flags.global = true;
        }
        b'i' => {
          if flags.ignore_case {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            });
          }
          flags.ignore_case = true;
        }
        b'm' => {
          if flags.multiline {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            });
          }
          flags.multiline = true;
        }
        b's' => {
          if flags.dot_all {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            });
          }
          flags.dot_all = true;
        }
        b'u' => {
          if flags.unicode {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            });
          }
          flags.unicode = true;
        }
        b'y' => {
          if flags.sticky {
            return Err(RegExpSyntaxError {
              message: "Invalid flags supplied to RegExp constructor",
            });
          }
          flags.sticky = true;
        }
        _ => {
          return Err(RegExpSyntaxError {
            message: "Invalid flags supplied to RegExp constructor",
          })
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
  insts: Box<[Inst]>,
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
    let mut total = self.insts.len().saturating_mul(mem::size_of::<Inst>());
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
          program: Box::new(program.try_clone()?),
          negative: *negative,
        },
        Inst::Match => Inst::Match,
      };
      // `insts` was reserved to `self.insts.len()` above; pushing within that bound should not need
      // to grow the Vec, but keep the push fallible to uphold the `try_clone` contract.
      vec_try_push_vm(&mut insts, cloned)?;
    }

    Ok(Self {
      insts: insts.into_boxed_slice(),
      capture_count: self.capture_count,
      repeat_count: self.repeat_count,
    })
  }
}

#[derive(Debug)]
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
  items: Box<[CharClassItem]>,
}

impl CharClass {
  fn heap_size_bytes(&self) -> usize {
    self.items.len().saturating_mul(mem::size_of::<CharClassItem>())
  }

  fn try_clone(&self) -> Result<Self, RegExpCompileError> {
    let mut items: Vec<CharClassItem> = Vec::new();
    items
      .try_reserve_exact(self.items.len())
      .map_err(|_| RegExpCompileError::OutOfMemory)?;
    items.extend_from_slice(&self.items);
    Ok(Self {
      negated: self.negated,
      items: items.into_boxed_slice(),
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
        let is_space = matches!(
          u,
          0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0xFEFF
        );
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

/// Compilation tick cadence (in approximate UTF-16 code units consumed / IR steps).
///
/// This is used to ensure hostile RegExp patterns cannot monopolize the VM for long periods of
/// time without observing fuel/deadline/interrupt budgets.
const REGEXP_COMPILE_TICK_EVERY: usize = 1024;

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
  tick: &mut dyn FnMut() -> Result<(), VmError>,
) -> Result<RegExpProgram, RegExpCompileError> {
  let (disj, capture_count) = {
    let mut parser = Parser::new(pattern, flags, tick);
    let disj = parser.parse_disjunction(None)?;
    if parser.peek().is_some() {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    }
    // Capture 0 is the overall match.
    let capture_count = parser.capture_count as usize + 1;
    (disj, capture_count)
  };

  let mut builder = ProgramBuilder::new(capture_count, tick);
  builder.compile_disjunction(&disj)?;
  builder.emit(Inst::Match)?;
  Ok(builder.finish())
}

pub(crate) fn compile_regexp(
  pattern: &[u16],
  flags: RegExpFlags,
) -> Result<RegExpProgram, RegExpCompileError> {
  let mut tick = || Ok(());
  compile_regexp_with_budget(pattern, flags, &mut tick)
}

struct Parser<'a, 't> {
  units: &'a [u16],
  idx: usize,
  flags: RegExpFlags,
  capture_count: u32,
  tick: &'t mut dyn FnMut() -> Result<(), VmError>,
  steps: usize,
  next_tick: usize,
}

impl<'a, 't> Parser<'a, 't> {
  fn new(
    units: &'a [u16],
    flags: RegExpFlags,
    tick: &'t mut dyn FnMut() -> Result<(), VmError>,
  ) -> Self {
    Self {
      units,
      idx: 0,
      flags,
      capture_count: 0,
      tick,
      steps: 0,
      next_tick: REGEXP_COMPILE_TICK_EVERY,
    }
  }

  fn peek(&self) -> Option<u16> {
    self.units.get(self.idx).copied()
  }

  fn bump(&mut self, n: usize) -> Result<(), RegExpCompileError> {
    self.idx = self.idx.saturating_add(n);
    self.steps = self.steps.saturating_add(n);
    while self.steps >= self.next_tick {
      (self.tick)().map_err(RegExpCompileError::from)?;
      self.next_tick = self.next_tick.saturating_add(REGEXP_COMPILE_TICK_EVERY);
    }
    Ok(())
  }

  fn next(&mut self) -> Result<Option<u16>, RegExpCompileError> {
    let u = self.peek();
    if u.is_some() {
      self.bump(1)?;
    }
    Ok(u)
  }

  fn eat(&mut self, ch: u16) -> Result<bool, RegExpCompileError> {
    if self.peek() != Some(ch) {
      return Ok(false);
    }
    self.bump(1)?;
    Ok(true)
  }

  fn parse_disjunction(
    &mut self,
    terminator: Option<u16>,
  ) -> Result<Disjunction, RegExpCompileError> {
    let mut alts: Vec<Alternative> = Vec::new();
    vec_try_push(&mut alts, self.parse_alternative(terminator)?)?;
    while self.eat(b'|' as u16)? {
      vec_try_push(&mut alts, self.parse_alternative(terminator)?)?;
    }
    Ok(Disjunction { alts })
  }

  fn parse_alternative(
    &mut self,
    terminator: Option<u16>,
  ) -> Result<Alternative, RegExpCompileError> {
    let mut terms: Vec<Term> = Vec::new();
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
      let term = self.parse_term(terminator)?;
      vec_try_push(&mut terms, term)?;
    }
    Ok(Alternative { terms })
  }

  fn parse_term(&mut self, terminator: Option<u16>) -> Result<Term, RegExpCompileError> {
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
            self.bump(3)?;
            let disj = self.parse_disjunction(Some(b')' as u16))?;
            if !self.eat(b')' as u16)? {
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
        let _ = self.next()?;
        return Ok(Term::Assertion(Assertion::Start));
      }
      x if x == (b'$' as u16) => {
        let _ = self.next()?;
        return Ok(Term::Assertion(Assertion::End));
      }
      x if x == (b'\\' as u16) => {
        // Might be a boundary assertion.
        let save = self.idx;
        let _ = self.next()?;
        let Some(next) = self.next()? else {
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
    let atom = self.parse_atom(terminator)?;
    let quant = self.parse_quantifier_if_present()?;
    Ok(Term::Atom(atom, quant))
  }

  fn parse_atom(&mut self, terminator: Option<u16>) -> Result<Atom, RegExpCompileError> {
    let Some(u) = self.next()? else {
      return Err(RegExpSyntaxError {
        message: "Invalid regular expression",
      }
      .into());
    };

    match u {
      x if x == (b'.' as u16) => Ok(Atom::Any),
      x if x == (b'[' as u16) => self.parse_class(),
      x if x == (b'(' as u16) => self.parse_group(),
      x if x == (b'\\' as u16) => self.parse_escape_atom(),
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

  fn parse_group(&mut self) -> Result<Atom, RegExpCompileError> {
    // `(` has already been consumed.
    if self.eat(b'?' as u16)? {
      let Some(next) = self.next()? else {
        return Err(RegExpSyntaxError { message: "Invalid group" }.into());
      };
      match next {
        x if x == (b':' as u16) => {
          // Non-capturing group.
          let disj = self.parse_disjunction(Some(b')' as u16))?;
          if !self.eat(b')' as u16)? {
            return Err(RegExpSyntaxError {
              message: "Unterminated group",
            }
            .into());
          }
          Ok(Atom::Group { capture: None, disj })
        }
        x if x == (b'<' as u16) => {
          // Named capturing group: `(?<name>...)`.
          while let Some(u) = self.peek() {
            let _ = self.next()?;
            if u == (b'>' as u16) {
              break;
            }
          }
          if self.peek().is_none() {
            return Err(RegExpSyntaxError { message: "Invalid group" }.into());
          }
          self.capture_count = self.capture_count.saturating_add(1);
          let idx = self.capture_count;
          let disj = self.parse_disjunction(Some(b')' as u16))?;
          if !self.eat(b')' as u16)? {
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
      let disj = self.parse_disjunction(Some(b')' as u16))?;
      if !self.eat(b')' as u16)? {
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

  fn parse_class(&mut self) -> Result<Atom, RegExpCompileError> {
    // `[` has already been consumed.
    let mut negated = false;
    if self.eat(b'^' as u16)? {
      negated = true;
    }
    let mut items: Vec<CharClassItem> = Vec::new();

    let mut first = true;
    loop {
      let Some(u) = self.peek() else {
        return Err(RegExpSyntaxError {
          message: "Unterminated character class",
        }
        .into());
      };
      if u == (b']' as u16) && !first {
        let _ = self.next()?;
        break;
      }
      first = false;

      let atom = self.parse_class_atom()?;
      // Range?
      if self.peek() == Some(b'-' as u16) {
        // Only treat as range when there's a following atom before `]`.
        let save = self.idx;
        let _ = self.next()?; // consume '-'
        if self.peek() == Some(b']' as u16) {
          // Literal '-' at end.
          self.idx = save;
        } else {
          let atom2 = self.parse_class_atom()?;
          if let (CharClassItem::Char(a), CharClassItem::Char(b)) = (atom, atom2) {
            vec_try_push(&mut items, CharClassItem::Range(a, b))?;
            continue;
          } else {
            // Not a valid range; treat '-' literally and keep both atoms.
            self.idx = save;
          }
        }
      }
      vec_try_push(&mut items, atom)?;
    }

    let items = items.into_boxed_slice();
    Ok(Atom::Class(CharClass { negated, items }))
  }

  fn parse_class_atom(&mut self) -> Result<CharClassItem, RegExpCompileError> {
    let Some(u) = self.next()? else {
      return Err(RegExpSyntaxError {
        message: "Invalid character class",
      }
      .into());
    };
    match u {
      x if x == (b'\\' as u16) => {
        let Some(e) = self.next()? else {
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
          x if x == (b'x' as u16) => Ok(CharClassItem::Char(self.parse_hex_escape_2()?)),
          x if x == (b'u' as u16) => Ok(CharClassItem::Char(self.parse_unicode_escape()?)),
          other => Ok(CharClassItem::Char(other)),
        }
      }
      other => Ok(CharClassItem::Char(other)),
    }
  }

  fn parse_escape_atom(&mut self) -> Result<Atom, RegExpCompileError> {
    let Some(e) = self.next()? else {
      return Err(RegExpSyntaxError { message: "Invalid escape" }.into());
    };
    match e {
      x if x == (b'd' as u16) => Ok(Atom::Class(CharClass {
        negated: false,
        items: boxed_slice_one(CharClassItem::Digit { negated: false })?,
      })),
      x if x == (b'D' as u16) => Ok(Atom::Class(CharClass {
        negated: false,
        items: boxed_slice_one(CharClassItem::Digit { negated: true })?,
      })),
      x if x == (b'w' as u16) => Ok(Atom::Class(CharClass {
        negated: false,
        items: boxed_slice_one(CharClassItem::Word { negated: false })?,
      })),
      x if x == (b'W' as u16) => Ok(Atom::Class(CharClass {
        negated: false,
        items: boxed_slice_one(CharClassItem::Word { negated: true })?,
      })),
      x if x == (b's' as u16) => Ok(Atom::Class(CharClass {
        negated: false,
        items: boxed_slice_one(CharClassItem::Space { negated: false })?,
      })),
      x if x == (b'S' as u16) => Ok(Atom::Class(CharClass {
        negated: false,
        items: boxed_slice_one(CharClassItem::Space { negated: true })?,
      })),
      x if x == (b'n' as u16) => Ok(Atom::Literal(0x000A)),
      x if x == (b'r' as u16) => Ok(Atom::Literal(0x000D)),
      x if x == (b't' as u16) => Ok(Atom::Literal(0x0009)),
      x if x == (b'v' as u16) => Ok(Atom::Literal(0x000B)),
      x if x == (b'f' as u16) => Ok(Atom::Literal(0x000C)),
      x if x == (b'0' as u16) => Ok(Atom::Literal(0x0000)),
      x if (b'1' as u16..=b'9' as u16).contains(&x) => {
        // Decimal escape => backreference (approximation).
        let mut n: u32 = (x - (b'0' as u16)) as u32;
        while let Some(d) = self.peek() {
          if !(b'0' as u16..=b'9' as u16).contains(&d) {
            break;
          }
          let _ = self.next()?;
          n = n
            .saturating_mul(10)
            .saturating_add((d - (b'0' as u16)) as u32);
        }
        Ok(Atom::BackRef(n))
      }
      x if x == (b'x' as u16) => Ok(Atom::Literal(self.parse_hex_escape_2()?)),
      x if x == (b'u' as u16) => Ok(Atom::Literal(self.parse_unicode_escape()?)),
      other => Ok(Atom::Literal(other)),
    }
  }

  fn parse_hex_escape_2(&mut self) -> Result<u16, RegExpCompileError> {
    let h1 = self
      .next()?
      .ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
        message: "Invalid escape",
      }))?;
    let h2 = self
      .next()?
      .ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
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

  fn parse_unicode_escape(&mut self) -> Result<u16, RegExpCompileError> {
    if self.flags.unicode && self.peek() == Some(b'{' as u16) {
      // \u{...}
      let _ = self.next()?;
      let mut value: u32 = 0;
      let mut saw_digit = false;
      while let Some(u) = self.peek() {
        if u == (b'}' as u16) {
          let _ = self.next()?;
          break;
        }
        let d = hex_value(u).ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
          message: "Invalid escape",
        }))?;
        let _ = self.next()?;
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
        let u = self
          .next()?
          .ok_or(RegExpCompileError::Syntax(RegExpSyntaxError {
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

  fn parse_quantifier_if_present(&mut self) -> Result<Option<Quantifier>, RegExpCompileError> {
    let Some(u) = self.peek() else {
      return Ok(None);
    };
    let (mut min, max): (u32, Option<u32>) = match u {
      x if x == (b'*' as u16) => {
        let _ = self.next()?;
        (0, None)
      }
      x if x == (b'+' as u16) => {
        let _ = self.next()?;
        (1, None)
      }
      x if x == (b'?' as u16) => {
        let _ = self.next()?;
        (0, Some(1))
      }
      x if x == (b'{' as u16) => {
        let save = self.idx;
        let _ = self.next()?;
        let Some(first) = self.peek() else {
          self.idx = save;
          return Ok(None);
        };
        if !(b'0' as u16..=b'9' as u16).contains(&first) {
          // Not a quantifier; treat `{` as a literal.
          self.idx = save;
          return Ok(None);
        }
        let m = self.parse_decimal_u32()?;
        let mut n: Option<u32> = None;
        if self.eat(b',' as u16)? {
          if let Some(d) = self.peek() {
            if (b'0' as u16..=b'9' as u16).contains(&d) {
              n = Some(self.parse_decimal_u32()?);
            } else {
              n = None;
            }
          }
        } else {
          n = Some(m);
        }
        if !self.eat(b'}' as u16)? {
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
      let _ = self.next()?;
      greedy = false;
    }

    // Special-case: `{0,}` should be treated as `*`.
    if max.is_none() && min == 0 {
      min = 0;
    }

    Ok(Some(Quantifier { min, max, greedy }))
  }

  fn parse_decimal_u32(&mut self) -> Result<u32, RegExpCompileError> {
    let mut n: u32 = 0;
    while let Some(u) = self.peek() {
      if !(b'0' as u16..=b'9' as u16).contains(&u) {
        break;
      }
      let _ = self.next()?;
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

struct ProgramBuilder<'t> {
  insts: Vec<Inst>,
  repeat_count: usize,
  capture_count: usize,
  tick: &'t mut dyn FnMut() -> Result<(), VmError>,
  steps: usize,
  next_tick: usize,
}

impl<'t> ProgramBuilder<'t> {
  fn new(
    capture_count: usize,
    tick: &'t mut dyn FnMut() -> Result<(), VmError>,
  ) -> Self {
    Self {
      insts: Vec::new(),
      repeat_count: 0,
      capture_count,
      tick,
      steps: 0,
      next_tick: REGEXP_COMPILE_TICK_EVERY,
    }
  }

  fn finish(self) -> RegExpProgram {
    RegExpProgram {
      insts: self.insts.into_boxed_slice(),
      capture_count: self.capture_count,
      repeat_count: self.repeat_count,
    }
  }

  fn bump(&mut self, n: usize) -> Result<(), RegExpCompileError> {
    self.steps = self.steps.saturating_add(n);
    while self.steps >= self.next_tick {
      (self.tick)().map_err(RegExpCompileError::from)?;
      self.next_tick = self.next_tick.saturating_add(REGEXP_COMPILE_TICK_EVERY);
    }
    Ok(())
  }

  fn emit(&mut self, inst: Inst) -> Result<usize, RegExpCompileError> {
    self.bump(1)?;
    if self.insts.len() == self.insts.capacity() {
      self
        .insts
        .try_reserve(1)
        .map_err(|_| RegExpCompileError::OutOfMemory)?;
    }
    let pc = self.insts.len();
    self.insts.push(inst);
    Ok(pc)
  }

  fn compile_disjunction(&mut self, disj: &Disjunction) -> Result<(), RegExpCompileError> {
    if disj.alts.is_empty() {
      return Ok(());
    }
    if disj.alts.len() == 1 {
      return self.compile_alternative(&disj.alts[0]);
    }

    let mut end_jumps: Vec<usize> = Vec::new();
    for (i, alt) in disj.alts.iter().enumerate() {
      if i + 1 == disj.alts.len() {
        self.compile_alternative(alt)?;
        break;
      }

      // Split to this alternative (fallthrough) or the next one (patched).
      let split_pc = self.emit(Inst::Split(self.insts.len() + 1, 0))?;
      self.compile_alternative(alt)?;
      let jmp_pc = self.emit(Inst::Jump(0))?;
      vec_try_push(&mut end_jumps, jmp_pc)?;
      // Patch the split's second branch to the start of the next alternative.
      let next_pc = self.insts.len();
      let Inst::Split(_, ref mut b) = self.insts[split_pc] else {
        unreachable!();
      };
      *b = next_pc;
    }

    let end = self.insts.len();
    for (i, pc) in end_jumps.into_iter().enumerate() {
      if i != 0 && i % REGEXP_COMPILE_TICK_EVERY == 0 {
        self.bump(REGEXP_COMPILE_TICK_EVERY)?;
      }
      let Inst::Jump(ref mut target) = self.insts[pc] else {
        unreachable!();
      };
      *target = end;
    }
    Ok(())
  }

  fn compile_alternative(&mut self, alt: &Alternative) -> Result<(), RegExpCompileError> {
    for term in alt.terms.iter() {
      self.compile_term(term)?;
    }
    Ok(())
  }

  fn compile_term(&mut self, term: &Term) -> Result<(), RegExpCompileError> {
    match term {
      Term::Assertion(a) => self.compile_assertion(a),
      Term::Atom(atom, quant) => {
        if let Some(q) = quant {
          self.compile_quantified(atom, *q)
        } else {
          self.compile_atom(atom)
        }
      }
    }
  }

  fn compile_assertion(&mut self, a: &Assertion) -> Result<(), RegExpCompileError> {
    match a {
      Assertion::Start => {
        self.emit(Inst::AssertStart)?;
      }
      Assertion::End => {
        self.emit(Inst::AssertEnd)?;
      }
      Assertion::WordBoundary => {
        self.emit(Inst::WordBoundary { negated: false })?;
      }
      Assertion::NotWordBoundary => {
        self.emit(Inst::WordBoundary { negated: true })?;
      }
      Assertion::LookAhead { negative, disj } => {
        // Compile lookahead into a nested program that shares the outer capture slot numbering.
        let tick = &mut *self.tick;
        let mut nested = ProgramBuilder::new(self.capture_count, tick);
        nested.compile_disjunction(disj)?;
        nested.emit(Inst::Match)?;
        let nested_prog = nested.finish();
        self.emit(Inst::LookAhead {
          program: Box::new(nested_prog),
          negative: *negative,
        })?;
      }
    }
    Ok(())
  }

  fn compile_quantified(&mut self, atom: &Atom, q: Quantifier) -> Result<(), RegExpCompileError> {
    let id = self.repeat_count;
    self.repeat_count = self.repeat_count.saturating_add(1);
    let start_pc = self.emit(Inst::RepeatStart {
      id,
      min: q.min,
      max: q.max,
      greedy: q.greedy,
      exit: 0, // patch
    })?;
    self.compile_atom(atom)?;
    self.emit(Inst::RepeatEnd { start: start_pc })?;
    let exit = self.insts.len();
    let Inst::RepeatStart { exit: ref mut e, .. } = self.insts[start_pc] else {
      unreachable!();
    };
    *e = exit;
    Ok(())
  }

  fn compile_atom(&mut self, atom: &Atom) -> Result<(), RegExpCompileError> {
    match atom {
      Atom::Literal(u) => {
        self.emit(Inst::Char(*u))?;
      }
      Atom::Any => {
        self.emit(Inst::Any)?;
      }
      Atom::Class(cls) => {
        self.emit(Inst::Class(cls.try_clone()?))?;
      }
      Atom::BackRef(n) => {
        self.emit(Inst::BackRef(*n))?;
      }
      Atom::Group { capture, disj } => {
        if let Some(idx) = capture {
          let start_slot = (*idx as usize).saturating_mul(2);
          self.emit(Inst::Save(start_slot))?;
          self.compile_disjunction(disj)?;
          self.emit(Inst::Save(start_slot + 1))?;
        } else {
          self.compile_disjunction(disj)?;
        }
      }
    }
    Ok(())
  }
}
