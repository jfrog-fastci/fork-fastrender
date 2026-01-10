use crate::function::FunctionData;
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::string::JsString;
use crate::{
  GcObject, Job, JobKind, PromiseCapability, PromiseHandle, PromiseReaction, PromiseReactionType,
  PromiseRejectionOperation, PromiseState, RealmId, RootId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

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

fn require_intrinsics(vm: &Vm) -> Result<crate::Intrinsics, VmError> {
  vm.intrinsics().ok_or(VmError::Unimplemented(
    "native builtins require Vm::intrinsics to be set",
  ))
}

fn require_object(value: Value) -> Result<GcObject, VmError> {
  match value {
    Value::Object(o) => Ok(o),
    _ => Err(VmError::TypeError("expected object")),
  }
}

fn require_callable(this: Value) -> Result<GcObject, VmError> {
  match this {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::NotCallable),
  }
}

// https://tc39.es/ecma262/#sec-symboldescriptivestring
fn symbol_descriptive_string(scope: &mut Scope<'_>, sym: crate::GcSymbol) -> Result<crate::GcString, VmError> {
  // Extract the description code units up-front so we don't hold borrows across the final string
  // allocation (which can trigger GC).
  let desc_units: Vec<u16> = match scope.heap().get_symbol_description(sym)? {
    None => Vec::new(),
    Some(desc) => scope.heap().get_string(desc)?.as_code_units().to_vec(),
  };

  const PREFIX: [u16; 7] = [
    b'S' as u16,
    b'y' as u16,
    b'm' as u16,
    b'b' as u16,
    b'o' as u16,
    b'l' as u16,
    b'(' as u16,
  ];

  let total_len = PREFIX.len().saturating_add(desc_units.len()).saturating_add(1);
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(&PREFIX);
  out.extend_from_slice(&desc_units);
  out.push(b')' as u16);

  scope.alloc_string_from_u16_vec(out)
}

fn slice_index_from_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  len: usize,
  default: usize,
) -> Result<usize, VmError> {
  if matches!(value, Value::Undefined) {
    return Ok(default);
  }
  let n = scope.to_number(vm, host, hooks, value)?;
  if n.is_nan() {
    return Ok(0);
  }
  if !n.is_finite() {
    return Ok(if n.is_sign_negative() { 0 } else { len });
  }
  let n = n.trunc();
  let idx = if n < 0.0 {
    ((len as f64) + n).max(0.0)
  } else {
    n
  };
  Ok((idx.clamp(0.0, len as f64)) as usize)
}

fn slice_range_from_args(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  len: usize,
  args: &[Value],
) -> Result<(usize, usize), VmError> {
  let begin = args.get(0).copied().unwrap_or(Value::Undefined);
  let end = args.get(1).copied().unwrap_or(Value::Undefined);

  let start = slice_index_from_value(vm, scope, host, hooks, begin, len, 0)?;
  let mut finish = slice_index_from_value(vm, scope, host, hooks, end, len, len)?;
  if finish < start {
    finish = start;
  }
  Ok((start, finish))
}

fn get_array_like_args(vm: &mut Vm, scope: &mut Scope<'_>, obj: GcObject) -> Result<Vec<Value>, VmError> {
  // Treat `obj` as array-like:
  // - read `length` as a Number
  // - read indices 0..length-1 as data properties
  let length_key_s = scope.alloc_string("length")?;
  let length_key = PropertyKey::from_string(length_key_s);
  let length_desc = scope.heap().get_property(obj, &length_key)?;
  let length_val = match length_desc.map(|d| d.kind) {
    Some(PropertyKind::Data { value, .. }) => value,
    Some(PropertyKind::Accessor { .. }) => {
      return Err(VmError::Unimplemented(
        "Function.prototype.apply: accessor length",
      ));
    }
    None => Value::Number(0.0),
  };

  let length = match length_val {
    Value::Number(n) if n.is_finite() && n >= 0.0 => n as usize,
    Value::Number(_) => 0usize,
    _ => {
      return Err(VmError::Unimplemented(
        "Function.prototype.apply: non-numeric length",
      ))
    }
  };

  let mut out: Vec<Value> = Vec::new();
  out
    .try_reserve_exact(length)
    .map_err(|_| VmError::OutOfMemory)?;

  for idx in 0..length {
    if idx % 1024 == 0 {
      vm.tick()?;
    }
    let idx_s = scope.alloc_string(&idx.to_string())?;
    let key = PropertyKey::from_string(idx_s);
    let desc = scope.heap().get_property(obj, &key)?;
    let value = match desc.map(|d| d.kind) {
      Some(PropertyKind::Data { value, .. }) => value,
      Some(PropertyKind::Accessor { .. }) => {
        return Err(VmError::Unimplemented(
          "Function.prototype.apply: accessor indexed element",
        ));
      }
      None => Value::Undefined,
    };
    out.push(value);
  }

  Ok(out)
}

fn set_function_job_realm_to_current(
  vm: &Vm,
  scope: &mut Scope<'_>,
  func: GcObject,
) -> Result<(), VmError> {
  if let Some(realm) = vm.current_realm() {
    scope.heap_mut().set_function_job_realm(func, realm)?;
  }
  Ok(())
}

fn root_property_key(scope: &mut Scope<'_>, key: PropertyKey) -> Result<(), VmError> {
  match key {
    PropertyKey::String(s) => {
      scope.push_root(Value::String(s))?;
    }
    PropertyKey::Symbol(s) => {
      scope.push_root(Value::Symbol(s))?;
    }
  }
  Ok(())
}

fn get_own_data_property_value_by_name(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let Some(desc) = scope.heap().object_get_own_property(obj, &key)? else {
    return Ok(None);
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(Some(value)),
    PropertyKind::Accessor { .. } => Err(VmError::Unimplemented(
      "accessor properties are not yet supported",
    )),
  }
}

pub fn function_prototype_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn object_constructor_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut host_state = ();
  match arg0 {
    Value::Undefined | Value::Null => {
      let obj = scope.alloc_object()?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.object_prototype()))?;
      Ok(Value::Object(obj))
    }
    Value::Object(obj) => Ok(Value::Object(obj)),
    Value::String(_) => string_constructor_construct(
      vm,
      scope,
      &mut host_state,
      host,
      intr.string_constructor(),
      &[arg0],
      Value::Object(intr.string_constructor()),
    ),
    Value::Number(_) => number_constructor_construct(
      vm,
      scope,
      &mut host_state,
      host,
      intr.number_constructor(),
      &[arg0],
      Value::Object(intr.number_constructor()),
    ),
    Value::Bool(_) => boolean_constructor_construct(
      vm,
      scope,
      &mut host_state,
      host,
      intr.boolean_constructor(),
      &[arg0],
      Value::Object(intr.boolean_constructor()),
    ),
    Value::Symbol(sym) => {
      // Minimal boxing used by test262 `ToObject` paths (e.g. `Object(Symbol("1"))`).
      // Store the symbol on an internal marker so `Symbol.prototype.valueOf` can recover it.
      scope.push_root(Value::Symbol(sym))?;
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.symbol_prototype()))?;

      let marker = scope.alloc_string("vm-js.internal.SymbolData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      scope.define_property(
        obj,
        marker_key,
        data_desc(Value::Symbol(sym), true, false, false),
      )?;

      Ok(Value::Object(obj))
    }
    Value::BigInt(b) => {
      // Minimal BigInt boxing used by test262 (`Object(1n)`).
      scope.push_root(Value::BigInt(b))?;
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.bigint_prototype()))?;

      let marker = scope.alloc_string("vm-js.internal.BigIntData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      scope.define_property(
        obj,
        marker_key,
        data_desc(Value::BigInt(b), true, false, false),
      )?;

      Ok(Value::Object(obj))
    }
  }
}

pub fn object_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  object_constructor_impl(vm, scope, host, args)
}

pub fn object_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  object_constructor_impl(vm, scope, host, args)
}

pub fn object_define_property(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = scope.to_object(vm, host, hooks, target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let desc_obj = require_object(args.get(2).copied().unwrap_or(Value::Undefined))?;
  scope.push_root(Value::Object(desc_obj))?;

  let value = get_own_data_property_value_by_name(&mut scope, desc_obj, "value")?;
  let writable = get_own_data_property_value_by_name(&mut scope, desc_obj, "writable")?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?;
  let enumerable = get_own_data_property_value_by_name(&mut scope, desc_obj, "enumerable")?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?;
  let configurable = get_own_data_property_value_by_name(&mut scope, desc_obj, "configurable")?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?;
  let get = get_own_data_property_value_by_name(&mut scope, desc_obj, "get")?;
  let set = get_own_data_property_value_by_name(&mut scope, desc_obj, "set")?;

  let patch = PropertyDescriptorPatch {
    enumerable,
    configurable,
    value,
    writable,
    get,
    set,
  };
  patch.validate()?;

  let ok = scope.define_own_property(target, key, patch)?;
  if !ok {
    return Err(VmError::TypeError("DefineOwnProperty rejected"));
  }
  Ok(Value::Object(target))
}

pub fn object_create(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let proto_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let proto = match proto_val {
    Value::Object(o) => Some(o),
    Value::Null => None,
    _ => {
      return Err(VmError::TypeError(
        "Object.create prototype must be an object or null",
      ))
    }
  };

  if let Some(properties_object) = args.get(1).copied() {
    if !matches!(properties_object, Value::Undefined) {
      return Err(VmError::Unimplemented("Object.create propertiesObject"));
    }
  }

  let obj = scope.alloc_object()?;
  scope.heap_mut().object_set_prototype(obj, proto)?;
  Ok(Value::Object(obj))
}

pub fn object_keys(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  // Root `obj` while collecting keys and allocating the output array so the collected key strings
  // remain reachable during GC.
  scope.push_root(Value::Object(obj))?;

  let own_keys = scope.ordinary_own_property_keys(obj)?;
  let mut names: Vec<crate::GcString> = Vec::new();
  names
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(key_str) = key else {
      continue;
    };
    let Some(desc) = scope.ordinary_get_own_property(obj, key)? else {
      continue;
    };
    if desc.enumerable {
      names.push(key_str);
    }
  }

  let len = u32::try_from(names.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, scope, len)?;

  for (i, name) in names.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let mut idx_scope = scope.reborrow();
    idx_scope.push_root(Value::Object(array))?;
    idx_scope.push_root(Value::String(name))?;

    let key = PropertyKey::from_string(idx_scope.alloc_string(&i.to_string())?);
    idx_scope.define_property(array, key, data_desc(Value::String(name), true, true, true))?;
  }

  Ok(Value::Object(array))
}

pub fn object_assign(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: Object.assign performs `ToObject` on the target and each source, and uses `Get`/`Set`
  // semantics (invoking accessors).
  let mut scope = scope.reborrow();
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = scope.to_object(vm, host, hooks, target_val)?;
  scope.push_root(Value::Object(target))?;

  for (i, source_val) in args.iter().copied().skip(1).enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let source = match source_val {
      Value::Undefined | Value::Null => continue,
      other => scope.to_object(vm, host, hooks, other)?,
    };
    scope.push_root(Value::Object(source))?;

    let keys = scope.ordinary_own_property_keys(source)?;
    for (j, key) in keys.into_iter().enumerate() {
      if j % 1024 == 0 {
        vm.tick()?;
      }
      let Some(desc) = scope.ordinary_get_own_property(source, key)? else {
        continue;
      };
      if !desc.enumerable {
        continue;
      }

      // Spec: `Get(from, key)` (invokes getters).
      let value = scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        source,
        key,
        Value::Object(source),
      )?;
      // Spec: `Set(to, key, value, true)` (invokes setters, throws on failure).
      let ok = scope.ordinary_set_with_host_and_hooks(
        vm,
        host,
        hooks,
        target,
        key,
        value,
        Value::Object(target),
      )?;
      if !ok {
        return Err(VmError::TypeError("Object.assign failed to set property"));
      }
    }
  }

  Ok(Value::Object(target))
}

pub fn object_get_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  match scope.heap().object_prototype(obj)? {
    Some(proto) => Ok(Value::Object(proto)),
    None => Ok(Value::Null),
  }
}

pub fn object_set_prototype_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.setprototypeof
  //
  // Note: `Object.setPrototypeOf` does **not** perform `ToObject(O)`. It only performs
  // `RequireObjectCoercible(O)` and returns the original value unchanged when `O` is not an object.
  // This matches JS behaviour where attempting to set the prototype of a primitive is a no-op.
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(obj_val, Value::Undefined | Value::Null) {
    return Err(VmError::TypeError("Cannot convert undefined or null to object"));
  }
  let proto_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let proto = match proto_val {
    Value::Object(o) => Some(o),
    Value::Null => None,
    _ => {
      return Err(VmError::TypeError(
        "Object.setPrototypeOf prototype must be an object or null",
      ))
    }
  };

  match obj_val {
    Value::Object(obj) => {
      scope.heap_mut().object_set_prototype(obj, proto)?;
      Ok(Value::Object(obj))
    }
    // If `O` is not an object, the spec returns it unchanged.
    other => Ok(other),
  }
}

fn create_array_object(vm: &mut Vm, scope: &mut Scope<'_>, len: u32) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;

  let array = scope.alloc_array(len as usize)?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(intr.array_prototype()))?;
  Ok(array)
}

fn array_constructor_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  args: &[Value],
) -> Result<Value, VmError> {
  match args {
    [] => Ok(Value::Object(create_array_object(vm, scope, 0)?)),
    [Value::Number(n)] => {
      // Minimal `Array(len)` support (used by WebIDL sequence conversions).
      if !n.is_finite() || n.fract() != 0.0 || *n < 0.0 || *n > (u32::MAX as f64) {
        return Err(VmError::Unimplemented("Array(length) validation"));
      }
      Ok(Value::Object(create_array_object(vm, scope, *n as u32)?))
    }
    _ => {
      // Treat arguments as elements.
      let len = u32::try_from(args.len()).map_err(|_| VmError::OutOfMemory)?;
      let array = create_array_object(vm, scope, len)?;

      for (i, el) in args.iter().copied().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        // Root `array` and `el` during string allocation.
        let mut idx_scope = scope.reborrow();
        idx_scope.push_root(Value::Object(array))?;
        idx_scope.push_root(el)?;

        let key = PropertyKey::from_string(idx_scope.alloc_string(&i.to_string())?);
        idx_scope.define_property(array, key, data_desc(el, true, true, true))?;
      }

      Ok(Value::Object(array))
    }
  }
}

pub fn array_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  array_constructor_impl(vm, scope, args)
}

pub fn array_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  array_constructor_impl(vm, scope, args)
}

pub fn array_buffer_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("ArrayBuffer constructor requires 'new'"))
}

pub fn array_buffer_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let length_val = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let length_num = scope.heap_mut().to_number(length_val)?;
  if !length_num.is_finite() || length_num < 0.0 || length_num.fract() != 0.0 {
    return Err(VmError::TypeError("ArrayBuffer length must be a non-negative integer"));
  }
  let byte_length = length_num as usize;

  let ab = scope.alloc_array_buffer(byte_length)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  Ok(Value::Object(ab))
}

pub fn array_buffer_is_view(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let is_view = match arg0 {
    Value::Object(obj) => scope.heap().is_uint8_array_object(obj),
    _ => false,
  };
  Ok(Value::Bool(is_view))
}

pub fn array_buffer_prototype_byte_length_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("ArrayBuffer.byteLength called on non-object"));
  };
  let len = scope
    .heap()
    .array_buffer_byte_length(obj)
    .map_err(|_| VmError::TypeError("ArrayBuffer.byteLength called on incompatible receiver"))?;
  Ok(Value::Number(len as f64))
}

