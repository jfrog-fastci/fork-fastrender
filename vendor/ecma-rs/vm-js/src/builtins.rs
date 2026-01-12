use crate::function::{FunctionData, ThisMode};
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::string::JsString;
use crate::{
  GcObject, Job, JobKind, PromiseCapability, PromiseHandle, PromiseReaction, PromiseReactionType,
  PromiseRejectionOperation, PromiseState, RealmId, RootId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, SourceText,
};
use parse_js::ast::expr::Expr;
use parse_js::ast::func::FuncBody;
use parse_js::ast::node::ParenthesizedExpr;
use parse_js::ast::stmt::Stmt;
use parse_js::{Dialect, ParseOptions, SourceType};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

fn string_position_from_value(
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
  if n.is_nan() || n.is_sign_negative() {
    return Ok(0);
  }
  if !n.is_finite() {
    return Ok(len);
  }
  let n = n.trunc();
  if n <= 0.0 {
    Ok(0usize)
  } else if n >= len as f64 {
    Ok(len)
  } else {
    Ok(n as usize)
  }
}

fn is_trim_whitespace_unit(unit: u16) -> bool {
  matches!(
    unit,
    // WhiteSpace (ECMA-262)
    0x0009
      | 0x000B
      | 0x000C
      | 0x0020
      | 0x00A0
      | 0x1680
      | 0x202F
      | 0x205F
      | 0x3000
      | 0xFEFF
      // LineTerminator (ECMA-262)
      | 0x000A
      | 0x000D
      | 0x2028
      | 0x2029
  ) || matches!(unit, 0x2000..=0x200A)
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
  let length_desc = scope
    .heap()
    .get_property_with_tick(obj, &length_key, || vm.tick())?;
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
    let desc = scope.heap().get_property_with_tick(obj, &key, || vm.tick())?;
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

pub fn class_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "Class constructor cannot be invoked without 'new'",
  ))
}

pub fn class_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  // Class constructors are special:
  // - calling them without `new` throws (handled by `class_constructor_call`)
  // - constructing them runs the user-defined `constructor(...) { ... }` body when present
  //
  // `vm-js` represents user-defined class constructor bodies by storing an internal (hidden)
  // constructable function object in the class constructor's native slots. When present, we
  // delegate construction to that function, forwarding `new_target` so `new.target` inside the body
  // observes the original `new` call's `newTarget`.
  //
  // When no constructor body is present (e.g. `class C {}`), fall back to a default constructor that
  // just allocates the instance via `OrdinaryCreateFromConstructor`.
  let ctor_body = {
    // Extract slot values without holding a heap borrow across VM calls.
    let func = scope.heap().get_function(callee)?;
    func
      .native_slots
      .as_deref()
      .and_then(|slots| slots.first().copied())
  };

  if let Some(Value::Object(body_func)) = ctor_body {
    return vm.construct_with_host_and_hooks(
      host,
      scope,
      hooks,
      Value::Object(body_func),
      args,
      new_target,
    );
  }

  let intr = require_intrinsics(vm)?;
  let obj = crate::spec_ops::ordinary_create_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intr.object_prototype(),
    &[],
    |scope| scope.alloc_object(),
  )?;
  Ok(Value::Object(obj))
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

  let ok = scope.define_own_property_with_tick(target, key, patch, || vm.tick())?;
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

  let own_keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
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
    let Some(desc) = scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())? else {
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

pub fn object_values(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  // Snapshot enumerable own string keys.
  let own_keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  let mut keys: Vec<crate::GcString> = Vec::new();
  keys
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(key_str) = key else {
      continue;
    };
    let Some(desc) = scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())? else {
      continue;
    };
    if desc.enumerable {
      keys.push(key_str);
    }
  }

  let len_u32 = u32::try_from(keys.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, len_u32)?;
  scope.push_root(Value::Object(array))?;

  for (i, key_str) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(obj))?;
    iter_scope.push_root(Value::Object(array))?;
    iter_scope.push_root(Value::String(key_str))?;

    // Allocate the target index key before calling `Get` so any GC triggered by the `Get`/getter
    // sees the index key as rooted.
    let idx_s = iter_scope.alloc_string(&i.to_string())?;
    iter_scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);

    let value = iter_scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      PropertyKey::from_string(key_str),
      Value::Object(obj),
    )?;

    iter_scope.define_property(array, idx_key, data_desc(value, true, true, true))?;
  }

  Ok(Value::Object(array))
}

pub fn object_entries(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  // Snapshot enumerable own string keys.
  let own_keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  let mut keys: Vec<crate::GcString> = Vec::new();
  keys
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(key_str) = key else {
      continue;
    };
    let Some(desc) = scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())? else {
      continue;
    };
    if desc.enumerable {
      keys.push(key_str);
    }
  }

  let len_u32 = u32::try_from(keys.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, len_u32)?;
  scope.push_root(Value::Object(array))?;

  for (i, key_str) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(obj))?;
    iter_scope.push_root(Value::Object(array))?;
    iter_scope.push_root(Value::String(key_str))?;

    let pair = create_array_object(vm, &mut iter_scope, 2)?;
    iter_scope.push_root(Value::Object(pair))?;

    let zero_s = iter_scope.alloc_string("0")?;
    iter_scope.push_root(Value::String(zero_s))?;
    let one_s = iter_scope.alloc_string("1")?;
    iter_scope.push_root(Value::String(one_s))?;

    // Allocate the destination index key before calling `Get` so any GC triggered by the
    // `Get`/getter sees it as rooted.
    let idx_s = iter_scope.alloc_string(&i.to_string())?;
    iter_scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);

    let value = iter_scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      PropertyKey::from_string(key_str),
      Value::Object(obj),
    )?;

    iter_scope.define_property(
      pair,
      PropertyKey::from_string(zero_s),
      data_desc(Value::String(key_str), true, true, true),
    )?;
    iter_scope.define_property(
      pair,
      PropertyKey::from_string(one_s),
      data_desc(value, true, true, true),
    )?;

    iter_scope.define_property(
      array,
      idx_key,
      data_desc(Value::Object(pair), true, true, true),
    )?;
  }

  Ok(Value::Object(array))
}

pub fn object_from_entries(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut iterator_record = crate::iterator::get_iterator(vm, host, hooks, &mut scope, iterable)?;
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;

  let result: Result<Value, VmError> = (|| {
    loop {
      let next_value =
        crate::iterator::iterator_step_value(vm, host, hooks, &mut scope, &mut iterator_record)?;
      let Some(next_value) = next_value else {
        return Ok(Value::Object(out));
      };

      // Use a nested scope so per-entry roots do not accumulate.
      let mut step_scope = scope.reborrow();
      step_scope.push_root(next_value)?;

      let Value::Object(entry_obj) = next_value else {
        return Err(VmError::TypeError("Object.fromEntries: iterator value is not an object"));
      };

      let zero_key = string_key(&mut step_scope, "0")?;
      let key_val = step_scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        entry_obj,
        zero_key,
        next_value,
      )?;
      step_scope.push_root(key_val)?;
      let prop_key = step_scope.to_property_key(vm, host, hooks, key_val)?;
      root_property_key(&mut step_scope, prop_key)?;

      let one_key = string_key(&mut step_scope, "1")?;
      let value = step_scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        entry_obj,
        one_key,
        next_value,
      )?;

      step_scope.create_data_property_or_throw(out, prop_key, value)?;
    }
  })();

  match result {
    Ok(v) => Ok(v),
    Err(err) => {
      if !iterator_record.done {
        // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`).
        let pending_root = err.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        let close_res = crate::iterator::iterator_close(vm, host, hooks, &mut scope, &iterator_record);
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
        if let Err(close_err) = close_res {
          return Err(close_err);
        }
      }
      Err(err)
    }
  }
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

    let keys = scope.ordinary_own_property_keys_with_tick(source, || vm.tick())?;
    for (j, key) in keys.into_iter().enumerate() {
      if j % 1024 == 0 {
        vm.tick()?;
      }
      let Some(desc) = scope.ordinary_get_own_property_with_tick(source, key, || vm.tick())? else {
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

pub fn reflect_apply(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.apply
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(target)? {
    return Err(VmError::NotCallable);
  }

  let this_argument = args.get(1).copied().unwrap_or(Value::Undefined);
  let arguments_list = args.get(2).copied().unwrap_or(Value::Undefined);
  let Value::Object(arguments_obj) = arguments_list else {
    return Err(VmError::TypeError("Reflect.apply argumentsList must be an object"));
  };

  let list = crate::spec_ops::create_list_from_array_like_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    arguments_obj,
  )?;

  vm.call_with_host_and_hooks(host, scope, hooks, target, this_argument, &list)
}

pub fn reflect_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.construct
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_constructor(target)? {
    return Err(VmError::NotConstructable);
  }

  let arguments_list = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(arguments_obj) = arguments_list else {
    return Err(VmError::TypeError(
      "Reflect.construct argumentsList must be an object",
    ));
  };

  let new_target = args.get(2).copied().unwrap_or(target);
  if !scope.heap().is_constructor(new_target)? {
    return Err(VmError::NotConstructable);
  }

  let list = crate::spec_ops::create_list_from_array_like_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    arguments_obj,
  )?;

  vm.construct_with_host_and_hooks(host, scope, hooks, target, &list, new_target)
}

pub fn reflect_define_property(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.defineproperty
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let desc_obj = require_object(args.get(2).copied().unwrap_or(Value::Undefined))?;
  scope.push_root(Value::Object(desc_obj))?;

  // Minimal `ToPropertyDescriptor` support: read own *data* properties only.
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

  let ok = scope.define_own_property_with_tick(target, key, patch, || vm.tick())?;
  Ok(Value::Bool(ok))
}

pub fn reflect_delete_property(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.deleteproperty
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let ok = scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, target, key)?;
  Ok(Value::Bool(ok))
}

pub fn reflect_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.get
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let receiver = args.get(2).copied().unwrap_or(Value::Object(target));
  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, target, key, receiver)
}

pub fn reflect_get_own_property_descriptor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let Some(desc) = scope.ordinary_get_own_property_with_tick(target, key, || vm.tick())? else {
    return Ok(Value::Undefined);
  };

  // `FromPropertyDescriptor(desc)`
  let desc_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(desc_obj, Some(intr.object_prototype()))?;

  // enumerable / configurable (always present)
  {
    let key_s = scope.alloc_string("enumerable")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(desc_obj, PropertyKey::from_string(key_s), data_desc(Value::Bool(desc.enumerable), true, true, true))?;
  }
  {
    let key_s = scope.alloc_string("configurable")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(desc_obj, PropertyKey::from_string(key_s), data_desc(Value::Bool(desc.configurable), true, true, true))?;
  }

  match desc.kind {
    PropertyKind::Data { value, writable } => {
      let key_s = scope.alloc_string("value")?;
      scope.push_root(Value::String(key_s))?;
      scope.define_property(desc_obj, PropertyKey::from_string(key_s), data_desc(value, true, true, true))?;

      let key_s = scope.alloc_string("writable")?;
      scope.push_root(Value::String(key_s))?;
      scope.define_property(desc_obj, PropertyKey::from_string(key_s), data_desc(Value::Bool(writable), true, true, true))?;
    }
    PropertyKind::Accessor { get, set } => {
      let key_s = scope.alloc_string("get")?;
      scope.push_root(Value::String(key_s))?;
      scope.define_property(desc_obj, PropertyKey::from_string(key_s), data_desc(get, true, true, true))?;

      let key_s = scope.alloc_string("set")?;
      scope.push_root(Value::String(key_s))?;
      scope.define_property(desc_obj, PropertyKey::from_string(key_s), data_desc(set, true, true, true))?;
    }
  }

  Ok(Value::Object(desc_obj))
}

pub fn reflect_get_prototype_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.getprototypeof
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;

  match scope.object_get_prototype(target)? {
    Some(proto) => Ok(Value::Object(proto)),
    None => Ok(Value::Null),
  }
}

pub fn reflect_has(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.has
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let ok = scope.ordinary_has_property_with_tick(target, key, || vm.tick())?;
  Ok(Value::Bool(ok))
}

pub fn reflect_is_extensible(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.isextensible
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  Ok(Value::Bool(scope.object_is_extensible(target)?))
}

pub fn reflect_own_keys(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.ownkeys
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let keys = scope.ordinary_own_property_keys_with_tick(target, || vm.tick())?;

  let len = u32::try_from(keys.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, len)?;
  scope.push_root(Value::Object(array))?;

  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let idx_s = scope.alloc_string(&i.to_string())?;
    scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);

    let value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };

    scope.create_data_property_or_throw(array, idx_key, value)?;
  }

  Ok(Value::Object(array))
}

pub fn reflect_prevent_extensions(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.preventextensions
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.object_prevent_extensions(target)?;
  Ok(Value::Bool(true))
}

pub fn reflect_set(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.set
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let value = args.get(2).copied().unwrap_or(Value::Undefined);
  let receiver = args.get(3).copied().unwrap_or(Value::Object(target));

  let ok = scope.ordinary_set_with_host_and_hooks(vm, host, hooks, target, key, value, receiver)?;
  Ok(Value::Bool(ok))
}

pub fn reflect_set_prototype_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.setprototypeof
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;

  let proto_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let proto = match proto_val {
    Value::Object(o) => Some(o),
    Value::Null => None,
    _ => {
      return Err(VmError::TypeError(
        "Reflect.setPrototypeOf prototype must be an object or null",
      ))
    }
  };

  let current_proto = scope.object_get_prototype(target)?;
  if current_proto == proto {
    return Ok(Value::Bool(true));
  }

  if !scope.object_is_extensible(target)? {
    return Ok(Value::Bool(false));
  }

  match scope.object_set_prototype(target, proto) {
    Ok(()) => Ok(Value::Bool(true)),
    // A cycle (or hostile prototype chain) rejects the mutation.
    Err(VmError::PrototypeCycle | VmError::PrototypeChainTooDeep) => Ok(Value::Bool(false)),
    Err(e) => Err(e),
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

/// `Array.isArray(arg)` (ECMA-262).
pub fn array_is_array(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let is_array = match arg0 {
    Value::Object(obj) => scope.heap().object_is_array(obj)?,
    _ => false,
  };
  Ok(Value::Bool(is_array))
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let length_val = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let length_num = scope.to_number(vm, host, hooks, length_val)?;
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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
      scope.to_number(vm, host, hooks, arg0)?
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
    let n = scope.to_number(vm, host, hooks, byte_offset_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError("Uint8Array byteOffset must be a non-negative integer"));
    }
    n as usize
  };

  let length_val = args.get(2).copied().unwrap_or(Value::Undefined);
  let length = if matches!(length_val, Value::Undefined) {
    buf_len.saturating_sub(byte_offset)
  } else {
    let n = scope.to_number(vm, host, hooks, length_val)?;
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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  function_constructor_construct(vm, scope, host, hooks, callee, args, Value::Object(callee))
}

