use crate::api::ConsoleMessageLevel;
use crate::error::Error;
use std::borrow::Cow;
use std::sync::Arc;
use vm_js::{Heap, PropertyKey, StackFrame, Value, VmError};

const MAX_THROWN_STRING_CODE_UNITS: usize = 4096;
const MAX_STACK_TRACE_FRAMES: usize = 32;
const MAX_STACK_FRAME_TEXT_BYTES: usize = 256;
const MAX_STACK_TRACE_BYTES: usize = 16 * 1024;
const MAX_STACK_PROPERTY_CODE_UNITS: usize = MAX_STACK_TRACE_BYTES * 2;
const MAX_THROWN_OBJECT_PROTOTYPE_CHAIN: usize = 16;
const MAX_SYNTAX_DIAGNOSTICS: usize = 8;
const MAX_SYNTAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
const MAX_SYNTAX_ERROR_BYTES: usize = 8 * 1024;
const MAX_CONSOLE_MESSAGE_BYTES: usize = 8 * 1024;

fn format_number_fallback(n: f64) -> String {
  if n.is_nan() {
    return "NaN".to_string();
  }
  if n == f64::INFINITY {
    return "Infinity".to_string();
  }
  if n == f64::NEG_INFINITY {
    return "-Infinity".to_string();
  }
  // Match ECMAScript: `(-0).toString()` is `"0"`.
  if n == 0.0 {
    return "0".to_string();
  }
  format!("{n}")
}

fn truncate_utf8(s: &str, max_bytes: usize) -> Cow<'_, str> {
  if s.len() <= max_bytes {
    return Cow::Borrowed(s);
  }
  let mut end = max_bytes;
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  let mut out = String::new();
  // Keep host allocations bounded even for hostile inputs.
  out.push_str(&s[..end]);
  out.push_str("...");
  Cow::Owned(out)
}

fn format_stack_frame(frame: &StackFrame) -> String {
  let source = truncate_utf8(frame.source.as_ref(), MAX_STACK_FRAME_TEXT_BYTES);
  match frame.function.as_deref() {
    Some(function) => {
      let function = truncate_utf8(function, MAX_STACK_FRAME_TEXT_BYTES);
      format!(
        "at {function} ({source}:{line}:{col})",
        function = function,
        source = source,
        line = frame.line,
        col = frame.col
      )
    }
    None => format!(
      "at {source}:{line}:{col}",
      source = source,
      line = frame.line,
      col = frame.col
    ),
  }
}

pub(crate) fn format_stack_trace_limited(frames: &[StackFrame]) -> String {
  if frames.is_empty() {
    return String::new();
  }

  let mut out = String::new();
  let mut truncated = false;

  for (idx, frame) in frames.iter().take(MAX_STACK_TRACE_FRAMES).enumerate() {
    let line = format_stack_frame(frame);
    let extra = if idx == 0 { 0 } else { 1 };
    if out.len() + extra + line.len() > MAX_STACK_TRACE_BYTES {
      truncated = true;
      break;
    }
    if idx > 0 {
      out.push('\n');
    }
    out.push_str(&line);
  }

  if frames.len() > MAX_STACK_TRACE_FRAMES {
    truncated = true;
  }

  if truncated {
    const TRUNCATED: &str = "\n...";
    if out.len() + TRUNCATED.len() <= MAX_STACK_TRACE_BYTES {
      out.push_str(TRUNCATED);
    }
  }

  out
}

pub(crate) fn vm_error_is_js_exception(err: &VmError) -> bool {
  matches!(
    err,
    VmError::Throw(_)
      | VmError::ThrowWithStack { .. }
      | VmError::TypeError(_)
      | VmError::NotCallable
      | VmError::NotConstructable
      | VmError::Syntax(_)
  )
}

fn format_syntax_error_limited(diags: &[diagnostics::Diagnostic]) -> String {
  let mut out = String::from("syntax error");
  let mut truncated = false;
  let mut wrote_any = false;

  for diag in diags.iter().take(MAX_SYNTAX_DIAGNOSTICS) {
    let message = diag.message.trim();
    if message.is_empty() {
      continue;
    }

    let message = truncate_utf8(message, MAX_SYNTAX_DIAGNOSTIC_MESSAGE_BYTES);
    let (sep, extra) = if wrote_any {
      ('\n', 1)
    } else {
      (':', 2) // ": "
    };

    if out.len() + extra + message.len() > MAX_SYNTAX_ERROR_BYTES {
      truncated = true;
      break;
    }
    out.push(sep);
    if !wrote_any {
      out.push(' ');
      wrote_any = true;
    }
    out.push_str(&message);
  }

  if diags.len() > MAX_SYNTAX_DIAGNOSTICS {
    truncated = true;
  }

  if truncated {
    const TRUNCATED: &str = "\n...";
    if out.len() + TRUNCATED.len() <= MAX_SYNTAX_ERROR_BYTES {
      out.push_str(TRUNCATED);
    }
  }

  out
}