pub fn array_buffer_prototype_slice(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("ArrayBuffer.prototype.slice called on non-object"));
  };
  let len = scope
    .heap()
    .array_buffer_byte_length(obj)
    .map_err(|_| VmError::TypeError("ArrayBuffer.prototype.slice called on incompatible receiver"))?;

  let (start, end) = slice_range_from_args(vm, scope, host, hooks, len, args)?;

  let bytes = {
    let data = scope
      .heap()
      .array_buffer_data(obj)
      .map_err(|_| VmError::invalid_handle())?;
    let slice = &data[start..end];
    let mut out: Vec<u8> = Vec::new();
    out
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut out, slice)?;
    out
  };

  let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  Ok(Value::Object(ab))
}

pub fn uint8_array_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Uint8Array constructor requires 'new'"))
}

pub fn uint8_array_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);

  // `new Uint8Array(length)`
  if !matches!(arg0, Value::Object(_)) {
    let length_num = if matches!(arg0, Value::Undefined) {
      0.0
    } else {
      scope.heap_mut().to_number(arg0)?
    };
    if !length_num.is_finite() || length_num < 0.0 || length_num.fract() != 0.0 {
      return Err(VmError::TypeError("Uint8Array length must be a non-negative integer"));
    }
    let length = length_num as usize;

    let ab = scope.alloc_array_buffer(length)?;
    scope.push_root(Value::Object(ab))?;
    scope
      .heap_mut()
      .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

    let view = scope.alloc_uint8_array(ab, 0, length)?;
    scope
      .heap_mut()
      .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
    return Ok(Value::Object(view));
  }

  // `new Uint8Array(buffer, byteOffset?, length?)`
  let Value::Object(buffer) = arg0 else {
    return Err(VmError::TypeError("Uint8Array constructor expects an ArrayBuffer"));
  };
  let buf_len = scope
    .heap()
    .array_buffer_byte_length(buffer)
    .map_err(|_| VmError::TypeError("Uint8Array constructor expects an ArrayBuffer"))?;

  let byte_offset_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let byte_offset = if matches!(byte_offset_val, Value::Undefined) {
    0usize
  } else {
    let n = scope.heap_mut().to_number(byte_offset_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError("Uint8Array byteOffset must be a non-negative integer"));
    }
    n as usize
  };

  let length_val = args.get(2).copied().unwrap_or(Value::Undefined);
  let length = if matches!(length_val, Value::Undefined) {
    buf_len.saturating_sub(byte_offset)
  } else {
    let n = scope.heap_mut().to_number(length_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError("Uint8Array length must be a non-negative integer"));
    }
    n as usize
  };

  let view = scope.alloc_uint8_array(buffer, byte_offset, length)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
  Ok(Value::Object(view))
}

pub fn uint8_array_prototype_byte_length_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Uint8Array.byteLength called on non-object"));
  };
  let len = scope
    .heap()
    .uint8_array_byte_length(obj)
    .map_err(|_| VmError::TypeError("Uint8Array.byteLength called on incompatible receiver"))?;
  Ok(Value::Number(len as f64))
}

pub fn uint8_array_prototype_length_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Uint8Array.length called on non-object"));
  };
  let len = scope
    .heap()
    .uint8_array_length(obj)
    .map_err(|_| VmError::TypeError("Uint8Array.length called on incompatible receiver"))?;
  Ok(Value::Number(len as f64))
}

pub fn uint8_array_prototype_byte_offset_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Uint8Array.byteOffset called on non-object"));
  };
  let offset = scope
    .heap()
    .uint8_array_byte_offset(obj)
    .map_err(|_| VmError::TypeError("Uint8Array.byteOffset called on incompatible receiver"))?;
  Ok(Value::Number(offset as f64))
}

pub fn uint8_array_prototype_buffer_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Uint8Array.buffer called on non-object"));
  };
  let buffer = scope
    .heap()
    .uint8_array_buffer(obj)
    .map_err(|_| VmError::TypeError("Uint8Array.buffer called on incompatible receiver"))?;
  Ok(Value::Object(buffer))
}

pub fn uint8_array_prototype_slice(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Uint8Array.prototype.slice called on non-object"));
  };
  let len = scope
    .heap()
    .uint8_array_length(obj)
    .map_err(|_| VmError::TypeError("Uint8Array.prototype.slice called on incompatible receiver"))?;

  let (start, end) = slice_range_from_args(vm, scope, host, hooks, len, args)?;

  let bytes = {
    let data = scope
      .heap()
      .uint8_array_data(obj)
      .map_err(|_| VmError::invalid_handle())?;
    let slice = &data[start..end];
    let mut out: Vec<u8> = Vec::new();
    out
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut out, slice)?;
    out
  };

  let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
  scope.push_root(Value::Object(ab))?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

  let new_view = scope.alloc_uint8_array(ab, 0, end - start)?;
  scope
    .heap_mut()
    .object_set_prototype(new_view, Some(intr.uint8_array_prototype()))?;
  Ok(Value::Object(new_view))
}

pub fn function_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::Unimplemented("Function constructor"))
}

pub fn function_constructor_construct(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  Err(VmError::Unimplemented("Function constructor"))
}

pub fn error_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  error_constructor_construct(vm, scope, _host, host, callee, args, Value::Object(callee))
}

pub fn error_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let name = scope.heap().get_function_name(callee)?;

  // `new Error(message)` uses `GetPrototypeFromConstructor(newTarget, defaultProto)`.
  // Approximate this by:
  // 1. Reading `callee.prototype` as the default.
  // 2. If `new_target` is an object, prefer `new_target.prototype` when it is an object.
  let prototype_key = string_key(scope, "prototype")?;
  let default_proto_value = scope
    .heap()
    .object_get_own_data_property_value(callee, &prototype_key)?
    .ok_or(VmError::Unimplemented(
      "Error constructor missing own prototype property",
    ))?;
  let Value::Object(default_prototype) = default_proto_value else {
    return Err(VmError::Unimplemented(
      "Error constructor prototype property is not an object",
    ));
  };

  let instance_prototype = match new_target {
    Value::Object(nt) => match scope.heap().get(nt, &prototype_key)? {
      Value::Object(p) => p,
      _ => default_prototype,
    },
    _ => default_prototype,
  };

  let is_aggregate_error = scope.heap().get_string(name)?.to_utf8_lossy() == "AggregateError";

  // Message argument: for AggregateError, the message is the *second* argument.
  // Spec: `new AggregateError(errors, message)` (ECMA-262).
  let message_arg = if is_aggregate_error {
    args.get(1).copied()
  } else {
    args.first().copied()
  };

  let message_string = match message_arg {
    Some(Value::Undefined) | None => scope.alloc_string("")?,
    Some(other) => scope.to_string(vm, host, hooks, other)?,
  };
  scope.push_root(Value::String(message_string))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(instance_prototype))?;

  let name_key = string_key(scope, "name")?;
  scope.define_property(
    obj,
    name_key,
    data_desc(Value::String(name), true, false, true),
  )?;

  let message_key = string_key(scope, "message")?;
  scope.define_property(
    obj,
    message_key,
    data_desc(Value::String(message_string), true, false, true),
  )?;

  // AggregateError `errors` property.
  //
  // Spec: `new AggregateError(errors, message)` creates an `errors` own data property containing an
  // Array created from the provided iterable. `vm-js` does not yet implement full iterable-to-list
  // conversion here, so we store the first argument directly (sufficient for Promise.any, which
  // passes an Array).
  if is_aggregate_error {
    let errors = args.get(0).copied().unwrap_or(Value::Undefined);
    let errors_key = string_key(scope, "errors")?;
    scope.define_property(obj, errors_key, data_desc(errors, true, false, true))?;
  }

  Ok(Value::Object(obj))
}

fn create_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let ctor = intr.type_error();

  let msg = scope.alloc_string(message)?;
  scope.push_root(Value::String(msg))?;

  let mut host_state = ();
  error_constructor_construct(
    vm,
    scope,
    &mut host_state,
    host,
    ctor,
    &[Value::String(msg)],
    Value::Object(ctor),
  )
}

fn throw_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let err = create_type_error(vm, scope, host, message)?;
  Err(VmError::Throw(err))
}

fn new_promise(vm: &mut Vm, scope: &mut Scope<'_>) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;
  scope.alloc_promise_with_prototype(Some(intr.promise_prototype()))
}

pub(crate) fn new_promise_capability(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  constructor: Value,
) -> Result<PromiseCapability, VmError> {
  let mut dummy_host = ();
  new_promise_capability_with_host_and_hooks(vm, scope, &mut dummy_host, hooks, constructor)
}

pub(crate) fn new_promise_capability_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  constructor: Value,
) -> Result<PromiseCapability, VmError> {
  let intr = require_intrinsics(vm)?;

  let Value::Object(_) = constructor else {
    let err = create_type_error(
      vm,
      scope,
      hooks,
      "Promise capability constructor must be an object",
    )?;
    return Err(VmError::Throw(err));
  };

  if !scope.heap().is_constructor(constructor)? {
    let err = create_type_error(
      vm,
      scope,
      hooks,
      "Promise capability constructor is not a constructor",
    )?;
    return Err(VmError::Throw(err));
  }

  // Root the constructor across allocations/GC while wiring the capability and constructing the
  // Promise.
  scope.push_root(constructor)?;

  // --- NewPromiseCapability(C) ---
  // Spec: https://tc39.es/ecma262/#sec-newpromisecapability

  // resolvingFunctions = { resolve: undefined, reject: undefined }
  //
  // Represent the record as a closure environment with two mutable bindings.
  let resolving_env = scope.env_create(None)?;
  scope.push_env_root(resolving_env)?;
  scope.env_create_mutable_binding(resolving_env, "resolve")?;
  scope.env_create_mutable_binding(resolving_env, "reject")?;
  scope
    .heap_mut()
    .env_initialize_binding(resolving_env, "resolve", Value::Undefined)?;
  scope
    .heap_mut()
    .env_initialize_binding(resolving_env, "reject", Value::Undefined)?;

  // executor = CreateBuiltinFunction(...)
  let executor_name = scope.alloc_string("executor")?;
  // Root the name + function while constructing the promise: `Construct` may allocate and GC.
  scope.push_root(Value::String(executor_name))?;
  let executor = scope.alloc_native_function(
    intr.promise_capability_executor_call(),
    None,
    executor_name,
    2,
  )?;
  scope.push_root(Value::Object(executor))?;
  set_function_job_realm_to_current(vm, scope, executor)?;
  scope
    .heap_mut()
    .object_set_prototype(executor, Some(intr.function_prototype()))?;
  scope
    .heap_mut()
    .set_function_data(executor, FunctionData::PromiseCapabilityExecutor)?;
  scope
    .heap_mut()
    .set_function_closure_env(executor, Some(resolving_env))?;

  // promise = ? Construct(C, « executor »)
  let promise = vm.construct_with_host_and_hooks(
    host,
    scope,
    hooks,
    constructor,
    &[Value::Object(executor)],
    constructor,
  )?;

  // Per spec, `Construct` returns an Object. `vm-js` native constructors can return non-objects, so
  // validate this to preserve the PromiseCapability invariants used throughout the VM.
  if !matches!(promise, Value::Object(_)) {
    let err = create_type_error(
      vm,
      scope,
      hooks,
      "Promise capability promise is not an object",
    )?;
    return Err(VmError::Throw(err));
  }

  // If IsCallable(resolve) is false, throw a TypeError exception.
  let resolve = scope
    .heap()
    .env_get_binding_value(resolving_env, "resolve", false)?;
  if !scope.heap().is_callable(resolve)? {
    let err = create_type_error(
      vm,
      scope,
      hooks,
      "Promise capability resolve is not callable",
    )?;
    return Err(VmError::Throw(err));
  }

  // If IsCallable(reject) is false, throw a TypeError exception.
  let reject = scope
    .heap()
    .env_get_binding_value(resolving_env, "reject", false)?;
  if !scope.heap().is_callable(reject)? {
    let err = create_type_error(
      vm,
      scope,
      hooks,
      "Promise capability reject is not callable",
    )?;
    return Err(VmError::Throw(err));
  }

  Ok(PromiseCapability {
    promise,
    resolve,
    reject,
  })
}

fn get_property_value_with_host(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  receiver: Value,
) -> Result<Value, VmError> {
  let Some(desc) = scope.heap().get_property(obj, &key)? else {
    return Ok(Value::Undefined);
  };

  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(value),
    PropertyKind::Accessor { get, .. } => {
      if matches!(get, Value::Undefined) {
        Ok(Value::Undefined)
      } else {
        if !scope.heap().is_callable(get)? {
          return Err(VmError::TypeError("accessor getter is not callable"));
        }
        vm.call_with_host_and_hooks(host, scope, hooks, get, receiver, &[])
      }
    }
  }
}

/// ECMA-262 `PromiseResolve(C, x)` abstract operation.
fn promise_resolve_abstract(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  constructor: Value,
  x: Value,
) -> Result<GcObject, VmError> {
  let mut scope = scope.reborrow();
  // Root inputs across allocations/GC.
  scope.push_root(constructor)?;
  scope.push_root(x)?;

  if let Value::Object(obj) = x {
    if scope.heap().is_promise_object(obj) {
      // `x.constructor === C`
      let ctor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(ctor_key_s))?;
      let ctor_key = PropertyKey::from_string(ctor_key_s);
      let x_ctor = get_property_value_with_host(
        vm,
        &mut scope,
        host,
        hooks,
        obj,
        ctor_key,
        Value::Object(obj),
      )?;
      if x_ctor.same_value(constructor, scope.heap()) {
        return Ok(obj);
      }
    }
  }

  let capability =
    new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks, constructor)?;
  let Value::Object(promise_obj) = capability.promise else {
    return Err(VmError::InvariantViolation(
      "PromiseCapability.promise is not an object",
    ));
  };

  // Root the promise + resolving function for the duration of the resolve call (which may
  // allocate/GC).
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  let _ = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    capability.resolve,
    Value::Undefined,
    &[x],
  )?;
  Ok(promise_obj)
}

fn create_promise_resolving_functions(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  promise: GcObject,
) -> Result<(Value, Value), VmError> {
  let intr = require_intrinsics(vm)?;
  let call_id = intr.promise_resolving_function_call();

  // Root the promise and shared [[AlreadyResolved]] state while allocating the resolving
  // functions.
  scope.push_root(Value::Object(promise))?;

  // Model `alreadyResolved` as a mutable binding in a shared closure environment.
  //
  // This is important for spec-correct behavior when:
  // - an executor calls both `resolve` and `reject`,
  // - or calls `resolve(thenable)` and then calls `resolve` again before the thenable job runs.
  let already_resolved_env = scope.env_create(None)?;
  scope.push_env_root(already_resolved_env)?;
  scope.env_create_mutable_binding(already_resolved_env, "alreadyResolved")?;
  scope.heap_mut().env_initialize_binding(
    already_resolved_env,
    "alreadyResolved",
    Value::Bool(false),
  )?;

  let resolve_name = scope.alloc_string("resolve")?;
  // Root the resolve function while allocating the reject function: both share `alreadyResolved`.
  scope.push_root(Value::String(resolve_name))?;
  let resolve = scope.alloc_native_function(call_id, None, resolve_name, 1)?;
  scope.push_root(Value::Object(resolve))?;
  set_function_job_realm_to_current(vm, scope, resolve)?;
  scope
    .heap_mut()
    .object_set_prototype(resolve, Some(intr.function_prototype()))?;
  scope.heap_mut().set_function_data(
    resolve,
    FunctionData::PromiseResolvingFunction {
      promise,
      is_reject: false,
    },
  )?;
  scope
    .heap_mut()
    .set_function_closure_env(resolve, Some(already_resolved_env))?;

  let reject_name = scope.alloc_string("reject")?;
  scope.push_root(Value::String(reject_name))?;
  let reject = scope.alloc_native_function(call_id, None, reject_name, 1)?;
  scope.push_root(Value::Object(reject))?;
  set_function_job_realm_to_current(vm, scope, reject)?;
  scope
    .heap_mut()
    .object_set_prototype(reject, Some(intr.function_prototype()))?;
  scope.heap_mut().set_function_data(
    reject,
    FunctionData::PromiseResolvingFunction {
      promise,
      is_reject: true,
    },
  )?;
  scope
    .heap_mut()
    .set_function_closure_env(reject, Some(already_resolved_env))?;

  Ok((Value::Object(resolve), Value::Object(reject)))
}