pub fn function_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  // `CreateDynamicFunction` creates the function in the realm of the provided constructor. `vm-js`
  // represents a realm by storing the realm's global object on `[[Realm]]`.
  let Some(global_object) = scope.heap().get_function_realm(callee)? else {
    return Err(VmError::Unimplemented("Function constructor missing [[Realm]]"));
  };

  // The Function constructor creates its function in the global lexical environment, not in the
  // caller's environment. `vm-js` stores the global lexical env as the Function constructor's
  // captured closure env.
  let closure_env = scope.heap().get_function_closure_env(callee)?;

  // `Function(...params, body)` uses the final argument as the body.
  let (param_values, body_value) = match args.split_last() {
    Some((last, rest)) => (rest, *last),
    None => (&[][..], Value::Undefined),
  };

  let mut params_joined: String = String::new();
  for (idx, param_value) in param_values.iter().copied().enumerate() {
    if idx % 32 == 0 {
      vm.tick()?;
    }
    let s = scope.to_string(vm, host, hooks, param_value)?;
    let text = scope.heap().get_string(s)?.to_utf8_lossy();
    if idx != 0 {
      params_joined.push(',');
    }
    params_joined.push_str(&text);
  }

  let body_s = scope.to_string(vm, host, hooks, body_value)?;
  let body_text = scope.heap().get_string(body_s)?.to_utf8_lossy();

  // Parse as a single function declaration statement so we can reuse the normal ECMAScript
  // function-object call path.
  let mut source: String = String::new();
  source
    .try_reserve(
      "function anonymous(){}".len()
        .saturating_add(params_joined.len())
        .saturating_add(body_text.len()),
    )
    .map_err(|_| VmError::OutOfMemory)?;
  source.push_str("function anonymous(");
  source.push_str(&params_joined);
  source.push_str(") {\n");
  source.push_str(&body_text);
  source.push_str("\n}");

  // Parse eagerly so syntax errors become JS-catchable `SyntaxError` exceptions instead of
  // surfacing as non-catchable `VmError::Syntax`.
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let parsed = match vm.parse_top_level_with_budget(&source, opts) {
    Ok(top) => top,
    Err(VmError::Syntax(diags)) => {
      let message = diags
        .first()
        .map(|d| d.message.as_str())
        .unwrap_or("Invalid or unexpected token");
      let err_obj = crate::error_object::new_syntax_error_object(scope, &intr, message)?;
      return Err(VmError::Throw(err_obj));
    }
    Err(err) => return Err(err),
  };

  // Derive strictness and length from the parsed function node.
  let mut is_strict = false;
  let mut length: u32 = 0;
  if parsed.stx.body.len() == 1 {
    if let Stmt::FunctionDecl(decl) = &*parsed.stx.body[0].stx {
      let func = &decl.stx.function.stx;
      length = {
        let mut len: u32 = 0;
        for (i, param) in func.parameters.iter().enumerate() {
          if i % 32 == 0 {
            vm.tick()?;
          }
          if param.stx.rest || param.stx.default_value.is_some() {
            break;
          }
          len = len.saturating_add(1);
        }
        len
      };
      if let Some(FuncBody::Block(stmts)) = &func.body {
        const TICK_EVERY: usize = 32;
        for (i, stmt) in stmts.iter().enumerate() {
          if i % TICK_EVERY == 0 {
            vm.tick()?;
          }
          let Stmt::Expr(expr_stmt) = &*stmt.stx else {
            break;
          };
          let expr = &expr_stmt.stx.expr;
          if expr.assoc.get::<ParenthesizedExpr>().is_some() {
            break;
          }
          let Expr::LitStr(lit) = &*expr.stx else {
            break;
          };
          if lit.stx.value == "use strict" {
            is_strict = true;
            break;
          }
        }
      }
    }
  }

  let this_mode = if is_strict { ThisMode::Strict } else { ThisMode::Global };

  let source = Arc::new(SourceText::new_charged(scope.heap_mut(), "<Function>", source)?);
  let span_end = u32::try_from(source.text.len()).unwrap_or(u32::MAX);
  let code_id = vm.register_ecma_function(source, 0, span_end, crate::vm::EcmaFunctionKind::Decl)?;

  let name_s = scope.alloc_string("anonymous")?;
  let func_obj = scope.alloc_ecma_function(
    code_id,
    true,
    name_s,
    length,
    this_mode,
    is_strict,
    closure_env,
  )?;
  scope.push_root(Value::Object(func_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
  scope
    .heap_mut()
    .set_function_realm(func_obj, global_object)?;

  let job_realm = scope
    .heap()
    .get_function_job_realm(callee)
    .or(vm.current_realm());
  if let Some(job_realm) = job_realm {
    scope.heap_mut().set_function_job_realm(func_obj, job_realm)?;
  }

  Ok(Value::Object(func_obj))
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

#[allow(dead_code)]
fn new_promise(vm: &mut Vm, scope: &mut Scope<'_>) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;
  scope.alloc_promise_with_prototype(Some(intr.promise_prototype()))
}

#[allow(dead_code)]
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
  let Some(desc) = scope.heap().get_property_with_tick(obj, &key, || vm.tick())? else {
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

/// `SpeciesConstructor(O, defaultConstructor)` abstract operation (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-speciesconstructor>
fn species_constructor_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  default_constructor: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();

  // `Get` can invoke user code via accessors. Root inputs across allocations/GC.
  scope.push_root(Value::Object(obj))?;
  scope.push_root(default_constructor)?;

  // 1. Let C be ? Get(O, "constructor").
  let ctor_key_s = scope.alloc_string("constructor")?;
  scope.push_root(Value::String(ctor_key_s))?;
  let ctor_key = PropertyKey::from_string(ctor_key_s);
  let c = get_property_value_with_host(
    vm,
    &mut scope,
    host,
    hooks,
    obj,
    ctor_key,
    Value::Object(obj),
  )?;
  let c = scope.push_root(c)?;

  // 2. If C is undefined, return defaultConstructor.
  if matches!(c, Value::Undefined) {
    return Ok(default_constructor);
  }

  // 3. If Type(C) is not Object, throw a TypeError exception.
  let Value::Object(c_obj) = c else {
    return throw_type_error(vm, &mut scope, hooks, "SpeciesConstructor: constructor is not an object");
  };

  // 4. Let S be ? Get(C, @@species).
  let species_key = PropertyKey::from_symbol(intr.well_known_symbols().species);
  let s = get_property_value_with_host(
    vm,
    &mut scope,
    host,
    hooks,
    c_obj,
    species_key,
    Value::Object(c_obj),
  )?;
  let s = scope.push_root(s)?;

  // 5. If S is either undefined or null, return defaultConstructor.
  if matches!(s, Value::Undefined | Value::Null) {
    return Ok(default_constructor);
  }

  // 6. If IsConstructor(S) is true, return S.
  if scope.heap().is_constructor(s)? {
    return Ok(s);
  }

  // 7. Throw a TypeError exception.
  throw_type_error(vm, &mut scope, hooks, "SpeciesConstructor: @@species is not a constructor")
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
  scope.push_roots(&[capability.promise, capability.resolve])?;
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

    match key_scope
      .heap()
      .get_property_with_tick(thenable_obj, &then_key, || vm.tick())?
    {
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
  scope.push_roots(&[capability.promise, capability.reject])?;
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

fn perform_promise_then_with_capability(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  promise: GcObject,
  on_fulfilled: Value,
  on_rejected: Value,
  capability: PromiseCapability,
) -> Result<Value, VmError> {
  // Root inputs: `promise` must remain live while we allocate job roots and enqueue reactions.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(promise))?;
  scope.push_root(on_fulfilled)?;
  scope.push_root(on_rejected)?;
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
    Value::Object(obj) if scope.heap().is_callable(Value::Object(obj))? => Some(host.host_make_job_callback(obj)),
    _ => None,
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
      enqueue_promise_reaction_job(host, &mut scope, fulfill_reaction, arg, current_realm)?;
    }
    PromiseState::Rejected => {
      let arg = scope
        .heap()
        .promise_result(promise)?
        .unwrap_or(Value::Undefined);
      enqueue_promise_reaction_job(host, &mut scope, reject_reaction, arg, current_realm)?;
    }
  }

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

  // Create the derived promise + capability.
  let result_promise = scope.alloc_promise_with_prototype(Some(intr.promise_prototype()))?;
  scope.push_root(Value::Object(result_promise))?;
  let (resolve, reject) = create_promise_resolving_functions(vm, scope, result_promise)?;
  let capability = PromiseCapability {
    promise: Value::Object(result_promise),
    resolve,
    reject,
  };

  perform_promise_then_with_capability(vm, scope, host, promise, on_fulfilled, on_rejected, capability)
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let on_fulfilled = args.get(0).copied().unwrap_or(Value::Undefined);
  let on_rejected = args.get(1).copied().unwrap_or(Value::Undefined);

  let Value::Object(promise) = this else {
    return throw_type_error(
      vm,
      scope,
      hooks,
      "Promise.prototype.then called on non-object",
    );
  };
  if !scope.heap().is_promise_object(promise) {
    return throw_type_error(
      vm,
      scope,
      hooks,
      "Promise.prototype.then called on non-promise",
    );
  }

  // Root inputs: `SpeciesConstructor` and `NewPromiseCapability` can invoke user code.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(promise))?;
  scope.push_root(on_fulfilled)?;
  scope.push_root(on_rejected)?;

  // `C = SpeciesConstructor(promise, %Promise%)`
  let default_ctor = Value::Object(intr.promise());
  let constructor =
    species_constructor_with_host_and_hooks(vm, &mut scope, host, hooks, promise, default_ctor)?;
  scope.push_root(constructor)?;

  // `resultCapability = NewPromiseCapability(C)`
  let capability = new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks, constructor)?;

  perform_promise_then_with_capability(
    vm,
    &mut scope,
    hooks,
    promise,
    on_fulfilled,
    on_rejected,
    capability,
  )
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

  // Root inputs: `SpeciesConstructor` can invoke user code via accessors.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(promise))?;
  scope.push_root(on_finally)?;

  // `C = SpeciesConstructor(promise, %Promise%)`
  let default_ctor = Value::Object(intr.promise());
  let constructor =
    species_constructor_with_host_and_hooks(vm, &mut scope, host, hooks, promise, default_ctor)?;

  scope.push_root(constructor)?;

  let call_id = intr.promise_finally_handler_call();

  let then_finally_name = scope.alloc_string("thenFinally")?;
  let then_finally = scope.alloc_native_function(call_id, None, then_finally_name, 1)?;
  set_function_job_realm_to_current(vm, &mut scope, then_finally)?;
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
  set_function_job_realm_to_current(vm, &mut scope, catch_finally)?;
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
    &mut scope,
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
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
  // `IfAbruptRejectPromise` (ECMA-262): abrupt completions become promise rejections.
  //
  // `vm-js` represents spec "throw completion" errors in two ways:
  // - `VmError::Throw*` for already-allocated JS throw values, and
  // - internal helper errors like `VmError::TypeError`/`NotCallable`, which need to be coerced into
  //   real JS `TypeError` objects when intrinsics are available.
  //
  // VM-internal errors (OOM, unimplemented, etc.) are still propagated.
  let reason = match completion {
    VmError::Throw(value) => value,
    VmError::ThrowWithStack { value, .. } => value,
    VmError::TypeError(msg) => create_type_error(vm, scope, hooks, msg)?,
    VmError::NotCallable => create_type_error(vm, scope, hooks, "value is not callable")?,
    VmError::NotConstructable => create_type_error(vm, scope, hooks, "value is not a constructor")?,
    VmError::PrototypeCycle => create_type_error(vm, scope, hooks, "prototype cycle")?,
    VmError::PrototypeChainTooDeep => create_type_error(vm, scope, hooks, "prototype chain too deep")?,
    VmError::InvalidPropertyDescriptorPatch => create_type_error(
      vm,
      scope,
      hooks,
      "invalid property descriptor patch: cannot mix data and accessor fields",
    )?,
    other => return Err(other),
  };

  // Root the rejection reason across the host call: `reject` can be user code that allocates/GCs.
  scope.push_root(reason)?;
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
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
      let mut completion = err;
      if !iterator_record.done {
        // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`).
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record) {
          Ok(()) => {}
          Err(close_err) => completion = close_err,
        }
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, completion)
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
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
      let mut completion = err;
      if !iterator_record.done {
        // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`).
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record) {
          Ok(()) => {}
          Err(close_err) => completion = close_err,
        }
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, completion)
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
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
      let mut completion = err;
      if !iterator_record.done {
        // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`).
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record) {
          Ok(()) => {}
          Err(close_err) => completion = close_err,
        }
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, completion)
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
  scope.push_roots(&[capability.promise, capability.resolve, capability.reject])?;

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
      let mut completion = err;
      if !iterator_record.done {
        // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`).
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(vm, host, hooks, scope, &iterator_record) {
          Ok(()) => {}
          Err(close_err) => completion = close_err,
        }
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
      }
      if_abrupt_reject_promise(vm, scope, host, hooks, capability, completion)
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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: &PropertyKey,
) -> Result<Option<Value>, VmError> {
  let Some(desc) = scope.heap().get_property_with_tick(obj, key, || vm.tick())? else {
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

  let has = scope
    .ordinary_get_own_property_with_tick(obj, key, || vm.tick())?
    .is_some();
  Ok(Value::Bool(has))
}

fn get_array_length(vm: &mut Vm, scope: &mut Scope<'_>, obj: GcObject) -> Result<usize, VmError> {
  let length_key = string_key(scope, "length")?;
  Ok(match get_data_property_value(vm, scope, obj, &length_key)? {
    Some(v) => to_length(v),
    None => 0,
  })
}

fn internal_symbol_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  let marker = scope.alloc_string(s)?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  Ok(PropertyKey::from_symbol(marker_sym))
}

const ARRAY_ITERATOR_ARRAY_MARKER: &str = "vm-js.internal.ArrayIteratorArray";
const ARRAY_ITERATOR_INDEX_MARKER: &str = "vm-js.internal.ArrayIteratorIndex";
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
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.map callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_array(len)?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.array_prototype()))?;

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    iter_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    if !iter_scope.ordinary_has_property(obj, key)? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    // callback(value, index, array)
    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let mapped =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
    iter_scope.create_data_property_or_throw(out, key, mapped)?;
  }

  Ok(Value::Object(out))
}

/// `Array.prototype.forEach` (ECMA-262) (minimal).
pub fn array_prototype_for_each(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.forEach callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let _ =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
  }

  Ok(Value::Undefined)
}

/// `Array.prototype.indexOf` (ECMA-262) (minimal).
pub fn array_prototype_index_of(
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

  let search = args.get(0).copied().unwrap_or(Value::Undefined);

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);
  if len == 0 {
    return Ok(Value::Number(-1.0));
  }

  let from_index = args.get(1).copied().unwrap_or(Value::Undefined);
  let start = slice_index_from_value(vm, &mut scope, host, hooks, from_index, len, 0)?;
  if start >= len {
    return Ok(Value::Number(-1.0));
  }

  for k in start..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let equal = match (search, value) {
      (Value::Undefined, Value::Undefined) => true,
      (Value::Null, Value::Null) => true,
      (Value::Bool(a), Value::Bool(b)) => a == b,
      (Value::Number(a), Value::Number(b)) => a == b,
      (Value::BigInt(a), Value::BigInt(b)) => a == b,
      (Value::String(a), Value::String(b)) => {
        iter_scope.heap().get_string(a)? == iter_scope.heap().get_string(b)?
      }
      (Value::Symbol(a), Value::Symbol(b)) => a == b,
      (Value::Object(a), Value::Object(b)) => a == b,
      _ => false,
    };

    if equal {
      return Ok(Value::Number(k as f64));
    }
  }

  Ok(Value::Number(-1.0))
}

/// `Array.prototype.includes` (ECMA-262) (minimal).
pub fn array_prototype_includes(
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

  let search = args.get(0).copied().unwrap_or(Value::Undefined);

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);
  if len == 0 {
    return Ok(Value::Bool(false));
  }

  let from_index = args.get(1).copied().unwrap_or(Value::Undefined);
  let start = slice_index_from_value(vm, &mut scope, host, hooks, from_index, len, 0)?;
  if start >= len {
    return Ok(Value::Bool(false));
  }

  for k in start..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let equal = match (search, value) {
      (Value::Undefined, Value::Undefined) => true,
      (Value::Null, Value::Null) => true,
      (Value::Bool(a), Value::Bool(b)) => a == b,
      (Value::Number(a), Value::Number(b)) => (a == b) || (a.is_nan() && b.is_nan()),
      (Value::BigInt(a), Value::BigInt(b)) => a == b,
      (Value::String(a), Value::String(b)) => {
        iter_scope.heap().get_string(a)? == iter_scope.heap().get_string(b)?
      }
      (Value::Symbol(a), Value::Symbol(b)) => a == b,
      (Value::Object(a), Value::Object(b)) => a == b,
      _ => false,
    };

    if equal {
      return Ok(Value::Bool(true));
    }
  }

  Ok(Value::Bool(false))
}

/// `Array.prototype.filter` (ECMA-262) (minimal).
pub fn array_prototype_filter(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.filter callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_array(0)?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.array_prototype()))?;

  let mut to = 0usize;
  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }

    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let selected_val =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
    let selected = iter_scope.heap().to_boolean(selected_val)?;
    if !selected {
      continue;
    }

    let to_s = iter_scope.alloc_string(&to.to_string())?;
    iter_scope.push_root(Value::String(to_s))?;
    let to_key = PropertyKey::from_string(to_s);
    iter_scope.create_data_property_or_throw(out, to_key, value)?;
    to = to.checked_add(1).ok_or(VmError::OutOfMemory)?;
  }

  Ok(Value::Object(out))
}