fn format_thrown_value(heap: &mut Heap, value: Value) -> Option<String> {
  let obj = match value {
    Value::Undefined => return Some("undefined".to_string()),
    Value::Null => return Some("null".to_string()),
    Value::Bool(b) => return Some(b.to_string()),
    Value::Number(n) => {
      if let Ok(s) = heap.to_string(Value::Number(n)) {
        if let Ok(js) = heap.get_string(s) {
          if js.len_code_units() <= MAX_THROWN_STRING_CODE_UNITS {
            return Some(js.to_utf8_lossy());
          }
        }
      }
      return Some(format_number_fallback(n));
    }
    Value::BigInt(b) => {
      let mut out = heap
        .get_bigint(b)
        .ok()
        .and_then(|bi| bi.to_string_radix_with_tick(10, &mut || Ok(())).ok())
        .unwrap_or_else(|| "[bigint]".to_string());
      // Match common JS console output (`1n`) and disambiguate from Numbers.
      out.push('n');
      return Some(out);
    }
    Value::Symbol(sym) => {
      let desc_s = heap.get_symbol_description(sym).ok().flatten();
      if let Some(desc_s) = desc_s {
        return Some(match heap.get_string(desc_s) {
          Ok(js) => {
            if js.len_code_units() <= MAX_THROWN_STRING_CODE_UNITS {
              let desc = js.to_utf8_lossy();
              format!("Symbol({desc})")
            } else {
              "Symbol([exception string exceeded limit])".to_string()
            }
          }
          Err(_) => "[symbol]".to_string(),
        });
      }
      return Some("Symbol()".to_string());
    }
    Value::String(s) => {
      return Some(match heap.get_string(s) {
        Ok(js) => {
          if js.len_code_units() <= MAX_THROWN_STRING_CODE_UNITS {
            js.to_utf8_lossy()
          } else {
            "[exception string exceeded limit]".to_string()
          }
        }
        Err(_) => "[string]".to_string(),
      });
    }
    Value::Object(obj) => obj,
  };

  let is_promise = heap.is_promise_object(obj);
  let is_function = heap.is_callable(Value::Object(obj)).unwrap_or(false);
  let fallback_marker = if is_promise {
    "[promise]"
  } else if is_function {
    "[function]"
  } else {
    "[object]"
  };

  let mut scope = heap.scope();
  if scope.push_root(Value::Object(obj)).is_err() {
    // If we cannot grow the scope root stack, avoid any further heap allocations that might trigger
    // GC and invalidate the unrooted object handle.
    return Some(fallback_marker.to_string());
  }

  let mut get_prop_str = |name: &str| -> Option<String> {
    let key_s = scope.alloc_string(name).ok()?;
    scope.push_root(Value::String(key_s)).ok()?;
    let key = PropertyKey::from_string(key_s);

    let mut current = obj;
    for _ in 0..MAX_THROWN_OBJECT_PROTOTYPE_CHAIN {
      match scope
        .heap()
        .object_get_own_data_property_value(current, &key)
      {
        Ok(Some(Value::String(s))) => {
          let js = scope.heap().get_string(s).ok()?;
          if js.len_code_units() > MAX_THROWN_STRING_CODE_UNITS {
            return Some("[exception string exceeded limit]".to_string());
          }
          return Some(js.to_utf8_lossy());
        }
        Ok(Some(_)) => return None,
        Ok(None) => {}
        Err(_) => return None,
      }

      match scope.object_get_prototype(current) {
        Ok(Some(proto)) => current = proto,
        _ => return None,
      }
    }
    None
  };

  let name = get_prop_str("name");
  let message = get_prop_str("message");
  if is_function {
    // For callable objects, `name` is usually the function's identifier, which can be confusing in
    // host-side exception output ("f" looks like an Error name). Use an explicit marker.
    return Some(match name {
      Some(name) if !name.is_empty() => format!("[function {name}]"),
      _ => "[function]".to_string(),
    });
  }
  match (name, message) {
    (Some(name), Some(message)) if !message.is_empty() => Some(format!("{name}: {message}")),
    (Some(name), _) if !name.is_empty() => Some(name),
    (_, Some(message)) if !message.is_empty() => Some(message),
    _ => Some(fallback_marker.to_string()),
  }
}

fn get_thrown_stack_property(heap: &mut Heap, value: Value) -> Option<String> {
  let obj = match value {
    Value::Object(obj) => obj,
    _ => return None,
  };

  let mut scope = heap.scope();
  if scope.push_root(Value::Object(obj)).is_err() {
    return None;
  }

  let key_s = scope.alloc_string("stack").ok()?;
  scope.push_root(Value::String(key_s)).ok()?;
  let key = PropertyKey::from_string(key_s);

  let mut current = obj;
  for _ in 0..MAX_THROWN_OBJECT_PROTOTYPE_CHAIN {
    match scope
      .heap()
      .object_get_own_data_property_value(current, &key)
    {
      Ok(Some(Value::String(s))) => {
        let js = scope.heap().get_string(s).ok()?;
        if js.len_code_units() > MAX_STACK_PROPERTY_CODE_UNITS {
          return Some("[stack trace exceeded limit]".to_string());
        }
        let mut stack = js.to_utf8_lossy();
        if stack.len() > MAX_STACK_TRACE_BYTES {
          let mut end = MAX_STACK_TRACE_BYTES;
          while end > 0 && !stack.is_char_boundary(end) {
            end -= 1;
          }
          stack.truncate(end);
          stack.push_str("...");
        }
        return Some(stack);
      }
      Ok(Some(_)) => return None,
      Ok(None) => {}
      Err(_) => return None,
    }

    match scope.object_get_prototype(current) {
      Ok(Some(proto)) => current = proto,
      _ => return None,
    }
  }

  None
}

