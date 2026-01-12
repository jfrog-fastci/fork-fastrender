//! Runtime boundary required by Web IDL algorithms.
//!
//! The spec-shaped conversion and overload-resolution algorithms live in
//! `crates/webidl-bindings-core`. This crate keeps a legacy `VmJsRuntime` implementation and
//! additional helpers for bindings installation, so we re-export the core runtime traits here and
//! layer the bindings-facing API on top.

pub use webidl_bindings_core::runtime::{
  interface_id_from_name, InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind,
  JsRuntime, WebIdlHooks, WebIdlJsRuntime, WebIdlLimits,
};

use webidl_vm_js::CallbackHandle;
/// Host-facing runtime API used by generated WebIDL bindings.
///
/// This sits *above* [`WebIdlJsRuntime`]. The `WebIdlJsRuntime` trait is intentionally scoped to the
/// ECMAScript abstract operations required by WebIDL conversion algorithms. Bindings generation
/// needs additional capabilities:
/// - creating host objects/functions,
/// - defining properties/prototype chains, and
/// - constructing primitive JS values for return values.
///
/// The binding callbacks are expressed as plain function pointers rather than closures so they can
/// take an explicit `&mut Host` parameter. This keeps the binding layer free of global state and
/// avoids needing runtime-specific "host pointer" plumbing in generated code.
pub type NativeHostFunction<R, Host> =
  fn(
    rt: &mut R,
    host: &mut Host,
    this: <R as JsRuntime>::JsValue,
    args: &[<R as JsRuntime>::JsValue],
  ) -> Result<<R as JsRuntime>::JsValue, <R as JsRuntime>::Error>;

pub trait WebIdlBindingsRuntime<Host>: WebIdlJsRuntime {
  fn js_bool(&self, value: bool) -> Self::JsValue {
    self.js_boolean(value)
  }

  fn js_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error> {
    self.alloc_string(value)
  }

  /// Convert a JS string value to a Rust `String` (UTF-8, lossy).
  fn js_string_to_rust_string(&mut self, value: Self::JsValue) -> Result<String, Self::Error> {
    self.string_to_utf8_lossy(value)
  }

  /// Create a property key from an ASCII/UTF-8 string.
  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error> {
    self.property_key_from_str(name)
  }

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    self.alloc_object()
  }

  /// Create a host-defined function object with WebIDL-visible metadata.
  ///
  /// - `name` is used for the function's `.name` property.
  /// - `length` is used for the function's `.length` property (required argument count).
  fn create_function(
    &mut self,
    name: &str,
    length: u32,
    f: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error>;

  /// Create a host-defined constructor function object with WebIDL-visible metadata.
  ///
  /// Some JS runtimes distinguish between `[[Call]]` and `[[Construct]]` internal methods. WebIDL
  /// interface objects with a `constructor(...)` member need both:
  /// - `call` is used for `Ctor(...)` and should generally throw a TypeError ("Illegal constructor").
  /// - `construct` is used for `new Ctor(...)`.
  ///
  /// The default implementation falls back to [`WebIdlBindingsRuntime::create_function`], using the
  /// `construct` callback for both calling and constructing. This is sufficient for runtimes that
  /// do not expose separate `[[Construct]]` plumbing yet (but note: it does not enforce "illegal
  /// constructor" behavior when called without `new`).
  fn create_constructor(
    &mut self,
    name: &str,
    length: u32,
    _call: NativeHostFunction<Self, Host>,
    construct: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    self.create_function(name, length, construct)
  }

  /// Define a data property with explicit ECMAScript attributes.
  fn define_data_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error>;

  /// Define an accessor property with explicit ECMAScript attributes.
  fn define_accessor_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    get: Self::JsValue,
    set: Self::JsValue,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error>;

  fn set_prototype(
    &mut self,
    obj: Self::JsValue,
    proto: Option<Self::JsValue>,
  ) -> Result<(), Self::Error>;

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error>;

  /// Root and return a WebIDL callback function handle.
  ///
  /// Generated bindings use this to pass callbacks to the host in a GC-safe way.
  fn root_callback_function(
    &mut self,
    _value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
    Err(self.throw_type_error("Callback functions are not supported by this runtime"))
  }

  /// Root and return a WebIDL callback interface handle.
  ///
  /// Callback interfaces accept callable functions or objects with a callable `handleEvent` method.
  fn root_callback_interface(
    &mut self,
    _value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
    Err(self.throw_type_error("Callback interfaces are not supported by this runtime"))
  }

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    let key = self.property_key_from_str(name)?;
    self.define_data_property(obj, key, value, enumerable)
  }

  fn define_data_property_str_with_attrs(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    let key = self.property_key_from_str(name)?;
    self.define_data_property_with_attrs(obj, key, value, writable, enumerable, configurable)
  }

  fn define_accessor_property_str_with_attrs(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    get: Self::JsValue,
    set: Self::JsValue,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    let key = self.property_key_from_str(name)?;
    self.define_accessor_property_with_attrs(obj, key, get, set, enumerable, configurable)
  }

  /// Defines a WebIDL operation method property.
  ///
  /// This follows the Web IDL JavaScript binding:
  /// - writable: true
  /// - configurable: true
  /// - enumerable: false
  fn define_method(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    func: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str_with_attrs(obj, name, func, true, false, true)
  }

  /// Defines a WebIDL attribute accessor property.
  ///
  /// - configurable: true
  /// - enumerable: true
  fn define_attribute_accessor(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    get: Self::JsValue,
    set: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_accessor_property_str_with_attrs(obj, name, get, set, true, true)
  }

  /// Defines a WebIDL constant property.
  ///
  /// - writable: false
  /// - configurable: false
  /// - enumerable: true
  fn define_constant(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str_with_attrs(obj, name, value, false, true, false)
  }

  /// Defines an interface constructor, wiring `.prototype` and `prototype.constructor`.
  ///
  /// This is a convenience for generated bindings that need to produce WebIDL-shaped interface
  /// objects. The property attributes are chosen to match WebIDL's requirements for interface
  /// objects and prototype objects:
  /// - `global[name]`: writable + configurable, non-enumerable
  /// - `ctor.prototype`: non-writable, non-enumerable, non-configurable
  /// - `proto.constructor`: non-writable, non-enumerable, non-configurable
  fn define_constructor(
    &mut self,
    global: Self::JsValue,
    name: &str,
    ctor: Self::JsValue,
    proto: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str_with_attrs(global, name, ctor, true, false, true)?;
    self.define_data_property_str_with_attrs(ctor, "prototype", proto, false, false, false)?;
    self.define_data_property_str_with_attrs(proto, "constructor", ctor, false, false, false)?;
    Ok(())
  }
}