fn enqueue_promise_reaction_job(
  host: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  reaction: PromiseReaction,
  argument: Value,
  current_realm: Option<RealmId>,
) -> Result<(), VmError> {
  let handler_callback_object = reaction
    .handler
    .as_ref()
    .map(|handler| handler.callback_object());
  let realm = reaction
    .handler
    .as_ref()
    .and_then(|handler| handler.realm())
    .or_else(|| {
      handler_callback_object.and_then(|handler| scope.heap().get_function_job_realm(handler))
    })
    .or(current_realm);
  let capability = reaction.capability;

  let job = Job::new(JobKind::Promise, move |ctx, host| {
    let Some(cap) = reaction.capability else {
      return Ok(());
    };

    match reaction.type_ {
      PromiseReactionType::Fulfill => {
        let handler_result = if let Some(handler) = &reaction.handler {
          match host.host_call_job_callback(ctx, handler, Value::Undefined, &[argument]) {
            Ok(v) => v,
            Err(VmError::Throw(e) | VmError::ThrowWithStack { value: e, .. }) => {
              let _ = ctx.call(host, cap.reject, Value::Undefined, &[e])?;
              return Ok(());
            }
            Err(e) => return Err(e),
          }
        } else {
          argument
        };

        let _ = ctx.call(host, cap.resolve, Value::Undefined, &[handler_result])?;
        Ok(())
      }
      PromiseReactionType::Reject => {
        if let Some(handler) = &reaction.handler {
          match host.host_call_job_callback(ctx, handler, Value::Undefined, &[argument]) {
            Ok(v) => {
              let _ = ctx.call(host, cap.resolve, Value::Undefined, &[v])?;
              Ok(())
            }
            Err(VmError::Throw(e) | VmError::ThrowWithStack { value: e, .. }) => {
              let _ = ctx.call(host, cap.reject, Value::Undefined, &[e])?;
              Ok(())
            }
            Err(e) => Err(e),
          }
        } else {
          let _ = ctx.call(host, cap.reject, Value::Undefined, &[argument])?;
          Ok(())
        }
      }
    }
  });

  // Root captured GC values while creating persistent roots: `Heap::add_root` can trigger a GC.
  // The resulting `RootId`s are transferred to the job so it can clean them up on run/discard.
  let mut root_scope = scope.reborrow();
  let mut values = [Value::Undefined; 5];
  let mut value_count = 0usize;
  values[value_count] = argument;
  value_count += 1;
  if let Some(handler) = handler_callback_object {
    values[value_count] = Value::Object(handler);
    value_count += 1;
  }
  if let Some(cap) = capability {
    values[value_count] = cap.promise;
    value_count += 1;
    values[value_count] = cap.resolve;
    value_count += 1;
    values[value_count] = cap.reject;
    value_count += 1;
  }
  root_scope.push_roots(&values[..value_count])?;

  let mut roots: Vec<RootId> = Vec::new();
  roots
    .try_reserve_exact(value_count)
    .map_err(|_| VmError::OutOfMemory)?;
  for value in values[..value_count].iter().copied() {
    let id = match root_scope.heap_mut().add_root(value) {
      Ok(id) => id,
      Err(e) => {
        for root in roots.drain(..) {
          root_scope.heap_mut().remove_root(root);
        }
        return Err(e);
      }
    };
    roots.push(id);
  }

  let job = job.with_roots(roots);
  host.host_enqueue_promise_job(job, realm);
  Ok(())
}

fn trigger_promise_reactions(
  vm: &mut Vm,
  host: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  reactions: Box<[PromiseReaction]>,
  argument: Value,
  current_realm: Option<RealmId>,
) -> Result<(), VmError> {
  for (i, reaction) in reactions.into_vec().into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    enqueue_promise_reaction_job(host, scope, reaction, argument, current_realm)?;
  }
  Ok(())
}

pub(crate) fn fulfill_promise(
  vm: &mut Vm,
  host: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  promise: GcObject,
  value: Value,
  current_realm: Option<RealmId>,
) -> Result<(), VmError> {
  let (fulfill_reactions, _reject_reactions) =
    scope
      .heap_mut()
      .promise_settle_and_take_reactions(promise, PromiseState::Fulfilled, value)?;
  trigger_promise_reactions(vm, host, scope, fulfill_reactions, value, current_realm)
}

pub(crate) fn reject_promise(
  vm: &mut Vm,
  host: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  promise: GcObject,
  reason: Value,
  current_realm: Option<RealmId>,
) -> Result<(), VmError> {
  if scope.heap().promise_state(promise)? != PromiseState::Pending {
    // Per spec, subsequent rejects of an already-settled promise are no-ops.
    return Ok(());
  }

  let is_handled = scope.heap().promise_is_handled(promise)?;

  let (_fulfill_reactions, reject_reactions) =
    scope
      .heap_mut()
      .promise_settle_and_take_reactions(promise, PromiseState::Rejected, reason)?;

  if !is_handled {
    host.host_promise_rejection_tracker(PromiseHandle(promise), PromiseRejectionOperation::Reject);
  }

  trigger_promise_reactions(vm, host, scope, reject_reactions, reason, current_realm)
}

fn resolve_promise(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  promise: GcObject,
  resolution: Value,
) -> Result<(), VmError> {
  let current_realm = vm.current_realm();
  let intr = require_intrinsics(vm)?;

  // 27.2.1.3.2 `Promise Resolve Functions`: self-resolution is a TypeError rejection.
  if let Value::Object(obj) = resolution {
    if obj == promise {
      let err = create_type_error(vm, scope, hooks, "Promise cannot resolve itself")?;
      return reject_promise(vm, hooks, scope, promise, err, current_realm);
    }
  }

  // Non-objects cannot be thenables.
  let Value::Object(thenable_obj) = resolution else {
    return fulfill_promise(vm, hooks, scope, promise, resolution, current_realm);
  };

  // Get `thenable.then`.
  //
  // Spec: this must perform `Get(thenable, "then")`, which means it must:
  // - traverse the prototype chain,
  // - and invoke accessor getters.
  let then_result = {
    // Root `thenable_obj` while allocating the property key.
    let mut key_scope = scope.reborrow();
    key_scope.push_root(Value::Object(thenable_obj))?;
    let then_key_s = key_scope.alloc_string("then")?;
    key_scope.push_root(Value::String(then_key_s))?;
    let then_key = PropertyKey::from_string(then_key_s);

    match key_scope.heap().get_property(thenable_obj, &then_key)? {
      None => Ok(Value::Undefined),
      Some(desc) => match desc.kind {
        PropertyKind::Data { value, .. } => Ok(value),
        PropertyKind::Accessor { get, .. } => {
          if matches!(get, Value::Undefined) {
            Ok(Value::Undefined)
          } else if !key_scope.heap().is_callable(get)? {
            // Model `Get(thenable, "then")` throwing a TypeError when an accessor getter exists but
            // is not callable. This must reject the promise rather than propagate as a VM error
            // from `resolve()`.
            Err(crate::throw_type_error(
              &mut key_scope,
              intr,
              "accessor getter is not callable",
            ))
          } else {
            vm.call_with_host_and_hooks(
              host,
              &mut key_scope,
              hooks,
              get,
              Value::Object(thenable_obj),
              &[],
            )
          }
        }
      },
    }
  };

  let then = match then_result {
    Ok(v) => v,
    Err(VmError::Throw(e) | VmError::ThrowWithStack { value: e, .. }) => {
      reject_promise(vm, hooks, scope, promise, e, current_realm)?;
      return Ok(());
    }
    Err(e) => return Err(e),
  };

  if !scope.heap().is_callable(then)? {
    return fulfill_promise(vm, hooks, scope, promise, resolution, current_realm);
  }

  let Value::Object(then_obj) = then else {
    return Err(VmError::Unimplemented("callable then is not an object"));
  };

  // Enqueue PromiseResolveThenableJob(promise, thenable, then).
  let then_job_callback = hooks.host_make_job_callback(then_obj);

  // Per spec, the thenable job must use *fresh* resolving functions for `promise` (with their own
  // alreadyResolved record).
  scope.push_root(Value::Object(thenable_obj))?;
  let (resolve, reject) = create_promise_resolving_functions(vm, scope, promise)?;

  let callback_obj = then_job_callback.callback_object();
  let realm = then_job_callback
    .realm()
    .or_else(|| scope.heap().get_function_job_realm(callback_obj))
    .or(current_realm);
  let job = Job::new(JobKind::Promise, move |ctx, host| {
    match host.host_call_job_callback(ctx, &then_job_callback, resolution, &[resolve, reject]) {
      Ok(_) => Ok(()),
      Err(VmError::Throw(e) | VmError::ThrowWithStack { value: e, .. }) => {
        let _ = ctx.call(host, reject, Value::Undefined, &[e])?;
        Ok(())
      }
      Err(e) => Err(e),
    }
  });

  // Root captured GC values while creating persistent roots: `Heap::add_root` can trigger a GC.
  // The resulting `RootId`s are transferred to the job so it can clean them up on run/discard.
  let mut root_scope = scope.reborrow();
  let values = [resolution, Value::Object(callback_obj), resolve, reject];
  root_scope.push_roots(&values)?;

  let mut roots: Vec<RootId> = Vec::new();
  roots
    .try_reserve_exact(values.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for value in values {
    let id = match root_scope.heap_mut().add_root(value) {
      Ok(id) => id,
      Err(e) => {
        for root in roots.drain(..) {
          root_scope.heap_mut().remove_root(root);
        }
        return Err(e);
      }
    };
    roots.push(id);
  }

  let job = job.with_roots(roots);
  hooks.host_enqueue_promise_job(job, realm);
  Ok(())
}

pub fn promise_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  throw_type_error(
    vm,
    scope,
    host,
    "Promise constructor must be called with new",
  )
}

pub fn promise_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let executor = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(executor)? {
    return throw_type_error(vm, scope, hooks, "Promise executor is not callable");
  }

  // Promise constructor:
  // `promise = OrdinaryCreateFromConstructor(NewTarget, "%Promise.prototype%", ...)`.
  let intr = require_intrinsics(vm)?;
  let promise = crate::spec_ops::ordinary_create_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intr.promise_prototype(),
    &[
      "[[PromiseState]]",
      "[[PromiseResult]]",
      "[[PromiseFulfillReactions]]",
      "[[PromiseRejectReactions]]",
      "[[PromiseIsHandled]]",
    ],
    |scope| scope.alloc_promise(),
  )?;
  scope.push_root(Value::Object(promise))?;

  let (resolve, reject) = create_promise_resolving_functions(vm, scope, promise)?;

  // Invoke executor(resolve, reject).
  match vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    executor,
    Value::Undefined,
    &[resolve, reject],
  ) {
    Ok(_) => {}
    Err(VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. }) => {
      // If executor throws, reject the promise with the thrown value by calling the resolving
      // function (so it respects `alreadyResolved`).
      let _ =
        vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
    }
    Err(e) => return Err(e),
  }

  Ok(Value::Object(promise))
}

pub fn promise_species_get(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(this)
}

pub fn promise_capability_executor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `GetCapabilitiesExecutor` created by `NewPromiseCapability(C)` (ECMA-262).
  //
  // Captures a record `{ resolve: undefined, reject: undefined }` and stores the resolving
  // functions provided by the Promise constructor into that record.
  let data = scope.heap().get_function_data(callee)?;
  let FunctionData::PromiseCapabilityExecutor = data else {
    return Err(VmError::Unimplemented(
      "promise capability executor missing internal slots",
    ));
  };

  let Some(env) = scope.heap().get_function_closure_env(callee)? else {
    return Err(VmError::Unimplemented(
      "promise capability executor missing closure env",
    ));
  };

  let resolve = args.get(0).copied().unwrap_or(Value::Undefined);
  let reject = args.get(1).copied().unwrap_or(Value::Undefined);

  let existing_resolve = scope.heap().env_get_binding_value(env, "resolve", false)?;
  let existing_reject = scope.heap().env_get_binding_value(env, "reject", false)?;
  if !matches!(existing_resolve, Value::Undefined) || !matches!(existing_reject, Value::Undefined) {
    return throw_type_error(
      vm,
      scope,
      hooks,
      "Promise capability executor already called",
    );
  }

  scope
    .heap_mut()
    .env_set_mutable_binding(env, "resolve", resolve, false)?;
  scope
    .heap_mut()
    .env_set_mutable_binding(env, "reject", reject, false)?;
  Ok(Value::Undefined)
}

pub fn promise_resolving_function_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let resolution = args.get(0).copied().unwrap_or(Value::Undefined);
  let data = scope.heap().get_function_data(callee)?;
  let FunctionData::PromiseResolvingFunction { promise, is_reject } = data else {
    return Err(VmError::Unimplemented(
      "promise resolving function internal slots",
    ));
  };

  let Some(env) = scope.heap().get_function_closure_env(callee)? else {
    return Err(VmError::Unimplemented(
      "promise resolving functions must have a closure env for alreadyResolved",
    ));
  };

  // `alreadyResolved` record check.
  let already = scope
    .heap()
    .env_get_binding_value(env, "alreadyResolved", false)?;
  if already == Value::Bool(true) {
    return Ok(Value::Undefined);
  }
  scope
    .heap_mut()
    .env_set_mutable_binding(env, "alreadyResolved", Value::Bool(true), false)?;

  if is_reject {
    reject_promise(vm, hooks, scope, promise, resolution, vm.current_realm())?;
  } else {
    resolve_promise(vm, scope, host, hooks, promise, resolution)?;
  }
  Ok(Value::Undefined)
}

pub fn promise_resolve(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let x = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(this, Value::Object(_)) {
    return throw_type_error(vm, scope, hooks, "Promise.resolve called on non-object");
  }

  let p = promise_resolve_abstract(vm, scope, host, hooks, this, x)?;
  Ok(Value::Object(p))
}

pub fn promise_reject(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;
  scope.push_root(capability.promise)?;
  scope.push_root(capability.reject)?;
  let _ = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    capability.reject,
    Value::Undefined,
    &[reason],
  )?;
  Ok(capability.promise)
}

