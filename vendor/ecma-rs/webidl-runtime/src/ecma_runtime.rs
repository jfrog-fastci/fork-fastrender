use crate::runtime::{
  InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind, JsRuntime,
  NativeHostFunction, WebIdlBindingsRuntime, WebIdlHooks, WebIdlJsRuntime, WebIdlLimits,
};
use std::any::TypeId;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;
use vm_js::{
  GcBigInt, GcObject, GcString, GcSymbol, Heap, HeapLimits, JsBigInt, NativeFunctionId,
  PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError, WeakGcObject,
};
use webidl_vm_js::{CallbackHandle, CallbackKind};

mod ecma_webidl;
pub use ecma_webidl::VmJsWebIdlCx;

type HostFn = Rc<dyn Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError>>;

fn is_ecma_whitespace(c: char) -> bool {
  // ECMA-262 WhiteSpace + LineTerminator code points (used by `TrimString` / `StringToNumber`).
  matches!(
    c,
    '\u{0009}' // Tab
      | '\u{000A}' // LF
      | '\u{000B}' // VT
      | '\u{000C}' // FF
      | '\u{000D}' // CR
      | '\u{0020}' // Space
      | '\u{00A0}' // No-break space
      | '\u{1680}' // Ogham space mark
      | '\u{2000}'
      ..='\u{200A}' // En quad..hair space
      | '\u{2028}' // Line separator
      | '\u{2029}' // Paragraph separator
      | '\u{202F}' // Narrow no-break space
      | '\u{205F}' // Medium mathematical space
      | '\u{3000}' // Ideographic space
      | '\u{FEFF}' // BOM
  )
}

fn parse_integer_radix_to_f64(digits: &str, radix: u32) -> Option<f64> {
  fn ascii_digit_value(b: u8) -> Option<u32> {
    match b {
      b'0'..=b'9' => Some((b - b'0') as u32),
      b'a'..=b'z' => Some((b - b'a') as u32 + 10),
      b'A'..=b'Z' => Some((b - b'A') as u32 + 10),
      _ => None,
    }
  }

  if digits.is_empty() {
    return None;
  }
  let radix_f: f64 = radix as f64;
  let mut value: f64 = 0.0;
  // ECMA-262's NonDecimalIntegerLiteral grammar only accepts ASCII digits/letters. Avoid `char::to_digit`
  // so we don't accidentally accept other Unicode digit characters.
  for b in digits.bytes() {
    let digit = ascii_digit_value(b)?;
    if digit >= radix {
      return None;
    }
    value = value.mul_add(radix_f, digit as f64);
  }
  Some(value)
}

#[derive(Clone)]
enum HostObjectKind {
  PlatformObject {
    primary_interface: &'static str,
    implements: &'static [&'static str],
    opaque: u64,
  },
  Function(HostFn),
  Error {
    name: &'static str,
  },
  // Built-in internal-slot stubs (legacy fallback for embeddings that need heap-only objects).
  //
  // Note: `vm-js` now provides real `ArrayBuffer` / `DataView` / typed array heap object kinds, so
  // the runtime prefers using `Heap` internal-slot checks. These variants remain as a migration
  // escape hatch for unit tests or hosts that still fabricate buffer-like objects without the
  // `vm-js` builtins.
  #[allow(dead_code)]
  ArrayBuffer {
    shared: bool,
  },
  #[allow(dead_code)]
  DataView,
  #[allow(dead_code)]
  TypedArray {
    name: &'static str,
  },
}

/// A concrete [`WebIdlJsRuntime`] implementation backed by `ecma-rs`'s `vm-js` value types.
///
/// `vm-js` is currently a GC/value/interrupt scaffolding crate. For Web IDL conversions and
/// overload resolution we only need a small subset of ECMAScript abstract operations and an
/// object/property model that host bindings can use. This runtime provides that adapter layer.
pub struct VmJsRuntime {
  heap: Heap,
  /// Host-side metadata for objects that need special behaviour (callability, platform object
  /// branding, internal-slot stubs, etc). Keyed by `WeakGcObject` so it does not keep wrappers alive.
  host_objects: HashMap<WeakGcObject, HostObjectKind>,
  global_object: Option<GcObject>,
  webidl_limits: WebIdlLimits,
  well_known_iterator: Option<GcSymbol>,
  well_known_async_iterator: Option<GcSymbol>,

  // Internal-slot emulation for `ToObject` wrapper objects.
  //
  // These are stored as hidden, non-enumerable symbol-keyed data properties so the heap GC traces
  // the stored values without requiring host-side tracing hooks.
  string_data_symbol: Option<GcSymbol>,
  boolean_data_symbol: Option<GcSymbol>,
  number_data_symbol: Option<GcSymbol>,
  bigint_data_symbol: Option<GcSymbol>,
  symbol_data_symbol: Option<GcSymbol>,

  last_swept_gc_runs: u64,

  bindings_host_ptr: Option<NonNull<()>>,
  bindings_host_type_id: Option<TypeId>,

  // Explicit string interning for cases where host code needs stable `Value::String` identity.
  //
  // `vm-js` `Value` equality compares string handles (not contents). Embeddings that use strings as
  // sentinels for JS-visible identity must opt in to interning; regular string allocation does not
  // intern by default.
  interned_strings: HashMap<String, GcString>,
}

