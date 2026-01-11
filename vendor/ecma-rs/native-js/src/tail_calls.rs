//! Tail-call validation utilities for native-js.
//!
//! ## Why this exists
//! Our initial stack scanning strategy walks frames via **frame pointers** and uses the **return
//! address** from each frame to look up the corresponding LLVM StackMap record.
//!
//! If LLVM performs tail call optimization (TCO), it can replace `call + ret` with a `jmp`, reusing
//! the caller's stack frame. That breaks the return-address ↔ stackmap mapping and can cause GC root
//! scanning to misinterpret frames.
//!
//! We therefore enforce:
//! - function attr: `"disable-tail-calls"="true"` on TS-generated functions
//! - callsite marker: `notail` on generated calls
//! - and we validate optimized machine code in regression tests via `llvm-objdump`.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};

#[derive(Debug, Clone)]
pub struct DisassembledInstruction {
  pub mnemonic: String,
  pub relocation_symbol: Option<String>,
  pub text: String,
}

/// Parse `llvm-objdump -dr` output into a per-function instruction listing.
///
/// This is intentionally lightweight and only extracts what we need for validation:
/// - function name (from `<name>:` headers)
/// - instruction mnemonic (from the first tab-separated token)
/// - relocation symbol attached to the previous instruction (from `R_* <sym>` lines)
pub fn parse_objdump_disassembly(disasm: &str) -> HashMap<String, Vec<DisassembledInstruction>> {
  let mut out: HashMap<String, Vec<DisassembledInstruction>> = HashMap::new();

  let mut current_fn: Option<String> = None;
  let mut last_instr_idx: Option<usize> = None;

  for line in disasm.lines() {
    if let Some(fn_name) = parse_function_header(line) {
      current_fn = Some(fn_name.clone());
      out.entry(fn_name).or_default();
      last_instr_idx = None;
      continue;
    }

    let Some(fn_name) = current_fn.as_ref() else {
      continue;
    };

    if let Some(mnemonic) = parse_instruction_mnemonic(line) {
      let instrs = out.get_mut(fn_name).expect("function exists");
      instrs.push(DisassembledInstruction {
        mnemonic,
        relocation_symbol: None,
        text: line.to_owned(),
      });
      last_instr_idx = Some(instrs.len() - 1);
      continue;
    }

    if let Some(symbol) = parse_relocation_symbol(line) {
      if let Some(idx) = last_instr_idx {
        if let Some(instrs) = out.get_mut(fn_name) {
          if let Some(instr) = instrs.get_mut(idx) {
            instr.relocation_symbol = Some(symbol);
          }
        }
      }
    }
  }

  out
}

pub fn assert_objdump_has_section(section_headers: &str, section_name: &str) -> Result<()> {
  if section_headers.contains(section_name) {
    return Ok(());
  }
  bail!(
    "expected object to contain section {section_name:?}, but it was not found in:\n{section_headers}"
  );
}

/// Validate that none of the provided functions contains a tailcall-style jump.
///
/// Currently we flag a *direct* tailcall as:
/// - an instruction whose mnemonic starts with `jmp`
/// - and that instruction has a relocation to some symbol (i.e. it's jumping to a function symbol,
///   not an intra-function basic-block label).
pub fn assert_no_tail_call_jumps(objdump_dr: &str, functions: &[&str]) -> Result<()> {
  let parsed = parse_objdump_disassembly(objdump_dr);

  for &func in functions {
    let instrs = parsed
      .get(func)
      .ok_or_else(|| anyhow!("function {func:?} not found in objdump output"))?;

    for instr in instrs {
      if instr.mnemonic.starts_with("jmp") && instr.relocation_symbol.is_some() {
        bail!("tail call jump detected in function {func:?}: {text}", text = instr.text);
      }
    }
  }

  Ok(())
}

