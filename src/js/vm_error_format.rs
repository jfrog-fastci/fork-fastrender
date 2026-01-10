use crate::error::Error;
use std::borrow::Cow;
use vm_js::{Heap, PropertyKey, StackFrame, Value, VmError};

const MAX_THROWN_STRING_CODE_UNITS: usize = 4096;
const MAX_STACK_TRACE_FRAMES: usize = 32;
const MAX_STACK_FRAME_TEXT_BYTES: usize = 256;
const MAX_STACK_TRACE_BYTES: usize = 16 * 1024;

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

fn format_stack_trace_limited(frames: &[StackFrame]) -> String {
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

fn format_thrown_value(heap: &mut Heap, value: Value) -> Option<String> {
  let obj = match value {
    Value::Undefined => return Some("undefined".to_string()),
    Value::Null => return Some("null".to_string()),
    Value::Bool(b) => return Some(b.to_string()),
    Value::Number(_) => {
      if let Ok(s) = heap.to_string(value) {
        if let Ok(js) = heap.get_string(s) {
          if js.len_code_units() <= MAX_THROWN_STRING_CODE_UNITS {
            return Some(js.to_utf8_lossy());
          }
        }
      }
      return None;
    }
    // Converting arbitrary BigInts to decimal strings can allocate unbounded host memory. Keep this
    // bounded and return a stable marker instead.
    Value::BigInt(_) => return Some("[bigint]".to_string()),
    Value::Symbol(_) => return Some("[symbol]".to_string()),
    Value::String(s) => {
      if let Ok(js) = heap.get_string(s) {
        if js.len_code_units() <= MAX_THROWN_STRING_CODE_UNITS {
          return Some(js.to_utf8_lossy());
        }
      }
      return None;
    }
    Value::Object(obj) => obj,
  };

  let mut scope = heap.scope();
  let _ = scope.push_root(Value::Object(obj));

  let mut get_prop_str = |name: &str| -> Option<String> {
    let key_s = scope.alloc_string(name).ok()?;
    scope.push_root(Value::String(key_s)).ok()?;
    let key = PropertyKey::from_string(key_s);
    let value = scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .ok()?
      .unwrap_or(Value::Undefined);
    match value {
      Value::String(s) => {
        let js = scope.heap().get_string(s).ok()?;
        if js.len_code_units() > MAX_THROWN_STRING_CODE_UNITS {
          return None;
        }
        Some(js.to_utf8_lossy())
      }
      _ => None,
    }
  };

  let name = get_prop_str("name");
  let message = get_prop_str("message");
  match (name, message) {
    (Some(name), Some(message)) if !message.is_empty() => Some(format!("{name}: {message}")),
    (Some(name), _) if !name.is_empty() => Some(name),
    (_, Some(message)) if !message.is_empty() => Some(message),
    _ => None,
  }
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
    let mut msg = format_thrown_value(heap, value).unwrap_or_else(|| "uncaught exception".to_string());

    if let Some(frames) = err.thrown_stack() {
      let stack = format_stack_trace_limited(frames);
      if !stack.is_empty() {
        msg.push('\n');
        msg.push_str(&stack);
      }
    }

    return msg;
  }

  match err {
    VmError::Syntax(diags) => format!("syntax error: {diags:?}"),
    other => other.to_string(),
  }
}