impl VmJsRuntime {
  fn value_is_valid_or_primitive(&self, value: Value) -> bool {
    match value {
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) | Value::BigInt(_) => true,
      Value::String(s) => self.heap.is_valid_string(s),
      Value::Symbol(s) => self.heap.is_valid_symbol(s),
      Value::Object(o) => self.heap.is_valid_object(o),
    }
  }

  /// Creates a new runtime with conservative heap limits.
  pub fn new() -> Self {
    // Defaults should be conservative; real embeddings should plumb in renderer budgets via
    // `VmJsRuntime::with_limits`.
    Self::with_limits(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024))
  }

  pub fn with_limits(limits: HeapLimits) -> Self {
    Self {
      heap: Heap::new(limits),
      host_objects: HashMap::new(),
      global_object: None,
      webidl_limits: WebIdlLimits::default(),
      well_known_iterator: None,
      well_known_async_iterator: None,
      string_data_symbol: None,
      boolean_data_symbol: None,
      number_data_symbol: None,
      bigint_data_symbol: None,
      symbol_data_symbol: None,
      last_swept_gc_runs: 0,
      bindings_host_ptr: None,
      bindings_host_type_id: None,
      interned_strings: HashMap::new(),
    }
  }

  /// Run `f` with a temporary `&mut Host` available to values created via
  /// [`WebIdlBindingsRuntime::create_function`].
  ///
  /// This is primarily intended for unit tests and host-driven calls. Embeddings that execute JS
  /// code should ensure they establish an equivalent host context before invoking JS functions that
  /// were created via `create_function`.
  pub fn with_host_context<Host: 'static, R>(
    &mut self,
    host: &mut Host,
    f: impl FnOnce(&mut Self) -> Result<R, VmError>,
  ) -> Result<R, VmError> {
    let host_type_id = TypeId::of::<Host>();
    if let Some(existing) = self.bindings_host_type_id {
      if existing != host_type_id {
        return Err(
          self.throw_type_error("VmJsRuntime host context type mismatch for WebIDL bindings"),
        );
      }
    } else {
      self.bindings_host_type_id = Some(host_type_id);
    }

    let prev = self.bindings_host_ptr;
    self.bindings_host_ptr = Some(NonNull::from(host).cast());

    let out = f(self);

    self.bindings_host_ptr = prev;
    out
  }

  /// Runs `f` with a `webidl::JsRuntime` conversion context.
  ///
  /// The `vendor/ecma-rs/webidl` conversions and overload resolution helpers can keep `vm-js` GC
  /// handles alive across multiple allocations. Since this legacy runtime does not execute real JS
  /// code (and therefore has no VM stack), callers must use an explicit conversion context that
  /// roots produced/consumed values for the duration of the conversion.
  ///
  /// Note: some WebIDL algorithms allocate *before* they first pass the input value back into the
  /// runtime (for example when fetching well-known symbols). If the caller is holding `vm-js` GC
  /// handles that are not otherwise reachable from the GC root set, those values must be rooted for
  /// the duration of the conversion (e.g. via [`VmJsRuntime::with_webidl_cx_rooted`] or the legacy
  /// [`JsRuntime::with_stack_roots`] helper).
  #[inline]
  pub fn with_webidl_cx<R>(&mut self, f: impl FnOnce(&mut VmJsWebIdlCx<'_>) -> R) -> R {
    let mut cx = VmJsWebIdlCx::new(self);
    f(&mut cx)
  }

  /// Runs `f` with a `webidl::JsRuntime` conversion context while treating `roots` as stack GC roots.
  ///
  /// This is a convenience wrapper around [`JsRuntime::with_stack_roots`] and
  /// [`VmJsRuntime::with_webidl_cx`]. It is typically what host code wants when calling into the
  /// `webidl` crate with handles that are not otherwise reachable from the VM root set.
  #[inline]
  pub fn with_webidl_cx_rooted<R>(
    &mut self,
    roots: &[Value],
    f: impl FnOnce(&mut VmJsWebIdlCx<'_>) -> R,
  ) -> Result<R, VmError> {
    <Self as JsRuntime>::with_stack_roots(self, roots, |rt| Ok(rt.with_webidl_cx(f)))
  }

  pub fn heap(&self) -> &Heap {
    &self.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.heap
  }

  pub fn set_webidl_limits(&mut self, limits: WebIdlLimits) {
    self.webidl_limits = limits;
  }

  fn sweep_dead_host_objects_if_needed(&mut self) {
    let gc_runs = self.heap.gc_runs();
    if gc_runs == self.last_swept_gc_runs {
      return;
    }
    self.last_swept_gc_runs = gc_runs;

    let heap = &self.heap;
    self
      .host_objects
      .retain(|weak, _| weak.upgrade(heap).is_some());
  }

  fn with_stack_roots<R>(
    &mut self,
    roots: impl IntoIterator<Item = Value>,
    f: impl FnOnce(&mut Self) -> Result<R, VmError>,
  ) -> Result<R, VmError> {
    // We can't hold a `Scope` guard across `f(self)` because `f` needs mutable access to the heap
    // (and may create its own scopes).
    //
    // Instead, explicitly push stack roots and truncate them after `f` returns.
    let base_len = self.heap.stack_root_len();

    // Collect roots so we can push them onto the `vm-js` root stack in a single operation.
    //
    // This ensures *all* pending roots are treated as roots if growing the root stack triggers a
    // GC. Pushing one-by-one is incorrect under extreme GC pressure: GC could collect not-yet-pushed
    // values between individual `push_stack_root` calls.
    // Allocate the temporary root list fallibly so hostile input cannot abort the host process on
    // allocator OOM.
    let mut roots_vec: Vec<Value> = Vec::new();
    let iter = roots.into_iter();
    let (lower, upper) = iter.size_hint();
    let reserve = upper.unwrap_or(lower);
    if reserve != 0 {
      roots_vec
        .try_reserve_exact(reserve)
        .map_err(|_| VmError::OutOfMemory)?;
    }
    for v in iter {
      if roots_vec.len() == roots_vec.capacity() {
        roots_vec
          .try_reserve_exact(1)
          .map_err(|_| VmError::OutOfMemory)?;
      }
      roots_vec.push(v);
    }

    // `vm-js` only debug-asserts root validity when pushing stack roots. Ensure we return an error
    // in release builds rather than silently enqueuing stale handles.
    for &v in &roots_vec {
      if !self.value_is_valid_or_primitive(v) {
        self.heap.truncate_stack_roots(base_len);
        return Err(VmError::invalid_handle());
      }
    }

    if let Err(err) = self.heap.push_stack_roots(&roots_vec) {
      self.heap.truncate_stack_roots(base_len);
      return Err(err);
    }

    let result = f(self);
    self.heap.truncate_stack_roots(base_len);
    result
  }

  fn alloc_string_handle(&mut self, s: &str) -> Result<GcString, VmError> {
    let mut scope = self.heap.scope();
    scope.alloc_string(s)
  }

  /// Creates a string [`PropertyKey`] from a Rust `&str`.
  ///
  /// This is a convenience for embeddings that want to access properties by name without having to
  /// allocate a JS string value first.
  pub fn prop_key_str(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    self.property_key_from_str(s)
  }

  fn internal_symbol(
    slot: &mut Option<GcSymbol>,
    heap: &mut Heap,
    key: GcString,
  ) -> Result<GcSymbol, VmError> {
    if let Some(sym) = *slot {
      return Ok(sym);
    }
    let sym = heap.symbol_for(key)?;
    *slot = Some(sym);
    Ok(sym)
  }

  fn string_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[StringData]]")?;
    Self::internal_symbol(&mut self.string_data_symbol, &mut self.heap, key)
  }

  fn boolean_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[BooleanData]]")?;
    Self::internal_symbol(&mut self.boolean_data_symbol, &mut self.heap, key)
  }

  fn number_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[NumberData]]")?;
    Self::internal_symbol(&mut self.number_data_symbol, &mut self.heap, key)
  }

  fn bigint_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[BigIntData]]")?;
    Self::internal_symbol(&mut self.bigint_data_symbol, &mut self.heap, key)
  }

  fn symbol_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[SymbolData]]")?;
    Self::internal_symbol(&mut self.symbol_data_symbol, &mut self.heap, key)
  }

  fn is_internal_key(&self, key: &PropertyKey) -> bool {
    let PropertyKey::Symbol(sym) = key else {
      return false;
    };
    self.string_data_symbol == Some(*sym)
      || self.boolean_data_symbol == Some(*sym)
      || self.number_data_symbol == Some(*sym)
      || self.bigint_data_symbol == Some(*sym)
      || self.symbol_data_symbol == Some(*sym)
  }

  /// Create a property key for the given string.
  ///
  /// This interns and roots the underlying string so the returned key remains valid even if the
  /// heap later runs GC.
  pub fn prop_key(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    self.prop_key_str(s)
  }

  /// Interns a JS string and returns a stable `Value::String` handle.
  ///
  /// `vm-js` `Value` equality compares string handles (not string contents). Most runtime code
  /// should continue to use [`VmJsRuntime::alloc_string_value`], but when host code needs a stable
  /// JS-visible identity sentinel it can opt into interning via this method.
  ///
  /// Note: interned strings are rooted for the lifetime of the runtime. Callers should only intern
  /// a small, bounded set of fixed strings.
  pub fn intern_string_value(&mut self, s: &str) -> Result<Value, VmError> {
    if let Some(handle) = self.interned_strings.get(s).copied() {
      return Ok(Value::String(handle));
    }

    // Ensure we can insert into the intern table without risking an OOM abort during `insert`.
    self
      .interned_strings
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    let mut key = String::new();
    key.try_reserve(s.len()).map_err(|_| VmError::OutOfMemory)?;
    key.push_str(s);

    let handle = self.alloc_string_handle(&key)?;
    // Keep this handle alive for the lifetime of the runtime.
    let _ = self.heap.add_root(Value::String(handle))?;
    self.interned_strings.insert(key, handle);
    Ok(Value::String(handle))
  }

  pub fn alloc_string_value(&mut self, s: &str) -> Result<Value, VmError> {
    Ok(Value::String(self.alloc_string_handle(s)?))
  }

  pub fn alloc_object_value(&mut self) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    Ok(Value::Object(obj))
  }

  pub fn alloc_platform_object_value(
    &mut self,
    primary_interface: &'static str,
    implements: &'static [&'static str],
    opaque: u64,
  ) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.host_objects.insert(
      WeakGcObject::from(obj),
      HostObjectKind::PlatformObject {
        primary_interface,
        implements,
        opaque,
      },
    );
    Ok(Value::Object(obj))
  }

  pub fn platform_object_opaque(&self, v: Value) -> Option<u64> {
    let Value::Object(obj) = v else {
      return None;
    };
    if !self.heap.is_valid_object(obj) {
      return None;
    }
    match self.host_objects.get(&WeakGcObject::from(obj))? {
      HostObjectKind::PlatformObject { opaque, .. } => Some(*opaque),
      _ => None,
    }
  }

  pub fn platform_object_primary_interface(&self, v: Value) -> Option<&'static str> {
    let Value::Object(obj) = v else {
      return None;
    };
    if !self.heap.is_valid_object(obj) {
      return None;
    }
    match self.host_objects.get(&WeakGcObject::from(obj))? {
      HostObjectKind::PlatformObject {
        primary_interface, ..
      } => Some(*primary_interface),
      _ => None,
    }
  }

  pub fn implements_interface(&self, v: Value, interface: &str) -> bool {
    <Self as WebIdlHooks<Value>>::implements_interface(
      self,
      v,
      crate::interface_id_from_name(interface),
    )
  }

  pub fn set_prototype(&mut self, obj: Value, proto: Option<Value>) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("set_prototype: receiver is not an object"));
    };
    let proto_obj = match proto {
      None => None,
      Some(Value::Object(p)) => Some(p),
      Some(_) => return Err(self.throw_type_error("set_prototype: prototype is not an object")),
    };
    self.heap.object_set_prototype(obj, proto_obj)
  }

  pub fn define_accessor_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
    get: Value,
    set: Value,
    enumerable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_accessor_property: receiver is not an object"));
    };

    let desc = PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: PropertyKind::Accessor { get, set },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)
  }

  fn define_hidden_slot(
    &mut self,
    obj: GcObject,
    sym: GcSymbol,
    value: Value,
  ) -> Result<(), VmError> {
    let desc = PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value,
        writable: false,
      },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, PropertyKey::Symbol(sym), desc)
  }

  pub fn alloc_string_object_value(&mut self, s: &str) -> Result<Value, VmError> {
    let string_data = self.alloc_string_handle(s)?;
    self.alloc_string_object_from_handle(string_data)
  }

  fn alloc_string_object_from_handle(&mut self, string_data: GcString) -> Result<Value, VmError> {
    // Root `string_data` before allocating the hidden-slot symbol. Under extreme GC pressure (e.g.
    // `HeapLimits::gc_threshold = 0`), any heap allocation can trigger collection, and the input
    // `GcString` handle is otherwise invisible to the GC.
    self.with_stack_roots([Value::String(string_data)], |rt| {
      let sym = rt.string_data_symbol()?;
      let obj = {
        let mut scope = rt.heap.scope();
        scope.alloc_object()?
      };
      rt.define_hidden_slot(obj, sym, Value::String(string_data))?;
      Ok(Value::Object(obj))
    })
  }

  fn alloc_boolean_object_value(&mut self, boolean_data: bool) -> Result<Value, VmError> {
    let sym = self.boolean_data_symbol()?;
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.define_hidden_slot(obj, sym, Value::Bool(boolean_data))?;
    Ok(Value::Object(obj))
  }

  fn alloc_number_object_value(&mut self, number_data: f64) -> Result<Value, VmError> {
    let sym = self.number_data_symbol()?;
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.define_hidden_slot(obj, sym, Value::Number(number_data))?;
    Ok(Value::Object(obj))
  }

  fn alloc_bigint_object_value(&mut self, bigint_data: GcBigInt) -> Result<Value, VmError> {
    // Root `bigint_data` before allocating the hidden-slot symbol (see `alloc_string_object_from_handle`).
    self.with_stack_roots([Value::BigInt(bigint_data)], |rt| {
      let sym = rt.bigint_data_symbol()?;
      let obj = {
        let mut scope = rt.heap.scope();
        scope.alloc_object()?
      };
      rt.define_hidden_slot(obj, sym, Value::BigInt(bigint_data))?;
      Ok(Value::Object(obj))
    })
  }

  fn alloc_symbol_object_value(&mut self, symbol_data: GcSymbol) -> Result<Value, VmError> {
    // Root `symbol_data` before allocating the hidden-slot symbol (see `alloc_string_object_from_handle`).
    self.with_stack_roots([Value::Symbol(symbol_data)], |rt| {
      let sym = rt.symbol_data_symbol()?;
      let obj = {
        let mut scope = rt.heap.scope();
        scope.alloc_object()?
      };
      rt.define_hidden_slot(obj, sym, Value::Symbol(symbol_data))?;
      Ok(Value::Object(obj))
    })
  }

  pub fn alloc_function_value<F>(&mut self, f: F) -> Result<Value, VmError>
  where
    F: Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError> + 'static,
  {
    self.alloc_function_value_with_name_length("host", 0, f)
  }

  pub fn alloc_function_value_with_name_length<F>(
    &mut self,
    name: &str,
    length: u32,
    f: F,
  ) -> Result<Value, VmError>
  where
    F: Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError> + 'static,
  {
    // Allocate a real `vm-js` function object so callers can treat it like a normal JS Function:
    // - it has `[[Call]]` so `typeof`/callability checks in `vm-js` can evolve naturally, and
    // - it participates in the regular object/prototype/property APIs.
    //
    // `VmJsRuntime` still dispatches calls via `host_objects` (not via `vm-js::Vm` native call
    // tables), so we use a dummy `NativeFunctionId` here.
    let obj = {
      let mut scope = self.heap.scope();
      let name = scope.alloc_string(name)?;
      scope.alloc_native_function(NativeFunctionId(0), None, name, length)?
    };
    self.host_objects.insert(
      WeakGcObject::from(obj),
      HostObjectKind::Function(Rc::new(f)),
    );
    Ok(Value::Object(obj))
  }

  fn call_internal(
    &mut self,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self.sweep_dead_host_objects_if_needed();
    let Value::Object(func) = callee else {
      return Err(self.throw_type_error("value is not callable"));
    };
    if !self.heap.is_valid_object(func) {
      return Err(VmError::invalid_handle());
    }
    let Some(HostObjectKind::Function(f)) = self.host_objects.get(&WeakGcObject::from(func)) else {
      return Err(self.throw_type_error("value is not callable"));
    };

    let f = f.clone();
    self.with_stack_roots(
      std::iter::once(callee)
        .chain(std::iter::once(this))
        .chain(args.iter().copied()),
      |rt| f(rt, this, args),
    )
  }

  /// Call a JS function value.
  ///
  /// This currently only supports host-defined function objects created via
  /// [`VmJsRuntime::alloc_function_value`]. It is sufficient for early host integration plumbing
  /// while the full `vm-js` interpreter is still under development.
  pub fn call_function(
    &mut self,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    <Self as JsRuntime>::call(self, callee, this, args)
  }

  fn to_number_from_string(&self, s: GcString) -> Result<f64, VmError> {
    let js = self.heap.get_string(s)?;
    // Converting JS UTF-16 strings to Rust UTF-8 `String` allocates in the host. Keep this bounded
    // to avoid large host allocations even when the VM heap is capped.
    //
    // For oversized strings, fall back to `NaN` instead of throwing: ECMAScript `ToNumber` on
    // strings does not throw.
    if js.len_code_units() > self.webidl_limits.max_string_code_units {
      return Ok(f64::NAN);
    }
    let text = js.to_utf8_lossy();
    let trimmed = text.trim_matches(is_ecma_whitespace);
    if trimmed.is_empty() {
      return Ok(0.0);
    }
    if trimmed.eq("Infinity") || trimmed.eq("+Infinity") {
      return Ok(f64::INFINITY);
    }
    if trimmed.eq("-Infinity") {
      return Ok(f64::NEG_INFINITY);
    }
    // Hex/binary/octal integer literals (ToNumber per ECMA-262).
    //
    // Note: StringToNumber does *not* accept a leading sign for these forms:
    // `Number("-0x10")` is `NaN` (use `parseInt` for signed radix parsing).
    let (has_sign, rest) = if let Some(rest) = trimmed.strip_prefix('-') {
      (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
      (true, rest)
    } else {
      (false, trimmed)
    };
    if has_sign {
      if rest.starts_with("0x")
        || rest.starts_with("0X")
        || rest.starts_with("0b")
        || rest.starts_with("0B")
        || rest.starts_with("0o")
        || rest.starts_with("0O")
      {
        return Ok(f64::NAN);
      }
    }

    if let Some(rest) = trimmed
      .strip_prefix("0x")
      .or_else(|| trimmed.strip_prefix("0X"))
    {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Some(v) = parse_integer_radix_to_f64(rest, 16) {
        return Ok(v);
      }
      return Ok(f64::NAN);
    }
    if let Some(rest) = trimmed
      .strip_prefix("0b")
      .or_else(|| trimmed.strip_prefix("0B"))
    {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Some(v) = parse_integer_radix_to_f64(rest, 2) {
        return Ok(v);
      }
      return Ok(f64::NAN);
    }
    if let Some(rest) = trimmed
      .strip_prefix("0o")
      .or_else(|| trimmed.strip_prefix("0O"))
    {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Some(v) = parse_integer_radix_to_f64(rest, 8) {
        return Ok(v);
      }
      return Ok(f64::NAN);
    }

    // Rust's `f64::from_str` accepts spellings like "inf"/"infinity" (case-insensitive), which are
    // *not* valid under ECMA-262 `StringToNumber`. For decimal numeric strings, the only permitted
    // alphabetic character is the exponent marker `e`/`E`. Reject anything else before delegating
    // to Rust's parser so `Number("inf")` correctly yields NaN.
    if trimmed
      .as_bytes()
      .iter()
      .any(|b| b.is_ascii_alphabetic() && !matches!(b, b'e' | b'E'))
    {
      return Ok(f64::NAN);
    }

    match trimmed.parse::<f64>() {
      Ok(v) => Ok(v),
      Err(_) => Ok(f64::NAN),
    }
  }

  fn bigint_from_string(&mut self, s: GcString) -> Result<GcBigInt, VmError> {
    let js = self.heap.get_string(s)?;
    if js.len_code_units() > self.webidl_limits.max_string_code_units {
      return Err(self.throw_range_error("BigInt string exceeds maximum length"));
    }

    let units = js.as_code_units();
    let parsed = JsBigInt::parse_utf16_string_with_tick(units, &mut || Ok(()))?;
    let Some(bi) = parsed else {
      return Err(self.throw_syntax_error("Cannot convert string to a BigInt"));
    };

    let mut scope = self.heap.scope();
    scope.alloc_bigint(bi)
  }

  fn to_string_from_number(&mut self, n: f64) -> Result<GcString, VmError> {
    // Use `vm-js`'s spec-shaped `Number::toString` implementation instead of Rust's float formatting.
    // This matters for threshold behaviors like:
    // - `1e21` → `"1e+21"` (not `"1000000000000000000000"`)
    // - `1e-7` → `"1e-7"` (not `"0.0000001"`)
    self.heap.to_string(Value::Number(n))
  }

  fn create_error_object(&mut self, name: &'static str, message: &str) -> Value {
    let obj = match {
      let mut scope = self.heap.scope();
      scope.alloc_object()
    } {
      Ok(obj) => obj,
      Err(_) => return Value::Undefined,
    };

    // Record host-side metadata immediately. This does not keep the wrapper alive (keyed by
    // `WeakGcObject`), but lets us implement `Error.prototype.toString`-like behavior.
    self
      .host_objects
      .insert(WeakGcObject::from(obj), HostObjectKind::Error { name });

    // Under extreme GC pressure (e.g. tests that force a GC before every allocation), we must keep
    // the error object rooted while allocating its `name`/`message` strings and property keys.
    //
    // `vm-js` does not trace Rust locals, so an allocation for `message` can GC and collect `obj`
    // unless it is explicitly rooted.
    let Ok(message_handle) = self.with_stack_roots([Value::Object(obj)], |rt| {
      rt.alloc_string_handle(message)
        .or_else(|_| rt.alloc_string_handle("error"))
    }) else {
      return Value::Undefined;
    };

    let _ = self.with_stack_roots([Value::Object(obj), Value::String(message_handle)], |rt| {
      let name_value = rt.alloc_string(name)?;
      rt.with_stack_roots([name_value], |rt| {
        let name_key = rt.property_key_from_str("name")?;
        let name_key_value = match name_key {
          PropertyKey::String(s) => Some(Value::String(s)),
          PropertyKey::Symbol(s) => Some(Value::Symbol(s)),
        };
        rt.with_stack_roots(name_key_value, |rt| {
          rt.define_data_property(Value::Object(obj), name_key, name_value, false)
        })?;

        let message_key = rt.property_key_from_str("message")?;
        let message_key_value = match message_key {
          PropertyKey::String(s) => Some(Value::String(s)),
          PropertyKey::Symbol(s) => Some(Value::Symbol(s)),
        };
        rt.with_stack_roots(message_key_value, |rt| {
          rt.define_data_property(
            Value::Object(obj),
            message_key,
            Value::String(message_handle),
            false,
          )
        })?;
        Ok(())
      })
    });

    Value::Object(obj)
  }

  fn string_object_data(&self, obj: GcObject) -> Result<Option<GcString>, VmError> {
    let Some(sym) = self.string_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::String(s))) => Ok(Some(s)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn boolean_object_data(&self, obj: GcObject) -> Result<Option<bool>, VmError> {
    let Some(sym) = self.boolean_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::Bool(b))) => Ok(Some(b)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn number_object_data(&self, obj: GcObject) -> Result<Option<f64>, VmError> {
    let Some(sym) = self.number_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::Number(n))) => Ok(Some(n)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn bigint_object_data(&self, obj: GcObject) -> Result<Option<GcBigInt>, VmError> {
    let Some(sym) = self.bigint_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::BigInt(n))) => Ok(Some(n)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn symbol_object_data(&self, obj: GcObject) -> Result<Option<GcSymbol>, VmError> {
    let Some(sym) = self.symbol_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::Symbol(s))) => Ok(Some(s)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn throw_symbol_to_number(&mut self, symbol_data: GcSymbol) -> VmError {
    const MAX_DESC_CODE_UNITS: usize = 1024;
    let message = match self.heap.symbol_description(symbol_data) {
      Some(handle) => match self.heap.get_string(handle) {
        Ok(s) if s.len_code_units() <= MAX_DESC_CODE_UNITS => {
          format!(
            "Cannot convert a Symbol({}) value to a number",
            s.to_utf8_lossy()
          )
        }
        _ => "Cannot convert a Symbol value to a number".to_string(),
      },
      None => "Cannot convert a Symbol value to a number".to_string(),
    };
    self.throw_type_error(&message)
  }

  fn throw_symbol_to_string(&mut self, symbol_data: GcSymbol) -> VmError {
    const MAX_DESC_CODE_UNITS: usize = 1024;
    let message = match self.heap.symbol_description(symbol_data) {
      Some(handle) => match self.heap.get_string(handle) {
        Ok(s) if s.len_code_units() <= MAX_DESC_CODE_UNITS => {
          format!(
            "Cannot convert a Symbol({}) value to a string",
            s.to_utf8_lossy()
          )
        }
        _ => "Cannot convert a Symbol value to a string".to_string(),
      },
      None => "Cannot convert a Symbol value to a string".to_string(),
    };
    self.throw_type_error(&message)
  }

  fn throw_syntax_error(&mut self, message: &str) -> VmError {
    VmError::Throw(self.create_error_object("SyntaxError", message))
  }
}