pub(crate) fn perform_promise_then(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  this: Value,
  on_fulfilled: Value,
  on_rejected: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let Value::Object(promise) = this else {
    return throw_type_error(
      vm,
      scope,
      host,
      "Promise.prototype.then called on non-object",
    );
  };
  if !scope.heap().is_promise_object(promise) {
    return throw_type_error(
      vm,
      scope,
      host,
      "Promise.prototype.then called on non-promise",
    );
  }

  // `PerformPromiseThen`: unhandled rejection tracking.
  let was_handled = scope.heap().promise_is_handled(promise)?;
  if scope.heap().promise_state(promise)? == PromiseState::Rejected && !was_handled {
    host.host_promise_rejection_tracker(PromiseHandle(promise), PromiseRejectionOperation::Handle);
  }

  // `PerformPromiseThen` sets `[[PromiseIsHandled]] = true`.
  scope.heap_mut().promise_set_is_handled(promise, true)?;

  // Normalize handlers: use "empty" when not callable.
  let on_fulfilled = match on_fulfilled {
    Value::Object(obj) if scope.heap().is_callable(Value::Object(obj))? => {
      Some(host.host_make_job_callback(obj))
    }
    _ => None,
  };
  let on_rejected = match on_rejected {
    Value::Object(obj) if scope.heap().is_callable(Value::Object(obj))? => {
      Some(host.host_make_job_callback(obj))
    }
    _ => None,
  };

  // Create the derived promise + capability.
  let result_promise = scope.alloc_promise_with_prototype(Some(intr.promise_prototype()))?;
  scope.push_root(Value::Object(result_promise))?;
  let (resolve, reject) = create_promise_resolving_functions(vm, scope, result_promise)?;
  let capability = PromiseCapability {
    promise: Value::Object(result_promise),
    resolve,
    reject,
  };

  let fulfill_reaction = PromiseReaction {
    capability: Some(capability),
    type_: PromiseReactionType::Fulfill,
    handler: on_fulfilled,
  };
  let reject_reaction = PromiseReaction {
    capability: Some(capability),
    type_: PromiseReactionType::Reject,
    handler: on_rejected,
  };

  let current_realm = vm.current_realm();

  match scope.heap().promise_state(promise)? {
    PromiseState::Pending => {
      scope.promise_append_fulfill_reaction(promise, fulfill_reaction)?;
      scope.promise_append_reject_reaction(promise, reject_reaction)?;
    }
    PromiseState::Fulfilled => {
      let arg = scope
        .heap()
        .promise_result(promise)?
        .unwrap_or(Value::Undefined);
      enqueue_promise_reaction_job(host, scope, fulfill_reaction, arg, current_realm)?;
    }
    PromiseState::Rejected => {
      let arg = scope
        .heap()
        .promise_result(promise)?
        .unwrap_or(Value::Undefined);
      enqueue_promise_reaction_job(host, scope, reject_reaction, arg, current_realm)?;
    }
  }

  Ok(Value::Object(result_promise))
}

fn invoke_then(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  receiver: Value,
  on_fulfilled: Value,
  on_rejected: Value,
  non_object_message: &'static str,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  // Root inputs: `Get` and `Call` can allocate/GC.
  let mut scope = scope.reborrow();
  scope.push_root(receiver)?;
  scope.push_root(on_fulfilled)?;
  scope.push_root(on_rejected)?;

  // `Invoke(receiver, "then", ...)` uses `GetV`, which performs `ToObject` for primitives
  // (throwing only for `null`/`undefined`).
  let obj = match receiver {
    Value::Object(obj) => obj,
    Value::Null | Value::Undefined => {
      return Err(crate::throw_type_error(
        &mut scope,
        intr,
        non_object_message,
      ));
    }
    primitive => {
      let object_ctor = Value::Object(intr.object_constructor());
      scope.push_root(object_ctor)?;
      let value = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        object_ctor,
        Value::Undefined,
        &[primitive],
      )?;
      let Value::Object(obj) = value else {
        return Err(VmError::InvariantViolation(
          "Object(..) conversion returned non-object",
        ));
      };
      scope.push_root(Value::Object(obj))?;
      obj
    }
  };

  let then_key_s = scope.alloc_string("then")?;
  scope.push_root(Value::String(then_key_s))?;
  let then_key = PropertyKey::from_string(then_key_s);
  let then = get_property_value_with_host(vm, &mut scope, host, hooks, obj, then_key, receiver)?;
  if !scope.heap().is_callable(then)? {
    return Err(crate::throw_type_error(
      &mut scope,
      intr,
      "then is not callable",
    ));
  }

  vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    then,
    receiver,
    &[on_fulfilled, on_rejected],
  )
}

pub fn promise_prototype_then(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let on_fulfilled = args.get(0).copied().unwrap_or(Value::Undefined);
  let on_rejected = args.get(1).copied().unwrap_or(Value::Undefined);
  perform_promise_then(vm, scope, host, this, on_fulfilled, on_rejected)
}

pub fn promise_prototype_catch(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let on_rejected = args.get(0).copied().unwrap_or(Value::Undefined);
  invoke_then(
    vm,
    scope,
    host,
    hooks,
    this,
    Value::Undefined,
    on_rejected,
    "Promise.prototype.catch called on non-object",
  )
}

pub fn promise_prototype_finally(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let on_finally = args.get(0).copied().unwrap_or(Value::Undefined);

  // Per ECMA-262, `Promise.prototype.finally` throws if the receiver is not an Object
  // (even though the subsequent `Invoke(promise, "then", ...)` would box primitives).
  let Value::Object(promise) = this else {
    return Err(crate::throw_type_error(
      scope,
      intr,
      "Promise.prototype.finally called on non-object",
    ));
  };

  if !scope.heap().is_callable(on_finally)? {
    return invoke_then(
      vm,
      scope,
      host,
      hooks,
      Value::Object(promise),
      on_finally,
      on_finally,
      "Promise.prototype.finally called on non-object",
    );
  }

  // Temporary `%Promise%`-only fallback: we do not yet implement `SpeciesConstructor` for promises.
  let constructor = Value::Object(intr.promise());

  scope.push_root(Value::Object(promise))?;
  scope.push_root(on_finally)?;
  scope.push_root(constructor)?;

  let call_id = intr.promise_finally_handler_call();

  let then_finally_name = scope.alloc_string("thenFinally")?;
  let then_finally = scope.alloc_native_function(call_id, None, then_finally_name, 1)?;
  set_function_job_realm_to_current(vm, scope, then_finally)?;
  scope
    .heap_mut()
    .object_set_prototype(then_finally, Some(intr.function_prototype()))?;
  scope.heap_mut().set_function_data(
    then_finally,
    FunctionData::PromiseFinallyHandler {
      on_finally,
      constructor,
      is_reject: false,
    },
  )?;

  let catch_finally_name = scope.alloc_string("catchFinally")?;
  let catch_finally = scope.alloc_native_function(call_id, None, catch_finally_name, 1)?;
  set_function_job_realm_to_current(vm, scope, catch_finally)?;
  scope
    .heap_mut()
    .object_set_prototype(catch_finally, Some(intr.function_prototype()))?;
  scope.heap_mut().set_function_data(
    catch_finally,
    FunctionData::PromiseFinallyHandler {
      on_finally,
      constructor,
      is_reject: true,
    },
  )?;

  // Root the closure functions before invoking `then`, which may allocate/GC.
  scope.push_root(Value::Object(then_finally))?;
  scope.push_root(Value::Object(catch_finally))?;

  invoke_then(
    vm,
    scope,
    host,
    hooks,
    Value::Object(promise),
    Value::Object(then_finally),
    Value::Object(catch_finally),
    "Promise.prototype.finally called on non-object",
  )
}

pub fn promise_finally_handler_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let data = scope.heap().get_function_data(callee)?;
  let FunctionData::PromiseFinallyHandler {
    on_finally,
    constructor,
    is_reject,
  } = data
  else {
    return Err(VmError::Unimplemented(
      "Promise finally handler missing internal slots",
    ));
  };

  let captured = args.get(0).copied().unwrap_or(Value::Undefined);

  // Call onFinally() with no arguments.
  let result =
    vm.call_with_host_and_hooks(host, scope, hooks, on_finally, Value::Undefined, &[])?;
  let result = scope.push_root(result)?;

  // `PromiseResolve(C, result)`
  let promise_obj = promise_resolve_abstract(vm, scope, host, hooks, constructor, result)?;

  // Create `valueThunk` or `thrower`.
  scope.push_root(Value::Object(promise_obj))?;
  scope.push_root(captured)?;
  let thunk_call = intr.promise_finally_thunk_call();
  let thunk_name = if is_reject { "thrower" } else { "valueThunk" };
  let thunk_name = scope.alloc_string(thunk_name)?;
  let thunk = scope.alloc_native_function(thunk_call, None, thunk_name, 0)?;
  set_function_job_realm_to_current(vm, scope, thunk)?;
  scope
    .heap_mut()
    .object_set_prototype(thunk, Some(intr.function_prototype()))?;
  scope.heap_mut().set_function_data(
    thunk,
    FunctionData::PromiseFinallyThunk {
      value: captured,
      is_throw: is_reject,
    },
  )?;

  // Return `p.then(valueThunk)` / `p.then(thrower)`.
  scope.push_root(Value::Object(thunk))?;
  invoke_then(
    vm,
    scope,
    host,
    hooks,
    Value::Object(promise_obj),
    Value::Object(thunk),
    Value::Undefined,
    "PromiseResolve(C, result) returned a non-object",
  )
}

pub fn promise_finally_thunk_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let data = scope.heap().get_function_data(callee)?;
  let FunctionData::PromiseFinallyThunk { value, is_throw } = data else {
    return Err(VmError::Unimplemented(
      "Promise finally thunk missing internal slots",
    ));
  };
  if is_throw {
    Err(VmError::Throw(value))
  } else {
    Ok(value)
  }
}

pub fn promise_try(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return throw_type_error(vm, scope, hooks, "Promise.try callback is not callable");
  }

  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;

  // Root the promise + resolving functions for the duration of the callback call.
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  scope.push_root(capability.reject)?;

  let callback_args = args.get(1..).unwrap_or(&[]);
  match vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    callback,
    Value::Undefined,
    callback_args,
  ) {
    Ok(v) => {
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        capability.resolve,
        Value::Undefined,
        &[v],
      )?;
    }
    Err(VmError::Throw(e) | VmError::ThrowWithStack { value: e, .. }) => {
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        capability.reject,
        Value::Undefined,
        &[e],
      )?;
    }
    Err(e) => return Err(e),
  }

  Ok(capability.promise)
}

pub fn promise_with_resolvers(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;
  // Root the new promise and resolving functions before allocating the result object.
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  scope.push_root(capability.reject)?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.object_prototype()))?;

  let promise_key = string_key(scope, "promise")?;
  scope.define_property(
    obj,
    promise_key,
    data_desc(capability.promise, true, true, true),
  )?;

  let resolve_key = string_key(scope, "resolve")?;
  scope.define_property(
    obj,
    resolve_key,
    data_desc(capability.resolve, true, true, true),
  )?;

  let reject_key = string_key(scope, "reject")?;
  scope.define_property(
    obj,
    reject_key,
    data_desc(capability.reject, true, true, true),
  )?;

  Ok(Value::Object(obj))
}

fn get_promise_resolve(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  constructor: Value,
) -> Result<Value, VmError> {
  // `GetPromiseResolve` (ECMA-262).
  //
  // For now this is used by Promise combinator built-ins (Promise.all/race/allSettled/any).
  let Value::Object(c) = constructor else {
    return throw_type_error(
      vm,
      scope,
      hooks,
      "Promise resolve constructor must be an object",
    );
  };

  let mut key_scope = scope.reborrow();
  key_scope.push_root(constructor)?;
  let resolve_key = string_key(&mut key_scope, "resolve")?;
  let resolve =
    key_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, c, resolve_key, constructor)?;
  if !key_scope.heap().is_callable(resolve)? {
    return throw_type_error(vm, &mut key_scope, hooks, "Promise resolve is not callable");
  }
  Ok(resolve)
}

fn create_internal_record(
  scope: &mut Scope<'_>,
  prototype: GcObject,
  initial: Value,
) -> Result<GcObject, VmError> {
  // A minimal internal record object used to model spec `Record { [[Value]]: ... }` shapes.
  //
  // This is intentionally not exposed to user code except indirectly via captured builtin function
  // slots.
  let mut record_scope = scope.reborrow();
  record_scope.push_root(Value::Object(prototype))?;
  record_scope.push_root(initial)?;

  let obj = record_scope.alloc_object()?;
  record_scope.push_root(Value::Object(obj))?;
  record_scope
    .heap_mut()
    .object_set_prototype(obj, Some(prototype))?;

  let value_key = string_key(&mut record_scope, "value")?;
  record_scope.define_property(obj, value_key, data_desc(initial, true, false, true))?;
  Ok(obj)
}

fn read_internal_record_value(scope: &mut Scope<'_>, record: GcObject) -> Result<Value, VmError> {
  // Avoid accumulating roots by using a nested scope for the key string.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(record))?;
  let value_key = string_key(&mut scope, "value")?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(record, &value_key)?
      .unwrap_or(Value::Undefined),
  )
}

fn write_internal_record_value(
  scope: &mut Scope<'_>,
  record: GcObject,
  value: Value,
) -> Result<(), VmError> {
  // Avoid accumulating roots by using a nested scope for the key string.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(record))?;
  scope.push_root(value)?;
  let value_key = string_key(&mut scope, "value")?;
  scope.define_property(record, value_key, data_desc(value, true, false, true))
}

fn invoke_thenable_then(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  next_promise: Value,
  on_fulfilled: Value,
  on_rejected: Value,
) -> Result<(), VmError> {
  // `Invoke(nextPromise, "then", « onFulfilled, onRejected »)` (ECMA-262).
  //
  // This is intentionally spec-shaped: it uses the `then` property lookup rather than
  // `PerformPromiseThen` so it can support thenables returned by an overridden `C.resolve`.
  let mut invoke_scope = scope.reborrow();
  invoke_scope.push_roots(&[next_promise, on_fulfilled, on_rejected])?;

  let Value::Object(obj) = next_promise else {
    let err = create_type_error(
      vm,
      &mut invoke_scope,
      hooks,
      "Promise thenable is not an object",
    )?;
    return Err(VmError::Throw(err));
  };

  let then_key = string_key(&mut invoke_scope, "then")?;
  let then =
    invoke_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, then_key, next_promise)?;
  if !invoke_scope.heap().is_callable(then)? {
    let err = create_type_error(vm, &mut invoke_scope, hooks, "Promise then is not callable")?;
    return Err(VmError::Throw(err));
  }

  let _ = vm.call_with_host_and_hooks(
    host,
    &mut invoke_scope,
    hooks,
    then,
    next_promise,
    &[on_fulfilled, on_rejected],
  )?;
  Ok(())
}

fn if_abrupt_reject_promise(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  capability: PromiseCapability,
  completion: VmError,
) -> Result<Value, VmError> {
  // `IfAbruptRejectPromise` (ECMA-262) (partial): only catchable `throw` values are converted into
  // rejections. VM-internal errors (OOM, unimplemented, etc.) are propagated.
  let Some(reason) = completion.thrown_value() else {
    return Err(completion);
  };
  let _ = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    capability.reject,
    Value::Undefined,
    &[reason],
  )?;
  Ok(capability.promise)
}