/// Convert a `vm-js` [`VmError`] into a `fastrender` [`Error`], attempting to preserve thrown string
/// values and captured stack traces while keeping host allocations bounded.
pub(crate) fn vm_error_to_error(heap: &mut Heap, err: VmError) -> Error {
  Error::Other(vm_error_to_string(heap, err))
}

/// Best-effort string formatting for `vm-js` errors.
///
/// If the error is a JS throw with a captured stack trace (`VmError::ThrowWithStack`), the returned
/// string includes a newline-delimited stack trace.
pub(crate) fn vm_error_to_string(heap: &mut Heap, err: VmError) -> String {
  if let VmError::Termination(term) = &err {
    let mut msg = err.to_string();
    let stack = format_stack_trace_limited(&term.stack);
    if !stack.is_empty() {
      msg.push('\n');
      msg.push_str(&stack);
    }
    return msg;
  }

  if let Some(value) = err.thrown_value() {
    let mut msg =
      format_thrown_value(heap, value).unwrap_or_else(|| "uncaught exception".to_string());

    let stack = err
      .thrown_stack()
      .map(format_stack_trace_limited)
      .filter(|s| !s.is_empty())
      .or_else(|| get_thrown_stack_property(heap, value).filter(|s| !s.is_empty()));

    if let Some(stack) = stack {
      msg.push('\n');
      msg.push_str(&stack);
    }

    return msg;
  }

  match err {
    VmError::Syntax(diags) => format_syntax_error_limited(&diags),
    other => other.to_string(),
  }
}

/// Split a `vm-js` error into a primary message and an optional stack trace for diagnostics.
///
/// Unlike [`vm_error_to_string`], this preserves the stack as a separate field so callers can record
/// it in structured diagnostics.
pub(crate) fn vm_error_to_message_and_stack(
  heap: &mut Heap,
  err: VmError,
) -> (String, Option<String>) {
  if let VmError::Termination(term) = &err {
    let msg = err.to_string();
    let stack = format_stack_trace_limited(&term.stack);
    return (msg, (!stack.is_empty()).then_some(stack));
  }

  if let Some(value) = err.thrown_value() {
    let msg = format_thrown_value(heap, value).unwrap_or_else(|| "uncaught exception".to_string());
    let stack = err
      .thrown_stack()
      .map(format_stack_trace_limited)
      .filter(|s| !s.is_empty())
      .or_else(|| get_thrown_stack_property(heap, value).filter(|s| !s.is_empty()));
    return (msg, stack);
  }

  match err {
    VmError::Syntax(diags) => (format_syntax_error_limited(&diags), None),
    other => (other.to_string(), None),
  }
}

fn truncate_in_place_to_char_boundary(out: &mut String, max_bytes: usize) {
  if out.len() <= max_bytes {
    return;
  }
  let mut end = max_bytes;
  while end > 0 && !out.is_char_boundary(end) {
    end -= 1;
  }
  out.truncate(end);
}

fn ensure_trailing_ellipsis(out: &mut String, max_total_bytes: usize) {
  if max_total_bytes == 0 {
    out.clear();
    return;
  }
  if max_total_bytes <= 3 {
    truncate_in_place_to_char_boundary(out, max_total_bytes);
    return;
  }
  let max_prefix = max_total_bytes - 3;
  truncate_in_place_to_char_boundary(out, max_prefix);
  out.push_str("...");
}

fn push_truncated(out: &mut String, s: &str, max_total_bytes: usize) -> bool {
  if s.is_empty() {
    return false;
  }

  if out.len() >= max_total_bytes {
    ensure_trailing_ellipsis(out, max_total_bytes);
    return true;
  }

  let remaining = max_total_bytes - out.len();
  if s.len() <= remaining {
    out.push_str(s);
    return false;
  }

  if max_total_bytes <= 3 {
    let mut end = remaining;
    while end > 0 && !s.is_char_boundary(end) {
      end -= 1;
    }
    out.push_str(&s[..end]);
    return true;
  }

  // Preserve space for "...", trimming existing content if needed.
  let max_prefix = max_total_bytes - 3;
  truncate_in_place_to_char_boundary(out, max_prefix);
  let remaining_prefix = max_prefix - out.len();
  let mut end = remaining_prefix;
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  out.push_str(&s[..end]);
  out.push_str("...");
  true
}

fn push_utf16_lossy_truncated(out: &mut String, units: &[u16], max_total_bytes: usize) -> bool {
  if units.is_empty() {
    return false;
  }

  if out.len() >= max_total_bytes {
    ensure_trailing_ellipsis(out, max_total_bytes);
    return true;
  }

  for unit in std::char::decode_utf16(units.iter().copied()) {
    let ch = unit.unwrap_or(char::REPLACEMENT_CHARACTER);
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    if out.len() + encoded.len() > max_total_bytes {
      ensure_trailing_ellipsis(out, max_total_bytes);
      return true;
    }
    out.push_str(encoded);
  }

  false
}