impl WebIdlHooks<Value> for VmJsRuntime {
  fn is_platform_object(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::PlatformObject { .. })
    )
  }

  fn implements_interface(&self, value: Value, interface: InterfaceId) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    };
    let Some(HostObjectKind::PlatformObject {
      primary_interface,
      implements,
      ..
    }) = self.host_objects.get(&WeakGcObject::from(obj))
    else {
      return false;
    };
    crate::interface_id_from_name(primary_interface) == interface
      || implements
        .iter()
        .any(|name| crate::interface_id_from_name(name) == interface)
  }
}

impl JsRuntime for VmJsRuntime {
  type JsValue = Value;
  type PropertyKey = PropertyKey;
  type Error = VmError;

  fn with_stack_roots<R, F>(&mut self, roots: &[Value], f: F) -> Result<R, VmError>
  where
    F: FnOnce(&mut Self) -> Result<R, VmError>,
  {
    VmJsRuntime::with_stack_roots(self, roots.iter().copied(), f)
  }

  fn js_undefined(&self) -> Value {
    Value::Undefined
  }

  fn js_null(&self) -> Value {
    Value::Null
  }

  fn js_boolean(&self, value: bool) -> Value {
    Value::Bool(value)
  }

  fn js_number(&self, value: f64) -> Value {
    Value::Number(value)
  }

