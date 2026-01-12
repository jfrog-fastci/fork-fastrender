use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::{
  builtins, GcObject, GcString, GcSymbol, NativeConstructId, NativeFunctionId, RootId, Scope, Value,
  Vm, VmError,
};

/// ECMAScript well-known symbols (ECMA-262 "Well-known Symbols" table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WellKnownSymbols {
  pub async_iterator: GcSymbol,
  pub has_instance: GcSymbol,
  pub is_concat_spreadable: GcSymbol,
  pub iterator: GcSymbol,
  pub match_: GcSymbol,
  pub match_all: GcSymbol,
  pub replace: GcSymbol,
  pub search: GcSymbol,
  pub species: GcSymbol,
  pub split: GcSymbol,
  pub to_primitive: GcSymbol,
  pub to_string_tag: GcSymbol,
  pub unscopables: GcSymbol,
}

/// The set of ECMAScript intrinsics for a realm.
///
/// These are kept alive independently of any global bindings so that deleting global properties
/// (e.g. `delete globalThis.TypeError`) does not allow the GC to collect the engine's intrinsic
/// graph.
#[derive(Debug, Clone, Copy)]
pub struct Intrinsics {
  well_known_symbols: WellKnownSymbols,
  object_prototype: GcObject,
  function_prototype: GcObject,
  iterator_prototype: GcObject,
  generator_function: GcObject,
  generator_function_prototype: GcObject,
  generator_prototype: GcObject,
  array_iterator_prototype: GcObject,
  array_prototype: GcObject,
  string_iterator_prototype: GcObject,
  string_prototype: GcObject,
  regexp_prototype: GcObject,
  number_prototype: GcObject,
  boolean_prototype: GcObject,
  bigint_prototype: GcObject,
  date_prototype: GcObject,
  symbol_prototype: GcObject,
  array_buffer_prototype: GcObject,
  uint8_array_prototype: GcObject,
  int8_array_prototype: GcObject,
  uint8_clamped_array_prototype: GcObject,
  int16_array_prototype: GcObject,
  uint16_array_prototype: GcObject,
  int32_array_prototype: GcObject,
  uint32_array_prototype: GcObject,
  float32_array_prototype: GcObject,
  float64_array_prototype: GcObject,
  data_view_prototype: GcObject,
  weak_map_prototype: GcObject,
  weak_set_prototype: GcObject,
  object_constructor: GcObject,
  function_constructor: GcObject,
  generator_function_constructor: GcObject,
  array_constructor: GcObject,
  proxy_constructor: GcObject,
  string_constructor: GcObject,
  regexp_constructor: GcObject,
  number_constructor: GcObject,
  boolean_constructor: GcObject,
  date_constructor: GcObject,
  symbol_constructor: GcObject,
  array_buffer: GcObject,
  uint8_array: GcObject,
  int8_array: GcObject,
  uint8_clamped_array: GcObject,
  int16_array: GcObject,
  uint16_array: GcObject,
  int32_array: GcObject,
  uint32_array: GcObject,
  float32_array: GcObject,
  float64_array: GcObject,
  data_view: GcObject,
  weak_map: GcObject,
  weak_set: GcObject,
  is_nan: GcObject,
  is_finite: GcObject,
  eval: GcObject,
  parse_int: GcObject,
  parse_float: GcObject,
  encode_uri: GcObject,
  encode_uri_component: GcObject,
  decode_uri: GcObject,
  decode_uri_component: GcObject,
  math: GcObject,
  json: GcObject,
  reflect: GcObject,

  error: GcObject,
  error_prototype: GcObject,
  type_error: GcObject,
  type_error_prototype: GcObject,
  range_error: GcObject,
  range_error_prototype: GcObject,
  reference_error: GcObject,
  reference_error_prototype: GcObject,
  syntax_error: GcObject,
  syntax_error_prototype: GcObject,
  eval_error: GcObject,
  eval_error_prototype: GcObject,
  uri_error: GcObject,
  uri_error_prototype: GcObject,
  aggregate_error: GcObject,
  aggregate_error_prototype: GcObject,

  promise: GcObject,
  promise_prototype: GcObject,
  promise_prototype_then: GcObject,
  promise_capability_executor_call: NativeFunctionId,
  promise_resolving_function_call: NativeFunctionId,
  promise_finally_handler_call: NativeFunctionId,
  promise_finally_thunk_call: NativeFunctionId,
  promise_all_resolve_element_call: NativeFunctionId,
  promise_all_settled_element_call: NativeFunctionId,
  promise_any_reject_element_call: NativeFunctionId,

  // Revocation function created by `Proxy.revocable`.
  proxy_revoker_call: NativeFunctionId,

  class_constructor_call: NativeFunctionId,
  class_constructor_construct: NativeConstructId,
}

#[derive(Clone, Copy)]
struct CommonKeys {
  constructor: PropertyKey,
  prototype: PropertyKey,
  name: PropertyKey,
  length: PropertyKey,
}

fn data_desc(
  value: Value,
  writable: bool,
  enumerable: bool,
  configurable: bool,
) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable,
    kind: PropertyKind::Data { value, writable },
  }
}

fn install_to_string_tag(
  scope: &mut Scope<'_>,
  obj: GcObject,
  to_string_tag: GcSymbol,
  tag: &str,
) -> Result<(), VmError> {
  let tag_value = scope.alloc_string(tag)?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(
    obj,
    PropertyKey::Symbol(to_string_tag),
    // `@@toStringTag` is non-writable, non-enumerable, and typically configurable.
    data_desc(Value::String(tag_value), false, false, true),
  )?;
  Ok(())
}

fn alloc_rooted_object(
  scope: &mut Scope<'_>,
  roots: &mut Vec<RootId>,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object()?;
  roots.push(scope.heap_mut().add_root(Value::Object(obj))?);
  Ok(obj)
}

fn alloc_rooted_native_function(
  scope: &mut Scope<'_>,
  roots: &mut Vec<RootId>,
  call: NativeFunctionId,
  construct: Option<NativeConstructId>,
  name: GcString,
  length: u32,
) -> Result<GcObject, VmError> {
  let func = scope.alloc_native_function(call, construct, name, length)?;
  roots.push(scope.heap_mut().add_root(Value::Object(func))?);
  Ok(func)
}

fn alloc_rooted_symbol(
  scope: &mut Scope<'_>,
  roots: &mut Vec<RootId>,
  description: &str,
) -> Result<GcSymbol, VmError> {
  let desc_string = scope.alloc_string(description)?;
  let sym = scope.new_symbol(Some(desc_string))?;
  roots.push(scope.heap_mut().add_root(Value::Symbol(sym))?);
  Ok(sym)
}

fn init_native_error(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  roots: &mut Vec<RootId>,
  common: CommonKeys,
  constructor_prototype: GcObject,
  base_prototype: GcObject,
  to_string_tag: GcSymbol,
  call: NativeFunctionId,
  construct: NativeConstructId,
  name: &str,
  length: u32,
) -> Result<(GcObject, GcObject), VmError> {
  // `%X.prototype%`
  let prototype = alloc_rooted_object(scope, roots)?;
  scope
    .heap_mut()
    .object_set_prototype(prototype, Some(base_prototype))?;

  // `@@toStringTag` for native error prototypes is `"Error"` (ECMA-262).
  //
  // This ensures `Object.prototype.toString.call(new TypeError())` yields `"[object Error]"`.
  install_to_string_tag(scope, prototype, to_string_tag, "Error")?;

  // Create (and store) the name string early so it is kept alive by the rooted objects before any
  // subsequent allocations/GC.
  let name_string = scope.alloc_string(name)?;

  let constructor = alloc_rooted_native_function(
    scope,
    roots,
    call,
    Some(construct),
    name_string,
    length,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(constructor, Some(constructor_prototype))?;

  // X.prototype.constructor
  scope.define_property(
    prototype,
    common.constructor,
    data_desc(Value::Object(constructor), true, false, true),
  )?;
  // X.prototype.name
  scope.define_property(
    prototype,
    common.name,
    data_desc(Value::String(name_string), true, false, true),
  )?;

  // X.prototype on the constructor
  scope.define_property(
    constructor,
    common.prototype,
    // Per ECMA-262, constructor `.prototype` properties are writable but non-configurable.
    data_desc(Value::Object(prototype), true, false, false),
  )?;
  // X.name / X.length
  scope.define_property(
    constructor,
    common.name,
    data_desc(Value::String(name_string), false, false, true),
  )?;
  scope.define_property(
    constructor,
    common.length,
    data_desc(Value::Number(length as f64), false, false, true),
  )?;

  Ok((constructor, prototype))
}

fn install_object_static_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  roots: &mut Vec<RootId>,
  function_prototype: GcObject,
  object_constructor: GcObject,
  name: &str,
  length: u32,
  call: crate::vm::NativeCall,
) -> Result<(), VmError> {
  let call_id = vm.register_native_call(call)?;
  let name_string = scope.alloc_string(name)?;
  let func = alloc_rooted_native_function(scope, roots, call_id, None, name_string, length)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(function_prototype))?;

  scope.define_property(
    object_constructor,
    PropertyKey::from_string(name_string),
    data_desc(Value::Object(func), true, false, true),
  )?;
  Ok(())
}

fn install_object_static_methods(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  roots: &mut Vec<RootId>,
  function_prototype: GcObject,
  object_constructor: GcObject,
) -> Result<(), VmError> {
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "defineProperty",
    3,
    builtins::object_define_property,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "defineProperties",
    2,
    builtins::object_define_properties,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "getOwnPropertyNames",
    1,
    builtins::object_get_own_property_names,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "getOwnPropertySymbols",
    1,
    builtins::object_get_own_property_symbols,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "create",
    2,
    builtins::object_create,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "is",
    2,
    builtins::object_is,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "hasOwn",
    2,
    builtins::object_has_own,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "getOwnPropertyDescriptor",
    2,
    builtins::object_get_own_property_descriptor,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "getOwnPropertyDescriptors",
    1,
    builtins::object_get_own_property_descriptors,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "preventExtensions",
    1,
    builtins::object_prevent_extensions,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "isExtensible",
    1,
    builtins::object_is_extensible,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "seal",
    1,
    builtins::object_seal,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "isSealed",
    1,
    builtins::object_is_sealed,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "freeze",
    1,
    builtins::object_freeze,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "isFrozen",
    1,
    builtins::object_is_frozen,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "keys",
    1,
    builtins::object_keys,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "values",
    1,
    builtins::object_values,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "entries",
    1,
    builtins::object_entries,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "fromEntries",
    1,
    builtins::object_from_entries,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "assign",
    2,
    builtins::object_assign,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "getPrototypeOf",
    1,
    builtins::object_get_prototype_of,
  )?;
  install_object_static_method(
    vm,
    scope,
    roots,
    function_prototype,
    object_constructor,
    "setPrototypeOf",
    2,
    builtins::object_set_prototype_of,
  )?;
  Ok(())
}

impl Intrinsics {
  pub(crate) fn init(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    roots: &mut Vec<RootId>,
  ) -> Result<Self, VmError> {
    let well_known_symbols = WellKnownSymbols::init(scope, roots)?;

    // --- Base prototypes ---
    let object_prototype = alloc_rooted_object(scope, roots)?;

    let function_prototype_call = vm.register_native_call(builtins::function_prototype_call)?;
    // ECMA-262: %Function.prototype% has a "name" property whose value is the empty String.
    let function_prototype_name = scope.alloc_string("")?;
    let function_prototype = alloc_rooted_native_function(
      scope,
      roots,
      function_prototype_call,
      None,
      function_prototype_name,
      0,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(function_prototype, Some(object_prototype))?;

    let iterator_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(iterator_prototype, Some(object_prototype))?;

    // `%ArrayIteratorPrototype%`
    let array_iterator_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(array_iterator_prototype, Some(iterator_prototype))?;

    // `%StringIteratorPrototype%`
    let string_iterator_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(string_iterator_prototype, Some(iterator_prototype))?;
    let array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(array_prototype, Some(object_prototype))?;

    let string_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(string_prototype, Some(object_prototype))?;

    let regexp_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(regexp_prototype, Some(object_prototype))?;

    let number_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(number_prototype, Some(object_prototype))?;

    let boolean_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(boolean_prototype, Some(object_prototype))?;

    let bigint_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(bigint_prototype, Some(object_prototype))?;

    let date_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(date_prototype, Some(object_prototype))?;

    let symbol_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(symbol_prototype, Some(object_prototype))?;

    let array_buffer_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(array_buffer_prototype, Some(object_prototype))?;

    let uint8_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(uint8_array_prototype, Some(object_prototype))?;

    let int8_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(int8_array_prototype, Some(object_prototype))?;

    let uint8_clamped_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(uint8_clamped_array_prototype, Some(object_prototype))?;

    let int16_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(int16_array_prototype, Some(object_prototype))?;

    let uint16_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(uint16_array_prototype, Some(object_prototype))?;

    let int32_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(int32_array_prototype, Some(object_prototype))?;

    let uint32_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(uint32_array_prototype, Some(object_prototype))?;

    let float32_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(float32_array_prototype, Some(object_prototype))?;

    let float64_array_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(float64_array_prototype, Some(object_prototype))?;

    let data_view_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(data_view_prototype, Some(object_prototype))?;

    // `%WeakMap.prototype%` / `%WeakSet.prototype%` (minimal).
    //
    // These prototypes are currently used for `Object.prototype.toString` tagging via
    // `@@toStringTag`.
    let weak_map_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(weak_map_prototype, Some(object_prototype))?;
    let weak_set_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(weak_set_prototype, Some(object_prototype))?;

    // `@@toStringTag` on intrinsic prototypes (ECMA-262).
    //
    // These are consulted by `Object.prototype.toString` via `Get(O, @@toStringTag)`.
    install_to_string_tag(
      scope,
      function_prototype,
      well_known_symbols.to_string_tag,
      "Function",
    )?;
    install_to_string_tag(scope, array_prototype, well_known_symbols.to_string_tag, "Array")?;
    install_to_string_tag(scope, string_prototype, well_known_symbols.to_string_tag, "String")?;
    install_to_string_tag(scope, number_prototype, well_known_symbols.to_string_tag, "Number")?;
    install_to_string_tag(
      scope,
      boolean_prototype,
      well_known_symbols.to_string_tag,
      "Boolean",
    )?;
    install_to_string_tag(scope, bigint_prototype, well_known_symbols.to_string_tag, "BigInt")?;
    install_to_string_tag(scope, date_prototype, well_known_symbols.to_string_tag, "Date")?;
    install_to_string_tag(scope, symbol_prototype, well_known_symbols.to_string_tag, "Symbol")?;
    install_to_string_tag(
      scope,
      array_buffer_prototype,
      well_known_symbols.to_string_tag,
      "ArrayBuffer",
    )?;
    install_to_string_tag(
      scope,
      uint8_array_prototype,
      well_known_symbols.to_string_tag,
      "Uint8Array",
    )?;
    install_to_string_tag(scope, int8_array_prototype, well_known_symbols.to_string_tag, "Int8Array")?;
    install_to_string_tag(
      scope,
      uint8_clamped_array_prototype,
      well_known_symbols.to_string_tag,
      "Uint8ClampedArray",
    )?;
    install_to_string_tag(scope, int16_array_prototype, well_known_symbols.to_string_tag, "Int16Array")?;
    install_to_string_tag(
      scope,
      uint16_array_prototype,
      well_known_symbols.to_string_tag,
      "Uint16Array",
    )?;
    install_to_string_tag(scope, int32_array_prototype, well_known_symbols.to_string_tag, "Int32Array")?;
    install_to_string_tag(
      scope,
      uint32_array_prototype,
      well_known_symbols.to_string_tag,
      "Uint32Array",
    )?;
    install_to_string_tag(
      scope,
      float32_array_prototype,
      well_known_symbols.to_string_tag,
      "Float32Array",
    )?;
    install_to_string_tag(
      scope,
      float64_array_prototype,
      well_known_symbols.to_string_tag,
      "Float64Array",
    )?;
    install_to_string_tag(scope, data_view_prototype, well_known_symbols.to_string_tag, "DataView")?;
    install_to_string_tag(scope, weak_map_prototype, well_known_symbols.to_string_tag, "WeakMap")?;
    install_to_string_tag(scope, weak_set_prototype, well_known_symbols.to_string_tag, "WeakSet")?;

    // --- Common property keys used throughout the intrinsic graph ---
    //
    // Root these key strings for the duration of intrinsic initialization: subsequent allocations
    // may trigger GC before we store the keys on any rooted object.
    let constructor_key_s = scope.alloc_string("constructor")?;
    scope.push_root(Value::String(constructor_key_s))?;
    let prototype_key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(prototype_key_s))?;
    let name_key_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(name_key_s))?;
    let length_key_s = scope.alloc_string("length")?;
    scope.push_root(Value::String(length_key_s))?;

    let common = CommonKeys {
      constructor: PropertyKey::from_string(constructor_key_s),
      prototype: PropertyKey::from_string(prototype_key_s),
      name: PropertyKey::from_string(name_key_s),
      length: PropertyKey::from_string(length_key_s),
    };

