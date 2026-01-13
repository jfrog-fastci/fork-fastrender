//! Runtime boundary required by Web IDL algorithms.
//!
//! Web IDL conversions and overload resolution are specified in terms of ECMAScript abstract
//! operations. This module defines the minimal operations that the binding layer needs from an
//! embedded JS engine.

pub use crate::{
  interface_id_from_name, InterfaceId, JsOwnPropertyDescriptor, JsPropertyKind, WebIdlHooks,
  WebIdlLimits,
};

/// ECMAScript "IteratorRecord" (ECMA-262).
///
/// `GetIteratorFromMethod` returns an iterator record; `IteratorStepValue` mutates the record's
/// `[[Done]]` slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IteratorRecord<V> {
  pub iterator: V,
  pub next_method: V,
  pub done: bool,
}

/// Runtime operations used by Web IDL algorithms.
///
/// This trait is intentionally small and only covers the operations that Web IDL conversions and
/// overload resolution depend on. It is *not* intended to be a general JS embedding API.
pub trait JsRuntime {
  /// ECMAScript value type.
  type JsValue: Copy;
  /// ECMAScript property key type (`String | Symbol`).
  type PropertyKey: Copy;
  /// Error type used by the runtime (usually the engine's exception/termination type).
  type Error;

  /// Run `f` with `roots` treated as GC roots for the duration of the call.
  ///
  /// WebIDL conversion algorithms may keep VM values in local variables across allocations (for
  /// example iterator records when converting `sequence<T>`). GC-backed runtimes must ensure those
  /// values remain rooted so they cannot be collected while host code is still using them.
  fn with_stack_roots<R, F>(&mut self, roots: &[Self::JsValue], f: F) -> Result<R, Self::Error>
  where
    F: FnOnce(&mut Self) -> Result<R, Self::Error>;

  fn js_undefined(&self) -> Self::JsValue;
  fn js_null(&self) -> Self::JsValue;
  fn js_boolean(&self, value: bool) -> Self::JsValue;
  fn js_number(&self, value: f64) -> Self::JsValue;
  fn alloc_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error>;
  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Self::JsValue, Self::Error>;
  fn is_undefined(&self, value: Self::JsValue) -> bool;
  fn is_null(&self, value: Self::JsValue) -> bool;

  /// Temporarily exposes the UTF-16 code units of a JS string value.
  ///
  /// Implementations may accept both string primitives and String objects.
  /// The code units are borrowed from the runtime and must not escape the callback.
  fn with_string_code_units<R>(
    &mut self,
    string: Self::JsValue,
    f: impl FnOnce(&[u16]) -> R,
  ) -> Result<R, Self::Error>;

  /// Constructs a property key from a Rust string.
  ///
  /// This is primarily used by generated bindings and conversion algorithms that need to access
  /// object properties by name in a runtime-agnostic way.
  fn property_key_from_str(&mut self, s: &str) -> Result<Self::PropertyKey, Self::Error>;
  fn property_key_from_u32(&mut self, index: u32) -> Result<Self::PropertyKey, Self::Error>;

  /// Returns true if `key` is a Symbol.
  fn property_key_is_symbol(&self, key: Self::PropertyKey) -> bool;

  /// Returns true if `key` is a String.
  fn property_key_is_string(&self, key: Self::PropertyKey) -> bool;