fn decode_utf16_scalar(units: &[u16], idx: usize) -> (char, usize) {
  let u = units[idx];
  if (0xD800..=0xDBFF).contains(&u) {
    if let Some(&u2) = units.get(idx + 1) {
      if (0xDC00..=0xDFFF).contains(&u2) {
        let high = (u as u32) - 0xD800;
        let low = (u2 as u32) - 0xDC00;
        let cp = 0x10000 + (high << 10) + low;
        if let Some(ch) = char::from_u32(cp) {
          return (ch, 2);
        }
      }
    }
    return ('\u{FFFD}', 1);
  }
  if (0xDC00..=0xDFFF).contains(&u) {
    return ('\u{FFFD}', 1);
  }
  (char::from_u32(u as u32).unwrap_or('\u{FFFD}'), 1)
}

fn console_number_from_primitive_limited(heap: &mut Heap, value: Value) -> f64 {
  match value {
    Value::Object(_) | Value::Symbol(_) => f64::NAN,
    Value::BigInt(b) => match heap.get_bigint(b) {
      Ok(bi) => bi
        .to_string_radix_with_tick(10, &mut || Ok(()))
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(f64::NAN),
      Err(_) => f64::NAN,
    },
    Value::String(s) => {
      let len = match heap.get_string(s) {
        Ok(js) => js.len_code_units(),
        Err(_) => return f64::NAN,
      };
      if len > MAX_THROWN_STRING_CODE_UNITS {
        return f64::NAN;
      }
      heap.to_number(Value::String(s)).unwrap_or(f64::NAN)
    }
    other => heap.to_number(other).unwrap_or(f64::NAN),
  }
}

fn format_console_number_substitution(heap: &mut Heap, value: Value, spec: char) -> String {
  if let Value::BigInt(b) = value {
    if let Ok(bi) = heap.get_bigint(b) {
      if let Ok(s) = bi.to_string_radix_with_tick(10, &mut || Ok(())) {
        return s;
      }
    }
    return "NaN".to_string();
  }
  let n = console_number_from_primitive_limited(heap, value);
  match spec {
    'i' => format_number_fallback(n.trunc()),
    'd' | 'f' => format_number_fallback(n),
    _ => "NaN".to_string(),
  }
}