/// Convenience wrapper that checks every function whose symbol name starts with `ts_`.
pub fn assert_no_tail_call_jumps_in_ts_functions(objdump_dr: &str) -> Result<()> {
  let parsed = parse_objdump_disassembly(objdump_dr);
  let mut ts_functions: Vec<&str> = parsed
    .keys()
    .filter_map(|name| name.strip_prefix("ts_").map(|_| name.as_str()))
    .collect();
  ts_functions.sort_unstable();
  assert_no_tail_call_jumps(objdump_dr, &ts_functions)
}

pub fn assert_function_has_ret(objdump_dr: &str, function: &str) -> Result<()> {
  let parsed = parse_objdump_disassembly(objdump_dr);
  let instrs = parsed
    .get(function)
    .ok_or_else(|| anyhow!("function {function:?} not found in objdump output"))?;

  if instrs.iter().any(|i| i.mnemonic.starts_with("ret")) {
    return Ok(());
  }

  bail!(
    "expected function {function:?} to contain a return instruction; got:\n{}",
    pretty_print_instructions(instrs)
  );
}

pub fn assert_function_calls_symbol(objdump_dr: &str, caller: &str, callee: &str) -> Result<()> {
  let parsed = parse_objdump_disassembly(objdump_dr);
  let instrs = parsed
    .get(caller)
    .ok_or_else(|| anyhow!("function {caller:?} not found in objdump output"))?;

  if instrs.iter().any(|i| {
    i.mnemonic.starts_with("call") && i.relocation_symbol.as_deref() == Some(callee)
  }) {
    return Ok(());
  }

  bail!(
    "expected function {caller:?} to contain a call to {callee:?}; got:\n{}",
    pretty_print_instructions(instrs)
  );
}

pub fn assert_function_does_not_jump_to_symbol(
  objdump_dr: &str,
  caller: &str,
  callee: &str,
) -> Result<()> {
  let parsed = parse_objdump_disassembly(objdump_dr);
  let instrs = parsed
    .get(caller)
    .ok_or_else(|| anyhow!("function {caller:?} not found in objdump output"))?;

  if instrs.iter().any(|i| {
    i.mnemonic.starts_with("jmp") && i.relocation_symbol.as_deref() == Some(callee)
  }) {
    bail!(
      "expected function {caller:?} not to tailcall-jump to {callee:?}; got:\n{}",
      pretty_print_instructions(instrs)
    );
  }

  Ok(())
}

fn pretty_print_instructions(instrs: &[DisassembledInstruction]) -> String {
  let mut out = String::new();
  for instr in instrs {
    out.push_str(&instr.text);
    out.push('\n');
  }
  out
}

fn parse_function_header(line: &str) -> Option<String> {
  // Example:
  //   0000000000000010 <caller>:
  let line = line.trim();
  let start = line.find('<')?;
  let end_rel = line[start + 1..].find('>')?;
  let end = start + 1 + end_rel;
  if !line[end + 1..].starts_with(':') {
    return None;
  }
  Some(line[start + 1..end].to_owned())
}

fn parse_instruction_mnemonic(line: &str) -> Option<String> {
  // Instruction lines look like:
  //   10: e8 00 00 00 00               	callq	0x16 <caller+0x6>
  //
  // Relocation lines look like:
  //   0000000000000012:  R_X86_64_PLT32	callee-0x4
  //
  // We only want the former.
  let trimmed = line.trim_start();
  let (_addr, rest) = trimmed.split_once(':')?;
  if rest.trim_start().starts_with("R_") {
    return None;
  }
  let (_before_tab, after_tab) = line.split_once('\t')?;
  let mnemonic = after_tab.split_whitespace().next()?.to_owned();
  Some(mnemonic)
}

fn parse_relocation_symbol(line: &str) -> Option<String> {
  let trimmed = line.trim_start();
  let (_addr, rest) = trimmed.split_once(':')?;
  let rest = rest.trim_start();
  if !rest.starts_with("R_") {
    return None;
  }
  let mut parts = rest.split_whitespace();
  let _reloc_type = parts.next()?;
  let sym = parts.next()?;
  Some(sym.split('-').next().unwrap_or(sym).to_owned())
}

