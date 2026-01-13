use crate::fallible_alloc::arc_try_new_vm;
use crate::fallible_format;
use crate::property::{PropertyKey, PropertyKind};
use crate::source::{format_stack_trace, StackFrame};
use crate::{
  Budget, CompiledScript, Heap, HeapLimits, JsRuntime, Realm, SourceText, SourceTextInput,
  Termination, TerminationReason, Value, Vm, VmError, VmOptions,
};
use std::sync::Arc;

const OOM_PLACEHOLDER: &str = "<oom>";
const INVALID_STRING_PLACEHOLDER: &str = "<invalid string>";
const UNCAUGHT_EXCEPTION_PLACEHOLDER: &str = "uncaught exception";

/// Structured, host-friendly telemetry for a [`VmError`].
///
/// This report is **best-effort**:
/// - It never invokes user code (`ToString`, getters, Proxy traps).
/// - Host allocations are fallible; if formatting runs out of memory, fields may be empty.
///
/// The `kind` field is a stable, JSON-friendly string (e.g. `"throw"`, `"termination"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmErrorReport {
  /// Stable error kind string suitable for telemetry.
  pub kind: &'static str,
  /// A bounded, host-owned UTF-8 message.
  pub message: String,
  /// For `kind == "throw"` when the thrown value is a native Error object, the Error's `name`
  /// property (e.g. `"TypeError"`).
  pub exception_name: Option<String>,
  /// For `kind == "throw"` when the thrown value is a native Error object, the Error's `message`
  /// property (without the `"TypeError: "` prefix).
  pub exception_message: Option<String>,
  /// Captured stack frames when available.
  pub stack: Vec<StackFrame>,
  /// For `kind == "termination"`, the termination reason.
  pub termination_reason: Option<TerminationReason>,
}

fn format_value_debug_best_effort(value: Value) -> String {
  #[inline]
  fn push_u32(out: &mut String, value: u32) -> bool {
    fallible_format::try_write_u32(out, value).is_ok()
  }

  #[inline]
  fn push_handle(out: &mut String, ty: &str, index: u32, generation: u32) -> bool {
    if !push_str_best_effort(out, ty) || !push_char_best_effort(out, '(') {
      return false;
    }
    if !push_str_best_effort(out, "HeapId { index: ") {
      return false;
    }
    if !push_u32(out, index) {
      return false;
    }
    if !push_str_best_effort(out, ", generation: ") {
      return false;
    }
    if !push_u32(out, generation) {
      return false;
    }
    if !push_str_best_effort(out, " }") {
      return false;
    }
    push_char_best_effort(out, ')')
  }

  #[inline]
  fn push_f64(out: &mut String, n: f64) -> bool {
    if n.is_nan() {
      return push_str_best_effort(out, "NaN");
    }
    if n.is_infinite() {
      return push_str_best_effort(out, if n.is_sign_negative() { "-inf" } else { "inf" });
    }
    if n == 0.0 && n.is_sign_negative() {
      return push_str_best_effort(out, "-0.0");
    }
    // Avoid `format_args!("{n:?}")` which may allocate infallibly under the hood. `ryu` formats into
    // a stack buffer.
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(n);
    push_str_best_effort(out, s)
  }

  let mut out = String::new();
  // Best-effort preallocation; keep this small (this path is a fallback when ToString fails).
  let _ = out.try_reserve(128);

  match value {
    Value::Undefined => {
      let _ = push_str_best_effort(&mut out, "Undefined");
    }
    Value::Null => {
      let _ = push_str_best_effort(&mut out, "Null");
    }
    Value::Bool(b) => {
      if push_str_best_effort(&mut out, "Bool(") {
        let _ = push_str_best_effort(&mut out, if b { "true" } else { "false" });
        let _ = push_char_best_effort(&mut out, ')');
      }
    }
    Value::Number(n) => {
      if push_str_best_effort(&mut out, "Number(") {
        let _ = push_f64(&mut out, n);
        let _ = push_char_best_effort(&mut out, ')');
      }
    }
    Value::BigInt(b) => {
      if push_str_best_effort(&mut out, "BigInt(") {
        let _ = push_handle(&mut out, "GcBigInt", b.index(), b.generation());
        let _ = push_char_best_effort(&mut out, ')');
      }
    }
    Value::String(s) => {
      if push_str_best_effort(&mut out, "String(") {
        let _ = push_handle(&mut out, "GcString", s.index(), s.generation());
        let _ = push_char_best_effort(&mut out, ')');
      }
    }
    Value::Symbol(sym) => {
      if push_str_best_effort(&mut out, "Symbol(") {
        let _ = push_handle(&mut out, "GcSymbol", sym.index(), sym.generation());
        let _ = push_char_best_effort(&mut out, ')');
      }
    }
    Value::Object(obj) => {
      if push_str_best_effort(&mut out, "Object(") {
        let _ = push_handle(&mut out, "GcObject", obj.index(), obj.generation());
        let _ = push_char_best_effort(&mut out, ')');
      }
    }
  };

  if out.is_empty() {
    return string_from_str_best_effort(UNCAUGHT_EXCEPTION_PLACEHOLDER);
  }
  out
}

#[inline]
fn string_from_str_best_effort(s: &str) -> String {
  let mut out = String::new();
  if out.try_reserve_exact(s.len()).is_ok() {
    out.push_str(s);
  }
  out
}