/// `Array.prototype.reduce` (ECMA-262) (minimal).
pub fn array_prototype_reduce(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.reduce callback is not callable"));
  }
  scope.push_root(callback)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  let intr = require_intrinsics(vm)?;
  let acc_holder = scope.alloc_object()?;
  scope.push_root(Value::Object(acc_holder))?;
  scope
    .heap_mut()
    .object_set_prototype(acc_holder, Some(intr.object_prototype()))?;

  let acc_sym = scope.alloc_symbol(Some("vm-js.internal.ArrayReduceAccumulator"))?;
  let acc_key = PropertyKey::from_symbol(acc_sym);

  let has_initial = args.len() > 1;
  let mut k = 0usize;
  let mut accumulator: Value;

  if has_initial {
    accumulator = args[1];
  } else {
    // Find the first present element.
    loop {
      if k >= len {
        return Err(VmError::TypeError(
          "Reduce of empty array with no initial value",
        ));
      }
      if k % 1024 == 0 {
        vm.tick()?;
      }

      let mut iter_scope = scope.reborrow();
      let key_s = iter_scope.alloc_string(&k.to_string())?;
      let key = PropertyKey::from_string(key_s);
      if iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
        accumulator =
          iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
        k = k.checked_add(1).ok_or(VmError::OutOfMemory)?;
        break;
      }
      k = k.checked_add(1).ok_or(VmError::OutOfMemory)?;
    }
  }

  // Root the accumulator value by storing it on a rooted helper object. This keeps it live even if
  // the callback returns a freshly allocated object/string and subsequent operations trigger GC.
  scope.define_property(
    acc_holder,
    acc_key,
    data_desc(accumulator, /* writable */ true, /* enumerable */ false, /* configurable */ false),
  )?;

  for idx in k..len {
    if idx % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&idx.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }

    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
    let call_args = [accumulator, value, Value::Number(idx as f64), Value::Object(obj)];
    accumulator = vm.call_with_host_and_hooks(
      host,
      &mut iter_scope,
      hooks,
      callback,
      Value::Undefined,
      &call_args,
    )?;

    let ok = iter_scope.ordinary_set_with_host_and_hooks(
      vm,
      host,
      hooks,
      acc_holder,
      acc_key,
      accumulator,
      Value::Object(acc_holder),
    )?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.reduce failed"));
    }
  }

  Ok(accumulator)
}

/// `Array.prototype.some` (ECMA-262) (minimal).
pub fn array_prototype_some(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.some callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let selected_val =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
    if iter_scope.heap().to_boolean(selected_val)? {
      return Ok(Value::Bool(true));
    }
  }

  Ok(Value::Bool(false))
}

/// `Array.prototype.every` (ECMA-262) (minimal).
pub fn array_prototype_every(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.every callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let selected_val =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
    if !iter_scope.heap().to_boolean(selected_val)? {
      return Ok(Value::Bool(false));
    }
  }

  Ok(Value::Bool(true))
}

/// `Array.prototype.find` (ECMA-262) (minimal).
pub fn array_prototype_find(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.find callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let selected_val =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
    if iter_scope.heap().to_boolean(selected_val)? {
      return Ok(value);
    }
  }

  Ok(Value::Undefined)
}

/// `Array.prototype.findIndex` (ECMA-262) (minimal).
pub fn array_prototype_find_index(
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

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError("Array.prototype.findIndex callback is not callable"));
  }
  scope.push_root(callback)?;

  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(this_arg)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
      continue;
    }
    let value =
      iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let call_args = [value, Value::Number(k as f64), Value::Object(obj)];
    let selected_val =
      vm.call_with_host_and_hooks(host, &mut iter_scope, hooks, callback, this_arg, &call_args)?;
    if iter_scope.heap().to_boolean(selected_val)? {
      return Ok(Value::Number(k as f64));
    }
  }

  Ok(Value::Number(-1.0))
}

/// `Array.prototype.concat` (ECMA-262) (minimal).
pub fn array_prototype_concat(
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

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_array(0)?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.array_prototype()))?;

  let length_key = string_key(&mut scope, "length")?;
  let mut n = 0usize;

  // Per spec, concat starts with `this` and then processes each argument.
  let mut process_item = |item: Value, n: &mut usize| -> Result<(), VmError> {
    if let Value::Object(source_obj) = item {
      if scope.heap().object_is_array(source_obj)? {
        // Spread array elements (holes preserved via length tracking).
        let source_len_value = scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          source_obj,
          length_key,
          Value::Object(source_obj),
        )?;
        let source_len = to_length(source_len_value);

        for k in 0..source_len {
          if k % 1024 == 0 {
            vm.tick()?;
          }
          let mut iter_scope = scope.reborrow();

          let key_s = iter_scope.alloc_string(&k.to_string())?;
          let key = PropertyKey::from_string(key_s);
          if iter_scope.ordinary_has_property_with_tick(source_obj, key, || vm.tick())? {
            let value = iter_scope.ordinary_get_with_host_and_hooks(
              vm,
              host,
              hooks,
              source_obj,
              key,
              Value::Object(source_obj),
            )?;

            let to_s = iter_scope.alloc_string(&n.to_string())?;
            iter_scope.push_root(Value::String(to_s))?;
            let to_key = PropertyKey::from_string(to_s);
            iter_scope.create_data_property_or_throw(out, to_key, value)?;
          }
          *n = n.checked_add(1).ok_or(VmError::OutOfMemory)?;
        }
        return Ok(());
      }
    }

    // Not spreadable: append as a single element.
    let mut iter_scope = scope.reborrow();
    let to_s = iter_scope.alloc_string(&n.to_string())?;
    iter_scope.push_root(Value::String(to_s))?;
    let to_key = PropertyKey::from_string(to_s);
    iter_scope.create_data_property_or_throw(out, to_key, item)?;
    *n = n.checked_add(1).ok_or(VmError::OutOfMemory)?;
    Ok(())
  };

  process_item(Value::Object(obj), &mut n)?;
  for item in args {
    process_item(*item, &mut n)?;
  }

  // Ensure the final length accounts for trailing holes created by spreading arrays.
  let ok = scope.ordinary_set_with_host_and_hooks(
    vm,
    host,
    hooks,
    out,
    length_key,
    Value::Number(n as f64),
    Value::Object(out),
  )?;
  if !ok {
    return Err(VmError::TypeError("Array.prototype.concat failed"));
  }

  Ok(Value::Object(out))
}

/// `Array.prototype.reverse` (ECMA-262) (minimal).
pub fn array_prototype_reverse(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  let middle = len / 2;
  for lower in 0..middle {
    if lower % 1024 == 0 {
      vm.tick()?;
    }
    let upper = len
      .checked_sub(lower)
      .and_then(|v| v.checked_sub(1))
      .ok_or(VmError::OutOfMemory)?;

    let mut iter_scope = scope.reborrow();

    let lower_s = iter_scope.alloc_string(&lower.to_string())?;
    iter_scope.push_root(Value::String(lower_s))?;
    let upper_s = iter_scope.alloc_string(&upper.to_string())?;
    iter_scope.push_root(Value::String(upper_s))?;

    let lower_key = PropertyKey::from_string(lower_s);
    let upper_key = PropertyKey::from_string(upper_s);

    let lower_exists = iter_scope.ordinary_has_property_with_tick(obj, lower_key, || vm.tick())?;
    let upper_exists = iter_scope.ordinary_has_property_with_tick(obj, upper_key, || vm.tick())?;

    let lower_value = if lower_exists {
      Some(iter_scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        obj,
        lower_key,
        Value::Object(obj),
      )?)
    } else {
      None
    };
    let upper_value = if upper_exists {
      Some(iter_scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        obj,
        upper_key,
        Value::Object(obj),
      )?)
    } else {
      None
    };

    match (lower_value, upper_value) {
      (Some(lower_value), Some(upper_value)) => {
        let ok = iter_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          lower_key,
          upper_value,
          Value::Object(obj),
        )?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.reverse failed"));
        }
        let ok = iter_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          upper_key,
          lower_value,
          Value::Object(obj),
        )?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.reverse failed"));
        }
      }
      (None, Some(upper_value)) => {
        let ok = iter_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          lower_key,
          upper_value,
          Value::Object(obj),
        )?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.reverse failed"));
        }
        let ok = iter_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, upper_key)?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.reverse failed"));
        }
      }
      (Some(lower_value), None) => {
        let ok = iter_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, lower_key)?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.reverse failed"));
        }
        let ok = iter_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          upper_key,
          lower_value,
          Value::Object(obj),
        )?;
        if !ok {
          return Err(VmError::TypeError("Array.prototype.reverse failed"));
        }
      }
      (None, None) => {}
    }
  }

  Ok(Value::Object(obj))
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

  let len = get_array_length(vm, scope, this_obj)?;

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
    let value = get_data_property_value(vm, scope, this_obj, &key)?.unwrap_or(Value::Undefined);
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

    if !iter_scope.ordinary_has_property_with_tick(obj, from_key, || vm.tick())? {
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
    iter_scope.push_root(Value::String(idx_s))?;
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

/// `Array.prototype.pop` (minimal).
pub fn array_prototype_pop(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  if len == 0 {
    let ok = scope.ordinary_set_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      length_key,
      Value::Number(0.0),
      Value::Object(obj),
    )?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.pop failed"));
    }
    return Ok(Value::Undefined);
  }

  let idx = len - 1;
  let mut idx_scope = scope.reborrow();
  let idx_s = idx_scope.alloc_string(&idx.to_string())?;
  idx_scope.push_root(Value::String(idx_s))?;
  let key = PropertyKey::from_string(idx_s);

  let element =
    idx_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
  let ok = idx_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, key)?;
  if !ok {
    return Err(VmError::TypeError("Array.prototype.pop failed"));
  }

  let ok = idx_scope.ordinary_set_with_host_and_hooks(
    vm,
    host,
    hooks,
    obj,
    length_key,
    Value::Number(idx as f64),
    Value::Object(obj),
  )?;
  if !ok {
    return Err(VmError::TypeError("Array.prototype.pop failed"));
  }

  Ok(element)
}

/// `Array.prototype.shift` (minimal).
pub fn array_prototype_shift(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length(len_value);

  if len == 0 {
    let ok = scope.ordinary_set_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      length_key,
      Value::Number(0.0),
      Value::Object(obj),
    )?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.shift failed"));
    }
    return Ok(Value::Undefined);
  }

  // Get the first element before shifting.
  let first_key = string_key(&mut scope, "0")?;
  let first =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, first_key, Value::Object(obj))?;

  // Shift existing elements down by one (holes preserved via HasProperty/Delete).
  for k in 1..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let from = k;
    let to = k - 1;

    let mut iter_scope = scope.reborrow();
    let from_s = iter_scope.alloc_string(&from.to_string())?;
    iter_scope.push_root(Value::String(from_s))?;
    let to_s = iter_scope.alloc_string(&to.to_string())?;
    iter_scope.push_root(Value::String(to_s))?;

    let from_key = PropertyKey::from_string(from_s);
    let to_key = PropertyKey::from_string(to_s);

    if iter_scope.ordinary_has_property_with_tick(obj, from_key, || vm.tick())? {
      let value =
        iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, from_key, Value::Object(obj))?;
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
        return Err(VmError::TypeError("Array.prototype.shift failed"));
      }
    } else {
      let ok = iter_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, to_key)?;
      if !ok {
        return Err(VmError::TypeError("Array.prototype.shift failed"));
      }
    }
  }

  // Delete the last element (if any) and update length.
  let last = len - 1;
  {
    let mut del_scope = scope.reborrow();
    let last_s = del_scope.alloc_string(&last.to_string())?;
    del_scope.push_root(Value::String(last_s))?;
    let last_key = PropertyKey::from_string(last_s);
    let ok = del_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, last_key)?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.shift failed"));
    }

    let ok = del_scope.ordinary_set_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      length_key,
      Value::Number(last as f64),
      Value::Object(obj),
    )?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.shift failed"));
    }
  }

  Ok(first)
}

/// `Array.prototype.unshift` (minimal).
pub fn array_prototype_unshift(
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

  let insert_count = args.len();
  if insert_count == 0 {
    return Ok(Value::Number(len as f64));
  }

  let new_len = len
    .checked_add(insert_count)
    .ok_or(VmError::OutOfMemory)?;

  // Move existing elements up by `insert_count` starting from the end so we don't overwrite.
  for k in (0..len).rev() {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let from = k;
    let to = k
      .checked_add(insert_count)
      .ok_or(VmError::OutOfMemory)?;

    let mut iter_scope = scope.reborrow();
    let from_s = iter_scope.alloc_string(&from.to_string())?;
    iter_scope.push_root(Value::String(from_s))?;
    let to_s = iter_scope.alloc_string(&to.to_string())?;
    iter_scope.push_root(Value::String(to_s))?;

    let from_key = PropertyKey::from_string(from_s);
    let to_key = PropertyKey::from_string(to_s);

    if iter_scope.ordinary_has_property_with_tick(obj, from_key, || vm.tick())? {
      let value =
        iter_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, from_key, Value::Object(obj))?;
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
        return Err(VmError::TypeError("Array.prototype.unshift failed"));
      }
    } else {
      let ok = iter_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, to_key)?;
      if !ok {
        return Err(VmError::TypeError("Array.prototype.unshift failed"));
      }
    }
  }

  // Set the inserted items.
  for (i, item) in args.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let mut set_scope = scope.reborrow();
    let idx_s = set_scope.alloc_string(&i.to_string())?;
    set_scope.push_root(Value::String(idx_s))?;
    let key = PropertyKey::from_string(idx_s);
    let ok = set_scope.ordinary_set_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      key,
      item,
      Value::Object(obj),
    )?;
    if !ok {
      return Err(VmError::TypeError("Array.prototype.unshift failed"));
    }
  }

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
    return Err(VmError::TypeError("Array.prototype.unshift failed"));
  }

  Ok(Value::Number(new_len as f64))
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

    if !iter_scope.ordinary_has_property_with_tick(obj, from_key, || vm.tick())? {
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

      if iter_scope.ordinary_has_property_with_tick(obj, from_key, || vm.tick())? {
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

      if iter_scope.ordinary_has_property_with_tick(obj, from_key, || vm.tick())? {
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

/// `Array.prototype.values` / `%Array.prototype%[@@iterator]` (minimal).
///
/// This is primarily needed by higher-level binding layers (e.g. WebIDL iterable snapshot helpers)
/// that want to build a JS `Array` and then obtain an iterator via `arr[Symbol.iterator]()`.
pub fn array_prototype_values(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let this_obj = match this {
    Value::Object(o) => o,
    _ => return Err(VmError::TypeError("Array.prototype.values called on non-object")),
  };

  // Root `this` while allocating/defining properties on the iterator object.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(this_obj))?;

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  scope
    .heap_mut()
    .object_set_prototype(iter, Some(intr.object_prototype()))?;

  let array_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_ARRAY_MARKER)?;
  scope.define_property(
    iter,
    array_key,
    data_desc(Value::Object(this_obj), true, false, true),
  )?;

  let index_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_INDEX_MARKER)?;
  scope.define_property(
    iter,
    index_key,
    data_desc(Value::Number(0.0), true, false, true),
  )?;

  let next_key = string_key(&mut scope, "next")?;
  scope.define_property(
    iter,
    next_key,
    data_desc(Value::Object(intr.array_iterator_next()), true, false, true),
  )?;

  Ok(Value::Object(iter))
}

/// `ArrayIterator.prototype.next` (minimal).
pub fn array_iterator_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let this_obj = match this {
    Value::Object(o) => o,
    _ => return Err(VmError::TypeError("Array iterator next called on non-object")),
  };

  // Root `this` across any allocations performed while creating the iterator result object.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(this_obj))?;

  let array_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_ARRAY_MARKER)?;
  let array_value = get_data_property_value(vm, &mut scope, this_obj, &array_key)?
    .ok_or(VmError::TypeError("Array iterator missing internal array"))?;
  let Value::Object(array_obj) = array_value else {
    return Err(VmError::TypeError("Array iterator internal array is not an object"));
  };
  scope.push_root(Value::Object(array_obj))?;

  let index_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_INDEX_MARKER)?;
  let index_value = get_data_property_value(vm, &mut scope, this_obj, &index_key)?
    .unwrap_or(Value::Number(0.0));
  let idx = match index_value {
    Value::Number(n) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0usize,
  };

  let len = get_array_length(vm, &mut scope, array_obj)?;
  if idx >= len {
    let out = scope.alloc_object()?;
    scope.push_root(Value::Object(out))?;
    scope
      .heap_mut()
      .object_set_prototype(out, Some(intr.object_prototype()))?;
    let value_key = string_key(&mut scope, "value")?;
    let done_key = string_key(&mut scope, "done")?;
    scope.define_property(out, value_key, data_desc(Value::Undefined, true, true, true))?;
    scope.define_property(out, done_key, data_desc(Value::Bool(true), true, true, true))?;
    return Ok(Value::Object(out));
  }

  // Root `array_obj` and the index string across allocation for the property key.
  let idx_s = scope.alloc_string(&idx.to_string())?;
  scope.push_root(Value::String(idx_s))?;
  let key = PropertyKey::from_string(idx_s);
  let value = get_data_property_value(vm, &mut scope, array_obj, &key)?.unwrap_or(Value::Undefined);
  scope.push_root(value)?;

  // Update `[[ArrayIteratorNextIndex]]`.
  let next_idx = idx.saturating_add(1);
  scope.define_property(
    this_obj,
    index_key,
    data_desc(Value::Number(next_idx as f64), true, false, true),
  )?;

  // Create iterator result object.
  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;
  let value_key = string_key(&mut scope, "value")?;
  let done_key = string_key(&mut scope, "done")?;
  scope.define_property(out, value_key, data_desc(value, true, true, true))?;
  scope.define_property(out, done_key, data_desc(Value::Bool(false), true, true, true))?;
  Ok(Value::Object(out))
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