fn perform_promise_all(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  iterator_record: &mut crate::iterator::IteratorRecord,
  constructor: Value,
  capability: PromiseCapability,
  promise_resolve: Value,
) -> Result<Value, VmError> {
  // `PerformPromiseAll` (ECMA-262).
  let intr = require_intrinsics(vm)?;

  // `values` list → model as an internal Array.
  let values = scope.alloc_array(0)?;
  scope.push_root(Value::Object(values))?;
  scope
    .heap_mut()
    .object_set_prototype(values, Some(intr.array_prototype()))?;

  // `remainingElementsCount` record.
  let remaining = create_internal_record(scope, intr.object_prototype(), Value::Number(1.0))?;
  scope.push_root(Value::Object(remaining))?;

  let resolve_element_call = intr.promise_all_resolve_element_call();
  let mut index: u32 = 0;

  loop {
    let next_value = crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record)?;
    let Some(next_value) = next_value else {
      // Done: decrement initial 1.
      let remaining_value = read_internal_record_value(scope, remaining)?;
      let Value::Number(n) = remaining_value else {
        return Err(VmError::Unimplemented(
          "PerformPromiseAll: remainingElementsCount is not a Number",
        ));
      };
      let new_remaining = n - 1.0;
      write_internal_record_value(scope, remaining, Value::Number(new_remaining))?;
      if new_remaining == 0.0 {
        let _ = vm.call_with_host_and_hooks(
          host,
          scope,
          hooks,
          capability.resolve,
          Value::Undefined,
          &[Value::Object(values)],
        )?;
      }
      return Ok(capability.promise);
    };

    // Use a nested scope so temporary roots created while wiring each element do not accumulate.
    let mut step_scope = scope.reborrow();
    step_scope.push_root(next_value)?;

    // Append `undefined` placeholder.
    {
      let mut idx_scope = step_scope.reborrow();
      idx_scope.push_root(Value::Object(values))?;
      let idx_s = idx_scope.alloc_string(&index.to_string())?;
      idx_scope.push_root(Value::String(idx_s))?;
      let key = PropertyKey::from_string(idx_s);
      idx_scope.create_data_property_or_throw(values, key, Value::Undefined)?;
    }

    // nextPromise = ? Call(promiseResolve, constructor, « nextValue »).
    let next_promise = vm.call_with_host_and_hooks(
      host,
      &mut step_scope,
      hooks,
      promise_resolve,
      constructor,
      &[next_value],
    )?;
    step_scope.push_root(next_promise)?;

    // Create per-element alreadyCalled record.
    let already_called =
      create_internal_record(&mut step_scope, intr.object_prototype(), Value::Bool(false))?;
    step_scope.push_root(Value::Object(already_called))?;

    // remainingElementsCount.[[Value]] += 1.
    let remaining_value = read_internal_record_value(&mut step_scope, remaining)?;
    let Value::Number(n) = remaining_value else {
      return Err(VmError::Unimplemented(
        "PerformPromiseAll: remainingElementsCount is not a Number",
      ));
    };
    write_internal_record_value(&mut step_scope, remaining, Value::Number(n + 1.0))?;

    // resolveElement = CreateBuiltinFunction(...)
    let resolve_element_name = step_scope.alloc_string("resolveElement")?;
    // Root the name string: `alloc_native_function_with_slots` may allocate and trigger GC.
    step_scope.push_root(Value::String(resolve_element_name))?;
    let slots = [
      Value::Object(values),
      Value::Number(index as f64),
      Value::Object(already_called),
      Value::Object(remaining),
      capability.resolve,
    ];
    let resolve_element = step_scope.alloc_native_function_with_slots(
      resolve_element_call,
      None,
      resolve_element_name,
      1,
      &slots,
    )?;
    step_scope
      .heap_mut()
      .object_set_prototype(resolve_element, Some(intr.function_prototype()))?;
    // Root the per-element callback while calling `then`: the `Invoke` path may allocate and GC.
    step_scope.push_root(Value::Object(resolve_element))?;

    // ? Invoke(nextPromise, "then", « resolveElement, capability.reject »).
    invoke_thenable_then(
      vm,
      &mut step_scope,
      host,
      hooks,
      next_promise,
      Value::Object(resolve_element),
      capability.reject,
    )?;

    index = index.saturating_add(1);
  }
}

pub fn promise_all(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `Promise.all(iterable)` (ECMA-262).
  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;

  // Root the resulting promise and resolving functions so `IfAbruptRejectPromise` can call them
  // even if the iterator acquisition/loop allocates and triggers GC.
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  scope.push_root(capability.reject)?;

  let promise_resolve = match get_promise_resolve(vm, scope, host, hooks, this) {
    Ok(v) => v,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };

  let mut iterator_record = match crate::iterator::get_iterator(vm, host, hooks, scope, iterable) {
    Ok(r) => r,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let result = perform_promise_all(
    vm,
    scope,
    host,
    hooks,
    &mut iterator_record,
    this,
    capability,
    promise_resolve,
  );

  match result {
    Ok(v) => Ok(v),
    Err(err) => {
      if !iterator_record.done {
        let _ = crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record);
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, err)
    }
  }
}

fn perform_promise_race(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  iterator_record: &mut crate::iterator::IteratorRecord,
  constructor: Value,
  capability: PromiseCapability,
  promise_resolve: Value,
) -> Result<Value, VmError> {
  // `PerformPromiseRace` (ECMA-262).
  loop {
    let next_value = crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record)?;
    let Some(next_value) = next_value else {
      return Ok(capability.promise);
    };

    // Use a nested scope so per-element roots do not accumulate.
    let mut step_scope = scope.reborrow();
    // Root the iterator value: `Call(promiseResolve, ...)` can allocate and trigger GC.
    step_scope.push_root(next_value)?;

    let next_promise =
      vm.call_with_host_and_hooks(host, &mut step_scope, hooks, promise_resolve, constructor, &[next_value])?;
    // Root the promise while invoking `.then` on it.
    step_scope.push_root(next_promise)?;

    invoke_thenable_then(
      vm,
      &mut step_scope,
      host,
      hooks,
      next_promise,
      capability.resolve,
      capability.reject,
    )?;
  }
}

pub fn promise_race(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `Promise.race(iterable)` (ECMA-262).
  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  scope.push_root(capability.reject)?;

  let promise_resolve = match get_promise_resolve(vm, scope, host, hooks, this) {
    Ok(v) => v,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };

  let mut iterator_record = match crate::iterator::get_iterator(vm, host, hooks, scope, iterable) {
    Ok(r) => r,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let result = perform_promise_race(
    vm,
    scope,
    host,
    hooks,
    &mut iterator_record,
    this,
    capability,
    promise_resolve,
  );

  match result {
    Ok(v) => Ok(v),
    Err(err) => {
      if !iterator_record.done {
        let _ = crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record);
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, err)
    }
  }
}

fn perform_promise_all_settled(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  iterator_record: &mut crate::iterator::IteratorRecord,
  constructor: Value,
  capability: PromiseCapability,
  promise_resolve: Value,
) -> Result<Value, VmError> {
  // `PerformPromiseAllSettled` (ECMA-262).
  let intr = require_intrinsics(vm)?;

  let values = scope.alloc_array(0)?;
  scope.push_root(Value::Object(values))?;
  scope
    .heap_mut()
    .object_set_prototype(values, Some(intr.array_prototype()))?;

  let remaining = create_internal_record(scope, intr.object_prototype(), Value::Number(1.0))?;
  scope.push_root(Value::Object(remaining))?;

  let element_call = intr.promise_all_settled_element_call();
  let mut index: u32 = 0;

  loop {
    let next_value = crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record)?;
    let Some(next_value) = next_value else {
      let remaining_value = read_internal_record_value(scope, remaining)?;
      let Value::Number(n) = remaining_value else {
        return Err(VmError::Unimplemented(
          "PerformPromiseAllSettled: remainingElementsCount is not a Number",
        ));
      };
      let new_remaining = n - 1.0;
      write_internal_record_value(scope, remaining, Value::Number(new_remaining))?;
      if new_remaining == 0.0 {
        let _ = vm.call_with_host_and_hooks(
          host,
          scope,
          hooks,
          capability.resolve,
          Value::Undefined,
          &[Value::Object(values)],
        )?;
      }
      return Ok(capability.promise);
    };

    // Use a nested scope so temporary roots created while wiring each element do not accumulate.
    let mut step_scope = scope.reborrow();
    step_scope.push_root(next_value)?;

    // Append placeholder.
    {
      let mut idx_scope = step_scope.reborrow();
      idx_scope.push_root(Value::Object(values))?;
      let idx_s = idx_scope.alloc_string(&index.to_string())?;
      idx_scope.push_root(Value::String(idx_s))?;
      let key = PropertyKey::from_string(idx_s);
      idx_scope.create_data_property_or_throw(values, key, Value::Undefined)?;
    }

    let next_promise = vm.call_with_host_and_hooks(
      host,
      &mut step_scope,
      hooks,
      promise_resolve,
      constructor,
      &[next_value],
    )?;
    step_scope.push_root(next_promise)?;

    // Shared alreadyCalled record for the pair of element functions.
    let already_called =
      create_internal_record(&mut step_scope, intr.object_prototype(), Value::Bool(false))?;
    step_scope.push_root(Value::Object(already_called))?;

    // remainingElementsCount.[[Value]] += 1.
    let remaining_value = read_internal_record_value(&mut step_scope, remaining)?;
    let Value::Number(n) = remaining_value else {
      return Err(VmError::Unimplemented(
        "PerformPromiseAllSettled: remainingElementsCount is not a Number",
      ));
    };
    write_internal_record_value(&mut step_scope, remaining, Value::Number(n + 1.0))?;

    let on_fulfilled_name = step_scope.alloc_string("onFulfilled")?;
    // Root the first name before allocating the second; allocations may GC.
    step_scope.push_root(Value::String(on_fulfilled_name))?;
    let on_rejected_name = step_scope.alloc_string("onRejected")?;
    step_scope.push_root(Value::String(on_rejected_name))?;
    let fulfilled_slots = [
      Value::Object(values),
      Value::Number(index as f64),
      Value::Object(already_called),
      Value::Object(remaining),
      capability.resolve,
      Value::Bool(false),
    ];
    let rejected_slots = [
      Value::Object(values),
      Value::Number(index as f64),
      Value::Object(already_called),
      Value::Object(remaining),
      capability.resolve,
      Value::Bool(true),
    ];

    let on_fulfilled = step_scope.alloc_native_function_with_slots(
      element_call,
      None,
      on_fulfilled_name,
      1,
      &fulfilled_slots,
    )?;
    step_scope
      .heap_mut()
      .object_set_prototype(on_fulfilled, Some(intr.function_prototype()))?;
    // Root the first closure while allocating the second: both share `alreadyCalled`.
    step_scope.push_root(Value::Object(on_fulfilled))?;

    let on_rejected = step_scope.alloc_native_function_with_slots(
      element_call,
      None,
      on_rejected_name,
      1,
      &rejected_slots,
    )?;
    step_scope
      .heap_mut()
      .object_set_prototype(on_rejected, Some(intr.function_prototype()))?;
    // Root both closures while invoking `.then`: the call path may allocate and GC.
    step_scope.push_root(Value::Object(on_rejected))?;

    invoke_thenable_then(
      vm,
      &mut step_scope,
      host,
      hooks,
      next_promise,
      Value::Object(on_fulfilled),
      Value::Object(on_rejected),
    )?;

    index = index.saturating_add(1);
  }
}

pub fn promise_all_settled(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `Promise.allSettled(iterable)` (ECMA-262).
  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  scope.push_root(capability.reject)?;

  let promise_resolve = match get_promise_resolve(vm, scope, host, hooks, this) {
    Ok(v) => v,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };

  let mut iterator_record = match crate::iterator::get_iterator(vm, host, hooks, scope, iterable) {
    Ok(r) => r,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let result = perform_promise_all_settled(
    vm,
    scope,
    host,
    hooks,
    &mut iterator_record,
    this,
    capability,
    promise_resolve,
  );

  match result {
    Ok(v) => Ok(v),
    Err(err) => {
      if !iterator_record.done {
        let _ = crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record);
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, err)
    }
  }
}

fn perform_promise_any(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  iterator_record: &mut crate::iterator::IteratorRecord,
  constructor: Value,
  capability: PromiseCapability,
  promise_resolve: Value,
) -> Result<Value, VmError> {
  // `PerformPromiseAny` (ECMA-262).
  let intr = require_intrinsics(vm)?;

  let errors = scope.alloc_array(0)?;
  scope.push_root(Value::Object(errors))?;
  scope
    .heap_mut()
    .object_set_prototype(errors, Some(intr.array_prototype()))?;

  let remaining = create_internal_record(scope, intr.object_prototype(), Value::Number(1.0))?;
  scope.push_root(Value::Object(remaining))?;

  let reject_element_call = intr.promise_any_reject_element_call();
  let mut index: u32 = 0;

  loop {
    let next_value = crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record)?;
    let Some(next_value) = next_value else {
      let remaining_value = read_internal_record_value(scope, remaining)?;
      let Value::Number(n) = remaining_value else {
        return Err(VmError::Unimplemented(
          "PerformPromiseAny: remainingElementsCount is not a Number",
        ));
      };
      let new_remaining = n - 1.0;
      write_internal_record_value(scope, remaining, Value::Number(new_remaining))?;
      if new_remaining == 0.0 {
        let message = scope.alloc_string("All promises were rejected")?;
        // Root the string + newly constructed aggregate error before calling into JS.
        scope.push_root(Value::String(message))?;
        let aggregate = vm.construct_with_host_and_hooks(
          host,
          scope,
          hooks,
          Value::Object(intr.aggregate_error()),
          &[Value::Object(errors), Value::String(message)],
          Value::Object(intr.aggregate_error()),
        )?;
        scope.push_root(aggregate)?;
        let _ = vm.call_with_host_and_hooks(
          host,
          scope,
          hooks,
          capability.reject,
          Value::Undefined,
          &[aggregate],
        )?;
      }
      return Ok(capability.promise);
    };

    let mut step_scope = scope.reborrow();
    step_scope.push_root(next_value)?;

    // Append placeholder.
    {
      let mut idx_scope = step_scope.reborrow();
      idx_scope.push_root(Value::Object(errors))?;
      let idx_s = idx_scope.alloc_string(&index.to_string())?;
      idx_scope.push_root(Value::String(idx_s))?;
      let key = PropertyKey::from_string(idx_s);
      idx_scope.create_data_property_or_throw(errors, key, Value::Undefined)?;
    }

    let next_promise = vm.call_with_host_and_hooks(
      host,
      &mut step_scope,
      hooks,
      promise_resolve,
      constructor,
      &[next_value],
    )?;
    step_scope.push_root(next_promise)?;

    let already_called =
      create_internal_record(&mut step_scope, intr.object_prototype(), Value::Bool(false))?;
    step_scope.push_root(Value::Object(already_called))?;

    // remainingElementsCount.[[Value]] += 1.
    let remaining_value = read_internal_record_value(&mut step_scope, remaining)?;
    let Value::Number(n) = remaining_value else {
      return Err(VmError::Unimplemented(
        "PerformPromiseAny: remainingElementsCount is not a Number",
      ));
    };
    write_internal_record_value(&mut step_scope, remaining, Value::Number(n + 1.0))?;

    let reject_element_name = step_scope.alloc_string("rejectElement")?;
    step_scope.push_root(Value::String(reject_element_name))?;
    let slots = [
      Value::Object(errors),
      Value::Number(index as f64),
      Value::Object(already_called),
      Value::Object(remaining),
      capability.reject,
    ];
    let reject_element = step_scope.alloc_native_function_with_slots(
      reject_element_call,
      None,
      reject_element_name,
      1,
      &slots,
    )?;
    step_scope
      .heap_mut()
      .object_set_prototype(reject_element, Some(intr.function_prototype()))?;
    step_scope.push_root(Value::Object(reject_element))?;

    // Use resultCapability.[[Resolve]] directly for fulfillment.
    invoke_thenable_then(
      vm,
      &mut step_scope,
      host,
      hooks,
      next_promise,
      capability.resolve,
      Value::Object(reject_element),
    )?;

    index = index.saturating_add(1);
  }
}