#[inline]
fn push_str_best_effort(out: &mut String, s: &str) -> bool {
  if out.try_reserve(s.len()).is_err() {
    return false;
  }
  out.push_str(s);
  true
}

#[inline]
fn push_char_best_effort(out: &mut String, ch: char) -> bool {
  let mut buf = [0u8; 4];
  let encoded = ch.encode_utf8(&mut buf);
  if out.try_reserve(encoded.len()).is_err() {
    return false;
  }
  out.push(ch);
  true
}

#[inline]
fn clone_stack_best_effort(stack: &[StackFrame]) -> Vec<StackFrame> {
  if stack.is_empty() {
    return Vec::new();
  }

  let mut out: Vec<StackFrame> = Vec::new();
  // Prefer an exact reserve so we don't repeatedly reallocate in the common case, but fall back to
  // incremental best-effort cloning when we cannot allocate the full capacity up-front.
  if out.try_reserve_exact(stack.len()).is_ok() {
    for frame in stack {
      out.push(frame.clone());
    }
    return out;
  }

  // Incrementally extend until we hit OOM; return a partial stack rather than dropping it entirely.
  for frame in stack {
    if out.try_reserve(1).is_err() {
      break;
    }
    out.push(frame.clone());
  }
  out
}

/// Host integration hooks for [`Agent`] script execution.
///
/// This is intentionally minimal today (jobs/modules are separate workstreams), but shaped so it
/// can be extended without redesign later.
pub trait HostHooks {
  /// Invoked after a script run, to allow the embedding to perform a microtask checkpoint.
  ///
  /// This mirrors the HTML event loop's
  /// ["perform a microtask checkpoint"](https://html.spec.whatwg.org/multipage/webappapis.html#perform-a-microtask-checkpoint)
  /// step that occurs after script execution.
  ///
  /// The default implementation does nothing.
  fn microtask_checkpoint(&mut self, _agent: &mut Agent) -> Result<(), VmError> {
    Ok(())
  }
}

/// A spec-shaped embedding façade that bundles a [`Vm`], [`Heap`], and at least one [`Realm`].
///
/// This is the primary entry point for host embeddings (FastRender, WebIDL adapters, etc). It
/// centralizes ownership and exposes safe entry points for script execution and host integration.
pub struct Agent {
  runtime: JsRuntime,
}

#[inline]
fn is_hard_stop_error(err: &VmError) -> bool {
  matches!(err, VmError::Termination(_) | VmError::OutOfMemory)
}

impl Agent {
  /// Creates a new [`Agent`] from an already-constructed [`Vm`] and [`Heap`], and initializes a
  /// fresh [`Realm`] on that heap.
  pub fn new(vm: Vm, heap: Heap) -> Result<Self, VmError> {
    Ok(Self {
      runtime: JsRuntime::new(vm, heap)?,
    })
  }

  /// Convenience constructor from [`VmOptions`] and [`HeapLimits`].
  pub fn with_options(vm_options: VmOptions, heap_limits: HeapLimits) -> Result<Self, VmError> {
    let vm = Vm::new(vm_options);
    let heap = Heap::new(heap_limits);
    Self::new(vm, heap)
  }

  /// Borrows the underlying [`Vm`].
  #[inline]
  pub fn vm(&self) -> &Vm {
    &self.runtime.vm
  }

  /// Borrows the underlying [`Vm`] mutably.
  #[inline]
  pub fn vm_mut(&mut self) -> &mut Vm {
    &mut self.runtime.vm
  }

  /// Borrows the underlying [`Heap`].
  #[inline]
  pub fn heap(&self) -> &Heap {
    self.runtime.heap()
  }

  /// Borrows the underlying [`Heap`] mutably.
  #[inline]
  pub fn heap_mut(&mut self) -> &mut Heap {
    self.runtime.heap_mut()
  }

  /// Borrows the primary [`Realm`].
  #[inline]
  pub fn realm(&self) -> &Realm {
    self.runtime.realm()
  }

  /// Borrow-split the agent into its core components: the VM, the current realm, and the heap.
  ///
  /// This mirrors [`JsRuntime::vm_realm_and_heap_mut`] and exists so embeddings (including internal
  /// test harnesses) can access `&mut Vm` + `&mut Heap` while also needing immutable access to
  /// realm metadata (global object, intrinsics, realm id) without an embedder-side raw-pointer
  /// workaround.
  pub fn vm_realm_and_heap_mut(&mut self) -> (&mut Vm, &Realm, &mut Heap) {
    self.runtime.vm_realm_and_heap_mut()
  }

  /// Perform a microtask checkpoint, draining the VM-owned microtask queue.
  ///
  /// This is a convenience wrapper around [`Vm::perform_microtask_checkpoint`] for lightweight
  /// embeddings (including fuzzing harnesses) that use the VM-owned [`MicrotaskQueue`].
  pub fn perform_microtask_checkpoint(&mut self) -> Result<(), VmError> {
    // Borrow-split the runtime to avoid needing embedder-side raw pointers.
    let vm = &mut self.runtime.vm;
    let heap = &mut self.runtime.heap;
    vm.perform_microtask_checkpoint(heap)
  }