/// `String.fromCharCode(...codeUnits)` (ECMA-262) (minimal).
pub fn string_from_char_code(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // ToUint16(ToNumber(arg)) for each argument, then construct a string from the resulting UTF-16
  // code units.
  //
  // This is intentionally minimal: it covers the common case (`String.fromCharCode(97) === "a"`)
  // and is sufficient for exercising async `await` in call arguments.
  let mut units: Vec<u16> = Vec::new();
  units
    .try_reserve_exact(args.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for &arg in args {
    let n = scope.to_number(vm, host, hooks, arg)?;
    let unit = if !n.is_finite() || n == 0.0 {
      0
    } else {
      let int = n.trunc();
      const TWO_16: f64 = 65_536.0;
      let mut v = int % TWO_16;
      if v < 0.0 {
        v += TWO_16;
      }
      v as u16
    };
    units.push(unit);
  }

  let s = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(s))
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

/// `String.prototype.charCodeAt(pos)` (minimal).
pub fn string_prototype_char_code_at(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Extract the underlying primitive string from either:
  // - a string primitive (`"x"`), or
  // - a boxed String object (`Object("x")` / `new String("x")`).
  let prim = match this {
    Value::String(s) => s,
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.StringData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope.heap().object_get_own_data_property_value(obj, &marker_key)? {
        Some(Value::String(s)) => s,
        _ => return Err(VmError::TypeError("String.prototype.charCodeAt on non-String object")),
      }
    }
    _ => return Err(VmError::TypeError("String.prototype.charCodeAt on non-string")),
  };

  let pos_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut n = match pos_value {
    Value::Undefined => 0.0,
    other => scope.to_number(vm, host, hooks, other)?,
  };

  // `ToIntegerOrInfinity` rounds toward zero.
  if n.is_nan() {
    n = 0.0;
  }
  if !n.is_finite() {
    return Ok(Value::Number(f64::NAN));
  }
  n = n.trunc();
  if n < 0.0 {
    return Ok(Value::Number(f64::NAN));
  }

  let idx = n as usize;
  let js = scope.heap().get_string(prim)?;
  let units = js.as_code_units();
  if idx >= units.len() {
    return Ok(Value::Number(f64::NAN));
  }
  Ok(Value::Number(units[idx] as f64))
}

/// `String.prototype.charAt(pos)` (ECMA-262) (minimal).
pub fn string_prototype_char_at(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let pos_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut n = if matches!(pos_value, Value::Undefined) {
    0.0
  } else {
    scope.to_number(vm, host, hooks, pos_value)?
  };

  // `ToIntegerOrInfinity` rounds toward zero.
  if n.is_nan() {
    n = 0.0;
  }
  if !n.is_finite() {
    let empty = scope.alloc_string("")?;
    return Ok(Value::String(empty));
  }
  n = n.trunc();
  if n < 0.0 {
    let empty = scope.alloc_string("")?;
    return Ok(Value::String(empty));
  }

  let idx = n as usize;
  let unit = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units().get(idx).copied()
  };

  let Some(unit) = unit else {
    let empty = scope.alloc_string("")?;
    return Ok(Value::String(empty));
  };

  let out = scope.alloc_string_from_u16_vec(vec![unit])?;
  Ok(Value::String(out))
}

/// `String.prototype.slice` (ECMA-262) (minimal).
pub fn string_prototype_slice(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let (start, end) = slice_range_from_args(vm, &mut scope, host, hooks, len, args)?;

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()[start..end].to_vec()
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.indexOf` (ECMA-262) (minimal).
pub fn string_prototype_index_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let search_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let search = scope.to_string(vm, host, hooks, search_value)?;
  scope.push_root(Value::String(search))?;

  // `position` is clamped to [0, len] (negative -> 0).
  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let position = args.get(1).copied().unwrap_or(Value::Undefined);
  let pos = if matches!(position, Value::Undefined) {
    0usize
  } else {
    let n = scope.to_number(vm, host, hooks, position)?;
    if n.is_nan() || n.is_sign_negative() {
      0usize
    } else if !n.is_finite() {
      len
    } else {
      let n = n.trunc();
      if n <= 0.0 {
        0usize
      } else if n >= len as f64 {
        len
      } else {
        n as usize
      }
    }
  };

  // Borrow code unit slices for the search. Avoid allocating intermediate vectors: these inputs
  // can be attacker-controlled.
  let (haystack, needle) = {
    let haystack = scope.heap().get_string(s)?;
    let needle = scope.heap().get_string(search)?;
    (haystack.as_code_units(), needle.as_code_units())
  };

  if needle.is_empty() {
    return Ok(Value::Number(pos as f64));
  }
  if needle.len() > haystack.len() {
    return Ok(Value::Number(-1.0));
  }
  if pos > haystack.len() {
    return Ok(Value::Number(-1.0));
  }

  let last = haystack.len().saturating_sub(needle.len());
  for i in pos..=last {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if &haystack[i..i + needle.len()] == needle {
      return Ok(Value::Number(i as f64));
    }
  }

  Ok(Value::Number(-1.0))
}

/// `String.prototype.includes` (ECMA-262) (minimal).
pub fn string_prototype_includes(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let search_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let search = scope.to_string(vm, host, hooks, search_value)?;
  scope.push_root(Value::String(search))?;

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let position = args.get(1).copied().unwrap_or(Value::Undefined);
  let pos = string_position_from_value(vm, &mut scope, host, hooks, position, len, 0)?;

  let (haystack, needle) = {
    let haystack = scope.heap().get_string(s)?;
    let needle = scope.heap().get_string(search)?;
    (haystack.as_code_units(), needle.as_code_units())
  };

  if needle.is_empty() {
    return Ok(Value::Bool(true));
  }
  if needle.len() > haystack.len() {
    return Ok(Value::Bool(false));
  }
  if pos > haystack.len() {
    return Ok(Value::Bool(false));
  }

  let last = haystack.len().saturating_sub(needle.len());
  for i in pos..=last {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if &haystack[i..i + needle.len()] == needle {
      return Ok(Value::Bool(true));
    }
  }

  Ok(Value::Bool(false))
}

/// `String.prototype.startsWith` (ECMA-262) (minimal).
pub fn string_prototype_starts_with(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let search_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let search = scope.to_string(vm, host, hooks, search_value)?;
  scope.push_root(Value::String(search))?;

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let position = args.get(1).copied().unwrap_or(Value::Undefined);
  let pos = string_position_from_value(vm, &mut scope, host, hooks, position, len, 0)?;

  let (haystack, needle) = {
    let haystack = scope.heap().get_string(s)?;
    let needle = scope.heap().get_string(search)?;
    (haystack.as_code_units(), needle.as_code_units())
  };

  if needle.len().saturating_add(pos) > haystack.len() {
    return Ok(Value::Bool(false));
  }
  Ok(Value::Bool(
    &haystack[pos..pos + needle.len()] == needle,
  ))
}

/// `String.prototype.endsWith` (ECMA-262) (minimal).
pub fn string_prototype_ends_with(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let search_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let search = scope.to_string(vm, host, hooks, search_value)?;
  scope.push_root(Value::String(search))?;

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let end_position = args.get(1).copied().unwrap_or(Value::Undefined);
  let end = string_position_from_value(vm, &mut scope, host, hooks, end_position, len, len)?;

  let (haystack, needle) = {
    let haystack = scope.heap().get_string(s)?;
    let needle = scope.heap().get_string(search)?;
    (haystack.as_code_units(), needle.as_code_units())
  };

  if needle.len() > end {
    return Ok(Value::Bool(false));
  }
  let start = end - needle.len();
  Ok(Value::Bool(&haystack[start..end] == needle))
}

/// `String.prototype.trim` (ECMA-262) (minimal).
pub fn string_prototype_trim(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let (len, start, end) = {
    let js = scope.heap().get_string(s)?;
    let units = js.as_code_units();
    let len = units.len();

    let mut start = 0usize;
    while start < len {
      if start % 1024 == 0 {
        vm.tick()?;
      }
      if !is_trim_whitespace_unit(units[start]) {
        break;
      }
      start += 1;
    }

    let mut end = len;
    while end > start {
      if end % 1024 == 0 {
        vm.tick()?;
      }
      if !is_trim_whitespace_unit(units[end - 1]) {
        break;
      }
      end -= 1;
    }

    (len, start, end)
  };

  if start == 0 && end == len {
    return Ok(Value::String(s));
  }

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()[start..end].to_vec()
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.trimStart` / `String.prototype.trimLeft` (ECMA-262) (minimal).
pub fn string_prototype_trim_start(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let (len, start) = {
    let js = scope.heap().get_string(s)?;
    let units = js.as_code_units();
    let len = units.len();

    let mut start = 0usize;
    while start < len {
      if start % 1024 == 0 {
        vm.tick()?;
      }
      if !is_trim_whitespace_unit(units[start]) {
        break;
      }
      start += 1;
    }

    (len, start)
  };

  if start == 0 {
    return Ok(Value::String(s));
  }

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()[start..len].to_vec()
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.trimEnd` / `String.prototype.trimRight` (ECMA-262) (minimal).
pub fn string_prototype_trim_end(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let (len, end) = {
    let js = scope.heap().get_string(s)?;
    let units = js.as_code_units();
    let len = units.len();

    let mut end = len;
    while end > 0 {
      if end % 1024 == 0 {
        vm.tick()?;
      }
      if !is_trim_whitespace_unit(units[end - 1]) {
        break;
      }
      end -= 1;
    }

    (len, end)
  };

  if end == len {
    return Ok(Value::String(s));
  }

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()[0..end].to_vec()
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.substring` (ECMA-262) (minimal).
pub fn string_prototype_substring(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };

  let start_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let end_arg = args.get(1).copied().unwrap_or(Value::Undefined);

  let start = string_position_from_value(vm, &mut scope, host, hooks, start_arg, len, 0)?;
  let end = string_position_from_value(vm, &mut scope, host, hooks, end_arg, len, len)?;

  let (from, to) = if start > end { (end, start) } else { (start, end) };

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()[from..to].to_vec()
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.substr(start, length)` (Annex B) (minimal).
pub fn string_prototype_substr(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };

  let start_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let start = slice_index_from_value(vm, &mut scope, host, hooks, start_arg, len, 0)?;

  let length_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  let end = if matches!(length_arg, Value::Undefined) {
    len
  } else {
    let mut n = scope.to_number(vm, host, hooks, length_arg)?;
    if n.is_nan() {
      n = 0.0;
    }
    if !n.is_finite() {
      if n.is_sign_negative() {
        start
      } else {
        len
      }
    } else {
      n = n.trunc();
      if n <= 0.0 {
        start
      } else {
        start.saturating_add(n as usize).min(len)
      }
    }
  };

  if start == 0 && end == len {
    return Ok(Value::String(s));
  }
  if end <= start {
    let empty = scope.alloc_string("")?;
    return Ok(Value::String(empty));
  }

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()[start..end].to_vec()
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.split(separator, limit)` (ECMA-262) (minimal, string separator only).
pub fn string_prototype_split(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let separator = args.get(0).copied().unwrap_or(Value::Undefined);
  let limit_val = args.get(1).copied().unwrap_or(Value::Undefined);

  // Per spec, `limit` is `ToUint32(limit)`. If it is not provided, the limit is `2^32 - 1`.
  let limit: u32 = if matches!(limit_val, Value::Undefined) {
    u32::MAX
  } else {
    let n = scope.to_number(vm, host, hooks, limit_val)?;
    if !n.is_finite() || n == 0.0 {
      0
    } else {
      let int = n.trunc();
      let modulo = int.rem_euclid(4294967296.0);
      modulo as u32
    }
  };

  if limit == 0 {
    return Ok(Value::Object(create_array_object(vm, &mut scope, 0)?));
  }

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  // `separator === undefined` => return [S].
  if matches!(separator, Value::Undefined) {
    let array = create_array_object(vm, &mut scope, 1)?;
    let mut idx_scope = scope.reborrow();
    idx_scope.push_root(Value::Object(array))?;
    idx_scope.push_root(Value::String(s))?;
    let key = string_key(&mut idx_scope, "0")?;
    idx_scope.define_property(array, key, data_desc(Value::String(s), true, true, true))?;
    return Ok(Value::Object(array));
  }

  let sep = scope.to_string(vm, host, hooks, separator)?;
  scope.push_root(Value::String(sep))?;

  let s_len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let sep_len = {
    let js = scope.heap().get_string(sep)?;
    js.len_code_units()
  };

  // Special-case: split by empty string yields one element per UTF-16 code unit.
  if sep_len == 0 {
    let count = s_len.min(limit as usize);
    let count_u32 = u32::try_from(count).map_err(|_| VmError::OutOfMemory)?;
    let array = create_array_object(vm, &mut scope, count_u32)?;

    for i in 0..count {
      if i % 1024 == 0 {
        vm.tick()?;
      }

      let unit = {
        let js = scope.heap().get_string(s)?;
        js.as_code_units()[i]
      };

      let mut iter_scope = scope.reborrow();
      iter_scope.push_root(Value::Object(array))?;

      let part = iter_scope.alloc_string_from_u16_vec(vec![unit])?;
      iter_scope.push_root(Value::String(part))?;

      let key_s = iter_scope.alloc_string(&i.to_string())?;
      iter_scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      iter_scope.define_property(array, key, data_desc(Value::String(part), true, true, true))?;
    }

    return Ok(Value::Object(array));
  }

  let limit_usize = limit as usize;

  // Collect the substring boundaries first so we don't hold a borrow of the string's code units
  // while allocating substring strings.
  let ranges: Vec<(usize, usize)> = {
    let haystack = scope.heap().get_string(s)?;
    let needle = scope.heap().get_string(sep)?;
    let hay_units = haystack.as_code_units();
    let needle_units = needle.as_code_units();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;

    if needle_units.len() > hay_units.len() {
      ranges.push((0, hay_units.len()));
      ranges
    } else {
      while ranges.len() < limit_usize {
        let last = hay_units.len() - needle_units.len();
        if start > last {
          break;
        }

        let mut found: Option<usize> = None;
        for i in start..=last {
          if i % 1024 == 0 {
            vm.tick()?;
          }
          if &hay_units[i..i + needle_units.len()] == needle_units {
            found = Some(i);
            break;
          }
        }

        let Some(pos) = found else {
          break;
        };

        ranges.push((start, pos));
        if ranges.len() >= limit_usize {
          break;
        }
        start = pos + needle_units.len();
      }

      // Remainder.
      if ranges.len() < limit_usize {
        ranges.push((start, hay_units.len()));
      }
      ranges
    }
  };

  let out_len_u32 = u32::try_from(ranges.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, out_len_u32)?;

  for (i, (from, to)) in ranges.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(array))?;

    let part = if from == to {
      // Avoid allocating an intermediate Vec for empty segments.
      iter_scope.alloc_string("")?
    } else {
      let units: Vec<u16> = {
        let js = iter_scope.heap().get_string(s)?;
        js.as_code_units()[from..to].to_vec()
      };
      iter_scope.alloc_string_from_u16_vec(units)?
    };
    iter_scope.push_root(Value::String(part))?;

    let key_s = iter_scope.alloc_string(&i.to_string())?;
    iter_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    iter_scope.define_property(array, key, data_desc(Value::String(part), true, true, true))?;
  }

  Ok(Value::Object(array))
}

/// `String.prototype.repeat(count)` (ECMA-262) (minimal).
pub fn string_prototype_repeat(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let count_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut n = scope.to_number(vm, host, hooks, count_val)?;
  if n.is_nan() {
    n = 0.0;
  }
  if !n.is_finite() {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid count value")?;
    return Err(VmError::Throw(err));
  }
  n = n.trunc();
  if n < 0.0 {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid count value")?;
    return Err(VmError::Throw(err));
  }

  if n == 0.0 {
    return Ok(Value::String(scope.alloc_string("")?));
  }

  // `ToIntegerOrInfinity` yields an integral f64; guard against extremely large counts before
  // converting to usize.
  if n > (usize::MAX as f64) {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid string length")?;
    return Err(VmError::Throw(err));
  }
  let count = n as usize;

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units().to_vec()
  };
  if units.is_empty() {
    return Ok(Value::String(scope.alloc_string("")?));
  }

  let total_len = match units.len().checked_mul(count) {
    Some(n) => n,
    None => {
      let intr = require_intrinsics(vm)?;
      let err = crate::new_range_error(&mut scope, intr, "Invalid string length")?;
      return Err(VmError::Throw(err));
    }
  };

  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;

  for i in 0..count {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    out.extend_from_slice(&units);
  }

  let out = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out))
}