  /// Converts a property key to a JS String value.
  ///
  /// Per ECMAScript `ToString`, this must throw a TypeError if `key` is a Symbol.
  fn property_key_to_js_string(
    &mut self,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error>;

  fn alloc_object(&mut self) -> Result<Self::JsValue, Self::Error>;
  fn alloc_array(&mut self) -> Result<Self::JsValue, Self::Error>;

  fn define_data_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error>;

  fn is_object(&self, value: Self::JsValue) -> bool;
  fn is_callable(&self, value: Self::JsValue) -> bool;
  fn is_boolean(&self, value: Self::JsValue) -> bool;
  fn is_number(&self, value: Self::JsValue) -> bool;
  fn is_bigint(&self, value: Self::JsValue) -> bool;
  fn is_string(&self, value: Self::JsValue) -> bool;
  fn is_symbol(&self, value: Self::JsValue) -> bool;

  /// ECMAScript abstract operation `ToObject ( argument )`.
  ///
  /// Spec: <https://tc39.es/ecma262/#sec-toobject>
  ///
  /// Implementations must throw a `TypeError` when `value` is `null` or `undefined`.
  fn to_object(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;
  /// ECMAScript abstract operation `Call ( F, V, argumentsList )`.
  ///
  /// Spec: <https://tc39.es/ecma262/#sec-call>
  ///
  /// Implementations must throw a `TypeError` when `callee` is not callable.
  fn call(
    &mut self,
    callee: Self::JsValue,
    this: Self::JsValue,
    args: &[Self::JsValue],
  ) -> Result<Self::JsValue, Self::Error>;

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error>;
  fn to_number(&mut self, value: Self::JsValue) -> Result<f64, Self::Error>;
  fn to_string(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;
  fn string_to_utf8_lossy(&mut self, string: Self::JsValue) -> Result<String, Self::Error>;
  fn to_bigint(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;
  fn to_numeric(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;

  fn get(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error>;

  fn own_property_keys(
    &mut self,
    obj: Self::JsValue,
  ) -> Result<Vec<Self::PropertyKey>, Self::Error>;

  fn get_own_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<JsOwnPropertyDescriptor<Self::JsValue>>, Self::Error>;

  fn get_method(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<Self::JsValue>, Self::Error>;

  fn get_iterator_from_method(
    &mut self,
    iterable: Self::JsValue,
    method: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error>;

  fn iterator_step_value(
    &mut self,
    iterator_record: &mut IteratorRecord<Self::JsValue>,
  ) -> Result<Option<Self::JsValue>, Self::Error>;
}

/// Web IDL-specific runtime hooks that sit on top of the core ECMAScript operations.
pub trait WebIdlJsRuntime: JsRuntime {
  fn limits(&self) -> WebIdlLimits;
  fn hooks(&self) -> &dyn WebIdlHooks<Self::JsValue>;

  /// ECMAScript `PromiseResolve(%Promise%, value)` (ECMA-262).
  ///
  /// WebIDL `Promise<T>` conversions use this to coerce any ECMAScript value into a Promise object.
  ///
  /// Implementations should use the intrinsic `%Promise%` constructor of the current realm.
  fn promise_resolve(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;

  /// `%Symbol.iterator%`.
  fn symbol_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error>;
  /// `%Symbol.asyncIterator%`.
  fn symbol_async_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error>;

  /// Converts a JavaScript `Symbol` value into a property key value suitable for `get`/`get_method`.
  fn symbol_to_property_key(
    &mut self,
    symbol: Self::JsValue,
  ) -> Result<Self::PropertyKey, Self::Error>;

  /// If the value is a platform object, returns its embedding-defined opaque id.
  ///
  /// This is useful for bindings that need to map JS wrappers back to host objects.
  fn platform_object_opaque(&self, _value: Self::JsValue) -> Option<u64> {
    None
  }

  /// Returns whether `value` is a platform object (an embedding-defined object that implements one
  /// or more Web IDL interfaces).
  ///
  /// Spec: https://webidl.spec.whatwg.org/#dfn-platform-object
  fn is_platform_object(&self, value: Self::JsValue) -> bool {
    self.hooks().is_platform_object(value)
  }

  /// Returns whether `value` implements the Web IDL interface `interface`.
  ///
  /// If `value` is not a platform object, this must return false.
  fn implements_interface(&self, value: Self::JsValue, interface: InterfaceId) -> bool {
    self.hooks().implements_interface(value, interface)
  }

  /// Returns true if the value is a String object (has `[[StringData]]`).
  fn is_string_object(&self, value: Self::JsValue) -> bool;

  /// Returns true if the value is an ArrayBuffer object (has `[[ArrayBufferData]]`).
  fn is_array_buffer(&self, value: Self::JsValue) -> bool;

  /// Returns true if the value is a SharedArrayBuffer object (`IsSharedArrayBuffer`).
  fn is_shared_array_buffer(&self, value: Self::JsValue) -> bool;

  /// Returns true if the value is a DataView object.
  fn is_data_view(&self, value: Self::JsValue) -> bool;

  /// If the value is a TypedArray object, returns its `TypedArrayName` internal slot.
  fn typed_array_name(&self, value: Self::JsValue) -> Option<&'static str>;

  /// Converts an opaque platform object back into a JS value for this runtime.
  ///
  /// This provides an escape hatch for interface return values while the binding generator does
  /// not yet synthesize wrapper objects. Implementations should return `None` if the platform
  /// object does not belong to this runtime.
  fn platform_object_to_js_value(
    &mut self,
    value: &crate::ir::PlatformObject,
  ) -> Option<Self::JsValue>;

  fn throw_type_error(&mut self, message: &str) -> Self::Error;
  fn throw_range_error(&mut self, message: &str) -> Self::Error;
}