  fn alloc_string(&mut self, value: &str) -> Result<Value, VmError> {
    self.alloc_string_value(value)
  }

  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Value, VmError> {
    let handle = {
      let mut scope = self.heap.scope();
      scope.alloc_string_from_code_units(units)?
    };
    Ok(Value::String(handle))
  }

  fn is_undefined(&self, value: Value) -> bool {
    matches!(value, Value::Undefined)
  }

  fn is_null(&self, value: Value) -> bool {
    matches!(value, Value::Null)
  }

  fn with_string_code_units<R>(
    &mut self,
    string: Value,
    f: impl FnOnce(&[u16]) -> R,
  ) -> Result<R, VmError> {
    let handle = match string {
      Value::String(s) => s,
      Value::Object(obj) => self
        .string_object_data(obj)?
        .ok_or_else(|| self.throw_type_error("value is not a string"))?,
      _ => return Err(self.throw_type_error("value is not a string")),
    };
    let js = self.heap.get_string(handle)?;
    Ok(f(js.as_code_units()))
  }

  fn property_key_from_str(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::String(self.alloc_string_handle(s)?))
  }

  fn property_key_from_u32(&mut self, index: u32) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::String(
      self.alloc_string_handle(&index.to_string())?,
    ))
  }

  fn property_key_is_symbol(&self, key: PropertyKey) -> bool {
    matches!(key, PropertyKey::Symbol(_))
  }

  fn property_key_is_string(&self, key: PropertyKey) -> bool {
    matches!(key, PropertyKey::String(_))
  }

  fn property_key_to_js_string(&mut self, key: PropertyKey) -> Result<Value, VmError> {
    match key {
      PropertyKey::String(s) => Ok(Value::String(s)),
      PropertyKey::Symbol(_) => {
        Err(self.throw_type_error("Cannot convert a Symbol property key to a string"))
      }
    }
  }

  fn alloc_object(&mut self) -> Result<Value, VmError> {
    self.alloc_object_value()
  }

  fn alloc_array(&mut self) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_array(0)?
    };
    Ok(Value::Object(obj))
  }

  fn define_data_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
    value: Value,
    enumerable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_data_property: receiver is not an object"));
    };

    let desc = PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)
  }

  fn is_object(&self, value: Value) -> bool {
    matches!(value, Value::Object(_))
  }

  fn is_callable(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::Function(_))
    )
  }

  fn is_boolean(&self, value: Value) -> bool {
    matches!(value, Value::Bool(_))
  }

  fn is_number(&self, value: Value) -> bool {
    matches!(value, Value::Number(_))
  }

  fn is_bigint(&self, value: Value) -> bool {
    matches!(value, Value::BigInt(_))
  }

  fn is_string(&self, value: Value) -> bool {
    matches!(value, Value::String(_))
  }

  fn is_symbol(&self, value: Value) -> bool {
    matches!(value, Value::Symbol(_))
  }

  fn to_object(&mut self, value: Value) -> Result<Value, VmError> {
    match value {
      Value::Undefined | Value::Null => {
        Err(self.throw_type_error("ToObject: cannot convert null or undefined to object"))
      }
      Value::Object(_) => Ok(value),
      Value::String(string_data) => Ok(self.alloc_string_object_from_handle(string_data)?),
      Value::Bool(boolean_data) => Ok(self.alloc_boolean_object_value(boolean_data)?),
      Value::Number(number_data) => Ok(self.alloc_number_object_value(number_data)?),
      Value::BigInt(bigint_data) => Ok(self.alloc_bigint_object_value(bigint_data)?),
      Value::Symbol(symbol_data) => Ok(self.alloc_symbol_object_value(symbol_data)?),
    }
  }

  fn call(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, VmError> {
    self.call_internal(callee, this, args)
  }

  fn to_boolean(&mut self, value: Value) -> Result<bool, VmError> {
    Ok(match value {
      Value::Undefined | Value::Null => false,
      Value::Bool(b) => b,
      Value::Number(n) => !(n == 0.0 || n.is_nan()),
      Value::BigInt(n) => !self.heap.get_bigint(n)?.is_zero(),
      Value::String(s) => !self.heap.get_string(s)?.is_empty(),
      Value::Symbol(_) | Value::Object(_) => true,
    })
  }

  fn to_number(&mut self, value: Value) -> Result<f64, VmError> {
    Ok(match value {
      Value::Number(n) => n,
      Value::BigInt(_) => {
        return Err(self.throw_type_error("Cannot convert a BigInt value to a number"));
      }
      Value::Bool(b) => {
        if b {
          1.0
        } else {
          0.0
        }
      }
      Value::Null => 0.0,
      Value::Undefined => f64::NAN,
      Value::String(s) => self.to_number_from_string(s)?,
      Value::Symbol(_) => {
        return Err(self.throw_type_error("Cannot convert a Symbol value to a number"));
      }
      Value::Object(obj) => {
        if let Some(string_data) = self.string_object_data(obj)? {
          self.to_number_from_string(string_data)?
        } else if let Some(boolean_data) = self.boolean_object_data(obj)? {
          if boolean_data {
            1.0
          } else {
            0.0
          }
        } else if let Some(number_data) = self.number_object_data(obj)? {
          number_data
        } else if let Some(_) = self.bigint_object_data(obj)? {
          return Err(self.throw_type_error("Cannot convert a BigInt value to a number"));
        } else if let Some(symbol_data) = self.symbol_object_data(obj)? {
          return Err(self.throw_symbol_to_number(symbol_data));
        } else {
          f64::NAN
        }
      }
    })
  }

  fn to_string(&mut self, value: Value) -> Result<Value, VmError> {
    self.sweep_dead_host_objects_if_needed();

    self.with_stack_roots([value], |rt| {
      let s = match value {
        Value::String(s) => s,
        Value::Undefined => rt.alloc_string_handle("undefined")?,
        Value::Null => rt.alloc_string_handle("null")?,
        Value::Bool(true) => rt.alloc_string_handle("true")?,
        Value::Bool(false) => rt.alloc_string_handle("false")?,
        Value::Number(n) => rt.to_string_from_number(n)?,
        Value::BigInt(n) => {
          let bi = rt.heap.get_bigint(n)?;
          let est_units = bi.estimated_byte_len().saturating_mul(3).saturating_add(1);
          if est_units > rt.webidl_limits.max_string_code_units {
            return Err(rt.throw_range_error("BigInt string exceeds maximum length"));
          }
          let s = bi.to_string_radix_with_tick(10, &mut || Ok(()))?;
          if s.len() > rt.webidl_limits.max_string_code_units {
            return Err(rt.throw_range_error("BigInt string exceeds maximum length"));
          }
          rt.alloc_string_handle(&s)?
        }
        Value::Symbol(_) => {
          return Err(rt.throw_type_error("Cannot convert a Symbol value to a string"));
        }
        Value::Object(obj) => {
          if let Some(string_data) = rt.string_object_data(obj)? {
            string_data
          } else if let Some(boolean_data) = rt.boolean_object_data(obj)? {
            if boolean_data {
              rt.alloc_string_handle("true")?
            } else {
              rt.alloc_string_handle("false")?
            }
          } else if let Some(number_data) = rt.number_object_data(obj)? {
            rt.to_string_from_number(number_data)?
          } else if let Some(bigint_data) = rt.bigint_object_data(obj)? {
            let bi = rt.heap.get_bigint(bigint_data)?;
            let est_units = bi.estimated_byte_len().saturating_mul(3).saturating_add(1);
            if est_units > rt.webidl_limits.max_string_code_units {
              return Err(rt.throw_range_error("BigInt string exceeds maximum length"));
            }
            let s = bi.to_string_radix_with_tick(10, &mut || Ok(()))?;
            if s.len() > rt.webidl_limits.max_string_code_units {
              return Err(rt.throw_range_error("BigInt string exceeds maximum length"));
            }
            rt.alloc_string_handle(&s)?
          } else if let Some(symbol_data) = rt.symbol_object_data(obj)? {
            return Err(rt.throw_symbol_to_string(symbol_data));
          } else if rt.heap.is_valid_object(obj) {
            match rt.host_objects.get(&WeakGcObject::from(obj)) {
              Some(HostObjectKind::Error { name }) => {
                let name_str = (*name).to_string();
                let message_key = rt.property_key_from_str("message")?;
                let message_value = rt
                  .heap
                  .object_get_own_data_property_value(obj, &message_key)
                  .unwrap_or(None)
                  .unwrap_or(Value::Undefined);
                let message = match message_value {
                  Value::String(s) => {
                    let js = rt.heap.get_string(s)?;
                    if js.len_code_units() > rt.webidl_limits.max_string_code_units {
                      String::new()
                    } else {
                      js.to_utf8_lossy()
                    }
                  }
                  _ => String::new(),
                };
                let combined = if message.is_empty() {
                  name_str
                } else {
                  format!("{name_str}: {message}")
                };
                rt.alloc_string_handle(&combined)?
              }
              _ => rt.alloc_string_handle("[object Object]")?,
            }
          } else {
            rt.alloc_string_handle("[object Object]")?
          }
        }
      };
      Ok(Value::String(s))
    })
  }

  fn string_to_utf8_lossy(&mut self, string: Value) -> Result<String, VmError> {
    let handle = match string {
      Value::String(s) => s,
      Value::Object(obj) => self
        .string_object_data(obj)?
        .ok_or_else(|| self.throw_type_error("value is not a string"))?,
      _ => return Err(self.throw_type_error("value is not a string")),
    };
    let js = self.heap.get_string(handle)?;
    if js.len_code_units() > self.webidl_limits.max_string_code_units {
      return Err(self.throw_range_error("string exceeds maximum length"));
    }
    Ok(js.to_utf8_lossy())
  }

  fn to_bigint(&mut self, value: Value) -> Result<Value, VmError> {
    self.sweep_dead_host_objects_if_needed();
    self.with_stack_roots([value], |rt| {
      // Spec: <https://tc39.es/ecma262/#sec-tobigint>.
      let prim = match value {
        Value::Object(obj) => {
          if let Some(string_data) = rt.string_object_data(obj)? {
            Value::String(string_data)
          } else if let Some(boolean_data) = rt.boolean_object_data(obj)? {
            Value::Bool(boolean_data)
          } else if let Some(number_data) = rt.number_object_data(obj)? {
            Value::Number(number_data)
          } else if let Some(bigint_data) = rt.bigint_object_data(obj)? {
            Value::BigInt(bigint_data)
          } else if let Some(symbol_data) = rt.symbol_object_data(obj)? {
            Value::Symbol(symbol_data)
          } else {
            // Approximate `ToPrimitive(obj, number)` by falling back to our existing `ToString`
            // stub. (Plain objects stringify to "[object Object]".)
            rt.to_string(value)?
          }
        }
        other => other,
      };

      let bigint = match prim {
        Value::BigInt(b) => b,
        Value::Bool(true) => {
          let mut scope = rt.heap.scope();
          scope.alloc_bigint_from_u128(1)?
        }
        Value::Bool(false) => {
          let mut scope = rt.heap.scope();
          scope.alloc_bigint_from_u128(0)?
        }
        Value::String(s) => rt.bigint_from_string(s)?,
        Value::Undefined | Value::Null => {
          return Err(rt.throw_type_error("Cannot convert null or undefined to a BigInt"));
        }
        Value::Number(_) => {
          return Err(rt.throw_type_error("Cannot convert a Number value to a BigInt"));
        }
        Value::Symbol(_) => {
          return Err(rt.throw_type_error("Cannot convert a Symbol value to a BigInt"));
        }
        Value::Object(_) => unreachable!("ToPrimitive should have produced a primitive value"),
      };

      Ok(Value::BigInt(bigint))
    })
  }

  fn to_numeric(&mut self, value: Value) -> Result<Value, VmError> {
    if let Value::BigInt(_) = value {
      return Ok(value);
    }
    if let Value::Object(obj) = value {
      if let Some(bigint_data) = self.bigint_object_data(obj)? {
        return Ok(Value::BigInt(bigint_data));
      }
    }
    Ok(Value::Number(self.to_number(value)?))
  }

  fn get(&mut self, obj: Value, key: PropertyKey) -> Result<Value, VmError> {
    let Value::Object(receiver) = obj else {
      return Err(self.throw_type_error("Get: receiver is not an object"));
    };
    self.sweep_dead_host_objects_if_needed();

    let Some(desc) = self.heap.get_property(receiver, &key)? else {
      return Ok(Value::Undefined);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          return Ok(Value::Undefined);
        }
        if !self.is_callable(get) {
          return Err(self.throw_type_error("Getter is not callable"));
        }
        self.call_internal(get, Value::Object(receiver), &[])
      }
    }
  }

  fn own_property_keys(&mut self, obj: Value) -> Result<Vec<PropertyKey>, VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("OwnPropertyKeys: receiver is not an object"));
    };
    self.sweep_dead_host_objects_if_needed();

    let mut out = self.heap.own_property_keys(obj)?;
    out.retain(|k| !self.is_internal_key(k));
    Ok(out)
  }

  fn get_own_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
  ) -> Result<Option<JsOwnPropertyDescriptor<Value>>, VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("GetOwnProperty: receiver is not an object"));
    };
    let Some(desc) = self.heap.object_get_own_property(obj, &key)? else {
      return Ok(None);
    };

    let kind = match desc.kind {
      PropertyKind::Data { value, .. } => JsPropertyKind::Data { value },
      PropertyKind::Accessor { get, set } => JsPropertyKind::Accessor { get, set },
    };

    Ok(Some(JsOwnPropertyDescriptor {
      enumerable: desc.enumerable,
      kind,
    }))
  }

  fn get_method(&mut self, obj: Value, key: PropertyKey) -> Result<Option<Value>, VmError> {
    let func = self.get(obj, key)?;
    if matches!(func, Value::Undefined | Value::Null) {
      return Ok(None);
    }
    if !self.is_callable(func) {
      return Err(self.throw_type_error("GetMethod: property is not callable"));
    }
    Ok(Some(func))
  }

  fn get_iterator_from_method(
    &mut self,
    iterable: Value,
    method: Value,
  ) -> Result<IteratorRecord<Value>, VmError> {
    let iterator = self.call_internal(method, iterable, &[])?;
    if !self.is_object(iterator) {
      return Err(self.throw_type_error("Iterator method did not return an object"));
    }

    self.with_stack_roots([iterator], |rt| {
      let next_key = rt.property_key_from_str("next")?;
      let next = rt.get(iterator, next_key)?;
      if !rt.is_callable(next) {
        return Err(rt.throw_type_error("Iterator.next is not callable"));
      }

      Ok(IteratorRecord {
        iterator,
        next_method: next,
        done: false,
      })
    })
  }

  fn iterator_step_value(
    &mut self,
    iterator_record: &mut IteratorRecord<Value>,
  ) -> Result<Option<Value>, VmError> {
    if iterator_record.done {
      return Ok(None);
    }

    let iterator = iterator_record.iterator;
    let next_method = iterator_record.next_method;
    self.with_stack_roots([iterator, next_method], |rt| {
      let result = rt.call_internal(next_method, iterator, &[])?;
      if !rt.is_object(result) {
        return Err(rt.throw_type_error("Iterator.next() did not return an object"));
      }

      rt.with_stack_roots([result], |rt| {
        let done_key = rt.property_key_from_str("done")?;
        let done = rt.get(result, done_key)?;
        let done = rt.to_boolean(done)?;
        if done {
          iterator_record.done = true;
          return Ok(None);
        }

        let value_key = rt.property_key_from_str("value")?;
        let value = rt.get(result, value_key)?;
        Ok(Some(value))
      })
    })
  }
}