/// Deterministically format console arguments without invoking user-defined `toString` hooks.
///
/// This is intended for renderer diagnostics, so it is intentionally bounded and lossy for complex
/// objects.
pub(crate) fn format_console_arguments_limited(heap: &mut Heap, args: &[Value]) -> String {
  if args.is_empty() {
    return String::new();
  }

  let mut out = String::new();

  let Value::String(format_s) = args[0] else {
    for (idx, value) in args.iter().copied().enumerate() {
      if idx > 0 && push_truncated(&mut out, " ", MAX_CONSOLE_MESSAGE_BYTES) {
        break;
      }
      let formatted = format_thrown_value(heap, value).unwrap_or_else(|| "[exception]".to_string());
      if push_truncated(&mut out, &formatted, MAX_CONSOLE_MESSAGE_BYTES) {
        break;
      }
    }
    return out;
  };

  let fmt_len = match heap.get_string(format_s) {
    Ok(js) => js.len_code_units(),
    Err(_) => {
      for (idx, value) in args.iter().copied().enumerate() {
        if idx > 0 && push_truncated(&mut out, " ", MAX_CONSOLE_MESSAGE_BYTES) {
          break;
        }
        let formatted = format_thrown_value(heap, value).unwrap_or_else(|| "[exception]".to_string());
        if push_truncated(&mut out, &formatted, MAX_CONSOLE_MESSAGE_BYTES) {
          break;
        }
      }
      return out;
    }
  };

  let mut fmt_idx = 0usize;
  let mut arg_idx = 1usize;
  let mut truncated = false;

  while fmt_idx < fmt_len && !truncated {
    let mut specifier: Option<u16> = None;
    let mut emit_literal: Option<&'static str> = None;
    let mut advance = 0usize;

    {
      let Ok(js) = heap.get_string(format_s) else {
        break;
      };
      let units = js.as_code_units();

      let mut next_percent = fmt_idx;
      while next_percent < fmt_len && units[next_percent] != b'%' as u16 {
        next_percent += 1;
      }

      if next_percent > fmt_idx {
        truncated |= push_utf16_lossy_truncated(
          &mut out,
          &units[fmt_idx..next_percent],
          MAX_CONSOLE_MESSAGE_BYTES,
        );
      }

      fmt_idx = next_percent;
      if truncated || fmt_idx >= fmt_len {
        break;
      }

      debug_assert_eq!(units[fmt_idx], b'%' as u16);
      if fmt_idx + 1 >= fmt_len {
        // Trailing `%` => literal `%`.
        truncated |= push_truncated(&mut out, "%", MAX_CONSOLE_MESSAGE_BYTES);
        fmt_idx += 1;
        break;
      }

      let next_unit = units[fmt_idx + 1];

      // `%%` => literal `%`.
      if next_unit == b'%' as u16 {
        truncated |= push_truncated(&mut out, "%", MAX_CONSOLE_MESSAGE_BYTES);
        fmt_idx += 2;
        continue;
      }

      let next_char = (next_unit <= 0x7F).then_some(next_unit as u8 as char);
      match next_char {
        Some('s' | 'd' | 'i' | 'f' | 'o' | 'O' | 'c') => {
          if arg_idx >= args.len() {
            emit_literal = Some(match next_char.unwrap() {
              's' => "%s",
              'd' => "%d",
              'i' => "%i",
              'f' => "%f",
              'o' => "%o",
              'O' => "%O",
              'c' => "%c",
              _ => unreachable!(),
            });
            advance = 2;
          } else {
            specifier = Some(next_unit);
            advance = 2;
          }
        }
        _ => {
          // Unknown specifier: emit verbatim and do not consume an argument.
          truncated |= push_truncated(&mut out, "%", MAX_CONSOLE_MESSAGE_BYTES);
          // Emit the specifier char, preserving surrogate pairs when possible.
          let mut spec_units_len = 1usize;
          if (0xD800..=0xDBFF).contains(&next_unit) {
            if fmt_idx + 2 < fmt_len {
              let low = units[fmt_idx + 2];
              if (0xDC00..=0xDFFF).contains(&low) {
                spec_units_len = 2;
              }
            }
          }
          truncated |= push_utf16_lossy_truncated(
            &mut out,
            &units[fmt_idx + 1..fmt_idx + 1 + spec_units_len],
            MAX_CONSOLE_MESSAGE_BYTES,
          );
          fmt_idx += 1 + spec_units_len;
          continue;
        }
      }
    }

    if truncated {
      break;
    }

    if let Some(literal) = emit_literal {
      truncated |= push_truncated(&mut out, literal, MAX_CONSOLE_MESSAGE_BYTES);
      fmt_idx += advance;
      continue;
    }

    let Some(spec_unit) = specifier else {
      break;
    };
    fmt_idx += advance;

    // Recognized specifier with an argument available.
    let value = args[arg_idx];
    arg_idx += 1;

    match spec_unit as u8 as char {
      'c' => {
        // Consume the styling argument but emit nothing.
      }
      's' => match value {
        Value::String(s) => match heap.get_string(s) {
          Ok(js) => {
            truncated |=
              push_utf16_lossy_truncated(&mut out, js.as_code_units(), MAX_CONSOLE_MESSAGE_BYTES);
          }
          Err(_) => truncated |= push_truncated(&mut out, "[string]", MAX_CONSOLE_MESSAGE_BYTES),
        },
        other => {
          let formatted =
            format_thrown_value(heap, other).unwrap_or_else(|| "[exception]".to_string());
          truncated |= push_truncated(&mut out, &formatted, MAX_CONSOLE_MESSAGE_BYTES);
        }
      },
      'o' | 'O' => {
        let formatted = format_thrown_value(heap, value).unwrap_or_else(|| "[exception]".to_string());
        truncated |= push_truncated(&mut out, &formatted, MAX_CONSOLE_MESSAGE_BYTES);
      }
      'd' | 'f' | 'i' => {
        let formatted = format_console_number_substitution(heap, value, spec_unit as u8 as char);
        truncated |= push_truncated(&mut out, &formatted, MAX_CONSOLE_MESSAGE_BYTES);
      }
      _ => unreachable!(),
    }
  }

  // Append any unused arguments separated by spaces.
  if !truncated {
    for value in args.iter().copied().skip(arg_idx) {
      if !out.is_empty() && push_truncated(&mut out, " ", MAX_CONSOLE_MESSAGE_BYTES) {
        break;
      }
      let formatted = format_thrown_value(heap, value).unwrap_or_else(|| "[exception]".to_string());
      if push_truncated(&mut out, &formatted, MAX_CONSOLE_MESSAGE_BYTES) {
        break;
      }
    }
  }

  out
}

pub(crate) fn emit_console_message_to_stderr(level: ConsoleMessageLevel, message: &str) {
  use std::io::Write;

  let mut stderr = std::io::stderr().lock();
  let _ = write!(stderr, "[{}] ", level.as_str());

  // Keep each `console.*` call on a single stderr line even when arguments contain newlines.
  for (idx, part) in message.split(|c| c == '\n' || c == '\r').enumerate() {
    if idx > 0 {
      let _ = write!(stderr, " ");
    }
    let _ = write!(stderr, "{part}");
  }
  let _ = writeln!(stderr);
}