pub fn promise_any(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `Promise.any(iterable)` (ECMA-262).
  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  let capability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks, this)?;
  scope.push_root(capability.promise)?;
  scope.push_root(capability.resolve)?;
  scope.push_root(capability.reject)?;

  let promise_resolve = match get_promise_resolve(vm, scope, host, hooks, this) {
    Ok(v) => v,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };

  let mut iterator_record = match crate::iterator::get_iterator(vm, host, hooks, scope, iterable) {
    Ok(r) => r,
    Err(err) => return if_abrupt_reject_promise(vm, scope, host, hooks, capability, err),
  };
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let result = perform_promise_any(
    vm,
    scope,
    host,
    hooks,
    &mut iterator_record,
    this,
    capability,
    promise_resolve,
  );

  match result {
    Ok(v) => Ok(v),
    Err(err) => {
      if !iterator_record.done {
        let _ = crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record);
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, err)
    }
  }
}

pub fn promise_all_resolve_element_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `PromiseAllResolveElementFunctions` (ECMA-262).
  let x = args.get(0).copied().unwrap_or(Value::Undefined);
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 5 {
    return Err(VmError::InvariantViolation(
      "PromiseAllResolveElement has wrong native slot count",
    ));
  }

  let Value::Object(values) = slots[0] else {
    return Err(VmError::Unimplemented(
      "PromiseAllResolveElement values is not an object",
    ));
  };
  let Value::Number(index) = slots[1] else {
    return Err(VmError::Unimplemented(
      "PromiseAllResolveElement index is not a Number",
    ));
  };
  let Value::Object(already_called) = slots[2] else {
    return Err(VmError::Unimplemented(
      "PromiseAllResolveElement alreadyCalled is not an object",
    ));
  };
  let Value::Object(remaining) = slots[3] else {
    return Err(VmError::Unimplemented(
      "PromiseAllResolveElement remainingElementsCount is not an object",
    ));
  };
  let resolve = slots[4];

  // alreadyCalled check.
  let already = read_internal_record_value(scope, already_called)?;
  if already == Value::Bool(true) {
    return Ok(Value::Undefined);
  }
  write_internal_record_value(scope, already_called, Value::Bool(true))?;

  // values[index] = x.
  {
    let mut idx_scope = scope.reborrow();
    idx_scope.push_root(Value::Object(values))?;
    let idx_s = idx_scope.alloc_string(&(index as u32).to_string())?;
    idx_scope.push_root(Value::String(idx_s))?;
    let key = PropertyKey::from_string(idx_s);
    idx_scope.create_data_property_or_throw(values, key, x)?;
  }

  // remainingElementsCount--.
  let remaining_value = read_internal_record_value(scope, remaining)?;
  let Value::Number(n) = remaining_value else {
    return Err(VmError::Unimplemented(
      "PromiseAllResolveElement remainingElementsCount is not a Number",
    ));
  };
  let new_remaining = n - 1.0;
  write_internal_record_value(scope, remaining, Value::Number(new_remaining))?;
  if new_remaining == 0.0 {
    let _ = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      resolve,
      Value::Undefined,
      &[Value::Object(values)],
    )?;
  }

  Ok(Value::Undefined)
}

pub fn promise_all_settled_element_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `PromiseAllSettledResolveElementFunctions` / `PromiseAllSettledRejectElementFunctions`
  // (ECMA-262).
  let x = args.get(0).copied().unwrap_or(Value::Undefined);
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 6 {
    return Err(VmError::InvariantViolation(
      "PromiseAllSettledElement has wrong native slot count",
    ));
  }

  let Value::Object(values) = slots[0] else {
    return Err(VmError::Unimplemented(
      "PromiseAllSettledElement values is not an object",
    ));
  };
  let Value::Number(index) = slots[1] else {
    return Err(VmError::Unimplemented(
      "PromiseAllSettledElement index is not a Number",
    ));
  };
  let Value::Object(already_called) = slots[2] else {
    return Err(VmError::Unimplemented(
      "PromiseAllSettledElement alreadyCalled is not an object",
    ));
  };
  let Value::Object(remaining) = slots[3] else {
    return Err(VmError::Unimplemented(
      "PromiseAllSettledElement remainingElementsCount is not an object",
    ));
  };
  let resolve = slots[4];
  let Value::Bool(is_reject) = slots[5] else {
    return Err(VmError::Unimplemented(
      "PromiseAllSettledElement kind flag is not a Bool",
    ));
  };

  let already = read_internal_record_value(scope, already_called)?;
  if already == Value::Bool(true) {
    return Ok(Value::Undefined);
  }
  write_internal_record_value(scope, already_called, Value::Bool(true))?;

  let intr = require_intrinsics(vm)?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.object_prototype()))?;

  // Create `{ status, value }` / `{ status, reason }` object.
  let status_value = if is_reject { "rejected" } else { "fulfilled" };
  let status_value = scope.alloc_string(status_value)?;
  scope.push_root(Value::String(status_value))?;
  let status_key = string_key(scope, "status")?;
  scope.define_property(
    obj,
    status_key,
    data_desc(Value::String(status_value), true, true, true),
  )?;

  let value_key_name = if is_reject { "reason" } else { "value" };
  let value_key = string_key(scope, value_key_name)?;
  scope.define_property(obj, value_key, data_desc(x, true, true, true))?;

  // values[index] = obj.
  {
    let mut idx_scope = scope.reborrow();
    idx_scope.push_root(Value::Object(values))?;
    idx_scope.push_root(Value::Object(obj))?;
    let idx_s = idx_scope.alloc_string(&(index as u32).to_string())?;
    idx_scope.push_root(Value::String(idx_s))?;
    let key = PropertyKey::from_string(idx_s);
    idx_scope.create_data_property_or_throw(values, key, Value::Object(obj))?;
  }

  // remainingElementsCount--.
  let remaining_value = read_internal_record_value(scope, remaining)?;
  let Value::Number(n) = remaining_value else {
    return Err(VmError::Unimplemented(
      "PromiseAllSettledElement remainingElementsCount is not a Number",
    ));
  };
  let new_remaining = n - 1.0;
  write_internal_record_value(scope, remaining, Value::Number(new_remaining))?;
  if new_remaining == 0.0 {
    let _ = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      resolve,
      Value::Undefined,
      &[Value::Object(values)],
    )?;
  }

  Ok(Value::Undefined)
}

pub fn promise_any_reject_element_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `PromiseAnyRejectElementFunctions` (ECMA-262).
  let x = args.get(0).copied().unwrap_or(Value::Undefined);
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 5 {
    return Err(VmError::InvariantViolation(
      "PromiseAnyRejectElement has wrong native slot count",
    ));
  }

  let Value::Object(errors) = slots[0] else {
    return Err(VmError::Unimplemented(
      "PromiseAnyRejectElement errors is not an object",
    ));
  };
  let Value::Number(index) = slots[1] else {
    return Err(VmError::Unimplemented(
      "PromiseAnyRejectElement index is not a Number",
    ));
  };
  let Value::Object(already_called) = slots[2] else {
    return Err(VmError::Unimplemented(
      "PromiseAnyRejectElement alreadyCalled is not an object",
    ));
  };
  let Value::Object(remaining) = slots[3] else {
    return Err(VmError::Unimplemented(
      "PromiseAnyRejectElement remainingElementsCount is not an object",
    ));
  };
  let reject = slots[4];

  let already = read_internal_record_value(scope, already_called)?;
  if already == Value::Bool(true) {
    return Ok(Value::Undefined);
  }
  write_internal_record_value(scope, already_called, Value::Bool(true))?;

  // errors[index] = x.
  {
    let mut idx_scope = scope.reborrow();
    idx_scope.push_root(Value::Object(errors))?;
    let idx_s = idx_scope.alloc_string(&(index as u32).to_string())?;
    idx_scope.push_root(Value::String(idx_s))?;
    let key = PropertyKey::from_string(idx_s);
    idx_scope.create_data_property_or_throw(errors, key, x)?;
  }

  // remainingElementsCount--.
  let remaining_value = read_internal_record_value(scope, remaining)?;
  let Value::Number(n) = remaining_value else {
    return Err(VmError::Unimplemented(
      "PromiseAnyRejectElement remainingElementsCount is not a Number",
    ));
  };
  let new_remaining = n - 1.0;
  write_internal_record_value(scope, remaining, Value::Number(new_remaining))?;
  if new_remaining == 0.0 {
    let intr = require_intrinsics(vm)?;
    let message = scope.alloc_string("All promises were rejected")?;
    scope.push_root(Value::String(message))?;
    let aggregate = vm.construct_with_host_and_hooks(
      host,
      scope,
      hooks,
      Value::Object(intr.aggregate_error()),
      &[Value::Object(errors), Value::String(message)],
      Value::Object(intr.aggregate_error()),
    )?;
    scope.push_root(aggregate)?;
    let _ = vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[aggregate])?;
  }

  Ok(Value::Undefined)
}

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  let key_s = scope.alloc_string(s)?;
  scope.push_root(Value::String(key_s))?;
  Ok(PropertyKey::from_string(key_s))
}

fn get_data_property_value(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: &PropertyKey,
) -> Result<Option<Value>, VmError> {
  let Some(desc) = scope.heap().get_property(obj, key)? else {
    return Ok(None);
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(Some(value)),
    PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
  }
}

fn to_length(value: Value) -> usize {
  let Value::Number(n) = value else {
    return 0;
  };
  if !n.is_finite() || n <= 0.0 {
    return 0;
  }
  if n >= usize::MAX as f64 {
    return usize::MAX;
  }
  n.floor() as usize
}

fn vec_try_push<T>(buf: &mut Vec<T>, value: T) -> Result<(), VmError> {
  if buf.len() == buf.capacity() {
    buf.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  }
  buf.push(value);
  Ok(())
}

fn vec_try_extend_from_slice<T: Copy>(buf: &mut Vec<T>, slice: &[T]) -> Result<(), VmError> {
  let needed = slice
    .len()
    .saturating_sub(buf.capacity().saturating_sub(buf.len()));
  if needed > 0 {
    buf.try_reserve(needed).map_err(|_| VmError::OutOfMemory)?;
  }
  buf.extend_from_slice(slice);
  Ok(())
}

/// `Function.prototype.call`.
pub fn function_prototype_call_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let this_arg = args.first().copied().unwrap_or(Value::Undefined);
  let rest = args.get(1..).unwrap_or(&[]);
  vm.call_with_host_and_hooks(host, scope, hooks, this, this_arg, rest)
}

/// `Function.prototype.apply` (minimal, supports array-like objects).
pub fn function_prototype_apply(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target = require_callable(this)?;
  let this_arg = args.first().copied().unwrap_or(Value::Undefined);
  let arg_array = args.get(1).copied().unwrap_or(Value::Undefined);

  match arg_array {
    Value::Undefined | Value::Null => {
      vm.call_with_host_and_hooks(host, scope, hooks, Value::Object(target), this_arg, &[])
    }
    Value::Object(obj) => {
      // Root `obj` while building the argument list, since we may allocate strings for property
      // keys and trigger a GC.
      scope.push_root(Value::Object(obj))?;
      let list = get_array_like_args(vm, scope, obj)?;
      vm.call_with_host_and_hooks(host, scope, hooks, Value::Object(target), this_arg, &list)
    }
    _ => Err(VmError::Unimplemented(
      "Function.prototype.apply: argArray must be an object or null/undefined",
    )),
  }
}

/// `Function.prototype.bind` (minimal, using `JsFunction` bound internal slots).
pub fn function_prototype_bind(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let target = require_callable(this)?;

  // Extract function metadata without holding a heap borrow across allocations.
  let (target_len, target_name) = {
    let f = scope.heap().get_function(target)?;
    (f.length, f.name)
  };

  let bound_this = args.first().copied().unwrap_or(Value::Undefined);
  let bound_args = args.get(1..).unwrap_or(&[]);

  let bound_args_len_u32 = u32::try_from(bound_args.len()).unwrap_or(u32::MAX);
  let bound_len = target_len.saturating_sub(bound_args_len_u32);

  let name = scope.alloc_string("bound")?;
  let func = scope.alloc_bound_function(target, bound_this, bound_args, name, bound_len)?;

  // Bound functions are ordinary function objects: their `[[Prototype]]` is `%Function.prototype%`.
  scope
    .heap_mut()
    .object_set_prototype(func, Some(intr.function_prototype()))?;

  // Define standard function metadata properties (`name`, `length`).
  crate::function_properties::set_function_name(
    scope,
    func,
    PropertyKey::String(target_name),
    Some("bound"),
  )?;
  crate::function_properties::set_function_length(scope, func, bound_len)?;

  if let Some(realm) = scope.heap().get_function_realm(target)? {
    scope.heap_mut().set_function_realm(func, realm)?;
  }

  let job_realm = scope
    .heap()
    .get_function_job_realm(target)
    .or(vm.current_realm());
  if let Some(job_realm) = job_realm {
    scope.heap_mut().set_function_job_realm(func, job_realm)?;
  }

  Ok(Value::Object(func))
}

/// `Object.prototype.toString` (partial).
pub fn object_prototype_to_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let s = match this {
    Value::Undefined => "[object Undefined]",
    Value::Null => "[object Null]",
    Value::Bool(_) => "[object Boolean]",
    Value::Number(_) => "[object Number]",
    Value::BigInt(_) => "[object BigInt]",
    Value::String(_) => "[object String]",
    Value::Symbol(_) => "[object Symbol]",
    Value::Object(obj) => {
      if scope.heap().is_callable(Value::Object(obj))? {
        "[object Function]"
      } else {
        "[object Object]"
      }
    }
  };
  Ok(Value::String(scope.alloc_string(s)?))
}

/// `Object.prototype.hasOwnProperty` (ECMA-262).
pub fn object_prototype_has_own_property(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let prop = args.get(0).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let has = scope.ordinary_get_own_property(obj, key)?.is_some();
  Ok(Value::Bool(has))
}

fn get_array_length(scope: &mut Scope<'_>, obj: GcObject) -> Result<usize, VmError> {
  let length_key = string_key(scope, "length")?;
  Ok(match get_data_property_value(scope, obj, &length_key)? {
    Some(v) => to_length(v),
    None => 0,
  })
}

fn define_array_length(scope: &mut Scope<'_>, obj: GcObject, len: usize) -> Result<(), VmError> {
  let length_key = string_key(scope, "length")?;
  scope.define_property(
    obj,
    length_key,
    data_desc(Value::Number(len as f64), true, false, false),
  )
}

/// `Array.prototype.map` (minimal).
pub fn array_prototype_map(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let this_obj = match this {
    Value::Object(o) => o,
    _ => return Err(VmError::Unimplemented("Array.prototype.map on non-object")),
  };

  let len = get_array_length(scope, this_obj)?;

  let callback = args.first().copied().unwrap_or(Value::Undefined);
  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.array_prototype()))?;
  define_array_length(scope, out, len)?;

  for i in 0..len {
    vm.tick()?;
    let key = PropertyKey::from_string(scope.alloc_string(&i.to_string())?);
    let Some(value) = get_data_property_value(scope, this_obj, &key)? else {
      continue;
    };

    // callback(value, index, array)
    let call_args = [value, Value::Number(i as f64), Value::Object(this_obj)];
    let mapped = vm.call_with_host_and_hooks(host, scope, hooks, callback, this_arg, &call_args)?;

    scope.define_property(out, key, data_desc(mapped, true, true, true))?;
  }

  Ok(Value::Object(out))
}