  /// Run a classic script with a per-run [`Budget`].
  ///
  /// This:
  /// - applies `budget` for the duration of the run (restoring the previous VM budget afterwards),
  /// - executes `source_text` as a classic script, and
  /// - invokes [`HostHooks::microtask_checkpoint`] afterwards (if provided).
  pub fn run_script<'a>(
    &mut self,
    source_name: impl Into<SourceTextInput<'a>>,
    source_text: impl Into<SourceTextInput<'a>>,
    budget: Budget,
    mut host_hooks: Option<&mut dyn HostHooks>,
  ) -> Result<Value, VmError> {
    let source = match SourceText::new_charged(self.heap_mut(), source_name, source_text)
      .and_then(arc_try_new_vm)
    {
      Ok(source) => source,
      Err(err) => {
        if is_hard_stop_error(&err) {
          self.runtime.teardown_microtasks();
        }
        return Err(err);
      }
    };

    // Swap the VM budget in/out without holding a borrow across `exec_script`.
    let prev_budget = self.runtime.vm.swap_budget_state(budget);

    let mut result = self.runtime.exec_script_source(source);

    // If the script executed (successfully or with a JS `throw`), the HTML script processing model
    // performs a microtask checkpoint afterwards. For now this is a host hook placeholder.
    if matches!(
      result,
      Ok(_) | Err(VmError::Throw(_)) | Err(VmError::ThrowWithStack { .. })
    ) {
      if let Some(hooks) = host_hooks.as_mut() {
        // Root the completion value across the checkpoint so a host checkpoint implementation can
        // allocate/GC without invalidating the returned value.
        let root = match &result {
          Ok(v) => self.heap_mut().add_root(*v).ok(),
          Err(err) => err
            .thrown_value()
            .and_then(|v| self.heap_mut().add_root(v).ok()),
        };

        // If we fail to allocate a persistent root (OOM), skip the checkpoint: running it without
        // rooting could allow GC to invalidate the completion value that we are about to return to
        // the host.
        let checkpoint_result = if root.is_some() {
          hooks.microtask_checkpoint(self)
        } else {
          Ok(())
        };

        if let Some(root) = root {
          self.heap_mut().remove_root(root);
        }

        result = match result {
          Ok(v) => checkpoint_result.map(|_| v),
          Err(err) => {
            // Preserve the original script error; checkpoint errors should be reported separately by
            // the host.
            let _ = checkpoint_result;
            Err(err)
          }
        };
      }
    }

    self.runtime.vm.restore_budget_state(prev_budget);

    if let Err(err) = &result {
      if is_hard_stop_error(err) {
        self.runtime.teardown_microtasks();
      }
    }
    result
  }

  /// Run a pre-compiled classic script (HIR) with a per-run [`Budget`].
  ///
  /// This mirrors [`Agent::run_script`], but executes HIR lowered via [`CompiledScript`]
  /// ([`JsRuntime::exec_compiled_script`]) instead of parsing source text at runtime.
  pub fn run_compiled_script(
    &mut self,
    script: Arc<CompiledScript>,
    budget: Budget,
    mut host_hooks: Option<&mut dyn HostHooks>,
  ) -> Result<Value, VmError> {
    // Swap the VM budget in/out without holding a borrow across `exec_compiled_script`.
    let prev_budget = self.runtime.vm.swap_budget_state(budget);

    let mut result = self.runtime.exec_compiled_script(script);

    // If the script executed (successfully or with a JS `throw`), the HTML script processing model
    // performs a microtask checkpoint afterwards. For now this is a host hook placeholder.
    if matches!(
      result,
      Ok(_) | Err(VmError::Throw(_)) | Err(VmError::ThrowWithStack { .. })
    ) {
      if let Some(hooks) = host_hooks.as_mut() {
        // Root the completion value across the checkpoint so a host checkpoint implementation can
        // allocate/GC without invalidating the returned value.
        let root = match &result {
          Ok(v) => self.heap_mut().add_root(*v).ok(),
          Err(err) => err
            .thrown_value()
            .and_then(|v| self.heap_mut().add_root(v).ok()),
        };

        // If we fail to allocate a persistent root (OOM), skip the checkpoint: running it without
        // rooting could allow GC to invalidate the completion value that we are about to return to
        // the host.
        let checkpoint_result = if root.is_some() {
          hooks.microtask_checkpoint(self)
        } else {
          Ok(())
        };

        if let Some(root) = root {
          self.heap_mut().remove_root(root);
        }

        result = match result {
          Ok(v) => checkpoint_result.map(|_| v),
          Err(err) => {
            // Preserve the original script error; checkpoint errors should be reported separately
            // by the host.
            let _ = checkpoint_result;
            Err(err)
          }
        };
      }
    }

    self.runtime.vm.restore_budget_state(prev_budget);

    if let Err(err) = &result {
      // Match `run_script` behaviour: if the run terminated due to a hard-stop error (fuel/deadline
      // exhaustion, interrupt, or OOM), discard any queued microtasks so we don't leak persistent
      // roots when the embedding will not drive the event loop further.
      //
      // Note: `JsRuntime::exec_compiled_script*` already tears down microtasks on hard-stop errors
      // originating from script execution. This is primarily a safety net for hard-stop errors
      // returned by the *host microtask checkpoint hook* itself.
      if is_hard_stop_error(err) {
        self.runtime.teardown_microtasks();
      }
    }

    result
  }

  /// Convert a JavaScript value into a host-owned string for exception reporting.
  ///
  /// This uses the VM's `ToString` implementation (via [`Heap::to_string`]).
  pub fn value_to_error_string(&mut self, value: Value) -> String {
    // Special-case native Error instances for host-facing formatting. `Heap::to_string` does not
    // support objects, and calling `Error.prototype.toString` would be user-observable.
    //
    // This path is side-effect free (no getters, no Proxy traps).
    if let Value::Object(obj) = value {
      if self.heap().is_error_object(obj) {
        if let Some(msg) = self.try_format_error_object_message(obj) {
          return msg;
        }
      }
    }

    let s = match self.heap_mut().to_string(value) {
      Ok(s) => s,
      // If ToString itself throws, format the thrown value.
      Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => {
        return self.value_to_error_string(v);
      }
      Err(VmError::OutOfMemory) => return string_from_str_best_effort(OOM_PLACEHOLDER),
      // Best-effort: fallback to a bounded debug-style representation without invoking user code.
      Err(_) => return format_value_debug_best_effort(value),
    };

    let Ok(js) = self.heap().get_string(s) else {
      return string_from_str_best_effort(INVALID_STRING_PLACEHOLDER);
    };

    // Bound attacker-controlled strings; host-visible error formatting should never allocate
    // unbounded Rust `String`s.
    let marker = "…";
    let max_bytes = fallible_format::MAX_ERROR_MESSAGE_BYTES;
    let max_before_marker = max_bytes.saturating_sub(marker.len());
    match crate::string::utf16_to_utf8_lossy_bounded(js.as_code_units(), max_before_marker) {
      Ok((mut out, truncated)) => {
        if truncated {
          // Best-effort truncation marker. If we can't allocate even a few bytes, return the
          // truncated prefix without the marker.
          if out.try_reserve(marker.len()).is_ok() {
            out.push_str(marker);
          }
        }
        out
      }
      Err(_) => string_from_str_best_effort(OOM_PLACEHOLDER),
    }
  }

  /// Formats a VM error into a host-visible string.
  pub fn format_vm_error(&mut self, err: &VmError) -> String {
    match err {
      // `VmError::Return` is an internal control-flow signal used by async evaluation machinery and
      // should never be observable at host boundaries. Treat it as an invariant violation in this
      // host-facing formatting API.
      VmError::Return(_) => {
        string_from_str_best_effort("invariant violation: internal Return completion escaped")
      }
      VmError::Throw(value) => self.format_thrown_value(*value),
      VmError::ThrowWithStack { value, stack } => {
        let msg = self.format_thrown_value(*value);
        if stack.is_empty() {
          msg
        } else {
          let stack_trace = format_stack_trace(stack);
          if stack_trace.is_empty() {
            return msg;
          }
          let mut out = msg;
          if out.try_reserve(1 + stack_trace.len()).is_err() {
            // If we can't allocate enough space for the stack trace, fall back to the exception
            // message alone.
            return out;
          }
          out.push('\n');
          out.push_str(&stack_trace);
          out
        }
      }
      VmError::Termination(term) => format_termination(term),
      VmError::OutOfMemory => string_from_str_best_effort("out of memory"),
      VmError::InvariantViolation(msg) => {
        fallible_format::try_format_error_message("invariant violation: ", msg, "")
          .unwrap_or_default()
      }
      VmError::LimitExceeded(msg) => {
        fallible_format::try_format_error_message("limit exceeded: ", msg, "").unwrap_or_default()
      }
      VmError::InvalidHandle { location } => {
        // Mirror the `Display` impl ("invalid handle ({location})") without infallible formatting.
        let mut out = String::new();
        if fallible_format::try_push_str(&mut out, "invalid handle (").is_err() {
          return String::new();
        }
        if fallible_format::try_push_str(&mut out, location.file()).is_err() {
          return String::new();
        }
        if fallible_format::try_push_char(&mut out, ':').is_err() {
          return String::new();
        }
        if fallible_format::try_write_u32(&mut out, location.line()).is_err() {
          return String::new();
        }
        if fallible_format::try_push_char(&mut out, ':').is_err() {
          return String::new();
        }
        if fallible_format::try_write_u32(&mut out, location.column()).is_err() {
          return String::new();
        }
        if fallible_format::try_push_char(&mut out, ')').is_err() {
          return String::new();
        }
        out
      }
      VmError::PrototypeCycle => string_from_str_best_effort("prototype cycle"),
      VmError::PrototypeChainTooDeep => string_from_str_best_effort("prototype chain too deep"),
      VmError::Unimplemented(msg) => {
        fallible_format::try_format_error_message("unimplemented: ", msg, "").unwrap_or_default()
      }
      VmError::InvalidPropertyDescriptorPatch => string_from_str_best_effort(
        "invalid property descriptor patch: cannot mix data and accessor fields",
      ),
      VmError::PropertyNotFound => string_from_str_best_effort("property not found"),
      VmError::PropertyNotData => string_from_str_best_effort("property is not a data property"),
      VmError::TypeError(msg) => {
        fallible_format::try_format_error_message("type error: ", msg, "").unwrap_or_default()
      }
      VmError::RangeError(msg) => {
        fallible_format::try_format_error_message("range error: ", msg, "").unwrap_or_default()
      }
      VmError::NotCallable => string_from_str_best_effort("value is not callable"),
      VmError::NotConstructable => string_from_str_best_effort("value is not a constructor"),
      // Internal-only completion used by async generator resumption; should never be observable to
      // host embeddings, but handle defensively.
      VmError::InternalReturn(_) => string_from_str_best_effort("internal return completion"),
      VmError::Syntax(_) => string_from_str_best_effort("syntax error"),
    }
  }

  /// Formats a VM error into a structured, host-owned report.
  ///
  /// This is intended for embeddings that want actionable telemetry (exception message + stack
  /// frames) without invoking user code. Like [`Agent::format_vm_error`], this is best-effort under
  /// host OOM: it returns partial/empty fields rather than aborting.
  pub fn error_report(&mut self, err: &VmError) -> VmErrorReport {
    match err {
      VmError::Return(_) => VmErrorReport {
        kind: "invariant_violation",
        message: string_from_str_best_effort("invariant violation: internal Return completion escaped"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::Throw(value) => {
        let (exception_name, exception_message) = match value {
          Value::Object(obj) if self.heap().is_error_object(*obj) => {
            self.try_extract_error_object_name_and_message(*obj)
          }
          _ => (None, None),
        };
        VmErrorReport {
          kind: "throw",
          message: self.format_thrown_value(*value),
          exception_name,
          exception_message,
          stack: Vec::new(),
          termination_reason: None,
        }
      }
      VmError::ThrowWithStack { value, stack } => {
        let (exception_name, exception_message) = match value {
          Value::Object(obj) if self.heap().is_error_object(*obj) => {
            self.try_extract_error_object_name_and_message(*obj)
          }
          _ => (None, None),
        };
        VmErrorReport {
          kind: "throw",
          message: self.format_thrown_value(*value),
          exception_name,
          exception_message,
          stack: clone_stack_best_effort(stack),
          termination_reason: None,
        }
      }
      VmError::Termination(term) => {
        let reason_str = match term.reason {
          TerminationReason::OutOfFuel => "execution terminated: out of fuel",
          TerminationReason::DeadlineExceeded => "execution terminated: deadline exceeded",
          TerminationReason::Interrupted => "execution terminated: interrupted",
          TerminationReason::OutOfMemory => "execution terminated: out of memory",
          TerminationReason::StackOverflow => "execution terminated: stack overflow",
        };
        VmErrorReport {
          kind: "termination",
          message: string_from_str_best_effort(reason_str),
          exception_name: None,
          exception_message: None,
          stack: clone_stack_best_effort(&term.stack),
          termination_reason: Some(term.reason),
        }
      }
      VmError::Syntax(diags) => {
        // Include the first diagnostic message when available; this is host-generated data and does
        // not invoke any user code. Keep the output bounded and OOM-safe.
        let detail = diags
          .first()
          .map(|d| d.message.as_str())
          .filter(|s| !s.is_empty());

        let message = if let Some(detail) = detail {
          fallible_format::try_format_error_message("syntax error: ", detail, "")
            .unwrap_or_else(|_| string_from_str_best_effort("syntax error"))
        } else {
          string_from_str_best_effort("syntax error")
        };

        VmErrorReport {
          kind: "syntax",
          message,
          exception_name: None,
          exception_message: None,
          stack: Vec::new(),
          termination_reason: None,
        }
      }
      VmError::OutOfMemory => VmErrorReport {
        kind: "out_of_memory",
        message: string_from_str_best_effort("out of memory"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::InvariantViolation(msg) => VmErrorReport {
        kind: "invariant_violation",
        message: fallible_format::try_format_error_message("invariant violation: ", msg, "")
          .unwrap_or_default(),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::LimitExceeded(msg) => VmErrorReport {
        kind: "limit_exceeded",
        message: fallible_format::try_format_error_message("limit exceeded: ", msg, "")
          .unwrap_or_default(),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::InvalidHandle { location } => {
        // Mirror the `Display` impl ("invalid handle ({location})") without infallible formatting.
        let mut message = String::new();
        let ok = fallible_format::try_push_str(&mut message, "invalid handle (")
          .and_then(|_| fallible_format::try_push_str(&mut message, location.file()))
          .and_then(|_| fallible_format::try_push_char(&mut message, ':'))
          .and_then(|_| fallible_format::try_write_u32(&mut message, location.line()))
          .and_then(|_| fallible_format::try_push_char(&mut message, ':'))
          .and_then(|_| fallible_format::try_write_u32(&mut message, location.column()))
          .and_then(|_| fallible_format::try_push_char(&mut message, ')'))
          .is_ok();
        if !ok {
          message = String::new();
        }
        VmErrorReport {
          kind: "invalid_handle",
          message,
          exception_name: None,
          exception_message: None,
          stack: Vec::new(),
          termination_reason: None,
        }
      }
      VmError::PrototypeCycle => VmErrorReport {
        kind: "prototype_cycle",
        message: string_from_str_best_effort("prototype cycle"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::PrototypeChainTooDeep => VmErrorReport {
        kind: "prototype_chain_too_deep",
        message: string_from_str_best_effort("prototype chain too deep"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::Unimplemented(msg) => VmErrorReport {
        kind: "unimplemented",
        message: fallible_format::try_format_error_message("unimplemented: ", msg, "")
          .unwrap_or_default(),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::InvalidPropertyDescriptorPatch => VmErrorReport {
        kind: "invalid_property_descriptor_patch",
        message: string_from_str_best_effort(
          "invalid property descriptor patch: cannot mix data and accessor fields",
        ),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::PropertyNotFound => VmErrorReport {
        kind: "property_not_found",
        message: string_from_str_best_effort("property not found"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::PropertyNotData => VmErrorReport {
        kind: "property_not_data",
        message: string_from_str_best_effort("property is not a data property"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::TypeError(msg) => VmErrorReport {
        kind: "type_error",
        message: fallible_format::try_format_error_message("type error: ", msg, "")
          .unwrap_or_default(),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::RangeError(msg) => VmErrorReport {
        kind: "range_error",
        message: fallible_format::try_format_error_message("range error: ", msg, "")
          .unwrap_or_default(),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::NotCallable => VmErrorReport {
        kind: "not_callable",
        message: string_from_str_best_effort("value is not callable"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      VmError::NotConstructable => VmErrorReport {
        kind: "not_constructable",
        message: string_from_str_best_effort("value is not a constructor"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
      // Internal-only completion used by async generator resumption; should never be observable to
      // host embeddings, but handle defensively.
      VmError::InternalReturn(_) => VmErrorReport {
        kind: "internal_return",
        message: string_from_str_best_effort("internal return completion"),
        exception_name: None,
        exception_message: None,
        stack: Vec::new(),
        termination_reason: None,
      },
    }
  }

  fn try_extract_error_object_name_and_message(
    &mut self,
    obj: crate::GcObject,
  ) -> (Option<String>, Option<String>) {
    // Best-effort: never allow failure to extract structured fields to change the overall error
    // report shape.
    let mut scope = self.heap_mut().scope();
    if scope.push_root(Value::Object(obj)).is_err() {
      return (None, None);
    }

    let name_key_s = match scope.common_key_name() {
      Ok(s) => s,
      Err(_) => return (None, None),
    };
    if scope.push_root(Value::String(name_key_s)).is_err() {
      return (None, None);
    }
    let message_key_s = match scope.common_key_message() {
      Ok(s) => s,
      Err(_) => return (None, None),
    };
    if scope.push_root(Value::String(message_key_s)).is_err() {
      return (None, None);
    }

    let name_key = PropertyKey::from_string(name_key_s);
    let message_key = PropertyKey::from_string(message_key_s);
    let heap = scope.heap();

    let max_bytes = fallible_format::MAX_ERROR_MESSAGE_BYTES;

    let to_utf8_bounded = |s: crate::GcString| -> Option<String> {
      let js = heap.get_string(s).ok()?;
      let (mut out, truncated) =
        crate::string::utf16_to_utf8_lossy_bounded(js.as_code_units(), max_bytes).ok()?;
      if truncated && out.len().saturating_add(3) <= max_bytes {
        // Best-effort truncation marker.
        let _ = push_str_best_effort(&mut out, "...");
      }
      Some(out)
    };

    let name_value = Self::get_data_string_property_from_chain(heap, obj, &name_key);
    let message_value = Self::get_data_string_property_from_chain(heap, obj, &message_key);

    (
      name_value.and_then(to_utf8_bounded),
      message_value.and_then(to_utf8_bounded),
    )
  }

  fn format_thrown_value(&mut self, value: Value) -> String {
    match value {
      Value::Object(obj) if self.heap().is_error_object(obj) => self
        .try_format_error_object_message(obj)
        // If we can't safely extract `"name"`/`"message"` (e.g. accessor properties), fall back to a
        // debug-style representation rather than `ToString`, which could invoke user code.
        .unwrap_or_else(|| format_value_debug_best_effort(value)),
      _ => self.value_to_error_string(value),
    }
  }

  /// Best-effort, host-friendly formatting for native Error objects.
  ///
  /// This avoids invoking user code by reading only data properties from the object/prototype
  /// chain (no accessors, no Proxy traps).
  fn try_format_error_object_message(&mut self, obj: crate::GcObject) -> Option<String> {
    // Root the thrown object while we allocate the key strings used for descriptor lookup.
    let mut scope = self.heap_mut().scope();
    scope.push_root(Value::Object(obj)).ok()?;

    let name_key_s = scope.common_key_name().ok()?;
    // Root key strings across subsequent allocations so they survive any GC triggered while we
    // allocate the other key.
    scope.push_root(Value::String(name_key_s)).ok()?;
    let message_key_s = scope.common_key_message().ok()?;
    scope.push_root(Value::String(message_key_s)).ok()?;

    let name_key = PropertyKey::from_string(name_key_s);
    let message_key = PropertyKey::from_string(message_key_s);

    let heap = scope.heap();

    let max_bytes = fallible_format::MAX_ERROR_MESSAGE_BYTES;

    // Convert a JS String value to a bounded UTF-8 `String`.
    //
    // This is best-effort: on OOM it returns a small placeholder rather than bubbling up `None`,
    // since `format_vm_error` would otherwise fall back to the debug-style representation of the
    // thrown object.
    let to_utf8_bounded = |s: crate::GcString, max: usize| -> Option<(String, bool)> {
      let js = heap.get_string(s).ok()?;
      match crate::string::utf16_to_utf8_lossy_bounded(js.as_code_units(), max) {
        Ok(v) => Some(v),
        Err(_) => Some((string_from_str_best_effort(OOM_PLACEHOLDER), false)),
      }
    };

    // Prefer the own `"name"`/`"message"` data properties used by `error_object::new_error`, but
    // also consult the prototype chain so `new TypeError("boom")`-style errors can use the builtin
    // prototype `"name"` (e.g. `%TypeError.prototype%.name === "TypeError"`).
    //
    // This intentionally avoids `Error.prototype.toString` since it is user-overridable and can
    // allocate or re-enter user code.
    let name_value = Self::get_data_string_property_from_chain(heap, obj, &name_key);
    let message_value = Self::get_data_string_property_from_chain(heap, obj, &message_key);

    // Fast-path emptiness checks without converting the full strings.
    let name_is_empty = name_value
      .and_then(|s| heap.get_string(s).ok())
      .is_some_and(|js| js.as_code_units().is_empty());
    let message_is_empty = message_value
      .and_then(|s| heap.get_string(s).ok())
      .is_some_and(|js| js.as_code_units().is_empty());

    let name = name_value.and_then(|s| to_utf8_bounded(s, max_bytes));
    // If we can't extract a string name, prefer message-only formatting over a generic placeholder.
    if name.is_none() {
      let Some((mut message, truncated)) =
        message_value.and_then(|s| to_utf8_bounded(s, max_bytes))
      else {
        return None;
      };
      if truncated && message.len().saturating_add(3) <= max_bytes {
        // Best-effort truncation marker.
        let _ = push_str_best_effort(&mut message, "...");
      }
      return Some(message);
    }

    let Some((mut name, mut truncated)) = name else {
      return None;
    };
    if message_value.is_none() || message_is_empty {
      if truncated && name.len().saturating_add(3) <= max_bytes {
        let _ = push_str_best_effort(&mut name, "...");
      }
      return Some(name);
    }

    // If `name` is empty, return `message` directly (spec-like `Error.prototype.toString`).
    if name_is_empty {
      let Some((mut message, message_truncated)) =
        message_value.and_then(|s| to_utf8_bounded(s, max_bytes))
      else {
        return Some(name);
      };
      truncated |= message_truncated;
      if truncated && message.len().saturating_add(3) <= max_bytes {
        let _ = push_str_best_effort(&mut message, "...");
      }
      return Some(message);
    }

    // `name` + ": " + `message`, bounded to `MAX_ERROR_MESSAGE_BYTES`.
    let separator = ": ";
    if name.len().saturating_add(separator.len()) >= max_bytes {
      if truncated && name.len().saturating_add(3) <= max_bytes {
        let _ = push_str_best_effort(&mut name, "...");
      }
      return Some(name);
    }

    let mut out = name;
    if !push_str_best_effort(&mut out, separator) {
      return Some(out);
    }

    let remaining = max_bytes.saturating_sub(out.len());
    let Some((message, message_truncated)) =
      message_value.and_then(|s| to_utf8_bounded(s, remaining))
    else {
      return Some(out);
    };
    truncated |= message_truncated;

    if !push_str_best_effort(&mut out, &message) {
      return Some(out);
    }

    if truncated && out.len().saturating_add(3) <= max_bytes {
      let _ = push_str_best_effort(&mut out, "...");
    }
    Some(out)
  }

  fn get_data_string_property_from_chain(
    heap: &Heap,
    start: crate::GcObject,
    key: &PropertyKey,
  ) -> Option<crate::GcString> {
    let mut current = Some(start);
    let mut steps = 0usize;
    while let Some(obj) = current {
      if steps >= crate::heap::MAX_PROTOTYPE_CHAIN {
        return None;
      }
      steps += 1;

      match heap.object_get_own_property(obj, key) {
        Ok(Some(desc)) => match desc.kind {
          PropertyKind::Data {
            value: Value::String(s),
            ..
          } => return Some(s),
          // If an accessor is present, or a non-string data property shadows the prototype chain,
          // we can't format without invoking user code (or implementing full ToString), so bail.
          PropertyKind::Accessor { .. } | PropertyKind::Data { .. } => return None,
        },
        Ok(None) => current = heap.object_prototype(obj).ok().flatten(),
        Err(_) => return None,
      }
    }
    None
  }
}

/// Formats a termination error into a stable, host-visible string, including stack frames.
pub fn format_termination(term: &Termination) -> String {
  let reason = match term.reason {
    crate::TerminationReason::OutOfFuel => "execution terminated: out of fuel",
    crate::TerminationReason::DeadlineExceeded => "execution terminated: deadline exceeded",
    crate::TerminationReason::Interrupted => "execution terminated: interrupted",
    crate::TerminationReason::OutOfMemory => "execution terminated: out of memory",
    crate::TerminationReason::StackOverflow => "execution terminated: stack overflow",
  };

  if term.stack.is_empty() {
    return string_from_str_best_effort(reason);
  }

  let stack_trace = format_stack_trace(&term.stack);
  if stack_trace.is_empty() {
    return string_from_str_best_effort(reason);
  }

  let mut out = String::new();
  if !push_str_best_effort(&mut out, reason) {
    return String::new();
  }
  if !push_char_best_effort(&mut out, '\n') {
    return String::new();
  }
  let _ = push_str_best_effort(&mut out, &stack_trace);
  out
}

#[cfg(test)]
mod error_report_tests {
  use crate::test_alloc::{FailAllocsGuard, FailNextMatchingAllocGuard};
  use crate::{Budget, HeapLimits, TerminationReason, Value, VmError, VmOptions};
  use std::sync::Arc;

  use super::{Agent, VmErrorReport};

  fn new_agent() -> Agent {
    Agent::with_options(
      VmOptions::default(),
      HeapLimits::new(1024 * 1024, 1024 * 1024),
    )
    .expect("create agent")
  }

  #[test]
  fn agent_error_report_throw_error_object_includes_message_and_stack() {
    let mut agent = new_agent();

    let err = agent
      .run_script(
        "throw.js",
        "function a() { throw new TypeError('boom'); }\na();",
        Budget::unlimited(1),
        None,
      )
      .unwrap_err();

    let report: VmErrorReport = agent.error_report(&err);
    assert_eq!(report.kind, "throw");
    assert!(
      report.message.contains("TypeError") && report.message.contains("boom"),
      "expected message to contain error name and message, got: {:?}",
      report.message
    );
    assert_eq!(
      report.exception_name.as_deref(),
      Some("TypeError"),
      "expected exception_name"
    );
    assert_eq!(
      report.exception_message.as_deref(),
      Some("boom"),
      "expected exception_message"
    );
    assert!(
      !report.stack.is_empty(),
      "expected stack frames for thrown error, got empty stack"
    );
    assert!(
      report.stack.iter().any(|f| &*f.source == "throw.js"),
      "expected stack to reference source name, got: {:#?}",
      report.stack
    );
    assert_eq!(report.termination_reason, None);
  }

  #[test]
  fn agent_error_report_termination_includes_reason() {
    let mut agent = new_agent();

    let err = agent
      .run_script(
        "fuel.js",
        "1",
        Budget {
          fuel: Some(0),
          deadline: None,
          check_time_every: 1,
        },
        None,
      )
      .unwrap_err();

    let report = agent.error_report(&err);
    assert_eq!(report.kind, "termination");
    assert_eq!(
      report.termination_reason,
      Some(TerminationReason::OutOfFuel)
    );
  }

  #[test]
  fn agent_error_report_termination_includes_stack_frames() {
    let mut agent = new_agent();

    let err = agent
      .run_script(
        "fuel_stack.js",
        "1",
        Budget {
          // Parsing consumes at least one tick; use a budget that allows parsing to complete but
          // terminates at script entry so the termination captures the script frame.
          fuel: Some(1),
          deadline: None,
          check_time_every: 1,
        },
        None,
      )
      .unwrap_err();

    let report = agent.error_report(&err);
    assert_eq!(report.kind, "termination");
    assert_eq!(
      report.termination_reason,
      Some(TerminationReason::OutOfFuel)
    );
    assert!(
      !report.stack.is_empty(),
      "expected termination report to include captured stack frames"
    );
    assert_eq!(&*report.stack[0].source, "fuel_stack.js");
  }

  #[test]
  fn agent_error_report_internal_error_kinds() {
    let mut agent = new_agent();

    let invalid = VmError::invalid_handle();
    let report = agent.error_report(&invalid);
    assert_eq!(report.kind, "invalid_handle");

    let inv = VmError::InvariantViolation("boom");
    let report = agent.error_report(&inv);
    assert_eq!(report.kind, "invariant_violation");
  }

  #[test]
  fn agent_error_report_syntax_includes_diagnostic_message() {
    let mut agent = new_agent();

    let err = agent
      .run_script("bad.js", "function {", Budget::unlimited(1), None)
      .unwrap_err();
    let report = agent.error_report(&err);
    assert_eq!(report.kind, "syntax");
    assert!(
      report.message.starts_with("syntax error"),
      "expected syntax message prefix, got: {:?}",
      report.message
    );
    assert_ne!(
      report.message, "syntax error",
      "expected syntax report to include diagnostic details"
    );
  }

  #[test]
  fn agent_error_report_does_not_invoke_error_name_getters() {
    let mut agent = new_agent();

    // Install an accessor on `%TypeError.prototype%.name` that would mutate global state if invoked.
    // Host-side error reporting must not run arbitrary JS (no getters / no Proxy traps).
    let err = agent
      .run_script(
        "error_name_getter.js",
        r#"
var called = 0;
Object.defineProperty(TypeError.prototype, "name", {
  get: function () { called++; return "TypeError"; }
});
throw new TypeError("boom");
"#,
        Budget::unlimited(1),
        None,
      )
      .unwrap_err();

    // Generate the structured report on the host. This must not invoke the getter above.
    let _ = agent.error_report(&err);

    let called = agent
      .run_script("called.js", "called", Budget::unlimited(1), None)
      .expect("script should run");
    assert_eq!(called, Value::Number(0.0));
  }

  #[test]
  fn agent_error_report_is_best_effort_under_host_oom() {
    let mut agent = new_agent();

    let err = agent
      .run_script(
        "throw.js",
        "throw new TypeError('boom');",
        Budget::unlimited(1),
        None,
      )
      .unwrap_err();

    // Force all host allocations to fail while formatting the report. This should not panic or
    // abort, and should return a partial/empty report.
    let _guard = FailAllocsGuard::new();
    let report = agent.error_report(&err);
    assert_eq!(report.kind, "throw");
    assert!(report.message.is_empty());
    assert!(report.exception_name.is_none());
    assert!(report.exception_message.is_none());
    assert!(report.stack.is_empty());
  }

  #[test]
  fn clone_stack_best_effort_falls_back_to_incremental_allocation() {
    // Ensure `clone_stack_best_effort` can still produce a stack even when the initial
    // `try_reserve_exact` fails.
    let frames = vec![
      crate::StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("a.js"),
        line: 1,
        col: 1,
      },
      crate::StackFrame {
        function: None,
        source: Arc::<str>::from("b.js"),
        line: 2,
        col: 3,
      },
      crate::StackFrame {
        function: Some(Arc::<str>::from("g")),
        source: Arc::<str>::from("c.js"),
        line: 4,
        col: 5,
      },
    ];

    let size = core::mem::size_of::<crate::StackFrame>() * frames.len();
    let align = core::mem::align_of::<crate::StackFrame>();
    let _guard = FailNextMatchingAllocGuard::new(size, align);

    let cloned = super::clone_stack_best_effort(&frames);
    assert_eq!(cloned, frames);
  }
}