pub(crate) fn stderr_console_sink() -> crate::js::ConsoleSink {
  Arc::new(|level, heap, args| {
    let message = format_console_arguments_limited(heap, args);
    emit_console_message_to_stderr(level, &message);
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use std::sync::Arc;
  use vm_js::{HeapLimits, PropertyDescriptor, PropertyKind};

  #[test]
  fn thrown_long_string_is_replaced_with_marker_and_includes_stack_trace() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();
    let units = vec![0x0061u16; MAX_THROWN_STRING_CODE_UNITS + 1];
    let long_s = scope
      .alloc_string_from_code_units(&units)
      .expect("alloc long thrown string");
    scope
      .push_root(Value::String(long_s))
      .expect("root long thrown string");

    let err = VmError::ThrowWithStack {
      value: Value::String(long_s),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("[exception string exceeded limit]"),
      "expected over-limit thrown string to be replaced with marker, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_object_long_message_is_replaced_with_marker() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let obj = scope.alloc_object().expect("alloc thrown object");
    scope
      .push_root(Value::Object(obj))
      .expect("root thrown object");

    let name_s = scope.alloc_string("Error").expect("alloc name");
    scope.push_root(Value::String(name_s)).expect("root name");

    let units = vec![0x0061u16; MAX_THROWN_STRING_CODE_UNITS + 1];
    let long_message_s = scope
      .alloc_string_from_code_units(&units)
      .expect("alloc long message");
    scope
      .push_root(Value::String(long_message_s))
      .expect("root long message");

    let name_key_s = scope.alloc_string("name").expect("alloc key");
    scope
      .push_root(Value::String(name_key_s))
      .expect("root key");
    let name_key = PropertyKey::from_string(name_key_s);
    scope
      .define_property(
        obj,
        name_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::String(name_s),
            writable: true,
          },
        },
      )
      .expect("define name");

    let msg_key_s = scope.alloc_string("message").expect("alloc key");
    scope.push_root(Value::String(msg_key_s)).expect("root key");
    let msg_key = PropertyKey::from_string(msg_key_s);
    scope
      .define_property(
        obj,
        msg_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::String(long_message_s),
            writable: true,
          },
        },
      )
      .expect("define message");

    let err = VmError::ThrowWithStack {
      value: Value::Object(obj),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("Error: [exception string exceeded limit]"),
      "expected over-limit message property to be replaced with marker, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_object_name_on_prototype_is_used() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let proto = scope.alloc_object().expect("alloc prototype");
    scope
      .push_root(Value::Object(proto))
      .expect("root prototype");
    let obj = scope.alloc_object().expect("alloc thrown object");
    scope
      .push_root(Value::Object(obj))
      .expect("root thrown object");
    scope
      .object_set_prototype(obj, Some(proto))
      .expect("set prototype");

    let name_s = scope.alloc_string("Error").expect("alloc name");
    scope.push_root(Value::String(name_s)).expect("root name");
    let name_key_s = scope.alloc_string("name").expect("alloc key");
    scope
      .push_root(Value::String(name_key_s))
      .expect("root key");
    let name_key = PropertyKey::from_string(name_key_s);
    scope
      .define_property(
        proto,
        name_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::String(name_s),
            writable: true,
          },
        },
      )
      .expect("define prototype name");

    let message_s = scope.alloc_string("boom").expect("alloc message");
    scope
      .push_root(Value::String(message_s))
      .expect("root message");
    let msg_key_s = scope.alloc_string("message").expect("alloc key");
    scope.push_root(Value::String(msg_key_s)).expect("root key");
    let msg_key = PropertyKey::from_string(msg_key_s);
    scope
      .define_property(
        obj,
        msg_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::String(message_s),
            writable: true,
          },
        },
      )
      .expect("define message");

    let err = VmError::ThrowWithStack {
      value: Value::Object(obj),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("Error: boom"),
      "expected name from prototype to be included, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_object_without_name_or_message_uses_object_marker() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let obj = scope.alloc_object().expect("alloc thrown object");
    scope
      .push_root(Value::Object(obj))
      .expect("root thrown object");

    let err = VmError::ThrowWithStack {
      value: Value::Object(obj),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("[object]"),
      "expected thrown object without name/message to use marker, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_number_falls_back_when_heap_cannot_allocate_string() {
    let mut heap = Heap::new(HeapLimits::new(1, 1));
    let err = VmError::ThrowWithStack {
      value: Value::Number(1.0),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(&mut heap, err);
    assert!(
      msg.starts_with('1'),
      "expected thrown number to be formatted even when heap OOM, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn syntax_error_is_formatted_without_debug_noise() {
    let mut realm =
      WindowRealm::new(WindowRealmConfig::new("https://example.com/")).expect("create realm");
    let err = realm
      .exec_script("function(")
      .expect_err("expected syntax error");
    let msg = vm_error_to_string(realm.heap_mut(), err);
    assert!(
      msg.starts_with("syntax error:"),
      "expected syntax error to include message, got {msg:?}"
    );
    assert!(
      !msg.contains("Diagnostic"),
      "expected syntax error formatting to avoid debug output, got {msg:?}"
    );
  }

  #[test]
  fn syntax_error_output_is_bounded_and_truncated() {
    let mut realm =
      WindowRealm::new(WindowRealmConfig::new("https://example.com/")).expect("create realm");
    let err = realm
      .exec_script("function(")
      .expect_err("expected syntax error");
    let VmError::Syntax(mut diags) = err else {
      panic!("expected VmError::Syntax");
    };
    assert!(!diags.is_empty(), "expected at least one diagnostic");

    diags[0].message = format!(
      "{}TAIL",
      "a".repeat(MAX_SYNTAX_DIAGNOSTIC_MESSAGE_BYTES + 1)
    );

    while diags.len() <= MAX_SYNTAX_DIAGNOSTICS + 1 {
      let mut diag = diags[0].clone();
      diag.message = format!("diag {}", diags.len());
      diags.push(diag);
    }

    let msg = vm_error_to_string(realm.heap_mut(), VmError::Syntax(diags));
    assert!(
      msg.len() <= MAX_SYNTAX_ERROR_BYTES,
      "expected syntax error string to be bounded, got len={} msg={msg:?}",
      msg.len()
    );
    assert!(
      !msg.contains("TAIL"),
      "expected long diagnostic messages to be truncated, got {msg:?}"
    );
    assert!(
      msg.ends_with("\n..."),
      "expected syntax error output to include truncation marker, got {msg:?}"
    );
  }

  #[test]
  fn thrown_bigint_is_formatted_and_includes_stack_trace() {
    let mut realm =
      WindowRealm::new(WindowRealmConfig::new("https://example.com/")).expect("create realm");
    let err = realm.exec_script("throw 1n").expect_err("expected throw");
    let msg = vm_error_to_string(realm.heap_mut(), err);
    assert!(
      msg.starts_with("1n"),
      "expected bigint thrown value to be formatted, got {msg:?}"
    );
    assert!(
      msg.contains("at <inline>:"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_symbol_is_formatted_and_includes_stack_trace() {
    let mut realm =
      WindowRealm::new(WindowRealmConfig::new("https://example.com/")).expect("create realm");
    let err = realm
      .exec_script("throw Symbol('x')")
      .expect_err("expected throw");
    let msg = vm_error_to_string(realm.heap_mut(), err);
    assert!(
      msg.starts_with("Symbol(x)"),
      "expected symbol thrown value to include description, got {msg:?}"
    );
    assert!(
      msg.contains("at <inline>:"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_function_without_name_or_message_uses_function_marker() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let name_s = scope.alloc_string("").expect("alloc name");
    scope.push_root(Value::String(name_s)).expect("root name");

    let func = scope
      .alloc_native_function(vm_js::NativeFunctionId(0), None, name_s, 0)
      .expect("alloc function");
    scope.push_root(Value::Object(func)).expect("root function");

    let err = VmError::ThrowWithStack {
      value: Value::Object(func),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("[function]"),
      "expected thrown function without name/message to use marker, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_named_function_uses_function_marker_with_name() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let empty_name_s = scope.alloc_string("").expect("alloc name");
    scope
      .push_root(Value::String(empty_name_s))
      .expect("root name");

    let func = scope
      .alloc_native_function(vm_js::NativeFunctionId(0), None, empty_name_s, 0)
      .expect("alloc function");
    scope.push_root(Value::Object(func)).expect("root function");

    let name_s = scope.alloc_string("foo").expect("alloc name");
    scope.push_root(Value::String(name_s)).expect("root name");
    let name_key_s = scope.alloc_string("name").expect("alloc key");
    scope
      .push_root(Value::String(name_key_s))
      .expect("root key");
    let name_key = PropertyKey::from_string(name_key_s);
    scope
      .define_property(
        func,
        name_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::String(name_s),
            writable: true,
          },
        },
      )
      .expect("define name");

    let err = VmError::ThrowWithStack {
      value: Value::Object(func),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("[function foo]"),
      "expected thrown function with name property to include it, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn thrown_promise_without_name_or_message_uses_promise_marker() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let promise = scope.alloc_promise().expect("alloc promise");
    scope
      .push_root(Value::Object(promise))
      .expect("root promise");

    let err = VmError::ThrowWithStack {
      value: Value::Object(promise),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let msg = vm_error_to_string(scope.heap_mut(), err);
    assert!(
      msg.starts_with("[promise]"),
      "expected thrown Promise to use marker, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace to be included, got {msg:?}"
    );
  }

  #[test]
  fn console_format_basic_substitution() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("[%s %d]").expect("alloc fmt");
    scope.push_root(Value::String(fmt_s)).expect("root fmt");
    let arg_s = scope.alloc_string("x").expect("alloc arg");
    scope.push_root(Value::String(arg_s)).expect("root arg");

    let msg = format_console_arguments_limited(
      scope.heap_mut(),
      &[Value::String(fmt_s), Value::String(arg_s), Value::Number(1.0)],
    );
    assert_eq!(msg, "[x 1]");
  }

  #[test]
  fn console_format_percent_escape() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%%").expect("alloc fmt");
    scope.push_root(Value::String(fmt_s)).expect("root fmt");

    let msg = format_console_arguments_limited(scope.heap_mut(), &[Value::String(fmt_s)]);
    assert_eq!(msg, "%");
  }

  #[test]
  fn console_format_c_consumes_argument() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%c%s").expect("alloc fmt");
    scope.push_root(Value::String(fmt_s)).expect("root fmt");
    let style_s = scope.alloc_string("color:red").expect("alloc style");
    scope
      .push_root(Value::String(style_s))
      .expect("root style");
    let arg_s = scope.alloc_string("x").expect("alloc arg");
    scope.push_root(Value::String(arg_s)).expect("root arg");

    let msg = format_console_arguments_limited(
      scope.heap_mut(),
      &[Value::String(fmt_s), Value::String(style_s), Value::String(arg_s)],
    );
    assert_eq!(msg, "x");
  }

  #[test]
  fn console_format_missing_arguments_leave_specifier_verbatim() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("[%s %d]").expect("alloc fmt");
    scope.push_root(Value::String(fmt_s)).expect("root fmt");
    let arg_s = scope.alloc_string("x").expect("alloc arg");
    scope.push_root(Value::String(arg_s)).expect("root arg");

    let msg = format_console_arguments_limited(
      scope.heap_mut(),
      &[Value::String(fmt_s), Value::String(arg_s)],
    );
    assert_eq!(msg, "[x %d]");
  }

  #[test]
  fn console_format_extra_arguments_are_appended() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%s").expect("alloc fmt");
    scope.push_root(Value::String(fmt_s)).expect("root fmt");
    let arg_s = scope.alloc_string("x").expect("alloc arg");
    scope.push_root(Value::String(arg_s)).expect("root arg");

    let msg = format_console_arguments_limited(
      scope.heap_mut(),
      &[Value::String(fmt_s), Value::String(arg_s), Value::Number(1.0)],
    );
    assert_eq!(msg, "x 1");
  }

  #[test]
  fn console_format_output_is_bounded_and_truncated() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%s").expect("alloc fmt");
    scope.push_root(Value::String(fmt_s)).expect("root fmt");

    let long_text = format!("{}TAIL", "a".repeat(MAX_CONSOLE_MESSAGE_BYTES + 8));
    let long_s = scope.alloc_string(&long_text).expect("alloc long");
    scope
      .push_root(Value::String(long_s))
      .expect("root long");

    let msg =
      format_console_arguments_limited(scope.heap_mut(), &[Value::String(fmt_s), Value::String(long_s)]);
    assert!(
      msg.len() <= MAX_CONSOLE_MESSAGE_BYTES,
      "expected console output to be bounded, got len={} msg={msg:?}",
      msg.len()
    );
    assert!(
      !msg.contains("TAIL"),
      "expected truncated console output to omit tail, got {msg:?}"
    );
    assert!(
      msg.ends_with("..."),
      "expected truncated console output to include ellipsis, got {msg:?}"
    );
  }

  #[test]
  fn console_substitution_strings_are_formatted() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("hello %s").expect("alloc format string");
    let arg_s = scope.alloc_string("world").expect("alloc arg string");
    scope
      .push_roots(&[Value::String(fmt_s), Value::String(arg_s)])
      .expect("root strings");

    let args = [Value::String(fmt_s), Value::String(arg_s), Value::Number(42.0)];
    let out = format_console_arguments_limited(scope.heap_mut(), &args);
    assert_eq!(out, "hello world 42");
  }

  #[test]
  fn console_substitution_percent_does_not_consume_args() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%% %s").expect("alloc format string");
    let arg_s = scope.alloc_string("ok").expect("alloc arg string");
    scope
      .push_roots(&[Value::String(fmt_s), Value::String(arg_s)])
      .expect("root strings");

    let args = [Value::String(fmt_s), Value::String(arg_s)];
    let out = format_console_arguments_limited(scope.heap_mut(), &args);
    assert_eq!(out, "% ok");
  }

  #[test]
  fn console_substitution_css_style_consumes_arg_without_output() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%cHello").expect("alloc format string");
    let style_s = scope.alloc_string("color: red").expect("alloc style string");
    let extra_s = scope.alloc_string("world").expect("alloc extra string");
    scope
      .push_roots(&[
        Value::String(fmt_s),
        Value::String(style_s),
        Value::String(extra_s),
      ])
      .expect("root strings");

    let args = [Value::String(fmt_s), Value::String(style_s), Value::String(extra_s)];
    let out = format_console_arguments_limited(scope.heap_mut(), &args);
    assert_eq!(out, "Hello world");
  }

  #[test]
  fn console_substitution_numeric_formats() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%d %i %f").expect("alloc format string");
    scope
      .push_root(Value::String(fmt_s))
      .expect("root format string");

    let args = [
      Value::String(fmt_s),
      Value::Number(1.5),
      Value::Number(1.5),
      Value::Number(1.5),
    ];
    let out = format_console_arguments_limited(scope.heap_mut(), &args);
    assert_eq!(out, "1.5 1 1.5");
  }

  #[test]
  fn console_substitution_object_formats() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("%o %O").expect("alloc format string");
    let obj = scope.alloc_object().expect("alloc object");
    scope
      .push_roots(&[Value::String(fmt_s), Value::Object(obj)])
      .expect("root args");

    let args = [Value::String(fmt_s), Value::Object(obj), Value::Object(obj)];
    let out = format_console_arguments_limited(scope.heap_mut(), &args);
    assert_eq!(out, "[object] [object]");
  }

  #[test]
  fn console_substitution_missing_args_leaves_sequence_intact() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let fmt_s = scope.alloc_string("x %s y").expect("alloc format string");
    scope
      .push_root(Value::String(fmt_s))
      .expect("root format string");

    let args = [Value::String(fmt_s)];
    let out = format_console_arguments_limited(scope.heap_mut(), &args);
    assert_eq!(out, "x %s y");
  }
}