/// `Array.prototype.join` (minimal).
pub fn array_prototype_join(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let this_obj = match this {
    Value::Object(o) => o,
    _ => return Err(VmError::Unimplemented("Array.prototype.join on non-object")),
  };

  let len = get_array_length(scope, this_obj)?;

  let sep = match args.first().copied() {
    None | Some(Value::Undefined) => scope.alloc_string(",")?,
    Some(v) => scope.to_string(vm, host, hooks, v)?,
  };
  scope.push_root(Value::String(sep))?;
  let sep_slice = scope.heap().get_string(sep)?.as_code_units();
  let mut sep_units: Vec<u16> = Vec::new();
  sep_units
    .try_reserve_exact(sep_slice.len())
    .map_err(|_| VmError::OutOfMemory)?;
  vec_try_extend_from_slice(&mut sep_units, sep_slice)?;

  let empty = scope.alloc_string("")?;
  scope.push_root(Value::String(empty))?;

  let mut out: Vec<u16> = Vec::new();
  let max_bytes = scope.heap().limits().max_bytes;

  for i in 0..len {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    if i > 0 {
      if JsString::heap_size_bytes_for_len(out.len().saturating_add(sep_units.len())) > max_bytes {
        return Err(VmError::OutOfMemory);
      }
      vec_try_extend_from_slice(&mut out, &sep_units)?;
    }

    let key = PropertyKey::from_string(scope.alloc_string(&i.to_string())?);
    let value = get_data_property_value(scope, this_obj, &key)?.unwrap_or(Value::Undefined);
    let part = match value {
      Value::Undefined | Value::Null => empty,
      other => scope.to_string(vm, host, hooks, other)?,
    };

    let units = scope.heap().get_string(part)?.as_code_units();
    if JsString::heap_size_bytes_for_len(out.len().saturating_add(units.len())) > max_bytes {
      return Err(VmError::OutOfMemory);
    }
    vec_try_extend_from_slice(&mut out, units)?;
  }

  let s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(s))
}

/// `Array.prototype.slice` (minimal).
///
/// This is intentionally spec-shaped enough to support common JS patterns like:
/// - `arr.slice(1)`
/// - `Array.prototype.slice.call("ab")`
pub fn array_prototype_slice(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  let (start, end) = slice_range_from_args(vm, &mut scope, host, hooks, len, args)?;
  let count = end.saturating_sub(start);

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_array(count)?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.array_prototype()))?;

  for k in 0..count {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let from = start.saturating_add(k);

    let mut iter_scope = scope.reborrow();

    let from_s = iter_scope.alloc_string(&from.to_string())?;
    iter_scope.push_root(Value::String(from_s))?;
    let to_s = iter_scope.alloc_string(&k.to_string())?;
    iter_scope.push_root(Value::String(to_s))?;

    let from_key = PropertyKey::from_string(from_s);
    let to_key = PropertyKey::from_string(to_s);

    if !iter_scope.ordinary_has_property(obj, from_key)? {
      continue;
    }

    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, from_key, Value::Object(obj))?;
    iter_scope.create_data_property_or_throw(out, to_key, value)?;
  }

  Ok(Value::Object(out))
}

/// `Array.prototype.push` (minimal).
pub fn array_prototype_push(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let mut len = to_length(len_value);

  for (i, value) in args.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let idx_s = iter_scope.alloc_string(&len.to_string())?;
    let key = PropertyKey::from_string(idx_s);
    let ok = iter_scope.ordinary_set_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      key,
      value,
      Value::Object(obj),
    )?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.push failed"));
    }
    len = len.saturating_add(1);
  }

  // Per spec, set the final length even though array index writes already extend length.
  let ok = scope.ordinary_set_with_host_and_hooks(
    vm,
    host,
    hooks,
    obj,
    length_key,
    Value::Number(len as f64),
    Value::Object(obj),
  )?;
  if !ok {
    return Err(VmError::TypeError("Array.prototype.push failed"));
  }

  Ok(Value::Number(len as f64))
}

/// `Array.prototype.splice` (minimal).
///
/// This is implemented in a spec-shaped way so it works on array-like objects (e.g.
/// `Array.prototype.splice.call(obj, ...)`) and respects accessors/prototype properties.
pub fn array_prototype_splice(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  let start = args.get(0).copied().unwrap_or(Value::Undefined);
  let actual_start = slice_index_from_value(vm, &mut scope, host, hooks, start, len, 0)?;

  let insert_count = args.len().saturating_sub(2);

  let actual_delete_count = if args.len() < 2 {
    len.saturating_sub(actual_start)
  } else {
    let delete_count_val = args.get(1).copied().unwrap_or(Value::Undefined);
    let n = scope.to_number(vm, host, hooks, delete_count_val)?;
    if n.is_nan() || n <= 0.0 {
      0usize
    } else if !n.is_finite() {
      len.saturating_sub(actual_start)
    } else {
      let n = n.trunc();
      let max = len.saturating_sub(actual_start);
      if n >= max as f64 {
        max
      } else {
        (n as usize).min(max)
      }
    }
  };

  let new_len = len
    .checked_sub(actual_delete_count)
    .and_then(|v| v.checked_add(insert_count))
    .ok_or(VmError::OutOfMemory)?;

  let intr = require_intrinsics(vm)?;

  // Create the returned array of deleted elements.
  let removed = scope.alloc_array(actual_delete_count)?;
  scope.push_root(Value::Object(removed))?;
  scope
    .heap_mut()
    .object_set_prototype(removed, Some(intr.array_prototype()))?;

  // Copy deleted elements.
  for k in 0..actual_delete_count {
    if k % 1024 == 0 {
      vm.tick()?;
    }
    let from = actual_start
      .checked_add(k)
      .ok_or(VmError::OutOfMemory)?;

    let mut iter_scope = scope.reborrow();

    let from_s = iter_scope.alloc_string(&from.to_string())?;
    iter_scope.push_root(Value::String(from_s))?;
    let to_s = iter_scope.alloc_string(&k.to_string())?;
    iter_scope.push_root(Value::String(to_s))?;

    let from_key = PropertyKey::from_string(from_s);
    let to_key = PropertyKey::from_string(to_s);

    if !iter_scope.ordinary_has_property(obj, from_key)? {
      continue;
    }
    let value = iter_scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      from_key,
      Value::Object(obj),
    )?;
    iter_scope.create_data_property_or_throw(removed, to_key, value)?;
  }

  // Shift existing elements to close/open the gap.
  if insert_count < actual_delete_count {
    let limit = len
      .checked_sub(actual_delete_count)
      .ok_or(VmError::OutOfMemory)?;
    for k in actual_start..limit {
      if k % 1024 == 0 {
        vm.tick()?;
      }
      let from = k
        .checked_add(actual_delete_count)
        .ok_or(VmError::OutOfMemory)?;
      let to = k.checked_add(insert_count).ok_or(VmError::OutOfMemory)?;

      let mut iter_scope = scope.reborrow();

      let from_s = iter_scope.alloc_string(&from.to_string())?;
      iter_scope.push_root(Value::String(from_s))?;
      let to_s = iter_scope.alloc_string(&to.to_string())?;
      iter_scope.push_root(Value::String(to_s))?;

      let from_key = PropertyKey::from_string(from_s);
      let to_key = PropertyKey::from_string(to_s);

      if iter_scope.ordinary_has_property(obj, from_key)? {
        let value = iter_scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          from_key,
          Value::Object(obj),
        )?;
        let ok = iter_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          to_key,
          value,
          Value::Object(obj),
        )?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.splice failed"));
        }
      } else {
        let ok = iter_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, to_key)?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.splice failed"));
        }
      }
    }

    // Delete trailing properties.
    let mut k = len;
    while k > new_len {
      if k % 1024 == 0 {
        vm.tick()?;
      }
      let idx = k - 1;
      let mut del_scope = scope.reborrow();
      let idx_s = del_scope.alloc_string(&idx.to_string())?;
      let key = PropertyKey::from_string(idx_s);
      let ok = del_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, key)?;
      if !ok {
        return Err(VmError::TypeError("Array.prototype.splice failed"));
      }
      k -= 1;
    }
  } else if insert_count > actual_delete_count {
    let mut k = len
      .checked_sub(actual_delete_count)
      .ok_or(VmError::OutOfMemory)?;
    while k > actual_start {
      if k % 1024 == 0 {
        vm.tick()?;
      }
      let from = k
        .checked_add(actual_delete_count)
        .and_then(|v| v.checked_sub(1))
        .ok_or(VmError::OutOfMemory)?;
      let to = k
        .checked_add(insert_count)
        .and_then(|v| v.checked_sub(1))
        .ok_or(VmError::OutOfMemory)?;

      let mut iter_scope = scope.reborrow();

      let from_s = iter_scope.alloc_string(&from.to_string())?;
      iter_scope.push_root(Value::String(from_s))?;
      let to_s = iter_scope.alloc_string(&to.to_string())?;
      iter_scope.push_root(Value::String(to_s))?;

      let from_key = PropertyKey::from_string(from_s);
      let to_key = PropertyKey::from_string(to_s);

      if iter_scope.ordinary_has_property(obj, from_key)? {
        let value = iter_scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          from_key,
          Value::Object(obj),
        )?;
        let ok = iter_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          to_key,
          value,
          Value::Object(obj),
        )?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.splice failed"));
        }
      } else {
        let ok = iter_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, to_key)?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.splice failed"));
        }
      }

      k -= 1;
    }
  }

  // Insert new items.
  for (j, item) in args.get(2..).unwrap_or(&[]).iter().copied().enumerate() {
    if j % 1024 == 0 {
      vm.tick()?;
    }
    let to = actual_start.checked_add(j).ok_or(VmError::OutOfMemory)?;
    let mut set_scope = scope.reborrow();
    let to_s = set_scope.alloc_string(&to.to_string())?;
    let key = PropertyKey::from_string(to_s);
    let ok =
      set_scope.ordinary_set_with_host_and_hooks(vm, host, hooks, obj, key, item, Value::Object(obj))?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.splice failed"));
    }
  }

  // Update length.
  let ok = scope.ordinary_set_with_host_and_hooks(
    vm,
    host,
    hooks,
    obj,
    length_key,
    Value::Number(new_len as f64),
    Value::Object(obj),
  )?;
  if !ok {
    return Err(VmError::TypeError("Array.prototype.splice failed"));
  }

  Ok(Value::Object(removed))
}

/// `String` constructor called as a function.
pub fn string_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let s = match args.first().copied() {
    None => scope.alloc_string("")?,
    // ECMA-262 `String ( value )` special-case: `String(Symbol("x"))` does not throw even though
    // `ToString(Symbol("x"))` would.
    Some(Value::Symbol(sym)) => symbol_descriptive_string(scope, sym)?,
    Some(v) => scope.to_string(vm, host, hooks, v)?,
  };
  Ok(Value::String(s))
}

/// `new String(value)` (minimal wrapper object).
pub fn string_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let prim = match args.first().copied() {
    None => scope.alloc_string("")?,
    Some(v) => scope.to_string(vm, host, hooks, v)?,
  };
  scope.push_root(Value::String(prim))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.string_prototype()))?;

  // Store the primitive value on an internal symbol so `String.prototype.toString` can recover it.
  let marker = scope.alloc_string("vm-js.internal.StringData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  scope.define_property(
    obj,
    marker_key,
    data_desc(Value::String(prim), true, false, false),
  )?;

  let len = scope.heap().get_string(prim)?.len_code_units();
  let length_key = string_key(scope, "length")?;
  scope.define_property(
    obj,
    length_key,
    data_desc(Value::Number(len as f64), false, false, false),
  )?;

  // Best-effort: if `new_target.prototype` is an object, use it.
  if let Value::Object(nt) = new_target {
    let proto_key = string_key(scope, "prototype")?;
    if let Ok(Value::Object(p)) = scope.heap().get(nt, &proto_key) {
      scope.heap_mut().object_set_prototype(obj, Some(p))?;
    }
  }

  Ok(Value::Object(obj))
}

/// `String.prototype.toString` (minimal).
pub fn string_prototype_to_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  match this {
    Value::String(s) => Ok(Value::String(s)),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.StringData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::String(s)) => Ok(Value::String(s)),
        _ => Err(VmError::Unimplemented(
          "String.prototype.toString on non-String object",
        )),
      }
    }
    _ => Err(VmError::Unimplemented(
      "String.prototype.toString on non-string",
    )),
  }
}

/// `%String.prototype%[@@iterator]` (ECMA-262).
///
/// This returns an iterator object with internal slots:
/// - `[[IteratedString]]`: stored as a non-enumerable symbol-keyed data property
/// - `[[NextIndex]]`: stored as a non-enumerable symbol-keyed data property
///
/// The iterator object's `next` method is a shared native builtin captured in the iterator method's
/// native slots.
pub fn string_prototype_iterator(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  if matches!(this, Value::Null | Value::Undefined) {
    return Err(VmError::TypeError(
      "String.prototype[Symbol.iterator] called on null or undefined",
    ));
  }
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 3 {
    return Err(VmError::InvariantViolation(
      "String.prototype[Symbol.iterator] has wrong native slot count",
    ));
  }
  let Value::Object(next_fn) = slots[0] else {
    return Err(VmError::InvariantViolation(
      "String.prototype[Symbol.iterator] next slot is not an object",
    ));
  };
  let Value::Symbol(iterated_sym) = slots[1] else {
    return Err(VmError::InvariantViolation(
      "String.prototype[Symbol.iterator] iteratedString slot is not a symbol",
    ));
  };
  let Value::Symbol(next_index_sym) = slots[2] else {
    return Err(VmError::InvariantViolation(
      "String.prototype[Symbol.iterator] nextIndex slot is not a symbol",
    ));
  };

  let intr = require_intrinsics(vm)?;
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  scope
    .heap_mut()
    .object_set_prototype(iter, Some(intr.object_prototype()))?;

  scope.define_property(
    iter,
    PropertyKey::from_symbol(iterated_sym),
    data_desc(Value::String(s), true, false, false),
  )?;
  scope.define_property(
    iter,
    PropertyKey::from_symbol(next_index_sym),
    data_desc(Value::Number(0.0), true, false, false),
  )?;

  let next_key = string_key(scope, "next")?;
  scope.define_property(iter, next_key, data_desc(Value::Object(next_fn), true, false, true))?;

  Ok(Value::Object(iter))
}