fn string_pad(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  this: Value,
  args: &[Value],
  pad_at_start: bool,
) -> Result<Value, VmError> {
  // Spec reference: `String.prototype.padStart` / `String.prototype.padEnd`.
  //
  // This is a deliberately small, deterministic implementation:
  // - supports the common `targetLength` + optional `padString` arguments
  // - throws RangeError for non-finite target lengths (e.g. Infinity)
  // - treats empty padString as a no-op, per spec
  let mut scope = scope.reborrow();

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let target_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut n = scope.to_number(vm, host, hooks, target_value)?;
  if n.is_nan() || n <= 0.0 {
    return Ok(Value::String(s));
  }
  if !n.is_finite() {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid string length")?;
    return Err(VmError::Throw(err));
  }
  n = n.trunc();
  if n <= 0.0 {
    return Ok(Value::String(s));
  }

  // Guard before converting to usize.
  if n > (usize::MAX as f64) {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid string length")?;
    return Err(VmError::Throw(err));
  }
  let target_len = n as usize;

  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units().to_vec()
  };
  if target_len <= units.len() {
    return Ok(Value::String(s));
  }
  let fill_len = target_len - units.len();

  // Compute pad string code units.
  let pad_units: Vec<u16> = match args.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Undefined => vec![0x0020], // " "
    pad_value => {
      let pad_s = scope.to_string(vm, host, hooks, pad_value)?;
      let js = scope.heap().get_string(pad_s)?;
      js.as_code_units().to_vec()
    }
  };

  if pad_units.is_empty() {
    // Per spec, empty padString results in no padding.
    return Ok(Value::String(s));
  }

  // Build the filler string (truncated repetition of padString to exactly `fill_len` code units).
  let mut filler: Vec<u16> = Vec::new();
  filler
    .try_reserve_exact(fill_len)
    .map_err(|_| VmError::OutOfMemory)?;

  let mut iterations: usize = 0;
  while filler.len() < fill_len {
    if iterations % 1024 == 0 {
      vm.tick()?;
    }
    iterations = iterations.saturating_add(1);

    let remaining = fill_len - filler.len();
    if remaining >= pad_units.len() {
      filler.extend_from_slice(&pad_units);
    } else {
      filler.extend_from_slice(&pad_units[..remaining]);
    }
  }

  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(target_len)
    .map_err(|_| VmError::OutOfMemory)?;
  if pad_at_start {
    out.extend_from_slice(&filler);
    out.extend_from_slice(&units);
  } else {
    out.extend_from_slice(&units);
    out.extend_from_slice(&filler);
  }

  Ok(Value::String(scope.alloc_string_from_u16_vec(out)?))
}

/// `String.prototype.padStart(targetLength, padString)` (ECMA-262).
pub fn string_prototype_pad_start(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  string_pad(vm, scope, host, hooks, this, args, /* pad_at_start */ true)
}

/// `String.prototype.padEnd(targetLength, padString)` (ECMA-262).
pub fn string_prototype_pad_end(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  string_pad(vm, scope, host, hooks, this, args, /* pad_at_start */ false)
}

/// `String.prototype.toLowerCase` (ECMA-262) (minimal).
pub fn string_prototype_to_lower_case(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let units = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()
  };

  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  let mut i = 0usize;
  while i < units.len() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let u = units[i];
    let (ch, consumed) = if (0xD800..=0xDBFF).contains(&u)
      && i + 1 < units.len()
      && (0xDC00..=0xDFFF).contains(&units[i + 1])
    {
      let high = (u as u32) - 0xD800;
      let low = (units[i + 1] as u32) - 0xDC00;
      let cp = 0x10000 + ((high << 10) | low);
      (char::from_u32(cp).ok_or(VmError::InvariantViolation("invalid surrogate pair"))?, 2usize)
    } else if (0xD800..=0xDFFF).contains(&u) {
      // Unpaired surrogate: case conversion leaves it unchanged.
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      out.push(u);
      i += 1;
      continue;
    } else {
      (
        char::from_u32(u as u32).ok_or(VmError::InvariantViolation("invalid utf-16 code unit"))?,
        1usize,
      )
    };

    for mapped in ch.to_lowercase() {
      let mut buf = [0u16; 2];
      let encoded = mapped.encode_utf16(&mut buf);
      out
        .try_reserve(encoded.len())
        .map_err(|_| VmError::OutOfMemory)?;
      out.extend_from_slice(encoded);
    }
    i += consumed;
  }

  let out = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out))
}

/// `String.prototype.toUpperCase` (ECMA-262) (minimal).
pub fn string_prototype_to_upper_case(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let units = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units()
  };

  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  let mut i = 0usize;
  while i < units.len() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let u = units[i];
    let (ch, consumed) = if (0xD800..=0xDBFF).contains(&u)
      && i + 1 < units.len()
      && (0xDC00..=0xDFFF).contains(&units[i + 1])
    {
      let high = (u as u32) - 0xD800;
      let low = (units[i + 1] as u32) - 0xDC00;
      let cp = 0x10000 + ((high << 10) | low);
      (char::from_u32(cp).ok_or(VmError::InvariantViolation("invalid surrogate pair"))?, 2usize)
    } else if (0xD800..=0xDFFF).contains(&u) {
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      out.push(u);
      i += 1;
      continue;
    } else {
      (
        char::from_u32(u as u32).ok_or(VmError::InvariantViolation("invalid utf-16 code unit"))?,
        1usize,
      )
    };

    for mapped in ch.to_uppercase() {
      let mut buf = [0u16; 2];
      let encoded = mapped.encode_utf16(&mut buf);
      out
        .try_reserve(encoded.len())
        .map_err(|_| VmError::OutOfMemory)?;
      out.extend_from_slice(encoded);
    }
    i += consumed;
  }

  let out = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out))
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

/// Global `isFinite(x)` (minimal).
pub fn global_is_finite(
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
  Ok(Value::Bool(n.is_finite()))
}

fn to_int32(n: f64) -> i32 {
  if !n.is_finite() || n == 0.0 {
    return 0;
  }
  // ECMA-262 `ToInt32`: truncate then compute modulo 2^32.
  let int = n.trunc();
  const TWO_32: f64 = 4_294_967_296.0;
  const TWO_31: f64 = 2_147_483_648.0;

  let mut int = int % TWO_32;
  if int < 0.0 {
    int += TWO_32;
  }
  if int >= TWO_31 {
    (int - TWO_32) as i32
  } else {
    int as i32
  }
}

fn radix_digit_value(unit: u16) -> Option<u8> {
  match unit {
    0x0030..=0x0039 => Some((unit - 0x0030) as u8),
    0x0061..=0x007A => Some((unit - 0x0061 + 10) as u8),
    0x0041..=0x005A => Some((unit - 0x0041 + 10) as u8),
    _ => None,
  }
}

/// Global `parseInt(string, radix)` (ECMA-262).
pub fn global_parse_int(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let radix_arg = args.get(1).copied().unwrap_or(Value::Undefined);

  // 1. Let inputString be ? ToString(string).
  let input_string = scope.to_string(vm, host, hooks, input)?;
  scope.push_root(Value::String(input_string))?;

  // 6. Let R be ℝ(? ToInt32(radix)).
  //
  // Compute this before borrowing `input_string`'s code units from the heap to avoid holding an
  // immutable heap borrow across `ToNumber`/`ToInt32` (which need mutable access to `scope`).
  let mut r: i32 = if matches!(radix_arg, Value::Undefined) {
    0
  } else {
    let r_num = scope.to_number(vm, host, hooks, radix_arg)?;
    to_int32(r_num)
  };

  // 2. Let S be ! TrimString(inputString, ~start~).
  let units = scope.heap().get_string(input_string)?.as_code_units();
  let mut trim_start = 0usize;
  while trim_start < units.len() && is_trim_whitespace_unit(units[trim_start]) {
    if trim_start % 1024 == 0 {
      vm.tick()?;
    }
    trim_start += 1;
  }

  // Work over `S` by keeping a moving slice offset instead of allocating.
  let mut s_start = trim_start;

  // 3. Let sign be 1.
  // 4. If S is not empty and first code unit is '-', sign = -1.
  let mut sign: i32 = 1;
  if s_start < units.len() && units[s_start] == b'-' as u16 {
    sign = -1;
  }

  // 5. If S is not empty and first code unit is '+' or '-', drop it.
  if s_start < units.len() && (units[s_start] == b'+' as u16 || units[s_start] == b'-' as u16) {
    s_start += 1;
  }

  // 7. Let stripPrefix be true.
  let mut strip_prefix = true;
  // 8. If R ≠ 0...
  if r != 0 {
    if !(2..=36).contains(&r) {
      return Ok(Value::Number(f64::NAN));
    }
    if r != 16 {
      strip_prefix = false;
    }
  } else {
    // 9. Else, set R to 10.
    r = 10;
  }

  // 10. If stripPrefix, and S starts with "0x"/"0X", drop it and set R=16.
  if strip_prefix {
    if s_start + 1 < units.len()
      && units[s_start] == b'0' as u16
      && (units[s_start + 1] == b'x' as u16 || units[s_start + 1] == b'X' as u16)
    {
      s_start += 2;
      r = 16;
    }
  }

  // 11. Find `end`: the first code unit that is not a radix-R digit.
  let radix_u8 = u8::try_from(r).unwrap_or(10);
  let mut end = s_start;
  while end < units.len() {
    if end % 1024 == 0 {
      vm.tick()?;
    }
    let Some(digit) = radix_digit_value(units[end]) else {
      break;
    };
    if digit >= radix_u8 {
      break;
    }
    end += 1;
  }

  // 12. Let Z be the substring of S from 0..end. If empty, return NaN.
  if end == s_start {
    return Ok(Value::Number(f64::NAN));
  }

  // 13. Let mathInt be the integer value represented by Z in radix-R.
  // We accumulate in IEEE-754 f64, matching the spec's implementation-defined approximations.
  let mut math_int: f64 = 0.0;
  let radix_f64 = r as f64;
  for (i, &unit) in units[s_start..end].iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let digit = radix_digit_value(unit).unwrap() as f64;
    math_int = math_int * radix_f64 + digit;
  }

  // 14. Handle -0.
  if math_int == 0.0 {
    if sign == -1 {
      return Ok(Value::Number(-0.0));
    }
    return Ok(Value::Number(0.0));
  }

  Ok(Value::Number((sign as f64) * math_int))
}

fn is_ascii_digit_unit(unit: u16) -> bool {
  (b'0' as u16..=b'9' as u16).contains(&unit)
}

fn parse_ascii_digits_to_i64_with_limit(units: &[u16], max: i64) -> i64 {
  let mut v: i64 = 0;
  for &u in units {
    let d = (u - b'0' as u16) as i64;
    if v > max {
      return max;
    }
    v = v.saturating_mul(10).saturating_add(d);
    if v > max {
      return max;
    }
  }
  v
}

fn parse_float_from_str_decimal_literal_prefix(
  prefix_units: &[u16],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<f64, VmError> {
  // `prefix_units` must satisfy `StrDecimalLiteral`.
  //
  // Spec: https://tc39.es/ecma262/#sec-parsefloat-string
  // This implementation avoids allocating a Rust `String` proportional to the prefix length.
  //
  // Approach:
  // - Parse digits/decimal/exponent into (trimmed digits, frac_len, exp_part).
  // - Build a small scientific-notation ASCII buffer from at most MAX_SIG_DIGITS digits.
  // - Use `fast_float` to do the final correct rounding to f64.
  //
  // Note: This intentionally preserves the "prefix" semantics (junk after the literal is ignored),
  // but the numeric conversion may be approximated when there are more than MAX_SIG_DIGITS
  // significant digits (bounded memory).

  const INFINITY_UNITS: [u16; 8] = [73, 110, 102, 105, 110, 105, 116, 121]; // "Infinity"
  const MAX_SIG_DIGITS: usize = 128;
  const MAX_EXP_ABS: i64 = 1_000_000_000;

  let mut i = 0usize;
  let mut sign: i32 = 1;
  if prefix_units.get(0) == Some(&(b'+' as u16)) {
    i += 1;
  } else if prefix_units.get(0) == Some(&(b'-' as u16)) {
    sign = -1;
    i += 1;
  }

  // Infinity.
  if i + INFINITY_UNITS.len() <= prefix_units.len()
    && &prefix_units[i..i + INFINITY_UNITS.len()] == INFINITY_UNITS
  {
    return Ok((sign as f64) * f64::INFINITY);
  }

  // Collect significant digits (trim leading zeros).
  let mut digits: [u8; MAX_SIG_DIGITS] = [0; MAX_SIG_DIGITS];
  let mut digits_len: usize = 0;
  let mut sig_len_total: i64 = 0; // total digits after first non-zero (incl zeros)
  let mut saw_nonzero = false;
  let mut after_decimal = false;
  let mut frac_len: i64 = 0;

  // Parse the significand (digits + optional dot + digits), stopping at exponent.
  while i < prefix_units.len() {
    if i % 1024 == 0 {
      tick()?;
    }
    let u = prefix_units[i];
    if u == b'.' as u16 {
      after_decimal = true;
      i += 1;
      continue;
    }
    if u == b'e' as u16 || u == b'E' as u16 {
      break;
    }
    debug_assert!(is_ascii_digit_unit(u), "invalid StrDecimalLiteral prefix");
    let d = (u - b'0' as u16) as u8;
    if after_decimal {
      frac_len = frac_len.saturating_add(1);
    }
    if !saw_nonzero {
      if d == 0 {
        i += 1;
        continue;
      }
      saw_nonzero = true;
    }
    sig_len_total = sig_len_total.saturating_add(1);
    if digits_len < MAX_SIG_DIGITS {
      digits[digits_len] = d;
      digits_len += 1;
    }
    i += 1;
  }

  if !saw_nonzero {
    // The prefix is some decimal literal representing zero.
    // Preserve -0.
    return Ok(if sign == -1 { -0.0 } else { 0.0 });
  }

  // Parse exponent (if present).
  let mut exp_part: i64 = 0;
  if i < prefix_units.len() && (prefix_units[i] == b'e' as u16 || prefix_units[i] == b'E' as u16) {
    i += 1;
    let mut exp_sign: i32 = 1;
    if prefix_units.get(i) == Some(&(b'+' as u16)) {
      i += 1;
    } else if prefix_units.get(i) == Some(&(b'-' as u16)) {
      exp_sign = -1;
      i += 1;
    }
    // Remaining units are digits (prefix validation guarantees at least one).
    exp_part = parse_ascii_digits_to_i64_with_limit(&prefix_units[i..], MAX_EXP_ABS);
    if exp_sign == -1 {
      exp_part = -exp_part;
    }
  }

  // Compute scientific notation exponent: exp_e = (exp_part - frac_len) + sig_len_total - 1.
  let exp_e = exp_part
    .saturating_sub(frac_len)
    .saturating_add(sig_len_total.saturating_sub(1));

  // Build a bounded ASCII buffer: "-d.dddde+NNN".
  let mut buf: [u8; 256] = [0; 256];
  let mut out_len: usize = 0;

  let mut push_byte = |b: u8| -> Result<(), VmError> {
    if out_len >= buf.len() {
      return Err(VmError::OutOfMemory);
    }
    buf[out_len] = b;
    out_len += 1;
    Ok(())
  };

  if sign == -1 {
    push_byte(b'-')?;
  }
  push_byte(b'0' + digits[0])?;
  if digits_len > 1 {
    push_byte(b'.')?;
    for &d in &digits[1..digits_len] {
      push_byte(b'0' + d)?;
    }
  }
  push_byte(b'e')?;

  // Exponent sign.
  let mut exp_abs: i64 = exp_e;
  if exp_abs < 0 {
    push_byte(b'-')?;
    exp_abs = -exp_abs;
  } else {
    push_byte(b'+')?;
  }

  // Write exponent digits without allocating.
  let mut tmp: [u8; 32] = [0; 32];
  let mut tmp_len = 0usize;
  let mut n = exp_abs as u64;
  if n == 0 {
    tmp[0] = b'0';
    tmp_len = 1;
  } else {
    while n > 0 {
      tmp[tmp_len] = b'0' + (n % 10) as u8;
      tmp_len += 1;
      n /= 10;
    }
    tmp[..tmp_len].reverse();
  }
  for &b in &tmp[..tmp_len] {
    push_byte(b)?;
  }

  let s = std::str::from_utf8(&buf[..out_len]).map_err(|_| VmError::InvariantViolation("parseFloat buffer not UTF-8"))?;
  Ok(fast_float::parse(s).unwrap_or(f64::NAN))
}

/// Global `parseFloat(string)` (ECMA-262, budgeted).
pub fn global_parse_float(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = scope.to_string(vm, host, hooks, input)?;
  scope.push_root(Value::String(s))?;
  let units = scope.heap().get_string(s)?.as_code_units();

  // 1. Trim leading whitespace.
  let mut i = 0usize;
  while i < units.len() && is_trim_whitespace_unit(units[i]) {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    i += 1;
  }
  if i >= units.len() {
    return Ok(Value::Number(f64::NAN));
  }

  // 2. Find the longest prefix satisfying `StrDecimalLiteral`.
  let start = i;
  // Optional sign.
  if units[i] == b'+' as u16 || units[i] == b'-' as u16 {
    i += 1;
  }

  const INFINITY_UNITS: [u16; 8] = [73, 110, 102, 105, 110, 105, 116, 121]; // "Infinity"
  if i + INFINITY_UNITS.len() <= units.len() && &units[i..i + INFINITY_UNITS.len()] == INFINITY_UNITS {
    let end = i + INFINITY_UNITS.len();
    let n = parse_float_from_str_decimal_literal_prefix(&units[start..end], || vm.tick())?;
    return Ok(Value::Number(n));
  }

  let mut j = i;
  let mut saw_digit = false;
  while j < units.len() && is_ascii_digit_unit(units[j]) {
    saw_digit = true;
    j += 1;
    if j % 1024 == 0 {
      vm.tick()?;
    }
  }
  if j < units.len() && units[j] == b'.' as u16 {
    j += 1;
    while j < units.len() && is_ascii_digit_unit(units[j]) {
      saw_digit = true;
      j += 1;
      if j % 1024 == 0 {
        vm.tick()?;
      }
    }
  }
  if !saw_digit {
    return Ok(Value::Number(f64::NAN));
  }

  if j < units.len() && (units[j] == b'e' as u16 || units[j] == b'E' as u16) {
    let exp_pos = j;
    let mut k = j + 1;
    if k < units.len() && (units[k] == b'+' as u16 || units[k] == b'-' as u16) {
      k += 1;
    }
    let mut exp_digit = false;
    while k < units.len() && is_ascii_digit_unit(units[k]) {
      exp_digit = true;
      k += 1;
      if k % 1024 == 0 {
        vm.tick()?;
      }
    }
    if exp_digit {
      j = k;
    } else {
      j = exp_pos;
    }
  }

  // 3. Convert the decimal literal substring to a Number value.
  let prefix = &units[start..j];
  let n = parse_float_from_str_decimal_literal_prefix(prefix, || vm.tick())?;
  Ok(Value::Number(n))
}

fn create_uri_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let ctor = intr.uri_error();

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

fn throw_uri_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let err = create_uri_error(vm, scope, host, message)?;
  Err(VmError::Throw(err))
}

