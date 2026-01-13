use crate::source::format_stack_trace;
use crate::property::{PropertyKey, PropertyKind};
use crate::{Budget, Heap, HeapLimits, JsRuntime, Realm, SourceText, Termination, Value, Vm, VmError, VmOptions};
use std::sync::Arc;

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
  pub fn run_script(
    &mut self,
    source_name: impl Into<Arc<str>>,
    source_text: impl Into<Arc<str>>,
    budget: Budget,
    mut host_hooks: Option<&mut dyn HostHooks>,
  ) -> Result<Value, VmError> {
    let source = Arc::new(SourceText::new_charged(self.heap_mut(), source_name, source_text)?);

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
    result
  }

  /// Convert a JavaScript value into a host-owned string for exception reporting.
  ///
  /// This uses the VM's `ToString` implementation (via [`Heap::to_string`]).
  pub fn value_to_error_string(&mut self, value: Value) -> String {
    let s = match self.heap_mut().to_string(value) {
      Ok(s) => s,
      // If ToString itself throws, format the thrown value.
      Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => {
        return self.value_to_error_string(v);
      }
      Err(_) => {
        let mut out = String::new();
        // Best-effort debug formatting; ignore fmt errors (only possible on OOM).
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{value:?}"));
        return out;
      }
    };

    self
      .heap()
      .get_string(s)
      .map(|js| js.to_utf8_lossy())
      .unwrap_or_else(|_| String::from("<invalid string>"))
  }

  /// Formats a VM error into a host-visible string.
  pub fn format_vm_error(&mut self, err: &VmError) -> String {
    match err {
      VmError::Throw(value) => self.format_thrown_value(*value),
      VmError::ThrowWithStack { value, stack } => {
        let msg = self.format_thrown_value(*value);
        if stack.is_empty() {
          msg
        } else {
          let stack_trace = format_stack_trace(stack);
          let mut out = msg;
          out.push('\n');
          out.push_str(&stack_trace);
          out
        }
      }
      VmError::Termination(term) => format_termination(term),
      other => {
        let mut out = String::new();
        // Best-effort formatting; ignore fmt errors (only possible on OOM).
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{other}"));
        out
      }
    }
  }

  fn format_thrown_value(&mut self, value: Value) -> String {
    match value {
      Value::Object(obj) if self.heap().is_error_object(obj) => self
        .try_format_error_object_message(obj)
        .unwrap_or_else(|| self.value_to_error_string(value)),
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

    let name_key = PropertyKey::from_string(scope.common_key_name().ok()?);
    let message_key = PropertyKey::from_string(scope.common_key_message().ok()?);

    let heap = scope.heap();

    let name = Self::get_data_string_property_from_chain(heap, obj, &name_key)
      .and_then(|s| heap.get_string(s).ok())
      .and_then(|js| crate::string::utf16_to_utf8_lossy(js.as_code_units()).ok());
    let message = Self::get_data_string_property_from_chain(heap, obj, &message_key)
      .and_then(|s| heap.get_string(s).ok())
      .and_then(|js| crate::string::utf16_to_utf8_lossy(js.as_code_units()).ok());

    let (name, message) = match (name, message) {
      (Some(name), Some(message)) => (name, message),
      (None, Some(message)) => {
        // If we can extract only a string `message`, prefer it over a Rust debug dump.
        return Some(message);
      }
      _ => return None,
    };

    if message.is_empty() {
      return Some(name);
    }

    let mut out = String::new();
    let needed = name.len().saturating_add(2).saturating_add(message.len());
    out.try_reserve_exact(needed).ok()?;
    out.push_str(&name);
    out.push_str(": ");
    out.push_str(&message);
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
  if term.stack.is_empty() {
    let mut out = String::new();
    // Best-effort formatting; ignore fmt errors (only possible on OOM).
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{term}"));
    out
  } else {
    let stack_trace = format_stack_trace(&term.stack);
    let mut out = String::new();
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{term}"));
    out.push('\n');
    out.push_str(&stack_trace);
    out
  }
}