    // --- Prototype/native method call handlers ---
    let object_prototype_to_string = vm.register_native_call(builtins::object_prototype_to_string)?;
    let object_prototype_has_own_property =
      vm.register_native_call(builtins::object_prototype_has_own_property)?;
    let object_prototype_proto_get =
      vm.register_native_call(builtins::object_prototype___proto___get)?;
    let object_prototype_proto_set =
      vm.register_native_call(builtins::object_prototype___proto___set)?;
    let object_prototype_is_prototype_of =
      vm.register_native_call(builtins::object_prototype_is_prototype_of)?;
    let object_prototype_property_is_enumerable =
      vm.register_native_call(builtins::object_prototype_property_is_enumerable)?;
    let object_prototype_to_locale_string =
      vm.register_native_call(builtins::object_prototype_to_locale_string)?;
    let function_prototype_call_method =
      vm.register_native_call(builtins::function_prototype_call_method)?;
    let function_prototype_apply_method =
      vm.register_native_call(builtins::function_prototype_apply)?;
    let function_prototype_bind_method =
      vm.register_native_call(builtins::function_prototype_bind)?;
    let function_prototype_to_string_method =
      vm.register_native_call(builtins::function_prototype_to_string)?;
    let function_prototype_symbol_has_instance =
      vm.register_native_call(builtins::function_prototype_symbol_has_instance)?;
    let throw_type_error_intrinsic_call =
      vm.register_native_call(builtins::throw_type_error_intrinsic)?;
    let array_prototype_map = vm.register_native_call(builtins::array_prototype_map)?;
    let array_prototype_for_each = vm.register_native_call(builtins::array_prototype_for_each)?;
    let array_prototype_index_of = vm.register_native_call(builtins::array_prototype_index_of)?;
    let array_prototype_includes = vm.register_native_call(builtins::array_prototype_includes)?;
    let array_prototype_filter = vm.register_native_call(builtins::array_prototype_filter)?;
    let array_prototype_reduce = vm.register_native_call(builtins::array_prototype_reduce)?;
    let array_prototype_some = vm.register_native_call(builtins::array_prototype_some)?;
    let array_prototype_every = vm.register_native_call(builtins::array_prototype_every)?;
    let array_prototype_find = vm.register_native_call(builtins::array_prototype_find)?;
    let array_prototype_find_index = vm.register_native_call(builtins::array_prototype_find_index)?;
    let array_prototype_concat = vm.register_native_call(builtins::array_prototype_concat)?;
    let array_prototype_reverse = vm.register_native_call(builtins::array_prototype_reverse)?;
    let array_prototype_sort = vm.register_native_call(builtins::array_prototype_sort)?;
    let array_prototype_join = vm.register_native_call(builtins::array_prototype_join)?;
    let array_prototype_slice = vm.register_native_call(builtins::array_prototype_slice)?;
    let array_prototype_push = vm.register_native_call(builtins::array_prototype_push)?;
    let array_prototype_pop = vm.register_native_call(builtins::array_prototype_pop)?;
    let array_prototype_shift = vm.register_native_call(builtins::array_prototype_shift)?;
    let array_prototype_unshift = vm.register_native_call(builtins::array_prototype_unshift)?;
    let array_prototype_splice = vm.register_native_call(builtins::array_prototype_splice)?;
    let array_is_array = vm.register_native_call(builtins::array_is_array)?;
    let array_constructor_from = vm.register_native_call(builtins::array_constructor_from)?;
    let array_prototype_keys = vm.register_native_call(builtins::array_prototype_keys)?;
    let array_prototype_entries = vm.register_native_call(builtins::array_prototype_entries)?;
    let array_prototype_values = vm.register_native_call(builtins::array_prototype_values)?;
    let array_iterator_next_call = vm.register_native_call(builtins::array_iterator_next)?;
    let iterator_prototype_iterator = vm.register_native_call(builtins::iterator_prototype_iterator)?;
    let string_prototype_to_string = vm.register_native_call(builtins::string_prototype_to_string)?;
    let string_prototype_char_code_at =
      vm.register_native_call(builtins::string_prototype_char_code_at)?;
    let string_prototype_char_at = vm.register_native_call(builtins::string_prototype_char_at)?;
    let string_from_char_code = vm.register_native_call(builtins::string_from_char_code)?;
    let string_from_code_point = vm.register_native_call(builtins::string_from_code_point)?;
    let string_raw = vm.register_native_call(builtins::string_raw)?;
    let string_prototype_trim = vm.register_native_call(builtins::string_prototype_trim)?;
    let string_prototype_trim_start = vm.register_native_call(builtins::string_prototype_trim_start)?;
    let string_prototype_trim_end = vm.register_native_call(builtins::string_prototype_trim_end)?;
    let string_prototype_substring = vm.register_native_call(builtins::string_prototype_substring)?;
    let string_prototype_substr = vm.register_native_call(builtins::string_prototype_substr)?;
    let string_prototype_match = vm.register_native_call(builtins::string_prototype_match)?;
    let string_prototype_match_all = vm.register_native_call(builtins::string_prototype_match_all)?;
    let string_prototype_search = vm.register_native_call(builtins::string_prototype_search)?;
    let string_prototype_replace = vm.register_native_call(builtins::string_prototype_replace)?;
    let string_prototype_replace_all =
      vm.register_native_call(builtins::string_prototype_replace_all)?;
    let string_prototype_split = vm.register_native_call(builtins::string_prototype_split)?;
    let string_prototype_repeat = vm.register_native_call(builtins::string_prototype_repeat)?;
    let string_prototype_code_point_at =
      vm.register_native_call(builtins::string_prototype_code_point_at)?;
    let string_prototype_at = vm.register_native_call(builtins::string_prototype_at)?;
    let string_prototype_pad_start = vm.register_native_call(builtins::string_prototype_pad_start)?;
    let string_prototype_pad_end = vm.register_native_call(builtins::string_prototype_pad_end)?;
    let string_prototype_to_lower_case =
      vm.register_native_call(builtins::string_prototype_to_lower_case)?;
    let string_prototype_to_upper_case =
      vm.register_native_call(builtins::string_prototype_to_upper_case)?;
    let string_prototype_slice = vm.register_native_call(builtins::string_prototype_slice)?;
    let string_prototype_index_of = vm.register_native_call(builtins::string_prototype_index_of)?;
    let string_prototype_includes = vm.register_native_call(builtins::string_prototype_includes)?;
    let string_prototype_starts_with = vm.register_native_call(builtins::string_prototype_starts_with)?;
    let string_prototype_ends_with = vm.register_native_call(builtins::string_prototype_ends_with)?;
    let string_prototype_iterator = vm.register_native_call(builtins::string_prototype_iterator)?;
    let string_iterator_next = vm.register_native_call(builtins::string_iterator_next)?;
    let regexp_prototype_exec = vm.register_native_call(builtins::regexp_prototype_exec)?;
    let regexp_prototype_test = vm.register_native_call(builtins::regexp_prototype_test)?;
    let regexp_prototype_source_get =
      vm.register_native_call(builtins::regexp_prototype_source_get)?;
    let regexp_prototype_flags_get = vm.register_native_call(builtins::regexp_prototype_flags_get)?;
    let regexp_prototype_symbol_match =
      vm.register_native_call(builtins::regexp_prototype_symbol_match)?;
    let regexp_prototype_symbol_search =
      vm.register_native_call(builtins::regexp_prototype_symbol_search)?;
    let regexp_prototype_symbol_replace =
      vm.register_native_call(builtins::regexp_prototype_symbol_replace)?;
    let regexp_prototype_symbol_split =
      vm.register_native_call(builtins::regexp_prototype_symbol_split)?;
    let regexp_prototype_symbol_match_all =
      vm.register_native_call(builtins::regexp_prototype_symbol_match_all)?;
    let regexp_string_iterator_next = vm.register_native_call(builtins::regexp_string_iterator_next)?;
    let iterator_prototype_symbol_iterator =
      vm.register_native_call(builtins::iterator_prototype_symbol_iterator)?;
    let number_prototype_value_of = vm.register_native_call(builtins::number_prototype_value_of)?;
    let number_prototype_to_string = vm.register_native_call(builtins::number_prototype_to_string)?;
    let number_prototype_to_fixed = vm.register_native_call(builtins::number_prototype_to_fixed)?;
    let number_prototype_to_exponential =
      vm.register_native_call(builtins::number_prototype_to_exponential)?;
    let number_prototype_to_precision =
      vm.register_native_call(builtins::number_prototype_to_precision)?;
    let number_prototype_to_locale_string =
      vm.register_native_call(builtins::number_prototype_to_locale_string)?;
    let boolean_prototype_value_of = vm.register_native_call(builtins::boolean_prototype_value_of)?;
    let boolean_prototype_to_string = vm.register_native_call(builtins::boolean_prototype_to_string)?;
    let number_is_nan = vm.register_native_call(builtins::number_is_nan)?;
    let number_is_finite = vm.register_native_call(builtins::number_is_finite)?;
    let number_is_integer = vm.register_native_call(builtins::number_is_integer)?;
    let number_is_safe_integer = vm.register_native_call(builtins::number_is_safe_integer)?;
    let bigint_prototype_value_of = vm.register_native_call(builtins::bigint_prototype_value_of)?;
    let date_prototype_to_string = vm.register_native_call(builtins::date_prototype_to_string)?;
    let date_prototype_to_utc_string = vm.register_native_call(builtins::date_prototype_to_utc_string)?;
    let date_prototype_to_iso_string = vm.register_native_call(builtins::date_prototype_to_iso_string)?;
    let date_prototype_get_time = vm.register_native_call(builtins::date_prototype_get_time)?;
    let date_prototype_value_of = vm.register_native_call(builtins::date_prototype_value_of)?;
    let date_prototype_to_primitive = vm.register_native_call(builtins::date_prototype_to_primitive)?;
    let date_now = vm.register_native_call(builtins::date_now)?;
    let date_parse = vm.register_native_call(builtins::date_parse)?;
    let date_utc = vm.register_native_call(builtins::date_utc)?;
    let symbol_prototype_value_of = vm.register_native_call(builtins::symbol_prototype_value_of)?;
    let symbol_prototype_to_string = vm.register_native_call(builtins::symbol_prototype_to_string)?;
    let symbol_prototype_to_primitive =
      vm.register_native_call(builtins::symbol_prototype_to_primitive)?;
    let symbol_prototype_description_get =
      vm.register_native_call(builtins::symbol_prototype_description_get)?;
    let symbol_for = vm.register_native_call(builtins::symbol_for)?;
    let symbol_key_for = vm.register_native_call(builtins::symbol_key_for)?;
    let error_prototype_to_string = vm.register_native_call(builtins::error_prototype_to_string)?;
    let json_parse = vm.register_native_call(builtins::json_parse)?;
    let json_stringify = vm.register_native_call(builtins::json_stringify)?;
    let math_abs = vm.register_native_call(builtins::math_abs)?;
    let math_acos = vm.register_native_call(builtins::math_acos)?;
    let math_acosh = vm.register_native_call(builtins::math_acosh)?;
    let math_asin = vm.register_native_call(builtins::math_asin)?;
    let math_asinh = vm.register_native_call(builtins::math_asinh)?;
    let math_atan = vm.register_native_call(builtins::math_atan)?;
    let math_atan2 = vm.register_native_call(builtins::math_atan2)?;
    let math_atanh = vm.register_native_call(builtins::math_atanh)?;
    let math_cbrt = vm.register_native_call(builtins::math_cbrt)?;
    let math_floor = vm.register_native_call(builtins::math_floor)?;
    let math_clz32 = vm.register_native_call(builtins::math_clz32)?;
    let math_ceil = vm.register_native_call(builtins::math_ceil)?;
    let math_cos = vm.register_native_call(builtins::math_cos)?;
    let math_cosh = vm.register_native_call(builtins::math_cosh)?;
    let math_expm1 = vm.register_native_call(builtins::math_expm1)?;
    let math_fround = vm.register_native_call(builtins::math_fround)?;
    let math_hypot = vm.register_native_call(builtins::math_hypot)?;
    let math_imul = vm.register_native_call(builtins::math_imul)?;
    let math_log1p = vm.register_native_call(builtins::math_log1p)?;
    let math_log10 = vm.register_native_call(builtins::math_log10)?;
    let math_log2 = vm.register_native_call(builtins::math_log2)?;
    let math_trunc = vm.register_native_call(builtins::math_trunc)?;
    let math_round = vm.register_native_call(builtins::math_round)?;
    let math_max = vm.register_native_call(builtins::math_max)?;
    let math_min = vm.register_native_call(builtins::math_min)?;
    let math_pow = vm.register_native_call(builtins::math_pow)?;
    let math_sqrt = vm.register_native_call(builtins::math_sqrt)?;
    let math_log = vm.register_native_call(builtins::math_log)?;
    let math_exp = vm.register_native_call(builtins::math_exp)?;
    let math_sign = vm.register_native_call(builtins::math_sign)?;
    let math_sin = vm.register_native_call(builtins::math_sin)?;
    let math_sinh = vm.register_native_call(builtins::math_sinh)?;
    let math_tan = vm.register_native_call(builtins::math_tan)?;
    let math_tanh = vm.register_native_call(builtins::math_tanh)?;
    let math_random = vm.register_native_call(builtins::math_random)?;
    let reflect_apply = vm.register_native_call(builtins::reflect_apply)?;
    let reflect_construct = vm.register_native_call(builtins::reflect_construct)?;
    let reflect_define_property = vm.register_native_call(builtins::reflect_define_property)?;
    let reflect_delete_property = vm.register_native_call(builtins::reflect_delete_property)?;
    let reflect_get = vm.register_native_call(builtins::reflect_get)?;
    let reflect_get_own_property_descriptor =
      vm.register_native_call(builtins::reflect_get_own_property_descriptor)?;
    let reflect_get_prototype_of = vm.register_native_call(builtins::reflect_get_prototype_of)?;
    let reflect_has = vm.register_native_call(builtins::reflect_has)?;
    let reflect_is_extensible = vm.register_native_call(builtins::reflect_is_extensible)?;
    let reflect_own_keys = vm.register_native_call(builtins::reflect_own_keys)?;
    let reflect_prevent_extensions = vm.register_native_call(builtins::reflect_prevent_extensions)?;
    let reflect_set = vm.register_native_call(builtins::reflect_set)?;
    let reflect_set_prototype_of = vm.register_native_call(builtins::reflect_set_prototype_of)?;

    // Generator intrinsics.
    let generator_function_constructor_call =
      vm.register_native_call(builtins::generator_function_constructor_call)?;
    let generator_function_constructor_construct = vm.register_native_construct(
      builtins::generator_function_constructor_construct,
    )?;
    let generator_prototype_next = vm.register_native_call(builtins::generator_prototype_next)?;
    let generator_prototype_return = vm.register_native_call(builtins::generator_prototype_return)?;
    let generator_prototype_throw = vm.register_native_call(builtins::generator_prototype_throw)?;

    // `%Number%`, `%Boolean%`, `%Date%`, and global functions.
    let number_call = vm.register_native_call(builtins::number_constructor_call)?;
    let number_construct = vm.register_native_construct(builtins::number_constructor_construct)?;
    let boolean_call = vm.register_native_call(builtins::boolean_constructor_call)?;
    let boolean_construct = vm.register_native_construct(builtins::boolean_constructor_construct)?;
    let date_call = vm.register_native_call(builtins::date_constructor_call)?;
    let date_construct = vm.register_native_construct(builtins::date_constructor_construct)?;
    let eval_call = vm.register_native_call(builtins::global_eval)?;
    let is_nan_call = vm.register_native_call(builtins::global_is_nan)?;
    let is_finite_call = vm.register_native_call(builtins::global_is_finite)?;
    let parse_int_call = vm.register_native_call(builtins::global_parse_int)?;
    let parse_float_call = vm.register_native_call(builtins::global_parse_float)?;
    let encode_uri_call = vm.register_native_call(builtins::global_encode_uri)?;
    let encode_uri_component_call = vm.register_native_call(builtins::global_encode_uri_component)?;
    let decode_uri_call = vm.register_native_call(builtins::global_decode_uri)?;
    let decode_uri_component_call = vm.register_native_call(builtins::global_decode_uri_component)?;