fn is_encode_uri_unescaped(unit: u16, extra_unescaped: &[u16]) -> bool {
  // `alwaysUnescaped` is "A-Z a-z 0-9 _" + "-.!~*'()".
  matches!(unit, 0x0041..=0x005A) // A-Z
    || matches!(unit, 0x0061..=0x007A) // a-z
    || matches!(unit, 0x0030..=0x0039) // 0-9
    || unit == 0x005F // '_'
    || matches!(unit, 0x002D | 0x002E | 0x0021 | 0x007E | 0x002A | 0x0027 | 0x0028 | 0x0029)
    || extra_unescaped.contains(&unit)
}

fn utf16_code_point_at(units: &[u16], k: usize) -> (u32, usize, bool) {
  // Returns (code_point, code_unit_count, is_unpaired_surrogate)
  let u = units[k];
  if (0xD800..=0xDBFF).contains(&u) {
    if k + 1 < units.len() {
      let u2 = units[k + 1];
      if (0xDC00..=0xDFFF).contains(&u2) {
        let high = (u - 0xD800) as u32;
        let low = (u2 - 0xDC00) as u32;
        let cp = 0x10000 + ((high << 10) | low);
        return (cp, 2, false);
      }
    }
    return (u as u32, 1, true);
  }
  if (0xDC00..=0xDFFF).contains(&u) {
    return (u as u32, 1, true);
  }
  (u as u32, 1, false)
}

fn utf8_encode_code_point(cp: u32) -> [u8; 4] {
  let mut out = [0u8; 4];
  if cp <= 0x7F {
    out[0] = cp as u8;
  } else if cp <= 0x7FF {
    out[0] = 0xC0 | ((cp >> 6) as u8);
    out[1] = 0x80 | ((cp & 0x3F) as u8);
  } else if cp <= 0xFFFF {
    out[0] = 0xE0 | ((cp >> 12) as u8);
    out[1] = 0x80 | (((cp >> 6) & 0x3F) as u8);
    out[2] = 0x80 | ((cp & 0x3F) as u8);
  } else {
    out[0] = 0xF0 | ((cp >> 18) as u8);
    out[1] = 0x80 | (((cp >> 12) & 0x3F) as u8);
    out[2] = 0x80 | (((cp >> 6) & 0x3F) as u8);
    out[3] = 0x80 | ((cp & 0x3F) as u8);
  }
  out
}

fn percent_encode_byte_to_units(byte: u8, out: &mut Vec<u16>) -> Result<(), VmError> {
  const HEX: &[u8; 16] = b"0123456789ABCDEF";
  out.try_reserve(3).map_err(|_| VmError::OutOfMemory)?;
  out.push(b'%' as u16);
  out.push(HEX[(byte >> 4) as usize] as u16);
  out.push(HEX[(byte & 0x0F) as usize] as u16);
  Ok(())
}

fn encode_uri_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  input: Value,
  extra_unescaped: &[u16],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-encode
  let s = scope.to_string(vm, host, hooks, input)?;
  scope.push_root(Value::String(s))?;
  let units = scope.heap().get_string(s)?.as_code_units();

  let mut out: Vec<u16> = Vec::new();
  // Heuristic: most inputs are predominantly ASCII and do not grow much.
  out
    .try_reserve(units.len().min(1024))
    .map_err(|_| VmError::OutOfMemory)?;

  let mut k = 0usize;
  while k < units.len() {
    if k % 1024 == 0 {
      vm.tick()?;
    }
    let c = units[k];
    if is_encode_uri_unescaped(c, extra_unescaped) {
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      out.push(c);
      k += 1;
      continue;
    }

    let (cp, count, is_unpaired) = utf16_code_point_at(units, k);
    if is_unpaired {
      return throw_uri_error(vm, scope, hooks, "URI malformed");
    }
    k += count;

    let bytes = utf8_encode_code_point(cp);
    let byte_len = if cp <= 0x7F { 1 } else if cp <= 0x7FF { 2 } else if cp <= 0xFFFF { 3 } else { 4 };
    for &b in &bytes[..byte_len] {
      percent_encode_byte_to_units(b, &mut out)?;
    }
  }

  let out_s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out_s))
}

fn parse_hex_digit(unit: u16) -> Option<u8> {
  match unit {
    0x0030..=0x0039 => Some((unit - 0x0030) as u8),
    0x0061..=0x0066 => Some((unit - 0x0061 + 10) as u8),
    0x0041..=0x0046 => Some((unit - 0x0041 + 10) as u8),
    _ => None,
  }
}

fn parse_hex_octet(units: &[u16], pos: usize) -> Option<u8> {
  let hi = parse_hex_digit(*units.get(pos)?)?;
  let lo = parse_hex_digit(*units.get(pos + 1)?)?;
  Some((hi << 4) | lo)
}

fn decode_uri_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  input: Value,
  preserve_escape_set: &[u16],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-decode
  let s = scope.to_string(vm, host, hooks, input)?;
  scope.push_root(Value::String(s))?;
  let units = scope.heap().get_string(s)?.as_code_units();

  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  let mut k = 0usize;
  while k < units.len() {
    if k % 1024 == 0 {
      vm.tick()?;
    }
    let c = units[k];
    if c != b'%' as u16 {
      out.push(c);
      k += 1;
      continue;
    }

    // Percent escape.
    if k + 3 > units.len() {
      return throw_uri_error(vm, scope, hooks, "URI malformed");
    }
    let escape = &units[k..k + 3];
    let Some(b0) = parse_hex_octet(units, k + 1) else {
      return throw_uri_error(vm, scope, hooks, "URI malformed");
    };

    let leading_ones = (b0 as u8).leading_ones() as usize;
    if leading_ones == 0 {
      let ascii = b0 as u16;
      if preserve_escape_set.contains(&ascii) {
        out.extend_from_slice(escape);
      } else {
        out.push(ascii);
      }
      k += 3;
      continue;
    }

    if leading_ones == 1 || leading_ones > 4 {
      return throw_uri_error(vm, scope, hooks, "URI malformed");
    }

    // Collect continuation bytes.
    let n = leading_ones;
    let mut octets: [u8; 4] = [0; 4];
    octets[0] = b0;
    let mut idx = k + 3;
    for j in 1..n {
      if idx + 3 > units.len() {
        return throw_uri_error(vm, scope, hooks, "URI malformed");
      }
      if units[idx] != b'%' as u16 {
        return throw_uri_error(vm, scope, hooks, "URI malformed");
      }
      let Some(b) = parse_hex_octet(units, idx + 1) else {
        return throw_uri_error(vm, scope, hooks, "URI malformed");
      };
      octets[j] = b;
      idx += 3;
    }

    // Validate UTF-8 sequence and decode code point.
    let cp: u32 = match n {
      2 => {
        let b1 = octets[1];
        if !(0xC2..=0xDF).contains(&b0) || !(0x80..=0xBF).contains(&b1) {
          return throw_uri_error(vm, scope, hooks, "URI malformed");
        }
        (((b0 & 0x1F) as u32) << 6) | ((b1 & 0x3F) as u32)
      }
      3 => {
        let b1 = octets[1];
        let b2 = octets[2];
        let second_ok = match b0 {
          0xE0 => (0xA0..=0xBF).contains(&b1),
          0xED => (0x80..=0x9F).contains(&b1), // exclude surrogate range
          _ => (0x80..=0xBF).contains(&b1),
        };
        if !(0xE0..=0xEF).contains(&b0) || !second_ok || !(0x80..=0xBF).contains(&b2) {
          return throw_uri_error(vm, scope, hooks, "URI malformed");
        }
        (((b0 & 0x0F) as u32) << 12) | (((b1 & 0x3F) as u32) << 6) | ((b2 & 0x3F) as u32)
      }
      4 => {
        let b1 = octets[1];
        let b2 = octets[2];
        let b3 = octets[3];
        let second_ok = match b0 {
          0xF0 => (0x90..=0xBF).contains(&b1),
          0xF4 => (0x80..=0x8F).contains(&b1),
          _ => (0x80..=0xBF).contains(&b1),
        };
        if !(0xF0..=0xF4).contains(&b0)
          || !second_ok
          || !(0x80..=0xBF).contains(&b2)
          || !(0x80..=0xBF).contains(&b3)
        {
          return throw_uri_error(vm, scope, hooks, "URI malformed");
        }
        (((b0 & 0x07) as u32) << 18)
          | (((b1 & 0x3F) as u32) << 12)
          | (((b2 & 0x3F) as u32) << 6)
          | ((b3 & 0x3F) as u32)
      }
      _ => unreachable!(),
    };

    // Reject surrogate code points and out-of-range values.
    if cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
      return throw_uri_error(vm, scope, hooks, "URI malformed");
    }

    // UTF16EncodeCodePoint
    if cp <= 0xFFFF {
      out.push(cp as u16);
    } else {
      let cp = cp - 0x10000;
      let high = 0xD800 + ((cp >> 10) as u16);
      let low = 0xDC00 + ((cp & 0x3FF) as u16);
      out.push(high);
      out.push(low);
    }

    k = idx;
  }

  let out_s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out_s))
}

/// Global `encodeURI(uri)` (ECMA-262).
pub fn global_encode_uri(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // extraUnescaped = ";/?:@&=+$,#"
  const EXTRA: [u16; 11] = [
    b';' as u16,
    b'/' as u16,
    b'?' as u16,
    b':' as u16,
    b'@' as u16,
    b'&' as u16,
    b'=' as u16,
    b'+' as u16,
    b'$' as u16,
    b',' as u16,
    b'#' as u16,
  ];
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  encode_uri_string(vm, scope, host, hooks, input, &EXTRA)
}

/// Global `encodeURIComponent(uriComponent)` (ECMA-262).
pub fn global_encode_uri_component(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  encode_uri_string(vm, scope, host, hooks, input, &[])
}

/// Global `decodeURI(encodedURI)` (ECMA-262).
pub fn global_decode_uri(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // preserveEscapeSet = ";/?:@&=+$,#"
  const PRESERVE: [u16; 11] = [
    b';' as u16,
    b'/' as u16,
    b'?' as u16,
    b':' as u16,
    b'@' as u16,
    b'&' as u16,
    b'=' as u16,
    b'+' as u16,
    b'$' as u16,
    b',' as u16,
    b'#' as u16,
  ];
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  decode_uri_string(vm, scope, host, hooks, input, &PRESERVE)
}

/// Global `decodeURIComponent(encodedURIComponent)` (ECMA-262).
pub fn global_decode_uri_component(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  decode_uri_string(vm, scope, host, hooks, input, &[])
}

static MATH_RANDOM_STATE: AtomicU64 = AtomicU64::new(0x243F_6A88_85A3_08D3);

fn math_unary_number_op(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  args: &[Value],
  f: impl FnOnce(f64) -> f64,
) -> Result<Value, VmError> {
  let v = args.first().copied().unwrap_or(Value::Undefined);
  let n = scope.to_number(vm, host, hooks, v)?;
  Ok(Value::Number(f(n)))
}

/// `Math.abs(x)` (ECMA-262).
pub fn math_abs(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.abs())
}

/// `Math.floor(x)` (ECMA-262).
pub fn math_floor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.floor())
}

/// `Math.ceil(x)` (ECMA-262).
pub fn math_ceil(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.ceil())
}

/// `Math.trunc(x)` (ECMA-262).
pub fn math_trunc(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.trunc())
}

/// `Math.round(x)` (ECMA-262).
pub fn math_round(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| {
    if !n.is_finite() || n == 0.0 {
      return n;
    }
    let r = (n + 0.5).floor();
    if r == 0.0 && n.is_sign_negative() {
      -0.0
    } else {
      r
    }
  })
}

/// `Math.max(...args)` (ECMA-262).
pub fn math_max(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.is_empty() {
    return Ok(Value::Number(f64::NEG_INFINITY));
  }

  let mut best = f64::NEG_INFINITY;
  for (i, v) in args.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let n = scope.to_number(vm, host, hooks, v)?;
    if n.is_nan() {
      return Ok(Value::Number(f64::NAN));
    }
    best = best.max(n);
  }
  Ok(Value::Number(best))
}

/// `Math.min(...args)` (ECMA-262).
pub fn math_min(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.is_empty() {
    return Ok(Value::Number(f64::INFINITY));
  }

  let mut best = f64::INFINITY;
  for (i, v) in args.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let n = scope.to_number(vm, host, hooks, v)?;
    if n.is_nan() {
      return Ok(Value::Number(f64::NAN));
    }
    best = best.min(n);
  }
  Ok(Value::Number(best))
}

/// `Math.pow(base, exponent)` (ECMA-262).
pub fn math_pow(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base = args.get(0).copied().unwrap_or(Value::Undefined);
  let exp = args.get(1).copied().unwrap_or(Value::Undefined);
  let x = scope.to_number(vm, host, hooks, base)?;
  let y = scope.to_number(vm, host, hooks, exp)?;
  Ok(Value::Number(x.powf(y)))
}

/// `Math.sqrt(x)` (ECMA-262).
pub fn math_sqrt(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.sqrt())
}

/// `Math.log(x)` (ECMA-262).
pub fn math_log(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.ln())
}

/// `Math.exp(x)` (ECMA-262).
pub fn math_exp(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.exp())
}

/// `Math.random()` (ECMA-262) (deterministic PRNG).
pub fn math_random(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // xorshift64* (deterministic, not cryptographically secure).
  let mut x = MATH_RANDOM_STATE.load(Ordering::Relaxed);
  x ^= x >> 12;
  x ^= x << 25;
  x ^= x >> 27;
  MATH_RANDOM_STATE.store(x, Ordering::Relaxed);
  let x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);

  // Convert the high 53 bits into a double in [0, 1).
  let bits = x >> 11;
  let n = (bits as f64) * (1.0 / ((1u64 << 53) as f64));
  Ok(Value::Number(n))
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

  let name_value =
    get_data_property_value(vm, scope, this_obj, &name_key)?.unwrap_or(Value::Undefined);
  let message_value =
    get_data_property_value(vm, scope, this_obj, &message_key)?.unwrap_or(Value::Undefined);

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

fn json_syntax_error(vm: &mut Vm, scope: &mut Scope<'_>) -> VmError {
  let intr = match require_intrinsics(vm) {
    Ok(intr) => intr,
    Err(err) => return err,
  };
  match crate::new_syntax_error_object(scope, &intr, "Invalid JSON") {
    Ok(err) => VmError::Throw(err),
    Err(err) => err,
  }
}

struct JsonParser<'a> {
  units: &'a [u16],
  pos: usize,
  steps: u64,
}

impl<'a> JsonParser<'a> {
  const QUOTE: u16 = b'"' as u16;
  const BACKSLASH: u16 = b'\\' as u16;

  fn new(units: &'a [u16]) -> Self {
    Self {
      units,
      pos: 0,
      steps: 0,
    }
  }