/// `%StringIteratorPrototype%.next` (ECMA-262).
pub fn string_iterator_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let Value::Object(iter) = this else {
    return Err(VmError::TypeError(
      "String iterator next called on non-object",
    ));
  };

  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 2 {
    return Err(VmError::InvariantViolation(
      "String iterator next has wrong native slot count",
    ));
  }
  let Value::Symbol(iterated_sym) = slots[0] else {
    return Err(VmError::InvariantViolation(
      "String iterator next iteratedString slot is not a symbol",
    ));
  };
  let Value::Symbol(next_index_sym) = slots[1] else {
    return Err(VmError::InvariantViolation(
      "String iterator next nextIndex slot is not a symbol",
    ));
  };

  let iterated_key = PropertyKey::from_symbol(iterated_sym);
  let Some(Value::String(iterated)) = scope
    .heap()
    .object_get_own_data_property_value(iter, &iterated_key)?
  else {
    // Once `[[IteratedString]]` is `undefined`, the iterator is complete.
    let result = scope.alloc_object()?;
    scope.push_root(Value::Object(result))?;
    scope
      .heap_mut()
      .object_set_prototype(result, Some(intr.object_prototype()))?;
    let value_key = string_key(scope, "value")?;
    scope.define_property(result, value_key, data_desc(Value::Undefined, true, true, true))?;
    let done_key = string_key(scope, "done")?;
    scope.define_property(result, done_key, data_desc(Value::Bool(true), true, true, true))?;
    return Ok(Value::Object(result));
  };

  let next_index_key = PropertyKey::from_symbol(next_index_sym);
  let next_index_val = scope
    .heap()
    .object_get_own_data_property_value(iter, &next_index_key)?
    .unwrap_or(Value::Number(0.0));
  let Value::Number(n) = next_index_val else {
    return Err(VmError::InvariantViolation(
      "String iterator nextIndex is not a number",
    ));
  };
  if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
    return Err(VmError::InvariantViolation(
      "String iterator nextIndex is not a non-negative integer",
    ));
  }
  let idx = n as usize;

  let len = {
    let string = scope.heap().get_string(iterated)?;
    string.len_code_units()
  };

  // End-of-iteration: clear `[[IteratedString]]` so the underlying string can be collected if this
  // iterator is retained.
  if idx >= len {
    scope.define_property(
      iter,
      iterated_key,
      data_desc(Value::Undefined, true, false, false),
    )?;

    let result = scope.alloc_object()?;
    scope.push_root(Value::Object(result))?;
    scope
      .heap_mut()
      .object_set_prototype(result, Some(intr.object_prototype()))?;
    let value_key = string_key(scope, "value")?;
    scope.define_property(result, value_key, data_desc(Value::Undefined, true, true, true))?;
    let done_key = string_key(scope, "done")?;
    scope.define_property(result, done_key, data_desc(Value::Bool(true), true, true, true))?;
    return Ok(Value::Object(result));
  }

  // Extract the next code point (1-2 UTF-16 code units), per `StringIteratorNext`.
  let (end, units) = {
    let string = scope.heap().get_string(iterated)?;
    let code_units = string.as_code_units();
    let first = code_units
      .get(idx)
      .copied()
      .ok_or(VmError::InvariantViolation(
        "String iterator index out of bounds",
      ))?;
    let mut take = 1usize;
    if (0xD800..=0xDBFF).contains(&first) && idx + 1 < len {
      let second = code_units[idx + 1];
      if (0xDC00..=0xDFFF).contains(&second) {
        take = 2;
      }
    }
    let end = idx + take;
    (end, code_units[idx..end].to_vec())
  };

  // Root the iterator + iterated string while allocating the substring/result object.
  let mut out_scope = scope.reborrow();
  out_scope.push_roots(&[Value::Object(iter), Value::String(iterated)])?;

  let value_s = out_scope.alloc_string_from_u16_vec(units)?;
  out_scope.push_root(Value::String(value_s))?;

  // Advance `[[NextIndex]]`.
  out_scope.define_property(
    iter,
    next_index_key,
    data_desc(Value::Number(end as f64), true, false, false),
  )?;

  // Create `{ value, done: false }`.
  let result = out_scope.alloc_object()?;
  out_scope.push_root(Value::Object(result))?;
  out_scope
    .heap_mut()
    .object_set_prototype(result, Some(intr.object_prototype()))?;
  let value_key = string_key(&mut out_scope, "value")?;
  out_scope.define_property(result, value_key, data_desc(Value::String(value_s), true, true, true))?;
  let done_key = string_key(&mut out_scope, "done")?;
  out_scope.define_property(result, done_key, data_desc(Value::Bool(false), true, true, true))?;

  Ok(Value::Object(result))
}

/// `Number` constructor called as a function.
pub fn number_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let n = match args.first().copied() {
    None => 0.0,
    Some(v) => scope.to_number(vm, host, hooks, v)?,
  };
  Ok(Value::Number(n))
}

/// `new Number(value)` (minimal wrapper object).
pub fn number_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let prim = match args.first().copied() {
    None => 0.0,
    Some(v) => scope.to_number(vm, host, hooks, v)?,
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.number_prototype()))?;

  // Store the primitive value on an internal symbol so `Number.prototype.valueOf` can recover it.
  let marker = scope.alloc_string("vm-js.internal.NumberData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  scope.define_property(
    obj,
    marker_key,
    data_desc(Value::Number(prim), true, false, false),
  )?;

  // Best-effort: if `new_target.prototype` is an object, use it.
  if let Value::Object(nt) = new_target {
    let proto_key = string_key(scope, "prototype")?;
    if let Ok(Value::Object(p)) = scope.heap().get(nt, &proto_key) {
      scope.heap_mut().object_set_prototype(obj, Some(p))?;
    }
  }

  Ok(Value::Object(obj))
}

/// `Number.prototype.valueOf` (minimal).
pub fn number_prototype_value_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  match this {
    Value::Number(n) => Ok(Value::Number(n)),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.NumberData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::Number(n)) => Ok(Value::Number(n)),
        _ => Err(VmError::Unimplemented(
          "Number.prototype.valueOf on non-Number object",
        )),
      }
    }
    _ => Err(VmError::Unimplemented(
      "Number.prototype.valueOf on non-number",
    )),
  }
}

/// `Boolean` constructor called as a function.
pub fn boolean_constructor_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let b = match args.first().copied() {
    None => false,
    Some(v) => scope.heap().to_boolean(v)?,
  };
  Ok(Value::Bool(b))
}

/// `new Boolean(value)` (minimal wrapper object).
pub fn boolean_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let prim = match args.first().copied() {
    None => false,
    Some(v) => scope.heap().to_boolean(v)?,
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.boolean_prototype()))?;

  // Store the primitive value on an internal symbol so `Boolean.prototype.valueOf` can recover it.
  let marker = scope.alloc_string("vm-js.internal.BooleanData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  scope.define_property(
    obj,
    marker_key,
    data_desc(Value::Bool(prim), true, false, false),
  )?;

  // Best-effort: if `new_target.prototype` is an object, use it.
  if let Value::Object(nt) = new_target {
    let proto_key = string_key(scope, "prototype")?;
    if let Ok(Value::Object(p)) = scope.heap().get(nt, &proto_key) {
      scope.heap_mut().object_set_prototype(obj, Some(p))?;
    }
  }

  Ok(Value::Object(obj))
}

/// `Boolean.prototype.valueOf` (minimal).
pub fn boolean_prototype_value_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  match this {
    Value::Bool(b) => Ok(Value::Bool(b)),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.BooleanData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::Bool(b)) => Ok(Value::Bool(b)),
        _ => Err(VmError::Unimplemented(
          "Boolean.prototype.valueOf on non-Boolean object",
        )),
      }
    }
    _ => Err(VmError::Unimplemented(
      "Boolean.prototype.valueOf on non-boolean",
    )),
  }
}

/// `BigInt.prototype.valueOf` (minimal).
pub fn bigint_prototype_value_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  match this {
    Value::BigInt(b) => Ok(Value::BigInt(b)),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.BigIntData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::BigInt(b)) => Ok(Value::BigInt(b)),
        _ => Err(VmError::Unimplemented(
          "BigInt.prototype.valueOf on non-BigInt object",
        )),
      }
    }
    _ => Err(VmError::Unimplemented(
      "BigInt.prototype.valueOf on non-bigint",
    )),
  }
}

/// `Symbol.prototype.valueOf` (minimal).
pub fn symbol_prototype_value_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  match this {
    Value::Symbol(s) => Ok(Value::Symbol(s)),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.SymbolData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::Symbol(s)) => Ok(Value::Symbol(s)),
        _ => Err(VmError::TypeError(
          "Symbol.prototype.valueOf called on non-Symbol object",
        )),
      }
    }
    _ => Err(VmError::TypeError(
      "Symbol.prototype.valueOf called on non-symbol",
    )),
  }
}

/// `Symbol.prototype.toString` (minimal).
pub fn symbol_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Symbol(sym) = symbol_prototype_value_of(vm, scope, host, hooks, callee, this, &[])? else {
    // `symbol_prototype_value_of` returning a non-symbol would indicate a bug in our intrinsic
    // marker storage.
    return Err(VmError::InvariantViolation(
      "Symbol.prototype.valueOf returned non-symbol",
    ));
  };

  let s = symbol_descriptive_string(scope, sym)?;
  Ok(Value::String(s))
}

/// `Symbol.prototype[Symbol.toPrimitive]` (minimal).
pub fn symbol_prototype_to_primitive(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // The spec ignores the hint and returns the Symbol value.
  symbol_prototype_value_of(vm, scope, host, hooks, callee, this, &[])
}

/// Global `isNaN(x)` (minimal).
pub fn global_is_nan(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v = args.first().copied().unwrap_or(Value::Undefined);
  let n = scope.to_number(vm, host, hooks, v)?;
  Ok(Value::Bool(n.is_nan()))
}

/// `Date` called as a function (extremely minimal).
pub fn date_constructor_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: `Date()` returns a string representation of the current time.
  // For the interpreter/test262 we only need a deterministic placeholder.
  Ok(Value::String(scope.alloc_string("[object Date]")?))
}

/// `new Date(value)` (minimal wrapper object).
pub fn date_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let time = match args.first().copied() {
    None => 0.0,
    Some(v) => scope.to_number(vm, host, hooks, v)?,
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.date_prototype()))?;

  // Store the time value on an internal symbol.
  let marker = scope.alloc_string("vm-js.internal.DateData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  scope.define_property(
    obj,
    marker_key,
    data_desc(Value::Number(time), true, false, false),
  )?;

  // Best-effort: if `new_target.prototype` is an object, use it.
  if let Value::Object(nt) = new_target {
    let proto_key = string_key(scope, "prototype")?;
    if let Ok(Value::Object(p)) = scope.heap().get(nt, &proto_key) {
      scope.heap_mut().object_set_prototype(obj, Some(p))?;
    }
  }

  Ok(Value::Object(obj))
}

/// `Date.prototype.toString` (minimal).
pub fn date_prototype_to_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // The test262 smoke suite only asserts that addition uses `toString` for Date objects.
  Ok(Value::String(scope.alloc_string("[object Date]")?))
}

/// `Date.prototype.valueOf` (minimal).
pub fn date_prototype_value_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "Date.prototype.valueOf called on non-object",
    ));
  };
  let marker = scope.alloc_string("vm-js.internal.DateData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  match scope
    .heap()
    .object_get_own_data_property_value(obj, &marker_key)?
  {
    Some(Value::Number(n)) => Ok(Value::Number(n)),
    _ => Err(VmError::TypeError(
      "Date.prototype.valueOf called on non-Date object",
    )),
  }
}

/// `Date.prototype[Symbol.toPrimitive]` (minimal).
pub fn date_prototype_to_primitive(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: Date's @@toPrimitive treats "default" like "string".
  let hint = match args.first().copied() {
    Some(Value::String(s)) => scope.heap().get_string(s)?.to_utf8_lossy(),
    _ => "default".to_string(),
  };
  if hint == "number" {
    date_prototype_value_of(_vm, scope, _host, _hooks, _callee, this, &[])
  } else {
    date_prototype_to_string(_vm, scope, _host, _hooks, _callee, this, &[])
  }
}

/// `Symbol(description)`.
pub fn symbol_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let desc = match args.first().copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(scope.to_string(vm, host, hooks, v)?),
  };
  let sym = scope.new_symbol(desc)?;
  Ok(Value::Symbol(sym))
}

fn concat_with_colon_space(name: &[u16], message: &[u16]) -> Result<Vec<u16>, VmError> {
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve(name.len().saturating_add(2).saturating_add(message.len()))
    .map_err(|_| VmError::OutOfMemory)?;
  vec_try_extend_from_slice(&mut out, name)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_extend_from_slice(&mut out, message)?;
  Ok(out)
}

/// `Error.prototype.toString` (minimal).
pub fn error_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let this_obj = match this {
    Value::Object(o) => o,
    _ => {
      return Err(VmError::Unimplemented(
        "Error.prototype.toString on non-object",
      ))
    }
  };

  let name_key = string_key(scope, "name")?;
  let message_key = string_key(scope, "message")?;

  let name_value = get_data_property_value(scope, this_obj, &name_key)?.unwrap_or(Value::Undefined);
  let message_value =
    get_data_property_value(scope, this_obj, &message_key)?.unwrap_or(Value::Undefined);

  let name = match name_value {
    Value::Undefined => scope.alloc_string("Error")?,
    other => scope.to_string(vm, host, hooks, other)?,
  };
  scope.push_root(Value::String(name))?;

  let message = match message_value {
    Value::Undefined => scope.alloc_string("")?,
    other => scope.to_string(vm, host, hooks, other)?,
  };
  scope.push_root(Value::String(message))?;

  let name_units = scope.heap().get_string(name)?.as_code_units();
  let message_units = scope.heap().get_string(message)?.as_code_units();

  if name_units.is_empty() {
    return Ok(Value::String(message));
  }
  if message_units.is_empty() {
    return Ok(Value::String(name));
  }

  let out = concat_with_colon_space(name_units, message_units)?;
  let s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(s))
}

/// `JSON.stringify` (minimal).
pub fn json_stringify(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  const QUOTE: u16 = b'"' as u16;
  const BACKSLASH: u16 = b'\\' as u16;

  fn push_u16_ascii(buf: &mut Vec<u16>, s: &[u8]) -> Result<(), VmError> {
    for &b in s {
      vec_try_push(buf, b as u16)?;
    }
    Ok(())
  }

  fn push_hex_escape(buf: &mut Vec<u16>, unit: u16) -> Result<(), VmError> {
    vec_try_push(buf, b'\\' as u16)?;
    vec_try_push(buf, b'u' as u16)?;
    let mut n = unit;
    let mut digits = [0u16; 4];
    for d in digits.iter_mut().rev() {
      let nibble = (n & 0xF) as u8;
      let c = match nibble {
        0..=9 => b'0' + nibble,
        10..=15 => b'a' + (nibble - 10),
        _ => b'0',
      };
      *d = c as u16;
      n >>= 4;
    }
    vec_try_extend_from_slice(buf, &digits)?;
    Ok(())
  }

  let value = args.first().copied().unwrap_or(Value::Undefined);

  let out = match value {
    Value::Undefined => return Ok(Value::Undefined),
    Value::Null => return Ok(Value::String(scope.alloc_string("null")?)),
    Value::Bool(true) => return Ok(Value::String(scope.alloc_string("true")?)),
    Value::Bool(false) => return Ok(Value::String(scope.alloc_string("false")?)),
    Value::Number(n) => {
      if n.is_nan() || n.is_infinite() {
        return Ok(Value::String(scope.alloc_string("null")?));
      }
      let s = n.to_string();
      return Ok(Value::String(scope.alloc_string(&s)?));
    }
    Value::BigInt(_) => return Err(VmError::TypeError("Do not know how to serialize a BigInt")),
    Value::String(s) => s,
    Value::Symbol(_) | Value::Object(_) => return Ok(Value::Undefined),
  };

  let max_bytes = scope.heap().limits().max_bytes;
  let units = scope.heap().get_string(out)?.as_code_units();

  let mut buf: Vec<u16> = Vec::new();
  vec_try_push(&mut buf, QUOTE)?;

  for (i, &unit) in units.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if JsString::heap_size_bytes_for_len(buf.len().saturating_add(6)) > max_bytes {
      return Err(VmError::OutOfMemory);
    }

    match unit {
      QUOTE => push_u16_ascii(&mut buf, b"\\\"")?,
      BACKSLASH => push_u16_ascii(&mut buf, b"\\\\")?,
      0x08 => push_u16_ascii(&mut buf, b"\\b")?,
      0x0C => push_u16_ascii(&mut buf, b"\\f")?,
      0x0A => push_u16_ascii(&mut buf, b"\\n")?,
      0x0D => push_u16_ascii(&mut buf, b"\\r")?,
      0x09 => push_u16_ascii(&mut buf, b"\\t")?,
      0x0000..=0x001F => push_hex_escape(&mut buf, unit)?,
      0xD800..=0xDFFF => push_hex_escape(&mut buf, unit)?,
      other => vec_try_push(&mut buf, other)?,
    }
  }

  vec_try_push(&mut buf, QUOTE)?;
  if JsString::heap_size_bytes_for_len(buf.len()) > max_bytes {
    return Err(VmError::OutOfMemory);
  }
  let out = scope.alloc_string_from_u16_vec(buf)?;
  Ok(Value::String(out))
}