impl WebIdlJsRuntime for VmJsRuntime {
  fn limits(&self) -> WebIdlLimits {
    self.webidl_limits
  }

  fn hooks(&self) -> &dyn WebIdlHooks<Value> {
    self
  }

  fn promise_resolve(&mut self, value: Value) -> Result<Value, VmError> {
    // Spec: https://tc39.es/ecma262/#sec-promise-resolve
    //
    // This legacy heap-only runtime does not execute JS, so we implement the shape of
    // `PromiseResolve(%Promise%, value)` with a minimal semantic subset:
    // - If `value` is already a Promise object, return it.
    // - Otherwise allocate a new already-fulfilled Promise with `[[PromiseResult]] = value`.
    //
    // This is sufficient for WebIDL `Promise<T>` *argument* conversion, where bindings primarily
    // need a Promise object handle to pass through to the host.
    self.with_stack_roots([value], |rt| {
      if let Value::Object(obj) = value {
        if rt.heap.is_promise_object(obj) {
          return Ok(value);
        }
      }

      // Root `value` across Promise allocation under aggressive GC settings.
      rt.with_stack_roots([value], |rt| {
        let promise = {
          let mut scope = rt.heap.scope();
          scope.alloc_promise()?
        };
        rt.heap.promise_fulfill(promise, value)?;
        Ok(Value::Object(promise))
      })
    })
  }