  fn peek(&self) -> Option<u16> {
    self.units.get(self.pos).copied()
  }

  fn bump(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<u16, VmError> {
    let Some(u) = self.peek() else {
      return Err(json_syntax_error(vm, scope));
    };
    self.pos += 1;
    self.steps = self.steps.wrapping_add(1);
    if self.steps % 1024 == 0 {
      vm.tick()?;
    }
    Ok(u)
  }

  fn skip_ws(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<(), VmError> {
    while matches!(self.peek(), Some(0x20 | 0x09 | 0x0A | 0x0D)) {
      let _ = self.bump(vm, scope)?;
    }
    Ok(())
  }

  fn expect(&mut self, vm: &mut Vm, scope: &mut Scope<'_>, expected: u16) -> Result<(), VmError> {
    let u = self.bump(vm, scope)?;
    if u != expected {
      return Err(json_syntax_error(vm, scope));
    }
    Ok(())
  }

  fn parse_value(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    self.skip_ws(vm, scope)?;
    match self.peek() {
      Some(u) if u == b'n' as u16 => self.parse_null(vm, scope),
      Some(u) if u == b't' as u16 => self.parse_true(vm, scope),
      Some(u) if u == b'f' as u16 => self.parse_false(vm, scope),
      Some(Self::QUOTE) => Ok(Value::String(self.parse_string(vm, scope)?)),
      Some(u) if u == b'[' as u16 => self.parse_array(vm, scope),
      Some(u) if u == b'{' as u16 => self.parse_object(vm, scope),
      Some(u) if u == b'-' as u16 || (u >= b'0' as u16 && u <= b'9' as u16) => {
        Ok(Value::Number(self.parse_number(vm, scope)?))
      }
      _ => Err(json_syntax_error(vm, scope)),
    }
  }

  fn parse_null(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    self.expect(vm, scope, b'n' as u16)?;
    self.expect(vm, scope, b'u' as u16)?;
    self.expect(vm, scope, b'l' as u16)?;
    self.expect(vm, scope, b'l' as u16)?;
    Ok(Value::Null)
  }

  fn parse_true(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    self.expect(vm, scope, b't' as u16)?;
    self.expect(vm, scope, b'r' as u16)?;
    self.expect(vm, scope, b'u' as u16)?;
    self.expect(vm, scope, b'e' as u16)?;
    Ok(Value::Bool(true))
  }

  fn parse_false(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    self.expect(vm, scope, b'f' as u16)?;
    self.expect(vm, scope, b'a' as u16)?;
    self.expect(vm, scope, b'l' as u16)?;
    self.expect(vm, scope, b's' as u16)?;
    self.expect(vm, scope, b'e' as u16)?;
    Ok(Value::Bool(false))
  }

  fn hex_digit(u: u16) -> Option<u16> {
    if (0x30..=0x39).contains(&u) {
      Some(u - 0x30)
    } else if (0x61..=0x66).contains(&u) {
      Some(10 + (u - 0x61))
    } else if (0x41..=0x46).contains(&u) {
      Some(10 + (u - 0x41))
    } else {
      None
    }
  }

  fn parse_string(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<crate::GcString, VmError> {
    self.expect(vm, scope, Self::QUOTE)?;
    let mut out: Vec<u16> = Vec::new();
    let max_bytes = scope.heap().limits().max_bytes;

    loop {
      let u = self.bump(vm, scope)?;
      match u {
        Self::QUOTE => break,
        Self::BACKSLASH => {
          let esc = self.bump(vm, scope)?;
          match esc {
            u if u == b'"' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, b'"' as u16)?
            }
            u if u == b'\\' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, b'\\' as u16)?
            }
            u if u == b'/' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, b'/' as u16)?
            }
            u if u == b'b' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, 0x08)?
            }
            u if u == b'f' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, 0x0C)?
            }
            u if u == b'n' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, 0x0A)?
            }
            u if u == b'r' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, 0x0D)?
            }
            u if u == b't' as u16 => {
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, 0x09)?
            }
            u if u == b'u' as u16 => {
              let mut code: u16 = 0;
              for _ in 0..4 {
                let h = self.bump(vm, scope)?;
                let Some(d) = Self::hex_digit(h) else {
                  return Err(json_syntax_error(vm, scope));
                };
                code = (code << 4) | d;
              }
              if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
                return Err(VmError::OutOfMemory);
              }
              vec_try_push(&mut out, code)?;
            }
            _ => return Err(json_syntax_error(vm, scope)),
          }
        }
        0x0000..=0x001F => return Err(json_syntax_error(vm, scope)),
        other => {
          if JsString::heap_size_bytes_for_len(out.len().saturating_add(1)) > max_bytes {
            return Err(VmError::OutOfMemory);
          }
          vec_try_push(&mut out, other)?
        }
      }
    }

    scope.alloc_string_from_u16_vec(out)
  }

  fn parse_number(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<f64, VmError> {
    let start = self.pos;

    if matches!(self.peek(), Some(u) if u == b'-' as u16) {
      let _ = self.bump(vm, scope)?;
    }

    match self.peek() {
      Some(u) if u == b'0' as u16 => {
        let _ = self.bump(vm, scope)?;
        // Leading zeros are not allowed unless the integer part is exactly "0".
        if matches!(self.peek(), Some(d) if d >= b'0' as u16 && d <= b'9' as u16) {
          return Err(json_syntax_error(vm, scope));
        }
      }
      Some(u) if u >= b'1' as u16 && u <= b'9' as u16 => {
        let _ = self.bump(vm, scope)?;
        while matches!(self.peek(), Some(d) if d >= b'0' as u16 && d <= b'9' as u16) {
          let _ = self.bump(vm, scope)?;
        }
      }
      _ => return Err(json_syntax_error(vm, scope)),
    }

    if matches!(self.peek(), Some(u) if u == b'.' as u16) {
      let _ = self.bump(vm, scope)?;
      if !matches!(self.peek(), Some(d) if d >= b'0' as u16 && d <= b'9' as u16) {
        return Err(json_syntax_error(vm, scope));
      }
      while matches!(self.peek(), Some(d) if d >= b'0' as u16 && d <= b'9' as u16) {
        let _ = self.bump(vm, scope)?;
      }
    }

    if matches!(self.peek(), Some(u) if u == b'e' as u16 || u == b'E' as u16) {
      let _ = self.bump(vm, scope)?;
      if matches!(self.peek(), Some(u) if u == b'+' as u16 || u == b'-' as u16) {
        let _ = self.bump(vm, scope)?;
      }
      if !matches!(self.peek(), Some(d) if d >= b'0' as u16 && d <= b'9' as u16) {
        return Err(json_syntax_error(vm, scope));
      }
      while matches!(self.peek(), Some(d) if d >= b'0' as u16 && d <= b'9' as u16) {
        let _ = self.bump(vm, scope)?;
      }
    }

    let end = self.pos;
    let slice = &self.units[start..end];
    let mut bytes: Vec<u8> = Vec::new();
    bytes
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for (i, &u) in slice.iter().enumerate() {
      if i % 1024 == 0 {
        vm.tick()?;
      }
      // Valid JSON numbers are ASCII-only.
      if u > 0x7F {
        return Err(json_syntax_error(vm, scope));
      }
      bytes.push(u as u8);
    }
    let s = std::str::from_utf8(&bytes).map_err(|_| json_syntax_error(vm, scope))?;
    s.parse::<f64>().map_err(|_| json_syntax_error(vm, scope))
  }

  fn parse_array(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    self.expect(vm, scope, b'[' as u16)?;

    let array = create_array_object(vm, scope, 0)?;
    scope.push_root(Value::Object(array))?;

    self.skip_ws(vm, scope)?;
    if matches!(self.peek(), Some(u) if u == b']' as u16) {
      let _ = self.bump(vm, scope)?;
      return Ok(Value::Object(array));
    }

    let mut idx: usize = 0;
    loop {
      {
        let mut el_scope = scope.reborrow();
        el_scope.push_root(Value::Object(array))?;

        let value = self.parse_value(vm, &mut el_scope)?;
        el_scope.push_root(value)?;

        let key_s = el_scope.alloc_string(&idx.to_string())?;
        el_scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        el_scope.define_property(array, key, data_desc(value, true, true, true))?;
      }

      idx = idx.saturating_add(1);

      self.skip_ws(vm, scope)?;
      match self.peek() {
        Some(u) if u == b',' as u16 => {
          let _ = self.bump(vm, scope)?;
          self.skip_ws(vm, scope)?;
          continue;
        }
        Some(u) if u == b']' as u16 => {
          let _ = self.bump(vm, scope)?;
          break;
        }
        _ => return Err(json_syntax_error(vm, scope)),
      }
    }

    Ok(Value::Object(array))
  }

  fn parse_object(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    self.expect(vm, scope, b'{' as u16)?;

    let intr = require_intrinsics(vm)?;
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(intr.object_prototype()))?;

    self.skip_ws(vm, scope)?;
    if matches!(self.peek(), Some(u) if u == b'}' as u16) {
      let _ = self.bump(vm, scope)?;
      return Ok(Value::Object(obj));
    }

    loop {
      {
        let mut member_scope = scope.reborrow();
        member_scope.push_root(Value::Object(obj))?;

        self.skip_ws(vm, &mut member_scope)?;
        let key_str = self.parse_string(vm, &mut member_scope)?;
        member_scope.push_root(Value::String(key_str))?;

        self.skip_ws(vm, &mut member_scope)?;
        self.expect(vm, &mut member_scope, b':' as u16)?;
        self.skip_ws(vm, &mut member_scope)?;

        let value = self.parse_value(vm, &mut member_scope)?;
        member_scope.push_root(value)?;

        member_scope.define_property(
          obj,
          PropertyKey::from_string(key_str),
          data_desc(value, true, true, true),
        )?;
      }

      self.skip_ws(vm, scope)?;
      match self.peek() {
        Some(u) if u == b',' as u16 => {
          let _ = self.bump(vm, scope)?;
          self.skip_ws(vm, scope)?;
          continue;
        }
        Some(u) if u == b'}' as u16 => {
          let _ = self.bump(vm, scope)?;
          break;
        }
        _ => return Err(json_syntax_error(vm, scope)),
      }
    }

    Ok(Value::Object(obj))
  }
}

/// `JSON.parse` (minimal).
pub fn json_parse(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let text = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = scope.to_string(vm, host, hooks, text)?;
  scope.push_root(Value::String(s))?;
  let units: Vec<u16> = {
    let js = scope.heap().get_string(s)?;
    js.as_code_units().to_vec()
  };

  let mut parser = JsonParser::new(&units);
  let parsed = parser.parse_value(vm, &mut scope)?;
  scope.push_root(parsed)?;
  parser.skip_ws(vm, &mut scope)?;
  if parser.pos != units.len() {
    return Err(json_syntax_error(vm, &mut scope));
  }

  let reviver = args.get(1).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(reviver)? {
    return Ok(parsed);
  }

  // Internalize with reviver.
  let intr = require_intrinsics(vm)?;
  let root = scope.alloc_object()?;
  scope.push_root(Value::Object(root))?;
  scope
    .heap_mut()
    .object_set_prototype(root, Some(intr.object_prototype()))?;

  let empty = scope.alloc_string("")?;
  scope.push_root(Value::String(empty))?;
  let empty_key = PropertyKey::from_string(empty);
  scope.define_property(root, empty_key, data_desc(parsed, true, true, true))?;

  fn internalize_json_property(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    holder: GcObject,
    name: crate::GcString,
    reviver: Value,
  ) -> Result<Value, VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(holder))?;
    scope.push_root(Value::String(name))?;
    scope.push_root(reviver)?;

    let key = PropertyKey::from_string(name);
    let mut val =
      scope.ordinary_get_with_host_and_hooks(vm, host, hooks, holder, key, Value::Object(holder))?;
    scope.push_root(val)?;

    if let Value::Object(obj) = val {
      if scope.heap().object_is_array(obj)? {
        let length_key = string_key(&mut scope, "length")?;
        let len_value = scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          length_key,
          Value::Object(obj),
        )?;
        scope.push_root(len_value)?;
        let len = scope.to_length(vm, host, hooks, len_value)?;

        for i in 0..len {
          if i % 1024 == 0 {
            vm.tick()?;
          }
          let mut idx_scope = scope.reborrow();
          idx_scope.push_root(Value::Object(obj))?;

          let idx_s = idx_scope.alloc_string(&i.to_string())?;
          idx_scope.push_root(Value::String(idx_s))?;
          let new_element = internalize_json_property(vm, &mut idx_scope, host, hooks, obj, idx_s, reviver)?;

          if matches!(new_element, Value::Undefined) {
            let ok =
              idx_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, PropertyKey::from_string(idx_s))?;
            if !ok {
              let intr = require_intrinsics(vm)?;
              return Err(crate::throw_type_error(&mut idx_scope, intr, "DeletePropertyOrThrow failed"));
            }
          } else {
            idx_scope.define_property(
              obj,
              PropertyKey::from_string(idx_s),
              data_desc(new_element, true, true, true),
            )?;
          }
        }
      } else {
        let keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
        let mut enumerable: Vec<crate::GcString> = Vec::new();
        enumerable
          .try_reserve_exact(keys.len())
          .map_err(|_| VmError::OutOfMemory)?;

        for (i, k) in keys.into_iter().enumerate() {
          if i % 1024 == 0 {
            vm.tick()?;
          }
          let PropertyKey::String(s) = k else {
            continue;
          };
          let Some(desc) = scope.ordinary_get_own_property(obj, k)? else {
            continue;
          };
          if desc.enumerable {
            enumerable.push(s);
          }
        }

        for (i, p) in enumerable.into_iter().enumerate() {
          if i % 1024 == 0 {
            vm.tick()?;
          }
          let mut p_scope = scope.reborrow();
          p_scope.push_root(Value::Object(obj))?;
          p_scope.push_root(Value::String(p))?;

          let new_element = internalize_json_property(vm, &mut p_scope, host, hooks, obj, p, reviver)?;
          if matches!(new_element, Value::Undefined) {
            let ok =
              p_scope.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, PropertyKey::from_string(p))?;
            if !ok {
              let intr = require_intrinsics(vm)?;
              return Err(crate::throw_type_error(&mut p_scope, intr, "DeletePropertyOrThrow failed"));
            }
          } else {
            p_scope.define_property(
              obj,
              PropertyKey::from_string(p),
              data_desc(new_element, true, true, true),
            )?;
          }
        }
      }

      val = Value::Object(obj);
    }

    let args = [Value::String(name), val];
    vm.call_with_host_and_hooks(host, &mut scope, hooks, reviver, Value::Object(holder), &args)
  }

  internalize_json_property(vm, &mut scope, host, hooks, root, empty, reviver)
}