    // `%IteratorPrototype%[@@iterator]`
    {
      let iter_name = scope.alloc_string("[Symbol.iterator]")?;
      scope.push_root(Value::String(iter_name))?;
      let iter_fn = scope.alloc_native_function(iterator_prototype_iterator, None, iter_name, 0)?;
      scope.push_root(Value::Object(iter_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(iter_fn, Some(function_prototype))?;
      scope.define_property(
        iterator_prototype,
        PropertyKey::Symbol(well_known_symbols.iterator),
        data_desc(Value::Object(iter_fn), true, false, true),
      )?;
    }

    // --- Baseline constructors ---
    // `%Object%`
    let object_call = vm.register_native_call(builtins::object_constructor_call)?;
    let object_construct =
      vm.register_native_construct(builtins::object_constructor_construct)?;
    let object_name = scope.alloc_string("Object")?;
    let object_constructor = alloc_rooted_native_function(
      scope,
      roots,
      object_call,
      Some(object_construct),
      object_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(object_constructor, Some(function_prototype))?;
    scope.define_property(
      object_constructor,
      common.prototype,
      data_desc(Value::Object(object_prototype), false, false, false),
    )?;
    scope.define_property(
      object_constructor,
      common.name,
      data_desc(Value::String(object_name), false, false, true),
    )?;
    scope.define_property(
      object_constructor,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      object_prototype,
      common.constructor,
      data_desc(Value::Object(object_constructor), true, false, true),
    )?;

    install_object_static_methods(vm, scope, roots, function_prototype, object_constructor)?;

      // Object.prototype.toString
      {
        let to_string_s = scope.alloc_string("toString")?;
        scope.push_root(Value::String(to_string_s))?;
        let key = PropertyKey::from_string(to_string_s);
        let func = scope.alloc_native_function(object_prototype_to_string, None, to_string_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
       scope.define_property(
         object_prototype,
         key,
         data_desc(Value::Object(func), true, false, true),
       )?;
     }

      // Annex B `Object.prototype.__proto__` (getter/setter).
      {
        let key_s = scope.alloc_string("__proto__")?;
        scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);

        let get_name = scope.alloc_string("get __proto__")?;
        scope.push_root(Value::String(get_name))?;
        let get = scope.alloc_native_function(object_prototype_proto_get, None, get_name, 0)?;
        scope.push_root(Value::Object(get))?;
        scope
          .heap_mut()
          .object_set_prototype(get, Some(function_prototype))?;

        let set_name = scope.alloc_string("set __proto__")?;
        scope.push_root(Value::String(set_name))?;
        let set = scope.alloc_native_function(object_prototype_proto_set, None, set_name, 1)?;
        scope.push_root(Value::Object(set))?;
        scope
          .heap_mut()
          .object_set_prototype(set, Some(function_prototype))?;

        scope.define_property(
          object_prototype,
          key,
          PropertyDescriptor {
            enumerable: false,
            configurable: true,
            kind: PropertyKind::Accessor {
              get: Value::Object(get),
              set: Value::Object(set),
            },
          },
        )?;
      }

      // Object.prototype.hasOwnProperty
      {
        let has_own_s = scope.alloc_string("hasOwnProperty")?;
        scope.push_root(Value::String(has_own_s))?;
        let key = PropertyKey::from_string(has_own_s);
        let func =
          scope.alloc_native_function(object_prototype_has_own_property, None, has_own_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
       scope.define_property(
          object_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // Object.prototype.isPrototypeOf
      {
        let is_prototype_of_s = scope.alloc_string("isPrototypeOf")?;
        scope.push_root(Value::String(is_prototype_of_s))?;
        let key = PropertyKey::from_string(is_prototype_of_s);
        let func = scope.alloc_native_function(
          object_prototype_is_prototype_of,
          None,
          is_prototype_of_s,
          1,
        )?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          object_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // Object.prototype.propertyIsEnumerable
      {
        let property_is_enumerable_s = scope.alloc_string("propertyIsEnumerable")?;
        scope.push_root(Value::String(property_is_enumerable_s))?;
        let key = PropertyKey::from_string(property_is_enumerable_s);
        let func = scope.alloc_native_function(
          object_prototype_property_is_enumerable,
          None,
          property_is_enumerable_s,
          1,
        )?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          object_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // Object.prototype.toLocaleString
      {
        let to_locale_string_s = scope.alloc_string("toLocaleString")?;
        scope.push_root(Value::String(to_locale_string_s))?;
        let key = PropertyKey::from_string(to_locale_string_s);
        let func = scope.alloc_native_function(
          object_prototype_to_locale_string,
          None,
          to_locale_string_s,
          0,
        )?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          object_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }
    // `%Function%`
    let function_call = vm.register_native_call(builtins::function_constructor_call)?;
    let function_construct =
      vm.register_native_construct(builtins::function_constructor_construct)?;
    let function_name = scope.alloc_string("Function")?;
    let function_constructor = alloc_rooted_native_function(
      scope,
      roots,
      function_call,
      Some(function_construct),
      function_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(function_constructor, Some(function_prototype))?;
    scope.define_property(
      function_constructor,
      common.prototype,
      data_desc(Value::Object(function_prototype), false, false, false),
    )?;
    scope.define_property(
      function_constructor,
      common.name,
      data_desc(Value::String(function_name), false, false, true),
    )?;
    scope.define_property(
      function_constructor,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      function_prototype,
      common.constructor,
      data_desc(Value::Object(function_constructor), true, false, true),
    )?;

      // Function.prototype.call
      {
        let call_s = scope.alloc_string("call")?;
        scope.push_root(Value::String(call_s))?;
        let key = PropertyKey::from_string(call_s);
        let func =
          scope.alloc_native_function(function_prototype_call_method, None, call_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        function_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Function.prototype.apply
    {
      let apply_s = scope.alloc_string("apply")?;
      scope.push_root(Value::String(apply_s))?;
      let key = PropertyKey::from_string(apply_s);
      let func =
        scope.alloc_native_function(function_prototype_apply_method, None, apply_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        function_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Function.prototype.bind
    {
      let bind_s = scope.alloc_string("bind")?;
      scope.push_root(Value::String(bind_s))?;
      let key = PropertyKey::from_string(bind_s);
      let func =
        scope.alloc_native_function(function_prototype_bind_method, None, bind_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        function_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Function.prototype.toString
    {
      let to_string_s = scope.alloc_string("toString")?;
      scope.push_root(Value::String(to_string_s))?;
      let key = PropertyKey::from_string(to_string_s);
      let func =
        scope.alloc_native_function(function_prototype_to_string_method, None, to_string_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        function_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Function.prototype[@@hasInstance]
    //
    // Spec: https://tc39.es/ecma262/#sec-function.prototype-@@hasinstance
    {
      let has_instance_s = scope.alloc_string("[Symbol.hasInstance]")?;
      scope.push_root(Value::String(has_instance_s))?;
      let func = scope.alloc_native_function(
        function_prototype_symbol_has_instance,
        None,
        has_instance_s,
        1,
      )?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        function_prototype,
        PropertyKey::Symbol(well_known_symbols.has_instance),
        // ECMA-262: Function.prototype[@@hasInstance] is non-writable, non-enumerable,
        // non-configurable.
        data_desc(Value::Object(func), false, false, false),
      )?;
    }

    // Function.prototype.caller / Function.prototype.arguments (restricted properties).
    {
      let thrower_name = scope.alloc_string("%ThrowTypeError%")?;
      let thrower_fn = alloc_rooted_native_function(
        scope,
        roots,
        throw_type_error_intrinsic_call,
        None,
        thrower_name,
        0,
      )?;
      scope
        .heap_mut()
        .object_set_prototype(thrower_fn, Some(function_prototype))?;

      let caller_s = scope.alloc_string("caller")?;
      scope.push_root(Value::String(caller_s))?;
      let caller_key = PropertyKey::from_string(caller_s);
      scope.define_property(
        function_prototype,
        caller_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(thrower_fn),
            set: Value::Object(thrower_fn),
          },
        },
      )?;

      let arguments_s = scope.alloc_string("arguments")?;
      scope.push_root(Value::String(arguments_s))?;
      let arguments_key = PropertyKey::from_string(arguments_s);
      scope.define_property(
        function_prototype,
        arguments_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(thrower_fn),
            set: Value::Object(thrower_fn),
          },
        },
      )?;
    }

    // `%GeneratorFunction.prototype%`
    let generator_function_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(generator_function_prototype, Some(function_prototype))?;

    // `%GeneratorPrototype%`
    let generator_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(generator_prototype, Some(iterator_prototype))?;

    // `%GeneratorFunction%`
    let generator_function_name = scope.alloc_string("GeneratorFunction")?;
    let generator_function = alloc_rooted_native_function(
      scope,
      roots,
      generator_function_constructor_call,
      Some(generator_function_constructor_construct),
      generator_function_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(generator_function, Some(function_constructor))?;
    // Override `.prototype` with the spec-required (non-writable, non-configurable) value.
    scope.define_property(
      generator_function,
      common.prototype,
      data_desc(
        Value::Object(generator_function_prototype),
        false,
        false,
        false,
      ),
    )?;

    // GeneratorFunction.prototype.constructor
    scope.define_property(
      generator_function_prototype,
      common.constructor,
      data_desc(Value::Object(generator_function), false, false, true),
    )?;
    // GeneratorFunction.prototype.prototype
    scope.define_property(
      generator_function_prototype,
      common.prototype,
      data_desc(Value::Object(generator_prototype), false, false, true),
    )?;
    // GeneratorFunction.prototype[@@toStringTag]
    {
      let tag = scope.alloc_string("GeneratorFunction")?;
      scope.define_property(
        generator_function_prototype,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(tag), false, false, true),
      )?;
    }

    // GeneratorPrototype.constructor
    scope.define_property(
      generator_prototype,
      common.constructor,
      data_desc(Value::Object(generator_function_prototype), false, false, true),
    )?;
    // GeneratorPrototype.next
    {
      let next_s = scope.alloc_string("next")?;
      scope.push_root(Value::String(next_s))?;
      let key = PropertyKey::from_string(next_s);
      let func = scope.alloc_native_function(generator_prototype_next, None, next_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        generator_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }
    // GeneratorPrototype.return
    {
      let return_s = scope.alloc_string("return")?;
      scope.push_root(Value::String(return_s))?;
      let key = PropertyKey::from_string(return_s);
      let func = scope.alloc_native_function(generator_prototype_return, None, return_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        generator_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }
    // GeneratorPrototype.throw
    {
      let throw_s = scope.alloc_string("throw")?;
      scope.push_root(Value::String(throw_s))?;
      let key = PropertyKey::from_string(throw_s);
      let func = scope.alloc_native_function(generator_prototype_throw, None, throw_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        generator_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }
    // GeneratorPrototype[@@toStringTag]
    {
      let tag = scope.alloc_string("Generator")?;
      scope.define_property(
        generator_prototype,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(tag), false, false, true),
      )?;
    }

    // `%Array%`
    let array_call = vm.register_native_call(builtins::array_constructor_call)?;
    let array_construct = vm.register_native_construct(builtins::array_constructor_construct)?;
    let array_name = scope.alloc_string("Array")?;
    let array_constructor = alloc_rooted_native_function(
      scope,
      roots,
      array_call,
      Some(array_construct),
      array_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(array_constructor, Some(function_prototype))?;
    scope.define_property(
      array_constructor,
      common.prototype,
      data_desc(Value::Object(array_prototype), false, false, false),
    )?;
    scope.define_property(
      array_constructor,
      common.name,
      data_desc(Value::String(array_name), false, false, true),
    )?;
    scope.define_property(
      array_constructor,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      array_prototype,
      common.constructor,
      data_desc(Value::Object(array_constructor), true, false, true),
    )?;

    // Array.isArray
    {
      let is_array_s = scope.alloc_string("isArray")?;
      scope.push_root(Value::String(is_array_s))?;
      let key = PropertyKey::from_string(is_array_s);
      let func = scope.alloc_native_function(array_is_array, None, is_array_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        array_constructor,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Array.from
    {
      let from_s = scope.alloc_string("from")?;
      scope.push_root(Value::String(from_s))?;
      let key = PropertyKey::from_string(from_s);
      let func = scope.alloc_native_function(array_constructor_from, None, from_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        array_constructor,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // `%Proxy%`
    //
    // This is currently minimal: it supports creation/revocation and the spec `IsArray`
    // interaction, but does not implement Proxy trap semantics.
    let proxy_call = vm.register_native_call(builtins::proxy_constructor_call)?;
    let proxy_construct = vm.register_native_construct(builtins::proxy_constructor_construct)?;
    let proxy_revocable = vm.register_native_call(builtins::proxy_revocable)?;
    let proxy_revoker_call = vm.register_native_call(builtins::proxy_revoker)?;
    let proxy_name = scope.alloc_string("Proxy")?;
    // `%Proxy%` is constructible, but does not have an own `"prototype"` property (ECMA-262,
    // test262 `built-ins/Proxy/proxy-no-prototype.js`).
    let proxy_constructor = scope.alloc_native_function_with_slots_and_env_no_constructor_prototype(
      proxy_call,
      Some(proxy_construct),
      proxy_name,
      2,
      &[],
      None,
    )?;
    roots.push(scope.heap_mut().add_root(Value::Object(proxy_constructor))?);
    scope
      .heap_mut()
      .object_set_prototype(proxy_constructor, Some(function_prototype))?;
    scope.define_property(
      proxy_constructor,
      common.name,
      data_desc(Value::String(proxy_name), false, false, true),
    )?;
    scope.define_property(
      proxy_constructor,
      common.length,
      data_desc(Value::Number(2.0), false, false, true),
    )?;

    // `Proxy.revocable`
    {
      let revocable_s = scope.alloc_string("revocable")?;
      scope.push_root(Value::String(revocable_s))?;
      let key = PropertyKey::from_string(revocable_s);
      let func = scope.alloc_native_function(proxy_revocable, None, revocable_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        proxy_constructor,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // `%ArrayIteratorPrototype%.next` / `ArrayIterator.prototype.next` (minimal).
    let array_iterator_next_name = scope.alloc_string("next")?;
    let array_iterator_next = alloc_rooted_native_function(
      scope,
      roots,
      array_iterator_next_call,
      None,
      array_iterator_next_name,
      0,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(array_iterator_next, Some(function_prototype))?;
    scope.define_property(
      array_iterator_prototype,
      PropertyKey::from_string(array_iterator_next_name),
      data_desc(Value::Object(array_iterator_next), true, false, true),
    )?;
    let array_iterator_tag = scope.alloc_string("Array Iterator")?;
    scope.define_property(
      array_iterator_prototype,
      PropertyKey::Symbol(well_known_symbols.to_string_tag),
      data_desc(Value::String(array_iterator_tag), false, false, true),
    )?;

      // Array.prototype.map / forEach / indexOf / includes / filter / reduce / some / every / find / findIndex / concat / reverse / sort / join / slice / push / pop / shift / unshift / splice
      {
        let map_s = scope.alloc_string("map")?;
        scope.push_root(Value::String(map_s))?;
        let map_key = PropertyKey::from_string(map_s);
        let map_fn = scope.alloc_native_function(array_prototype_map, None, map_s, 1)?;
        scope.push_root(Value::Object(map_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(map_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        map_key,
        data_desc(Value::Object(map_fn), true, false, true),
      )?;

        let for_each_s = scope.alloc_string("forEach")?;
        scope.push_root(Value::String(for_each_s))?;
        let for_each_key = PropertyKey::from_string(for_each_s);
        let for_each_fn = scope.alloc_native_function(array_prototype_for_each, None, for_each_s, 1)?;
        scope.push_root(Value::Object(for_each_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(for_each_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        for_each_key,
        data_desc(Value::Object(for_each_fn), true, false, true),
      )?;

        let index_of_s = scope.alloc_string("indexOf")?;
        scope.push_root(Value::String(index_of_s))?;
        let index_of_key = PropertyKey::from_string(index_of_s);
        let index_of_fn =
          scope.alloc_native_function(array_prototype_index_of, None, index_of_s, 1)?;
        scope.push_root(Value::Object(index_of_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(index_of_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          index_of_key,
          data_desc(Value::Object(index_of_fn), true, false, true),
        )?;

        let includes_s = scope.alloc_string("includes")?;
        scope.push_root(Value::String(includes_s))?;
        let includes_key = PropertyKey::from_string(includes_s);
        let includes_fn =
          scope.alloc_native_function(array_prototype_includes, None, includes_s, 1)?;
        scope.push_root(Value::Object(includes_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(includes_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          includes_key,
          data_desc(Value::Object(includes_fn), true, false, true),
        )?;

        let filter_s = scope.alloc_string("filter")?;
        scope.push_root(Value::String(filter_s))?;
        let filter_key = PropertyKey::from_string(filter_s);
        let filter_fn = scope.alloc_native_function(array_prototype_filter, None, filter_s, 1)?;
        scope.push_root(Value::Object(filter_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(filter_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          filter_key,
          data_desc(Value::Object(filter_fn), true, false, true),
        )?;

        let reduce_s = scope.alloc_string("reduce")?;
        scope.push_root(Value::String(reduce_s))?;
        let reduce_key = PropertyKey::from_string(reduce_s);
        let reduce_fn = scope.alloc_native_function(array_prototype_reduce, None, reduce_s, 1)?;
        scope.push_root(Value::Object(reduce_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(reduce_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          reduce_key,
          data_desc(Value::Object(reduce_fn), true, false, true),
        )?;

        let some_s = scope.alloc_string("some")?;
        scope.push_root(Value::String(some_s))?;
        let some_key = PropertyKey::from_string(some_s);
        let some_fn = scope.alloc_native_function(array_prototype_some, None, some_s, 1)?;
        scope.push_root(Value::Object(some_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(some_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          some_key,
          data_desc(Value::Object(some_fn), true, false, true),
        )?;

        let every_s = scope.alloc_string("every")?;
        scope.push_root(Value::String(every_s))?;
        let every_key = PropertyKey::from_string(every_s);
        let every_fn = scope.alloc_native_function(array_prototype_every, None, every_s, 1)?;
        scope.push_root(Value::Object(every_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(every_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          every_key,
          data_desc(Value::Object(every_fn), true, false, true),
        )?;

        let find_s = scope.alloc_string("find")?;
        scope.push_root(Value::String(find_s))?;
        let find_key = PropertyKey::from_string(find_s);
        let find_fn = scope.alloc_native_function(array_prototype_find, None, find_s, 1)?;
        scope.push_root(Value::Object(find_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(find_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          find_key,
          data_desc(Value::Object(find_fn), true, false, true),
        )?;

        let find_index_s = scope.alloc_string("findIndex")?;
        scope.push_root(Value::String(find_index_s))?;
        let find_index_key = PropertyKey::from_string(find_index_s);
        let find_index_fn =
          scope.alloc_native_function(array_prototype_find_index, None, find_index_s, 1)?;
        scope.push_root(Value::Object(find_index_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(find_index_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          find_index_key,
          data_desc(Value::Object(find_index_fn), true, false, true),
        )?;

        let concat_s = scope.alloc_string("concat")?;
        scope.push_root(Value::String(concat_s))?;
        let concat_key = PropertyKey::from_string(concat_s);
        let concat_fn = scope.alloc_native_function(array_prototype_concat, None, concat_s, 1)?;
        scope.push_root(Value::Object(concat_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(concat_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          concat_key,
          data_desc(Value::Object(concat_fn), true, false, true),
        )?;

        let reverse_s = scope.alloc_string("reverse")?;
        scope.push_root(Value::String(reverse_s))?;
        let reverse_key = PropertyKey::from_string(reverse_s);
        let reverse_fn = scope.alloc_native_function(array_prototype_reverse, None, reverse_s, 0)?;
        scope.push_root(Value::Object(reverse_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(reverse_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          reverse_key,
          data_desc(Value::Object(reverse_fn), true, false, true),
        )?;

        let sort_s = scope.alloc_string("sort")?;
        scope.push_root(Value::String(sort_s))?;
        let sort_key = PropertyKey::from_string(sort_s);
        let sort_fn = scope.alloc_native_function(array_prototype_sort, None, sort_s, 1)?;
        scope.push_root(Value::Object(sort_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(sort_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          sort_key,
          data_desc(Value::Object(sort_fn), true, false, true),
        )?;

        let join_s = scope.alloc_string("join")?;
        scope.push_root(Value::String(join_s))?;
        let join_key = PropertyKey::from_string(join_s);
        let join_fn = scope.alloc_native_function(array_prototype_join, None, join_s, 1)?;
        scope.push_root(Value::Object(join_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(join_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        join_key,
        data_desc(Value::Object(join_fn), true, false, true),
      )?;

        let slice_s = scope.alloc_string("slice")?;
        scope.push_root(Value::String(slice_s))?;
        let slice_key = PropertyKey::from_string(slice_s);
        let slice_fn = scope.alloc_native_function(array_prototype_slice, None, slice_s, 2)?;
        scope.push_root(Value::Object(slice_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(slice_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        slice_key,
        data_desc(Value::Object(slice_fn), true, false, true),
      )?;

        let push_s = scope.alloc_string("push")?;
        scope.push_root(Value::String(push_s))?;
        let push_key = PropertyKey::from_string(push_s);
        let push_fn = scope.alloc_native_function(array_prototype_push, None, push_s, 1)?;
        scope.push_root(Value::Object(push_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(push_fn, Some(function_prototype))?;
       scope.define_property(
         array_prototype,
         push_key,
         data_desc(Value::Object(push_fn), true, false, true),
       )?;

        let pop_s = scope.alloc_string("pop")?;
        scope.push_root(Value::String(pop_s))?;
        let pop_key = PropertyKey::from_string(pop_s);
        let pop_fn = scope.alloc_native_function(array_prototype_pop, None, pop_s, 0)?;
        scope.push_root(Value::Object(pop_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(pop_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        pop_key,
        data_desc(Value::Object(pop_fn), true, false, true),
      )?;

        let shift_s = scope.alloc_string("shift")?;
        scope.push_root(Value::String(shift_s))?;
        let shift_key = PropertyKey::from_string(shift_s);
        let shift_fn = scope.alloc_native_function(array_prototype_shift, None, shift_s, 0)?;
        scope.push_root(Value::Object(shift_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(shift_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        shift_key,
        data_desc(Value::Object(shift_fn), true, false, true),
      )?;

        let unshift_s = scope.alloc_string("unshift")?;
        scope.push_root(Value::String(unshift_s))?;
        let unshift_key = PropertyKey::from_string(unshift_s);
        let unshift_fn = scope.alloc_native_function(array_prototype_unshift, None, unshift_s, 1)?;
        scope.push_root(Value::Object(unshift_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(unshift_fn, Some(function_prototype))?;
      scope.define_property(
        array_prototype,
        unshift_key,
        data_desc(Value::Object(unshift_fn), true, false, true),
      )?;

        let splice_s = scope.alloc_string("splice")?;
        scope.push_root(Value::String(splice_s))?;
        let splice_key = PropertyKey::from_string(splice_s);
        let splice_fn = scope.alloc_native_function(array_prototype_splice, None, splice_s, 2)?;
        scope.push_root(Value::Object(splice_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(splice_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          splice_key,
          data_desc(Value::Object(splice_fn), true, false, true),
        )?;
        let values_s = scope.alloc_string("values")?;
        scope.push_root(Value::String(values_s))?;
        let values_key = PropertyKey::from_string(values_s);
        let values_fn = scope.alloc_native_function(array_prototype_values, None, values_s, 0)?;
        scope.push_root(Value::Object(values_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(values_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          values_key,
          data_desc(Value::Object(values_fn), true, false, true),
        )?;

        let keys_s = scope.alloc_string("keys")?;
        scope.push_root(Value::String(keys_s))?;
        let keys_key = PropertyKey::from_string(keys_s);
        let keys_fn = scope.alloc_native_function(array_prototype_keys, None, keys_s, 0)?;
        scope.push_root(Value::Object(keys_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(keys_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          keys_key,
          data_desc(Value::Object(keys_fn), true, false, true),
        )?;

        let entries_s = scope.alloc_string("entries")?;
        scope.push_root(Value::String(entries_s))?;
        let entries_key = PropertyKey::from_string(entries_s);
        let entries_fn = scope.alloc_native_function(array_prototype_entries, None, entries_s, 0)?;
        scope.push_root(Value::Object(entries_fn))?;
        scope
          .heap_mut()
          .object_set_prototype(entries_fn, Some(function_prototype))?;
        scope.define_property(
          array_prototype,
          entries_key,
          data_desc(Value::Object(entries_fn), true, false, true),
        )?;

        scope.define_property(
          array_prototype,
          PropertyKey::Symbol(well_known_symbols.iterator),
          data_desc(Value::Object(values_fn), true, false, true),
        )?;
    }

    // `%String%`
    let string_call = vm.register_native_call(builtins::string_constructor_call)?;
    let string_construct = vm.register_native_construct(builtins::string_constructor_construct)?;
    let string_name = scope.alloc_string("String")?;
    let string_constructor = alloc_rooted_native_function(
      scope,
      roots,
      string_call,
      Some(string_construct),
      string_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(string_constructor, Some(function_prototype))?;
    scope.define_property(
      string_constructor,
      common.prototype,
      data_desc(Value::Object(string_prototype), false, false, false),
    )?;
    scope.define_property(
      string_constructor,
      common.name,
      data_desc(Value::String(string_name), false, false, true),
    )?;
    scope.define_property(
      string_constructor,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      string_prototype,
      common.constructor,
      data_desc(Value::Object(string_constructor), true, false, true),
    )?;

      // String.fromCharCode
      {
        let from_char_code_s = scope.alloc_string("fromCharCode")?;
        scope.push_root(Value::String(from_char_code_s))?;
        let key = PropertyKey::from_string(from_char_code_s);
        let func = scope.alloc_native_function(string_from_char_code, None, from_char_code_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_constructor,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.fromCodePoint
      {
        let from_code_point_s = scope.alloc_string("fromCodePoint")?;
        scope.push_root(Value::String(from_code_point_s))?;
        let key = PropertyKey::from_string(from_code_point_s);
        let func =
          scope.alloc_native_function(string_from_code_point, None, from_code_point_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_constructor,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.raw
      {
        let raw_s = scope.alloc_string("raw")?;
        scope.push_root(Value::String(raw_s))?;
        let key = PropertyKey::from_string(raw_s);
        let func = scope.alloc_native_function(string_raw, None, raw_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_constructor,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.toString
      {
        let to_string_s = scope.alloc_string("toString")?;
        scope.push_root(Value::String(to_string_s))?;
        let key = PropertyKey::from_string(to_string_s);
        let func =
          scope.alloc_native_function(string_prototype_to_string, None, to_string_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        string_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

      // String.prototype.charCodeAt
      {
        let char_code_at_s = scope.alloc_string("charCodeAt")?;
        scope.push_root(Value::String(char_code_at_s))?;
        let key = PropertyKey::from_string(char_code_at_s);
        let func =
          scope.alloc_native_function(string_prototype_char_code_at, None, char_code_at_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.codePointAt
      {
        let code_point_at_s = scope.alloc_string("codePointAt")?;
        scope.push_root(Value::String(code_point_at_s))?;
        let key = PropertyKey::from_string(code_point_at_s);
        let func = scope.alloc_native_function(
          string_prototype_code_point_at,
          None,
          code_point_at_s,
          1,
        )?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.charAt
      {
        let char_at_s = scope.alloc_string("charAt")?;
        scope.push_root(Value::String(char_at_s))?;
        let key = PropertyKey::from_string(char_at_s);
        let func = scope.alloc_native_function(string_prototype_char_at, None, char_at_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.at
      {
        let at_s = scope.alloc_string("at")?;
        scope.push_root(Value::String(at_s))?;
        let key = PropertyKey::from_string(at_s);
        let func = scope.alloc_native_function(string_prototype_at, None, at_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.trim
      {
        let trim_s = scope.alloc_string("trim")?;
        scope.push_root(Value::String(trim_s))?;
        let key = PropertyKey::from_string(trim_s);
        let func = scope.alloc_native_function(string_prototype_trim, None, trim_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.trimStart
      {
        let trim_s = scope.alloc_string("trimStart")?;
        scope.push_root(Value::String(trim_s))?;
        let key = PropertyKey::from_string(trim_s);
        let func = scope.alloc_native_function(string_prototype_trim_start, None, trim_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.trimEnd
      {
        let trim_s = scope.alloc_string("trimEnd")?;
        scope.push_root(Value::String(trim_s))?;
        let key = PropertyKey::from_string(trim_s);
        let func = scope.alloc_native_function(string_prototype_trim_end, None, trim_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.trimLeft (Annex B)
      {
        let trim_s = scope.alloc_string("trimLeft")?;
        scope.push_root(Value::String(trim_s))?;
        let key = PropertyKey::from_string(trim_s);
        let func = scope.alloc_native_function(string_prototype_trim_start, None, trim_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.trimRight (Annex B)
      {
        let trim_s = scope.alloc_string("trimRight")?;
        scope.push_root(Value::String(trim_s))?;
        let key = PropertyKey::from_string(trim_s);
        let func = scope.alloc_native_function(string_prototype_trim_end, None, trim_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.substring
      {
        let substring_s = scope.alloc_string("substring")?;
        scope.push_root(Value::String(substring_s))?;
        let key = PropertyKey::from_string(substring_s);
        let func = scope.alloc_native_function(string_prototype_substring, None, substring_s, 2)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.substr (Annex B)
      {
        let substr_s = scope.alloc_string("substr")?;
        scope.push_root(Value::String(substr_s))?;
        let key = PropertyKey::from_string(substr_s);
        let func = scope.alloc_native_function(string_prototype_substr, None, substr_s, 2)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.match
      {
        let match_s = scope.alloc_string("match")?;
        scope.push_root(Value::String(match_s))?;
        let key = PropertyKey::from_string(match_s);
        let func = scope.alloc_native_function(string_prototype_match, None, match_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.matchAll
      {
        let match_all_s = scope.alloc_string("matchAll")?;
        scope.push_root(Value::String(match_all_s))?;
        let key = PropertyKey::from_string(match_all_s);
        let func = scope.alloc_native_function(string_prototype_match_all, None, match_all_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.search
      {
        let search_s = scope.alloc_string("search")?;
        scope.push_root(Value::String(search_s))?;
        let key = PropertyKey::from_string(search_s);
        let func = scope.alloc_native_function(string_prototype_search, None, search_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.replace
      {
        let replace_s = scope.alloc_string("replace")?;
        scope.push_root(Value::String(replace_s))?;
        let key = PropertyKey::from_string(replace_s);
        let func = scope.alloc_native_function(string_prototype_replace, None, replace_s, 2)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.split
      {
        let split_s = scope.alloc_string("split")?;
        scope.push_root(Value::String(split_s))?;
        let key = PropertyKey::from_string(split_s);
        let func = scope.alloc_native_function(string_prototype_split, None, split_s, 2)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.repeat
      {
        let repeat_s = scope.alloc_string("repeat")?;
        scope.push_root(Value::String(repeat_s))?;
        let key = PropertyKey::from_string(repeat_s);
        let func = scope.alloc_native_function(string_prototype_repeat, None, repeat_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.padStart
      {
        let pad_start_s = scope.alloc_string("padStart")?;
        scope.push_root(Value::String(pad_start_s))?;
        let key = PropertyKey::from_string(pad_start_s);
        let func = scope.alloc_native_function(string_prototype_pad_start, None, pad_start_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.padEnd
      {
        let pad_end_s = scope.alloc_string("padEnd")?;
        scope.push_root(Value::String(pad_end_s))?;
        let key = PropertyKey::from_string(pad_end_s);
        let func = scope.alloc_native_function(string_prototype_pad_end, None, pad_end_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.replaceAll
      {
        let replace_all_s = scope.alloc_string("replaceAll")?;
        scope.push_root(Value::String(replace_all_s))?;
        let key = PropertyKey::from_string(replace_all_s);
        let func = scope.alloc_native_function(string_prototype_replace_all, None, replace_all_s, 2)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.toLowerCase
      {
        let to_lower_s = scope.alloc_string("toLowerCase")?;
        scope.push_root(Value::String(to_lower_s))?;
        let key = PropertyKey::from_string(to_lower_s);
        let func = scope.alloc_native_function(string_prototype_to_lower_case, None, to_lower_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.toUpperCase
      {
        let to_upper_s = scope.alloc_string("toUpperCase")?;
        scope.push_root(Value::String(to_upper_s))?;
        let key = PropertyKey::from_string(to_upper_s);
        let func = scope.alloc_native_function(string_prototype_to_upper_case, None, to_upper_s, 0)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.slice
      {
        let slice_s = scope.alloc_string("slice")?;
        scope.push_root(Value::String(slice_s))?;
        let key = PropertyKey::from_string(slice_s);
        let func = scope.alloc_native_function(string_prototype_slice, None, slice_s, 2)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        string_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

      // String.prototype.indexOf
      {
        let index_of_s = scope.alloc_string("indexOf")?;
        scope.push_root(Value::String(index_of_s))?;
        let key = PropertyKey::from_string(index_of_s);
        let func =
          scope.alloc_native_function(string_prototype_index_of, None, index_of_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        string_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

      // String.prototype.includes
      {
        let includes_s = scope.alloc_string("includes")?;
        scope.push_root(Value::String(includes_s))?;
        let key = PropertyKey::from_string(includes_s);
        let func =
          scope.alloc_native_function(string_prototype_includes, None, includes_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.startsWith
      {
        let starts_with_s = scope.alloc_string("startsWith")?;
        scope.push_root(Value::String(starts_with_s))?;
        let key = PropertyKey::from_string(starts_with_s);
        let func =
          scope.alloc_native_function(string_prototype_starts_with, None, starts_with_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

      // String.prototype.endsWith
      {
        let ends_with_s = scope.alloc_string("endsWith")?;
        scope.push_root(Value::String(ends_with_s))?;
        let key = PropertyKey::from_string(ends_with_s);
        let func =
          scope.alloc_native_function(string_prototype_ends_with, None, ends_with_s, 1)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(
          string_prototype,
          key,
          data_desc(Value::Object(func), true, false, true),
        )?;
      }

    // String.prototype[@@iterator]
    {
      // Internal symbols used to model `[[IteratedString]]` / `[[NextIndex]]` slots on string
      // iterator objects.
      let iterated_key_s = scope.alloc_string("vm-js.internal.StringIteratorIteratedString")?;
      scope.push_root(Value::String(iterated_key_s))?;
      let iterated_sym = scope.heap_mut().symbol_for(iterated_key_s)?;
      scope.push_root(Value::Symbol(iterated_sym))?;

      let next_index_key_s = scope.alloc_string("vm-js.internal.StringIteratorNextIndex")?;
      scope.push_root(Value::String(next_index_key_s))?;
      let next_index_sym = scope.heap_mut().symbol_for(next_index_key_s)?;
      scope.push_root(Value::Symbol(next_index_sym))?;

      // Shared `%StringIteratorPrototype%.next` builtin, parameterized by the internal symbol keys.
      let next_name = scope.alloc_string("next")?;
      scope.push_root(Value::String(next_name))?;
      let next_slots = [Value::Symbol(iterated_sym), Value::Symbol(next_index_sym)];
      let next_fn = scope.alloc_native_function_with_slots(string_iterator_next, None, next_name, 0, &next_slots)?;
      scope.push_root(Value::Object(next_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(next_fn, Some(function_prototype))?;
      scope.define_property(
        string_iterator_prototype,
        PropertyKey::from_string(next_name),
        data_desc(Value::Object(next_fn), true, false, true),
      )?;
      let string_iterator_tag = scope.alloc_string("String Iterator")?;
      scope.define_property(
        string_iterator_prototype,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(string_iterator_tag), false, false, true),
      )?;

      let iter_name = scope.alloc_string("[Symbol.iterator]")?;
      scope.push_root(Value::String(iter_name))?;
      let iter_slots = [Value::Symbol(iterated_sym), Value::Symbol(next_index_sym)];
      let iter_fn =
        scope.alloc_native_function_with_slots(string_prototype_iterator, None, iter_name, 0, &iter_slots)?;
      scope.push_root(Value::Object(iter_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(iter_fn, Some(function_prototype))?;
      scope.define_property(
        string_prototype,
        PropertyKey::Symbol(well_known_symbols.iterator),
        data_desc(Value::Object(iter_fn), true, false, true),
      )?;
    }

    // `%RegExp%`
    let regexp_call = vm.register_native_call(builtins::regexp_constructor_call)?;
    let regexp_construct = vm.register_native_construct(builtins::regexp_constructor_construct)?;
    let regexp_name = scope.alloc_string("RegExp")?;
    let regexp_constructor = alloc_rooted_native_function(
      scope,
      roots,
      regexp_call,
      Some(regexp_construct),
      regexp_name,
      2,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(regexp_constructor, Some(function_prototype))?;
    scope.define_property(
      regexp_constructor,
      common.prototype,
      data_desc(Value::Object(regexp_prototype), false, false, false),
    )?;
    scope.define_property(
      regexp_constructor,
      common.name,
      data_desc(Value::String(regexp_name), false, false, true),
    )?;
    scope.define_property(
      regexp_constructor,
      common.length,
      data_desc(Value::Number(2.0), false, false, true),
    )?;
    scope.define_property(
      regexp_prototype,
      common.constructor,
      data_desc(Value::Object(regexp_constructor), true, false, true),
    )?;

    // RegExp.prototype.exec
    {
      let exec_s = scope.alloc_string("exec")?;
      scope.push_root(Value::String(exec_s))?;
      let key = PropertyKey::from_string(exec_s);
      let func = scope.alloc_native_function(regexp_prototype_exec, None, exec_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // RegExp.prototype.test
    {
      let test_s = scope.alloc_string("test")?;
      scope.push_root(Value::String(test_s))?;
      let key = PropertyKey::from_string(test_s);
      let func = scope.alloc_native_function(regexp_prototype_test, None, test_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // RegExp.prototype.source
    {
      let key_s = scope.alloc_string("source")?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let get_name = scope.alloc_string("get source")?;
      let get = scope.alloc_native_function(regexp_prototype_source_get, None, get_name, 0)?;
      scope.push_root(Value::Object(get))?;
      scope
        .heap_mut()
        .object_set_prototype(get, Some(function_prototype))?;

      scope.define_property(
        regexp_prototype,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(get),
            set: Value::Undefined,
          },
        },
      )?;
    }

    // RegExp.prototype.flags
    {
      let key_s = scope.alloc_string("flags")?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let get_name = scope.alloc_string("get flags")?;
      let get = scope.alloc_native_function(regexp_prototype_flags_get, None, get_name, 0)?;
      scope.push_root(Value::Object(get))?;
      scope
        .heap_mut()
        .object_set_prototype(get, Some(function_prototype))?;

      scope.define_property(
        regexp_prototype,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(get),
            set: Value::Undefined,
          },
        },
      )?;
    }

    // RegExp.prototype[@@match]
    {
      let name_s = scope.alloc_string("[Symbol.match]")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(regexp_prototype_symbol_match, None, name_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        PropertyKey::Symbol(well_known_symbols.match_),
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // RegExp.prototype[@@search]
    {
      let name_s = scope.alloc_string("[Symbol.search]")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(regexp_prototype_symbol_search, None, name_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        PropertyKey::Symbol(well_known_symbols.search),
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // RegExp.prototype[@@replace]
    {
      let name_s = scope.alloc_string("[Symbol.replace]")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(regexp_prototype_symbol_replace, None, name_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        PropertyKey::Symbol(well_known_symbols.replace),
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // RegExp.prototype[@@split]
    {
      let name_s = scope.alloc_string("[Symbol.split]")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(regexp_prototype_symbol_split, None, name_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        PropertyKey::Symbol(well_known_symbols.split),
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // RegExp.prototype[@@matchAll]
    {
      // Internal slot keys for RegExpStringIterator objects.
      let iterating_key_s = scope.alloc_string("vm-js.internal.RegExpStringIteratorIteratingRegExp")?;
      scope.push_root(Value::String(iterating_key_s))?;
      let iterating_sym = scope.heap_mut().symbol_for(iterating_key_s)?;
      scope.push_root(Value::Symbol(iterating_sym))?;

      let iterated_key_s = scope.alloc_string("vm-js.internal.RegExpStringIteratorIteratedString")?;
      scope.push_root(Value::String(iterated_key_s))?;
      let iterated_sym = scope.heap_mut().symbol_for(iterated_key_s)?;
      scope.push_root(Value::Symbol(iterated_sym))?;

      let global_key_s = scope.alloc_string("vm-js.internal.RegExpStringIteratorGlobal")?;
      scope.push_root(Value::String(global_key_s))?;
      let global_sym = scope.heap_mut().symbol_for(global_key_s)?;
      scope.push_root(Value::Symbol(global_sym))?;

      let unicode_key_s = scope.alloc_string("vm-js.internal.RegExpStringIteratorUnicode")?;
      scope.push_root(Value::String(unicode_key_s))?;
      let unicode_sym = scope.heap_mut().symbol_for(unicode_key_s)?;
      scope.push_root(Value::Symbol(unicode_sym))?;

      let done_key_s = scope.alloc_string("vm-js.internal.RegExpStringIteratorDone")?;
      scope.push_root(Value::String(done_key_s))?;
      let done_sym = scope.heap_mut().symbol_for(done_key_s)?;
      scope.push_root(Value::Symbol(done_sym))?;

      // Shared iterator `next` method (captures internal symbols in native slots).
      let next_name = scope.alloc_string("next")?;
      scope.push_root(Value::String(next_name))?;
      let next_slots = [
        Value::Symbol(iterating_sym),
        Value::Symbol(iterated_sym),
        Value::Symbol(global_sym),
        Value::Symbol(unicode_sym),
        Value::Symbol(done_sym),
      ];
      let next_fn = scope.alloc_native_function_with_slots(
        regexp_string_iterator_next,
        None,
        next_name,
        0,
        &next_slots,
      )?;
      scope.push_root(Value::Object(next_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(next_fn, Some(function_prototype))?;

      // Shared `@@iterator` method that returns `this`.
      let iter_name = scope.alloc_string("[Symbol.iterator]")?;
      scope.push_root(Value::String(iter_name))?;
      let iterator_fn =
        scope.alloc_native_function(iterator_prototype_symbol_iterator, None, iter_name, 0)?;
      scope.push_root(Value::Object(iterator_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(iterator_fn, Some(function_prototype))?;

      let match_all_name = scope.alloc_string("[Symbol.matchAll]")?;
      scope.push_root(Value::String(match_all_name))?;
      let match_all_slots = [
        Value::Object(next_fn),
        Value::Symbol(iterating_sym),
        Value::Symbol(iterated_sym),
        Value::Symbol(global_sym),
        Value::Symbol(unicode_sym),
        Value::Symbol(done_sym),
        Value::Object(iterator_fn),
      ];
      let match_all_fn = scope.alloc_native_function_with_slots(
        regexp_prototype_symbol_match_all,
        None,
        match_all_name,
        1,
        &match_all_slots,
      )?;
      scope.push_root(Value::Object(match_all_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(match_all_fn, Some(function_prototype))?;
      scope.define_property(
        regexp_prototype,
        PropertyKey::Symbol(well_known_symbols.match_all),
        data_desc(Value::Object(match_all_fn), true, false, true),
      )?;
    }
    // `%Number%`
    let number_name = scope.alloc_string("Number")?;
    let number_constructor = alloc_rooted_native_function(
      scope,
      roots,
      number_call,
      Some(number_construct),
      number_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(number_constructor, Some(function_prototype))?;
    scope.define_property(
      number_constructor,
      common.prototype,
      data_desc(Value::Object(number_prototype), false, false, false),
    )?;
    scope.define_property(
      number_constructor,
      common.name,
      data_desc(Value::String(number_name), false, false, true),
    )?;
    scope.define_property(
      number_constructor,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      number_prototype,
      common.constructor,
      data_desc(Value::Object(number_constructor), true, false, true),
    )?;

    // Number.prototype.valueOf
    {
      let value_of_s = scope.alloc_string("valueOf")?;
      scope.push_root(Value::String(value_of_s))?;
      let key = PropertyKey::from_string(value_of_s);
      let func = scope.alloc_native_function(number_prototype_value_of, None, value_of_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        number_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Number.prototype.toString
    {
      let to_string_s = scope.alloc_string("toString")?;
      scope.push_root(Value::String(to_string_s))?;
      let key = PropertyKey::from_string(to_string_s);
      let func = scope.alloc_native_function(number_prototype_to_string, None, to_string_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        number_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Number.prototype.toFixed
    {
      let to_fixed_s = scope.alloc_string("toFixed")?;
      scope.push_root(Value::String(to_fixed_s))?;
      let key = PropertyKey::from_string(to_fixed_s);
      let func = scope.alloc_native_function(number_prototype_to_fixed, None, to_fixed_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        number_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Number.prototype.toExponential
    {
      let to_exp_s = scope.alloc_string("toExponential")?;
      scope.push_root(Value::String(to_exp_s))?;
      let key = PropertyKey::from_string(to_exp_s);
      let func = scope.alloc_native_function(number_prototype_to_exponential, None, to_exp_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        number_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Number.prototype.toPrecision
    {
      let to_prec_s = scope.alloc_string("toPrecision")?;
      scope.push_root(Value::String(to_prec_s))?;
      let key = PropertyKey::from_string(to_prec_s);
      let func = scope.alloc_native_function(number_prototype_to_precision, None, to_prec_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        number_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Number.prototype.toLocaleString
    {
      let to_locale_s = scope.alloc_string("toLocaleString")?;
      scope.push_root(Value::String(to_locale_s))?;
      let key = PropertyKey::from_string(to_locale_s);
      let func =
        scope.alloc_native_function(number_prototype_to_locale_string, None, to_locale_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        number_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Number static properties.
    {
      let cases: [(&str, Value); 8] = [
        ("NaN", Value::Number(f64::NAN)),
        ("POSITIVE_INFINITY", Value::Number(f64::INFINITY)),
        ("NEGATIVE_INFINITY", Value::Number(f64::NEG_INFINITY)),
        ("MAX_VALUE", Value::Number(f64::MAX)),
        // JS `Number.MIN_VALUE` is the smallest positive **subnormal** (`5e-324`), not
        // `f64::MIN_POSITIVE` (smallest positive normal).
        ("MIN_VALUE", Value::Number(f64::from_bits(1))),
        ("EPSILON", Value::Number(f64::EPSILON)),
        ("MAX_SAFE_INTEGER", Value::Number(9007199254740991.0)),
        ("MIN_SAFE_INTEGER", Value::Number(-9007199254740991.0)),
      ];
      for (name, value) in cases {
        let key_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        scope.define_property(number_constructor, key, data_desc(value, false, false, false))?;
      }
    }

    // Number static methods.
    {
      let cases = [
        ("isNaN", number_is_nan, 1u32),
        ("isFinite", number_is_finite, 1u32),
        ("isInteger", number_is_integer, 1u32),
        ("isSafeInteger", number_is_safe_integer, 1u32),
      ];
      for (name, call, length) in cases {
        let name_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(name_s))?;
        let key = PropertyKey::from_string(name_s);
        let func = scope.alloc_native_function(call, None, name_s, length)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        scope.define_property(number_constructor, key, data_desc(Value::Object(func), true, false, true))?;
      }
    }

    // `%Boolean%`
    let boolean_name = scope.alloc_string("Boolean")?;
    let boolean_constructor = alloc_rooted_native_function(
      scope,
      roots,
      boolean_call,
      Some(boolean_construct),
      boolean_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(boolean_constructor, Some(function_prototype))?;
    scope.define_property(
      boolean_constructor,
      common.prototype,
      data_desc(Value::Object(boolean_prototype), false, false, false),
    )?;
    scope.define_property(
      boolean_constructor,
      common.name,
      data_desc(Value::String(boolean_name), false, false, true),
    )?;
    scope.define_property(
      boolean_constructor,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      boolean_prototype,
      common.constructor,
      data_desc(Value::Object(boolean_constructor), true, false, true),
    )?;

    // Boolean.prototype.valueOf
    {
      let value_of_s = scope.alloc_string("valueOf")?;
      scope.push_root(Value::String(value_of_s))?;
      let key = PropertyKey::from_string(value_of_s);
      let func = scope.alloc_native_function(boolean_prototype_value_of, None, value_of_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        boolean_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Boolean.prototype.toString
    {
      let to_string_s = scope.alloc_string("toString")?;
      scope.push_root(Value::String(to_string_s))?;
      let key = PropertyKey::from_string(to_string_s);
      let func = scope.alloc_native_function(boolean_prototype_to_string, None, to_string_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        boolean_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // BigInt.prototype.valueOf
    {
      let value_of_s = scope.alloc_string("valueOf")?;
      scope.push_root(Value::String(value_of_s))?;
      let key = PropertyKey::from_string(value_of_s);
      let func = scope.alloc_native_function(bigint_prototype_value_of, None, value_of_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        bigint_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // `%Date%`
    let date_name = scope.alloc_string("Date")?;
    let date_constructor = alloc_rooted_native_function(
      scope,
      roots,
      date_call,
      Some(date_construct),
      date_name,
      7,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(date_constructor, Some(function_prototype))?;
    scope.define_property(
      date_constructor,
      common.prototype,
      data_desc(Value::Object(date_prototype), false, false, false),
    )?;
    scope.define_property(
      date_constructor,
      common.name,
      data_desc(Value::String(date_name), false, false, true),
    )?;
    scope.define_property(
      date_constructor,
      common.length,
      data_desc(Value::Number(7.0), false, false, true),
    )?;
    scope.define_property(
      date_prototype,
      common.constructor,
      data_desc(Value::Object(date_constructor), true, false, true),
    )?;

    // Date.now / parse / UTC
    {
      let now_s = scope.alloc_string("now")?;
      scope.push_root(Value::String(now_s))?;
      let now_key = PropertyKey::from_string(now_s);
      let now_fn = scope.alloc_native_function(date_now, None, now_s, 0)?;
      scope.push_root(Value::Object(now_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(now_fn, Some(function_prototype))?;
      scope.define_property(
        date_constructor,
        now_key,
        data_desc(Value::Object(now_fn), true, false, true),
      )?;

      let parse_s = scope.alloc_string("parse")?;
      scope.push_root(Value::String(parse_s))?;
      let parse_key = PropertyKey::from_string(parse_s);
      let parse_fn = scope.alloc_native_function(date_parse, None, parse_s, 1)?;
      scope.push_root(Value::Object(parse_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(parse_fn, Some(function_prototype))?;
      scope.define_property(
        date_constructor,
        parse_key,
        data_desc(Value::Object(parse_fn), true, false, true),
      )?;

      let utc_s = scope.alloc_string("UTC")?;
      scope.push_root(Value::String(utc_s))?;
      let utc_key = PropertyKey::from_string(utc_s);
      let utc_fn = scope.alloc_native_function(date_utc, None, utc_s, 7)?;
      scope.push_root(Value::Object(utc_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(utc_fn, Some(function_prototype))?;
      scope.define_property(
        date_constructor,
        utc_key,
        data_desc(Value::Object(utc_fn), true, false, true),
      )?;
    }

    // Date.prototype.toString / toUTCString / toISOString / getTime / valueOf / @@toPrimitive
    {
      let to_string_s = scope.alloc_string("toString")?;
      scope.push_root(Value::String(to_string_s))?;
      let to_string_key = PropertyKey::from_string(to_string_s);
      let to_string_fn = scope.alloc_native_function(date_prototype_to_string, None, to_string_s, 0)?;
      scope.push_root(Value::Object(to_string_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(to_string_fn, Some(function_prototype))?;
      scope.define_property(
        date_prototype,
        to_string_key,
        data_desc(Value::Object(to_string_fn), true, false, true),
      )?;

      let to_utc_s = scope.alloc_string("toUTCString")?;
      scope.push_root(Value::String(to_utc_s))?;
      let to_utc_key = PropertyKey::from_string(to_utc_s);
      let to_utc_fn =
        scope.alloc_native_function(date_prototype_to_utc_string, None, to_utc_s, 0)?;
      scope.push_root(Value::Object(to_utc_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(to_utc_fn, Some(function_prototype))?;
      scope.define_property(
        date_prototype,
        to_utc_key,
        data_desc(Value::Object(to_utc_fn), true, false, true),
      )?;

      let to_iso_s = scope.alloc_string("toISOString")?;
      scope.push_root(Value::String(to_iso_s))?;
      let to_iso_key = PropertyKey::from_string(to_iso_s);
      let to_iso_fn =
        scope.alloc_native_function(date_prototype_to_iso_string, None, to_iso_s, 0)?;
      scope.push_root(Value::Object(to_iso_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(to_iso_fn, Some(function_prototype))?;
      scope.define_property(
        date_prototype,
        to_iso_key,
        data_desc(Value::Object(to_iso_fn), true, false, true),
      )?;

      let get_time_s = scope.alloc_string("getTime")?;
      scope.push_root(Value::String(get_time_s))?;
      let get_time_key = PropertyKey::from_string(get_time_s);
      let get_time_fn = scope.alloc_native_function(date_prototype_get_time, None, get_time_s, 0)?;
      scope.push_root(Value::Object(get_time_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(get_time_fn, Some(function_prototype))?;
      scope.define_property(
        date_prototype,
        get_time_key,
        data_desc(Value::Object(get_time_fn), true, false, true),
      )?;

      let value_of_s = scope.alloc_string("valueOf")?;
      scope.push_root(Value::String(value_of_s))?;
      let value_of_key = PropertyKey::from_string(value_of_s);
      let value_of_fn = scope.alloc_native_function(date_prototype_value_of, None, value_of_s, 0)?;
      scope.push_root(Value::Object(value_of_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(value_of_fn, Some(function_prototype))?;
      scope.define_property(
        date_prototype,
        value_of_key,
        data_desc(Value::Object(value_of_fn), true, false, true),
      )?;

      let to_prim_s = scope.alloc_string("[Symbol.toPrimitive]")?;
      scope.push_root(Value::String(to_prim_s))?;
      let to_prim_fn =
        scope.alloc_native_function(date_prototype_to_primitive, None, to_prim_s, 1)?;
      scope.push_root(Value::Object(to_prim_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(to_prim_fn, Some(function_prototype))?;
      scope.define_property(
        date_prototype,
        PropertyKey::Symbol(well_known_symbols.to_primitive),
        // Per ECMA-262, `Date.prototype[@@toPrimitive]` is non-writable.
        data_desc(Value::Object(to_prim_fn), false, false, true),
      )?;
    }

    // `%eval%` (global function)
    let eval_name = scope.alloc_string("eval")?;
    let eval = alloc_rooted_native_function(scope, roots, eval_call, None, eval_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(eval, Some(function_prototype))?;

    // `%isNaN%` (global function)
    let is_nan_name = scope.alloc_string("isNaN")?;
    let is_nan = alloc_rooted_native_function(scope, roots, is_nan_call, None, is_nan_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(is_nan, Some(function_prototype))?;

    // `%isFinite%` (global function)
    let is_finite_name = scope.alloc_string("isFinite")?;
    let is_finite =
      alloc_rooted_native_function(scope, roots, is_finite_call, None, is_finite_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(is_finite, Some(function_prototype))?;

    // `%parseInt%` (global function)
    let parse_int_name = scope.alloc_string("parseInt")?;
    let parse_int =
      alloc_rooted_native_function(scope, roots, parse_int_call, None, parse_int_name, 2)?;
    scope
      .heap_mut()
      .object_set_prototype(parse_int, Some(function_prototype))?;

    // `%parseFloat%` (global function)
    let parse_float_name = scope.alloc_string("parseFloat")?;
    let parse_float = alloc_rooted_native_function(
      scope,
      roots,
      parse_float_call,
      None,
      parse_float_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(parse_float, Some(function_prototype))?;

    // Number.parseInt / Number.parseFloat (aliases of global functions per ECMA-262).
    {
      let parse_int_key_s = scope.alloc_string("parseInt")?;
      scope.push_root(Value::String(parse_int_key_s))?;
      let parse_int_key = PropertyKey::from_string(parse_int_key_s);
      scope.define_property(
        number_constructor,
        parse_int_key,
        data_desc(Value::Object(parse_int), true, false, true),
      )?;

      let parse_float_key_s = scope.alloc_string("parseFloat")?;
      scope.push_root(Value::String(parse_float_key_s))?;
      let parse_float_key = PropertyKey::from_string(parse_float_key_s);
      scope.define_property(
        number_constructor,
        parse_float_key,
        data_desc(Value::Object(parse_float), true, false, true),
      )?;
    }

    // `%encodeURI%` (global function)
    let encode_uri_name = scope.alloc_string("encodeURI")?;
    let encode_uri =
      alloc_rooted_native_function(scope, roots, encode_uri_call, None, encode_uri_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(encode_uri, Some(function_prototype))?;

    // `%encodeURIComponent%` (global function)
    let encode_uri_component_name = scope.alloc_string("encodeURIComponent")?;
    let encode_uri_component = alloc_rooted_native_function(
      scope,
      roots,
      encode_uri_component_call,
      None,
      encode_uri_component_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(encode_uri_component, Some(function_prototype))?;

    // `%decodeURI%` (global function)
    let decode_uri_name = scope.alloc_string("decodeURI")?;
    let decode_uri =
      alloc_rooted_native_function(scope, roots, decode_uri_call, None, decode_uri_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(decode_uri, Some(function_prototype))?;

    // `%decodeURIComponent%` (global function)
    let decode_uri_component_name = scope.alloc_string("decodeURIComponent")?;
    let decode_uri_component = alloc_rooted_native_function(
      scope,
      roots,
      decode_uri_component_call,
      None,
      decode_uri_component_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(decode_uri_component, Some(function_prototype))?;

    // `%Symbol%`
    let symbol_call = vm.register_native_call(builtins::symbol_constructor_call)?;
    let symbol_name = scope.alloc_string("Symbol")?;
    let symbol_constructor =
      alloc_rooted_native_function(scope, roots, symbol_call, None, symbol_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(symbol_constructor, Some(function_prototype))?;
    scope.define_property(
      symbol_constructor,
      common.prototype,
      data_desc(Value::Object(symbol_prototype), false, false, false),
    )?;
    scope.define_property(
      symbol_constructor,
      common.name,
      data_desc(Value::String(symbol_name), false, false, true),
    )?;
    scope.define_property(
      symbol_constructor,
      common.length,
      // Per ECMA-262, `Symbol([description])` has no required parameters.
      data_desc(Value::Number(0.0), false, false, true),
    )?;
    scope.define_property(
      symbol_prototype,
      common.constructor,
      data_desc(Value::Object(symbol_constructor), true, false, true),
    )?;

    // Symbol.prototype.valueOf
    {
      let value_of_s = scope.alloc_string("valueOf")?;
      scope.push_root(Value::String(value_of_s))?;
      let key = PropertyKey::from_string(value_of_s);
      let func = scope.alloc_native_function(symbol_prototype_value_of, None, value_of_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        symbol_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Symbol.prototype.toString
    {
      let to_string_s = scope.alloc_string("toString")?;
      scope.push_root(Value::String(to_string_s))?;
      let key = PropertyKey::from_string(to_string_s);
      let func = scope.alloc_native_function(symbol_prototype_to_string, None, to_string_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        symbol_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Symbol.prototype[Symbol.toPrimitive]
    {
      let to_prim_s = scope.alloc_string("[Symbol.toPrimitive]")?;
      scope.push_root(Value::String(to_prim_s))?;
      let to_prim_fn =
        scope.alloc_native_function(symbol_prototype_to_primitive, None, to_prim_s, 1)?;
      scope.push_root(Value::Object(to_prim_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(to_prim_fn, Some(function_prototype))?;
      scope.define_property(
        symbol_prototype,
        PropertyKey::Symbol(well_known_symbols.to_primitive),
        // Per ECMA-262, `Symbol.prototype[@@toPrimitive]` is non-writable.
        data_desc(Value::Object(to_prim_fn), false, false, true),
      )?;
    }

    // Symbol.prototype.description
    {
      let key_s = scope.alloc_string("description")?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let get_name = scope.alloc_string("get description")?;
      let get = scope.alloc_native_function(symbol_prototype_description_get, None, get_name, 0)?;
      scope.push_root(Value::Object(get))?;
      scope
        .heap_mut()
        .object_set_prototype(get, Some(function_prototype))?;

      scope.define_property(
        symbol_prototype,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(get),
            set: Value::Undefined,
          },
        },
      )?;
    }

    // Symbol.prototype[Symbol.toStringTag]
    {
      let to_string_tag_value = scope.alloc_string("Symbol")?;
      scope.define_property(
        symbol_prototype,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(to_string_tag_value), false, false, true),
      )?;
    }

    // Symbol.for / Symbol.keyFor
    {
      let for_s = scope.alloc_string("for")?;
      scope.push_root(Value::String(for_s))?;
      let key = PropertyKey::from_string(for_s);
      let func = scope.alloc_native_function(symbol_for, None, for_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        symbol_constructor,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;

      let key_for_s = scope.alloc_string("keyFor")?;
      scope.push_root(Value::String(key_for_s))?;
      let key = PropertyKey::from_string(key_for_s);
      let func = scope.alloc_native_function(symbol_key_for, None, key_for_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        symbol_constructor,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // Install well-known symbols as properties on the global `Symbol` constructor.
    {
      let wks = &well_known_symbols;
      let cases = [
        ("asyncIterator", wks.async_iterator),
        ("hasInstance", wks.has_instance),
        ("isConcatSpreadable", wks.is_concat_spreadable),
        ("iterator", wks.iterator),
        ("match", wks.match_),
        ("matchAll", wks.match_all),
        ("replace", wks.replace),
        ("search", wks.search),
        ("species", wks.species),
        ("split", wks.split),
        ("toPrimitive", wks.to_primitive),
        ("toStringTag", wks.to_string_tag),
        ("unscopables", wks.unscopables),
      ];
      for (name, sym) in cases {
        let key_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        scope.define_property(
          symbol_constructor,
          key,
          data_desc(Value::Symbol(sym), false, false, false),
        )?;
      }
    }

    // `%ArrayBuffer%`
    let array_buffer_call = vm.register_native_call(builtins::array_buffer_constructor_call)?;
    let array_buffer_construct =
      vm.register_native_construct(builtins::array_buffer_constructor_construct)?;
    let array_buffer_name = scope.alloc_string("ArrayBuffer")?;
    let array_buffer = alloc_rooted_native_function(
      scope,
      roots,
      array_buffer_call,
      Some(array_buffer_construct),
      array_buffer_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(array_buffer, Some(function_prototype))?;
    scope.define_property(
      array_buffer,
      common.prototype,
      data_desc(Value::Object(array_buffer_prototype), true, false, false),
    )?;
    scope.define_property(
      array_buffer,
      common.name,
      data_desc(Value::String(array_buffer_name), false, false, true),
    )?;
    scope.define_property(
      array_buffer,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      array_buffer_prototype,
      common.constructor,
      data_desc(Value::Object(array_buffer), true, false, true),
    )?;

    // ArrayBuffer.isView
    {
      let is_view_call = vm.register_native_call(builtins::array_buffer_is_view)?;
      let is_view_s = scope.alloc_string("isView")?;
      scope.push_root(Value::String(is_view_s))?;
      let key = PropertyKey::from_string(is_view_s);
      let func = scope.alloc_native_function(is_view_call, None, is_view_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        array_buffer,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // ArrayBuffer.prototype.byteLength
    {
      let key_s = scope.alloc_string("byteLength")?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let get_call = vm.register_native_call(builtins::array_buffer_prototype_byte_length_get)?;
      let get_name = scope.alloc_string("get byteLength")?;
      let get = scope.alloc_native_function(get_call, None, get_name, 0)?;
      scope.push_root(Value::Object(get))?;
      scope
        .heap_mut()
        .object_set_prototype(get, Some(function_prototype))?;

      scope.define_property(
        array_buffer_prototype,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(get),
            set: Value::Undefined,
          },
        },
      )?;
    }

    // ArrayBuffer.prototype.detached
    {
      let key_s = scope.alloc_string("detached")?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let get_call = vm.register_native_call(builtins::array_buffer_prototype_detached_get)?;
      let get_name = scope.alloc_string("get detached")?;
      let get = scope.alloc_native_function(get_call, None, get_name, 0)?;
      scope.push_root(Value::Object(get))?;
      scope
        .heap_mut()
        .object_set_prototype(get, Some(function_prototype))?;

      scope.define_property(
        array_buffer_prototype,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(get),
            set: Value::Undefined,
          },
        },
      )?;
    }

    // ArrayBuffer.prototype.slice
    {
      let slice_call = vm.register_native_call(builtins::array_buffer_prototype_slice)?;
      let slice_s = scope.alloc_string("slice")?;
      scope.push_root(Value::String(slice_s))?;
      let key = PropertyKey::from_string(slice_s);
      let func = scope.alloc_native_function(slice_call, None, slice_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        array_buffer_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // `%Uint8Array%`
    let uint8_array_call = vm.register_native_call(builtins::uint8_array_constructor_call)?;
    let uint8_array_construct =
      vm.register_native_construct(builtins::uint8_array_constructor_construct)?;
    let uint8_array_name = scope.alloc_string("Uint8Array")?;
    let uint8_array = alloc_rooted_native_function(
      scope,
      roots,
      uint8_array_call,
      Some(uint8_array_construct),
      uint8_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(uint8_array, Some(function_prototype))?;
    scope.define_property(
      uint8_array,
      common.prototype,
      data_desc(Value::Object(uint8_array_prototype), true, false, false),
    )?;
    scope.define_property(
      uint8_array,
      common.name,
      data_desc(Value::String(uint8_array_name), false, false, true),
    )?;
    scope.define_property(
      uint8_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      uint8_array_prototype,
      common.constructor,
      data_desc(Value::Object(uint8_array), true, false, true),
    )?;

    // --- TypedArray constructors ---
    //
    // These currently support only:
    // - `new X(length)`
    // - `new X(arrayBuffer, byteOffset?, length?)`
    //
    // TODO: Add iterable/typed-array sources.

    // `%Int8Array%`
    let int8_array_call = vm.register_native_call(builtins::int8_array_constructor_call)?;
    let int8_array_construct =
      vm.register_native_construct(builtins::int8_array_constructor_construct)?;
    let int8_array_name = scope.alloc_string("Int8Array")?;
    let int8_array = alloc_rooted_native_function(
      scope,
      roots,
      int8_array_call,
      Some(int8_array_construct),
      int8_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(int8_array, Some(function_prototype))?;
    scope.define_property(
      int8_array,
      common.prototype,
      data_desc(Value::Object(int8_array_prototype), true, false, false),
    )?;
    scope.define_property(
      int8_array,
      common.name,
      data_desc(Value::String(int8_array_name), false, false, true),
    )?;
    scope.define_property(
      int8_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      int8_array_prototype,
      common.constructor,
      data_desc(Value::Object(int8_array), true, false, true),
    )?;

    // `%Uint8ClampedArray%`
    let uint8_clamped_array_call =
      vm.register_native_call(builtins::uint8_clamped_array_constructor_call)?;
    let uint8_clamped_array_construct =
      vm.register_native_construct(builtins::uint8_clamped_array_constructor_construct)?;
    let uint8_clamped_array_name = scope.alloc_string("Uint8ClampedArray")?;
    let uint8_clamped_array = alloc_rooted_native_function(
      scope,
      roots,
      uint8_clamped_array_call,
      Some(uint8_clamped_array_construct),
      uint8_clamped_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(uint8_clamped_array, Some(function_prototype))?;
    scope.define_property(
      uint8_clamped_array,
      common.prototype,
      data_desc(
        Value::Object(uint8_clamped_array_prototype),
        true,
        false,
        false,
      ),
    )?;
    scope.define_property(
      uint8_clamped_array,
      common.name,
      data_desc(Value::String(uint8_clamped_array_name), false, false, true),
    )?;
    scope.define_property(
      uint8_clamped_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      uint8_clamped_array_prototype,
      common.constructor,
      data_desc(Value::Object(uint8_clamped_array), true, false, true),
    )?;

    // `%Int16Array%`
    let int16_array_call = vm.register_native_call(builtins::int16_array_constructor_call)?;
    let int16_array_construct =
      vm.register_native_construct(builtins::int16_array_constructor_construct)?;
    let int16_array_name = scope.alloc_string("Int16Array")?;
    let int16_array = alloc_rooted_native_function(
      scope,
      roots,
      int16_array_call,
      Some(int16_array_construct),
      int16_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(int16_array, Some(function_prototype))?;
    scope.define_property(
      int16_array,
      common.prototype,
      data_desc(Value::Object(int16_array_prototype), true, false, false),
    )?;
    scope.define_property(
      int16_array,
      common.name,
      data_desc(Value::String(int16_array_name), false, false, true),
    )?;
    scope.define_property(
      int16_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      int16_array_prototype,
      common.constructor,
      data_desc(Value::Object(int16_array), true, false, true),
    )?;

    // `%Uint16Array%`
    let uint16_array_call = vm.register_native_call(builtins::uint16_array_constructor_call)?;
    let uint16_array_construct =
      vm.register_native_construct(builtins::uint16_array_constructor_construct)?;
    let uint16_array_name = scope.alloc_string("Uint16Array")?;
    let uint16_array = alloc_rooted_native_function(
      scope,
      roots,
      uint16_array_call,
      Some(uint16_array_construct),
      uint16_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(uint16_array, Some(function_prototype))?;
    scope.define_property(
      uint16_array,
      common.prototype,
      data_desc(Value::Object(uint16_array_prototype), true, false, false),
    )?;
    scope.define_property(
      uint16_array,
      common.name,
      data_desc(Value::String(uint16_array_name), false, false, true),
    )?;
    scope.define_property(
      uint16_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      uint16_array_prototype,
      common.constructor,
      data_desc(Value::Object(uint16_array), true, false, true),
    )?;

    // `%Int32Array%`
    let int32_array_call = vm.register_native_call(builtins::int32_array_constructor_call)?;
    let int32_array_construct =
      vm.register_native_construct(builtins::int32_array_constructor_construct)?;
    let int32_array_name = scope.alloc_string("Int32Array")?;
    let int32_array = alloc_rooted_native_function(
      scope,
      roots,
      int32_array_call,
      Some(int32_array_construct),
      int32_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(int32_array, Some(function_prototype))?;
    scope.define_property(
      int32_array,
      common.prototype,
      data_desc(Value::Object(int32_array_prototype), true, false, false),
    )?;
    scope.define_property(
      int32_array,
      common.name,
      data_desc(Value::String(int32_array_name), false, false, true),
    )?;
    scope.define_property(
      int32_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      int32_array_prototype,
      common.constructor,
      data_desc(Value::Object(int32_array), true, false, true),
    )?;

    // `%Uint32Array%`
    let uint32_array_call = vm.register_native_call(builtins::uint32_array_constructor_call)?;
    let uint32_array_construct =
      vm.register_native_construct(builtins::uint32_array_constructor_construct)?;
    let uint32_array_name = scope.alloc_string("Uint32Array")?;
    let uint32_array = alloc_rooted_native_function(
      scope,
      roots,
      uint32_array_call,
      Some(uint32_array_construct),
      uint32_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(uint32_array, Some(function_prototype))?;
    scope.define_property(
      uint32_array,
      common.prototype,
      data_desc(Value::Object(uint32_array_prototype), true, false, false),
    )?;
    scope.define_property(
      uint32_array,
      common.name,
      data_desc(Value::String(uint32_array_name), false, false, true),
    )?;
    scope.define_property(
      uint32_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      uint32_array_prototype,
      common.constructor,
      data_desc(Value::Object(uint32_array), true, false, true),
    )?;

    // `%Float32Array%`
    let float32_array_call = vm.register_native_call(builtins::float32_array_constructor_call)?;
    let float32_array_construct =
      vm.register_native_construct(builtins::float32_array_constructor_construct)?;
    let float32_array_name = scope.alloc_string("Float32Array")?;
    let float32_array = alloc_rooted_native_function(
      scope,
      roots,
      float32_array_call,
      Some(float32_array_construct),
      float32_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(float32_array, Some(function_prototype))?;
    scope.define_property(
      float32_array,
      common.prototype,
      data_desc(Value::Object(float32_array_prototype), true, false, false),
    )?;
    scope.define_property(
      float32_array,
      common.name,
      data_desc(Value::String(float32_array_name), false, false, true),
    )?;
    scope.define_property(
      float32_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      float32_array_prototype,
      common.constructor,
      data_desc(Value::Object(float32_array), true, false, true),
    )?;

    // `%Float64Array%`
    let float64_array_call = vm.register_native_call(builtins::float64_array_constructor_call)?;
    let float64_array_construct =
      vm.register_native_construct(builtins::float64_array_constructor_construct)?;
    let float64_array_name = scope.alloc_string("Float64Array")?;
    let float64_array = alloc_rooted_native_function(
      scope,
      roots,
      float64_array_call,
      Some(float64_array_construct),
      float64_array_name,
      3,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(float64_array, Some(function_prototype))?;
    scope.define_property(
      float64_array,
      common.prototype,
      data_desc(Value::Object(float64_array_prototype), true, false, false),
    )?;
    scope.define_property(
      float64_array,
      common.name,
      data_desc(Value::String(float64_array_name), false, false, true),
    )?;
    scope.define_property(
      float64_array,
      common.length,
      data_desc(Value::Number(3.0), false, false, true),
    )?;
    scope.define_property(
      float64_array_prototype,
      common.constructor,
      data_desc(Value::Object(float64_array), true, false, true),
    )?;

    // `%DataView%`
    let data_view_call = vm.register_native_call(builtins::data_view_constructor_call)?;
    let data_view_construct =
      vm.register_native_construct(builtins::data_view_constructor_construct)?;
    let data_view_name = scope.alloc_string("DataView")?;
    let data_view = alloc_rooted_native_function(
      scope,
      roots,
      data_view_call,
      Some(data_view_construct),
      data_view_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(data_view, Some(function_prototype))?;
    scope.define_property(
      data_view,
      common.prototype,
      data_desc(Value::Object(data_view_prototype), true, false, false),
    )?;
    scope.define_property(
      data_view,
      common.name,
      data_desc(Value::String(data_view_name), false, false, true),
    )?;
    scope.define_property(
      data_view,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;
    scope.define_property(
      data_view_prototype,
      common.constructor,
      data_desc(Value::Object(data_view), true, false, true),
    )?;

    // `%WeakMap%`
    let weak_map_call = vm.register_native_call(builtins::weak_map_constructor_call)?;
    let weak_map_construct = vm.register_native_construct(builtins::weak_map_constructor_construct)?;
    let weak_map_name = scope.alloc_string("WeakMap")?;
    let weak_map = alloc_rooted_native_function(
      scope,
      roots,
      weak_map_call,
      Some(weak_map_construct),
      weak_map_name,
      0,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(weak_map, Some(function_prototype))?;
    scope.define_property(
      weak_map,
      common.prototype,
      data_desc(Value::Object(weak_map_prototype), true, false, false),
    )?;
    scope.define_property(
      weak_map,
      common.name,
      data_desc(Value::String(weak_map_name), false, false, true),
    )?;
    scope.define_property(
      weak_map,
      common.length,
      data_desc(Value::Number(0.0), false, false, true),
    )?;
    scope.define_property(
      weak_map_prototype,
      common.constructor,
      data_desc(Value::Object(weak_map), true, false, true),
    )?;

    // WeakMap.prototype.get / set / has / delete
    {
      let get_call = vm.register_native_call(builtins::weak_map_prototype_get)?;
      let get_s = scope.alloc_string("get")?;
      scope.push_root(Value::String(get_s))?;
      let key = PropertyKey::from_string(get_s);
      let func = scope.alloc_native_function(get_call, None, get_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_map_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;

      let set_call = vm.register_native_call(builtins::weak_map_prototype_set)?;
      let set_s = scope.alloc_string("set")?;
      scope.push_root(Value::String(set_s))?;
      let key = PropertyKey::from_string(set_s);
      let func = scope.alloc_native_function(set_call, None, set_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_map_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;

      let has_call = vm.register_native_call(builtins::weak_map_prototype_has)?;
      let has_s = scope.alloc_string("has")?;
      scope.push_root(Value::String(has_s))?;
      let key = PropertyKey::from_string(has_s);
      let func = scope.alloc_native_function(has_call, None, has_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_map_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;

      let delete_call = vm.register_native_call(builtins::weak_map_prototype_delete)?;
      let delete_s = scope.alloc_string("delete")?;
      scope.push_root(Value::String(delete_s))?;
      let key = PropertyKey::from_string(delete_s);
      let func = scope.alloc_native_function(delete_call, None, delete_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_map_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // `%WeakSet%`
    let weak_set_call = vm.register_native_call(builtins::weak_set_constructor_call)?;
    let weak_set_construct = vm.register_native_construct(builtins::weak_set_constructor_construct)?;
    let weak_set_name = scope.alloc_string("WeakSet")?;
    let weak_set = alloc_rooted_native_function(
      scope,
      roots,
      weak_set_call,
      Some(weak_set_construct),
      weak_set_name,
      0,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(weak_set, Some(function_prototype))?;
    scope.define_property(
      weak_set,
      common.prototype,
      data_desc(Value::Object(weak_set_prototype), true, false, false),
    )?;
    scope.define_property(
      weak_set,
      common.name,
      data_desc(Value::String(weak_set_name), false, false, true),
    )?;
    scope.define_property(
      weak_set,
      common.length,
      data_desc(Value::Number(0.0), false, false, true),
    )?;
    scope.define_property(
      weak_set_prototype,
      common.constructor,
      data_desc(Value::Object(weak_set), true, false, true),
    )?;

    // WeakSet.prototype.add / has / delete
    {
      let add_call = vm.register_native_call(builtins::weak_set_prototype_add)?;
      let add_s = scope.alloc_string("add")?;
      scope.push_root(Value::String(add_s))?;
      let key = PropertyKey::from_string(add_s);
      let func = scope.alloc_native_function(add_call, None, add_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_set_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;

      let has_call = vm.register_native_call(builtins::weak_set_prototype_has)?;
      let has_s = scope.alloc_string("has")?;
      scope.push_root(Value::String(has_s))?;
      let key = PropertyKey::from_string(has_s);
      let func = scope.alloc_native_function(has_call, None, has_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_set_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;

      let delete_call = vm.register_native_call(builtins::weak_set_prototype_delete)?;
      let delete_s = scope.alloc_string("delete")?;
      scope.push_root(Value::String(delete_s))?;
      let key = PropertyKey::from_string(delete_s);
      let func = scope.alloc_native_function(delete_call, None, delete_s, 1)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        weak_set_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    // --- TypedArray prototype accessors/methods ---
    let typed_array_byte_length_get_call =
      vm.register_native_call(builtins::typed_array_prototype_byte_length_get)?;
    let typed_array_length_get_call =
      vm.register_native_call(builtins::typed_array_prototype_length_get)?;
    let typed_array_byte_offset_get_call =
      vm.register_native_call(builtins::typed_array_prototype_byte_offset_get)?;
    let typed_array_buffer_get_call =
      vm.register_native_call(builtins::typed_array_prototype_buffer_get)?;
    let typed_array_slice_call = vm.register_native_call(builtins::typed_array_prototype_slice)?;
    let typed_array_subarray_call =
      vm.register_native_call(builtins::typed_array_prototype_subarray)?;
    let typed_array_set_call = vm.register_native_call(builtins::typed_array_prototype_set)?;

    let make_getter = |scope: &mut Scope<'_>,
                       call: NativeFunctionId,
                       name: &str|
     -> Result<GcObject, VmError> {
      let name_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(name_s))?;
      let get = scope.alloc_native_function(call, None, name_s, 0)?;
      scope.push_root(Value::Object(get))?;
      scope
        .heap_mut()
        .object_set_prototype(get, Some(function_prototype))?;
      Ok(get)
    };

    let byte_length_get = make_getter(scope, typed_array_byte_length_get_call, "get byteLength")?;
    let length_get = make_getter(scope, typed_array_length_get_call, "get length")?;
    let byte_offset_get = make_getter(scope, typed_array_byte_offset_get_call, "get byteOffset")?;
    let buffer_get = make_getter(scope, typed_array_buffer_get_call, "get buffer")?;

    let make_method =
      |scope: &mut Scope<'_>, call: NativeFunctionId, name: &str, length: u32| -> Result<GcObject, VmError> {
        let name_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(name_s))?;
        let func = scope.alloc_native_function(call, None, name_s, length)?;
        scope.push_root(Value::Object(func))?;
        scope
          .heap_mut()
          .object_set_prototype(func, Some(function_prototype))?;
        Ok(func)
      };

    let slice_fn = make_method(scope, typed_array_slice_call, "slice", 2)?;
    let subarray_fn = make_method(scope, typed_array_subarray_call, "subarray", 2)?;
    let set_fn = make_method(scope, typed_array_set_call, "set", 1)?;

    // Root the key strings across subsequent allocations: we allocate multiple keys before storing
    // them on any rooted object.
    let byte_length_key_s = scope.alloc_string("byteLength")?;
    scope.push_root(Value::String(byte_length_key_s))?;
    let byte_length_key = PropertyKey::from_string(byte_length_key_s);
    let byte_offset_key_s = scope.alloc_string("byteOffset")?;
    scope.push_root(Value::String(byte_offset_key_s))?;
    let byte_offset_key = PropertyKey::from_string(byte_offset_key_s);
    let buffer_key_s = scope.alloc_string("buffer")?;
    scope.push_root(Value::String(buffer_key_s))?;
    let buffer_key = PropertyKey::from_string(buffer_key_s);
    let slice_key_s = scope.alloc_string("slice")?;
    scope.push_root(Value::String(slice_key_s))?;
    let slice_key = PropertyKey::from_string(slice_key_s);
    let subarray_key_s = scope.alloc_string("subarray")?;
    scope.push_root(Value::String(subarray_key_s))?;
    let subarray_key = PropertyKey::from_string(subarray_key_s);
    let set_key_s = scope.alloc_string("set")?;
    scope.push_root(Value::String(set_key_s))?;
    let set_key = PropertyKey::from_string(set_key_s);

    let typed_array_prototypes = [
      uint8_array_prototype,
      int8_array_prototype,
      uint8_clamped_array_prototype,
      int16_array_prototype,
      uint16_array_prototype,
      int32_array_prototype,
      uint32_array_prototype,
      float32_array_prototype,
      float64_array_prototype,
    ];

    for proto in typed_array_prototypes {
      scope.define_property(
        proto,
        byte_length_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(byte_length_get),
            set: Value::Undefined,
          },
        },
      )?;
      scope.define_property(
        proto,
        common.length,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(length_get),
            set: Value::Undefined,
          },
        },
      )?;
      scope.define_property(
        proto,
        byte_offset_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(byte_offset_get),
            set: Value::Undefined,
          },
        },
      )?;
      scope.define_property(
        proto,
        buffer_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(buffer_get),
            set: Value::Undefined,
          },
        },
      )?;

      scope.define_property(proto, slice_key, data_desc(Value::Object(slice_fn), true, false, true))?;
      scope.define_property(proto, subarray_key, data_desc(Value::Object(subarray_fn), true, false, true))?;
      scope.define_property(proto, set_key, data_desc(Value::Object(set_fn), true, false, true))?;
    }

    // --- DataView prototype accessors/methods ---
    let data_view_byte_length_get_call =
      vm.register_native_call(builtins::data_view_prototype_byte_length_get)?;
    let data_view_byte_offset_get_call =
      vm.register_native_call(builtins::data_view_prototype_byte_offset_get)?;
    let data_view_buffer_get_call =
      vm.register_native_call(builtins::data_view_prototype_buffer_get)?;

    let data_view_byte_length_get = make_getter(scope, data_view_byte_length_get_call, "get byteLength")?;
    let data_view_byte_offset_get = make_getter(scope, data_view_byte_offset_get_call, "get byteOffset")?;
    let data_view_buffer_get = make_getter(scope, data_view_buffer_get_call, "get buffer")?;

    scope.define_property(
      data_view_prototype,
      byte_length_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(data_view_byte_length_get),
          set: Value::Undefined,
        },
      },
    )?;
    scope.define_property(
      data_view_prototype,
      byte_offset_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(data_view_byte_offset_get),
          set: Value::Undefined,
        },
      },
    )?;
    scope.define_property(
      data_view_prototype,
      buffer_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(data_view_buffer_get),
          set: Value::Undefined,
        },
      },
    )?;

    let mut define_dv_method =
      |name: &str, call: NativeFunctionId, length: u32| -> Result<(), VmError> {
        let func = make_method(scope, call, name, length)?;
        let key_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        scope.define_property(data_view_prototype, key, data_desc(Value::Object(func), true, false, true))
      };

    define_dv_method(
      "getInt8",
      vm.register_native_call(builtins::data_view_prototype_get_int8)?,
      1,
    )?;
    define_dv_method(
      "getUint8",
      vm.register_native_call(builtins::data_view_prototype_get_uint8)?,
      1,
    )?;
    define_dv_method(
      "getInt16",
      vm.register_native_call(builtins::data_view_prototype_get_int16)?,
      2,
    )?;
    define_dv_method(
      "getUint16",
      vm.register_native_call(builtins::data_view_prototype_get_uint16)?,
      2,
    )?;
    define_dv_method(
      "getInt32",
      vm.register_native_call(builtins::data_view_prototype_get_int32)?,
      2,
    )?;
    define_dv_method(
      "getUint32",
      vm.register_native_call(builtins::data_view_prototype_get_uint32)?,
      2,
    )?;
    define_dv_method(
      "getFloat32",
      vm.register_native_call(builtins::data_view_prototype_get_float32)?,
      2,
    )?;
    define_dv_method(
      "getFloat64",
      vm.register_native_call(builtins::data_view_prototype_get_float64)?,
      2,
    )?;

    define_dv_method(
      "setInt8",
      vm.register_native_call(builtins::data_view_prototype_set_int8)?,
      2,
    )?;
    define_dv_method(
      "setUint8",
      vm.register_native_call(builtins::data_view_prototype_set_uint8)?,
      2,
    )?;
    define_dv_method(
      "setInt16",
      vm.register_native_call(builtins::data_view_prototype_set_int16)?,
      3,
    )?;
    define_dv_method(
      "setUint16",
      vm.register_native_call(builtins::data_view_prototype_set_uint16)?,
      3,
    )?;
    define_dv_method(
      "setInt32",
      vm.register_native_call(builtins::data_view_prototype_set_int32)?,
      3,
    )?;
    define_dv_method(
      "setUint32",
      vm.register_native_call(builtins::data_view_prototype_set_uint32)?,
      3,
    )?;
    define_dv_method(
      "setFloat32",
      vm.register_native_call(builtins::data_view_prototype_set_float32)?,
      3,
    )?;
    define_dv_method(
      "setFloat64",
      vm.register_native_call(builtins::data_view_prototype_set_float64)?,
      3,
    )?;

    // `%Math%`
    let math = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(math, Some(object_prototype))?;
    {
      let mut define_const = |name: &str, value: f64| -> Result<(), VmError> {
        let name_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(name_s))?;
        let key = PropertyKey::from_string(name_s);
        scope.define_property(
          math,
          key,
          data_desc(Value::Number(value), false, false, false),
        )
      };

      define_const("E", std::f64::consts::E)?;
      define_const("LN2", std::f64::consts::LN_2)?;
      define_const("LN10", std::f64::consts::LN_10)?;
      define_const("LOG2E", std::f64::consts::LOG2_E)?;
      define_const("LOG10E", std::f64::consts::LOG10_E)?;
      define_const("PI", std::f64::consts::PI)?;
      define_const("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2)?;
      define_const("SQRT2", std::f64::consts::SQRT_2)?;
    }
    {
      let mut define_method =
        |name: &str, call: NativeFunctionId, length: u32| -> Result<(), VmError> {
          let name_s = scope.alloc_string(name)?;
          scope.push_root(Value::String(name_s))?;
          let key = PropertyKey::from_string(name_s);
          let func = scope.alloc_native_function(call, None, name_s, length)?;
          scope.push_root(Value::Object(func))?;
          scope
            .heap_mut()
            .object_set_prototype(func, Some(function_prototype))?;
          scope.define_property(math, key, data_desc(Value::Object(func), true, false, true))?;
          Ok(())
        };

      define_method("abs", math_abs, 1)?;
      define_method("acos", math_acos, 1)?;
      define_method("acosh", math_acosh, 1)?;
      define_method("asin", math_asin, 1)?;
      define_method("asinh", math_asinh, 1)?;
      define_method("atan", math_atan, 1)?;
      define_method("atan2", math_atan2, 2)?;
      define_method("atanh", math_atanh, 1)?;
      define_method("cbrt", math_cbrt, 1)?;
      define_method("clz32", math_clz32, 1)?;
      define_method("floor", math_floor, 1)?;
      define_method("ceil", math_ceil, 1)?;
      define_method("cos", math_cos, 1)?;
      define_method("cosh", math_cosh, 1)?;
      define_method("expm1", math_expm1, 1)?;
      define_method("fround", math_fround, 1)?;
      define_method("hypot", math_hypot, 2)?;
      define_method("imul", math_imul, 2)?;
      define_method("log1p", math_log1p, 1)?;
      define_method("log10", math_log10, 1)?;
      define_method("log2", math_log2, 1)?;
      define_method("trunc", math_trunc, 1)?;
      define_method("round", math_round, 1)?;
      define_method("max", math_max, 2)?;
      define_method("min", math_min, 2)?;
      define_method("pow", math_pow, 2)?;
      define_method("sqrt", math_sqrt, 1)?;
      define_method("log", math_log, 1)?;
      define_method("exp", math_exp, 1)?;
      define_method("sign", math_sign, 1)?;
      define_method("sin", math_sin, 1)?;
      define_method("sinh", math_sinh, 1)?;
      define_method("tan", math_tan, 1)?;
      define_method("tanh", math_tanh, 1)?;
      define_method("random", math_random, 0)?;
    }

    {
      let to_string_tag_value = scope.alloc_string("Math")?;
      scope.define_property(
        math,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(to_string_tag_value), false, false, true),
      )?;
    }

    // `%JSON%`
    let json = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(json, Some(object_prototype))?;
    {
      let parse_s = scope.alloc_string("parse")?;
      scope.push_root(Value::String(parse_s))?;
      let key = PropertyKey::from_string(parse_s);
      let func = scope.alloc_native_function(json_parse, None, parse_s, 2)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(json, key, data_desc(Value::Object(func), true, false, true))?;
    }
    {
      let stringify_s = scope.alloc_string("stringify")?;
      scope.push_root(Value::String(stringify_s))?;
      let key = PropertyKey::from_string(stringify_s);
      let func = scope.alloc_native_function(json_stringify, None, stringify_s, 3)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(json, key, data_desc(Value::Object(func), true, false, true))?;
    }
    {
      let to_string_tag_value = scope.alloc_string("JSON")?;
      scope.define_property(
        json,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(to_string_tag_value), false, false, true),
      )?;
    }

    // `%Reflect%`
    let reflect = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(reflect, Some(object_prototype))?;
    {
      let mut define_method =
        |name: &str, call: NativeFunctionId, length: u32| -> Result<(), VmError> {
          let name_s = scope.alloc_string(name)?;
          scope.push_root(Value::String(name_s))?;
          let key = PropertyKey::from_string(name_s);
          let func = scope.alloc_native_function(call, None, name_s, length)?;
          scope.push_root(Value::Object(func))?;
          scope
            .heap_mut()
            .object_set_prototype(func, Some(function_prototype))?;
          scope.define_property(reflect, key, data_desc(Value::Object(func), true, false, true))?;
          Ok(())
        };
 
      define_method("apply", reflect_apply, 3)?;
      define_method("construct", reflect_construct, 2)?;
      define_method("defineProperty", reflect_define_property, 3)?;
      define_method("deleteProperty", reflect_delete_property, 2)?;
      define_method("get", reflect_get, 2)?;
      define_method("getOwnPropertyDescriptor", reflect_get_own_property_descriptor, 2)?;
      define_method("getPrototypeOf", reflect_get_prototype_of, 1)?;
      define_method("has", reflect_has, 2)?;
      define_method("isExtensible", reflect_is_extensible, 1)?;
      define_method("ownKeys", reflect_own_keys, 1)?;
      define_method("preventExtensions", reflect_prevent_extensions, 1)?;
      define_method("set", reflect_set, 3)?;
      define_method("setPrototypeOf", reflect_set_prototype_of, 2)?;
    }

    {
      let to_string_tag_value = scope.alloc_string("Reflect")?;
      scope.define_property(
        reflect,
        PropertyKey::Symbol(well_known_symbols.to_string_tag),
        data_desc(Value::String(to_string_tag_value), false, false, true),
      )?;
    }
    // --- Error + subclasses ---
    let error_call = vm.register_native_call(builtins::error_constructor_call)?;
    let error_construct = vm.register_native_construct(builtins::error_constructor_construct)?;
    let (error, error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      function_prototype,
      object_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "Error",
      1,
    )?;

    // Error.prototype.message
    //
    // Per ECMA-262, Error instances created without an explicit message argument inherit the empty
    // string `message` from `%Error.prototype%`.
    {
      let message_s = scope.alloc_string("message")?;
      scope.push_root(Value::String(message_s))?;
      let key = PropertyKey::from_string(message_s);
      let empty = scope.alloc_string("")?;
      scope.push_root(Value::String(empty))?;
      scope.define_property(
        error_prototype,
        key,
        data_desc(Value::String(empty), true, false, true),
      )?;
    }

    // Error.prototype.toString
    {
      let to_string_s = scope.alloc_string("toString")?;
      scope.push_root(Value::String(to_string_s))?;
      let key = PropertyKey::from_string(to_string_s);
      let func = scope.alloc_native_function(error_prototype_to_string, None, to_string_s, 0)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(function_prototype))?;
      scope.define_property(
        error_prototype,
        key,
        data_desc(Value::Object(func), true, false, true),
      )?;
    }

    let (type_error, type_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "TypeError",
      1,
    )?;

    let (range_error, range_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "RangeError",
      1,
    )?;

    let (reference_error, reference_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "ReferenceError",
      1,
    )?;

    let (syntax_error, syntax_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "SyntaxError",
      1,
    )?;

    let (eval_error, eval_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "EvalError",
      1,
    )?;

    let (uri_error, uri_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "URIError",
      1,
    )?;

    let (aggregate_error, aggregate_error_prototype) = init_native_error(
      vm,
      scope,
      roots,
      common,
      error,
      error_prototype,
      well_known_symbols.to_string_tag,
      error_call,
      error_construct,
      "AggregateError",
      2,
    )?;

    // --- Promise ---
    let promise_prototype = alloc_rooted_object(scope, roots)?;
    scope
      .heap_mut()
      .object_set_prototype(promise_prototype, Some(object_prototype))?;
    install_to_string_tag(
      scope,
      promise_prototype,
      well_known_symbols.to_string_tag,
      "Promise",
    )?;

    let promise_capability_executor_call =
      vm.register_native_call(builtins::promise_capability_executor_call)?;
    let promise_resolving_function_call =
      vm.register_native_call(builtins::promise_resolving_function_call)?;
    let promise_finally_handler_call =
      vm.register_native_call(builtins::promise_finally_handler_call)?;
    let promise_finally_thunk_call =
      vm.register_native_call(builtins::promise_finally_thunk_call)?;
    let promise_all_resolve_element_call =
      vm.register_native_call(builtins::promise_all_resolve_element_call)?;
    let promise_all_settled_element_call =
      vm.register_native_call(builtins::promise_all_settled_element_call)?;
    let promise_any_reject_element_call =
      vm.register_native_call(builtins::promise_any_reject_element_call)?;

    let class_constructor_call = vm.register_native_call(builtins::class_constructor_call)?;
    let class_constructor_construct =
      vm.register_native_construct(builtins::class_constructor_construct)?;

    let promise_call = vm.register_native_call(builtins::promise_constructor_call)?;
    let promise_construct = vm.register_native_construct(builtins::promise_constructor_construct)?;
    let promise_name = scope.alloc_string("Promise")?;
    let promise = alloc_rooted_native_function(
      scope,
      roots,
      promise_call,
      Some(promise_construct),
      promise_name,
      1,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(promise, Some(function_prototype))?;

    // Promise.prototype.constructor
    scope.define_property(
      promise_prototype,
      common.constructor,
      data_desc(Value::Object(promise), true, false, true),
    )?;

    // Promise.prototype on the constructor.
    scope.define_property(
      promise,
      common.prototype,
      data_desc(Value::Object(promise_prototype), true, false, false),
    )?;

    // Promise.name / Promise.length
    scope.define_property(
      promise,
      common.name,
      data_desc(Value::String(promise_name), false, false, true),
    )?;
    scope.define_property(
      promise,
      common.length,
      data_desc(Value::Number(1.0), false, false, true),
    )?;

    // Promise[@@species]
    //
    // Spec: `get Promise [ @@species ]` (ECMA-262).
    //
    // The getter returns the receiver and is used by `SpeciesConstructor`.
    let promise_species_call = vm.register_native_call(builtins::promise_species_get)?;
    let promise_species_name = scope.alloc_string("get [Symbol.species]")?;
    let promise_species_getter = alloc_rooted_native_function(
      scope,
      roots,
      promise_species_call,
      None,
      promise_species_name,
      0,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(promise_species_getter, Some(function_prototype))?;
    scope.define_property(
      promise,
      PropertyKey::Symbol(well_known_symbols.species),
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(promise_species_getter),
          set: Value::Undefined,
        },
      },
    )?;

    // Promise.resolve / Promise.reject
    {
      let resolve_call = vm.register_native_call(builtins::promise_resolve)?;
      let resolve_name = scope.alloc_string("resolve")?;
      let resolve = alloc_rooted_native_function(scope, roots, resolve_call, None, resolve_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(resolve, Some(function_prototype))?;

      let key = PropertyKey::from_string(scope.alloc_string("resolve")?);
      scope.define_property(
        promise,
        key,
        data_desc(Value::Object(resolve), true, false, true),
      )?;

      let reject_call = vm.register_native_call(builtins::promise_reject)?;
      let reject_name = scope.alloc_string("reject")?;
      let reject = alloc_rooted_native_function(scope, roots, reject_call, None, reject_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(reject, Some(function_prototype))?;

      let key = PropertyKey::from_string(scope.alloc_string("reject")?);
      scope.define_property(
        promise,
        key,
        data_desc(Value::Object(reject), true, false, true),
      )?;
    }

    // Promise.all / Promise.race / Promise.allSettled / Promise.any
    {
      let all_call = vm.register_native_call(builtins::promise_all)?;
      let all_name = scope.alloc_string("all")?;
      let all = alloc_rooted_native_function(scope, roots, all_call, None, all_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(all, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("all")?);
      scope.define_property(promise, key, data_desc(Value::Object(all), true, false, true))?;

      let race_call = vm.register_native_call(builtins::promise_race)?;
      let race_name = scope.alloc_string("race")?;
      let race = alloc_rooted_native_function(scope, roots, race_call, None, race_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(race, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("race")?);
      scope.define_property(promise, key, data_desc(Value::Object(race), true, false, true))?;

      let all_settled_call = vm.register_native_call(builtins::promise_all_settled)?;
      let all_settled_name = scope.alloc_string("allSettled")?;
      let all_settled = alloc_rooted_native_function(
        scope,
        roots,
        all_settled_call,
        None,
        all_settled_name,
        1,
      )?;
      scope
        .heap_mut()
        .object_set_prototype(all_settled, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("allSettled")?);
      scope.define_property(
        promise,
        key,
        data_desc(Value::Object(all_settled), true, false, true),
      )?;

      let any_call = vm.register_native_call(builtins::promise_any)?;
      let any_name = scope.alloc_string("any")?;
      let any = alloc_rooted_native_function(scope, roots, any_call, None, any_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(any, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("any")?);
      scope.define_property(promise, key, data_desc(Value::Object(any), true, false, true))?;
    }

    // Promise.try / Promise.withResolvers
    {
      let try_call = vm.register_native_call(builtins::promise_try)?;
      let try_name = scope.alloc_string("try")?;
      let try_ = alloc_rooted_native_function(scope, roots, try_call, None, try_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(try_, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("try")?);
      scope.define_property(promise, key, data_desc(Value::Object(try_), true, false, true))?;

      let with_resolvers_call = vm.register_native_call(builtins::promise_with_resolvers)?;
      let with_resolvers_name = scope.alloc_string("withResolvers")?;
      let with_resolvers = alloc_rooted_native_function(
        scope,
        roots,
        with_resolvers_call,
        None,
        with_resolvers_name,
        0,
      )?;
      scope
        .heap_mut()
        .object_set_prototype(with_resolvers, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("withResolvers")?);
      scope.define_property(
        promise,
        key,
        data_desc(Value::Object(with_resolvers), true, false, true),
      )?;
    }

    // Promise.all / Promise.race / Promise.allSettled / Promise.any
    {
      let all_call = vm.register_native_call(builtins::promise_all)?;
      let all_name = scope.alloc_string("all")?;
      let all = alloc_rooted_native_function(scope, roots, all_call, None, all_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(all, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("all")?);
      scope.define_property(promise, key, data_desc(Value::Object(all), true, false, true))?;

      let race_call = vm.register_native_call(builtins::promise_race)?;
      let race_name = scope.alloc_string("race")?;
      let race = alloc_rooted_native_function(scope, roots, race_call, None, race_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(race, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("race")?);
      scope.define_property(promise, key, data_desc(Value::Object(race), true, false, true))?;

      let all_settled_call = vm.register_native_call(builtins::promise_all_settled)?;
      let all_settled_name = scope.alloc_string("allSettled")?;
      let all_settled = alloc_rooted_native_function(
        scope,
        roots,
        all_settled_call,
        None,
        all_settled_name,
        1,
      )?;
      scope.heap_mut().object_set_prototype(
        all_settled,
        Some(function_prototype),
      )?;
      let key = PropertyKey::from_string(scope.alloc_string("allSettled")?);
      scope.define_property(
        promise,
        key,
        data_desc(Value::Object(all_settled), true, false, true),
      )?;

      let any_call = vm.register_native_call(builtins::promise_any)?;
      let any_name = scope.alloc_string("any")?;
      let any = alloc_rooted_native_function(scope, roots, any_call, None, any_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(any, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("any")?);
      scope.define_property(promise, key, data_desc(Value::Object(any), true, false, true))?;
    }

    // Promise.prototype.then / Promise.prototype.catch / Promise.prototype.finally
    let promise_prototype_then;
    {
      let then_call = vm.register_native_call(builtins::promise_prototype_then)?;
      let then_name = scope.alloc_string("then")?;
      let then = alloc_rooted_native_function(scope, roots, then_call, None, then_name, 2)?;
      scope
        .heap_mut()
        .object_set_prototype(then, Some(function_prototype))?;
      promise_prototype_then = then;

      let key = PropertyKey::from_string(scope.alloc_string("then")?);
      scope.define_property(
        promise_prototype,
        key,
        data_desc(Value::Object(then), true, false, true),
      )?;

      let catch_call = vm.register_native_call(builtins::promise_prototype_catch)?;
      let catch_name = scope.alloc_string("catch")?;
      let catch_ = alloc_rooted_native_function(scope, roots, catch_call, None, catch_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(catch_, Some(function_prototype))?;

      let key = PropertyKey::from_string(scope.alloc_string("catch")?);
      scope.define_property(
        promise_prototype,
        key,
        data_desc(Value::Object(catch_), true, false, true),
      )?;

      let finally_call = vm.register_native_call(builtins::promise_prototype_finally)?;
      let finally_name = scope.alloc_string("finally")?;
      let finally_ = alloc_rooted_native_function(scope, roots, finally_call, None, finally_name, 1)?;
      scope
        .heap_mut()
        .object_set_prototype(finally_, Some(function_prototype))?;
      let key = PropertyKey::from_string(scope.alloc_string("finally")?);
      scope.define_property(
        promise_prototype,
        key,
        data_desc(Value::Object(finally_), true, false, true),
      )?;
    }
    Ok(Self {
      well_known_symbols,
      object_prototype,
      function_prototype,
      iterator_prototype,
      generator_function,
      generator_function_prototype,
      generator_prototype,
      array_iterator_prototype,
      array_prototype,
      string_iterator_prototype,
      string_prototype,
      regexp_prototype,
      number_prototype,
      boolean_prototype,
      bigint_prototype,
      date_prototype,
      symbol_prototype,
      array_buffer_prototype,
      uint8_array_prototype,
      int8_array_prototype,
      uint8_clamped_array_prototype,
      int16_array_prototype,
      uint16_array_prototype,
      int32_array_prototype,
      uint32_array_prototype,
      float32_array_prototype,
      float64_array_prototype,
      data_view_prototype,
      weak_map_prototype,
      weak_set_prototype,
      object_constructor,
      function_constructor,
      generator_function_constructor: generator_function,
      array_constructor,
      proxy_constructor,
      string_constructor,
      regexp_constructor,
      number_constructor,
      boolean_constructor,
      date_constructor,
      symbol_constructor,
      array_buffer,
      uint8_array,
      int8_array,
      uint8_clamped_array,
      int16_array,
      uint16_array,
      int32_array,
      uint32_array,
      float32_array,
      float64_array,
      data_view,
      weak_map,
      weak_set,
      is_nan,
      is_finite,
      eval,
      parse_int,
      parse_float,
      encode_uri,
      encode_uri_component,
      decode_uri,
      decode_uri_component,
      math,
      json,
      reflect,
      error,
      error_prototype,
      type_error,
      type_error_prototype,
      range_error,
      range_error_prototype,
      reference_error,
      reference_error_prototype,
      syntax_error,
      syntax_error_prototype,
      eval_error,
      eval_error_prototype,
      uri_error,
      uri_error_prototype,
      aggregate_error,
      aggregate_error_prototype,

      promise,
      promise_prototype,
      promise_prototype_then,
      promise_capability_executor_call,
      promise_resolving_function_call,
      promise_finally_handler_call,
      promise_finally_thunk_call,
      promise_all_resolve_element_call,
      promise_all_settled_element_call,
      promise_any_reject_element_call,

      proxy_revoker_call,

      class_constructor_call,
      class_constructor_construct,
    })
  }

  pub fn well_known_symbols(&self) -> &WellKnownSymbols {
    &self.well_known_symbols
  }
  pub fn object_prototype(&self) -> GcObject {
    self.object_prototype
  }

  pub fn function_prototype(&self) -> GcObject {
    self.function_prototype
  }

  pub fn iterator_prototype(&self) -> GcObject {
    self.iterator_prototype
  }

  pub fn generator_function(&self) -> GcObject {
    self.generator_function
  }

  pub fn generator_function_prototype(&self) -> GcObject {
    self.generator_function_prototype
  }

  pub fn generator_prototype(&self) -> GcObject {
    self.generator_prototype
  }

  pub(crate) fn array_iterator_prototype(&self) -> GcObject {
    self.array_iterator_prototype
  }

  pub fn array_prototype(&self) -> GcObject {
    self.array_prototype
  }

  pub(crate) fn string_iterator_prototype(&self) -> GcObject {
    self.string_iterator_prototype
  }

  pub fn string_prototype(&self) -> GcObject {
    self.string_prototype
  }

  pub fn regexp_prototype(&self) -> GcObject {
    self.regexp_prototype
  }

  pub fn number_prototype(&self) -> GcObject {
    self.number_prototype
  }

  pub fn boolean_prototype(&self) -> GcObject {
    self.boolean_prototype
  }

  pub fn bigint_prototype(&self) -> GcObject {
    self.bigint_prototype
  }

  pub fn date_prototype(&self) -> GcObject {
    self.date_prototype
  }

  pub fn symbol_prototype(&self) -> GcObject {
    self.symbol_prototype
  }

  pub fn array_buffer_prototype(&self) -> GcObject {
    self.array_buffer_prototype
  }

  pub fn uint8_array_prototype(&self) -> GcObject {
    self.uint8_array_prototype
  }

  pub fn int8_array_prototype(&self) -> GcObject {
    self.int8_array_prototype
  }

  pub fn uint8_clamped_array_prototype(&self) -> GcObject {
    self.uint8_clamped_array_prototype
  }

  pub fn int16_array_prototype(&self) -> GcObject {
    self.int16_array_prototype
  }

  pub fn uint16_array_prototype(&self) -> GcObject {
    self.uint16_array_prototype
  }

  pub fn int32_array_prototype(&self) -> GcObject {
    self.int32_array_prototype
  }

  pub fn uint32_array_prototype(&self) -> GcObject {
    self.uint32_array_prototype
  }

  pub fn float32_array_prototype(&self) -> GcObject {
    self.float32_array_prototype
  }

  pub fn float64_array_prototype(&self) -> GcObject {
    self.float64_array_prototype
  }

  pub fn data_view_prototype(&self) -> GcObject {
    self.data_view_prototype
  }

  pub fn weak_map_prototype(&self) -> GcObject {
    self.weak_map_prototype
  }

  pub fn weak_set_prototype(&self) -> GcObject {
    self.weak_set_prototype
  }

  pub fn object_constructor(&self) -> GcObject {
    self.object_constructor
  }

  pub fn function_constructor(&self) -> GcObject {
    self.function_constructor
  }

  pub fn generator_function_constructor(&self) -> GcObject {
    self.generator_function_constructor
  }

  pub fn array_constructor(&self) -> GcObject {
    self.array_constructor
  }

  pub fn proxy_constructor(&self) -> GcObject {
    self.proxy_constructor
  }

  pub fn string_constructor(&self) -> GcObject {
    self.string_constructor
  }

  pub fn regexp_constructor(&self) -> GcObject {
    self.regexp_constructor
  }

  pub fn number_constructor(&self) -> GcObject {
    self.number_constructor
  }

  pub fn boolean_constructor(&self) -> GcObject {
    self.boolean_constructor
  }

  pub fn date_constructor(&self) -> GcObject {
    self.date_constructor
  }

  pub fn symbol_constructor(&self) -> GcObject {
    self.symbol_constructor
  }

  pub fn array_buffer(&self) -> GcObject {
    self.array_buffer
  }

  pub fn uint8_array(&self) -> GcObject {
    self.uint8_array
  }

  pub fn int8_array(&self) -> GcObject {
    self.int8_array
  }

  pub fn uint8_clamped_array(&self) -> GcObject {
    self.uint8_clamped_array
  }

  pub fn int16_array(&self) -> GcObject {
    self.int16_array
  }

  pub fn uint16_array(&self) -> GcObject {
    self.uint16_array
  }

  pub fn int32_array(&self) -> GcObject {
    self.int32_array
  }

  pub fn uint32_array(&self) -> GcObject {
    self.uint32_array
  }

  pub fn float32_array(&self) -> GcObject {
    self.float32_array
  }

  pub fn float64_array(&self) -> GcObject {
    self.float64_array
  }

  pub fn data_view(&self) -> GcObject {
    self.data_view
  }

  pub fn weak_map(&self) -> GcObject {
    self.weak_map
  }

  pub fn weak_set(&self) -> GcObject {
    self.weak_set
  }

  pub fn is_nan(&self) -> GcObject {
    self.is_nan
  }

  pub fn is_finite(&self) -> GcObject {
    self.is_finite
  }

  pub fn eval(&self) -> GcObject {
    self.eval
  }

  pub fn parse_int(&self) -> GcObject {
    self.parse_int
  }

  pub fn parse_float(&self) -> GcObject {
    self.parse_float
  }

  pub fn encode_uri(&self) -> GcObject {
    self.encode_uri
  }

  pub fn encode_uri_component(&self) -> GcObject {
    self.encode_uri_component
  }

  pub fn decode_uri(&self) -> GcObject {
    self.decode_uri
  }

  pub fn decode_uri_component(&self) -> GcObject {
    self.decode_uri_component
  }

  pub fn math(&self) -> GcObject {
    self.math
  }

  pub fn json(&self) -> GcObject {
    self.json
  }

  pub fn reflect(&self) -> GcObject {
    self.reflect
  }

  pub fn error(&self) -> GcObject {
    self.error
  }

  pub fn error_prototype(&self) -> GcObject {
    self.error_prototype
  }

  pub fn type_error(&self) -> GcObject {
    self.type_error
  }

  pub fn type_error_prototype(&self) -> GcObject {
    self.type_error_prototype
  }

  pub fn range_error(&self) -> GcObject {
    self.range_error
  }

  pub fn range_error_prototype(&self) -> GcObject {
    self.range_error_prototype
  }

  pub fn reference_error(&self) -> GcObject {
    self.reference_error
  }

  pub fn reference_error_prototype(&self) -> GcObject {
    self.reference_error_prototype
  }

  pub fn syntax_error(&self) -> GcObject {
    self.syntax_error
  }

  pub fn syntax_error_prototype(&self) -> GcObject {
    self.syntax_error_prototype
  }

  pub fn eval_error(&self) -> GcObject {
    self.eval_error
  }

  pub fn eval_error_prototype(&self) -> GcObject {
    self.eval_error_prototype
  }

  pub fn uri_error(&self) -> GcObject {
    self.uri_error
  }

  pub fn uri_error_prototype(&self) -> GcObject {
    self.uri_error_prototype
  }

  pub fn aggregate_error(&self) -> GcObject {
    self.aggregate_error
  }

  pub fn aggregate_error_prototype(&self) -> GcObject {
    self.aggregate_error_prototype
  }

  pub fn promise(&self) -> GcObject {
    self.promise
  }

  pub fn promise_prototype(&self) -> GcObject {
    self.promise_prototype
  }

  pub(crate) fn promise_prototype_then(&self) -> GcObject {
    self.promise_prototype_then
  }

  pub(crate) fn promise_capability_executor_call(&self) -> NativeFunctionId {
    self.promise_capability_executor_call
  }

  pub(crate) fn promise_resolving_function_call(&self) -> NativeFunctionId {
    self.promise_resolving_function_call
  }

  pub(crate) fn promise_finally_handler_call(&self) -> NativeFunctionId {
    self.promise_finally_handler_call
  }

  pub(crate) fn promise_finally_thunk_call(&self) -> NativeFunctionId {
    self.promise_finally_thunk_call
  }

  pub(crate) fn promise_all_resolve_element_call(&self) -> NativeFunctionId {
    self.promise_all_resolve_element_call
  }

  pub(crate) fn promise_all_settled_element_call(&self) -> NativeFunctionId {
    self.promise_all_settled_element_call
  }

  pub(crate) fn promise_any_reject_element_call(&self) -> NativeFunctionId {
    self.promise_any_reject_element_call
  }

  pub(crate) fn proxy_revoker_call(&self) -> NativeFunctionId {
    self.proxy_revoker_call
  }

  pub(crate) fn class_constructor_call(&self) -> NativeFunctionId {
    self.class_constructor_call
  }

  pub(crate) fn class_constructor_construct(&self) -> NativeConstructId {
    self.class_constructor_construct
  }
}

impl WellKnownSymbols {
  fn init(scope: &mut Scope<'_>, roots: &mut Vec<RootId>) -> Result<Self, VmError> {
    Ok(Self {
      async_iterator: alloc_rooted_symbol(scope, roots, "Symbol.asyncIterator")?,
      has_instance: alloc_rooted_symbol(scope, roots, "Symbol.hasInstance")?,
      is_concat_spreadable: alloc_rooted_symbol(scope, roots, "Symbol.isConcatSpreadable")?,
      iterator: alloc_rooted_symbol(scope, roots, "Symbol.iterator")?,
      match_: alloc_rooted_symbol(scope, roots, "Symbol.match")?,
      match_all: alloc_rooted_symbol(scope, roots, "Symbol.matchAll")?,
      replace: alloc_rooted_symbol(scope, roots, "Symbol.replace")?,
      search: alloc_rooted_symbol(scope, roots, "Symbol.search")?,
      species: alloc_rooted_symbol(scope, roots, "Symbol.species")?,
      split: alloc_rooted_symbol(scope, roots, "Symbol.split")?,
      to_primitive: alloc_rooted_symbol(scope, roots, "Symbol.toPrimitive")?,
      to_string_tag: alloc_rooted_symbol(scope, roots, "Symbol.toStringTag")?,
      unscopables: alloc_rooted_symbol(scope, roots, "Symbol.unscopables")?,
    })
  }
}