  fn symbol_iterator(&mut self) -> Result<PropertyKey, VmError> {
    if let Some(sym) = self.well_known_iterator {
      return Ok(PropertyKey::Symbol(sym));
    }
    let key = self.alloc_string_handle("Symbol.iterator")?;
    let sym = self.heap.symbol_for(key)?;
    self.well_known_iterator = Some(sym);
    Ok(PropertyKey::Symbol(sym))
  }

  fn symbol_async_iterator(&mut self) -> Result<PropertyKey, VmError> {
    if let Some(sym) = self.well_known_async_iterator {
      return Ok(PropertyKey::Symbol(sym));
    }
    let key = self.alloc_string_handle("Symbol.asyncIterator")?;
    let sym = self.heap.symbol_for(key)?;
    self.well_known_async_iterator = Some(sym);
    Ok(PropertyKey::Symbol(sym))
  }

  fn symbol_to_property_key(&mut self, symbol: Value) -> Result<PropertyKey, VmError> {
    let Value::Symbol(sym) = symbol else {
      return Err(self.throw_type_error("expected a Symbol value"));
    };
    Ok(PropertyKey::Symbol(sym))
  }

  fn platform_object_opaque(&self, value: Value) -> Option<u64> {
    VmJsRuntime::platform_object_opaque(self, value)
  }

  fn is_string_object(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(self.string_object_data(obj), Ok(Some(_)))
  }

  fn is_array_buffer(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    if self.heap.is_array_buffer_object(obj) {
      return true;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::ArrayBuffer { .. })
    )
  }

  fn is_shared_array_buffer(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::ArrayBuffer { shared: true })
    )
  }

  fn is_data_view(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    if self.heap.is_data_view_object(obj) {
      return true;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::DataView)
    )
  }

  fn typed_array_name(&self, value: Value) -> Option<&'static str> {
    let Value::Object(obj) = value else {
      return None;
    };
    if !self.heap.is_valid_object(obj) {
      return None;
    }
    if let Some(name) = self.heap.typed_array_name(obj) {
      return Some(name);
    }
    match self.host_objects.get(&WeakGcObject::from(obj))? {
      HostObjectKind::TypedArray { name } => Some(name),
      _ => None,
    }
  }

  fn platform_object_to_js_value(&mut self, value: &webidl::ir::PlatformObject) -> Option<Value> {
    value.downcast_ref::<Value>().copied()
  }

  fn throw_type_error(&mut self, message: &str) -> VmError {
    VmError::Throw(self.create_error_object("TypeError", message))
  }

  fn throw_range_error(&mut self, message: &str) -> VmError {
    VmError::Throw(self.create_error_object("RangeError", message))
  }
}