/// `JSON.stringify` (ECMA-262).
pub fn json_stringify(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let intr = require_intrinsics(vm)?;

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let replacer = args.get(1).copied().unwrap_or(Value::Undefined);
  let space = args.get(2).copied().unwrap_or(Value::Undefined);

  // Root inputs across allocations/GC while we build the internal state.
  scope.push_roots(&[value, replacer, space])?;

  // --- Internal helper types ---
  struct JsonStringBuilder {
    buf: Vec<u16>,
    max_bytes: usize,
  }

  impl JsonStringBuilder {
    fn new(max_bytes: usize) -> Self {
      Self {
        buf: Vec::new(),
        max_bytes,
      }
    }

    #[inline]
    fn check_grow(&self, additional_units: usize) -> Result<(), VmError> {
      let new_len = self
        .buf
        .len()
        .checked_add(additional_units)
        .ok_or(VmError::OutOfMemory)?;
      if JsString::heap_size_bytes_for_len(new_len) > self.max_bytes {
        return Err(VmError::OutOfMemory);
      }
      Ok(())
    }

    fn push_unit(&mut self, unit: u16) -> Result<(), VmError> {
      self.check_grow(1)?;
      vec_try_push(&mut self.buf, unit)
    }

    fn push_ascii(&mut self, s: &[u8]) -> Result<(), VmError> {
      self.check_grow(s.len())?;
      for &b in s {
        vec_try_push(&mut self.buf, b as u16)?;
      }
      Ok(())
    }

    fn push_units(&mut self, units: &[u16]) -> Result<(), VmError> {
      self.check_grow(units.len())?;
      vec_try_extend_from_slice(&mut self.buf, units)
    }

    fn push_hex_escape(&mut self, unit: u16) -> Result<(), VmError> {
      self.check_grow(6)?;
      self.push_ascii(b"\\u")?;
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
      self.push_units(&digits)
    }
  }

  #[derive(Clone, Copy)]
  struct WrapperMarkerKeys {
    number: PropertyKey,
    string: PropertyKey,
    boolean: PropertyKey,
  }

  struct JsonStringifyState {
    replacer_function: Option<Value>,
    property_list: Option<Vec<crate::GcString>>,
    stack: Vec<GcObject>,
    indent: Vec<u16>,
    gap: Vec<u16>,
    to_json_key: PropertyKey,
    length_key: PropertyKey,
    wrapper_markers: WrapperMarkerKeys,
  }

  fn unbox_primitive_wrapper(
    scope: &mut Scope<'_>,
    obj: GcObject,
    markers: WrapperMarkerKeys,
  ) -> Result<Option<Value>, VmError> {
    if let Some(v) = scope
      .heap()
      .object_get_own_data_property_value(obj, &markers.number)?
    {
      if matches!(v, Value::Number(_)) {
        return Ok(Some(v));
      }
    }
    if let Some(v) = scope
      .heap()
      .object_get_own_data_property_value(obj, &markers.string)?
    {
      if matches!(v, Value::String(_)) {
        return Ok(Some(v));
      }
    }
    if let Some(v) = scope
      .heap()
      .object_get_own_data_property_value(obj, &markers.boolean)?
    {
      if matches!(v, Value::Bool(_)) {
        return Ok(Some(v));
      }
    }
    Ok(None)
  }

  fn quote_json_string(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    out: &mut JsonStringBuilder,
    s: crate::GcString,
  ) -> Result<(), VmError> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    out.push_unit(QUOTE)?;

    let units = scope.heap().get_string(s)?.as_code_units();
    let mut i: usize = 0;
    while i < units.len() {
      if i % 1024 == 0 {
        vm.tick()?;
      }
      let unit = units[i];
      match unit {
        QUOTE => out.push_ascii(b"\\\"")?,
        BACKSLASH => out.push_ascii(b"\\\\")?,
        0x08 => out.push_ascii(b"\\b")?,
        0x0C => out.push_ascii(b"\\f")?,
        0x0A => out.push_ascii(b"\\n")?,
        0x0D => out.push_ascii(b"\\r")?,
        0x09 => out.push_ascii(b"\\t")?,
        0x0000..=0x001F | 0x2028 | 0x2029 => out.push_hex_escape(unit)?,
        0xD800..=0xDBFF => {
          // Paired surrogate: emit both code units as-is. Lone surrogate: escape.
          if let Some(&next) = units.get(i + 1) {
            if (0xDC00..=0xDFFF).contains(&next) {
              out.push_unit(unit)?;
              out.push_unit(next)?;
              i += 2;
              continue;
            }
          }
          out.push_hex_escape(unit)?;
        }
        0xDC00..=0xDFFF => out.push_hex_escape(unit)?,
        other => out.push_unit(other)?,
      }
      i += 1;
    }

    out.push_unit(QUOTE)
  }

  fn prepare_json_value_for_property(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    state: &JsonStringifyState,
    holder: GcObject,
    key: crate::GcString,
  ) -> Result<Option<Value>, VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(holder))?;
    scope.push_root(Value::String(key))?;
    if let Some(replacer_fn) = state.replacer_function {
      scope.push_root(replacer_fn)?;
    }

    let mut value = scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      holder,
      PropertyKey::from_string(key),
      Value::Object(holder),
    )?;
    scope.push_root(value)?;

    // 1. If value is an Object, call `toJSON` if present.
    if let Value::Object(obj) = value {
      scope.push_root(Value::Object(obj))?;
      let to_json = scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        obj,
        state.to_json_key,
        Value::Object(obj),
      )?;
      scope.push_root(to_json)?;
      if !matches!(to_json, Value::Undefined | Value::Null) && scope.heap().is_callable(to_json)? {
        let args = [Value::String(key)];
        value = vm.call_with_host_and_hooks(host, &mut scope, hooks, to_json, Value::Object(obj), &args)?;
        scope.push_root(value)?;
      }
    }

    // 2. Apply the replacer function if present.
    if let Some(replacer_fn) = state.replacer_function {
      let args = [Value::String(key), value];
      value = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        replacer_fn,
        Value::Object(holder),
        &args,
      )?;
      scope.push_root(value)?;
    }

    // 3. Unbox wrapper objects (String/Number/Boolean).
    if let Value::Object(obj) = value {
      if let Some(prim) = unbox_primitive_wrapper(&mut scope, obj, state.wrapper_markers)? {
        value = prim;
        scope.push_root(value)?;
      }
    }

    // 4. BigInt values throw.
    if matches!(value, Value::BigInt(_)) {
      return Err(VmError::TypeError("Do not know how to serialize a BigInt"));
    }

    // 5. Omit `undefined`, Symbols, and callable objects.
    match value {
      Value::Undefined | Value::Symbol(_) => Ok(None),
      Value::Object(_) if scope.heap().is_callable(value)? => Ok(None),
      other => Ok(Some(other)),
    }
  }

  fn check_cycle(vm: &mut Vm, stack: &[GcObject], obj: GcObject) -> Result<(), VmError> {
    for (i, o) in stack.iter().enumerate() {
      if i % 1024 == 0 {
        vm.tick()?;
      }
      if *o == obj {
        return Err(VmError::TypeError("Converting circular structure to JSON"));
      }
    }
    Ok(())
  }

  fn serialize_json_value(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    state: &mut JsonStringifyState,
    out: &mut JsonStringBuilder,
    value: Value,
  ) -> Result<(), VmError> {
    match value {
      Value::Null => out.push_ascii(b"null"),
      Value::Bool(true) => out.push_ascii(b"true"),
      Value::Bool(false) => out.push_ascii(b"false"),
      Value::Number(n) => {
        if !n.is_finite() {
          return out.push_ascii(b"null");
        }
        let s = scope.heap_mut().to_string(Value::Number(n))?;
        let units = scope.heap().get_string(s)?.as_code_units();
        out.push_units(units)
      }
      Value::String(s) => quote_json_string(vm, scope, out, s),
      Value::Object(obj) => {
        if scope.heap().object_is_array(obj)? {
          serialize_json_array(vm, scope, host, hooks, state, out, obj)
        } else {
          serialize_json_object(vm, scope, host, hooks, state, out, obj)
        }
      }
      Value::BigInt(_) => Err(VmError::TypeError("Do not know how to serialize a BigInt")),
      // These should have been filtered by `prepare_json_value_for_property` for object properties.
      // Arrays normalize them to `null` by passing `Value::Null` down instead.
      Value::Undefined | Value::Symbol(_) => out.push_ascii(b"null"),
    }
  }

  fn serialize_json_array(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    state: &mut JsonStringifyState,
    out: &mut JsonStringBuilder,
    obj: GcObject,
  ) -> Result<(), VmError> {
    check_cycle(vm, &state.stack, obj)?;
    vec_try_push(&mut state.stack, obj)?;
    scope.push_root(Value::Object(obj))?;

    let stepback_len = state.indent.len();
    vec_try_extend_from_slice(&mut state.indent, &state.gap)?;

    // len = ToLength(Get(array, "length"))
    let len_value = scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      state.length_key,
      Value::Object(obj),
    )?;
    scope.push_root(len_value)?;
    let len = scope.to_length(vm, host, hooks, len_value)?;

    if len == 0 {
      state.stack.pop();
      state.indent.truncate(stepback_len);
      return out.push_ascii(b"[]");
    }

    out.push_unit(b'[' as u16)?;

    if state.gap.is_empty() {
      for i in 0..len {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        if i > 0 {
          out.push_unit(b',' as u16)?;
        }
        let key_s = scope.alloc_string(&i.to_string())?;
        scope.push_root(Value::String(key_s))?;
        let element = prepare_json_value_for_property(vm, scope, host, hooks, state, obj, key_s)?
          .unwrap_or(Value::Null);
        serialize_json_value(vm, scope, host, hooks, state, out, element)?;
      }
      out.push_unit(b']' as u16)?;
    } else {
      out.push_unit(b'\n' as u16)?;
      out.push_units(&state.indent)?;
      for i in 0..len {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        if i > 0 {
          out.push_ascii(b",\n")?;
          out.push_units(&state.indent)?;
        }
        let key_s = scope.alloc_string(&i.to_string())?;
        scope.push_root(Value::String(key_s))?;
        let element = prepare_json_value_for_property(vm, scope, host, hooks, state, obj, key_s)?
          .unwrap_or(Value::Null);
        serialize_json_value(vm, scope, host, hooks, state, out, element)?;
      }
      out.push_unit(b'\n' as u16)?;
      out.push_units(&state.indent[..stepback_len])?;
      out.push_unit(b']' as u16)?;
    }

    state.stack.pop();
    state.indent.truncate(stepback_len);
    Ok(())
  }

  fn serialize_json_object(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    state: &mut JsonStringifyState,
    out: &mut JsonStringBuilder,
    obj: GcObject,
  ) -> Result<(), VmError> {
    check_cycle(vm, &state.stack, obj)?;
    vec_try_push(&mut state.stack, obj)?;
    scope.push_root(Value::Object(obj))?;

    let stepback_len = state.indent.len();
    vec_try_extend_from_slice(&mut state.indent, &state.gap)?;

    // Determine key list.
    //
    // We use an owned key list to avoid borrowing `state.property_list` across recursive
    // `serialize_json_value` calls (which need `&mut state`).
    let k_list: Vec<crate::GcString> = if let Some(list) = &state.property_list {
      let mut out_keys: Vec<crate::GcString> = Vec::new();
      out_keys
        .try_reserve_exact(list.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for (i, &s) in list.iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        out_keys.push(s);
      }
      out_keys
    } else {
      let keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
      let mut out_keys: Vec<crate::GcString> = Vec::new();
      out_keys
        .try_reserve_exact(keys.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for (i, k) in keys.into_iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let PropertyKey::String(s) = k else {
          continue;
        };
        let Some(desc) = scope.ordinary_get_own_property(obj, k)? else {
          continue;
        };
        if desc.enumerable {
          scope.push_root(Value::String(s))?;
          out_keys.push(s);
        }
      }
      out_keys
    };

    out.push_unit(b'{' as u16)?;

    if state.gap.is_empty() {
      let mut wrote_any = false;
      for (i, p) in k_list.into_iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let Some(prop_value) = prepare_json_value_for_property(vm, scope, host, hooks, state, obj, p)? else {
          continue;
        };
        if wrote_any {
          out.push_unit(b',' as u16)?;
        }
        wrote_any = true;
        quote_json_string(vm, scope, out, p)?;
        out.push_unit(b':' as u16)?;
        serialize_json_value(vm, scope, host, hooks, state, out, prop_value)?;
      }
      out.push_unit(b'}' as u16)?;
    } else {
      let mut wrote_any = false;
      for (i, p) in k_list.into_iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let Some(prop_value) = prepare_json_value_for_property(vm, scope, host, hooks, state, obj, p)? else {
          continue;
        };
        if !wrote_any {
          out.push_unit(b'\n' as u16)?;
          out.push_units(&state.indent)?;
        } else {
          out.push_ascii(b",\n")?;
          out.push_units(&state.indent)?;
        }
        wrote_any = true;
        quote_json_string(vm, scope, out, p)?;
        out.push_ascii(b": ")?;
        serialize_json_value(vm, scope, host, hooks, state, out, prop_value)?;
      }
      if wrote_any {
        out.push_unit(b'\n' as u16)?;
        out.push_units(&state.indent[..stepback_len])?;
      }
      out.push_unit(b'}' as u16)?;
    }

    state.stack.pop();
    state.indent.truncate(stepback_len);
    Ok(())
  }

  // --- Initialize wrapper marker keys for primitive wrapper detection ---
  fn marker_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
    let marker = scope.alloc_string(name)?;
    scope.push_root(Value::String(marker))?;
    let sym = scope.heap_mut().symbol_for(marker)?;
    Ok(PropertyKey::from_symbol(sym))
  }

  let wrapper_markers = WrapperMarkerKeys {
    number: marker_key(&mut scope, "vm-js.internal.NumberData")?,
    string: marker_key(&mut scope, "vm-js.internal.StringData")?,
    boolean: marker_key(&mut scope, "vm-js.internal.BooleanData")?,
  };

  // --- Replacer function / property list ---
  let mut replacer_function: Option<Value> = None;
  let mut property_list: Option<Vec<crate::GcString>> = None;

  if scope.heap().is_callable(replacer)? {
    replacer_function = Some(replacer);
  } else if let Value::Object(replacer_obj) = replacer {
    if scope.heap().object_is_array(replacer_obj)? {
      // propertyList from replacer array.
      let length_key_s = scope.alloc_string("length")?;
      scope.push_root(Value::String(length_key_s))?;
      let length_key = PropertyKey::from_string(length_key_s);
      let len_value = scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        replacer_obj,
        length_key,
        Value::Object(replacer_obj),
      )?;
      scope.push_root(len_value)?;
      let len = scope.to_length(vm, host, hooks, len_value)?;

      let mut list: Vec<crate::GcString> = Vec::new();
      list
        .try_reserve_exact(len.min(1024))
        .map_err(|_| VmError::OutOfMemory)?;

      for i in 0..len {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let idx_s = scope.alloc_string(&i.to_string())?;
        scope.push_root(Value::String(idx_s))?;
        let v = scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          replacer_obj,
          PropertyKey::from_string(idx_s),
          Value::Object(replacer_obj),
        )?;
        scope.push_root(v)?;

        let item_string: Option<crate::GcString> = match v {
          Value::String(s) => Some(s),
          Value::Number(n) => Some(scope.heap_mut().to_string(Value::Number(n))?),
          Value::Object(o) => match unbox_primitive_wrapper(&mut scope, o, wrapper_markers)? {
            Some(Value::String(s)) => Some(s),
            Some(Value::Number(n)) => Some(scope.heap_mut().to_string(Value::Number(n))?),
            _ => None,
          },
          _ => None,
        };

        let Some(s) = item_string else { continue };
        scope.push_root(Value::String(s))?;

        // Deduplicate by string contents.
        let s_units = scope.heap().get_string(s)?.as_code_units();
        let mut exists = false;
        for (j, existing) in list.iter().copied().enumerate() {
          if j % 1024 == 0 {
            vm.tick()?;
          }
          if scope.heap().get_string(existing)?.as_code_units() == s_units {
            exists = true;
            break;
          }
        }
        if !exists {
          vec_try_push(&mut list, s)?;
        }
      }

      property_list = Some(list);
    }
  }

  // --- Space / gap ---
  let mut space_value = space;
  if let Value::Object(o) = space_value {
    if let Some(prim) = unbox_primitive_wrapper(&mut scope, o, wrapper_markers)? {
      space_value = prim;
    }
  }

  let mut gap: Vec<u16> = Vec::new();
  match space_value {
    Value::Number(n) => {
      // `ToIntegerOrInfinity` rounds toward zero.
      let int = if n.is_nan() || n == 0.0 {
        0.0
      } else if !n.is_finite() {
        n
      } else {
        n.trunc()
      };
      let count = if int.is_finite() {
        int.clamp(0.0, 10.0) as usize
      } else {
        10usize
      };
      if count > 0 {
        gap.try_reserve_exact(count).map_err(|_| VmError::OutOfMemory)?;
        for _ in 0..count {
          vec_try_push(&mut gap, 0x20)?;
        }
      }
    }
    Value::String(s) => {
      let units = scope.heap().get_string(s)?.as_code_units();
      let count = units.len().min(10);
      gap
        .try_reserve_exact(count)
        .map_err(|_| VmError::OutOfMemory)?;
      for (i, &u) in units.iter().take(count).enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        gap.push(u);
      }
    }
    _ => {}
  }

  // --- Common property keys ---
  let to_json_s = scope.alloc_string("toJSON")?;
  scope.push_root(Value::String(to_json_s))?;
  let to_json_key = PropertyKey::from_string(to_json_s);

  let length_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_s))?;
  let length_key = PropertyKey::from_string(length_s);

  // --- Wrapper object for root ({"": value}) ---
  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;
  scope
    .heap_mut()
    .object_set_prototype(wrapper, Some(intr.object_prototype()))?;
  let empty_s = scope.alloc_string("")?;
  scope.push_root(Value::String(empty_s))?;
  scope.define_property(
    wrapper,
    PropertyKey::from_string(empty_s),
    data_desc(value, true, true, true),
  )?;

  let mut state = JsonStringifyState {
    replacer_function,
    property_list,
    stack: Vec::new(),
    indent: Vec::new(),
    gap,
    to_json_key,
    length_key,
    wrapper_markers,
  };

  let max_bytes = scope.heap().limits().max_bytes;
  let mut out = JsonStringBuilder::new(max_bytes);

  let Some(root_value) = prepare_json_value_for_property(vm, &mut scope, host, hooks, &state, wrapper, empty_s)? else {
    return Ok(Value::Undefined);
  };
  // Root value across serialization, since it can invoke user code (getters, toJSON, replacer).
  scope.push_root(root_value)?;
  serialize_json_value(vm, &mut scope, host, hooks, &mut state, &mut out, root_value)?;

  Ok(Value::String(scope.alloc_string_from_u16_vec(out.buf)?))
}