impl<Host: 'static> WebIdlBindingsRuntime<Host> for VmJsRuntime {
  fn create_function(
    &mut self,
    name: &str,
    length: u32,
    f: NativeHostFunction<Self, Host>,
  ) -> Result<Value, VmError> {
    let host_type_id = TypeId::of::<Host>();

    self.alloc_function_value_with_name_length(name, length, move |rt, this, args| {
      let Some(ptr) = rt.bindings_host_ptr else {
        return Err(rt.throw_type_error(
          "WebIDL bindings host context is not active (missing VmJsRuntime::with_host_context)",
        ));
      };
      if rt.bindings_host_type_id != Some(host_type_id) {
        return Err(
          rt.throw_type_error("WebIDL bindings host context type mismatch for this function"),
        );
      }

      // SAFETY:
      // - `with_host_context::<Host>` stores a `*mut Host` as `NonNull<()>`.
      // - We check the stored `TypeId` matches `Host` before casting.
      // - The host reference is only used for the duration of this call; it does not escape.
      let host: &mut Host = unsafe { ptr.cast::<Host>().as_mut() };
      f(rt, host, this, args)
    })
  }

  fn create_constructor(
    &mut self,
    name: &str,
    length: u32,
    call: NativeHostFunction<Self, Host>,
    _construct: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    // `VmJsRuntime` is a minimal harness runtime and does not model `[[Construct]]`. Still honour
    // the WebIDL requirement that interface objects are not callable without `new` by wiring the
    // `call` handler (usually an "Illegal constructor" TypeError thrower).
    self.create_function(name, length, call)
  }

  fn define_data_property_with_attrs(
    &mut self,
    obj: Value,
    key: PropertyKey,
    value: Value,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(
        self.throw_type_error("define_data_property_with_attrs: receiver is not an object"),
      );
    };

    let desc = PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Data { value, writable },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)
  }

  fn define_accessor_property_with_attrs(
    &mut self,
    obj: Value,
    key: PropertyKey,
    get: Value,
    set: Value,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(
        self.throw_type_error("define_accessor_property_with_attrs: receiver is not an object"),
      );
    };

    let desc = PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Accessor { get, set },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)
  }

  fn set_prototype(&mut self, obj: Value, proto: Option<Value>) -> Result<(), VmError> {
    VmJsRuntime::set_prototype(self, obj, proto)
  }

  fn global_object(&mut self) -> Result<Value, VmError> {
    if let Some(obj) = self.global_object {
      if self.heap.is_valid_object(obj) {
        return Ok(Value::Object(obj));
      }
    }

    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    let _ = self.heap.add_root(Value::Object(obj))?;
    self.global_object = Some(obj);
    Ok(Value::Object(obj))
  }

  fn root_callback_function(&mut self, value: Value) -> Result<CallbackHandle, VmError> {
    if matches!(value, Value::Undefined | Value::Null) {
      return Err(self.throw_type_error("Callback function is null or undefined"));
    }
    if !self.is_callable(value) {
      return Err(self.throw_type_error("Value is not a callable callback function"));
    }
    CallbackHandle::new(self.heap_mut(), CallbackKind::Function, value, None)
  }

  fn root_callback_interface(&mut self, value: Value) -> Result<CallbackHandle, VmError> {
    if matches!(value, Value::Undefined | Value::Null) {
      return Err(self.throw_type_error("Callback interface is null or undefined"));
    }

    if self.is_callable(value) {
      return CallbackHandle::new(self.heap_mut(), CallbackKind::Interface, value, None);
    }

    if !self.is_object(value) {
      return Err(self.throw_type_error("Value is not a callback interface object"));
    }

    let handle_event_key = self.property_key_from_str("handleEvent")?;
    if self.get_method(value, handle_event_key)?.is_none() {
      return Err(
        self.throw_type_error("Callback interface object is missing a callable handleEvent method"),
      );
    }

    CallbackHandle::new(self.heap_mut(), CallbackKind::Interface, value, None)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn alloc_bigint_u128(rt: &mut VmJsRuntime, value: u128) -> Value {
    let handle = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_bigint_from_u128(value).unwrap()
    };
    Value::BigInt(handle)
  }

  fn alloc_bigint_i128(rt: &mut VmJsRuntime, value: i128) -> Value {
    let handle = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_bigint_from_i128(value).unwrap()
    };
    Value::BigInt(handle)
  }

  fn assert_bigint_eq(rt: &VmJsRuntime, value: Value, expected: &JsBigInt) {
    let Value::BigInt(handle) = value else {
      panic!("expected BigInt");
    };
    assert_eq!(rt.heap().get_bigint(handle).unwrap(), expected);
  }

  #[test]
  fn alloc_function_value_creates_vmjs_function_object() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let f = rt
      .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
      .unwrap();
    let Value::Object(obj) = f else {
      panic!("expected function object");
    };
    assert!(
      rt.heap.get_function_native_slots(obj).is_ok(),
      "alloc_function_value should allocate a real vm-js Function heap object"
    );
  }

  #[test]
  fn alloc_string_value_does_not_implicitly_intern_window_or_document() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

    let a = rt.alloc_string_value("window").unwrap();
    let b = rt.alloc_string_value("window").unwrap();
    assert_ne!(a, b, "alloc_string_value(\"window\") should not be implicitly interned");

    let a = rt.alloc_string_value("document").unwrap();
    let b = rt.alloc_string_value("document").unwrap();
    assert_ne!(a, b, "alloc_string_value(\"document\") should not be implicitly interned");
  }

  #[test]
  fn buffer_source_internal_slot_checks_use_vm_js_heap_objects() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

    // Allocate a real `vm-js` ArrayBuffer + Uint8Array on the heap. Root them so subsequent
    // allocations (including during predicate checks in debug builds) can't collect them.
    let (buf, buf_root, u8, u8_root) = {
      let mut scope = rt.heap_mut().scope();
      let buf = scope.alloc_array_buffer(8)?;
      let buf_root = scope.heap_mut().add_root(Value::Object(buf))?;

      let u8 = scope.alloc_uint8_array(buf, 0, 8)?;
      let u8_root = scope.heap_mut().add_root(Value::Object(u8))?;

      (buf, buf_root, u8, u8_root)
    };

    assert!(
      WebIdlJsRuntime::is_array_buffer(&rt, Value::Object(buf)),
      "expected vm-js ArrayBuffer heap objects to satisfy WebIDL is_array_buffer"
    );

    assert_eq!(
      WebIdlJsRuntime::typed_array_name(&rt, Value::Object(u8)),
      Some("Uint8Array")
    );

    rt.heap_mut().remove_root(buf_root);
    rt.heap_mut().remove_root(u8_root);
    Ok(())
  }

  #[test]
  fn intern_string_value_provides_stable_identity_and_is_gc_rooted() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

    let a = rt.intern_string_value("window").unwrap();
    let b = rt.intern_string_value("window").unwrap();
    assert_eq!(a, b, "intern_string_value should return stable identity");

    // Ensure the interned handle remains valid even after an explicit GC.
    rt.heap_mut().collect_garbage();
    assert_eq!(as_utf8_lossy(&rt, a), "window");

    let c = rt.intern_string_value("window").unwrap();
    assert_eq!(a, c, "interned string should survive GC and keep stable identity");
  }

  #[test]
  fn to_string_primitives() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let s = rt.to_string(Value::Undefined).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "undefined");
    let s = rt.to_string(Value::Null).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "null");
    let s = rt.to_string(Value::Bool(true)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "true");
    let s = rt.to_string(Value::Bool(false)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "false");
    let s = rt.to_string(Value::Number(42.0)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "42");
    let s = rt.to_string(Value::Number(-0.0)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "0");
    let s = rt.to_string(Value::Number(f64::NAN)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "NaN");
    let s = rt.to_string(Value::Number(f64::INFINITY)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "Infinity");
    let s = rt.to_string(Value::Number(f64::NEG_INFINITY)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "-Infinity");
    // `Number::toString` formatting differs from Rust's default float formatting in a few key
    // threshold cases:
    // - values >= 1e21 use exponential form
    // - values < 1e-6 use exponential form
    let s = rt.to_string(Value::Number(1e21)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "1e+21");
    let s = rt.to_string(Value::Number(-1e21)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "-1e+21");
    let s = rt.to_string(Value::Number(1e20)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "100000000000000000000");
    let s = rt.to_string(Value::Number(1e-6)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "0.000001");
    let s = rt.to_string(Value::Number(1e-7)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "1e-7");
    let bigint = alloc_bigint_u128(&mut rt, 42);
    let s = rt.to_string(bigint).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "42");
    let bigint = alloc_bigint_i128(&mut rt, -42);
    let s = rt.to_string(bigint).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "-42");
  }

  #[test]
  fn string_to_utf8_lossy_enforces_max_string_code_units() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    rt.set_webidl_limits(WebIdlLimits {
      max_string_code_units: 16,
      ..WebIdlLimits::default()
    });

    // 17 UTF-16 code units exceeds the limit.
    let s = rt.alloc_string_value("12345678901234567").unwrap();
    let err = rt
      .string_to_utf8_lossy(s)
      .expect_err("expected oversized string conversion to throw");

    let Some(thrown) = err.thrown_value() else {
      panic!("expected Throw, got {err:?}");
    };

    let name_key = rt.property_key_from_str("name").unwrap();
    let name = rt.get(thrown, name_key).unwrap();
    let name = rt.string_to_utf8_lossy(name).unwrap();
    assert_eq!(name, "RangeError");
  }

  #[test]
  fn to_string_bigint_primitive() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let bigint = alloc_bigint_u128(&mut rt, 123);
    let s = rt.to_string(bigint).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "123");
  }

  #[test]
  fn to_object_wraps_bigint_and_to_string_roundtrips() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let bigint = alloc_bigint_u128(&mut rt, 7);
    let obj = rt.to_object(bigint).unwrap();
    assert!(rt.is_object(obj));
    let s = rt.to_string(obj).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "7");
  }

  #[test]
  fn to_object_string_is_gc_safe_under_extreme_gc_pressure() {
    // Force a GC before every heap allocation. Without careful rooting, intermediate `GcString`
    // handles (e.g. the string being wrapped) can be collected before they are stored in the
    // wrapper object.
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
    let s = rt.alloc_string_value("456").unwrap();
    let obj = rt.to_object(s).unwrap();
    assert!(matches!(obj, Value::Object(_)));
    assert!(rt.is_string_object(obj));
  }

  #[test]
  fn to_object_symbol_is_gc_safe_under_extreme_gc_pressure() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
    let sym = {
      let mut scope = rt.heap.scope();
      scope.alloc_symbol(Some("desc")).unwrap()
    };
    let obj = rt.to_object(Value::Symbol(sym)).unwrap();
    let Value::Object(obj_handle) = obj else {
      panic!("expected Symbol wrapper object");
    };
    assert_eq!(rt.symbol_object_data(obj_handle).unwrap(), Some(sym));
  }

  #[test]
  fn to_number_primitives() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let bigint_one = alloc_bigint_u128(&mut rt, 1);
    assert!(rt.to_number(Value::Undefined).unwrap().is_nan());
    assert_eq!(rt.to_number(Value::Null).unwrap(), 0.0);
    assert_eq!(rt.to_number(Value::Bool(true)).unwrap(), 1.0);
    assert_eq!(rt.to_number(Value::Bool(false)).unwrap(), 0.0);
    assert!(rt
      .to_number(bigint_one)
      .unwrap_err()
      .thrown_value()
      .is_some());
    let s = rt.alloc_string_value("  123  ").unwrap();
    assert_eq!(rt.to_number(s).unwrap(), 123.0);
    assert!(matches!(
      rt.to_number(bigint_one),
      Err(err) if err.thrown_value().is_some()
    ));

    // Per ECMA-262 StringToNumber, signed hex strings are not valid numeric literals.
    let s = rt.alloc_string_value("-0x10").unwrap();
    assert!(rt.to_number(s).unwrap().is_nan());
    let s = rt.alloc_string_value("+0x10").unwrap();
    assert!(rt.to_number(s).unwrap().is_nan());

    // Rust's float parser accepts spellings like "inf"/"infinity" which are not valid numeric
    // literals in ECMA-262. Ensure we reject them so `Number("inf")` produces NaN.
    for text in ["inf", "+inf", "-inf", "infinity", "INFINITY"] {
      let s = rt.alloc_string_value(text).unwrap();
      assert!(rt.to_number(s).unwrap().is_nan(), "Number({text:?})");
    }

    // Radix-prefixed integer literals must only accept ASCII digits/letters.
    for text in ["0b\u{0661}", "0o\u{0661}", "0x\u{0661}"] {
      let s = rt.alloc_string_value(text).unwrap();
      assert!(rt.to_number(s).unwrap().is_nan(), "Number({text:?})");
    }

    // Radix-prefixed integer literals can exceed `u64`. Ensure they still parse to a Number (f64)
    // instead of incorrectly producing NaN.
    let expected = 2f64.powi(64);
    let s = rt.alloc_string_value("0x10000000000000000").unwrap();
    assert_eq!(rt.to_number(s).unwrap(), expected);
    let s = rt.alloc_string_value("0o2000000000000000000000").unwrap();
    assert_eq!(rt.to_number(s).unwrap(), expected);
    let bin = format!("0b1{}", "0".repeat(64));
    let s = rt.alloc_string_value(&bin).unwrap();
    assert_eq!(rt.to_number(s).unwrap(), expected);

    let err = rt
      .to_number(bigint_one)
      .unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown TypeError, got {err:?}");
    };
    let message_key = rt.prop_key_str("message").unwrap();
    let message_value = rt.get(thrown, message_key).unwrap();
    assert_eq!(
      as_utf8_lossy(&rt, message_value),
      "Cannot convert a BigInt value to a number"
    );
  }

  #[test]
  fn to_object_bigint_allocates_wrapper() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let bigint = alloc_bigint_u128(&mut rt, 7);
    let obj = rt.to_object(bigint).unwrap();
    assert!(matches!(obj, Value::Object(_)));

    let s = rt.to_string(obj).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "7");

    let err = rt.to_number(obj).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown TypeError, got {err:?}");
    };
    let message_key = rt.prop_key_str("message").unwrap();
    let message_value = rt.get(thrown, message_key).unwrap();
    assert_eq!(
      as_utf8_lossy(&rt, message_value),
      "Cannot convert a BigInt value to a number"
    );
  }

  fn thrown_error_name(rt: &mut VmJsRuntime, err: VmError) -> String {
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };
    let name_key = rt.prop_key_str("name").unwrap();
    let name_value = rt.get(thrown, name_key).unwrap();
    as_utf8_lossy(rt, name_value)
  }

  #[test]
  fn to_number_on_bigint_throws_type_error() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let bigint_one = alloc_bigint_u128(&mut rt, 1);
    let err = rt
      .to_number(bigint_one)
      .unwrap_err();
    assert_eq!(thrown_error_name(&mut rt, err), "TypeError");
  }

  #[test]
  fn to_bigint_conversions() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let v = rt.to_bigint(Value::Bool(true)).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::from_u128(1).unwrap());
    let v = rt.to_bigint(Value::Bool(false)).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::zero());

    let s = rt.alloc_string_value("  123  ").unwrap();
    let v = rt.to_bigint(s).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::from_u128(123).unwrap());
    let s = rt.alloc_string_value("-123").unwrap();
    let v = rt.to_bigint(s).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::from_u128(123).unwrap().negate());
    let s = rt.alloc_string_value("0x10").unwrap();
    let v = rt.to_bigint(s).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::from_u128(16).unwrap());

    let s = rt.alloc_string_value("   ").unwrap();
    let v = rt.to_bigint(s).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::zero());

    let s = rt.alloc_string_value("+7").unwrap();
    let v = rt.to_bigint(s).unwrap();
    assert_bigint_eq(&rt, v, &JsBigInt::from_u128(7).unwrap());

    // BigInts should support arbitrarily large values (beyond `u128`/`i128`).
    let s = rt
      .alloc_string_value("340282366920938463463374607431768211456")
      .unwrap(); // u128::MAX + 1
    let v = rt.to_bigint(s).unwrap();
    let s = rt.to_string(v).unwrap();
    assert_eq!(
      as_utf8_lossy(&rt, s),
      "340282366920938463463374607431768211456"
    );
    let s = rt.alloc_string_value("-0x10").unwrap();
    let err = rt.to_bigint(s).unwrap_err();
    assert_eq!(thrown_error_name(&mut rt, err), "SyntaxError");

    let err = rt.to_bigint(Value::Number(1.0)).unwrap_err();
    assert_eq!(thrown_error_name(&mut rt, err), "TypeError");

    let bigint = alloc_bigint_u128(&mut rt, 7);
    let obj = rt.to_object(bigint).unwrap();
    assert_eq!(rt.to_bigint(obj).unwrap(), bigint);

    let obj = rt.to_object(Value::Number(7.0)).unwrap();
    let err = rt.to_bigint(obj).unwrap_err();
    assert_eq!(thrown_error_name(&mut rt, err), "TypeError");
  }

  #[test]
  fn to_boolean_bigint() {
    let mut rt = VmJsRuntime::new();
    let bigint = alloc_bigint_u128(&mut rt, 0);
    assert!(!rt.to_boolean(bigint).unwrap());
    let bigint = alloc_bigint_u128(&mut rt, 1);
    assert!(rt.to_boolean(bigint).unwrap());
  }

  #[test]
  fn to_string_and_to_number_on_string_object() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let obj = rt.alloc_string_object_value("456").unwrap();
    let s = rt.to_string(obj).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "456");
    assert_eq!(rt.to_number(obj).unwrap(), 456.0);
    assert!(rt.is_string_object(obj));
  }

  #[test]
  fn get_method_invokes_getter_once() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let calls_for_getter = calls.clone();

    let method_key = rt.property_key_from_str("method").unwrap();
    let method = rt
      .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
      .unwrap();

    let getter = rt
      .alloc_function_value(move |rt, this, _args| {
        calls_for_getter.set(calls_for_getter.get() + 1);
        rt.get(this, method_key)
      })
      .unwrap();

    let obj = rt.alloc_object_value().unwrap();
    rt.define_data_property(obj, method_key, method, true)
      .unwrap();
    let key = rt.property_key_from_str("m").unwrap();
    rt.define_accessor_property(obj, key, getter, Value::Undefined, true)
      .unwrap();

    let got = rt.get_method(obj, key).unwrap();
    assert!(got.is_some());
    assert_eq!(calls.get(), 1);
  }
  #[test]
  fn call_function_invokes_host_function_with_this_and_args() {
    let mut rt = VmJsRuntime::new();

    let this = rt.alloc_object_value().unwrap();
    let arg0 = rt.alloc_string_value("arg0").unwrap();
    let arg1 = Value::Number(123.0);

    let callee = rt
      .alloc_function_value(move |rt, got_this, got_args| {
        assert_eq!(got_this, this);
        assert_eq!(got_args, &[arg0, arg1]);
        rt.alloc_string_value("ret")
      })
      .unwrap();

    let ret = rt.call_function(callee, this, &[arg0, arg1]).unwrap();
    assert_eq!(as_utf8_lossy(&rt, ret), "ret");
  }

  #[test]
  fn call_function_non_callable_throws_type_error() {
    let mut rt = VmJsRuntime::new();

    let err = rt
      .call_function(Value::Number(1.0), Value::Undefined, &[])
      .unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown TypeError, got {err:?}");
    };

    let name_key = rt.prop_key_str("name").unwrap();
    let name_value = rt.get(thrown, name_key).unwrap();
    assert_eq!(as_utf8_lossy(&rt, name_value), "TypeError");
  }

  #[test]
  fn gc_collects_unreachable_values() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let baseline = rt.heap().used_bytes();

    let mut weak_objects: Vec<WeakGcObject> = Vec::new();
    let mut strings: Vec<GcString> = Vec::new();

    for i in 0..10_000u32 {
      let obj = rt.alloc_object_value()?;
      let Value::Object(obj) = obj else {
        unreachable!();
      };
      weak_objects.push(WeakGcObject::from(obj));

      let s = rt.alloc_string_value(&format!("s{i}"))?;
      let Value::String(s) = s else {
        unreachable!();
      };
      strings.push(s);
    }

    let before = rt.heap().used_bytes();
    assert!(before > baseline);

    rt.heap_mut().collect_garbage();
    let after = rt.heap().used_bytes();
    assert!(after < before);

    for weak in weak_objects {
      assert_eq!(weak.upgrade(rt.heap()), None);
    }
    for s in strings {
      assert!(!rt.heap().is_valid_string(s));
    }

    Ok(())
  }

  #[test]
  fn property_reachability_keeps_values_alive() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

    let obj = rt.alloc_object_value()?;
    let Value::Object(obj_handle) = obj else {
      unreachable!();
    };
    let weak = WeakGcObject::from(obj_handle);

    let value = rt.alloc_string_value("hello")?;
    let Value::String(s_handle) = value else {
      unreachable!();
    };

    let key = rt.property_key_from_str("x")?;
    rt.define_data_property(obj, key, value, true)?;

    let root = rt.heap_mut().add_root(Value::Object(obj_handle))?;
    rt.heap_mut().collect_garbage();

    assert_eq!(weak.upgrade(rt.heap()), Some(obj_handle));
    assert!(rt.heap().is_valid_string(s_handle));

    let got = rt.get(obj, key)?;
    assert_eq!(as_utf8_lossy(&rt, got), "hello");

    rt.heap_mut().remove_root(root);
    rt.heap_mut().collect_garbage();

    assert_eq!(weak.upgrade(rt.heap()), None);
    assert!(!rt.heap().is_valid_string(s_handle));

    Ok(())
  }

  #[test]
  fn own_property_keys_orders_indices_then_strings_then_symbols() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

    let obj = rt.alloc_object_value()?;

    let k_b = rt.property_key_from_str("b")?;
    let k_1 = rt.property_key_from_str("1")?;
    let k_a = rt.property_key_from_str("a")?;
    let k_0 = rt.property_key_from_str("0")?;
    let k_01 = rt.property_key_from_str("01")?;

    let sym1 = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_symbol(Some("s1"))?
    };
    let sym2 = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_symbol(Some("s2"))?
    };

    rt.define_data_property(obj, k_b, Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_1, Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_a, Value::Number(0.0), true)?;
    rt.define_data_property(obj, PropertyKey::Symbol(sym1), Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_0, Value::Number(0.0), true)?;
    rt.define_data_property(obj, PropertyKey::Symbol(sym2), Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_01, Value::Number(0.0), true)?;

    let got = rt.own_property_keys(obj)?;
    let expected = vec![
      k_0,
      k_1,
      k_b,
      k_a,
      k_01,
      PropertyKey::Symbol(sym1),
      PropertyKey::Symbol(sym2),
    ];
    assert_eq!(got.len(), expected.len());
    for (got, expected) in got.iter().zip(expected.iter()) {
      assert!(rt.heap().property_key_eq(got, expected));
    }

    Ok(())
  }

  #[test]
  fn inherited_accessor_get_uses_receiver_as_this() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

    let proto = rt.alloc_object_value()?;
    let instance = rt.alloc_object_value()?;
    rt.set_prototype(instance, Some(proto))?;

    let seen_this = std::rc::Rc::new(std::cell::Cell::new(Value::Undefined));
    let seen_this_for_getter = seen_this.clone();
    let getter = rt.alloc_function_value(move |_rt, this, _args| {
      seen_this_for_getter.set(this);
      Ok(Value::Undefined)
    })?;

    let key = rt.property_key_from_str("x")?;
    rt.define_accessor_property(proto, key, getter, Value::Undefined, true)?;

    let key = rt.property_key_from_str("x")?;
    let _ = rt.get(instance, key)?;
    assert_eq!(seen_this.get(), instance);
    Ok(())
  }

  #[test]
  fn platform_object_interface_checks() {
    let mut rt = VmJsRuntime::new();

    let a = crate::interface_id_from_name("A");
    let b = crate::interface_id_from_name("B");
    let c = crate::interface_id_from_name("C");

    let obj = rt.alloc_platform_object_value("A", &["B"], 1).unwrap();
    assert!(crate::runtime::WebIdlJsRuntime::is_platform_object(
      &rt, obj
    ));
    assert!(crate::runtime::WebIdlJsRuntime::implements_interface(
      &rt, obj, a
    ));
    assert!(crate::runtime::WebIdlJsRuntime::implements_interface(
      &rt, obj, b
    ));
    assert!(!crate::runtime::WebIdlJsRuntime::implements_interface(
      &rt, obj, c
    ));
  }

  #[test]
  fn ordinary_host_objects_are_not_platform_objects() {
    let mut rt = VmJsRuntime::new();

    let iface = crate::interface_id_from_name("A");

    let obj = rt.alloc_object_value().unwrap();
    assert!(!crate::runtime::WebIdlJsRuntime::is_platform_object(
      &rt, obj
    ));
    assert!(!crate::runtime::WebIdlJsRuntime::implements_interface(
      &rt, obj, iface
    ));

    let string_obj = rt.alloc_string_object_value("hi").unwrap();
    assert!(!crate::runtime::WebIdlJsRuntime::is_platform_object(
      &rt, string_obj
    ));
    assert!(!crate::runtime::WebIdlJsRuntime::implements_interface(
      &rt, string_obj, iface
    ));

    let func_obj = rt
      .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
      .unwrap();
    assert!(!crate::runtime::WebIdlJsRuntime::is_platform_object(
      &rt, func_obj
    ));
    assert!(!crate::runtime::WebIdlJsRuntime::implements_interface(
      &rt, func_obj, iface
    ));
  }
}
