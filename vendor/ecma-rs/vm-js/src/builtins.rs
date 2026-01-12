use crate::function::{CallHandler, FunctionData, ThisMode};
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::regexp::{advance_string_index, compile_regexp, RegExpCompileError, RegExpFlags};
use crate::string::{utf16_to_utf8_lossy_with_tick, JsString};
use crate::{
  heap::TypedArrayKind,
  GcObject, GcString, Job, JobKind, PromiseCapability, PromiseHandle, PromiseReaction, PromiseReactionType,
  PromiseRejectionOperation, PromiseState, RealmId, RootId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, SourceText,
};
use parse_js::ast::expr::Expr;
use parse_js::ast::func::FuncBody;
use parse_js::ast::node::{Node, ParenthesizedExpr};
use parse_js::ast::stmt::Stmt;
use parse_js::{Dialect, ParseOptions, SourceType};
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Arc;

fn strict_mode_stmts_contain_with(
  vm: &mut Vm,
  stmts: &[Node<Stmt>],
) -> Result<bool, VmError> {
  const TICK_EVERY: usize = 32;
  for (i, stmt) in stmts.iter().enumerate() {
    if i % TICK_EVERY == 0 {
      vm.tick()?;
    }
    if strict_mode_stmt_contains_with(vm, stmt)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn strict_mode_stmt_contains_with(vm: &mut Vm, stmt: &Node<Stmt>) -> Result<bool, VmError> {
  // `with` is a strict mode early error (ECMA-262 14.11.1 Static Semantics: Early Errors).
  //
  // This helper is intentionally lightweight and is reused by dynamic function constructors (e.g.
  // `Function(...)` / `GeneratorFunction(...)`) so strict-mode syntax errors are reported at
  // *construction time* rather than at first execution.
  vm.tick()?;

  match &*stmt.stx {
    Stmt::With(_) => Ok(true),
    Stmt::Block(block) => strict_mode_stmts_contain_with(vm, &block.stx.body),
    Stmt::If(stmt) => {
      if strict_mode_stmt_contains_with(vm, &stmt.stx.consequent)? {
        return Ok(true);
      }
      if let Some(alt) = &stmt.stx.alternate {
        if strict_mode_stmt_contains_with(vm, alt)? {
          return Ok(true);
        }
      }
      Ok(false)
    }
    Stmt::Try(stmt) => {
      if strict_mode_stmts_contain_with(vm, &stmt.stx.wrapped.stx.body)? {
        return Ok(true);
      }
      if let Some(catch) = &stmt.stx.catch {
        if strict_mode_stmts_contain_with(vm, &catch.stx.body)? {
          return Ok(true);
        }
      }
      if let Some(finally) = &stmt.stx.finally {
        if strict_mode_stmts_contain_with(vm, &finally.stx.body)? {
          return Ok(true);
        }
      }
      Ok(false)
    }
    Stmt::While(stmt) => strict_mode_stmt_contains_with(vm, &stmt.stx.body),
    Stmt::DoWhile(stmt) => strict_mode_stmt_contains_with(vm, &stmt.stx.body),
    Stmt::ForTriple(stmt) => strict_mode_stmts_contain_with(vm, &stmt.stx.body.stx.body),
    Stmt::ForIn(stmt) => strict_mode_stmts_contain_with(vm, &stmt.stx.body.stx.body),
    Stmt::ForOf(stmt) => strict_mode_stmts_contain_with(vm, &stmt.stx.body.stx.body),
    Stmt::Label(stmt) => strict_mode_stmt_contains_with(vm, &stmt.stx.statement),
    Stmt::Switch(stmt) => {
      const BRANCH_TICK_EVERY: usize = 32;
      for (i, branch) in stmt.stx.branches.iter().enumerate() {
        if i % BRANCH_TICK_EVERY == 0 {
          vm.tick()?;
        }
        if strict_mode_stmts_contain_with(vm, &branch.stx.body)? {
          return Ok(true);
        }
      }
      Ok(false)
    }
    _ => Ok(false),
  }
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
fn symbol_descriptive_string(
  scope: &mut Scope<'_>,
  sym: crate::GcSymbol,
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<crate::GcString, VmError> {
  // Determine the description length up-front so we can allocate the output buffer without holding
  // heap borrows across allocations (which can trigger GC).
  let desc = scope.heap().get_symbol_description(sym)?;
  let desc_len = match desc {
    None => 0,
    Some(desc) => scope.heap().get_string(desc)?.as_code_units().len(),
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

  let total_len = PREFIX.len().saturating_add(desc_len).saturating_add(1);
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;
  vec_try_extend_from_slice(&mut out, &PREFIX, || tick())?;
  if let Some(desc) = desc {
    let units = scope.heap().get_string(desc)?.as_code_units();
    vec_try_extend_from_slice(&mut out, units, || tick())?;
  }
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

// https://tc39.es/ecma262/#sec-frompropertydescriptor
fn from_property_descriptor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  desc: PropertyDescriptor,
) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;

  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;

  let enumerable_key = string_key(scope, "enumerable")?;
  let configurable_key = string_key(scope, "configurable")?;

  scope.create_data_property_or_throw(out, enumerable_key, Value::Bool(desc.enumerable))?;
  scope.create_data_property_or_throw(out, configurable_key, Value::Bool(desc.configurable))?;

  match desc.kind {
    PropertyKind::Data { value, writable } => {
      scope.push_root(value)?;
      let value_key = string_key(scope, "value")?;
      let writable_key = string_key(scope, "writable")?;
      scope.create_data_property_or_throw(out, value_key, value)?;
      scope.create_data_property_or_throw(out, writable_key, Value::Bool(writable))?;
    }
    PropertyKind::Accessor { get, set } => {
      scope.push_root(get)?;
      scope.push_root(set)?;
      let get_key = string_key(scope, "get")?;
      let set_key = string_key(scope, "set")?;
      scope.create_data_property_or_throw(out, get_key, get)?;
      scope.create_data_property_or_throw(out, set_key, set)?;
    }
  }

  Ok(out)
}

// https://tc39.es/ecma262/#sec-defineproperties
fn define_properties(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target: GcObject,
  properties: Value,
) -> Result<(), VmError> {
  let props_obj = scope.to_object(vm, host, hooks, properties)?;

  // Root `target`/`props_obj` for the duration of the operation so intermediate descriptor values
  // are reachable even if GC is triggered during `Get` or descriptor conversion.
  scope.push_roots(&[Value::Object(target), Value::Object(props_obj)])?;

  let mut tick = Vm::tick;
  let keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, props_obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `props_obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;
  let mut descriptors: Vec<(PropertyKey, PropertyDescriptorPatch)> = Vec::new();
  descriptors
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  // Collect descriptors first (per spec) so any getters on `properties` run before we start
  // mutating `target`.
  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    // `descValue = ? Get(properties, key)`
    let desc_value = scope.get_with_host_and_hooks(
      vm,
      host,
      hooks,
      props_obj,
      key,
      Value::Object(props_obj),
    )?;

    // Root `key` + `desc_value` during conversion so `ToPropertyDescriptor` can allocate freely.
    let desc = {
      let mut convert_scope = scope.reborrow();
      convert_scope.push_root(desc_value)?;
      let desc_obj = require_object(desc_value)?;
      crate::to_property_descriptor_with_host_and_hooks(vm, &mut convert_scope, host, hooks, desc_obj)?
    };

    // Persist any descriptor values across the subsequent define loop. These can be newly-allocated
    // objects returned from accessors and otherwise unreachable from the heap.
    if let Some(v) = desc.value {
      scope.push_root(v)?;
    }
    if let Some(v) = desc.get {
      scope.push_root(v)?;
    }
    if let Some(v) = desc.set {
      scope.push_root(v)?;
    }

    descriptors.push((key, desc));
  }

  for (i, (key, desc)) in descriptors.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let mut define_scope = scope.reborrow();
    define_scope.define_property_or_throw_with_host_and_hooks(vm, host, hooks, target, key, desc)?;
  }

  Ok(())
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

      let marker_sym = match scope.heap().internal_symbol_data_symbol() {
        Some(sym) => sym,
        None => {
          // If this is the first time we've created a boxed Symbol object, the internal marker
          // symbol may not yet exist in the global registry. Create it with budget ticks while
          // inserting into the sorted registry.
          let marker = scope.alloc_string("vm-js.internal.SymbolData")?;
          scope.heap_mut().symbol_for_with_tick(marker, || vm.tick())?
        }
      };
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
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let desc_obj = require_object(args.get(2).copied().unwrap_or(Value::Undefined))?;
  scope.push_root(Value::Object(desc_obj))?;

  let patch = crate::to_property_descriptor_with_host_and_hooks(vm, &mut scope, host, hooks, desc_obj)?;
  scope.define_property_or_throw_with_host_and_hooks(vm, host, hooks, target, key, patch)?;
  Ok(Value::Object(target))
}

pub fn object_define_properties(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let props_val = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(props_val)?;

  define_properties(vm, scope, host, hooks, target, props_val)?;
  Ok(Value::Object(target))
}

pub fn object_create(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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

  // Root the prototype across allocation/GC.
  if let Some(proto_obj) = proto {
    scope.push_root(Value::Object(proto_obj))?;
  }

  let obj = scope.alloc_object_with_prototype(proto)?;
  scope.push_root(Value::Object(obj))?;

  if let Some(properties_object) = args.get(1).copied() {
    if !matches!(properties_object, Value::Undefined) {
      scope.push_root(properties_object)?;
      define_properties(vm, scope, host, hooks, obj, properties_object)?;
    }
  }

  Ok(Value::Object(obj))
}

/// `Object.is(value1, value2)` (ECMA-262).
pub fn object_is(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.is
  let value1 = args.get(0).copied().unwrap_or(Value::Undefined);
  let value2 = args.get(1).copied().unwrap_or(Value::Undefined);
  Ok(Value::Bool(value1.same_value(value2, scope.heap())))
}

/// `Object.hasOwn(O, P)` (ECMA-262).
pub fn object_has_own(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.hasown
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let prop_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop_val)?;
  root_property_key(scope, key)?;

  let has = scope
    .object_get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)?
    .is_some();
  Ok(Value::Bool(has))
}

/// `Object.getOwnPropertyDescriptor(O, P)` (ECMA-262).
pub fn object_get_own_property_descriptor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.getownpropertydescriptor
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let prop_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop_val)?;
  root_property_key(scope, key)?;

  let is_proxy = scope.heap().get_proxy_data(obj)?.is_some();
  let Some(mut desc) = scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)? else {
    return Ok(Value::Undefined);
  };

  // Ensure string exotic index properties report their actual value (which is not stored in the
  // property table).
  if !is_proxy {
    if let PropertyKind::Data { writable, .. } = desc.kind {
      let value = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
      desc.kind = PropertyKind::Data { value, writable };
    }
  }

  let out = from_property_descriptor(vm, scope, desc)?;
  Ok(Value::Object(out))
}

pub fn object_get_own_property_descriptors(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.getownpropertydescriptors
  let mut scope = scope.reborrow();

  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let mut tick = Vm::tick;
  let keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;
  let is_proxy = scope.heap().is_proxy_object(obj);

  let intr = require_intrinsics(vm)?;
  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;

  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    // `GetOwnPropertyDescriptor` can return `undefined` even for keys returned by a Proxy `ownKeys`
    // trap; in that case, `Object.getOwnPropertyDescriptors` stores `undefined` for that key.
    let desc = scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, &mut tick)?;

    let mut item_scope = scope.reborrow();
    item_scope.push_root(Value::Object(out))?;
    root_property_key(&mut item_scope, key)?;

    let value = if let Some(mut desc) = desc {
      // Ensure string/typed-array exotic index properties report their actual value (which may not
      // be stored in the property table).
      if !is_proxy {
        if let PropertyKind::Data { writable, .. } = desc.kind {
          let v = item_scope.ordinary_get_with_host_and_hooks(
            vm,
            host,
            hooks,
            obj,
            key,
            Value::Object(obj),
          )?;
          desc.kind = PropertyKind::Data { value: v, writable };
        }
      }

      let desc_obj = from_property_descriptor(vm, &mut item_scope, desc)?;
      Value::Object(desc_obj)
    } else {
      Value::Undefined
    };

    item_scope.create_data_property_or_throw(out, key, value)?;
  }

  Ok(Value::Object(out))
}
pub fn object_get_own_property_names(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.getownpropertynames
  let mut scope = scope.reborrow();

  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let mut tick = Vm::tick;
  let own_keys =
    scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &own_keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

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
    names.push(key_str);
  }

  let len = u32::try_from(names.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, len)?;
  scope.push_root(Value::Object(array))?;

  for (i, name) in names.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let mut idx_scope = scope.reborrow();
    idx_scope.push_roots(&[Value::Object(array), Value::String(name)])?;

    let key = PropertyKey::from_string(idx_scope.alloc_string(&i.to_string())?);
    idx_scope.define_property(array, key, data_desc(Value::String(name), true, true, true))?;
  }

  Ok(Value::Object(array))
}

pub fn object_get_own_property_symbols(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.getownpropertysymbols
  let mut scope = scope.reborrow();

  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let mut tick = Vm::tick;
  let own_keys =
    scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &own_keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

  let mut symbols: Vec<crate::GcSymbol> = Vec::new();
  symbols
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::Symbol(s) = key else {
      continue;
    };
    symbols.push(s);
  }

  let len = u32::try_from(symbols.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, len)?;
  scope.push_root(Value::Object(array))?;

  for (i, sym) in symbols.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let mut idx_scope = scope.reborrow();
    idx_scope.push_roots(&[Value::Object(array), Value::Symbol(sym)])?;

    let key = PropertyKey::from_string(idx_scope.alloc_string(&i.to_string())?);
    idx_scope.define_property(array, key, data_desc(Value::Symbol(sym), true, true, true))?;
  }

  Ok(Value::Object(array))
}

#[derive(Debug, Clone, Copy)]
enum IntegrityLevel {
  Sealed,
  Frozen,
}

fn set_integrity_level(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  level: IntegrityLevel,
) -> Result<bool, VmError> {
  // https://tc39.es/ecma262/#sec-setintegritylevel
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let ok = scope.prevent_extensions_with_host_and_hooks(vm, host, hooks, obj)?;
  if !ok {
    return Ok(false);
  }

  let mut tick = Vm::tick;
  let keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    if matches!(level, IntegrityLevel::Frozen) {
      let Some(desc) =
        scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, &mut tick)?
      else {
        continue;
      };
      let mut patch = PropertyDescriptorPatch {
        configurable: Some(false),
        ..Default::default()
      };
      if desc.is_data_descriptor() {
        patch.writable = Some(false);
      }
      let mut define_scope = scope.reborrow();
      define_scope.define_property_or_throw_with_host_and_hooks(vm, host, hooks, obj, key, patch)?;
    } else {
      let mut define_scope = scope.reborrow();
      define_scope.define_property_or_throw_with_host_and_hooks(
        vm,
        host,
        hooks,
        obj,
        key,
        PropertyDescriptorPatch {
          configurable: Some(false),
          ..Default::default()
        },
      )?;
    }
  }

  Ok(true)
}

fn test_integrity_level(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  level: IntegrityLevel,
) -> Result<bool, VmError> {
  // https://tc39.es/ecma262/#sec-testintegritylevel
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  if scope.is_extensible_with_host_and_hooks(vm, host, hooks, obj)? {
    return Ok(false);
  }

  let mut tick = Vm::tick;
  let keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let Some(desc) = scope.get_own_property_with_host_and_hooks_with_tick(
      vm,
      host,
      hooks,
      obj,
      key,
      &mut tick,
    )?
    else {
      continue;
    };
    if desc.configurable {
      return Ok(false);
    }
    if matches!(level, IntegrityLevel::Frozen) && desc.is_data_descriptor() {
      if let PropertyKind::Data { writable, .. } = desc.kind {
        if writable {
          return Ok(false);
        }
      }
    }
  }

  Ok(true)
}

pub fn object_prevent_extensions(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.preventextensions
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = obj_val else {
    return Ok(obj_val);
  };
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let ok = scope.prevent_extensions_with_host_and_hooks(vm, host, hooks, obj)?;
  if !ok {
    return Err(VmError::TypeError("Object.preventExtensions failed"));
  }
  Ok(Value::Object(obj))
}

pub fn object_seal(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.seal
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = obj_val else {
    return Ok(obj_val);
  };
  scope.push_root(Value::Object(obj))?;
  if !set_integrity_level(vm, scope, host, hooks, obj, IntegrityLevel::Sealed)? {
    return Err(VmError::TypeError("Object.seal failed"));
  }
  Ok(Value::Object(obj))
}

pub fn object_is_sealed(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.issealed
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = obj_val else {
    return Ok(Value::Bool(true));
  };
  scope.push_root(Value::Object(obj))?;
  Ok(Value::Bool(test_integrity_level(
    vm,
    scope,
    host,
    hooks,
    obj,
    IntegrityLevel::Sealed,
  )?))
}

pub fn object_freeze(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.freeze
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = obj_val else {
    return Ok(obj_val);
  };
  scope.push_root(Value::Object(obj))?;
  if !set_integrity_level(vm, scope, host, hooks, obj, IntegrityLevel::Frozen)? {
    return Err(VmError::TypeError("Object.freeze failed"));
  }
  Ok(Value::Object(obj))
}

pub fn object_is_frozen(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // https://tc39.es/ecma262/#sec-object.isfrozen
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = obj_val else {
    return Ok(Value::Bool(true));
  };
  scope.push_root(Value::Object(obj))?;
  Ok(Value::Bool(test_integrity_level(
    vm,
    scope,
    host,
    hooks,
    obj,
    IntegrityLevel::Frozen,
  )?))
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

  let mut tick = Vm::tick;
  let own_keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &own_keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

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
    let Some(desc) =
      scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, &mut tick)?
    else {
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
    idx_scope.push_roots(&[Value::Object(array), Value::String(name)])?;

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
  let mut tick = Vm::tick;
  let own_keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &own_keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

  // Build the result array incrementally so `Get` side effects (getters) can affect the
  // enumerability/existence of subsequent properties (per `EnumerableOwnProperties`).
  let array = create_array_object(vm, &mut scope, 0)?;
  scope.push_root(Value::Object(array))?;

  let mut out_i: usize = 0;
  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let PropertyKey::String(key_str) = key else {
      continue;
    };
    let key = PropertyKey::from_string(key_str);

    let Some(desc) =
      scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, &mut tick)?
    else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    // Allocate the target index key before calling `Get` so any GC triggered by the `Get`/getter
    // sees the index key as rooted.
    let mut iter_scope = scope.reborrow();
    iter_scope.push_roots(&[Value::Object(obj), Value::Object(array), Value::String(key_str)])?;

    let idx_s = iter_scope.alloc_string(&out_i.to_string())?;
    iter_scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);

    let value = iter_scope.get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      key,
      Value::Object(obj),
    )?;

    iter_scope.define_property(array, idx_key, data_desc(value, true, true, true))?;
    out_i = out_i.checked_add(1).ok_or(VmError::OutOfMemory)?;
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
  let mut tick = Vm::tick;
  let own_keys = scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, obj, &mut tick)?;
  // Root keys eagerly: for Proxy objects, keys can be synthesized by the `ownKeys` trap and may not
  // be reachable from `obj` itself.
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &own_keys {
    key_roots.push(match *key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    });
  }
  scope.push_roots(&key_roots)?;

  // Build the result array incrementally so `Get` side effects (getters) can affect the
  // enumerability/existence of subsequent properties (per `EnumerableOwnProperties`).
  let array = create_array_object(vm, &mut scope, 0)?;
  scope.push_root(Value::Object(array))?;

  let mut out_i: usize = 0;
  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let PropertyKey::String(key_str) = key else {
      continue;
    };
    let key = PropertyKey::from_string(key_str);

    let Some(desc) =
      scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, &mut tick)?
    else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    let mut iter_scope = scope.reborrow();
    iter_scope.push_roots(&[Value::Object(obj), Value::Object(array), Value::String(key_str)])?;

    let pair = create_array_object(vm, &mut iter_scope, 2)?;
    iter_scope.push_root(Value::Object(pair))?;

    let zero_s = iter_scope.alloc_string("0")?;
    iter_scope.push_root(Value::String(zero_s))?;
    let one_s = iter_scope.alloc_string("1")?;
    iter_scope.push_root(Value::String(one_s))?;

    // Allocate the destination index key before calling `Get` so any GC triggered by the
    // `Get`/getter sees it as rooted.
    let idx_s = iter_scope.alloc_string(&out_i.to_string())?;
    iter_scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);

    let value = iter_scope.get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      key,
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

    out_i = out_i.checked_add(1).ok_or(VmError::OutOfMemory)?;
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

  loop {
    let next_value =
      match crate::iterator::iterator_step_value(vm, host, hooks, &mut scope, &mut iterator_record) {
        Ok(Some(v)) => v,
        Ok(None) => return Ok(Value::Object(out)),
        Err(err) => return Err(err),
      };

    let entry_result: Result<(), VmError> = (|| {
      // Use a nested scope so per-entry roots do not accumulate.
      let mut step_scope = scope.reborrow();
      step_scope.push_root(next_value)?;

      let Value::Object(entry_obj) = next_value else {
        return Err(VmError::TypeError("Object.fromEntries: iterator value is not an object"));
      };

      let zero_key = string_key(&mut step_scope, "0")?;
      let key_val = step_scope.get_with_host_and_hooks(
        vm,
        host,
        hooks,
        entry_obj,
        zero_key,
        next_value,
      )?;
      step_scope.push_root(key_val)?;

      let one_key = string_key(&mut step_scope, "1")?;
      let value = step_scope.get_with_host_and_hooks(
        vm,
        host,
        hooks,
        entry_obj,
        one_key,
        next_value,
      )?;
      step_scope.push_root(value)?;

      let prop_key = step_scope.to_property_key(vm, host, hooks, key_val)?;
      root_property_key(&mut step_scope, prop_key)?;

      step_scope.create_data_property_or_throw(out, prop_key, value)?;
      Ok(())
    })();

    if let Err(entry_err) = entry_result {
      if !iterator_record.done {
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = entry_err.is_throw_completion();
        let pending_root = entry_err
          .thrown_value()
          .map(|v| scope.heap_mut().add_root(v))
          .transpose()?;
        let close_res = crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          &mut scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        );
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
        if let Err(close_err) = close_res {
          // Only propagate close errors for non-catchable failures; otherwise preserve the original
          // throw completion.
          if original_is_throw && !close_err.is_throw_completion() {
            return Err(close_err);
          }
        }
      }
      return Err(entry_err);
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
  // semantics (invoking accessors and Proxy traps).
  let mut scope = scope.reborrow();
  let mut tick = Vm::tick;
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
    // Use a nested scope so per-source key roots do not accumulate.
    let mut source_scope = scope.reborrow();
    source_scope.push_root(Value::Object(source))?;

    let keys =
      source_scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, source, &mut tick)?;
    // Root all keys in one batch: keys returned from a Proxy `ownKeys` trap may not be reachable from
    // any object once the trap result array becomes unreachable.
    let mut key_roots: Vec<Value> = Vec::new();
    key_roots
      .try_reserve_exact(keys.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for key in &keys {
      key_roots.push(match *key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      });
    }
    source_scope.push_roots(&key_roots)?;

    for (j, key) in keys.into_iter().enumerate() {
      if j % 1024 == 0 {
        vm.tick()?;
      }
      let Some(desc) = source_scope.get_own_property_with_host_and_hooks_with_tick(
        vm,
        host,
        hooks,
        source,
        key,
        &mut tick,
      )?
      else {
        continue;
      };
      if !desc.enumerable {
        continue;
      }

      // Spec: `Get(from, key)` (invokes getters / Proxy traps).
      let value = source_scope.get_with_host_and_hooks(
        vm,
        host,
        hooks,
        source,
        key,
        Value::Object(source),
      )?;
      // Spec: `Set(to, key, value, true)` (invokes setters / Proxy traps, throws on failure).
      let ok = source_scope.set_with_host_and_hooks(
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
  let mut scope = scope.reborrow();
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;
  match scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)? {
    Some(proto) => Ok(Value::Object(proto)),
    None => Ok(Value::Null),
  }
}

pub fn object_set_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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
      // Root `obj`/`proto` across Proxy trap invocations.
      let mut scope = scope.reborrow();
      let roots = [Value::Object(obj), proto_val];
      scope.push_roots(&roots)?;

      let ok = scope.set_prototype_of_with_host_and_hooks(vm, host, hooks, obj, proto)?;
      if ok {
        Ok(Value::Object(obj))
      } else {
        Err(VmError::TypeError("Object.setPrototypeOf failed"))
      }
    }
    // If `O` is not an object, the spec returns it unchanged.
    other => Ok(other),
  }
}

pub fn object_is_extensible(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.isextensible
  //
  // `Object.isExtensible` returns `false` for non-objects (it does not perform `ToObject`).
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = arg0 else {
    return Ok(Value::Bool(false));
  };
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  Ok(Value::Bool(
    scope.is_extensible_with_host_and_hooks(vm, host, hooks, obj)?,
  ))
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

  // Spec: `ToPropertyDescriptor` reads properties via `HasProperty`/`Get`, so use the shared helper.
  let patch = crate::to_property_descriptor_with_host_and_hooks(vm, &mut scope, host, hooks, desc_obj)?;
  // Root any descriptor values for the duration of `DefineOwnProperty` in case they were computed
  // by accessors and are not otherwise reachable.
  let mut roots = [Value::Undefined; 3];
  let mut root_count = 0usize;
  if let Some(v) = patch.value {
    roots[root_count] = v;
    root_count += 1;
  }
  if let Some(v) = patch.get {
    roots[root_count] = v;
    root_count += 1;
  }
  if let Some(v) = patch.set {
    roots[root_count] = v;
    root_count += 1;
  }
  scope.push_roots(&roots[..root_count])?;

  let ok = scope.define_own_property_with_host_and_hooks(vm, host, hooks, target, key, patch)?;
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

  let ok =
    crate::spec_ops::internal_delete_with_host_and_hooks(vm, &mut scope, host, hooks, target, key)?;
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
  scope.get_with_host_and_hooks(vm, host, hooks, target, key, receiver)
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
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let Some(desc) =
    scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, key)?
  else {
    return Ok(Value::Undefined);
  };

  let desc_obj = crate::from_property_descriptor(&mut scope, desc)?;
  Ok(Value::Object(desc_obj))
}

pub fn reflect_get_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.getprototypeof
  let mut scope = scope.reborrow();
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  match scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, target)? {
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

  let ok =
    crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut scope, host, hooks, target, key)?;
  Ok(Value::Bool(ok))
}

pub fn reflect_is_extensible(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.isextensible
  let mut scope = scope.reborrow();
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;
  Ok(Value::Bool(
    scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?,
  ))
}

pub fn reflect_own_keys(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.ownkeys
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

  let keys = scope.own_property_keys_with_host_and_hooks(vm, host, hooks, target)?;

  let len = u32::try_from(keys.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, len)?;
  scope.push_root(Value::Object(array))?;

  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };

    // Root `array` and the element value during key string allocation and property creation.
    //
    // This matters in particular for Proxy `ownKeys` trap results, where keys can be freshly
    // allocated and not reachable from any other heap object until inserted into the output array.
    let mut idx_scope = scope.reborrow();
    idx_scope.push_roots(&[Value::Object(array), value])?;

    let idx_s = idx_scope.alloc_string(&i.to_string())?;
    idx_scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);

    idx_scope.create_data_property_or_throw(array, idx_key, value)?;
  }

  Ok(Value::Object(array))
}

pub fn reflect_prevent_extensions(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.preventextensions
  let mut scope = scope.reborrow();
  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;
  let ok = scope.prevent_extensions_with_host_and_hooks(vm, host, hooks, target)?;
  Ok(Value::Bool(ok))
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

  let ok =
    crate::spec_ops::internal_set_with_host_and_hooks(vm, &mut scope, host, hooks, target, key, value, receiver)?;
  Ok(Value::Bool(ok))
}

pub fn reflect_set_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-reflect.setprototypeof
  let mut scope = scope.reborrow();

  let target_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let target = require_object(target_val)?;
  scope.push_root(Value::Object(target))?;

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
  let ok = scope.set_prototype_of_with_host_and_hooks(vm, host, hooks, target, proto)?;
  Ok(Value::Bool(ok))
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
      // https://tc39.es/ecma262/#sec-array-constructor
      //
      // `Array(len)` / `new Array(len)` where `len` is a Number validates via:
      //   If `ToUint32(len) != len`, throw a RangeError.
      //
      // This accepts +0/-0 and integer lengths in the inclusive range [0, 2^32-1].
      if !n.is_finite() || n.fract() != 0.0 || *n < 0.0 || *n > (u32::MAX as f64) {
        let intr = require_intrinsics(vm)?;
        let err = crate::error_object::new_range_error(scope, intr, "Invalid array length")?;
        return Err(VmError::Throw(err));
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
        idx_scope.push_roots(&[Value::Object(array), el])?;

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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let is_array = crate::spec_ops::is_array_with_host_and_hooks(vm, scope, host, hooks, arg0)?;
  Ok(Value::Bool(is_array))
}

/// `Array.from(items, mapFn?, thisArg?)` (partial).
///
/// This is implemented primarily to support iterator consumption with correct Proxy semantics:
/// - `GetMethod(items, @@iterator)` to select iterable vs array-like
/// - `Get` for `"length"` and element keys using internal-method dispatch
///
/// Spec: <https://tc39.es/ecma262/#sec-array.from>
pub fn array_constructor_from(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();

  let items = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(items)?;

  // Optional mapping function.
  let mapfn = args.get(1).copied().unwrap_or(Value::Undefined);
  let mapping = !matches!(mapfn, Value::Undefined);
  if mapping && !scope.heap().is_callable(mapfn)? {
    return Err(VmError::TypeError("Array.from mapfn is not callable"));
  }
  if mapping {
    scope.push_root(mapfn)?;
  }
  let this_arg = args.get(2).copied().unwrap_or(Value::Undefined);
  if mapping {
    scope.push_root(this_arg)?;
  }

  // `usingIterator = ? GetMethod(items, @@iterator)`.
  let intr = require_intrinsics(vm)?;
  let iterator_sym = intr.well_known_symbols().iterator;
  let using_iterator = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    items,
    PropertyKey::from_symbol(iterator_sym),
  )?;

  // If `items` is iterable, follow the iterator protocol.
  if let Some(method) = using_iterator {
    scope.push_root(method)?;

    let out = create_array_object(vm, &mut scope, 0)?;
    scope.push_root(Value::Object(out))?;

    let mut iterator_record =
      crate::iterator::get_iterator_from_method(vm, host, hooks, &mut scope, items, method)?;
    scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

    let result: Result<Value, VmError> = (|| {
      let mut k: usize = 0;
      loop {
        if k % 1024 == 0 {
          vm.tick()?;
        }

        let next_value =
          crate::iterator::iterator_step_value(vm, host, hooks, &mut scope, &mut iterator_record)?;
        let Some(next_value) = next_value else {
          return Ok(Value::Object(out));
        };

        // Use a nested scope so per-iteration roots do not accumulate.
        let mut step_scope = scope.reborrow();
        step_scope.push_root(next_value)?;

        let mut mapped = next_value;
        if mapping {
          let args = [next_value, Value::Number(k as f64)];
          mapped = vm.call_with_host_and_hooks(host, &mut step_scope, hooks, mapfn, this_arg, &args)?;
        }
        step_scope.push_root(mapped)?;

        let idx_s = alloc_string_from_usize(&mut step_scope, k)?;
        step_scope.push_root(Value::String(idx_s))?;
        let idx_key = PropertyKey::from_string(idx_s);
        step_scope.create_data_property_or_throw(out, idx_key, mapped)?;

        k = k.checked_add(1).ok_or(VmError::OutOfMemory)?;
      }
    })();

    match result {
      Ok(v) => Ok(v),
      Err(err) => {
        // IteratorClose on abrupt completion, matching other iterator-consuming builtins.
        if !iterator_record.done {
          // Per ECMA-262 `IteratorClose`:
          // - If the original completion is a throw completion, any *throw completion* produced
          //   while getting/calling `iterator.return` is suppressed (original error preserved).
          // - vm-js also has non-catchable VM failures (termination, OOM, etc) which must never be
          //   replaced by a catchable iterator-closing error.
          let original_is_throw = err.is_throw_completion();
          let completion_kind = if original_is_throw {
            crate::iterator::CloseCompletionKind::Throw
          } else {
            crate::iterator::CloseCompletionKind::NonThrow
          };
          let pending_root = err
            .thrown_value()
            .map(|v| scope.heap_mut().add_root(v))
            .transpose()?;
          let close_res = crate::iterator::iterator_close(
            vm,
            host,
            hooks,
            &mut scope,
            &iterator_record,
            completion_kind,
          );
          if let Some(root) = pending_root {
            scope.heap_mut().remove_root(root);
          }
          if let Err(close_err) = close_res {
            if original_is_throw {
              return Err(close_err);
            }
          }
        }
        Err(err)
      }
    }
  } else {
    // Array-like path: `obj = ToObject(items)` then `len = ToLength(Get(obj, "length"))`.
    let obj = scope.to_object(vm, host, hooks, items)?;
    scope.push_root(Value::Object(obj))?;

    let out = create_array_object(vm, &mut scope, 0)?;
    scope.push_root(Value::Object(out))?;

    let length_key = string_key(&mut scope, "length")?;
    let len_value = scope.get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
    let len = scope.to_length(vm, host, hooks, len_value)?;

    for k in 0..len {
      if k % 1024 == 0 {
        vm.tick()?;
      }

      // Use a nested scope so per-iteration roots do not accumulate.
      let mut step_scope = scope.reborrow();

      let idx_s = alloc_string_from_usize(&mut step_scope, k)?;
      step_scope.push_root(Value::String(idx_s))?;
      let idx_key = PropertyKey::from_string(idx_s);

      let value = step_scope.get_with_host_and_hooks(vm, host, hooks, obj, idx_key, Value::Object(obj))?;
      step_scope.push_root(value)?;

      let mut mapped = value;
      if mapping {
        let args = [value, Value::Number(k as f64)];
        mapped = vm.call_with_host_and_hooks(host, &mut step_scope, hooks, mapfn, this_arg, &args)?;
      }
      step_scope.create_data_property_or_throw(out, idx_key, mapped)?;
    }

    Ok(Value::Object(out))
  }
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
fn proxy_constructor_impl(
  scope: &mut Scope<'_>,
  target: Value,
  handler: Value,
) -> Result<Value, VmError> {
  // `ProxyCreate(target, handler)` (ECMA-262).
  //
  // Spec: https://tc39.es/ecma262/#sec-proxycreate
  let target = require_object(target)?;
  let handler = require_object(handler)?;
  // Root inputs across allocation/GC while creating the proxy object.
  let mut proxy_scope = scope.reborrow();
  proxy_scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;
  let proxy = proxy_scope.alloc_proxy(Some(target), Some(handler))?;
  Ok(Value::Object(proxy))
}
/// `Proxy` constructor (ECMA-262).
///
/// Proxy must be called with `new`; calling it as a normal function throws a TypeError.
pub fn proxy_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Per ECMA-262, `Proxy` is not callable without `new`.
  Err(VmError::TypeError("Proxy constructor requires 'new'"))
}

/// `new Proxy(target, handler)` (ECMA-262).
pub fn proxy_constructor_construct(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let handler = args.get(1).copied().unwrap_or(Value::Undefined);
  proxy_constructor_impl(&mut scope, target, handler)
}

pub fn proxy_revocable(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-proxy.revocable
  let mut scope = scope.reborrow();
  let intr = require_intrinsics(vm)?;

  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let handler = args.get(1).copied().unwrap_or(Value::Undefined);

  let proxy = proxy_constructor_impl(&mut scope, target, handler)?;
  let Value::Object(proxy) = proxy else {
    return Err(VmError::InvariantViolation(
      "ProxyCreate should always return an object",
    ));
  };
  scope.push_root(Value::Object(proxy))?;

  // Create the revocation function capturing the proxy in a native slot.
  let revoke_name = scope.alloc_string("revoke")?;
  scope.push_root(Value::String(revoke_name))?;
  let revoke = scope.alloc_native_function_with_slots(
    intr.proxy_revoker_call(),
    None,
    revoke_name,
    0,
    &[Value::Object(proxy)],
  )?;
  scope.push_root(Value::Object(revoke))?;
  // Match `CreateBuiltinFunction` semantics: the revoker should be associated with the current
  // realm so any job/microtask bookkeeping stays consistent if the function is passed around.
  set_function_job_realm_to_current(vm, &mut scope, revoke)?;
  scope
    .heap_mut()
    .object_set_prototype(revoke, Some(intr.function_prototype()))?;

  // Build the `{ proxy, revoke }` result object.
  let result = scope.alloc_object()?;
  scope.push_root(Value::Object(result))?;
  scope
    .heap_mut()
    .object_set_prototype(result, Some(intr.object_prototype()))?;

  let proxy_key = string_key(&mut scope, "proxy")?;
  scope.define_property(
    result,
    proxy_key,
    data_desc(Value::Object(proxy), true, false, true),
  )?;
  let revoke_key = PropertyKey::from_string(revoke_name);
  scope.define_property(
    result,
    revoke_key,
    data_desc(Value::Object(revoke), true, false, true),
  )?;

  Ok(Value::Object(result))
}
/// Revocation function created by `Proxy.revocable`.
pub fn proxy_revoker(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-proxy-revocation-functions
  //
  // The revoke function captures the proxy in an internal slot `[[RevocableProxy]]`. After the
  // first call, the slot is set to `null` so:
  // - subsequent calls are no-ops, and
  // - the revoke function does not keep the proxy alive for GC.
  let slot0 = scope
    .heap()
    .get_function_native_slots(callee)?
    .first()
    .copied()
    .unwrap_or(Value::Undefined);

  let proxy = match slot0 {
    Value::Object(proxy) => proxy,
    Value::Null | Value::Undefined => return Ok(Value::Undefined),
    _ => {
      return Err(VmError::InvariantViolation(
        "Proxy.revocable revoke function missing proxy slot",
      ))
    }
  };

  // Clear the captured slot before revoking to match spec ordering.
  scope
    .heap_mut()
    .set_function_native_slot(callee, 0, Value::Null)?;
  scope.revoke_proxy(proxy)?;
  Ok(Value::Undefined)
}

// Alias kept for compatibility with older intrinsic initialization code that refers to
// `builtins::proxy_revoke`.
#[allow(dead_code)]
pub fn proxy_revoke(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  proxy_revoker(vm, scope, host, hooks, callee, this, args)
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

  // Spec: https://tc39.es/ecma262/#sec-arraybuffer-constructor
  //
  // `new ArrayBuffer(length)` uses `ToIndex(length)`, so it:
  // - treats `undefined` as 0
  // - truncates fractional lengths (`1.9` → `1`)
  // - treats `NaN` as 0
  // - rejects negative values and +∞ with RangeError
  // - rejects values > 2^53 - 1 with RangeError
  let length_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let byte_length: usize = if matches!(length_val, Value::Undefined) {
    0
  } else {
    let num = scope.to_number(vm, host, hooks, length_val)?;
    // `ToIntegerOrInfinity`.
    let integer = if num.is_nan() {
      0.0
    } else if num.is_infinite() {
      num
    } else {
      num.trunc()
    };

    if integer < 0.0 {
      let err = crate::error_object::new_range_error(scope, intr, "Invalid array buffer length")?;
      return Err(VmError::Throw(err));
    }

    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0; // 2^53 - 1
    let index = if integer <= 0.0 {
      0.0
    } else if integer > MAX_SAFE_INTEGER {
      MAX_SAFE_INTEGER
    } else {
      integer
    };

    // `ToIndex` requires the clamped `ToLength` result to be exactly equal to the integer.
    if index != integer {
      let err = crate::error_object::new_range_error(scope, intr, "Invalid array buffer length")?;
      return Err(VmError::Throw(err));
    }

    // `index` is an integral f64 in [0, 2^53 - 1], so casting to u64 is exact.
    let index_u64 = index as u64;
    match usize::try_from(index_u64) {
      Ok(n) => n,
      Err(_) => {
        // If the host `usize` can't represent `index`, treat it as an invalid (too large) length.
        let err =
          crate::error_object::new_range_error(scope, intr, "Invalid array buffer length")?;
        return Err(VmError::Throw(err));
      }
    }
  };

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
    Value::Object(obj) => scope.heap().is_array_buffer_view_object(obj),
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

pub fn array_buffer_prototype_detached_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("ArrayBuffer.detached called on non-object"));
  };
  let detached = scope
    .heap()
    .array_buffer_is_detached(obj)
    .map_err(|_| VmError::TypeError("ArrayBuffer.detached called on incompatible receiver"))?;
  Ok(Value::Bool(detached))
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
  let detached = scope
    .heap()
    .array_buffer_is_detached(obj)
    .map_err(|_| VmError::TypeError("ArrayBuffer.prototype.slice called on incompatible receiver"))?;
  if detached {
    return Err(VmError::TypeError(
      "ArrayBuffer.prototype.slice called on detached ArrayBuffer",
    ));
  }
  let len = scope
    .heap()
    .array_buffer_byte_length(obj)
    .map_err(|_| VmError::TypeError("ArrayBuffer.prototype.slice called on incompatible receiver"))?;

  let (start, end) = slice_range_from_args(vm, scope, host, hooks, len, args)?;

  let bytes = {
    let data = scope.heap().array_buffer_data(obj)?;
    let slice = &data[start..end];
    let mut out: Vec<u8> = Vec::new();
    out
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut out, slice, || vm.tick())?;
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
  // Brand check first (buffer must be an ArrayBuffer). Note: per ECMA-262
  // `InitializeTypedArrayFromArrayBuffer`, the `byteOffset`/`length` arguments are converted
  // before checking for a detached buffer.
  if !scope.heap().is_array_buffer_object(buffer) {
    return Err(VmError::TypeError("Uint8Array constructor expects an ArrayBuffer"));
  }

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
    None
  } else {
    let n = scope.to_number(vm, host, hooks, length_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError("Uint8Array length must be a non-negative integer"));
    }
    Some(n as usize)
  };

  if scope
    .heap()
    .is_detached_array_buffer(buffer)
    .map_err(|_| VmError::TypeError("Uint8Array constructor expects an ArrayBuffer"))?
  {
    return Err(VmError::TypeError(
      "Uint8Array constructor cannot use a detached ArrayBuffer",
    ));
  }
  let buf_len = scope
    .heap()
    .array_buffer_byte_length(buffer)
    .map_err(|_| VmError::TypeError("Uint8Array constructor expects an ArrayBuffer"))?;

  let length = match length {
    None => buf_len.saturating_sub(byte_offset),
    Some(length) => length,
  };

  let view = scope.alloc_uint8_array(buffer, byte_offset, length)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
  Ok(Value::Object(view))
}

fn typed_array_prototype_for_kind(intr: &crate::Intrinsics, kind: TypedArrayKind) -> GcObject {
  match kind {
    TypedArrayKind::Int8 => intr.int8_array_prototype(),
    TypedArrayKind::Uint8 => intr.uint8_array_prototype(),
    TypedArrayKind::Uint8Clamped => intr.uint8_clamped_array_prototype(),
    TypedArrayKind::Int16 => intr.int16_array_prototype(),
    TypedArrayKind::Uint16 => intr.uint16_array_prototype(),
    TypedArrayKind::Int32 => intr.int32_array_prototype(),
    TypedArrayKind::Uint32 => intr.uint32_array_prototype(),
    TypedArrayKind::Float32 => intr.float32_array_prototype(),
    TypedArrayKind::Float64 => intr.float64_array_prototype(),
  }
}

fn typed_array_constructor_construct_impl(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  intr: crate::Intrinsics,
  kind: TypedArrayKind,
  prototype: GcObject,
  args: &[Value],
) -> Result<Value, VmError> {
  let bytes_per_element = kind.bytes_per_element();
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);

  // `new TypedArray(length)`
  if !matches!(arg0, Value::Object(_)) {
    let length_num = if matches!(arg0, Value::Undefined) {
      0.0
    } else {
      scope.heap_mut().to_number(arg0)?
    };
    if !length_num.is_finite() || length_num < 0.0 || length_num.fract() != 0.0 {
      return Err(VmError::TypeError(
        "TypedArray length must be a non-negative integer",
      ));
    }
    let length = length_num as usize;
    let byte_length = length
      .checked_mul(bytes_per_element)
      .ok_or(VmError::OutOfMemory)?;

    let ab = scope.alloc_array_buffer(byte_length)?;
    scope.push_root(Value::Object(ab))?;
    scope
      .heap_mut()
      .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

    let view = scope.alloc_typed_array(kind, ab, 0, length)?;
    scope
      .heap_mut()
      .object_set_prototype(view, Some(prototype))?;
    return Ok(Value::Object(view));
  }

  // `new TypedArray(buffer, byteOffset?, length?)`
  let Value::Object(buffer) = arg0 else {
    return Err(VmError::TypeError("TypedArray constructor expects an ArrayBuffer"));
  };
  let buf_len = scope
    .heap()
    .array_buffer_byte_length(buffer)
    .map_err(|_| VmError::TypeError("TypedArray constructor expects an ArrayBuffer"))?;

  let byte_offset_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let byte_offset = if matches!(byte_offset_val, Value::Undefined) {
    0usize
  } else {
    let n = scope.heap_mut().to_number(byte_offset_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError(
        "TypedArray byteOffset must be a non-negative integer",
      ));
    }
    n as usize
  };
  if byte_offset % bytes_per_element != 0 {
    return Err(VmError::TypeError("TypedArray byteOffset must be aligned"));
  }
  if byte_offset > buf_len {
    return Err(VmError::TypeError("TypedArray view out of bounds"));
  }

  let length_val = args.get(2).copied().unwrap_or(Value::Undefined);
  let length = if matches!(length_val, Value::Undefined) {
    let remaining = buf_len - byte_offset;
    if remaining % bytes_per_element != 0 {
      return Err(VmError::TypeError("TypedArray view out of bounds"));
    }
    remaining / bytes_per_element
  } else {
    let n = scope.heap_mut().to_number(length_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError(
        "TypedArray length must be a non-negative integer",
      ));
    }
    n as usize
  };

  let byte_length = length
    .checked_mul(bytes_per_element)
    .ok_or(VmError::OutOfMemory)?;
  let end = byte_offset
    .checked_add(byte_length)
    .ok_or(VmError::OutOfMemory)?;
  if end > buf_len {
    return Err(VmError::TypeError("TypedArray view out of bounds"));
  }

  let view = scope.alloc_typed_array(kind, buffer, byte_offset, length)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(prototype))?;
  Ok(Value::Object(view))
}

macro_rules! typed_array_ctor {
  ($call:ident, $construct:ident, $kind:expr, $name:literal, $proto:ident, $length:expr) => {
    pub fn $call(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Err(VmError::TypeError(concat!($name, " constructor requires 'new'")))
    }

    pub fn $construct(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      let intr = require_intrinsics(vm)?;
      typed_array_constructor_construct_impl(vm, scope, intr, $kind, intr.$proto(), args)
    }
  };
}

typed_array_ctor!(
  int8_array_constructor_call,
  int8_array_constructor_construct,
  TypedArrayKind::Int8,
  "Int8Array",
  int8_array_prototype,
  3
);
typed_array_ctor!(
  uint8_clamped_array_constructor_call,
  uint8_clamped_array_constructor_construct,
  TypedArrayKind::Uint8Clamped,
  "Uint8ClampedArray",
  uint8_clamped_array_prototype,
  3
);
typed_array_ctor!(
  int16_array_constructor_call,
  int16_array_constructor_construct,
  TypedArrayKind::Int16,
  "Int16Array",
  int16_array_prototype,
  3
);
typed_array_ctor!(
  uint16_array_constructor_call,
  uint16_array_constructor_construct,
  TypedArrayKind::Uint16,
  "Uint16Array",
  uint16_array_prototype,
  3
);
typed_array_ctor!(
  int32_array_constructor_call,
  int32_array_constructor_construct,
  TypedArrayKind::Int32,
  "Int32Array",
  int32_array_prototype,
  3
);
typed_array_ctor!(
  uint32_array_constructor_call,
  uint32_array_constructor_construct,
  TypedArrayKind::Uint32,
  "Uint32Array",
  uint32_array_prototype,
  3
);
typed_array_ctor!(
  float32_array_constructor_call,
  float32_array_constructor_construct,
  TypedArrayKind::Float32,
  "Float32Array",
  float32_array_prototype,
  3
);
typed_array_ctor!(
  float64_array_constructor_call,
  float64_array_constructor_construct,
  TypedArrayKind::Float64,
  "Float64Array",
  float64_array_prototype,
  3
);

pub fn typed_array_prototype_byte_length_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("TypedArray.byteLength called on non-object"));
  };
  let len = scope
    .heap()
    .typed_array_byte_length(obj)
    .map_err(|_| VmError::TypeError("TypedArray.byteLength called on incompatible receiver"))?;
  Ok(Value::Number(len as f64))
}

pub fn typed_array_prototype_length_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("TypedArray.length called on non-object"));
  };
  let len = scope
    .heap()
    .typed_array_length(obj)
    .map_err(|_| VmError::TypeError("TypedArray.length called on incompatible receiver"))?;
  Ok(Value::Number(len as f64))
}

pub fn typed_array_prototype_byte_offset_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("TypedArray.byteOffset called on non-object"));
  };
  let offset = scope
    .heap()
    .typed_array_byte_offset(obj)
    .map_err(|_| VmError::TypeError("TypedArray.byteOffset called on incompatible receiver"))?;
  Ok(Value::Number(offset as f64))
}

pub fn typed_array_prototype_buffer_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("TypedArray.buffer called on non-object"));
  };
  let buffer = scope
    .heap()
    .typed_array_buffer(obj)
    .map_err(|_| VmError::TypeError("TypedArray.buffer called on incompatible receiver"))?;
  Ok(Value::Object(buffer))
}

pub fn typed_array_prototype_subarray(
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
    return Err(VmError::TypeError(
      "TypedArray.prototype.subarray called on non-object",
    ));
  };
  scope.push_root(Value::Object(obj))?;

  let kind = scope
    .heap()
    .typed_array_kind(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.subarray called on incompatible receiver"))?;
  let len = scope
    .heap()
    .typed_array_length(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.subarray called on incompatible receiver"))?;

  let buffer = scope
    .heap()
    .typed_array_buffer(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.subarray called on incompatible receiver"))?;

  // Spec: `%TypedArray%.prototype.subarray` performs `ToIntegerOrInfinity` conversions on `start`
  // and `end` even if the typed array is out-of-bounds/detached.
  //
  // Any eventual detached-buffer TypeError comes later (during typed array creation), *after*
  // argument conversions (which can have user-observable side effects).
  let (start, end) = slice_range_from_args(vm, scope, host, hooks, len, args)?;
  let bytes_per_element = kind.bytes_per_element();

  let byte_offset = scope
    .heap()
    .typed_array_byte_offset(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.subarray called on incompatible receiver"))?;

  let rel_offset = start
    .checked_mul(bytes_per_element)
    .ok_or(VmError::OutOfMemory)?;
  let new_byte_offset = byte_offset
    .checked_add(rel_offset)
    .ok_or(VmError::OutOfMemory)?;
  let new_len = end - start;

  // Root buffer across allocation.
  scope.push_root(Value::Object(buffer))?;
  let view = scope.alloc_typed_array(kind, buffer, new_byte_offset, new_len)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(typed_array_prototype_for_kind(&intr, kind)))?;
  Ok(Value::Object(view))
}

pub fn typed_array_prototype_slice(
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
    return Err(VmError::TypeError(
      "TypedArray.prototype.slice called on non-object",
    ));
  };
  scope.push_root(Value::Object(obj))?;

  let kind = scope
    .heap()
    .typed_array_kind(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.slice called on incompatible receiver"))?;
  let len = scope
    .heap()
    .typed_array_length(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.slice called on incompatible receiver"))?;

  // Spec: `%TypedArray%.prototype.slice` validates the typed array (including detached buffer
  // checks) before coercing start/end arguments.
  //
  // Ensure detached buffers throw *before* `slice_range_from_args`, which can execute user code via
  // `valueOf`/`toString`.
  let buffer = scope
    .heap()
    .typed_array_buffer(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.slice called on incompatible receiver"))?;
  if scope.heap().array_buffer_data(buffer).is_err() {
    return Err(VmError::TypeError("ArrayBuffer is detached"));
  }

  let (start, end) = slice_range_from_args(vm, scope, host, hooks, len, args)?;
  let bytes_per_element = kind.bytes_per_element();

  let byte_offset = scope
    .heap()
    .typed_array_byte_offset(obj)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.slice called on incompatible receiver"))?;

  let start_byte = byte_offset
    .checked_add(start.checked_mul(bytes_per_element).ok_or(VmError::OutOfMemory)?)
    .ok_or(VmError::OutOfMemory)?;
  let end_byte = byte_offset
    .checked_add(end.checked_mul(bytes_per_element).ok_or(VmError::OutOfMemory)?)
    .ok_or(VmError::OutOfMemory)?;

  // Copy the visible byte range.
  scope.push_root(Value::Object(buffer))?;
  let bytes = {
    let data = scope.heap().array_buffer_data(buffer)?;
    let slice = data.get(start_byte..end_byte).ok_or(VmError::TypeError(
      "TypedArray.prototype.slice out of bounds",
    ))?;
    let mut out: Vec<u8> = Vec::new();
    out
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut out, slice, || vm.tick())?;
    out
  };

  let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
  scope.push_root(Value::Object(ab))?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

  let new_view = scope.alloc_typed_array(kind, ab, 0, end - start)?;
  scope
    .heap_mut()
    .object_set_prototype(new_view, Some(typed_array_prototype_for_kind(&intr, kind)))?;
  Ok(Value::Object(new_view))
}

pub fn typed_array_prototype_set(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(target) = this else {
    return Err(VmError::TypeError("TypedArray.prototype.set called on non-object"));
  };

  // RequireInternalSlot(target, [[TypedArrayName]]).
  let _ = scope
    .heap()
    .typed_array_kind(target)
    .map_err(|_| VmError::TypeError("TypedArray.prototype.set called on incompatible receiver"))?;

  let source_val = args.get(0).copied().unwrap_or(Value::Undefined);

  // Per ECMA-262, the `offset` argument is coerced via `ToIntegerOrInfinity` *before* detached/
  // out-of-bounds typed array checks are performed.
  let offset_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let offset = scope.to_integer_or_infinity(vm, host, hooks, offset_val)?;
  if offset < 0.0 {
    return Err(VmError::RangeError(
      "TypedArray.prototype.set offset must be a non-negative integer",
    ));
  }

  // After offset coercion, `%TypedArray%.prototype.set` must throw a TypeError if the target typed
  // array is out-of-bounds (including when its backing ArrayBuffer is detached).
  if scope.heap().typed_array_is_out_of_bounds(target)? {
    return Err(VmError::TypeError("ArrayBuffer is detached"));
  }

  // TypedArray source fast path.
  if let Value::Object(source) = source_val {
    if scope.heap().is_typed_array_object(source) {
      // After offset coercion, typed array sources must also throw when detached/out-of-bounds.
      if scope.heap().typed_array_is_out_of_bounds(source)? {
        return Err(VmError::TypeError("ArrayBuffer is detached"));
      }

      // Bounds checks happen after the detached/out-of-bounds checks (spec ordering).
      let target_len = scope.heap().typed_array_length(target)?;
      let source_len = scope.heap().typed_array_length(source)?;
      if offset.is_infinite() || offset > target_len as f64 {
        return Err(VmError::RangeError("TypedArray.prototype.set out of bounds"));
      }
      let offset = offset as usize;
      if source_len > target_len - offset {
        return Err(VmError::RangeError("TypedArray.prototype.set out of bounds"));
      }

      // Root source/target while copying.
      scope.push_roots(&[Value::Object(target), Value::Object(source)])?;

      // Copy values into a temporary Vec so overlapping ranges behave correctly.
      let mut tmp: Vec<Value> = Vec::new();
      tmp
        .try_reserve_exact(source_len)
        .map_err(|_| VmError::OutOfMemory)?;
      const TICK_EVERY: usize = 1024;
      for i in 0..source_len {
        if i % TICK_EVERY == 0 {
          vm.tick()?;
        }
        let v = scope
          .heap()
          .typed_array_get_element_value(source, i)?
          .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
        tmp.push(v);
      }
      for (i, v) in tmp.into_iter().enumerate() {
        if i % TICK_EVERY == 0 {
          vm.tick()?;
        }
        let ok = scope
          .heap_mut()
          .typed_array_set_element_value(target, offset + i, v)?;
        if !ok {
          return Err(VmError::TypeError("ArrayBuffer is detached"));
        }
      }

      return Ok(Value::Undefined);
    }
  }

  // Array-like source path (spec `SetTypedArrayFromArrayLike`).
  //
  // Note: we do the target out-of-bounds check above (after offset coercion) before touching the
  // source to ensure detached typed arrays always throw TypeError rather than other errors.
  let source_obj = scope.to_object(vm, host, hooks, source_val)?;
  scope.push_root(Value::Object(source_obj))?;

  let source_len = length_of_array_like_usize(vm, scope, host, hooks, source_obj)?;

  // Bounds checks happen after the detached/out-of-bounds check (spec ordering).
  let target_len = scope.heap().typed_array_length(target)?;
  if offset.is_infinite() || offset > target_len as f64 {
    return Err(VmError::RangeError("TypedArray.prototype.set out of bounds"));
  }
  let offset = offset as usize;
  if source_len > target_len - offset {
    return Err(VmError::RangeError("TypedArray.prototype.set out of bounds"));
  }

  // Copy values from the array-like source. This follows the spec shape:
  // `Get` each index and perform numeric conversion before storing into the target typed array.
  const TICK_EVERY: usize = 1024;
  for i in 0..source_len {
    if i % TICK_EVERY == 0 {
      vm.tick()?;
    }
    // Root `source_obj`/`target` across key allocation and `Get`/`ToNumber` calls (which may
    // allocate and can invoke user JS).
    let mut idx_scope = scope.reborrow();
    idx_scope.push_roots(&[Value::Object(source_obj), Value::Object(target)])?;

    let key_s = idx_scope.alloc_string(&i.to_string())?;
    let key = PropertyKey::from_string(key_s);
    let value = vm.get_with_host_and_hooks(host, &mut idx_scope, hooks, source_obj, key)?;

    let n = idx_scope.to_number(vm, host, hooks, value)?;
    let ok = idx_scope
      .heap_mut()
      .typed_array_set_element_value(target, offset + i, Value::Number(n))?;
    if !ok {
      return Err(VmError::TypeError("ArrayBuffer is detached"));
    }
  }

  Ok(Value::Undefined)
}

pub fn data_view_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("DataView constructor requires 'new'"))
}

pub fn data_view_constructor_construct(
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
  let Value::Object(buffer) = arg0 else {
    return Err(VmError::TypeError("DataView constructor expects an ArrayBuffer"));
  };
  let buf_len = scope
    .heap()
    .array_buffer_byte_length(buffer)
    .map_err(|_| VmError::TypeError("DataView constructor expects an ArrayBuffer"))?;

  let byte_offset_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let byte_offset = if matches!(byte_offset_val, Value::Undefined) {
    0usize
  } else {
    let n = scope.heap_mut().to_number(byte_offset_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError("DataView byteOffset must be a non-negative integer"));
    }
    n as usize
  };
  // Per ECMAScript, DataView construction on a detached ArrayBuffer throws (even for a 0-length
  // view). The detachment check happens after ToIndex(byteOffset) conversion.
  if scope
    .heap()
    .is_detached_array_buffer(buffer)
    .map_err(|_| VmError::TypeError("DataView constructor expects an ArrayBuffer"))?
  {
    return Err(VmError::TypeError("DataView constructor on detached ArrayBuffer"));
  }
  if byte_offset > buf_len {
    return Err(VmError::TypeError("DataView view out of bounds"));
  }

  let byte_length_val = args.get(2).copied().unwrap_or(Value::Undefined);
  let byte_length = if matches!(byte_length_val, Value::Undefined) {
    buf_len - byte_offset
  } else {
    let n = scope.heap_mut().to_number(byte_length_val)?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::TypeError("DataView byteLength must be a non-negative integer"));
    }
    n as usize
  };
  let end = byte_offset
    .checked_add(byte_length)
    .ok_or(VmError::OutOfMemory)?;
  if end > buf_len {
    return Err(VmError::TypeError("DataView view out of bounds"));
  }

  let view = scope.alloc_data_view(buffer, byte_offset, byte_length)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(intr.data_view_prototype()))?;
  Ok(Value::Object(view))
}

pub fn data_view_prototype_byte_length_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("DataView.byteLength called on non-object"));
  };
  let heap = scope.heap();
  let buffer = heap
    .data_view_buffer(obj)
    .map_err(|_| VmError::TypeError("DataView.byteLength called on incompatible receiver"))?;
  let len = if heap
    .is_detached_array_buffer(buffer)
    .map_err(|_| VmError::TypeError("DataView.byteLength called on incompatible receiver"))?
  {
    0
  } else {
    heap
      .data_view_byte_length(obj)
      .map_err(|_| VmError::TypeError("DataView.byteLength called on incompatible receiver"))?
  };
  Ok(Value::Number(len as f64))
}

pub fn data_view_prototype_byte_offset_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("DataView.byteOffset called on non-object"));
  };
  let heap = scope.heap();
  let buffer = heap
    .data_view_buffer(obj)
    .map_err(|_| VmError::TypeError("DataView.byteOffset called on incompatible receiver"))?;
  let offset = if heap
    .is_detached_array_buffer(buffer)
    .map_err(|_| VmError::TypeError("DataView.byteOffset called on incompatible receiver"))?
  {
    0
  } else {
    heap
      .data_view_byte_offset(obj)
      .map_err(|_| VmError::TypeError("DataView.byteOffset called on incompatible receiver"))?
  };
  Ok(Value::Number(offset as f64))
}

pub fn data_view_prototype_buffer_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("DataView.buffer called on non-object"));
  };
  let buffer = scope
    .heap()
    .data_view_buffer(obj)
    .map_err(|_| VmError::TypeError("DataView.buffer called on incompatible receiver"))?;
  Ok(Value::Object(buffer))
}

#[derive(Clone, Copy)]
enum DataViewElementKind {
  Int8,
  Uint8,
  Int16,
  Uint16,
  Int32,
  Uint32,
  Float32,
  Float64,
}

impl DataViewElementKind {
  fn size(self) -> usize {
    match self {
      DataViewElementKind::Int8 | DataViewElementKind::Uint8 => 1,
      DataViewElementKind::Int16 | DataViewElementKind::Uint16 => 2,
      DataViewElementKind::Int32 | DataViewElementKind::Uint32 | DataViewElementKind::Float32 => 4,
      DataViewElementKind::Float64 => 8,
    }
  }
}

fn data_view_get_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  this: Value,
  args: &[Value],
  kind: DataViewElementKind,
) -> Result<Value, VmError> {
  let Value::Object(view_obj) = this else {
    return Err(VmError::TypeError("DataView method called on non-object"));
  };
  let byte_length = scope
    .heap()
    .data_view_byte_length(view_obj)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?;
  let view_byte_offset = scope
    .heap()
    .data_view_byte_offset(view_obj)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?;
  let buffer = scope
    .heap()
    .data_view_buffer(view_obj)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?;

  let offset_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let offset_num = scope.to_number(vm, host, hooks, offset_val)?;
  if !offset_num.is_finite() || offset_num < 0.0 || offset_num.fract() != 0.0 {
    return Err(VmError::TypeError("DataView offset must be a non-negative integer"));
  }
  let offset = offset_num as usize;

  let little_endian = match args.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Undefined => false,
    v => scope.heap().to_boolean(v)?,
  };

  // Spec: detached ArrayBuffer check happens after ToIndex(byteOffset).
  if scope
    .heap()
    .is_detached_array_buffer(buffer)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?
  {
    return Err(VmError::TypeError("ArrayBuffer is detached"));
  }

  let size = kind.size();
  let end = offset.checked_add(size).ok_or(VmError::OutOfMemory)?;
  if end > byte_length {
    return Err(VmError::TypeError("DataView offset out of bounds"));
  }

  scope.push_root(Value::Object(buffer))?;
  let data = scope.heap().array_buffer_data(buffer)?;
  let abs = view_byte_offset
    .checked_add(offset)
    .ok_or(VmError::OutOfMemory)?;

  let value = match kind {
    DataViewElementKind::Int8 => {
      let b = *data.get(abs).ok_or(VmError::TypeError("DataView offset out of bounds"))?;
      (b as i8) as f64
    }
    DataViewElementKind::Uint8 => {
      let b = *data.get(abs).ok_or(VmError::TypeError("DataView offset out of bounds"))?;
      b as f64
    }
    DataViewElementKind::Int16 => {
      let bytes: [u8; 2] = data
        .get(abs..abs + 2)
        .ok_or(VmError::TypeError("DataView offset out of bounds"))?
        .try_into()
        .map_err(|_| VmError::InvariantViolation("DataView slice length mismatch"))?;
      if little_endian {
        i16::from_le_bytes(bytes) as f64
      } else {
        i16::from_be_bytes(bytes) as f64
      }
    }
    DataViewElementKind::Uint16 => {
      let bytes: [u8; 2] = data
        .get(abs..abs + 2)
        .ok_or(VmError::TypeError("DataView offset out of bounds"))?
        .try_into()
        .map_err(|_| VmError::InvariantViolation("DataView slice length mismatch"))?;
      if little_endian {
        u16::from_le_bytes(bytes) as f64
      } else {
        u16::from_be_bytes(bytes) as f64
      }
    }
    DataViewElementKind::Int32 => {
      let bytes: [u8; 4] = data
        .get(abs..abs + 4)
        .ok_or(VmError::TypeError("DataView offset out of bounds"))?
        .try_into()
        .map_err(|_| VmError::InvariantViolation("DataView slice length mismatch"))?;
      if little_endian {
        i32::from_le_bytes(bytes) as f64
      } else {
        i32::from_be_bytes(bytes) as f64
      }
    }
    DataViewElementKind::Uint32 => {
      let bytes: [u8; 4] = data
        .get(abs..abs + 4)
        .ok_or(VmError::TypeError("DataView offset out of bounds"))?
        .try_into()
        .map_err(|_| VmError::InvariantViolation("DataView slice length mismatch"))?;
      if little_endian {
        u32::from_le_bytes(bytes) as f64
      } else {
        u32::from_be_bytes(bytes) as f64
      }
    }
    DataViewElementKind::Float32 => {
      let bytes: [u8; 4] = data
        .get(abs..abs + 4)
        .ok_or(VmError::TypeError("DataView offset out of bounds"))?
        .try_into()
        .map_err(|_| VmError::InvariantViolation("DataView slice length mismatch"))?;
      let bits = if little_endian {
        u32::from_le_bytes(bytes)
      } else {
        u32::from_be_bytes(bytes)
      };
      f32::from_bits(bits) as f64
    }
    DataViewElementKind::Float64 => {
      let bytes: [u8; 8] = data
        .get(abs..abs + 8)
        .ok_or(VmError::TypeError("DataView offset out of bounds"))?
        .try_into()
        .map_err(|_| VmError::InvariantViolation("DataView slice length mismatch"))?;
      let bits = if little_endian {
        u64::from_le_bytes(bytes)
      } else {
        u64::from_be_bytes(bytes)
      };
      f64::from_bits(bits)
    }
  };

  Ok(Value::Number(value))
}

fn data_view_set_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  this: Value,
  args: &[Value],
  kind: DataViewElementKind,
) -> Result<Value, VmError> {
  let Value::Object(view_obj) = this else {
    return Err(VmError::TypeError("DataView method called on non-object"));
  };
  let byte_length = scope
    .heap()
    .data_view_byte_length(view_obj)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?;
  let view_byte_offset = scope
    .heap()
    .data_view_byte_offset(view_obj)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?;
  let buffer = scope
    .heap()
    .data_view_buffer(view_obj)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?;

  let offset_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let offset_num = scope.to_number(vm, host, hooks, offset_val)?;
  if !offset_num.is_finite() || offset_num < 0.0 || offset_num.fract() != 0.0 {
    return Err(VmError::TypeError("DataView offset must be a non-negative integer"));
  }
  let offset = offset_num as usize;

  let value = args.get(1).copied().unwrap_or(Value::Undefined);
  let little_endian = match args.get(2).copied().unwrap_or(Value::Undefined) {
    Value::Undefined => false,
    v => scope.heap().to_boolean(v)?,
  };

  // Spec: detached ArrayBuffer check happens after ToIndex(byteOffset).
  if scope
    .heap()
    .is_detached_array_buffer(buffer)
    .map_err(|_| VmError::TypeError("DataView method called on incompatible receiver"))?
  {
    return Err(VmError::TypeError("ArrayBuffer is detached"));
  }

  let size = kind.size();
  let end = offset.checked_add(size).ok_or(VmError::OutOfMemory)?;
  if end > byte_length {
    return Err(VmError::TypeError("DataView offset out of bounds"));
  }

  // Convert via ToNumber (like typed arrays).
  let n = scope.to_number(vm, host, hooks, value)?;

  let bytes: Vec<u8> = match kind {
    DataViewElementKind::Int8 => {
      let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(256.0) as u8 as i8 };
      vec![v as u8]
    }
    DataViewElementKind::Uint8 => {
      let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(256.0) as u8 };
      vec![v]
    }
    DataViewElementKind::Int16 => {
      let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(65_536.0) as u16 as i16 };
      if little_endian {
        v.to_le_bytes().to_vec()
      } else {
        v.to_be_bytes().to_vec()
      }
    }
    DataViewElementKind::Uint16 => {
      let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(65_536.0) as u16 };
      if little_endian {
        v.to_le_bytes().to_vec()
      } else {
        v.to_be_bytes().to_vec()
      }
    }
    DataViewElementKind::Int32 => {
      let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(4_294_967_296.0) as u32 as i32 };
      if little_endian {
        v.to_le_bytes().to_vec()
      } else {
        v.to_be_bytes().to_vec()
      }
    }
    DataViewElementKind::Uint32 => {
      let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(4_294_967_296.0) as u32 };
      if little_endian {
        v.to_le_bytes().to_vec()
      } else {
        v.to_be_bytes().to_vec()
      }
    }
    DataViewElementKind::Float32 => {
      let v = (n as f32).to_bits();
      if little_endian {
        v.to_le_bytes().to_vec()
      } else {
        v.to_be_bytes().to_vec()
      }
    }
    DataViewElementKind::Float64 => {
      let v = n.to_bits();
      if little_endian {
        v.to_le_bytes().to_vec()
      } else {
        v.to_be_bytes().to_vec()
      }
    }
  };

  debug_assert_eq!(bytes.len(), size);
  let abs = view_byte_offset
    .checked_add(offset)
    .ok_or(VmError::OutOfMemory)?;
  scope.push_root(Value::Object(buffer))?;
  scope.heap_mut().array_buffer_write(buffer, abs, &bytes)?;
  Ok(Value::Undefined)
}

macro_rules! data_view_get {
  ($name:ident, $kind:expr) => {
    pub fn $name(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      data_view_get_impl(vm, scope, host, hooks, this, args, $kind)
    }
  };
}

macro_rules! data_view_set {
  ($name:ident, $kind:expr) => {
    pub fn $name(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      data_view_set_impl(vm, scope, host, hooks, this, args, $kind)
    }
  };
}

data_view_get!(data_view_prototype_get_int8, DataViewElementKind::Int8);
data_view_get!(data_view_prototype_get_uint8, DataViewElementKind::Uint8);
data_view_get!(data_view_prototype_get_int16, DataViewElementKind::Int16);
data_view_get!(data_view_prototype_get_uint16, DataViewElementKind::Uint16);
data_view_get!(data_view_prototype_get_int32, DataViewElementKind::Int32);
data_view_get!(data_view_prototype_get_uint32, DataViewElementKind::Uint32);
data_view_get!(data_view_prototype_get_float32, DataViewElementKind::Float32);
data_view_get!(data_view_prototype_get_float64, DataViewElementKind::Float64);

data_view_set!(data_view_prototype_set_int8, DataViewElementKind::Int8);
data_view_set!(data_view_prototype_set_uint8, DataViewElementKind::Uint8);
data_view_set!(data_view_prototype_set_int16, DataViewElementKind::Int16);
data_view_set!(data_view_prototype_set_uint16, DataViewElementKind::Uint16);
data_view_set!(data_view_prototype_set_int32, DataViewElementKind::Int32);
data_view_set!(data_view_prototype_set_uint32, DataViewElementKind::Uint32);
data_view_set!(data_view_prototype_set_float32, DataViewElementKind::Float32);
data_view_set!(data_view_prototype_set_float64, DataViewElementKind::Float64);

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
    let units = scope.heap().get_string(s)?.as_code_units();
    let text = utf16_to_utf8_lossy_with_tick(units, || vm.tick())?;
    if idx != 0 {
      params_joined
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      params_joined.push(',');
    }
    params_joined
      .try_reserve(text.len())
      .map_err(|_| VmError::OutOfMemory)?;
    params_joined.push_str(&text);
  }

  let body_s = scope.to_string(vm, host, hooks, body_value)?;
  let body_units = scope.heap().get_string(body_s)?.as_code_units();
  let body_text = utf16_to_utf8_lossy_with_tick(body_units, || vm.tick())?;

  // Parse as a single function declaration statement so we can reuse the normal ECMAScript
  // function-object call path.
  let mut source: String = String::new();
  const PREFIX: &str = "function anonymous(";
  const MIDDLE: &str = "\n) {\n";
  const SUFFIX: &str = "\n}";
  let total_len = PREFIX
    .len()
    .checked_add(params_joined.len())
    .and_then(|n| n.checked_add(MIDDLE.len()))
    .and_then(|n| n.checked_add(body_text.len()))
    .and_then(|n| n.checked_add(SUFFIX.len()))
    .ok_or(VmError::OutOfMemory)?;
  source
    .try_reserve(total_len)
    .map_err(|_| VmError::OutOfMemory)?;
  source.push_str(PREFIX);
  source.push_str(&params_joined);
  source.push_str(MIDDLE);
  source.push_str(&body_text);
  source.push_str(SUFFIX);

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
  {
    let mut tick = || vm.tick();
    match crate::early_errors::validate_top_level(
      &parsed.stx.body,
      crate::early_errors::EarlyErrorOptions::script(false),
      &mut tick,
    ) {
      Ok(()) => {}
      Err(VmError::Syntax(diags)) => {
        let message = diags
          .first()
          .map(|d| d.message.as_str())
          .unwrap_or("Invalid or unexpected token");
        let err_obj = crate::error_object::new_syntax_error_object(scope, &intr, message)?;
        return Err(VmError::Throw(err_obj));
      }
      Err(err) => return Err(err),
    }
  }

  // Derive strictness and length from the parsed function node.
  let mut is_strict = false;
  let mut length: u32 = 0;
  let mut body_stmts: Option<&[Node<Stmt>]> = None;
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
        body_stmts = Some(stmts);
      }
    }
  }

  if is_strict {
    // Strict-mode `with` is required to be rejected during dynamic function creation (spec
    // `CreateDynamicFunction`), not deferred until first execution.
    if let Some(stmts) = body_stmts {
      if strict_mode_stmts_contain_with(vm, stmts)? {
        let err_obj = crate::error_object::new_syntax_error_object(
          scope,
          &intr,
          "with statements are not allowed in strict mode",
        )?;
        return Err(VmError::Throw(err_obj));
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

/// `%GeneratorFunction%` (ECMA-262).
///
/// Calling `%GeneratorFunction%` as a function behaves like `new %GeneratorFunction%`.
///
/// Note: generator function *execution* semantics are implemented in a separate task. This
/// constructor implements dynamic creation semantics so code can observe `%GeneratorFunction%` and
/// create generator function objects.
pub fn generator_function_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  generator_function_constructor_construct(vm, scope, host, hooks, callee, args, Value::Object(callee))
}

/// `%GeneratorFunction%` `[[Construct]]` (ECMA-262).
///
/// This is `CreateDynamicFunction` for generator functions: it parses the provided parameter list
/// and body text and returns a fresh generator function object.
pub fn generator_function_constructor_construct(
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
    return Err(VmError::Unimplemented(
      "GeneratorFunction constructor missing [[Realm]]",
    ));
  };

  // Like `%Function%`, `%GeneratorFunction%` creates its function in the global lexical
  // environment, not in the caller's environment. In `vm-js`, this is stored as the intrinsic
  // Function constructor's captured closure env.
  let closure_env = match scope.heap().get_function_closure_env(callee)? {
    Some(env) => Some(env),
    None => scope.heap().get_function_closure_env(intr.function_constructor())?,
  };

  // `GeneratorFunction(...params, body)` uses the final argument as the body.
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
    let units = scope.heap().get_string(s)?.as_code_units();
    let text = utf16_to_utf8_lossy_with_tick(units, || vm.tick())?;
    if idx != 0 {
      params_joined
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      params_joined.push(',');
    }
    params_joined
      .try_reserve(text.len())
      .map_err(|_| VmError::OutOfMemory)?;
    params_joined.push_str(&text);
  }

  let body_s = scope.to_string(vm, host, hooks, body_value)?;
  let body_units = scope.heap().get_string(body_s)?.as_code_units();
  let body_text = utf16_to_utf8_lossy_with_tick(body_units, || vm.tick())?;

  // Parse as a single generator function declaration statement so we can reuse the normal
  // ECMAScript function-object call path.
  let mut source: String = String::new();
  const PREFIX: &str = "function* anonymous(";
  const MIDDLE: &str = "\n) {\n";
  const SUFFIX: &str = "\n}";
  let total_len = PREFIX
    .len()
    .checked_add(params_joined.len())
    .and_then(|n| n.checked_add(MIDDLE.len()))
    .and_then(|n| n.checked_add(body_text.len()))
    .and_then(|n| n.checked_add(SUFFIX.len()))
    .ok_or(VmError::OutOfMemory)?;
  source
    .try_reserve(total_len)
    .map_err(|_| VmError::OutOfMemory)?;
  source.push_str(PREFIX);
  source.push_str(&params_joined);
  source.push_str(MIDDLE);
  source.push_str(&body_text);
  source.push_str(SUFFIX);

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
  {
    let mut tick = || vm.tick();
    match crate::early_errors::validate_top_level(
      &parsed.stx.body,
      crate::early_errors::EarlyErrorOptions::script(false),
      &mut tick,
    ) {
      Ok(()) => {}
      Err(VmError::Syntax(diags)) => {
        let message = diags
          .first()
          .map(|d| d.message.as_str())
          .unwrap_or("Invalid or unexpected token");
        let err_obj = crate::error_object::new_syntax_error_object(scope, &intr, message)?;
        return Err(VmError::Throw(err_obj));
      }
      Err(err) => return Err(err),
    }
  }

  // Derive strictness and length from the parsed function node.
  let mut is_strict = false;
  let mut length: u32 = 0;
  let mut body_stmts: Option<&[Node<Stmt>]> = None;
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
        body_stmts = Some(stmts);
      }
    }
  }

  if is_strict {
    // Strict-mode `with` is required to be rejected during dynamic generator function creation
    // (spec `CreateDynamicFunction`), not deferred until first execution.
    if let Some(stmts) = body_stmts {
      if strict_mode_stmts_contain_with(vm, stmts)? {
        let err_obj = crate::error_object::new_syntax_error_object(
          scope,
          &intr,
          "with statements are not allowed in strict mode",
        )?;
        return Err(VmError::Throw(err_obj));
      }
    }
  }

  let this_mode = if is_strict { ThisMode::Strict } else { ThisMode::Global };

  let source = Arc::new(SourceText::new_charged(
    scope.heap_mut(),
    "<GeneratorFunction>",
    source,
  )?);
  let span_end = u32::try_from(source.text.len()).unwrap_or(u32::MAX);
  let code_id = vm.register_ecma_function(source, 0, span_end, crate::vm::EcmaFunctionKind::Decl)?;

  let name_s = scope.alloc_string("anonymous")?;
  let func_obj = scope.alloc_ecma_function(
    code_id,
    false,
    name_s,
    length,
    this_mode,
    is_strict,
    closure_env,
  )?;
  scope.push_root(Value::Object(func_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(func_obj, Some(intr.generator_function_prototype()))?;
  crate::function_properties::make_generator_function_instance_prototype(
    scope,
    func_obj,
    intr.generator_prototype(),
  )?;
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
  let intr = require_intrinsics(vm)?;

  // --- Determine which intrinsic default prototype to use ---
  //
  // Each NativeError constructor uses a different intrinsic fallback (e.g. `TypeError` uses
  // `%TypeError.prototype%`). This must not depend on the user-mutable `callee.prototype` data
  // property.
  let default_proto = if callee == intr.error() {
    intr.error_prototype()
  } else if callee == intr.type_error() {
    intr.type_error_prototype()
  } else if callee == intr.range_error() {
    intr.range_error_prototype()
  } else if callee == intr.reference_error() {
    intr.reference_error_prototype()
  } else if callee == intr.syntax_error() {
    intr.syntax_error_prototype()
  } else if callee == intr.eval_error() {
    intr.eval_error_prototype()
  } else if callee == intr.uri_error() {
    intr.uri_error_prototype()
  } else if callee == intr.aggregate_error() {
    intr.aggregate_error_prototype()
  } else {
    // Defensive fallback: this call/construct handler is only registered for error constructors.
    intr.error_prototype()
  };

  let is_aggregate_error = callee == intr.aggregate_error();

  // --- Create the instance (spec: OrdinaryCreateFromConstructor) ---
  let obj = crate::spec_ops::ordinary_create_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    default_proto,
    &[],
    |scope| scope.alloc_error(),
  )?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  // AggregateError: `errors` is created from the provided iterable.
  if is_aggregate_error {
    let errors_iterable = args.get(0).copied().unwrap_or(Value::Undefined);
    scope.push_root(errors_iterable)?;
    let errors_array = aggregate_error_iterable_to_list_array(vm, &mut scope, host, hooks, errors_iterable)?;
    scope.push_root(Value::Object(errors_array))?;

    let errors_key = string_key(&mut scope, "errors")?;
    scope.define_property(
      obj,
      errors_key,
      data_desc(Value::Object(errors_array), true, false, true),
    )?;
  }

  // Message argument: for AggregateError, the message is the *second* argument.
  // Spec: `new AggregateError(errors, message [, options])` (ECMA-262).
  let message_arg = if is_aggregate_error {
    args.get(1).copied()
  } else {
    args.get(0).copied()
  };
  if let Some(message_value) = message_arg {
    if !matches!(message_value, Value::Undefined) {
      let message_string = scope.to_string(vm, host, hooks, message_value)?;
      scope.push_root(Value::String(message_string))?;
      let message_key = string_key(&mut scope, "message")?;
      scope.define_property(
        obj,
        message_key,
        data_desc(Value::String(message_string), true, false, true),
      )?;
    }
  }

  // ES2022 Error cause option.
  // Spec: `InstallErrorCause(O, options)`.
  let options_arg = if is_aggregate_error {
    args.get(2).copied()
  } else {
    args.get(1).copied()
  };
  if let Some(options_value) = options_arg {
    if !matches!(options_value, Value::Undefined) {
      let Value::Object(options_obj) = options_value else {
        return Err(VmError::TypeError("Error options must be an object"));
      };
      scope.push_root(Value::Object(options_obj))?;

      let cause_key = string_key(&mut scope, "cause")?;
      // `Get(options, "cause")` must be Proxy-aware.
      let cause =
        scope.get_with_host_and_hooks(vm, host, hooks, options_obj, cause_key, Value::Object(options_obj))?;
      if !matches!(cause, Value::Undefined) {
        scope.push_root(cause)?;
        scope.define_property(obj, cause_key, data_desc(cause, true, false, true))?;
      }
    }
  }

  Ok(Value::Object(obj))
}

fn aggregate_error_iterable_to_list_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  iterable: Value,
) -> Result<GcObject, VmError> {
  // Spec: `IterableToList` followed by `CreateArrayFromList`.
  //
  // This is intentionally implemented by iterating and constructing the output array rather than
  // collecting into an intermediate `Vec<Value>`, so element values become GC-reachable
  // incrementally (important for large iterables).
  let mut iterator_record = crate::iterator::get_iterator(vm, host, hooks, scope, iterable)?;
  // Root iterator record values for the duration of iteration.
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  // Create the output array with length 0 and push elements as we iterate.
  let array = create_array_object(vm, scope, 0)?;
  scope.push_root(Value::Object(array))?;

  let mut idx: u32 = 0;
  let result: Result<(), VmError> = (|| {
    loop {
      if idx % 1024 == 0 {
        vm.tick()?;
      }

      let next_value = crate::iterator::iterator_step_value(vm, host, hooks, scope, &mut iterator_record)?;
      let Some(next_value) = next_value else {
        break;
      };

      // Root `array` and the per-element value while creating the property key.
      let mut idx_scope = scope.reborrow();
      idx_scope.push_root(Value::Object(array))?;
      idx_scope.push_root(next_value)?;

      let key_s = idx_scope.alloc_string(&idx.to_string())?;
      idx_scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      idx_scope.create_data_property_or_throw(array, key, next_value)?;
      idx = idx.wrapping_add(1);
    }
    Ok(())
  })();

  match result {
    Ok(()) => Ok(array),
    Err(err) => {
      if !iterator_record.done {
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = err.is_throw_completion();
        let pending_root = err
          .thrown_value()
          .map(|v| scope.heap_mut().add_root(v))
          .transpose()?;
        let close_res = crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        );
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
        if let Err(close_err) = close_res {
          // Only propagate close errors for non-catchable failures; otherwise preserve the original
          // throw completion.
          if original_is_throw && !close_err.is_throw_completion() {
            return Err(close_err);
          }
        }
      }
      Err(err)
    }
  }
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

fn create_syntax_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  message: &str,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  crate::error_object::new_syntax_error_object(scope, &intr, message)
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

/// %ThrowTypeError% intrinsic used by restricted function properties (ECMA-262).
///
/// This must always throw a `TypeError` and is shared between:
/// - `Function.prototype.caller`
/// - `Function.prototype.arguments`
///
/// Test262 also asserts that the same function object is used as both the getter and setter and
/// that it is shared across both properties.
pub fn throw_type_error_intrinsic(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let err_obj = crate::error_object::new_type_error_object(scope, &intr, "Restricted function property")?;
  Err(VmError::Throw(err_obj))
}

fn throw_syntax_error(vm: &mut Vm, scope: &mut Scope<'_>, message: &str) -> Result<Value, VmError> {
  let err = create_syntax_error(vm, scope, message)?;
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
  scope.push_roots(&[Value::Object(obj), default_constructor])?;

  // 1. Let C be ? Get(O, "constructor").
  let ctor_key_s = scope.alloc_string("constructor")?;
  scope.push_root(Value::String(ctor_key_s))?;
  let ctor_key = PropertyKey::from_string(ctor_key_s);
  let c = scope.get_with_host_and_hooks(vm, host, hooks, obj, ctor_key, Value::Object(obj))?;
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
  let s = scope.get_with_host_and_hooks(vm, host, hooks, c_obj, species_key, Value::Object(c_obj))?;
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
pub(crate) fn promise_resolve_abstract(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  constructor: Value,
  x: Value,
) -> Result<GcObject, VmError> {
  let mut scope = scope.reborrow();
  // Root inputs across allocations/GC.
  scope.push_roots(&[constructor, x])?;

  if let Value::Object(obj) = x {
    if scope.heap().is_promise_object(obj) {
      // `x.constructor === C`
      let ctor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(ctor_key_s))?;
      let ctor_key = PropertyKey::from_string(ctor_key_s);
      let x_ctor = match vm.get_with_host_and_hooks(host, &mut scope, hooks, obj, ctor_key) {
        Ok(v) => v,
        Err(err) => return Err(crate::vm::coerce_error_to_throw(vm, &mut scope, err)),
      };
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
    let Some(cap) = capability else {
      // Spec invariant: `NewPromiseReactionJob` is only constructed with an undefined capability for
      // `Await`, where the handlers are always callable. A missing reject handler would imply a
      // `ThrowCompletion` handler result, which the spec rules out via an assertion.
      //
      // Mirror the invariant checks in `promise_jobs::new_promise_reaction_job` so await resumption
      // uses the spec's `resultCapability = undefined` behavior.
      let Some(handler) = &reaction.handler else {
        if matches!(reaction.type_, PromiseReactionType::Reject) {
          return Err(VmError::InvariantViolation(
            "PromiseReactionJob reject handler is missing while capability is undefined",
          ));
        }
        return Ok(());
      };

      return match host.host_call_job_callback(ctx, handler, Value::Undefined, &[argument]) {
        Ok(_) => Ok(()),
        Err(VmError::Throw(_) | VmError::ThrowWithStack { .. }) => Err(VmError::InvariantViolation(
          "PromiseReactionJob handler threw while capability is undefined",
        )),
        Err(e) => Err(e),
      };
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

  // If the resolution is already a Promise object, adopt it directly by attaching reactions to its
  // internal slots.
  //
  // Critically, this must **not** call `thenable.then(...)` for Promise objects:
  // - Calling `%Promise.prototype.then%` would invoke `SpeciesConstructor`, which is observable via
  //   `thenable.constructor[Symbol.species]` (and should not happen for async/await or top-level
  //   await).
  // - More generally, resolving a Promise with another Promise should not be affected by tampering
  //   with the Promise's `.then` method.
  //
  // This corresponds to `PerformPromiseThen(thenable, resolve, reject, resultCapability = undefined)`.
  if scope.heap().is_promise_object(thenable_obj) {
    // Root `thenable_obj` while allocating the resolving functions + appending/enqueueing
    // reactions: these operations can allocate/GC.
    scope.push_root(Value::Object(thenable_obj))?;
    let (resolve, reject) = create_promise_resolving_functions(vm, scope, promise)?;

    let mut then_scope = scope.reborrow();
    then_scope.push_roots(&[Value::Object(thenable_obj), resolve, reject])?;
    perform_promise_then_no_capability(
      vm,
      &mut then_scope,
      hooks,
      Value::Object(thenable_obj),
      resolve,
      reject,
    )?;
    return Ok(());
  }

  // Get `thenable.then`.
  //
  // Spec: this must perform `Get(thenable, "then")`, which means it must:
  // - invoke Proxy `get` traps,
  // - consult host exotic getters after an own-property miss and before walking the prototype chain,
  // - traverse the prototype chain,
  // - and invoke accessor getters.
  let then_result = {
    // Root `thenable_obj` while allocating the property key.
    let mut key_scope = scope.reborrow();
    key_scope.push_root(Value::Object(thenable_obj))?;

    let receiver = Value::Object(thenable_obj);
    let then_key_s = key_scope.alloc_string("then")?;
    // Root key + receiver before calling into host hooks / invoking accessors.
    key_scope.push_roots(&[Value::String(then_key_s), receiver])?;
    let then_key = PropertyKey::from_string(then_key_s);

    match key_scope.get_with_host_and_hooks(vm, host, hooks, thenable_obj, then_key, receiver) {
      Ok(v) => Ok(v),
      Err(err) => Err(crate::vm::coerce_error_to_throw(vm, &mut key_scope, err)),
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

  // Per spec, the thenable job must use *fresh* resolving functions for `promise` (with their own
  // alreadyResolved record).
  scope.push_root(Value::Object(thenable_obj))?;

  // Fast path for native Promises: if `then` is the intrinsic `%Promise.prototype%.then`, adopt the
  // thenable's state via `PerformPromiseThen` (without a derived promise/capability). This avoids
  // invoking `SpeciesConstructor` via `Promise.prototype.then`, which must not be observable in
  // async/await (`Await` uses `PromiseResolve(%Promise%, value)`).
  if scope.heap().is_promise_object(thenable_obj) {
    let then_key_s = scope.alloc_string("then")?;
    scope.push_root(Value::String(then_key_s))?;
    let then_key = PropertyKey::from_string(then_key_s);
    let intrinsic_then = scope.heap().get(intr.promise_prototype(), &then_key)?;
    if then.same_value(intrinsic_then, scope.heap()) {
      let (resolve, reject) = create_promise_resolving_functions(vm, scope, promise)?;
      scope.push_roots(&[resolve, reject])?;
      perform_promise_then_no_capability(
        vm,
        scope,
        hooks,
        Value::Object(thenable_obj),
        resolve,
        reject,
      )?;
      return Ok(());
    }
  }

  let (resolve, reject) = create_promise_resolving_functions(vm, scope, promise)?;

  // Enqueue PromiseResolveThenableJob(promise, thenable, then).
  let then_job_callback = hooks.host_make_job_callback(then_obj);

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

/// `%IteratorPrototype%[@@iterator]` (ECMA-262).
///
/// All built-in iterator objects are iterable (calling `@@iterator` returns the iterator itself).
pub fn iterator_prototype_iterator(
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
  scope.push_roots(&[Value::Object(promise), on_fulfilled, on_rejected])?;
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

  // Root the input Promise + handlers before allocating the derived promise/capability.
  //
  // `Promise.prototype.then` allocates several objects (the derived promise and resolving
  // functions). Those allocations can trigger GC, and the incoming `this`/handler values are only
  // held in Rust locals at this point (not traced by GC). Root them so they remain valid.
  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::Object(promise), on_fulfilled, on_rejected])?;

  // Create the derived promise + capability.
  let result_promise = scope.alloc_promise_with_prototype(Some(intr.promise_prototype()))?;
  scope.push_root(Value::Object(result_promise))?;
  let (resolve, reject) = create_promise_resolving_functions(vm, &mut scope, result_promise)?;
  let capability = PromiseCapability {
    promise: Value::Object(result_promise),
    resolve,
    reject,
  };

  perform_promise_then_with_capability(
    vm,
    &mut scope,
    host,
    promise,
    on_fulfilled,
    on_rejected,
    capability,
  )
}

/// `PerformPromiseThen(promise, onFulfilled, onRejected, resultCapability = undefined)`.
///
/// This is used by the spec's `Await` abstract operation (and therefore by async/await and module
/// top-level await) to attach Promise reactions **without** creating a derived promise.
pub(crate) fn perform_promise_then_no_capability(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Value,
  on_rejected: Value,
) -> Result<(), VmError> {
  let Value::Object(promise_obj) = promise else {
    return Err(VmError::TypeError("expected Promise object"));
  };
  if !scope.heap().is_promise_object(promise_obj) {
    return Err(VmError::TypeError("expected Promise object"));
  }

  // `PerformPromiseThen`: unhandled rejection tracking.
  let was_handled = scope.heap().promise_is_handled(promise_obj)?;
  if scope.heap().promise_state(promise_obj)? == PromiseState::Rejected && !was_handled {
    host.host_promise_rejection_tracker(
      PromiseHandle(promise_obj),
      PromiseRejectionOperation::Handle,
    );
  }

  // `PerformPromiseThen` sets `[[PromiseIsHandled]] = true`.
  scope.heap_mut().promise_set_is_handled(promise_obj, true)?;

  // `Await` always provides callable handlers; treat non-callable values as an engine invariant
  // violation so we don't accidentally model the `Identity`/`Thrower` substitution from
  // `Promise.prototype.then`.
  let Value::Object(on_fulfilled_obj) = on_fulfilled else {
    return Err(VmError::InvariantViolation(
      "PerformPromiseThen(no capability) onFulfilled is not an object",
    ));
  };
  if !scope.heap().is_callable(Value::Object(on_fulfilled_obj))? {
    return Err(VmError::InvariantViolation(
      "PerformPromiseThen(no capability) onFulfilled is not callable",
    ));
  }
  let Value::Object(on_rejected_obj) = on_rejected else {
    return Err(VmError::InvariantViolation(
      "PerformPromiseThen(no capability) onRejected is not an object",
    ));
  };
  if !scope.heap().is_callable(Value::Object(on_rejected_obj))? {
    return Err(VmError::InvariantViolation(
      "PerformPromiseThen(no capability) onRejected is not callable",
    ));
  }

  let fulfill_reaction = PromiseReaction {
    capability: None,
    type_: PromiseReactionType::Fulfill,
    handler: Some(host.host_make_job_callback(on_fulfilled_obj)),
  };
  let reject_reaction = PromiseReaction {
    capability: None,
    type_: PromiseReactionType::Reject,
    handler: Some(host.host_make_job_callback(on_rejected_obj)),
  };

  let current_realm = vm.current_realm();

  match scope.heap().promise_state(promise_obj)? {
    PromiseState::Pending => {
      scope.promise_append_fulfill_reaction(promise_obj, fulfill_reaction)?;
      scope.promise_append_reject_reaction(promise_obj, reject_reaction)?;
    }
    PromiseState::Fulfilled => {
      let arg = scope
        .heap()
        .promise_result(promise_obj)?
        .unwrap_or(Value::Undefined);
      enqueue_promise_reaction_job(host, scope, fulfill_reaction, arg, current_realm)?;
    }
    PromiseState::Rejected => {
      let arg = scope
        .heap()
        .promise_result(promise_obj)?
        .unwrap_or(Value::Undefined);
      enqueue_promise_reaction_job(host, scope, reject_reaction, arg, current_realm)?;
    }
  }

  Ok(())
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
  scope.push_roots(&[receiver, on_fulfilled, on_rejected])?;

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
  let then = scope.get_with_host_and_hooks(vm, host, hooks, obj, then_key, receiver)?;
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
  scope.push_roots(&[Value::Object(promise), on_fulfilled, on_rejected])?;

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
  scope.push_roots(&[Value::Object(promise), on_finally])?;

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
  scope.push_roots(&[Value::Object(then_finally), Value::Object(catch_finally)])?;

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
  scope.push_roots(&[Value::Object(promise_obj), captured])?;
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
  // Spec: `GetPromiseResolve(C)` uses `Get(C, "resolve")`, which must be Proxy-aware.
  let resolve = key_scope.get_with_host_and_hooks(vm, host, hooks, c, resolve_key, constructor)?;
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
  record_scope.push_roots(&[Value::Object(prototype), initial])?;

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
  scope.push_roots(&[Value::Object(record), value])?;
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
  // Spec: `Invoke(V, P)` uses `GetV(V, P)` / `[[Get]]`, which must be Proxy-aware.
  let then = invoke_scope.get_with_host_and_hooks(vm, host, hooks, obj, then_key, next_promise)?;
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
    let next_value = match crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record) {
      Ok(v) => v,
      // `IteratorNext` marks the iterator record as done on `next()` errors, so outer wrappers will
      // correctly skip `IteratorClose` in that case. For errors while reading `done`/`value`,
      // `[[Done]]` remains false and outer wrappers will attempt `IteratorClose`.
      Err(err) => return Err(err),
    };
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
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = completion.is_throw_completion();
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        ) {
          Ok(()) => {}
          Err(close_err) => {
            // Only propagate close errors for non-catchable failures; otherwise preserve the
            // original throw completion.
            if original_is_throw && !close_err.is_throw_completion() {
              completion = close_err;
            }
          }
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
    let next_value = match crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record) {
      Ok(v) => v,
      // See `perform_promise_all` for why we don't force `[[Done]] = true` here.
      Err(err) => return Err(err),
    };
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
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = completion.is_throw_completion();
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        ) {
          Ok(()) => {}
          Err(close_err) => {
            // Only propagate close errors for non-catchable failures; otherwise preserve the
            // original throw completion.
            if original_is_throw && !close_err.is_throw_completion() {
              completion = close_err;
            }
          }
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
    let next_value = match crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record) {
      Ok(v) => v,
      // See `perform_promise_all` for why we don't force `[[Done]] = true` here.
      Err(err) => return Err(err),
    };
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
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = completion.is_throw_completion();
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        ) {
          Ok(()) => {}
          Err(close_err) => {
            // Only propagate close errors for non-catchable failures; otherwise preserve the
            // original throw completion.
            if original_is_throw && !close_err.is_throw_completion() {
              completion = close_err;
            }
          }
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
    let next_value = match crate::iterator::iterator_step_value(vm, host, hooks, scope, iterator_record) {
      Ok(v) => v,
      // See `perform_promise_all` for why we don't force `[[Done]] = true` here.
      Err(err) => return Err(err),
    };
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
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = completion.is_throw_completion();
        let pending_root =
          completion.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
        match crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        ) {
          Ok(()) => {}
          Err(close_err) => {
            // Only propagate close errors for non-catchable failures; otherwise preserve the
            // original throw completion.
            if original_is_throw && !close_err.is_throw_completion() {
              completion = close_err;
            }
          }
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
    idx_scope.push_roots(&[Value::Object(values), Value::Object(obj)])?;
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

fn alloc_string_from_usize(scope: &mut Scope<'_>, n: usize) -> Result<crate::GcString, VmError> {
  // Avoid intermediate Rust `String` allocations (which are infallible and can abort the process on
  // allocator OOM).
  if n == 0 {
    return scope.alloc_string("0");
  }

  // `usize::MAX` is at most 20 decimal digits on 64-bit platforms; keep a larger buffer for safety.
  let mut buf = [0u8; 32];
  let mut pos = buf.len();
  let mut x = n;
  while x > 0 {
    let digit = (x % 10) as u8;
    x /= 10;
    pos -= 1;
    buf[pos] = b'0' + digit;
  }

  let s = std::str::from_utf8(&buf[pos..]).unwrap_or("0");
  scope.alloc_string(s)
}

fn iterator_result_object(
  scope: &mut Scope<'_>,
  object_prototype: GcObject,
  value: Value,
  done: bool,
) -> Result<GcObject, VmError> {
  // Root the produced value across allocations for the result object and its property keys.
  let mut scope = scope.reborrow();
  scope.push_root(value)?;

  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(object_prototype))?;

  let value_key = string_key(&mut scope, "value")?;
  let done_key = string_key(&mut scope, "done")?;
  scope.define_property(out, value_key, data_desc(value, true, true, true))?;
  scope.define_property(out, done_key, data_desc(Value::Bool(done), true, true, true))?;
  Ok(out)
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

fn to_length_usize(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<usize, VmError> {
  scope.to_length(vm, host, hooks, value)
}

fn length_of_array_like_usize(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<usize, VmError> {
  crate::spec_ops::length_of_array_like_with_host_and_hooks(vm, scope, host, hooks, obj)
}

fn to_length_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<usize, VmError> {
  let n = scope.to_number(vm, host, hooks, value)?;
  if n.is_nan() || n <= 0.0 {
    return Ok(0);
  }
  if !n.is_finite() {
    return Ok(if n.is_sign_negative() { 0 } else { usize::MAX });
  }
  let n = n.trunc();
  if n <= 0.0 {
    return Ok(0);
  }
  Ok((n.min(usize::MAX as f64)) as usize)
}

fn require_regexp_object(scope: &mut Scope<'_>, value: Value) -> Result<GcObject, VmError> {
  let obj = require_object(value)?;
  if !scope.heap().is_regexp_object(obj) {
    return Err(VmError::TypeError("RegExp method called on incompatible receiver"));
  }
  Ok(obj)
}

fn regexp_get_last_index_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  rx: GcObject,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(rx))?;
  let key = string_key(&mut scope, "lastIndex")?;
  let v = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, rx, key, Value::Object(rx))?;
  Ok(v)
}

fn regexp_get_last_index(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  rx: GcObject,
) -> Result<usize, VmError> {
  let v = regexp_get_last_index_value(vm, scope, host, hooks, rx)?;
  to_length_with_host_and_hooks(vm, scope, host, hooks, v)
}

fn regexp_set_last_index(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  rx: GcObject,
  index: Value,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(rx))?;
  scope.push_root(index)?;
  let key = string_key(&mut scope, "lastIndex")?;
  let ok = scope.ordinary_set_with_host_and_hooks(
    vm,
    host,
    hooks,
    rx,
    key,
    index,
    Value::Object(rx),
  )?;
  if !ok {
    return Err(VmError::TypeError("RegExp lastIndex is not writable"));
  }
  Ok(())
}

#[derive(Debug, Clone)]
struct RegExpExecRaw {
  m: crate::regexp::RegExpMatch,
  index: usize,
}

#[derive(Debug, Clone)]
struct RegExpExecArray {
  array: GcObject,
  match_len: usize,
}

fn regexp_exec_raw(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  rx: GcObject,
  input: GcString,
) -> Result<Option<RegExpExecRaw>, VmError> {
  let flags = scope.heap().regexp_flags(rx)?;
  let global_or_sticky = flags.global || flags.sticky;

  // Root `rx`/`input` while reading/writing lastIndex.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(rx))?;
  scope.push_root(Value::String(input))?;

  let s_len = {
    let s = scope.heap().get_string(input)?;
    s.len_code_units()
  };

  let mut start = 0usize;
  if global_or_sticky {
    start = regexp_get_last_index(vm, &mut scope, host, hooks, rx)?;
  }

  if start > s_len {
    if global_or_sticky {
      regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(0.0))?;
    }
    return Ok(None);
  }

  let program = scope.heap().regexp_program(rx)?;

  let mut k = start;
  loop {
    if k > s_len {
      break;
    }
    // Run the VM at this candidate index (anchored).
    let m = {
      let s = scope.heap().get_string(input)?;
      let mut tick = || vm.tick();
      program.exec_at(s.as_code_units(), k, flags, &mut tick, None)?
    };
    if let Some(m) = m {
      let end = m.end;
      if global_or_sticky {
        regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(end as f64))?;
      }
      return Ok(Some(RegExpExecRaw { m, index: k }));
    }
    if flags.sticky {
      break;
    }
    k = {
      let js = scope.heap().get_string(input)?;
      advance_string_index(js.as_code_units(), k, flags.unicode)
    };
    if k > s_len {
      break;
    }
    if k % 1024 == 0 {
      vm.tick()?;
    }
  }

  if global_or_sticky {
    regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(0.0))?;
  }
  Ok(None)
}

fn regexp_exec_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  rx: GcObject,
  input: GcString,
) -> Result<Option<RegExpExecArray>, VmError> {
  let Some(raw) = regexp_exec_raw(vm, scope, host, hooks, rx, input)? else {
    return Ok(None);
  };
  let flags = scope.heap().regexp_flags(rx)?;
  let program = scope.heap().regexp_program(rx)?;
  let capture_count = program.capture_count;

  let match_len = raw.m.end.saturating_sub(raw.index);
  let array_len_u32 = u32::try_from(capture_count).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, scope, array_len_u32)?;

  // Root `array` + `input` across element allocations.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(array))?;
  scope.push_root(Value::Object(rx))?;
  scope.push_root(Value::String(input))?;

  // Fill captures.
  for i in 0..capture_count {
    if i % 64 == 0 {
      vm.tick()?;
    }
    let start_slot = i.saturating_mul(2);
    let end_slot = start_slot.saturating_add(1);
    let (start, end) = (
      raw.m.captures.get(start_slot).copied().unwrap_or(usize::MAX),
      raw.m.captures.get(end_slot).copied().unwrap_or(usize::MAX),
    );
    let value = if start == usize::MAX || end == usize::MAX || end < start {
      Value::Undefined
    } else {
      let units: Vec<u16> = {
        let s = scope.heap().get_string(input)?;
        let slice = &s.as_code_units()[start..end];
        let mut buf: Vec<u16> = Vec::new();
        buf
          .try_reserve_exact(slice.len())
          .map_err(|_| VmError::OutOfMemory)?;
        buf.extend_from_slice(slice);
        buf
      };
      let s = scope.alloc_string_from_u16_vec(units)?;
      scope.push_root(Value::String(s))?;
      Value::String(s)
    };
    let key_s = scope.alloc_string(&i.to_string())?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(array, key, data_desc(value, true, true, true))?;
  }

  // index / input
  {
    let index_key = string_key(&mut scope, "index")?;
    scope.define_property(
      array,
      index_key,
      data_desc(Value::Number(raw.index as f64), true, false, true),
    )?;
    let input_key = string_key(&mut scope, "input")?;
    scope.define_property(
      array,
      input_key,
      data_desc(Value::String(input), true, false, true),
    )?;
  }

  // If this is a sticky regexp, ECMAScript sets `lastIndex` based on match end; our exec already did.
  // Preserve the `u` flag for callers implementing `AdvanceStringIndex`.
  let _ = flags;

  Ok(Some(RegExpExecArray {
    array,
    match_len,
  }))
}

fn vec_try_push<T>(buf: &mut Vec<T>, value: T) -> Result<(), VmError> {
  if buf.len() == buf.capacity() {
    buf.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  }
  buf.push(value);
  Ok(())
}

fn vec_try_extend_from_slice<T: Copy>(
  buf: &mut Vec<T>,
  slice: &[T],
  tick: impl FnMut() -> Result<(), VmError>,
) -> Result<(), VmError> {
  crate::tick::vec_try_extend_from_slice_with_ticks(buf, slice, tick)
}

fn vec_try_extend_from_slice_u16_with_ticks(
  vm: &mut Vm,
  buf: &mut Vec<u16>,
  slice: &[u16],
) -> Result<(), VmError> {
  // Budget large `O(n)` copies by extending in chunks and ticking between chunks.
  const TICK_EVERY: usize = 4096;
  let needed = slice
    .len()
    .saturating_sub(buf.capacity().saturating_sub(buf.len()));
  if needed > 0 {
    buf.try_reserve(needed).map_err(|_| VmError::OutOfMemory)?;
  }

  let mut start = 0usize;
  while start < slice.len() {
    let end = slice
      .len()
      .min(start.saturating_add(TICK_EVERY));
    buf.extend_from_slice(&slice[start..end]);
    start = end;
    if start < slice.len() {
      vm.tick()?;
    }
  }
  Ok(())
}

fn alloc_string_from_utf8_with_ticks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  s: &str,
) -> Result<crate::GcString, VmError> {
  // `vm-js` budgets large `O(n)` string conversions by ticking while encoding UTF-8 into UTF-16
  // code units. This avoids `Function.prototype.toString` being an unbudgeted escape hatch for
  // arbitrarily large source strings.
  const TICK_EVERY: usize = 4096;
  let mut units_len: usize = 0;
  for (i, _) in s.encode_utf16().enumerate() {
    if i % TICK_EVERY == 0 {
      vm.tick()?;
    }
    units_len = units_len.saturating_add(1);
  }

  let mut units: Vec<u16> = Vec::new();
  units
    .try_reserve_exact(units_len)
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, unit) in s.encode_utf16().enumerate() {
    if i % TICK_EVERY == 0 {
      vm.tick()?;
    }
    units.push(unit);
  }

  debug_assert_eq!(units.len(), units_len);
  scope.alloc_string_from_u16_vec(units)
}

fn canonical_native_function_string(
  scope: &mut Scope<'_>,
  name: crate::GcString,
) -> Result<crate::GcString, VmError> {
  const PREFIX: [u16; 9] = [
    b'f' as u16,
    b'u' as u16,
    b'n' as u16,
    b'c' as u16,
    b't' as u16,
    b'i' as u16,
    b'o' as u16,
    b'n' as u16,
    b' ' as u16,
  ];
  const SUFFIX: [u16; 20] = [
    b'(' as u16,
    b')' as u16,
    b' ' as u16,
    b'{' as u16,
    b' ' as u16,
    b'[' as u16,
    b'n' as u16,
    b'a' as u16,
    b't' as u16,
    b'i' as u16,
    b'v' as u16,
    b'e' as u16,
    b' ' as u16,
    b'c' as u16,
    b'o' as u16,
    b'd' as u16,
    b'e' as u16,
    b']' as u16,
    b' ' as u16,
    b'}' as u16,
  ];

  let name_len = scope.heap().get_string(name)?.len_code_units();
  let total_len = PREFIX
    .len()
    .saturating_add(name_len)
    .saturating_add(SUFFIX.len());
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;

  out.extend_from_slice(&PREFIX);
  out.extend_from_slice(scope.heap().get_string(name)?.as_code_units());
  out.extend_from_slice(&SUFFIX);

  debug_assert_eq!(out.len(), total_len);
  scope.alloc_string_from_u16_vec(out)
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

/// `Function.prototype.apply`.
pub fn function_prototype_apply(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if !scope.heap().is_callable(this)? {
    return Err(VmError::TypeError(
      "Function.prototype.apply called on non-callable",
    ));
  }

  let this_arg = args.first().copied().unwrap_or(Value::Undefined);
  let arg_array = args.get(1).copied().unwrap_or(Value::Undefined);

  if matches!(arg_array, Value::Undefined | Value::Null) {
    return vm.call_with_host_and_hooks(host, scope, hooks, this, this_arg, &[]);
  }

  let mut scope = scope.reborrow();
  scope.push_roots(&[this, this_arg, arg_array])?;

  let arg_obj = scope.to_object(vm, host, hooks, arg_array)?;
  let list = crate::spec_ops::create_list_from_array_like_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    arg_obj,
  )?;

  vm.call_with_host_and_hooks(host, &mut scope, hooks, this, this_arg, &list)
}

/// `Function.prototype.bind` (minimal, using `JsFunction` bound internal slots).
pub fn function_prototype_bind(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  // `bind` needs the target to be an object so it can be stored in `[[BoundTargetFunction]]`.
  let Value::Object(target) = this else {
    return Err(VmError::TypeError("Function.prototype.bind called on non-object"));
  };
  if !scope.heap().is_callable(Value::Object(target))? {
    // Includes revoked proxies (which do not have `[[Call]]`).
    return Err(VmError::TypeError("Function.prototype.bind called on non-callable"));
  }

  let bound_this = args.first().copied().unwrap_or(Value::Undefined);
  let bound_args = args.get(1..).unwrap_or(&[]);

  let mut scope = scope.reborrow();
  // Root target/bound_this across `Get` and metadata coercions (which can invoke user code).
  scope.push_root(Value::Object(target))?;
  scope.push_root(bound_this)?;

  // Spec: https://tc39.es/ecma262/#sec-function.prototype.bind
  // `targetLen = ToLength(Get(target, "length"))` where `Get` is Proxy-trap-observable.
  let length_key = string_key(&mut scope, "length")?;
  let len_value = vm.get_with_host_and_hooks(host, &mut scope, hooks, target, length_key)?;
  scope.push_root(len_value)?;
  let target_len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;
  let bound_len = target_len.saturating_sub(bound_args.len());
  let bound_len = u32::try_from(bound_len).unwrap_or(u32::MAX);

  // Create the bound function object (ordinary function with bound internal slots).
  let name = scope.alloc_string("bound")?;
  let func = scope.alloc_bound_function(target, bound_this, bound_args, name, bound_len)?;

  // Bound functions are ordinary function objects: their `[[Prototype]]` is `%Function.prototype%`.
  scope
    .heap_mut()
    .object_set_prototype(func, Some(intr.function_prototype()))?;

  // Define standard function metadata properties (`name`, `length`).
  // `targetName = Get(target, "name")`, non-string => empty string. `Get` is Proxy-trap-observable.
  let name_key = string_key(&mut scope, "name")?;
  let name_value = vm.get_with_host_and_hooks(host, &mut scope, hooks, target, name_key)?;
  scope.push_root(name_value)?;
  let target_name = match name_value {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };

  crate::function_properties::set_function_name(
    &mut scope,
    func,
    PropertyKey::String(target_name),
    Some("bound"),
  )?;
  crate::function_properties::set_function_length(&mut scope, func, bound_len)?;

  // Best-effort realm propagation: follow proxy chains to the underlying function when possible.
  let target_realm = {
    let mut obj = target;
    let mut remaining = crate::heap::MAX_PROTOTYPE_CHAIN;
    loop {
      if remaining == 0 {
        break None;
      }
      remaining -= 1;
      if let Ok(realm) = scope.heap().get_function_realm(obj) {
        break realm;
      }
      let Some(proxy) = scope.heap().get_proxy_data(obj)? else {
        break None;
      };
      let (Some(next), Some(_handler)) = (proxy.target, proxy.handler) else {
        break None;
      };
      obj = next;
    }
  };
  if let Some(realm) = target_realm {
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

/// `Function.prototype.toString` (spec-shaped, partial).
pub fn function_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  if !scope.heap().is_callable(this)? {
    return Err(VmError::TypeError(
      "Function.prototype.toString called on non-callable receiver",
    ));
  }
  let Value::Object(func_obj) = this else {
    // `is_callable` returning true implies an object, but keep this defensive to preserve the
    // TypeError shape.
    return Err(VmError::NotCallable);
  };

  // Callable Proxy objects (and other exotic callables) do not have `HeapObject::Function` payload,
  // but `Function.prototype.toString` must still succeed for them.
  //
  // Spec: https://tc39.es/ecma262/#sec-function.prototype.tostring
  let (call_handler, name, is_bound) = match scope.heap().get_function(func_obj) {
    Ok(func) => (Some(func.call.clone()), Some(func.name), func.bound_target.is_some()),
    Err(VmError::NotCallable) => (None, None, false),
    Err(other) => return Err(other),
  };

  // Bound functions never expose their target source text via `toString`; per spec (and JS engine
  // behaviour), they stringify as `[native code]`.
  if is_bound {
    let Some(name) = name else {
      return Err(VmError::InvariantViolation(
        "bound function missing internal name string",
      ));
    };
    let s = canonical_native_function_string(scope, name)?;
    return Ok(Value::String(s));
  }

  // Non-function callables have no internal name slot; stringify as an anonymous native function.
  let name = if let Some(name) = name {
    name
  } else {
    let empty = scope.alloc_string("")?;
    scope.push_root(Value::String(empty))?;
    empty
  };

  let Some(call_handler) = call_handler else {
    let s = canonical_native_function_string(scope, name)?;
    return Ok(Value::String(s));
  };

  match call_handler {
    CallHandler::Native(_) | CallHandler::User(_) => {
      let s = canonical_native_function_string(scope, name)?;
      Ok(Value::String(s))
    }
    CallHandler::Ecma(code_id) => {
      let Some((source, span_start, span_end, kind)) = vm.ecma_function_source_span(code_id) else {
        let s = canonical_native_function_string(scope, name)?;
        return Ok(Value::String(s));
      };

      let text: &str = &source.text;
      let mut start = (span_start as usize).min(text.len());
      let mut end = (span_end as usize).min(text.len());
      if start > end {
        let s = canonical_native_function_string(scope, name)?;
        return Ok(Value::String(s));
      }

      // Clamp to UTF-8 boundaries so we never panic on invalid spans.
      while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
      }
      while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
      }
      if start > end {
        let s = canonical_native_function_string(scope, name)?;
        return Ok(Value::String(s));
      }

      let Some(mut snippet) = text.get(start..end) else {
        let s = canonical_native_function_string(scope, name)?;
        return Ok(Value::String(s));
      };

      // `parse-js` spans for some expression nodes can include trailing delimiter tokens from the
      // enclosing syntax (e.g. `;` from expression statements). `Function.prototype.toString`
      // should return the function source text itself, so trim common delimiter suffixes.
      if kind == crate::vm::EcmaFunctionKind::Expr {
        let trimmed = snippet.trim_end();
        snippet = trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end();
      }

      let s = alloc_string_from_utf8_with_ticks(vm, scope, snippet)?;
      Ok(Value::String(s))
    }
  }
}

/// `Function.prototype[@@hasInstance]`.
///
/// Spec: <https://tc39.es/ecma262/#sec-function.prototype-@@hasinstance>
///
/// This is the default `@@hasInstance` implementation used by the `instanceof` operator.
pub fn function_prototype_symbol_has_instance(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // 1. Let F be the this value.
  // 2. Return OrdinaryHasInstance(F, V).
  //
  // Spec: https://tc39.es/ecma262/#sec-ordinaryhasinstance
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut scope = scope.reborrow();
  scope.push_roots(&[this, v])?;

  // 1. If IsCallable(C) is false, return false.
  if !scope.heap().is_callable(this)? {
    return Ok(Value::Bool(false));
  }
  let Value::Object(mut constructor) = this else {
    // `IsCallable` returning true for a non-object would be an internal bug.
    return Err(VmError::InvariantViolation(
      "Function.prototype[@@hasInstance]: IsCallable returned true for non-object",
    ));
  };

  // 2. Bound functions delegate `instanceof` checks to their target.
  let mut bound_steps = 0usize;
  while let Ok(func) = scope.heap().get_function(constructor) {
    let Some(bound_target) = func.bound_target else {
      break;
    };

    // Budget extremely deep bound chains, and prevent hangs if an invariant is violated.
    const TICK_EVERY: usize = 32;
    if bound_steps != 0 && bound_steps % TICK_EVERY == 0 {
      vm.tick()?;
    }

    if bound_steps >= crate::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }
    bound_steps += 1;
    constructor = bound_target;
  }

  // 3. If Type(O) is not Object, return false.
  let Value::Object(object) = v else {
    return Ok(Value::Bool(false));
  };

  // 4. Let P be Get(C, "prototype").
  let prototype_s = scope.alloc_string("prototype")?;
  scope.push_root(Value::String(prototype_s))?;
  let prototype = scope.get_with_host_and_hooks(
    vm,
    host,
    hooks,
    constructor,
    PropertyKey::from_string(prototype_s),
    Value::Object(constructor),
  )?;

  // 5. If Type(P) is not Object, throw a TypeError exception.
  let Value::Object(prototype) = prototype else {
    return Err(VmError::TypeError(
      "Function has non-object prototype in instanceof check",
    ));
  };

  // Root `prototype` for the duration of the algorithm. For Proxy constructors, `Get(C,
  // "prototype")` can return an object that is not reachable from `C`/its target/handler, and we
  // must keep it alive across the prototype-chain walk.
  scope.push_root(Value::Object(prototype))?;

  // 6. Repeat
  //   a. Let O be O.[[GetPrototypeOf]]().
  //   b. ReturnIfAbrupt(O).
  //   c. If O is null, return false.
  //   d. If SameValue(P, O) is true, return true.
  let mut current = scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, object)?;
  let mut steps = 0usize;
  let mut visited: HashSet<GcObject> = HashSet::new();
  while let Some(obj) = current {
    // Budget the prototype traversal: hostile inputs can synthesize extremely deep chains.
    //
    // Note: avoid ticking on the first iteration so shallow checks don't double-charge fuel (the
    // surrounding expression/call already ticks).
    const TICK_EVERY: usize = 32;
    if steps != 0 && steps % TICK_EVERY == 0 {
      vm.tick()?;
    }

    if steps >= crate::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }
    steps += 1;

    if visited.try_reserve(1).is_err() {
      return Err(VmError::OutOfMemory);
    }
    if !visited.insert(obj) {
      return Err(VmError::PrototypeCycle);
    }

    // Root this prototype step. Proxy `getPrototypeOf` traps can synthesize objects that are not
    // necessarily reachable from the original LHS; keep them alive until the algorithm completes.
    scope.push_root(Value::Object(obj))?;

    if obj == prototype {
      return Ok(Value::Bool(true));
    }
    current = scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)?;
  }

  Ok(Value::Bool(false))
}

/// `Object.prototype.toString`.
pub fn object_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.tostring
  //
  // We keep the algorithm spec-shaped:
  // 1. Special-case `undefined` / `null`.
  // 2. `O = ToObject(this)`.
  // 3. Compute a `builtinTag` (Array / Arguments / Function / Error / primitive wrappers / Date /
  //    RegExp / Generator / Object).
  // 4. `tag = Get(O, @@toStringTag)` (prototype chain lookup; host-aware; Proxy-aware).
  // 5. If `tag` is a string, use it; otherwise use `builtinTag`.
  // 6. Return `[object ${tag}]`.
  let mut scope = scope.reborrow();

  const PREFIX: [u16; 8] = [
    b'[' as u16,
    b'o' as u16,
    b'b' as u16,
    b'j' as u16,
    b'e' as u16,
    b'c' as u16,
    b't' as u16,
    b' ' as u16,
  ];
  const SUFFIX: [u16; 1] = [b']' as u16];

  const TAG_UNDEFINED: [u16; 9] = [
    b'U' as u16,
    b'n' as u16,
    b'd' as u16,
    b'e' as u16,
    b'f' as u16,
    b'i' as u16,
    b'n' as u16,
    b'e' as u16,
    b'd' as u16,
  ];
  const TAG_NULL: [u16; 4] = [b'N' as u16, b'u' as u16, b'l' as u16, b'l' as u16];
  const TAG_ARGUMENTS: [u16; 9] = [
    b'A' as u16,
    b'r' as u16,
    b'g' as u16,
    b'u' as u16,
    b'm' as u16,
    b'e' as u16,
    b'n' as u16,
    b't' as u16,
    b's' as u16,
  ];
  const TAG_BOOLEAN: [u16; 7] = [
    b'B' as u16,
    b'o' as u16,
    b'o' as u16,
    b'l' as u16,
    b'e' as u16,
    b'a' as u16,
    b'n' as u16,
  ];
  const TAG_NUMBER: [u16; 6] = [b'N' as u16, b'u' as u16, b'm' as u16, b'b' as u16, b'e' as u16, b'r' as u16];
  const TAG_STRING: [u16; 6] = [b'S' as u16, b't' as u16, b'r' as u16, b'i' as u16, b'n' as u16, b'g' as u16];
  const TAG_ARRAY: [u16; 5] = [b'A' as u16, b'r' as u16, b'r' as u16, b'a' as u16, b'y' as u16];
  const TAG_DATE: [u16; 4] = [b'D' as u16, b'a' as u16, b't' as u16, b'e' as u16];
  const TAG_REGEXP: [u16; 6] = [b'R' as u16, b'e' as u16, b'g' as u16, b'E' as u16, b'x' as u16, b'p' as u16];
  const TAG_FUNCTION: [u16; 8] = [
    b'F' as u16,
    b'u' as u16,
    b'n' as u16,
    b'c' as u16,
    b't' as u16,
    b'i' as u16,
    b'o' as u16,
    b'n' as u16,
  ];
  const TAG_GENERATOR: [u16; 9] = [
    b'G' as u16,
    b'e' as u16,
    b'n' as u16,
    b'e' as u16,
    b'r' as u16,
    b'a' as u16,
    b't' as u16,
    b'o' as u16,
    b'r' as u16,
  ];
  const TAG_ERROR: [u16; 5] = [b'E' as u16, b'r' as u16, b'r' as u16, b'o' as u16, b'r' as u16];
  const TAG_OBJECT: [u16; 6] = [b'O' as u16, b'b' as u16, b'j' as u16, b'e' as u16, b'c' as u16, b't' as u16];

  // 1. Handle `undefined` / `null` early.
  if matches!(this, Value::Undefined) {
    let mut out = Vec::new();
    out
      .try_reserve_exact(PREFIX.len() + TAG_UNDEFINED.len() + SUFFIX.len())
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(&PREFIX);
    out.extend_from_slice(&TAG_UNDEFINED);
    out.extend_from_slice(&SUFFIX);
    return Ok(Value::String(scope.alloc_string_from_u16_vec(out)?));
  }
  if matches!(this, Value::Null) {
    let mut out = Vec::new();
    out
      .try_reserve_exact(PREFIX.len() + TAG_NULL.len() + SUFFIX.len())
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(&PREFIX);
    out.extend_from_slice(&TAG_NULL);
    out.extend_from_slice(&SUFFIX);
    return Ok(Value::String(scope.alloc_string_from_u16_vec(out)?));
  }

  let intr = require_intrinsics(vm)?;
  let o = scope.to_object(vm, host, hooks, this)?;
  let receiver = Value::Object(o);
  // Root `O` while computing tags and allocating the output string.
  scope.push_root(receiver)?;

  // --- `builtinTag` (ECMA-262) ---
  let is_array = crate::spec_ops::is_array_with_host_and_hooks(vm, &mut scope, host, hooks, receiver)?;

  // vm-js models primitive wrapper internal slots (e.g. [[BooleanData]]) via hidden symbol marker
  // properties stored as own data properties.
  //
  // Important: these internal slots are *not* present on Proxy objects, even when the target has
  // them.
  let can_check_markers = !scope.heap().is_proxy_object(o);
  let heap = scope.heap();
  let has_marker = |marker: Option<crate::GcSymbol>| -> Result<bool, VmError> {
    let Some(marker_sym) = marker else {
      return Ok(false);
    };
    let key = PropertyKey::from_symbol(marker_sym);
    Ok(
      heap
        .object_get_own_property(o, &key)?
        .map(|d| d.is_data_descriptor())
        .unwrap_or(false),
    )
  };

  let builtin_tag: &[u16] = if is_array {
    &TAG_ARRAY
  } else if scope.heap().is_arguments_object(o) {
    &TAG_ARGUMENTS
  } else if scope.heap().is_callable(receiver)? {
    // `IsCallable` follows Proxy chains (and is intentionally non-throwing for revoked proxies).
    &TAG_FUNCTION
  } else if scope.heap().is_error_object(o) {
    &TAG_ERROR
  } else if can_check_markers && has_marker(heap.internal_boolean_data_symbol())? {
    &TAG_BOOLEAN
  } else if can_check_markers && has_marker(heap.internal_number_data_symbol())? {
    &TAG_NUMBER
  } else if can_check_markers && has_marker(heap.internal_string_data_symbol())? {
    &TAG_STRING
  } else if scope.heap().is_date_object(o) {
    &TAG_DATE
  } else if scope.heap().is_regexp_object(o) {
    &TAG_REGEXP
  } else if scope.heap().is_generator_object(o) {
    &TAG_GENERATOR
  } else {
    &TAG_OBJECT
  };

  // `Get(O, @@toStringTag)`
  let to_string_tag_key = PropertyKey::from_symbol(intr.well_known_symbols().to_string_tag);
  let to_string_tag = crate::spec_ops::internal_get_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    o,
    to_string_tag_key,
    receiver,
  )?;
  let tag_string = match to_string_tag {
    Value::String(s) => Some(s),
    _ => None,
  };

  let tag_units_len = match tag_string {
    Some(s) => scope.heap().get_string(s)?.as_code_units().len(),
    None => builtin_tag.len(),
  };
  let total_len = PREFIX
    .len()
    .checked_add(tag_units_len)
    .and_then(|n| n.checked_add(SUFFIX.len()))
    .ok_or(VmError::OutOfMemory)?;
  let mut out = Vec::new();
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(&PREFIX);
  if let Some(s) = tag_string {
    {
      let units = scope.heap().get_string(s)?.as_code_units();
      out.extend_from_slice(units);
    }
  } else {
    out.extend_from_slice(builtin_tag);
  }
  out.extend_from_slice(&SUFFIX);
  Ok(Value::String(scope.alloc_string_from_u16_vec(out)?))
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
    .object_get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)?
    .is_some();
  Ok(Value::Bool(has))
}

/// `Object.prototype.__proto__` getter (Annex B).
#[allow(non_snake_case)]
pub fn object_prototype___proto___get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = scope.to_object(vm, host, hooks, this)?;
  match scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)? {
    Some(proto) => Ok(Value::Object(proto)),
    None => Ok(Value::Null),
  }
}

/// `Object.prototype.__proto__` setter (Annex B).
#[allow(non_snake_case)]
pub fn object_prototype___proto___set(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.__proto__
  //
  // The setter uses `RequireObjectCoercible` (i.e. throws on `null` / `undefined`) but is otherwise
  // a no-op for non-object receivers (primitives).
  if matches!(this, Value::Undefined | Value::Null) {
    return Err(VmError::TypeError("Cannot convert undefined or null to object"));
  }
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };
  let proto_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let proto = match proto_arg {
    Value::Object(o) => Some(o),
    Value::Null => None,
    // Spec: ignore attempts to set `[[Prototype]]` to non-object/non-null values.
    _ => return Ok(Value::Undefined),
  };

  // Root `obj`/`proto` across Proxy trap invocations.
  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::Object(obj), proto_arg])?;

  // `[[SetPrototypeOf]]` returns a boolean; per spec we return `undefined` regardless of success.
  let _ = scope.set_prototype_of_with_host_and_hooks(vm, host, hooks, obj, proto)?;
  Ok(Value::Undefined)
}

/// `Object.prototype.__defineGetter__(P, getter)` (Annex B).
#[allow(non_snake_case)]
pub fn object_prototype___define_getter__(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.__definegetter__
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let prop = args.get(0).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let getter = args.get(1).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(getter)? {
    return Err(VmError::TypeError("__defineGetter__ requires a callable getter"));
  }
  scope.push_root(getter)?;

  let ok = scope.define_own_property_with_tick(
    obj,
    key,
    PropertyDescriptorPatch {
      enumerable: Some(true),
      configurable: Some(true),
      get: Some(getter),
      ..Default::default()
    },
    || vm.tick(),
  )?;
  if !ok {
    return Err(VmError::TypeError("DefineOwnProperty rejected"));
  }
  Ok(Value::Undefined)
}

/// `Object.prototype.__defineSetter__(P, setter)` (Annex B).
#[allow(non_snake_case)]
pub fn object_prototype___define_setter__(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.__definesetter__
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let prop = args.get(0).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let setter = args.get(1).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(setter)? {
    return Err(VmError::TypeError("__defineSetter__ requires a callable setter"));
  }
  scope.push_root(setter)?;

  let ok = scope.define_own_property_with_tick(
    obj,
    key,
    PropertyDescriptorPatch {
      enumerable: Some(true),
      configurable: Some(true),
      set: Some(setter),
      ..Default::default()
    },
    || vm.tick(),
  )?;
  if !ok {
    return Err(VmError::TypeError("DefineOwnProperty rejected"));
  }
  Ok(Value::Undefined)
}

/// `Object.prototype.__lookupGetter__(P)` (Annex B).
#[allow(non_snake_case)]
pub fn object_prototype___lookup_getter__(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.__lookupgetter__
  let mut scope = scope.reborrow();

  let mut obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let prop = args.get(0).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let mut steps = 0usize;
  loop {
    if steps >= crate::heap::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }
    if steps % 1024 == 0 {
      vm.tick()?;
    }

    let desc = {
      // Root `obj` for trap lookups + allocations.
      let mut step_scope = scope.reborrow();
      step_scope.push_root(Value::Object(obj))?;
      step_scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)?
    };
    if let Some(desc) = desc {
      return Ok(match desc.kind {
        PropertyKind::Accessor { get, .. } => get,
        _ => Value::Undefined,
      });
    }

    let proto = {
      let mut step_scope = scope.reborrow();
      step_scope.push_root(Value::Object(obj))?;
      step_scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)?
    };
    let Some(proto) = proto else {
      return Ok(Value::Undefined);
    };
    obj = proto;
    steps += 1;
  }
}

/// `Object.prototype.__lookupSetter__(P)` (Annex B).
#[allow(non_snake_case)]
pub fn object_prototype___lookup_setter__(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.__lookupsetter__
  let mut scope = scope.reborrow();

  let mut obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let prop = args.get(0).copied().unwrap_or(Value::Undefined);
  let key = scope.to_property_key(vm, host, hooks, prop)?;
  root_property_key(&mut scope, key)?;

  let mut steps = 0usize;
  loop {
    if steps >= crate::heap::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }
    if steps % 1024 == 0 {
      vm.tick()?;
    }

    let desc = {
      let mut step_scope = scope.reborrow();
      step_scope.push_root(Value::Object(obj))?;
      step_scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)?
    };
    if let Some(desc) = desc {
      return Ok(match desc.kind {
        PropertyKind::Accessor { set, .. } => set,
        _ => Value::Undefined,
      });
    }

    let proto = {
      let mut step_scope = scope.reborrow();
      step_scope.push_root(Value::Object(obj))?;
      step_scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)?
    };
    let Some(proto) = proto else {
      return Ok(Value::Undefined);
    };
    obj = proto;
    steps += 1;
  }
}

/// `Object.prototype.isPrototypeOf(V)` (ECMA-262).
pub fn object_prototype_is_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(mut v_obj) = v else {
    // Spec note: `Object.prototype.isPrototypeOf` observes the argument type *before*
    // `ToObject(this value)` so `Object.prototype.isPrototypeOf.call(null, 1)` returns `false`
    // rather than throwing (legacy behaviour preserved by ECMA-262 ordering).
    return Ok(Value::Bool(false));
  };
  scope.push_roots(&[this, Value::Object(v_obj)])?;

  let o = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(o))?;

  let mut steps = 0usize;
  loop {
    if steps % 1024 == 0 {
      vm.tick()?;
    }
    if steps >= crate::heap::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }

    // Spec uses `[[GetPrototypeOf]]`, which is Proxy-aware (trap + revoked proxy errors).
    let p = {
      let mut step_scope = scope.reborrow();
      step_scope.push_root(Value::Object(v_obj))?;
      step_scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, v_obj)?
    };
    let Some(p) = p else {
      return Ok(Value::Bool(false));
    };
    if p == o {
      return Ok(Value::Bool(true));
    }
    v_obj = p;
    steps += 1;
  }
}

/// `Object.prototype.valueOf()` (ECMA-262).
pub fn object_prototype_value_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-object.prototype.valueof
  let obj = scope.to_object(vm, host, hooks, this)?;
  Ok(Value::Object(obj))
}

/// `Object.prototype.propertyIsEnumerable(V)` (ECMA-262).
pub fn object_prototype_property_is_enumerable(
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

  let Some(desc) = scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)? else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(desc.enumerable))
}

/// `Object.prototype.toLocaleString` (ECMA-262) (minimal).
pub fn object_prototype_to_locale_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: return ? Invoke(this, "toString")
  let mut scope = scope.reborrow();
  let receiver = scope.push_root(this)?;

  let obj = scope.to_object(vm, host, hooks, receiver)?;
  scope.push_root(Value::Object(obj))?;

  let to_string_key = string_key(&mut scope, "toString")?;
  let func = scope.get_with_host_and_hooks(vm, host, hooks, obj, to_string_key, receiver)?;
  if !scope.heap().is_callable(func)? {
    return Err(VmError::TypeError("toString is not callable"));
  }
  scope.push_root(func)?;

  vm.call_with_host_and_hooks(host, &mut scope, hooks, func, receiver, &[])
}

fn get_array_length(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<usize, VmError> {
  length_of_array_like_usize(vm, scope, host, hooks, obj)
}

fn internal_symbol_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  // Root the marker string while interning it: `symbol_for` can allocate and trigger GC.
  let mut scope = scope.reborrow();
  let marker = scope.alloc_string(s)?;
  scope.push_root(Value::String(marker))?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  Ok(PropertyKey::from_symbol(marker_sym))
}

const ARRAY_ITERATOR_ARRAY_MARKER: &str = "vm-js.internal.ArrayIteratorArray";
const ARRAY_ITERATOR_INDEX_MARKER: &str = "vm-js.internal.ArrayIteratorIndex";
const ARRAY_ITERATOR_KIND_MARKER: &str = "vm-js.internal.ArrayIteratorKind";
const GENERATOR_STATE_MARKER: &str = "vm-js.internal.GeneratorState";
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

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

    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;
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
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let equal = match (search, value) {
      (Value::Undefined, Value::Undefined) => true,
      (Value::Null, Value::Null) => true,
      (Value::Bool(a), Value::Bool(b)) => a == b,
      (Value::Number(a), Value::Number(b)) => a == b,
      (Value::BigInt(a), Value::BigInt(b)) => a == b,
      (Value::String(a), Value::String(b)) => {
        let a_units = iter_scope.heap().get_string(a)?.as_code_units();
        let b_units = iter_scope.heap().get_string(b)?.as_code_units();
        crate::tick::code_units_eq_with_ticks(a_units, b_units, || vm.tick())?
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;
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
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

    let equal = match (search, value) {
      (Value::Undefined, Value::Undefined) => true,
      (Value::Null, Value::Null) => true,
      (Value::Bool(a), Value::Bool(b)) => a == b,
      (Value::Number(a), Value::Number(b)) => (a == b) || (a.is_nan() && b.is_nan()),
      (Value::BigInt(a), Value::BigInt(b)) => a == b,
      (Value::String(a), Value::String(b)) => {
        let a_units = iter_scope.heap().get_string(a)?.as_code_units();
        let b_units = iter_scope.heap().get_string(b)?.as_code_units();
        crate::tick::code_units_eq_with_ticks(a_units, b_units, || vm.tick())?
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

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
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }

    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

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
      if crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
        accumulator =
          iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
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
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }

    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

  for k in 0..len {
    if k % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&k.to_string())?;
    let key = PropertyKey::from_string(key_s);
    if !crate::spec_ops::internal_has_property_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key)? {
      continue;
    }
    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;

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
    if crate::spec_ops::is_concat_spreadable_with_host_and_hooks(vm, &mut scope, host, hooks, item)? {
      let Value::Object(source_obj) = item else {
        return Err(VmError::InvariantViolation(
          "IsConcatSpreadable returned true for non-object",
        ));
      };

      // Spread array-like elements (holes preserved via length tracking).
      let source_len_value = crate::spec_ops::internal_get_with_host_and_hooks(
        vm,
        &mut scope,
        host,
        hooks,
        source_obj,
        length_key,
        Value::Object(source_obj),
      )?;
      let source_len = to_length_usize(vm, &mut scope, host, hooks, source_len_value)?;

      for k in 0..source_len {
        if k % 1024 == 0 {
          vm.tick()?;
        }
        let mut iter_scope = scope.reborrow();

        let key_s = iter_scope.alloc_string(&k.to_string())?;
        iter_scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        if crate::spec_ops::internal_has_property_with_host_and_hooks(
          vm,
          &mut iter_scope,
          host,
          hooks,
          source_obj,
          key,
        )? {
          let value = crate::spec_ops::internal_get_with_host_and_hooks(
            vm,
            &mut iter_scope,
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

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

/// `Array.prototype.sort` (ECMA-262).
pub fn array_prototype_sort(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  use std::cmp::Ordering;
  const TICK_EVERY: usize = 1024;

  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let comparefn = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(comparefn, Value::Undefined) && !scope.heap().is_callable(comparefn)? {
    return Err(VmError::TypeError("Array.prototype.sort compareFn is not callable"));
  }
  scope.push_root(comparefn)?;

  let length_key = string_key(&mut scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  let len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;

  #[derive(Clone, Copy)]
  struct SortItem {
    value: Value,
    original_pos: usize,
  }

  // --- SortIndexedProperties(O, len, SortCompare, SKIP-HOLES) ---
  //
  // Collect present indices (including inherited properties), skipping holes.
  let mut items: Vec<SortItem> = Vec::new();
  for k in 0..len {
    if k % TICK_EVERY == 0 {
      vm.tick()?;
    }

    let value = {
      let mut iter_scope = scope.reborrow();
      let key_s = iter_scope.alloc_string(&k.to_string())?;
      iter_scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      if !iter_scope.ordinary_has_property_with_tick(obj, key, || vm.tick())? {
        None
      } else {
        Some(iter_scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          key,
          Value::Object(obj),
        )?)
      }
    };

    let Some(value) = value else {
      continue;
    };

    // Root captured values for the duration of the sort so they remain alive even if user
    // comparator/toString side-effects delete them from the receiver.
    scope.push_root(value)?;
    let original_pos = items.len();
    vec_try_push(
      &mut items,
      SortItem {
        value,
        original_pos,
      },
    )?;
  }

  // --- Sort values (stable; ES2019+) ---
  let mut sort_err: Option<VmError> = None;
  let mut compare_count: usize = 0;
  items.sort_unstable_by(|a, b| {
    if sort_err.is_some() {
      return a.original_pos.cmp(&b.original_pos);
    }

    compare_count = compare_count.wrapping_add(1);
    if compare_count % TICK_EVERY == 0 {
      if let Err(e) = vm.tick() {
        sort_err = Some(e);
        return a.original_pos.cmp(&b.original_pos);
      }
    }

    let result: Result<Ordering, VmError> = (|| {
      // Undefined sorts to the end (regardless of comparefn).
      match (a.value, b.value) {
        (Value::Undefined, Value::Undefined) => return Ok(Ordering::Equal),
        (Value::Undefined, _) => return Ok(Ordering::Greater),
        (_, Value::Undefined) => return Ok(Ordering::Less),
        _ => {}
      }

      // User comparefn.
      if !matches!(comparefn, Value::Undefined) {
        let cmp_value = vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          comparefn,
          Value::Undefined,
          &[a.value, b.value],
        )?;
        let n = scope.to_number(vm, host, hooks, cmp_value)?;
        if n.is_nan() || n == 0.0 {
          return Ok(Ordering::Equal);
        }
        return Ok(if n < 0.0 { Ordering::Less } else { Ordering::Greater });
      }

      // Default string comparison (UTF-16 code unit order).
      //
      // Root both string results so the first string doesn't get collected if allocating the second
      // triggers GC.
      let mut cmp_scope = scope.reborrow();
      cmp_scope.push_roots(&[a.value, b.value])?;

      let a_str = cmp_scope.to_string(vm, host, hooks, a.value)?;
      cmp_scope.push_root(Value::String(a_str))?;
      let b_str = cmp_scope.to_string(vm, host, hooks, b.value)?;
      cmp_scope.push_root(Value::String(b_str))?;

      let a_units = cmp_scope.heap().get_string(a_str)?.as_code_units();
      let b_units = cmp_scope.heap().get_string(b_str)?.as_code_units();
      Ok(a_units.cmp(b_units))
    })();

    let ord = match result {
      Ok(ord) => ord,
      Err(e) => {
        sort_err = Some(e);
        Ordering::Equal
      }
    };

    if ord == Ordering::Equal {
      // Ensure stability by falling back to the original collection order when `SortCompare`
      // produces 0.
      a.original_pos.cmp(&b.original_pos)
    } else {
      ord
    }
  });

  if let Some(err) = sort_err {
    return Err(err);
  }

  // --- Write back ---
  //
  // Spec: `Set(O, ToString(j), sortedList[j], true)` for j < itemCount
  //       `DeletePropertyOrThrow(O, ToString(j))` for j >= itemCount
  let item_count = items.len();
  for j in 0..item_count {
    if j % TICK_EVERY == 0 {
      vm.tick()?;
    }
    let value = items[j].value;
    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&j.to_string())?;
    iter_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
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
      return Err(VmError::TypeError("Array.prototype.sort failed"));
    }
  }

  for j in item_count..len {
    if j % TICK_EVERY == 0 {
      vm.tick()?;
    }
    let mut iter_scope = scope.reborrow();
    let key_s = iter_scope.alloc_string(&j.to_string())?;
    iter_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    iter_scope.delete_property_or_throw(obj, key)?;
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
  let mut scope = scope.reborrow();

  let obj = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(obj))?;

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

  let sep = match args.first().copied() {
    None | Some(Value::Undefined) => scope.alloc_string(",")?,
    Some(v) => scope.to_string(vm, host, hooks, v)?,
  };

  if len == 0 {
    return Ok(Value::String(scope.alloc_string("")?));
  }

  let mut sep_units: Vec<u16> = Vec::new();
  if len > 1 {
    let sep_slice = scope.heap().get_string(sep)?.as_code_units();
    vec_try_extend_from_slice(&mut sep_units, sep_slice, || vm.tick())?;
  }

  let mut out: Vec<u16> = Vec::new();
  let max_bytes = scope.heap().limits().max_bytes;

  for i in 0..len {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    if i > 0 && !sep_units.is_empty() {
      if JsString::heap_size_bytes_for_len(out.len().saturating_add(sep_units.len())) > max_bytes {
        return Err(VmError::OutOfMemory);
      }
      vec_try_extend_from_slice(&mut out, &sep_units, || vm.tick())?;
    }

    // Use a nested scope so per-iteration roots do not accumulate.
    let mut iter_scope = scope.reborrow();

    let key_s = alloc_string_from_usize(&mut iter_scope, i)?;
    iter_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    let value = vm.get_with_host_and_hooks(host, &mut iter_scope, hooks, obj, key)?;
    if matches!(value, Value::Undefined | Value::Null) {
      continue;
    }

    let part = iter_scope.to_string(vm, host, hooks, value)?;
    let units = iter_scope.heap().get_string(part)?.as_code_units();
    if JsString::heap_size_bytes_for_len(out.len().saturating_add(units.len())) > max_bytes {
      return Err(VmError::OutOfMemory);
    }
    vec_try_extend_from_slice(&mut out, units, || vm.tick())?;
  }

  let s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(s))
}

/// `Array.prototype.toString` (minimal).
///
/// Spec: https://tc39.es/ecma262/#sec-array.prototype.tostring
pub fn array_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-array.prototype.tostring
  //
  // 1. Let array be ? ToObject(this value).
  // 2. Let func be ? Get(array, "join").
  // 3. If IsCallable(func) is false, set func to %Object.prototype.toString%.
  // 4. Return ? Call(func, array).
  //
  // Note: we don't currently store a direct handle to the intrinsic `%Object.prototype.toString%`
  // function object, so we call the builtin implementation directly when `join` is not callable.
  let mut scope = scope.reborrow();

  let array = scope.to_object(vm, host, hooks, this)?;
  scope.push_root(Value::Object(array))?;

  let join_key = string_key(&mut scope, "join")?;
  let func = scope.get_with_host_and_hooks(
    vm,
    host,
    hooks,
    array,
    join_key,
    Value::Object(array),
  )?;
  scope.push_root(func)?;

  if scope.heap().is_callable(func)? {
    vm.call_with_host_and_hooks(host, &mut scope, hooks, func, Value::Object(array), &[])
  } else {
    object_prototype_to_string(vm, &mut scope, host, hooks, callee, Value::Object(array), &[])
  }
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

  let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

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

    if !crate::spec_ops::internal_has_property_with_host_and_hooks(
      vm,
      &mut iter_scope,
      host,
      hooks,
      obj,
      from_key,
    )? {
      continue;
    }

    let value = iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, from_key, Value::Object(obj))?;
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
  let mut len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;

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
  let len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;

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
  let len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;

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
  let len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;

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
  let len = to_length_usize(vm, &mut scope, host, hooks, len_value)?;

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

#[derive(Clone, Copy)]
enum ArrayIteratorKind {
  Keys = 0,
  Values = 1,
  Entries = 2,
}

fn create_array_iterator_with_kind(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  this: Value,
  kind: ArrayIteratorKind,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let this_obj = scope.to_object(vm, host, hooks, this)?;

  // Root `this` while allocating/defining properties on the iterator object.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(this_obj))?;

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  scope
    .heap_mut()
    .object_set_prototype(iter, Some(intr.array_iterator_prototype()))?;

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

  let kind_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_KIND_MARKER)?;
  scope.define_property(
    iter,
    kind_key,
    data_desc(Value::Number(kind as u8 as f64), true, false, true),
  )?;
  Ok(Value::Object(iter))
}

/// `Array.prototype.values` / `%Array.prototype%[@@iterator]` (minimal).
///
/// This is primarily needed by higher-level binding layers (e.g. WebIDL iterable snapshot helpers)
/// that want to build a JS `Array` and then obtain an iterator via `arr[Symbol.iterator]()`.
pub fn array_prototype_values(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  create_array_iterator_with_kind(
    vm,
    scope,
    host,
    hooks,
    this,
    ArrayIteratorKind::Values,
  )
}

/// `Array.prototype.keys` (minimal).
pub fn array_prototype_keys(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  create_array_iterator_with_kind(
    vm,
    scope,
    host,
    hooks,
    this,
    ArrayIteratorKind::Keys,
  )
}

/// `Array.prototype.entries` (minimal).
pub fn array_prototype_entries(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  create_array_iterator_with_kind(
    vm,
    scope,
    host,
    hooks,
    this,
    ArrayIteratorKind::Entries,
  )
}

/// `ArrayIterator.prototype.next` (minimal).
pub fn array_iterator_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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
  let array_value = match get_data_property_value(vm, &mut scope, this_obj, &array_key) {
    Ok(Some(v)) => v,
    Ok(None) => return Err(VmError::TypeError("Array iterator missing internal array")),
    Err(VmError::PropertyNotData) => {
      return Err(VmError::TypeError(
        "Array iterator internal array is not a data property",
      ));
    }
    Err(err) => return Err(err),
  };
  let array_obj = match array_value {
    // Per spec, once the iterator is exhausted its internal `[[IteratedObject]]` is set to
    // `undefined`.
    Value::Undefined => {
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
    Value::Object(o) => o,
    _ => return Err(VmError::TypeError("Array iterator internal array is not an object")),
  };
  scope.push_root(Value::Object(array_obj))?;

  let index_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_INDEX_MARKER)?;
  let index_value = match get_data_property_value(vm, &mut scope, this_obj, &index_key) {
    Ok(v) => v.unwrap_or(Value::Number(0.0)),
    Err(VmError::PropertyNotData) => {
      return Err(VmError::TypeError(
        "Array iterator internal index is not a data property",
      ));
    }
    Err(err) => return Err(err),
  };
  let idx = match index_value {
    Value::Number(n) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0usize,
  };

  let kind_key = internal_symbol_key(&mut scope, ARRAY_ITERATOR_KIND_MARKER)?;
  let kind_value = match get_data_property_value(vm, &mut scope, this_obj, &kind_key) {
    Ok(v) => v.unwrap_or(Value::Number(ArrayIteratorKind::Values as u8 as f64)),
    Err(VmError::PropertyNotData) => {
      return Err(VmError::TypeError(
        "Array iterator internal kind is not a data property",
      ));
    }
    Err(err) => return Err(err),
  };
  let kind = match kind_value {
    Value::Number(n) if n.is_finite() && n.fract() == 0.0 => match n as u8 {
      0 => ArrayIteratorKind::Keys,
      1 => ArrayIteratorKind::Values,
      2 => ArrayIteratorKind::Entries,
      _ => return Err(VmError::TypeError("Array iterator internal kind is invalid")),
    },
    _ => return Err(VmError::TypeError("Array iterator internal kind is not a number")),
  };

  // LengthOfArrayLike: `ToLength(Get(obj, "length"))`.
  let length_key = string_key(&mut scope, "length")?;
  let len_value = scope.get_with_host_and_hooks(
    vm,
    host,
    hooks,
    array_obj,
    length_key,
    Value::Object(array_obj),
  )?;
  let len = scope.to_length(vm, host, hooks, len_value)?;
  if idx >= len {
    // End-of-iteration: clear `[[IteratedObject]]` so the underlying array can be collected if this
    // iterator is retained.
    scope.define_property(
      this_obj,
      array_key,
      data_desc(Value::Undefined, true, false, true),
    )?;

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

  let out_value = match kind {
    ArrayIteratorKind::Keys => {
      // Update `[[ArrayIteratorNextIndex]]`.
      let next_idx = idx.saturating_add(1);
      scope.define_property(
        this_obj,
        index_key,
        data_desc(Value::Number(next_idx as f64), true, false, true),
      )?;
      Value::Number(idx as f64)
    }
    ArrayIteratorKind::Values | ArrayIteratorKind::Entries => {
      // `value = Get(iteratedObject, ToString(nextIndex))` (Proxy-aware).
      let idx_s = alloc_string_from_usize(&mut scope, idx)?;
      scope.push_root(Value::String(idx_s))?;
      let key = PropertyKey::from_string(idx_s);
      let value =
        scope.get_with_host_and_hooks(vm, host, hooks, array_obj, key, Value::Object(array_obj))?;
      // Root the retrieved value across subsequent allocations/GC. This matters in particular for
      // accessors that return freshly allocated objects/strings.
      scope.push_root(value)?;

      // Update `[[ArrayIteratorNextIndex]]` *after* `Get`, matching the spec order and ensuring
      // re-entrancy observes the previous index.
      let next_idx = idx.saturating_add(1);
      scope.define_property(
        this_obj,
        index_key,
        data_desc(Value::Number(next_idx as f64), true, false, true),
      )?;

      match kind {
        ArrayIteratorKind::Values => value,
        ArrayIteratorKind::Entries => {
          let entry = scope.alloc_array(0)?;
          scope.push_root(Value::Object(entry))?;
          scope
            .heap_mut()
            .object_set_prototype(entry, Some(intr.array_prototype()))?;
          let key0 = string_key(&mut scope, "0")?;
          scope.create_data_property_or_throw(entry, key0, Value::Number(idx as f64))?;
          let key1 = string_key(&mut scope, "1")?;
          scope.create_data_property_or_throw(entry, key1, value)?;
          Value::Object(entry)
        }
        ArrayIteratorKind::Keys => {
          return Err(VmError::InvariantViolation(
            "ArrayIteratorKind::Keys reached in Values/Entries branch",
          ));
        }
      }
    }
  };

  // Create iterator result object.
  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;
  let value_key = string_key(&mut scope, "value")?;
  let done_key = string_key(&mut scope, "done")?;
  scope.define_property(out, value_key, data_desc(out_value, true, true, true))?;
  scope.define_property(out, done_key, data_desc(Value::Bool(false), true, true, true))?;
  Ok(Value::Object(out))
}

fn require_generator_object(
  scope: &mut Scope<'_>,
  this: Value,
  non_object_message: &'static str,
  incompatible_receiver_message: &'static str,
) -> Result<GcObject, VmError> {
  let Value::Object(this_obj) = this else {
    return Err(VmError::TypeError(non_object_message));
  };

  // Root `this` across the internal symbol allocation and the own-property lookup.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(this_obj))?;

  let marker_key = internal_symbol_key(&mut scope, GENERATOR_STATE_MARKER)?;
  let has_state = match scope
    .heap()
    .object_get_own_data_property_value(this_obj, &marker_key)
  {
    Ok(Some(_)) => true,
    Ok(None) => false,
    Err(VmError::PropertyNotData) => false,
    Err(e) => return Err(e),
  };

  if !has_state {
    return Err(VmError::TypeError(incompatible_receiver_message));
  }

  Ok(this_obj)
}

/// `%GeneratorPrototype%.next` (validation + minimal implementation).
pub fn generator_prototype_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_generator_object(
    scope,
    this,
    "Generator.prototype.next called on non-object",
    "Generator.prototype.next called on incompatible receiver",
  )?;
  crate::exec::generator_prototype_next(vm, scope, host, hooks, callee, this, args)
}

/// `%GeneratorPrototype%.return` (validation + stub).
pub fn generator_prototype_return(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_generator_object(
    scope,
    this,
    "Generator.prototype.return called on non-object",
    "Generator.prototype.return called on incompatible receiver",
  )?;
  Err(VmError::Unimplemented("GeneratorResumeAbrupt"))
}

/// `%GeneratorPrototype%.throw` (validation + stub).
pub fn generator_prototype_throw(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_generator_object(
    scope,
    this,
    "Generator.prototype.throw called on non-object",
    "Generator.prototype.throw called on incompatible receiver",
  )?;
  Err(VmError::Unimplemented("GeneratorResumeAbrupt"))
}

/// `%IteratorPrototype%[@@iterator]` (ECMA-262) (minimal).
///
/// Iterator objects are iterable: calling `iter[Symbol.iterator]()` returns `iter` itself.
pub fn iterator_prototype_symbol_iterator(
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
    Some(Value::Symbol(sym)) => symbol_descriptive_string(scope, sym, || vm.tick())?,
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

  let proto = crate::spec_ops::get_prototype_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intr.string_prototype(),
  )?;
  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

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

  Ok(Value::Object(obj))
}

fn regexp_constructor_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  pattern: Value,
  flags: Value,
  new_target: Value,
  allow_return_existing: bool,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  // `RegExp(pattern, flags)` returns `pattern` when `pattern` is a RegExp object and `flags` is
  // `undefined`.
  if allow_return_existing && matches!(flags, Value::Undefined) {
    if let Value::Object(obj) = pattern {
      if scope.heap().is_regexp_object(obj) {
        // Per spec, only return the input regexp if `pattern.constructor === newTarget`.
        //
        // Note: use full `Get` semantics (may invoke user code), so root the inputs.
        let mut check_scope = scope.reborrow();
        check_scope.push_root(Value::Object(obj))?;
        check_scope.push_root(new_target)?;
        let ctor_key = string_key(&mut check_scope, "constructor")?;
        let ctor_value = check_scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          obj,
          ctor_key,
          Value::Object(obj),
        )?;
        if ctor_value.same_value(new_target, check_scope.heap()) {
          return Ok(Value::Object(obj));
        }
      }
    }
  }

  // Derive the pattern source and flags string.
  let mut scope = scope.reborrow();
  scope.push_root(pattern)?;
  scope.push_root(flags)?;

  let (source_s, flags_s) = if let Value::Object(obj) = pattern {
    if scope.heap().is_regexp_object(obj) {
      let source = scope.heap().regexp_original_source(obj)?;
      let orig_flags = scope.heap().regexp_original_flags(obj)?;
      let flags_s = if matches!(flags, Value::Undefined) {
        orig_flags
      } else {
        scope.to_string(vm, host, hooks, flags)?
      };
      (source, flags_s)
    } else {
      let src = if matches!(pattern, Value::Undefined) {
        scope.alloc_string("")?
      } else {
        scope.to_string(vm, host, hooks, pattern)?
      };
      let fl = if matches!(flags, Value::Undefined) {
        scope.alloc_string("")?
      } else {
        scope.to_string(vm, host, hooks, flags)?
      };
      (src, fl)
    }
  } else {
    let src = if matches!(pattern, Value::Undefined) {
      scope.alloc_string("")?
    } else {
      scope.to_string(vm, host, hooks, pattern)?
    };
    let fl = if matches!(flags, Value::Undefined) {
      scope.alloc_string("")?
    } else {
      scope.to_string(vm, host, hooks, flags)?
    };
    (src, fl)
  };

  scope.push_root(Value::String(source_s))?;
  scope.push_root(Value::String(flags_s))?;

  // Parse flags.
  let parsed_flags = {
    let js = scope.heap().get_string(flags_s)?;
    match RegExpFlags::parse(js.as_code_units()) {
      Ok(f) => f,
      Err(e) => return throw_syntax_error(vm, &mut scope, e.message),
    }
  };

  // Compile pattern.
  let program = {
    let js = scope.heap().get_string(source_s)?;
    match compile_regexp(js.as_code_units(), parsed_flags) {
      Ok(p) => p,
      Err(RegExpCompileError::Syntax(e)) => return throw_syntax_error(vm, &mut scope, e.message),
      Err(RegExpCompileError::OutOfMemory) => return Err(VmError::OutOfMemory),
    }
  };

  // Allocate RegExp instance and set prototype from the constructor.
  let rx = crate::spec_ops::ordinary_create_from_constructor_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    new_target,
    intr.regexp_prototype(),
    &[],
    |scope| scope.alloc_regexp(source_s, flags_s, parsed_flags, program),
  )?;
  Ok(Value::Object(rx))
}

pub fn regexp_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let pattern = args.get(0).copied().unwrap_or(Value::Undefined);
  let flags = args.get(1).copied().unwrap_or(Value::Undefined);
  regexp_constructor_impl(
    vm,
    scope,
    host,
    hooks,
    pattern,
    flags,
    Value::Object(intr.regexp_constructor()),
    true,
  )
}

pub fn regexp_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let pattern = args.get(0).copied().unwrap_or(Value::Undefined);
  let flags = args.get(1).copied().unwrap_or(Value::Undefined);
  regexp_constructor_impl(vm, scope, host, hooks, pattern, flags, new_target, false)
}

/// `RegExp.prototype.exec(string)` (ECMA-262) (partial).
pub fn regexp_prototype_exec(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let rx = require_regexp_object(&mut scope, this)?;
  scope.push_root(Value::Object(rx))?;

  let input_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = scope.to_string(vm, host, hooks, input_val)?;
  scope.push_root(Value::String(input))?;

  let res = regexp_exec_array(vm, &mut scope, host, hooks, rx, input)?;
  Ok(match res {
    None => Value::Null,
    Some(r) => Value::Object(r.array),
  })
}

/// `RegExp.prototype.test(string)` (ECMA-262) (partial).
pub fn regexp_prototype_test(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let rx = require_regexp_object(&mut scope, this)?;
  scope.push_root(Value::Object(rx))?;

  let input_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = scope.to_string(vm, host, hooks, input_val)?;
  scope.push_root(Value::String(input))?;

  let ok = regexp_exec_raw(vm, &mut scope, host, hooks, rx, input)?.is_some();
  Ok(Value::Bool(ok))
}

/// `get RegExp.prototype.source` (ECMA-262) (minimal).
pub fn regexp_prototype_source_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let rx = require_regexp_object(scope, this)?;
  let source = scope.heap().regexp_original_source(rx)?;
  if scope.heap().get_string(source)?.is_empty() {
    let s = scope.alloc_string("(?:)")?;
    return Ok(Value::String(s));
  }
  Ok(Value::String(source))
}

/// `get RegExp.prototype.flags` (ECMA-262) (minimal).
pub fn regexp_prototype_flags_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let rx = require_regexp_object(scope, this)?;
  let flags = scope.heap().regexp_flags(rx)?;
  let s = scope.alloc_string(&flags.to_canonical_string())?;
  Ok(Value::String(s))
}

/// `%RegExp.prototype%[@@match]` (ECMA-262) (partial).
pub fn regexp_prototype_symbol_match(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let rx = require_regexp_object(&mut scope, this)?;
  scope.push_root(Value::Object(rx))?;
  let s_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = scope.to_string(vm, host, hooks, s_val)?;
  scope.push_root(Value::String(s))?;

  let flags = scope.heap().regexp_flags(rx)?;
  if !flags.global {
    let res = regexp_exec_array(vm, &mut scope, host, hooks, rx, s)?;
    return Ok(match res {
      None => Value::Null,
      Some(r) => Value::Object(r.array),
    });
  }

  regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(0.0))?;

  // Collect match ranges (avoid holding GC handles in a Rust Vec across allocations/GC).
  let mut ranges: Vec<(usize, usize)> = Vec::new();
  loop {
    let Some(raw) = regexp_exec_raw(vm, &mut scope, host, hooks, rx, s)? else {
      break;
    };
    ranges
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    let start = raw.index;
    let end = raw.m.end;
    ranges.push((start, end));
    if end == start {
      let li = regexp_get_last_index(vm, &mut scope, host, hooks, rx)?;
      let new_li = {
        let js = scope.heap().get_string(s)?;
        advance_string_index(js.as_code_units(), li, flags.unicode)
      };
      regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(new_li as f64))?;
    }
  }

  if ranges.is_empty() {
    return Ok(Value::Null);
  }

  let s_len = scope.heap().get_string(s)?.len_code_units();
  let array_len = u32::try_from(ranges.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, array_len)?;
  for (i, (from, to)) in ranges.into_iter().enumerate() {
    if i % 64 == 0 {
      vm.tick()?;
    }
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(array))?;
    iter_scope.push_root(Value::String(s))?;
    let part = if from == 0 && to == s_len {
      s
    } else if from == to {
      iter_scope.alloc_string("")?
    } else {
      let units: Vec<u16> = {
        let js = iter_scope.heap().get_string(s)?;
        let slice = &js.as_code_units()[from..to];
        let mut buf: Vec<u16> = Vec::new();
        buf
          .try_reserve_exact(slice.len())
          .map_err(|_| VmError::OutOfMemory)?;
        buf.extend_from_slice(slice);
        buf
      };
      iter_scope.alloc_string_from_u16_vec(units)?
    };
    iter_scope.push_root(Value::String(part))?;
    let key_s = iter_scope.alloc_string(&i.to_string())?;
    iter_scope.push_root(Value::String(key_s))?;
    iter_scope.define_property(
      array,
      PropertyKey::from_string(key_s),
      data_desc(Value::String(part), true, true, true),
    )?;
  }
  Ok(Value::Object(array))
}

/// `%RegExp.prototype%[@@search]` (ECMA-262) (partial).
pub fn regexp_prototype_symbol_search(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let rx = require_regexp_object(&mut scope, this)?;
  scope.push_root(Value::Object(rx))?;
  let s_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = scope.to_string(vm, host, hooks, s_val)?;
  scope.push_root(Value::String(s))?;

  let old_last_index = regexp_get_last_index_value(vm, &mut scope, host, hooks, rx)?;
  scope.push_root(old_last_index)?;

  regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(0.0))?;
  let res = regexp_exec_raw(vm, &mut scope, host, hooks, rx, s)?;
  regexp_set_last_index(vm, &mut scope, host, hooks, rx, old_last_index)?;

  Ok(match res {
    None => Value::Number(-1.0),
    Some(r) => Value::Number(r.index as f64),
  })
}

fn get_substitution(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  input: GcString,
  replace: GcString,
  match_range: (usize, usize),
  captures: &[usize],
  capture_count: usize,
) -> Result<Vec<u16>, VmError> {
  let replace_units = scope.heap().get_string(replace)?.as_code_units();
  let input_units = scope.heap().get_string(input)?.as_code_units();
  let (match_start, match_end) = match_range;

  let mut out: Vec<u16> = Vec::new();
  // Best-effort pre-reserve: replacement length + match length.
  out
    .try_reserve(replace_units.len().saturating_add(match_end.saturating_sub(match_start)))
    .map_err(|_| VmError::OutOfMemory)?;

  let mut i = 0usize;
  while i < replace_units.len() {
    if i % 128 == 0 {
      vm.tick()?;
    }
    let u = replace_units[i];
    if u != (b'$' as u16) || i + 1 >= replace_units.len() {
      vec_try_push(&mut out, u)?;
      i += 1;
      continue;
    }
    let next = replace_units[i + 1];
    match next {
      x if x == (b'$' as u16) => {
        vec_try_push(&mut out, b'$' as u16)?;
        i += 2;
      }
      x if x == (b'&' as u16) => {
        vec_try_extend_from_slice(&mut out, &input_units[match_start..match_end], || vm.tick())?;
        i += 2;
      }
      x if x == (b'`' as u16) => {
        vec_try_extend_from_slice(&mut out, &input_units[..match_start], || vm.tick())?;
        i += 2;
      }
      x if x == (b'\'' as u16) => {
        vec_try_extend_from_slice(&mut out, &input_units[match_end..], || vm.tick())?;
        i += 2;
      }
      x if (b'0' as u16..=b'9' as u16).contains(&x) => {
        let d1 = (x - (b'0' as u16)) as usize;
        let mut n = d1;
        let mut consumed = 2usize;
        if i + 2 < replace_units.len() {
          let x2 = replace_units[i + 2];
          if (b'0' as u16..=b'9' as u16).contains(&x2) {
            let d2 = (x2 - (b'0' as u16)) as usize;
            let two = d1.saturating_mul(10).saturating_add(d2);
            if two > 0 {
              n = two;
              consumed = 3;
            }
          }
        }
        if n == 0 || n >= capture_count {
          // Not a valid capture reference: emit `$` literally.
          vec_try_push(&mut out, b'$' as u16)?;
          i += 1;
          continue;
        }
        let start_slot = n.saturating_mul(2);
        let end_slot = start_slot.saturating_add(1);
        let (cap_start, cap_end) = (
          captures.get(start_slot).copied().unwrap_or(usize::MAX),
          captures.get(end_slot).copied().unwrap_or(usize::MAX),
        );
        if cap_start != usize::MAX && cap_end != usize::MAX && cap_end >= cap_start {
          vec_try_extend_from_slice(&mut out, &input_units[cap_start..cap_end], || vm.tick())?;
        }
        i += consumed;
      }
      _ => {
        // Treat `$` as a literal.
        vec_try_push(&mut out, b'$' as u16)?;
        i += 1;
      }
    }
  }

  Ok(out)
}

/// `%RegExp.prototype%[@@replace]` (ECMA-262) (partial).
pub fn regexp_prototype_symbol_replace(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let rx = require_regexp_object(&mut scope, this)?;
  scope.push_root(Value::Object(rx))?;

  let s_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = scope.to_string(vm, host, hooks, s_val)?;
  scope.push_root(Value::String(input))?;

  let replace_value = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(replace_value)?;

  let flags = scope.heap().regexp_flags(rx)?;
  let global = flags.global;

  if global {
    regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(0.0))?;
  }

  let program = scope.heap().regexp_program(rx)?;
  let capture_count = program.capture_count;

  let mut out_units: Vec<u16> = Vec::new();
  let mut last_pos = 0usize;

  loop {
    let raw = regexp_exec_raw(vm, &mut scope, host, hooks, rx, input)?;
    let Some(raw) = raw else {
      break;
    };
    let start = raw.index;
    let end = raw.m.end;
    if start < last_pos {
      break;
    }

    // Append the prefix since the last match.
    {
      let js = scope.heap().get_string(input)?;
      vec_try_extend_from_slice(&mut out_units, &js.as_code_units()[last_pos..start], || vm.tick())?;
    }

    let replacement_units: Vec<u16> = if scope.heap().is_callable(replace_value)? {
      // Call replacer function.
      let mut args_vec: Vec<Value> = Vec::new();
      args_vec
        .try_reserve_exact(capture_count.saturating_add(2))
        .map_err(|_| VmError::OutOfMemory)?;

      // match + captures as strings/undefined.
      for i in 0..capture_count {
        let start_slot = i.saturating_mul(2);
        let end_slot = start_slot.saturating_add(1);
        let (cap_start, cap_end) = (
          raw.m.captures.get(start_slot).copied().unwrap_or(usize::MAX),
          raw.m.captures.get(end_slot).copied().unwrap_or(usize::MAX),
        );
        if cap_start == usize::MAX || cap_end == usize::MAX || cap_end < cap_start {
          args_vec.push(Value::Undefined);
        } else {
          let units: Vec<u16> = {
            let js = scope.heap().get_string(input)?;
            let slice = &js.as_code_units()[cap_start..cap_end];
            let mut buf: Vec<u16> = Vec::new();
            buf
              .try_reserve_exact(slice.len())
              .map_err(|_| VmError::OutOfMemory)?;
            buf.extend_from_slice(slice);
            buf
          };
          let s = scope.alloc_string_from_u16_vec(units)?;
          scope.push_root(Value::String(s))?;
          args_vec.push(Value::String(s));
        }
      }
      args_vec.push(Value::Number(start as f64));
      args_vec.push(Value::String(input));

      let called = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        replace_value,
        Value::Undefined,
        &args_vec,
      )?;
      scope.push_root(called)?;
      let rep_s = scope.to_string(vm, host, hooks, called)?;
      let units = scope.heap().get_string(rep_s)?.as_code_units();
      let mut buf: Vec<u16> = Vec::new();
      buf
        .try_reserve_exact(units.len())
        .map_err(|_| VmError::OutOfMemory)?;
      buf.extend_from_slice(units);
      buf
    } else {
      let replace_s = scope.to_string(vm, host, hooks, replace_value)?;
      scope.push_root(Value::String(replace_s))?;
      get_substitution(
        vm,
        &mut scope,
        input,
        replace_s,
        (start, end),
        &raw.m.captures,
        capture_count,
      )?
    };

    vec_try_extend_from_slice(&mut out_units, &replacement_units, || vm.tick())?;

    last_pos = end;

    if global && end == start {
      let li = regexp_get_last_index(vm, &mut scope, host, hooks, rx)?;
      let new_li = {
        let js = scope.heap().get_string(input)?;
        advance_string_index(js.as_code_units(), li, flags.unicode)
      };
      regexp_set_last_index(vm, &mut scope, host, hooks, rx, Value::Number(new_li as f64))?;
    }

    if !global {
      break;
    }
  }

  // Append remainder.
  {
    let js = scope.heap().get_string(input)?;
    let len = js.len_code_units();
    if last_pos < len {
      vec_try_extend_from_slice(&mut out_units, &js.as_code_units()[last_pos..], || vm.tick())?;
    }
  }

  let out_s = scope.alloc_string_from_u16_vec(out_units)?;
  Ok(Value::String(out_s))
}

/// `%RegExp.prototype%[@@split]` (ECMA-262) (partial).
pub fn regexp_prototype_symbol_split(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let rx = require_regexp_object(&mut scope, this)?;
  scope.push_root(Value::Object(rx))?;

  let s_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = scope.to_string(vm, host, hooks, s_val)?;
  scope.push_root(Value::String(input))?;

  let limit_val = args.get(1).copied().unwrap_or(Value::Undefined);
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

  let flags = scope.heap().regexp_flags(rx)?;
  let program = scope.heap().regexp_program(rx)?;
  let capture_count = program.capture_count;
  let group_count = capture_count.saturating_sub(1);

  #[derive(Clone, Copy)]
  enum Part {
    Range(usize, usize),
    Undefined,
  }

  let input_units: Vec<u16> = {
    let js = scope.heap().get_string(input)?;
    let units = js.as_code_units();
    let mut buf: Vec<u16> = Vec::new();
    buf
      .try_reserve_exact(units.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend_from_slice(units);
    buf
  };
  let len = input_units.len();
  let limit_usize = limit as usize;

  let mut parts: Vec<Part> = Vec::new();
  let mut p = 0usize;
  let mut q = 0usize;

  while q <= len && parts.len() < limit_usize {
    // Find next match at/after q.
    let mut found: Option<(usize, crate::regexp::RegExpMatch)> = None;
    let mut k = q;
    while k <= len {
      if k % 1024 == 0 {
        vm.tick()?;
      }
      let matched = {
        let mut tick = || vm.tick();
        program.exec_at(&input_units, k, flags, &mut tick, None)?
      };
      if let Some(m) = matched {
        found = Some((k, m));
        break;
      }
      let next = advance_string_index(&input_units, k, flags.unicode);
      if next == k {
        break;
      }
      k = next;
    }

    let Some((match_start, m)) = found else { break };
    let match_end = m.end;

    // Avoid infinite loops on empty matches at the same position.
    if match_start == match_end && match_start == p {
      q = advance_string_index(&input_units, q, flags.unicode);
      continue;
    }

    vec_try_push(&mut parts, Part::Range(p, match_start))?;
    if parts.len() >= limit_usize {
      break;
    }

    for gi in 1..=group_count {
      if parts.len() >= limit_usize {
        break;
      }
      let start_slot = gi.saturating_mul(2);
      let end_slot = start_slot.saturating_add(1);
      let (cs, ce) = (
        m.captures.get(start_slot).copied().unwrap_or(usize::MAX),
        m.captures.get(end_slot).copied().unwrap_or(usize::MAX),
      );
      if cs == usize::MAX || ce == usize::MAX || ce < cs {
        vec_try_push(&mut parts, Part::Undefined)?;
      } else {
        vec_try_push(&mut parts, Part::Range(cs, ce))?;
      }
    }

    p = match_end;
    q = match_end;
  }

  if parts.len() < limit_usize {
    vec_try_push(&mut parts, Part::Range(p, len))?;
  }

  let out_len_u32 = u32::try_from(parts.len()).map_err(|_| VmError::OutOfMemory)?;
  let array = create_array_object(vm, &mut scope, out_len_u32)?;

  for (i, part) in parts.into_iter().enumerate() {
    if i % 128 == 0 {
      vm.tick()?;
    }
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(array))?;
    iter_scope.push_root(Value::String(input))?;
    let value = match part {
      Part::Undefined => Value::Undefined,
      Part::Range(from, to) => {
        if from == 0 && to == len {
          Value::String(input)
        } else if from == to {
          Value::String(iter_scope.alloc_string("")?)
        } else {
          let units = &input_units[from..to];
          let mut buf: Vec<u16> = Vec::new();
          buf
            .try_reserve_exact(units.len())
            .map_err(|_| VmError::OutOfMemory)?;
          buf.extend_from_slice(units);
          let s = iter_scope.alloc_string_from_u16_vec(buf)?;
          iter_scope.push_root(Value::String(s))?;
          Value::String(s)
        }
      }
    };
    let key_s = iter_scope.alloc_string(&i.to_string())?;
    iter_scope.push_root(Value::String(key_s))?;
    iter_scope.define_property(array, PropertyKey::from_string(key_s), data_desc(value, true, true, true))?;
  }

  Ok(Value::Object(array))
}

/// `%RegExp.prototype%[@@matchAll]` (ECMA-262) (partial).
pub fn regexp_prototype_symbol_match_all(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 7 {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] has wrong native slot count",
    ));
  }
  let Value::Object(next_fn) = slots[0] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] next slot is not an object",
    ));
  };
  let Value::Symbol(iterating_sym) = slots[1] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] iteratingRegExp slot is not a symbol",
    ));
  };
  let Value::Symbol(iterated_sym) = slots[2] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] iteratedString slot is not a symbol",
    ));
  };
  let Value::Symbol(global_sym) = slots[3] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] global slot is not a symbol",
    ));
  };
  let Value::Symbol(unicode_sym) = slots[4] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] unicode slot is not a symbol",
    ));
  };
  let Value::Symbol(done_sym) = slots[5] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] done slot is not a symbol",
    ));
  };
  let Value::Object(iterator_fn) = slots[6] else {
    return Err(VmError::InvariantViolation(
      "RegExp.prototype[@@matchAll] iterator slot is not an object",
    ));
  };

  let string_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = scope.to_string(vm, host, hooks, string_val)?;
  scope.push_root(Value::String(s))?;

  // Create matcher:
  // - If `this` is a RegExp object, clone it via `RegExp(this, this.flags)`.
  // - Otherwise, use `RegExp(this, "g")`.
  let matcher: GcObject;
  let (global, unicode) = if let Ok(rx) = require_regexp_object(scope, this) {
    let flags = scope.heap().regexp_flags(rx)?;
    if !flags.global {
      return Err(VmError::TypeError(
        "String.prototype.matchAll called with a non-global RegExp argument",
      ));
    }
    // Clone.
    let flags_s = scope.alloc_string(&flags.to_canonical_string())?;
    scope.push_root(Value::String(flags_s))?;
    let ctor = Value::Object(intr.regexp_constructor());
    let mut ctor_scope = scope.reborrow();
    ctor_scope.push_root(Value::Object(rx))?;
    ctor_scope.push_root(ctor)?;
    ctor_scope.push_root(Value::String(flags_s))?;
    let created = vm.construct_with_host_and_hooks(
      host,
      &mut ctor_scope,
      hooks,
      ctor,
      &[Value::Object(rx), Value::String(flags_s)],
      ctor,
    )?;
    let Value::Object(obj) = created else {
      return Err(VmError::InvariantViolation("RegExp constructor returned non-object"));
    };
    matcher = obj;

    // Preserve lastIndex.
    let last_index = regexp_get_last_index(vm, &mut ctor_scope, host, hooks, rx)?;
    regexp_set_last_index(
      vm,
      &mut ctor_scope,
      host,
      hooks,
      matcher,
      Value::Number(last_index as f64),
    )?;

    (flags.global, flags.unicode)
  } else {
    let ctor = Value::Object(intr.regexp_constructor());
    let g = scope.alloc_string("g")?;
    scope.push_root(Value::String(g))?;
    let mut ctor_scope = scope.reborrow();
    ctor_scope.push_root(this)?;
    ctor_scope.push_root(ctor)?;
    ctor_scope.push_root(Value::String(g))?;
    let created = vm.construct_with_host_and_hooks(host, &mut ctor_scope, hooks, ctor, &[this, Value::String(g)], ctor)?;
    let Value::Object(obj) = created else {
      return Err(VmError::InvariantViolation("RegExp constructor returned non-object"));
    };
    matcher = obj;
    (true, false)
  };

  // Create iterator object with internal slots stored as symbol-keyed properties.
  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  scope
    .heap_mut()
    .object_set_prototype(iter, Some(intr.object_prototype()))?;

  scope.define_property(
    iter,
    PropertyKey::from_symbol(iterating_sym),
    data_desc(Value::Object(matcher), true, false, false),
  )?;
  scope.define_property(
    iter,
    PropertyKey::from_symbol(iterated_sym),
    data_desc(Value::String(s), true, false, false),
  )?;
  scope.define_property(
    iter,
    PropertyKey::from_symbol(global_sym),
    data_desc(Value::Bool(global), true, false, false),
  )?;
  scope.define_property(
    iter,
    PropertyKey::from_symbol(unicode_sym),
    data_desc(Value::Bool(unicode), true, false, false),
  )?;
  scope.define_property(
    iter,
    PropertyKey::from_symbol(done_sym),
    data_desc(Value::Bool(false), true, false, false),
  )?;

  let next_key = string_key(scope, "next")?;
  scope.define_property(iter, next_key, data_desc(Value::Object(next_fn), true, false, true))?;

  // Ensure the iterator object is iterable: `%RegExpStringIteratorPrototype%[@@iterator]` returns
  // `this`, but `vm-js` does not yet model `%IteratorPrototype%` so we define an own property.
  scope.define_property(
    iter,
    PropertyKey::from_symbol(intr.well_known_symbols().iterator),
    data_desc(Value::Object(iterator_fn), true, false, true),
  )?;

  Ok(Value::Object(iter))
}

/// `%RegExpStringIteratorPrototype%.next` (ECMA-262) (partial).
pub fn regexp_string_iterator_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let Value::Object(iter) = this else {
    return Err(VmError::TypeError("RegExp string iterator next called on non-object"));
  };

  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 5 {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator next has wrong native slot count",
    ));
  }
  let Value::Symbol(iterating_sym) = slots[0] else {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator next iteratingRegExp slot is not a symbol",
    ));
  };
  let Value::Symbol(iterated_sym) = slots[1] else {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator next iteratedString slot is not a symbol",
    ));
  };
  let Value::Symbol(global_sym) = slots[2] else {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator next global slot is not a symbol",
    ));
  };
  let Value::Symbol(unicode_sym) = slots[3] else {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator next unicode slot is not a symbol",
    ));
  };
  let Value::Symbol(done_sym) = slots[4] else {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator next done slot is not a symbol",
    ));
  };

  let done_key = PropertyKey::from_symbol(done_sym);
  let done_val = scope
    .heap()
    .object_get_own_data_property_value(iter, &done_key)?
    .unwrap_or(Value::Bool(false));
  let done = matches!(done_val, Value::Bool(true));

  let iterated_key = PropertyKey::from_symbol(iterated_sym);
  let iterated_val = scope.heap().object_get_own_data_property_value(iter, &iterated_key)?;
  let Some(Value::String(iterated)) = iterated_val else {
    // No iterated string => done.
    return Ok(Value::Object(iterator_result_object(scope, intr.object_prototype(), Value::Undefined, true)?));
  };

  if done {
    return Ok(Value::Object(iterator_result_object(scope, intr.object_prototype(), Value::Undefined, true)?));
  }

  let iterating_key = PropertyKey::from_symbol(iterating_sym);
  let Some(Value::Object(matcher)) = scope
    .heap()
    .object_get_own_data_property_value(iter, &iterating_key)?
  else {
    return Err(VmError::InvariantViolation(
      "RegExp string iterator missing iteratingRegExp",
    ));
  };

  let global_key = PropertyKey::from_symbol(global_sym);
  let global = scope
    .heap()
    .object_get_own_data_property_value(iter, &global_key)?
    .unwrap_or(Value::Bool(false))
    .same_value(Value::Bool(true), scope.heap());

  let unicode_key = PropertyKey::from_symbol(unicode_sym);
  let unicode = scope
    .heap()
    .object_get_own_data_property_value(iter, &unicode_key)?
    .unwrap_or(Value::Bool(false))
    .same_value(Value::Bool(true), scope.heap());

  let res = regexp_exec_array(vm, scope, host, hooks, matcher, iterated)?;
  let Some(res) = res else {
    // Mark done and clear the iterated string to allow GC.
    scope.define_property(iter, done_key, data_desc(Value::Bool(true), true, false, false))?;
    scope.define_property(iter, iterated_key, data_desc(Value::Undefined, true, false, false))?;
    return Ok(Value::Object(iterator_result_object(scope, intr.object_prototype(), Value::Undefined, true)?));
  };

  if !global {
    scope.define_property(iter, done_key, data_desc(Value::Bool(true), true, false, false))?;
    scope.define_property(iter, iterated_key, data_desc(Value::Undefined, true, false, false))?;
  } else if res.match_len == 0 {
    let li = regexp_get_last_index(vm, scope, host, hooks, matcher)?;
    let new_li = {
      let js = scope.heap().get_string(iterated)?;
      advance_string_index(js.as_code_units(), li, unicode)
    };
    regexp_set_last_index(vm, scope, host, hooks, matcher, Value::Number(new_li as f64))?;
  }

  Ok(Value::Object(iterator_result_object(
    scope,
    intr.object_prototype(),
    Value::Object(res.array),
    false,
  )?))
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
  //
  // Pre-check heap limits so an attacker cannot force large non-GC-tracked allocations.
  scope.ensure_can_alloc_string_units(args.len())?;
  let mut units: Vec<u16> = Vec::new();
  units
    .try_reserve_exact(args.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, &arg) in args.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
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

/// `String.fromCodePoint(...codePoints)` (ECMA-262).
pub fn string_from_code_point(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-string.fromcodepoint
  //
  // Validate each argument as a code point in [0, 0x10FFFF], then UTF-16 encode.
  let mut out: Vec<u16> = Vec::new();

  for (i, &arg) in args.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let next = scope.to_number(vm, host, hooks, arg)?;
    // `nextCP` must be an *integral* number and within Unicode scalar range.
    //
    // `String.fromCodePoint(undefined)` must throw because `ToNumber(undefined) === NaN`.
    if !next.is_finite() || next.trunc() != next || next < 0.0 || next > 0x10FFFF as f64 {
      let intr = require_intrinsics(vm)?;
      let err = crate::new_range_error(scope, intr, "Invalid code point")?;
      return Err(VmError::Throw(err));
    }

    let cp = next as u32;
    if cp <= 0xFFFF {
      let new_len = out.len().checked_add(1).ok_or(VmError::OutOfMemory)?;
      scope.ensure_can_alloc_string_units(new_len)?;
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      out.push(cp as u16);
    } else {
      let cp = cp - 0x10000;
      let high = 0xD800 + ((cp >> 10) as u16);
      let low = 0xDC00 + ((cp & 0x3FF) as u16);
      let new_len = out.len().checked_add(2).ok_or(VmError::OutOfMemory)?;
      scope.ensure_can_alloc_string_units(new_len)?;
      out.try_reserve(2).map_err(|_| VmError::OutOfMemory)?;
      out.push(high);
      out.push(low);
    }
  }

  let s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(s))
}

/// `String.raw(callSite, ...substitutions)` (ECMA-262).
pub fn string_raw(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-string.raw
  let mut scope = scope.reborrow();

  let call_site = args.get(0).copied().unwrap_or(Value::Undefined);
  let template = scope.to_object(vm, host, hooks, call_site)?;
  scope.push_root(Value::Object(template))?;

  let raw_key_s = scope.alloc_string("raw")?;
  scope.push_root(Value::String(raw_key_s))?;
  let raw_key = PropertyKey::from_string(raw_key_s);
  let raw_val =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, template, raw_key, Value::Object(template))?;
  scope.push_root(raw_val)?;
  let raw = scope.to_object(vm, host, hooks, raw_val)?;
  scope.push_root(Value::Object(raw))?;

  let length_key_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_key_s))?;
  let length_key = PropertyKey::from_string(length_key_s);
  let length_val = scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    raw,
    length_key,
    Value::Object(raw),
  )?;
  scope.push_root(length_val)?;
  let literal_segments = scope.to_length(vm, host, hooks, length_val)?;

  if literal_segments == 0 {
    return Ok(Value::String(scope.alloc_string("")?));
  }

  let substitutions = args.get(1..).unwrap_or(&[]);
  let mut out: Vec<u16> = Vec::new();

  for i in 0..literal_segments {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    // Get raw[i] and append it.
    {
      let mut iter_scope = scope.reborrow();
      iter_scope.push_root(Value::Object(raw))?;

      let idx_s = iter_scope.alloc_string(&i.to_string())?;
      iter_scope.push_root(Value::String(idx_s))?;
      let idx_key = PropertyKey::from_string(idx_s);
      let next_seg = iter_scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        raw,
        idx_key,
        Value::Object(raw),
      )?;
      iter_scope.push_root(next_seg)?;
      let next_seg_s = iter_scope.to_string(vm, host, hooks, next_seg)?;
      iter_scope.push_root(Value::String(next_seg_s))?;
      let next_len = {
        let js = iter_scope.heap().get_string(next_seg_s)?;
        js.len_code_units()
      };
      let new_len = out
        .len()
        .checked_add(next_len)
        .ok_or(VmError::OutOfMemory)?;
      iter_scope.ensure_can_alloc_string_units(new_len)?;
      let units = { iter_scope.heap().get_string(next_seg_s)?.as_code_units() };
      vec_try_extend_from_slice_u16_with_ticks(vm, &mut out, units)?;

      // If this is not the last literal segment, append the substitution (if any).
      if i + 1 != literal_segments {
        if let Some(sub) = substitutions.get(i) {
          let sub_s = iter_scope.to_string(vm, host, hooks, *sub)?;
          iter_scope.push_root(Value::String(sub_s))?;
          let sub_len = { iter_scope.heap().get_string(sub_s)?.len_code_units() };
          let new_len = out
            .len()
            .checked_add(sub_len)
            .ok_or(VmError::OutOfMemory)?;
          iter_scope.ensure_can_alloc_string_units(new_len)?;
          let sub_units = { iter_scope.heap().get_string(sub_s)?.as_code_units() };
          vec_try_extend_from_slice_u16_with_ticks(vm, &mut out, sub_units)?;
        }
      }
    }
  }

  let out_s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out_s))
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

/// `String.prototype.codePointAt(pos)` (ECMA-262).
pub fn string_prototype_code_point_at(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-string.prototype.codepointat
  let mut scope = scope.reborrow();
  if matches!(this, Value::Undefined | Value::Null) {
    return Err(VmError::TypeError(
      "Cannot convert undefined or null to object",
    ));
  }

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let pos_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let pos = scope.to_integer_or_infinity(vm, host, hooks, pos_val)?;
  if !pos.is_finite() {
    return Ok(Value::Undefined);
  }
  if pos < 0.0 {
    return Ok(Value::Undefined);
  }
  if pos > (usize::MAX as f64) {
    return Ok(Value::Undefined);
  }
  let idx = pos as usize;

  let cp = {
    let js = scope.heap().get_string(s)?;
    let units = js.as_code_units();
    if idx >= units.len() {
      return Ok(Value::Undefined);
    }
    let first = units[idx];
    if (0xD800..=0xDBFF).contains(&first) && idx + 1 < units.len() {
      let second = units[idx + 1];
      if (0xDC00..=0xDFFF).contains(&second) {
        let high = (first as u32) - 0xD800;
        let low = (second as u32) - 0xDC00;
        0x10000 + ((high << 10) | low)
      } else {
        first as u32
      }
    } else {
      first as u32
    }
  };

  Ok(Value::Number(cp as f64))
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

/// `String.prototype.at(index)` (ECMA-262).
pub fn string_prototype_at(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-string.prototype.at
  let mut scope = scope.reborrow();
  if matches!(this, Value::Undefined | Value::Null) {
    return Err(VmError::TypeError(
      "Cannot convert undefined or null to object",
    ));
  }

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let pos_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let relative = scope.to_integer_or_infinity(vm, host, hooks, pos_val)?;
  if !relative.is_finite() {
    return Ok(Value::Undefined);
  }

  let len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  let k = if relative >= 0.0 {
    relative
  } else {
    (len as f64) + relative
  };
  if k < 0.0 || k >= len as f64 || k > (usize::MAX as f64) {
    return Ok(Value::Undefined);
  }
  let idx = k as usize;

  let cp = {
    let js = scope.heap().get_string(s)?;
    let units = js.as_code_units();
    let first = units[idx];
    if (0xD800..=0xDBFF).contains(&first) && idx + 1 < units.len() {
      let second = units[idx + 1];
      if (0xDC00..=0xDFFF).contains(&second) {
        let high = (first as u32) - 0xD800;
        let low = (second as u32) - 0xDC00;
        0x10000 + ((high << 10) | low)
      } else {
        first as u32
      }
    } else {
      first as u32
    }
  };

  let out_units: Vec<u16> = if cp <= 0xFFFF {
    vec![cp as u16]
  } else {
    let cp = cp - 0x10000;
    let high = 0xD800 + ((cp >> 10) as u16);
    let low = 0xDC00 + ((cp & 0x3FF) as u16);
    vec![high, low]
  };
  let out = scope.alloc_string_from_u16_vec(out_units)?;
  Ok(Value::String(out))
}

fn string_pad_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  this: Value,
  args: &[Value],
  at_start: bool,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  if matches!(this, Value::Undefined | Value::Null) {
    return Err(VmError::TypeError(
      "Cannot convert undefined or null to object",
    ));
  }

  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let max_len_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let max_len = scope.to_length(vm, host, hooks, max_len_val)?;

  let s_len = {
    let js = scope.heap().get_string(s)?;
    js.len_code_units()
  };
  if max_len <= s_len {
    return Ok(Value::String(s));
  }

  let fill_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let fill = if matches!(fill_val, Value::Undefined) {
    scope.alloc_string(" ")?
  } else {
    scope.to_string(vm, host, hooks, fill_val)?
  };
  scope.push_root(Value::String(fill))?;

  let fill_len = {
    let js = scope.heap().get_string(fill)?;
    js.len_code_units()
  };
  if fill_len == 0 {
    return Ok(Value::String(s));
  }

  let fill_needed = max_len.saturating_sub(s_len);

  // Ensure the resulting string fits within heap limits *before* allocating the backing vector.
  // This prevents attacker-controlled `maxLength` from forcing untracked (non-GC-heap) allocations.
  scope.ensure_can_alloc_string_units(max_len)?;

  let mut out: Vec<u16> = Vec::new();
  out.try_reserve_exact(max_len)
    .map_err(|_| VmError::OutOfMemory)?;

  {
    let s_js = scope.heap().get_string(s)?;
    let s_units = s_js.as_code_units();
    let fill_js = scope.heap().get_string(fill)?;
    let fill_units = fill_js.as_code_units();

    if at_start {
      let mut produced = 0usize;
      while produced < fill_needed {
        if produced % 1024 == 0 {
          vm.tick()?;
        }
        let take = (fill_needed - produced).min(fill_units.len());
        vec_try_extend_from_slice_u16_with_ticks(vm, &mut out, &fill_units[..take])?;
        produced += take;
      }
      vec_try_extend_from_slice_u16_with_ticks(vm, &mut out, s_units)?;
    } else {
      vec_try_extend_from_slice_u16_with_ticks(vm, &mut out, s_units)?;
      let mut produced = 0usize;
      while produced < fill_needed {
        if produced % 1024 == 0 {
          vm.tick()?;
        }
        let take = (fill_needed - produced).min(fill_units.len());
        vec_try_extend_from_slice_u16_with_ticks(vm, &mut out, &fill_units[..take])?;
        produced += take;
      }
    }
  }

  let out_s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out_s))
}

/// `String.prototype.padStart(maxLength, fillString)` (ECMA-262).
pub fn string_prototype_pad_start(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-string.prototype.padstart
  string_pad_impl(vm, scope, host, hooks, this, args, true)
}

/// `String.prototype.padEnd(maxLength, fillString)` (ECMA-262).
pub fn string_prototype_pad_end(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-string.prototype.padend
  string_pad_impl(vm, scope, host, hooks, this, args, false)
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
    let slice = &js.as_code_units()[start..end];
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    let slice = &js.as_code_units()[start..end];
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    let slice = &js.as_code_units()[start..len];
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    let slice = &js.as_code_units()[0..end];
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    let slice = &js.as_code_units()[from..to];
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    let slice = &js.as_code_units()[start..end];
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
  };
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

/// `String.prototype.match(regexp)` (ECMA-262) (partial).
pub fn string_prototype_match(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let regexp = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(regexp, Value::Undefined | Value::Null) {
    let method = crate::spec_ops::get_method_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      regexp,
      PropertyKey::from_symbol(intr.well_known_symbols().match_),
    )?;
    if let Some(method) = method {
      return vm.call_with_host_and_hooks(host, &mut scope, hooks, method, regexp, &[Value::String(s)]);
    }
  }

  // Fallback: `RegExpCreate(regexp, undefined)` then call `@@match`.
  let ctor = Value::Object(intr.regexp_constructor());
  let created = vm.construct_with_host_and_hooks(host, &mut scope, hooks, ctor, &[regexp], ctor)?;
  let Value::Object(rx) = created else {
    return Err(VmError::InvariantViolation("RegExp constructor returned non-object"));
  };
  scope.push_root(Value::Object(rx))?;

  let method = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(rx),
    PropertyKey::from_symbol(intr.well_known_symbols().match_),
  )?
  .ok_or(VmError::InvariantViolation("RegExp @@match missing"))?;
  vm.call_with_host_and_hooks(host, &mut scope, hooks, method, Value::Object(rx), &[Value::String(s)])
}

/// `String.prototype.matchAll(regexp)` (ECMA-262) (partial).
pub fn string_prototype_match_all(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let regexp = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(regexp, Value::Undefined | Value::Null) {
    let method = crate::spec_ops::get_method_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      regexp,
      PropertyKey::from_symbol(intr.well_known_symbols().match_all),
    )?;
    if let Some(method) = method {
      return vm.call_with_host_and_hooks(host, &mut scope, hooks, method, regexp, &[Value::String(s)]);
    }
  }

  // Fallback: `RegExpCreate(regexp, "g")` then call `@@matchAll`.
  let ctor = Value::Object(intr.regexp_constructor());
  let g = scope.alloc_string("g")?;
  scope.push_root(Value::String(g))?;
  let created = vm.construct_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    ctor,
    &[regexp, Value::String(g)],
    ctor,
  )?;
  let Value::Object(rx) = created else {
    return Err(VmError::InvariantViolation("RegExp constructor returned non-object"));
  };
  scope.push_root(Value::Object(rx))?;

  let method = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(rx),
    PropertyKey::from_symbol(intr.well_known_symbols().match_all),
  )?
  .ok_or(VmError::InvariantViolation("RegExp @@matchAll missing"))?;
  vm.call_with_host_and_hooks(host, &mut scope, hooks, method, Value::Object(rx), &[Value::String(s)])
}

/// `String.prototype.search(regexp)` (ECMA-262) (partial).
pub fn string_prototype_search(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let regexp = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(regexp, Value::Undefined | Value::Null) {
    let method = crate::spec_ops::get_method_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      regexp,
      PropertyKey::from_symbol(intr.well_known_symbols().search),
    )?;
    if let Some(method) = method {
      return vm.call_with_host_and_hooks(host, &mut scope, hooks, method, regexp, &[Value::String(s)]);
    }
  }

  // Fallback: `RegExpCreate(regexp, undefined)` then call `@@search`.
  let ctor = Value::Object(intr.regexp_constructor());
  let created = vm.construct_with_host_and_hooks(host, &mut scope, hooks, ctor, &[regexp], ctor)?;
  let Value::Object(rx) = created else {
    return Err(VmError::InvariantViolation("RegExp constructor returned non-object"));
  };
  scope.push_root(Value::Object(rx))?;

  let method = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(rx),
    PropertyKey::from_symbol(intr.well_known_symbols().search),
  )?
  .ok_or(VmError::InvariantViolation("RegExp @@search missing"))?;
  vm.call_with_host_and_hooks(host, &mut scope, hooks, method, Value::Object(rx), &[Value::String(s)])
}

/// `String.prototype.replace(searchValue, replaceValue)` (ECMA-262) (partial).
pub fn string_prototype_replace(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let search_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let replace_value = args.get(1).copied().unwrap_or(Value::Undefined);

  if !matches!(search_value, Value::Undefined | Value::Null) {
    let method = crate::spec_ops::get_method_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      search_value,
      PropertyKey::from_symbol(intr.well_known_symbols().replace),
    )?;
    if let Some(method) = method {
      return vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        method,
        search_value,
        &[Value::String(s), replace_value],
      );
    }
  }

  // String searchValue fallback.
  let search_s = scope.to_string(vm, host, hooks, search_value)?;
  scope.push_root(Value::String(search_s))?;

  let (pos, search_len) = {
    let hay = scope.heap().get_string(s)?.as_code_units();
    let needle = scope.heap().get_string(search_s)?.as_code_units();
    if needle.is_empty() {
      (0usize, 0usize)
    } else if needle.len() > hay.len() {
      return Ok(Value::String(s));
    } else {
      let last = hay.len() - needle.len();
      let mut found: Option<usize> = None;
      for i in 0..=last {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        if &hay[i..i + needle.len()] == needle {
          found = Some(i);
          break;
        }
      }
      let Some(pos) = found else {
        return Ok(Value::String(s));
      };
      (pos, needle.len())
    }
  };

  let replacement_units: Vec<u16> = if scope.heap().is_callable(replace_value)? {
    // Call replacer function with (matched, position, string).
    let matched = if search_len == 0 { scope.alloc_string("")? } else { search_s };
    scope.push_root(Value::String(matched))?;
    let called = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      replace_value,
      Value::Undefined,
      &[Value::String(matched), Value::Number(pos as f64), Value::String(s)],
    )?;
    scope.push_root(called)?;
    let rep_s = scope.to_string(vm, host, hooks, called)?;
    let units = scope.heap().get_string(rep_s)?.as_code_units();
    let mut buf: Vec<u16> = Vec::new();
    buf
      .try_reserve_exact(units.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend_from_slice(units);
    buf
  } else {
    let replace_s = scope.to_string(vm, host, hooks, replace_value)?;
    scope.push_root(Value::String(replace_s))?;
    let captures = [pos, pos.saturating_add(search_len)];
    get_substitution(
      vm,
      &mut scope,
      s,
      replace_s,
      (pos, pos.saturating_add(search_len)),
      &captures,
      1,
    )?
  };

  let out_units: Vec<u16> = {
    let hay = scope.heap().get_string(s)?.as_code_units();
    let mut out: Vec<u16> = Vec::new();
    out
      .try_reserve(hay.len().saturating_add(replacement_units.len()).saturating_add(8))
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(&hay[..pos]);
    out.extend_from_slice(&replacement_units);
    out.extend_from_slice(&hay[pos + search_len..]);
    out
  };
  let out_s = scope.alloc_string_from_u16_vec(out_units)?;
  Ok(Value::String(out_s))
}

/// `String.prototype.replaceAll(searchValue, replaceValue)` (ECMA-262) (partial).
pub fn string_prototype_replace_all(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let mut scope = scope.reborrow();
  let s = scope.to_string(vm, host, hooks, this)?;
  scope.push_root(Value::String(s))?;

  let search_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let replace_value = args.get(1).copied().unwrap_or(Value::Undefined);

  if let Value::Object(obj) = search_value {
    if scope.heap().is_regexp_object(obj) {
      let flags = scope.heap().regexp_flags(obj)?;
      if !flags.global {
        return Err(VmError::TypeError(
          "String.prototype.replaceAll called with a non-global RegExp argument",
        ));
      }
    }
  }

  if !matches!(search_value, Value::Undefined | Value::Null) {
    let method = crate::spec_ops::get_method_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      search_value,
      PropertyKey::from_symbol(intr.well_known_symbols().replace),
    )?;
    if let Some(method) = method {
      return vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        method,
        search_value,
        &[Value::String(s), replace_value],
      );
    }
  }

  let search_s = scope.to_string(vm, host, hooks, search_value)?;
  scope.push_root(Value::String(search_s))?;

  let replace_is_callable = scope.heap().is_callable(replace_value)?;
  let replace_s = if replace_is_callable {
    None
  } else {
    let s = scope.to_string(vm, host, hooks, replace_value)?;
    scope.push_root(Value::String(s))?;
    Some(s)
  };

  // Collect match positions first, then build the output.
  let positions: Vec<usize> = {
    let hay = scope.heap().get_string(s)?.as_code_units();
    let needle = scope.heap().get_string(search_s)?.as_code_units();
    if needle.is_empty() {
      let mut positions: Vec<usize> = Vec::new();
      positions
        .try_reserve_exact(hay.len().saturating_add(1))
        .map_err(|_| VmError::OutOfMemory)?;
      for i in 0..=hay.len() {
        positions.push(i);
      }
      positions
    } else if needle.len() > hay.len() {
      Vec::new()
    } else {
      let mut positions: Vec<usize> = Vec::new();
      let mut start = 0usize;
      let last = hay.len() - needle.len();
      while start <= last {
        if start % 1024 == 0 {
          vm.tick()?;
        }
        let mut found: Option<usize> = None;
        for i in start..=last {
          if i % 1024 == 0 {
            vm.tick()?;
          }
          if &hay[i..i + needle.len()] == needle {
            found = Some(i);
            break;
          }
        }
        let Some(pos) = found else {
          break;
        };
        if positions.len() == positions.capacity() {
          positions
            .try_reserve(1)
            .map_err(|_| VmError::OutOfMemory)?;
        }
        positions.push(pos);
        start = pos + needle.len();
      }
      positions
    }
  };

  if positions.is_empty() {
    return Ok(Value::String(s));
  }

  // Build output.
  let mut out: Vec<u16> = Vec::new();
  let mut last_end = 0usize;

  let hay_units: Vec<u16> = {
    let units = scope.heap().get_string(s)?.as_code_units();
    let mut buf: Vec<u16> = Vec::new();
    buf
      .try_reserve_exact(units.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend_from_slice(units);
    buf
  };
  let needle_len = scope.heap().get_string(search_s)?.len_code_units();

  for (mi, &pos) in positions.iter().enumerate() {
    if mi % 256 == 0 {
      vm.tick()?;
    }
    // Prefix.
    vec_try_extend_from_slice(&mut out, &hay_units[last_end..pos], || vm.tick())?;

    let match_end = pos.saturating_add(needle_len);

    // Replacement.
    if replace_is_callable {
      let matched = if needle_len == 0 { scope.alloc_string("")? } else { search_s };
      scope.push_root(Value::String(matched))?;
      let called = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        replace_value,
        Value::Undefined,
        &[Value::String(matched), Value::Number(pos as f64), Value::String(s)],
      )?;
      scope.push_root(called)?;
      let rep_s = scope.to_string(vm, host, hooks, called)?;
      let rep_units: Vec<u16> = {
        let units = scope.heap().get_string(rep_s)?.as_code_units();
        let mut buf: Vec<u16> = Vec::new();
        buf
          .try_reserve_exact(units.len())
          .map_err(|_| VmError::OutOfMemory)?;
        buf.extend_from_slice(units);
        buf
      };
      vec_try_extend_from_slice(&mut out, &rep_units, || vm.tick())?;
    } else {
      let replace_s = replace_s.expect("replace string should be computed");
      let captures = [pos, match_end];
      let rep_units = get_substitution(vm, &mut scope, s, replace_s, (pos, match_end), &captures, 1)?;
      vec_try_extend_from_slice(&mut out, &rep_units, || vm.tick())?;
    }

    last_end = match_end;

    // Empty needle: advance by one code unit to avoid infinite loops (replacement happens between
    // code units).
    if needle_len == 0 && last_end < hay_units.len() {
      vec_try_push(&mut out, hay_units[last_end])?;
      last_end += 1;
    }
  }

  // Remainder.
  vec_try_extend_from_slice(&mut out, &hay_units[last_end..], || vm.tick())?;

  let out_s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out_s))
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
    idx_scope.push_roots(&[Value::Object(array), Value::String(s)])?;
    let key = string_key(&mut idx_scope, "0")?;
    idx_scope.define_property(array, key, data_desc(Value::String(s), true, true, true))?;
    return Ok(Value::Object(array));
  }

  // `separator` has `@@split` => delegate.
  if !matches!(separator, Value::Null | Value::Undefined) {
    let intr = require_intrinsics(vm)?;
    if let Some(method) = crate::spec_ops::get_method_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      separator,
      PropertyKey::from_symbol(intr.well_known_symbols().split),
    )? {
      return vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        method,
        separator,
        &[Value::String(s), Value::Number(limit as f64)],
      );
    }
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
        let slice = &js.as_code_units()[from..to];
        let mut units: Vec<u16> = Vec::new();
        units
          .try_reserve_exact(slice.len())
          .map_err(|_| VmError::OutOfMemory)?;
        vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
        units
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
    let slice = js.as_code_units();
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    vec_try_extend_from_slice(&mut out, &units, || vm.tick())?;
  }

  let out = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(out))
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
/// The iterator object's `next` method is inherited from `%StringIteratorPrototype%`, and the
/// shared native builtin captures the internal slot symbol keys via its own native slots.
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
  if slots.len() != 2 {
    return Err(VmError::InvariantViolation(
      "String.prototype[Symbol.iterator] has wrong native slot count",
    ));
  }
  let Value::Symbol(iterated_sym) = slots[0] else {
    return Err(VmError::InvariantViolation(
      "String.prototype[Symbol.iterator] iteratedString slot is not a symbol",
    ));
  };
  let Value::Symbol(next_index_sym) = slots[1] else {
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
    .object_set_prototype(iter, Some(intr.string_iterator_prototype()))?;

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

  let proto = crate::spec_ops::get_prototype_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intr.number_prototype(),
  )?;
  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

  // Store the primitive value on an internal symbol so `Number.prototype.valueOf` can recover it.
  let marker = scope.alloc_string("vm-js.internal.NumberData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  scope.define_property(
    obj,
    marker_key,
    data_desc(Value::Number(prim), true, false, false),
  )?;

  Ok(Value::Object(obj))
}

fn this_number_value(scope: &mut Scope<'_>, this: Value, method: &'static str) -> Result<f64, VmError> {
  match this {
    Value::Number(n) => Ok(n),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.NumberData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::Number(n)) => Ok(n),
        _ => Err(VmError::TypeError(method)),
      }
    }
    _ => Err(VmError::TypeError(method)),
  }
}

fn this_boolean_value(scope: &mut Scope<'_>, this: Value, method: &'static str) -> Result<bool, VmError> {
  match this {
    Value::Bool(b) => Ok(b),
    Value::Object(obj) => {
      let marker = scope.alloc_string("vm-js.internal.BooleanData")?;
      let marker_sym = scope.heap_mut().symbol_for(marker)?;
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::Bool(b)) => Ok(b),
        _ => Err(VmError::TypeError(method)),
      }
    }
    _ => Err(VmError::TypeError(method)),
  }
}

#[derive(Clone, Debug)]
struct BigUint {
  // Little-endian base-2^32 limbs.
  limbs: Vec<u32>,
}

impl BigUint {
  fn zero() -> Self {
    Self { limbs: Vec::new() }
  }

  fn from_u64(n: u64) -> Self {
    if n == 0 {
      return Self::zero();
    }
    let lo = n as u32;
    let hi = (n >> 32) as u32;
    if hi == 0 {
      Self { limbs: vec![lo] }
    } else {
      Self { limbs: vec![lo, hi] }
    }
  }

  fn is_zero(&self) -> bool {
    self.limbs.is_empty()
  }

  fn normalize(&mut self) {
    while self.limbs.last().is_some_and(|&x| x == 0) {
      self.limbs.pop();
    }
  }

  fn bit_len(&self) -> u32 {
    let Some(&last) = self.limbs.last() else {
      return 0;
    };
    let hi = 32 - last.leading_zeros();
    (self.limbs.len() as u32 - 1) * 32 + hi
  }

  fn shl_bits(&mut self, bits: u32) -> Result<(), VmError> {
    if self.is_zero() || bits == 0 {
      return Ok(());
    }

    let word_shift = (bits / 32) as usize;
    let bit_shift = bits % 32;

    let mut out: Vec<u32> = Vec::new();
    out
      .try_reserve_exact(self.limbs.len().saturating_add(word_shift).saturating_add(1))
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend(std::iter::repeat(0).take(word_shift));
    out.extend_from_slice(&self.limbs);

    if bit_shift != 0 {
      let mut carry: u64 = 0;
      for limb in out.iter_mut() {
        let v = ((*limb as u64) << bit_shift) | carry;
        *limb = v as u32;
        carry = v >> 32;
      }
      if carry != 0 {
        out.push(carry as u32);
      }
    }

    self.limbs = out;
    self.normalize();
    Ok(())
  }

  fn mul_small(&mut self, mul: u32) -> Result<(), VmError> {
    if self.is_zero() || mul == 1 {
      return Ok(());
    }
    if mul == 0 {
      self.limbs.clear();
      return Ok(());
    }

    let mut carry: u64 = 0;
    for limb in self.limbs.iter_mut() {
      let v = (*limb as u64) * (mul as u64) + carry;
      *limb = v as u32;
      carry = v >> 32;
    }
    if carry != 0 {
      self.limbs.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
      self.limbs.push(carry as u32);
    }
    Ok(())
  }

  fn div_rem_small(&mut self, div: u32) -> Result<u32, VmError> {
    debug_assert!(div != 0);
    if self.is_zero() {
      return Ok(0);
    }

    let mut rem: u64 = 0;
    for limb in self.limbs.iter_mut().rev() {
      let cur = (rem << 32) | (*limb as u64);
      let q = cur / (div as u64);
      rem = cur % (div as u64);
      *limb = q as u32;
    }
    self.normalize();
    Ok(rem as u32)
  }

  fn shr_to_u32(&self, shift: u32) -> u32 {
    if self.is_zero() {
      return 0;
    }
    let word_shift = (shift / 32) as usize;
    let bit_shift = shift % 32;
    if word_shift >= self.limbs.len() {
      return 0;
    }
    let mut v = (self.limbs[word_shift] as u64) >> bit_shift;
    if bit_shift != 0 && word_shift + 1 < self.limbs.len() {
      v |= (self.limbs[word_shift + 1] as u64) << (32 - bit_shift);
    }
    v as u32
  }

  fn truncate_bits(&mut self, bits: u32) {
    if bits == 0 {
      self.limbs.clear();
      return;
    }
    let limbs_len = ((bits + 31) / 32) as usize;
    if self.limbs.len() > limbs_len {
      self.limbs.truncate(limbs_len);
    }
    let rem_bits = bits % 32;
    if rem_bits != 0 {
      if let Some(last) = self.limbs.last_mut() {
        let mask = (1u32 << rem_bits) - 1;
        *last &= mask;
      }
    }
    self.normalize();
  }
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
  Ok(Value::Number(this_number_value(
    scope,
    this,
    "Number.prototype.valueOf called on incompatible receiver",
  )?))
}

fn digit_to_ascii(digit: u32) -> u8 {
  debug_assert!(digit < 36);
  if digit < 10 {
    b'0' + (digit as u8)
  } else {
    b'a' + (digit as u8 - 10)
  }
}

fn number_to_string_radix(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  x: f64,
  radix: u32,
) -> Result<crate::GcString, VmError> {
  debug_assert!(radix >= 2 && radix <= 36);
  debug_assert!(x.is_finite());
  debug_assert!(x != 0.0);

  let x = if x == 0.0 { 0.0 } else { x };
  let negative = x < 0.0;
  let abs = if negative { -x } else { x };

  let bits = abs.to_bits();
  let exp_bits = ((bits >> 52) & 0x7ff) as i32;
  let frac_bits = bits & ((1u64 << 52) - 1);

  // Decompose `abs` into `mantissa * 2^shift` where `mantissa` is an integer and `shift` can be
  // negative. This is an exact representation of the IEEE-754 binary64 value.
  let (mantissa, shift): (u64, i32) = if exp_bits == 0 {
    // subnormal
    (frac_bits, -1074)
  } else {
    let e = exp_bits - 1023;
    ((1u64 << 52) | frac_bits, e - 52)
  };

  // Integer part.
  let mut int = if shift >= 0 {
    let mut v = BigUint::from_u64(mantissa);
    v.shl_bits(shift as u32)?;
    v
  } else {
    let k = (-shift) as u32;
    if k >= 64 {
      BigUint::zero()
    } else {
      BigUint::from_u64(mantissa >> k)
    }
  };

  // Fractional remainder: `rem / 2^denom_bits`.
  let (mut rem, denom_bits): (BigUint, u32) = if shift >= 0 {
    (BigUint::zero(), 0)
  } else {
    let k = (-shift) as u32;
    let r = if k >= 64 {
      mantissa
    } else {
      mantissa & ((1u64 << k) - 1)
    };
    (BigUint::from_u64(r), k)
  };

  // Convert integer part to digits by repeated division.
  let int_bit_len = int.bit_len().max(1);
  let max_int_digits = ((int_bit_len + 1) as usize).min(2048);
  let mut int_digits_rev: Vec<u8> = Vec::new();
  int_digits_rev
    .try_reserve_exact(max_int_digits)
    .map_err(|_| VmError::OutOfMemory)?;

  if int.is_zero() {
    int_digits_rev.push(b'0');
  } else {
    let mut steps = 0usize;
    while !int.is_zero() {
      if steps % 1024 == 0 {
        vm.tick()?;
      }
      let d = int.div_rem_small(radix)? as u32;
      int_digits_rev.push(digit_to_ascii(d));
      steps = steps.saturating_add(1);
      if steps >= 4096 {
        // Hard cap: string outputs should never be unbounded, even for hostile inputs.
        break;
      }
    }
  }

  // Reverse integer digits.
  let mut out: Vec<u16> = Vec::new();

  // Fractional digits will terminate for even radices; for odd radices, cap to keep runtime bounded.
  let max_frac_digits: usize = if denom_bits == 0 {
    0
  } else if radix % 2 == 0 {
    // Each digit in an even radix consumes at least one factor-of-two from the denominator.
    denom_bits as usize
  } else {
    2048
  };

  // Compute conservative output length up-front so we can allocate fallibly once.
  let sign_len = if negative { 1usize } else { 0usize };
  let int_len = int_digits_rev.len();
  let frac_len = if rem.is_zero() { 0usize } else { max_frac_digits.min(2048) };
  let dot_len = if frac_len != 0 { 1usize } else { 0usize };
  let total_len = sign_len
    .saturating_add(int_len)
    .saturating_add(dot_len)
    .saturating_add(frac_len);
  out
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;

  if negative {
    out.push(b'-' as u16);
  }

  for &b in int_digits_rev.iter().rev() {
    out.push(b as u16);
  }

  if !rem.is_zero() {
    out.push(b'.' as u16);

    let mut digits_written = 0usize;
    while !rem.is_zero() && digits_written < max_frac_digits {
      if digits_written % 1024 == 0 {
        vm.tick()?;
      }

      // rem = rem * radix
      rem.mul_small(radix)?;
      // digit = floor(rem / 2^denom_bits)
      let digit = rem.shr_to_u32(denom_bits);
      // rem = rem mod 2^denom_bits
      rem.truncate_bits(denom_bits);

      out.push(digit_to_ascii(digit) as u16);
      digits_written = digits_written.saturating_add(1);
    }
  }

  scope.alloc_string_from_u16_vec(out)
}

fn add_plus_to_exponent(s: &str) -> Result<String, VmError> {
  let Some((mant, exp)) = s.split_once('e') else {
    let mut out = String::new();
    out.try_reserve_exact(s.len()).map_err(|_| VmError::OutOfMemory)?;
    out.push_str(s);
    return Ok(out);
  };
  let mut out = String::new();
  out
    .try_reserve_exact(mant.len().saturating_add(1).saturating_add(exp.len()).saturating_add(1))
    .map_err(|_| VmError::OutOfMemory)?;
  out.push_str(mant);
  out.push('e');
  if exp.starts_with('-') {
    out.push_str(exp);
  } else {
    out.push('+');
    out.push_str(exp);
  }
  Ok(out)
}

/// `Number.prototype.toString(radix)` (ECMA-262).
pub fn number_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let x = this_number_value(
    &mut scope,
    this,
    "Number.prototype.toString called on incompatible receiver",
  )?;

  let radix_val = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(radix_val, Value::Undefined) {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }

  let mut radix = scope.to_number(vm, host, hooks, radix_val)?;
  if radix.is_nan() {
    radix = 0.0;
  }
  if !radix.is_finite() {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid radix")?;
    return Err(VmError::Throw(err));
  }
  radix = radix.trunc();
  if radix < 2.0 || radix > 36.0 {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid radix")?;
    return Err(VmError::Throw(err));
  }
  let radix_u32 = radix as u32;

  if x.is_nan() {
    return Ok(Value::String(scope.alloc_string("NaN")?));
  }
  if x == 0.0 {
    return Ok(Value::String(scope.alloc_string("0")?));
  }
  if x.is_infinite() || radix_u32 == 10 {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }

  let s = number_to_string_radix(vm, &mut scope, x, radix_u32)?;
  Ok(Value::String(s))
}

/// `Number.prototype.toFixed(fractionDigits)` (ECMA-262).
pub fn number_prototype_to_fixed(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let x = this_number_value(
    &mut scope,
    this,
    "Number.prototype.toFixed called on incompatible receiver",
  )?;

  let fd_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut f = if matches!(fd_val, Value::Undefined) {
    0.0
  } else {
    scope.to_number(vm, host, hooks, fd_val)?
  };
  if f.is_nan() {
    f = 0.0;
  }
  if !f.is_finite() {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid fractionDigits")?;
    return Err(VmError::Throw(err));
  }
  f = f.trunc();
  if f < 0.0 || f > 100.0 {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid fractionDigits")?;
    return Err(VmError::Throw(err));
  }
  let f = f as usize;

  if x.is_nan() || x.is_infinite() {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }
  if x.abs() >= 1e21 {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }

  // Normalize -0 to +0 so Rust's formatting doesn't produce "-0.00" etc.
  let x = if x == 0.0 { 0.0 } else { x };

  let mut buf = String::new();
  buf
    .try_reserve_exact(256)
    .map_err(|_| VmError::OutOfMemory)?;
  write!(&mut buf, "{:.*}", f, x).unwrap();
  let out = scope.alloc_string(&buf)?;
  Ok(Value::String(out))
}

/// `Number.prototype.toExponential(fractionDigits)` (ECMA-262).
pub fn number_prototype_to_exponential(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let x = this_number_value(
    &mut scope,
    this,
    "Number.prototype.toExponential called on incompatible receiver",
  )?;

  if x.is_nan() || x.is_infinite() {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }

  // Normalize -0 to +0 so Rust doesn't emit "-0e0".
  let x = if x == 0.0 { 0.0 } else { x };

  let fd_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let raw = if matches!(fd_val, Value::Undefined) {
    let mut buf = String::new();
    buf.try_reserve_exact(64).map_err(|_| VmError::OutOfMemory)?;
    write!(&mut buf, "{:e}", x).unwrap();
    buf
  } else {
    let mut f = scope.to_number(vm, host, hooks, fd_val)?;
    if f.is_nan() {
      f = 0.0;
    }
    if !f.is_finite() {
      let intr = require_intrinsics(vm)?;
      let err = crate::new_range_error(&mut scope, intr, "Invalid fractionDigits")?;
      return Err(VmError::Throw(err));
    }
    f = f.trunc();
    if f < 0.0 || f > 100.0 {
      let intr = require_intrinsics(vm)?;
      let err = crate::new_range_error(&mut scope, intr, "Invalid fractionDigits")?;
      return Err(VmError::Throw(err));
    }
    let f = f as usize;

    let mut buf = String::new();
    buf.try_reserve_exact(256).map_err(|_| VmError::OutOfMemory)?;
    write!(&mut buf, "{:.*e}", f, x).unwrap();
    buf
  };

  let fixed = add_plus_to_exponent(&raw)?;
  let out = scope.alloc_string(&fixed)?;
  Ok(Value::String(out))
}

/// `Number.prototype.toPrecision(precision)` (ECMA-262).
pub fn number_prototype_to_precision(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let x = this_number_value(
    &mut scope,
    this,
    "Number.prototype.toPrecision called on incompatible receiver",
  )?;

  let precision_val = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(precision_val, Value::Undefined) {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }

  let mut p = scope.to_number(vm, host, hooks, precision_val)?;
  if p.is_nan() {
    p = 0.0;
  }
  if !p.is_finite() {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid precision")?;
    return Err(VmError::Throw(err));
  }
  p = p.trunc();
  if p < 1.0 || p > 100.0 {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(&mut scope, intr, "Invalid precision")?;
    return Err(VmError::Throw(err));
  }
  let p = p as usize;

  if x.is_nan() || x.is_infinite() {
    return Ok(Value::String(scope.heap_mut().to_string(Value::Number(x))?));
  }

  // Normalize -0 to +0.
  let x = if x == 0.0 { 0.0 } else { x };
  let negative = x < 0.0;
  let abs = if negative { -x } else { x };

  // Start with exponential form at the requested precision so any rounding is consistent.
  let mut exp_buf = String::new();
  exp_buf
    .try_reserve_exact(256)
    .map_err(|_| VmError::OutOfMemory)?;
  write!(&mut exp_buf, "{:.*e}", p.saturating_sub(1), abs).unwrap();

  let Some((mantissa, exp_part)) = exp_buf.split_once('e') else {
    return Err(VmError::InvariantViolation("expected exponential formatting to contain 'e'"));
  };
  let exp: i32 = exp_part.parse().unwrap_or(0);

  // Extract mantissa digits (ASCII) without the decimal point.
  let mantissa_bytes = mantissa.as_bytes();
  let mut digits: Vec<u8> = Vec::new();
  digits
    .try_reserve_exact(p)
    .map_err(|_| VmError::OutOfMemory)?;
  for &b in mantissa_bytes {
    if b == b'.' {
      continue;
    }
    digits.push(b);
  }

  let use_exponential = exp < -6 || exp >= (p as i32);
  let mut out = String::new();

  if use_exponential {
    // `mantissa` already contains the decimal point and trailing zeros to produce exactly `p`
    // significant digits.
    let exp_fixed = add_plus_to_exponent(&exp_buf)?;
    out
      .try_reserve_exact((if negative { 1 } else { 0 }) + exp_fixed.len())
      .map_err(|_| VmError::OutOfMemory)?;
    if negative {
      out.push('-');
    }
    out.push_str(&exp_fixed);
  } else {
    // Convert to fixed notation by shifting the decimal point within `digits`.
    let decimal_pos: i32 = exp + 1;

    // Estimate output length for a single fallible reservation.
    let sign_len = if negative { 1usize } else { 0usize };
    let leading_zeros = if decimal_pos <= 0 { (-decimal_pos) as usize } else { 0 };
    let dot_len = if decimal_pos < (digits.len() as i32) { 1usize } else { 0usize };
    let int_pad_zeros = if decimal_pos > (digits.len() as i32) {
      (decimal_pos as usize).saturating_sub(digits.len())
    } else {
      0usize
    };

    let total_len = sign_len
      .saturating_add(1) // at least "0"
      .saturating_add(dot_len)
      .saturating_add(leading_zeros)
      .saturating_add(digits.len())
      .saturating_add(int_pad_zeros);
    out.try_reserve_exact(total_len).map_err(|_| VmError::OutOfMemory)?;

    if negative {
      out.push('-');
    }

    if decimal_pos <= 0 {
      out.push('0');
      out.push('.');
      for _ in 0..leading_zeros {
        out.push('0');
      }
      for &b in &digits {
        out.push(b as char);
      }
    } else {
      let dec = decimal_pos as usize;
      if dec >= digits.len() {
        for &b in &digits {
          out.push(b as char);
        }
        for _ in 0..int_pad_zeros {
          out.push('0');
        }
      } else {
        for &b in &digits[..dec] {
          out.push(b as char);
        }
        out.push('.');
        for &b in &digits[dec..] {
          out.push(b as char);
        }
      }
    }
  }

  let out = scope.alloc_string(&out)?;
  Ok(Value::String(out))
}

/// `Number.prototype.toLocaleString` (placeholder).
pub fn number_prototype_to_locale_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  number_prototype_to_string(vm, scope, host, hooks, callee, this, &[])
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;
  let prim = match args.first().copied() {
    None => false,
    Some(v) => scope.heap().to_boolean(v)?,
  };

  let proto = crate::spec_ops::get_prototype_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intr.boolean_prototype(),
  )?;
  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

  // Store the primitive value on an internal symbol so `Boolean.prototype.valueOf` can recover it.
  let marker = scope.alloc_string("vm-js.internal.BooleanData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  scope.define_property(
    obj,
    marker_key,
    data_desc(Value::Bool(prim), true, false, false),
  )?;

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
  Ok(Value::Bool(this_boolean_value(
    scope,
    this,
    "Boolean.prototype.valueOf called on incompatible receiver",
  )?))
}

/// `Boolean.prototype.toString`.
pub fn boolean_prototype_to_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let b = this_boolean_value(scope, this, "Boolean.prototype.toString called on incompatible receiver")?;
  if b {
    Ok(Value::String(scope.alloc_string("true")?))
  } else {
    Ok(Value::String(scope.alloc_string("false")?))
  }
}

/// `Number.isNaN`.
pub fn number_is_nan(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  match v {
    Value::Number(n) => Ok(Value::Bool(n.is_nan())),
    _ => Ok(Value::Bool(false)),
  }
}

/// `Number.isFinite`.
pub fn number_is_finite(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  match v {
    Value::Number(n) => Ok(Value::Bool(n.is_finite())),
    _ => Ok(Value::Bool(false)),
  }
}

/// `Number.isInteger`.
pub fn number_is_integer(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Number(n) = v else {
    return Ok(Value::Bool(false));
  };
  if !n.is_finite() {
    return Ok(Value::Bool(false));
  }
  Ok(Value::Bool(n.trunc() == n))
}

/// `Number.isSafeInteger`.
pub fn number_is_safe_integer(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Number(n) = v else {
    return Ok(Value::Bool(false));
  };
  if !n.is_finite() {
    return Ok(Value::Bool(false));
  }
  if n.trunc() != n {
    return Ok(Value::Bool(false));
  }
  Ok(Value::Bool(n.abs() <= 9007199254740991.0))
}

fn this_bigint_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  this: Value,
  method: &'static str,
) -> Result<crate::JsBigInt, VmError> {
  match this {
    Value::BigInt(b) => Ok(b),
    Value::Object(obj) => {
      let marker_sym = match scope.heap().internal_bigint_data_symbol() {
        Some(sym) => sym,
        None => {
          // Fall back to creating the marker symbol if it hasn't been interned yet (should be rare;
          // primarily reachable if a host created a BigInt wrapper without going through
          // `Object(1n)`).
          let marker = scope.alloc_string("vm-js.internal.BigIntData")?;
          scope.heap_mut().symbol_for_with_tick(marker, || vm.tick())?
        }
      };
      let marker_key = PropertyKey::from_symbol(marker_sym);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        Some(Value::BigInt(b)) => Ok(b),
        _ => Err(VmError::TypeError(method)),
      }
    }
    _ => Err(VmError::TypeError(method)),
  }
}

/// ECMAScript `ToBigInt` (minimal).
fn to_bigint(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<crate::JsBigInt, VmError> {
  // 1. Let prim be ? ToPrimitive(argument, hint Number).
  let prim = scope.to_primitive(vm, host, hooks, value, crate::ToPrimitiveHint::Number)?;

  match prim {
    Value::BigInt(b) => Ok(b),
    Value::Bool(b) => Ok(crate::JsBigInt::from_u128(if b { 1 } else { 0 })),
    Value::Number(n) => match crate::exec::f64_to_bigint_integral(n) {
      Some(bi) => Ok(bi),
      None => {
        let intr = require_intrinsics(vm)?;
        let err = crate::new_range_error(scope, intr, "Cannot convert number to BigInt")?;
        Err(VmError::Throw(err))
      }
    },
    Value::String(s) => {
      let mut tick = || vm.tick();
      match crate::exec::string_to_bigint(scope.heap(), s, &mut tick) {
        Ok(Some(bi)) => Ok(bi),
        Ok(None) => {
          let intr = require_intrinsics(vm)?;
          let err = crate::new_syntax_error_object(scope, &intr, "Cannot convert string to BigInt")?;
          Err(VmError::Throw(err))
        }
        Err(VmError::Unimplemented("BigInt parse overflow")) => {
          let intr = require_intrinsics(vm)?;
          let err = crate::new_range_error(scope, intr, "BigInt overflow")?;
          Err(VmError::Throw(err))
        }
        Err(err) => Err(err),
      }
    }
    _ => Err(VmError::TypeError("Cannot convert value to BigInt")),
  }
}

/// `BigInt(value)` (ECMA-262).
pub fn bigint_constructor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let bi = to_bigint(vm, scope, host, hooks, arg0)?;
  Ok(Value::BigInt(bi))
}

/// `BigInt.asIntN(bits, bigint)` (ECMA-262).
pub fn bigint_as_int_n(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let bits_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut bits = scope.to_number(vm, host, hooks, bits_val)?;
  if bits.is_nan() {
    bits = 0.0;
  }
  if !bits.is_finite() {
    let err = crate::new_range_error(scope, intr, "Invalid bits")?;
    return Err(VmError::Throw(err));
  }
  bits = bits.trunc();
  if bits < 0.0 || bits > 256.0 {
    let err = crate::new_range_error(scope, intr, "Invalid bits")?;
    return Err(VmError::Throw(err));
  }
  let bits_u32 = bits as u32;

  let bigint_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let bi = to_bigint(vm, scope, host, hooks, bigint_val)?;
  let Some(out) = bi.as_int_n(bits_u32) else {
    let err = crate::new_range_error(scope, intr, "Invalid bits")?;
    return Err(VmError::Throw(err));
  };
  Ok(Value::BigInt(out))
}

/// `BigInt.asUintN(bits, bigint)` (ECMA-262).
pub fn bigint_as_uint_n(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let bits_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut bits = scope.to_number(vm, host, hooks, bits_val)?;
  if bits.is_nan() {
    bits = 0.0;
  }
  if !bits.is_finite() {
    let err = crate::new_range_error(scope, intr, "Invalid bits")?;
    return Err(VmError::Throw(err));
  }
  bits = bits.trunc();
  if bits < 0.0 || bits > 256.0 {
    let err = crate::new_range_error(scope, intr, "Invalid bits")?;
    return Err(VmError::Throw(err));
  }
  let bits_u32 = bits as u32;

  let bigint_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let bi = to_bigint(vm, scope, host, hooks, bigint_val)?;
  let Some(out) = bi.as_uint_n(bits_u32) else {
    let err = crate::new_range_error(scope, intr, "Invalid bits")?;
    return Err(VmError::Throw(err));
  };
  Ok(Value::BigInt(out))
}

fn bigint_to_string_radix(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  x: crate::JsBigInt,
  radix: u32,
) -> Result<GcString, VmError> {
  debug_assert!((2..=36).contains(&radix));

  if x.is_zero() {
    return scope.alloc_string("0");
  }
  if radix == 10 {
    let s = x.to_decimal_string();
    return scope.alloc_string(&s);
  }

  let negative = x.is_negative();
  let mut n = if negative { x.negate() } else { x };
  let radix_bi = crate::JsBigInt::from_u128(radix as u128);

  // Worst-case (radix 2) a 256-bit integer has 256 digits, plus an optional `-`.
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(260)
    .map_err(|_| VmError::OutOfMemory)?;

  let mut steps = 0usize;
  while !n.is_zero() {
    if steps % 32 == 0 {
      vm.tick()?;
    }
    steps += 1;

    let rem = n
      .checked_rem(radix_bi)
      .ok_or(VmError::InvariantViolation("BigInt remainder failed"))?;
    let div = n
      .checked_div(radix_bi)
      .ok_or(VmError::InvariantViolation("BigInt division failed"))?;

    let Some(rem_i128) = rem.try_to_i128() else {
      return Err(VmError::InvariantViolation("BigInt remainder does not fit in i128"));
    };
    if rem_i128 < 0 {
      return Err(VmError::InvariantViolation("BigInt remainder is negative"));
    }
    let digit = rem_i128 as u32;
    out.push(digit_to_ascii(digit) as u16);

    n = div;
  }

  if negative {
    out.push(b'-' as u16);
  }
  out.reverse();
  scope.alloc_string_from_u16_vec(out)
}

/// `BigInt.prototype.valueOf` (ECMA-262).
pub fn bigint_prototype_value_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::BigInt(this_bigint_value(
    vm,
    scope,
    this,
    "BigInt.prototype.valueOf called on incompatible receiver",
  )?))
}

/// `BigInt.prototype.toString` (ECMA-262).
pub fn bigint_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let x = this_bigint_value(
    vm,
    scope,
    this,
    "BigInt.prototype.toString called on incompatible receiver",
  )?;

  let radix_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let radix_u32 = if matches!(radix_val, Value::Undefined) {
    10
  } else {
    let intr = require_intrinsics(vm)?;
    let mut radix = scope.to_number(vm, host, hooks, radix_val)?;
    if radix.is_nan() {
      radix = 0.0;
    }
    if !radix.is_finite() {
      let err = crate::new_range_error(scope, intr, "Invalid radix")?;
      return Err(VmError::Throw(err));
    }
    radix = radix.trunc();
    if radix < 2.0 || radix > 36.0 {
      let err = crate::new_range_error(scope, intr, "Invalid radix")?;
      return Err(VmError::Throw(err));
    }
    radix as u32
  };

  if radix_u32 == 10 {
    return Ok(Value::String(scope.alloc_string(&x.to_decimal_string())?));
  }

  let s = bigint_to_string_radix(vm, scope, x, radix_u32)?;
  Ok(Value::String(s))
}

/// `BigInt.prototype.toLocaleString` (placeholder).
pub fn bigint_prototype_to_locale_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  bigint_prototype_to_string(vm, scope, host, hooks, callee, this, &[])
}

/// `BigInt.prototype[Symbol.toPrimitive]` (minimal).
pub fn bigint_prototype_to_primitive(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // The spec ignores the hint and returns the BigInt value.
  Ok(Value::BigInt(this_bigint_value(
    vm,
    scope,
    this,
    "BigInt.prototype[@@toPrimitive] called on incompatible receiver",
  )?))
}

/// `Symbol.prototype.valueOf` (minimal).
pub fn symbol_prototype_value_of(
  vm: &mut Vm,
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
      let marker_sym = match scope.heap().internal_symbol_data_symbol() {
        Some(sym) => sym,
        None => {
          // Fall back to creating the marker symbol if it hasn't been interned yet (should be
          // rare; primarily reachable if a host created a Symbol wrapper without going through
          // `Object(Symbol(...))`).
          let marker = scope.alloc_string("vm-js.internal.SymbolData")?;
          scope.heap_mut().symbol_for_with_tick(marker, || vm.tick())?
        }
      };
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

  let s = symbol_descriptive_string(scope, sym, || vm.tick())?;
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

/// `get Symbol.prototype.description`.
pub fn symbol_prototype_description_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Symbol(sym) = symbol_prototype_value_of(vm, scope, host, hooks, callee, this, &[])? else {
    return Err(VmError::InvariantViolation(
      "Symbol.prototype.valueOf returned non-symbol",
    ));
  };
  let desc = scope.heap().get_symbol_description(sym)?;
  Ok(match desc {
    Some(s) => Value::String(s),
    None => Value::Undefined,
  })
}

/// Global `eval(x)`.
///
/// Note: direct eval is handled by the evaluator (`eval(...)` call expressions); this builtin
/// implements *indirect* eval semantics (ECMA-262 `PerformEval` with `direct = false`).
pub fn global_eval(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.first().copied().unwrap_or(Value::Undefined);
  let Value::String(source_string) = arg0 else {
    return Ok(arg0);
  };

  // Indirect eval executes in the global environment of the `eval` function's realm.
  let (global_object, global_lexical_env) = {
    let f = scope.heap().get_function(callee)?;
    let global_object = f.realm.ok_or(VmError::Unimplemented("eval missing [[Realm]]"))?;
    let global_lexical_env =
      f.closure_env
        .ok_or(VmError::Unimplemented("eval missing [[Environment]]"))?;
    (global_object, global_lexical_env)
  };

  let mut eval_scope = scope.reborrow();
  eval_scope.push_root(Value::Object(global_object))?;
  eval_scope.push_root(Value::String(source_string))?;
  eval_scope.push_env_root(global_lexical_env)?;

  crate::exec::perform_indirect_eval(
    vm,
    &mut eval_scope,
    host,
    hooks,
    global_object,
    global_lexical_env,
    source_string,
  )
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
    // `units[s_start..end]` was validated above to contain only radix digits, but keep this
    // fallible so malformed inputs can never trigger a panic.
    let digit = match radix_digit_value(unit) {
      Some(d) => d as f64,
      None => {
        return Err(VmError::InvariantViolation(
          "ParseInt digit slice contained non-radix digit",
        ));
      }
    };
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

fn parse_ascii_digits_to_i64_with_limit(
  units: &[u16],
  max: i64,
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<i64, VmError> {
  let mut v: i64 = 0;
  for (i, &u) in units.iter().enumerate() {
    if i % 1024 == 0 {
      tick()?;
    }
    let d = (u - b'0' as u16) as i64;
    if v > max {
      return Ok(max);
    }
    v = v.saturating_mul(10).saturating_add(d);
    if v > max {
      return Ok(max);
    }
  }
  Ok(v)
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
    exp_part = parse_ascii_digits_to_i64_with_limit(&prefix_units[i..], MAX_EXP_ABS, &mut tick)?;
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
      _ => return throw_uri_error(vm, scope, hooks, "URI malformed"),
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

fn math_binary_number_op(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  args: &[Value],
  f: impl FnOnce(f64, f64) -> f64,
) -> Result<Value, VmError> {
  let a = args.get(0).copied().unwrap_or(Value::Undefined);
  let b = args.get(1).copied().unwrap_or(Value::Undefined);
  let x = scope.to_number(vm, host, hooks, a)?;
  let y = scope.to_number(vm, host, hooks, b)?;
  Ok(Value::Number(f(x, y)))
}

fn to_uint32(n: f64) -> u32 {
  if !n.is_finite() || n == 0.0 {
    return 0;
  }
  // ECMA-262 `ToUint32`: truncate then compute modulo 2^32.
  let int = n.trunc();
  const TWO_32: f64 = 4_294_967_296.0;
  let mut int = int % TWO_32;
  if int < 0.0 {
    int += TWO_32;
  }
  int as u32
}

const MATH_VARIADIC_TICK_EVERY: usize = 32;

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

/// `Math.acos(x)` (ECMA-262).
pub fn math_acos(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.acos())
}

/// `Math.acosh(x)` (ECMA-262).
pub fn math_acosh(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.acosh())
}

/// `Math.asin(x)` (ECMA-262).
pub fn math_asin(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.asin())
}

/// `Math.asinh(x)` (ECMA-262).
pub fn math_asinh(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.asinh())
}

/// `Math.atan(x)` (ECMA-262).
pub fn math_atan(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.atan())
}

/// `Math.atan2(y, x)` (ECMA-262).
pub fn math_atan2(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_binary_number_op(vm, scope, host, hooks, args, |y, x| y.atan2(x))
}

/// `Math.atanh(x)` (ECMA-262).
pub fn math_atanh(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.atanh())
}

/// `Math.cbrt(x)` (ECMA-262).
pub fn math_cbrt(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.cbrt())
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

/// `Math.clz32(x)` (ECMA-262).
pub fn math_clz32(
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
  let u = to_uint32(n);
  Ok(Value::Number(u.leading_zeros() as f64))
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

/// `Math.cos(x)` (ECMA-262).
pub fn math_cos(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.cos())
}

/// `Math.cosh(x)` (ECMA-262).
pub fn math_cosh(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.cosh())
}

/// `Math.expm1(x)` (ECMA-262).
pub fn math_expm1(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.exp_m1())
}

/// `Math.fround(x)` (ECMA-262).
pub fn math_fround(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| (n as f32) as f64)
}

/// `Math.hypot(...args)` (ECMA-262).
pub fn math_hypot(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.is_empty() {
    return Ok(Value::Number(0.0));
  }

  let mut seen_nan = false;
  let mut acc = 0.0f64;
  for (i, v) in args.iter().copied().enumerate() {
    if i % MATH_VARIADIC_TICK_EVERY == 0 {
      vm.tick()?;
    }
    let n = scope.to_number(vm, host, hooks, v)?;
    if n.is_infinite() {
      // Spec: ±Infinity overrides NaN.
      return Ok(Value::Number(f64::INFINITY));
    }
    if n.is_nan() {
      seen_nan = true;
      continue;
    }
    if !seen_nan {
      acc = acc.hypot(n);
    }
  }

  if seen_nan {
    Ok(Value::Number(f64::NAN))
  } else {
    Ok(Value::Number(acc))
  }
}

/// `Math.imul(a, b)` (ECMA-262).
pub fn math_imul(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let a = args.get(0).copied().unwrap_or(Value::Undefined);
  let b = args.get(1).copied().unwrap_or(Value::Undefined);
  let x = scope.to_number(vm, host, hooks, a)?;
  let y = scope.to_number(vm, host, hooks, b)?;
  let ax = to_uint32(x);
  let by = to_uint32(y);
  let out = ax.wrapping_mul(by) as i32;
  Ok(Value::Number(out as f64))
}

/// `Math.log1p(x)` (ECMA-262).
pub fn math_log1p(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.ln_1p())
}

/// `Math.log10(x)` (ECMA-262).
pub fn math_log10(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.log10())
}

/// `Math.log2(x)` (ECMA-262).
pub fn math_log2(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.log2())
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
    if i % MATH_VARIADIC_TICK_EVERY == 0 {
      vm.tick()?;
    }
    let n = scope.to_number(vm, host, hooks, v)?;
    if n.is_nan() {
      return Ok(Value::Number(f64::NAN));
    }

    if n > best {
      best = n;
      continue;
    }
    if n == best && n == 0.0 && best.is_sign_negative() && !n.is_sign_negative() {
      // Spec: if either value is +0, `Math.max` must return +0.
      best = n;
    }
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
    if i % MATH_VARIADIC_TICK_EVERY == 0 {
      vm.tick()?;
    }
    let n = scope.to_number(vm, host, hooks, v)?;
    if n.is_nan() {
      return Ok(Value::Number(f64::NAN));
    }

    if n < best {
      best = n;
      continue;
    }
    if n == best && n == 0.0 && !best.is_sign_negative() && n.is_sign_negative() {
      // Spec: if either value is -0, `Math.min` must return -0.
      best = n;
    }
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

/// `Math.sign(x)` (ECMA-262).
pub fn math_sign(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| {
    if n.is_nan() {
      return f64::NAN;
    }
    if n == 0.0 {
      // Preserve the sign of zero.
      return n;
    }
    if n.is_sign_negative() {
      -1.0
    } else {
      1.0
    }
  })
}

/// `Math.sin(x)` (ECMA-262).
pub fn math_sin(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.sin())
}

/// `Math.sinh(x)` (ECMA-262).
pub fn math_sinh(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.sinh())
}

/// `Math.tan(x)` (ECMA-262).
pub fn math_tan(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.tan())
}

/// `Math.tanh(x)` (ECMA-262).
pub fn math_tanh(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  math_unary_number_op(vm, scope, host, hooks, args, |n| n.tanh())
}

/// `Math.random()` (ECMA-262) (deterministic PRNG).
pub fn math_random(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Host override hook (if any) wins, otherwise fall back to the VM's PRNG.
  let x = hooks.host_math_random_u64().unwrap_or_else(|| vm.next_math_random_u64());

  // Convert the high 53 bits into a double in [0, 1).
  let bits = x >> 11;
  let n = (bits as f64) * (1.0 / ((1u64 << 53) as f64));
  Ok(Value::Number(n))
}

const DATE_MS_PER_SECOND: i64 = 1_000;
const DATE_MS_PER_MINUTE: i64 = 60 * DATE_MS_PER_SECOND;
const DATE_MS_PER_HOUR: i64 = 60 * DATE_MS_PER_MINUTE;
const DATE_MS_PER_DAY: i64 = 24 * DATE_MS_PER_HOUR;
const DATE_TIME_CLIP_RANGE: f64 = 8.64e15;

fn date_time_clip(time: f64) -> f64 {
  if !time.is_finite() {
    return f64::NAN;
  }
  if time.abs() > DATE_TIME_CLIP_RANGE {
    return f64::NAN;
  }
  time.trunc()
}

fn to_integer_or_infinity_i64(n: f64) -> Option<i64> {
  // ECMA-262 `ToIntegerOrInfinity`:
  // - NaN => +0
  // - +/-Infinity => +/-Infinity
  // - finite => truncate toward zero
  let n = if n.is_nan() { 0.0 } else { n };
  if !n.is_finite() {
    return None;
  }
  let t = n.trunc();
  if t < i64::MIN as f64 || t > i64::MAX as f64 {
    return None;
  }
  Some(t as i64)
}

fn is_leap_year(year: i64) -> bool {
  year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn day_from_year(year: i64) -> i64 {
  365 * (year - 1970)
    + (year - 1969).div_euclid(4)
    - (year - 1901).div_euclid(100)
    + (year - 1601).div_euclid(400)
}

fn time_from_year(year: i64) -> i64 {
  day_from_year(year).saturating_mul(DATE_MS_PER_DAY)
}

fn year_from_time(time: i64) -> i64 {
  // Time values are limited by TimeClip to ±8.64e15ms, which corresponds to roughly ±275k years.
  // Use a fixed-range binary search to avoid fragile floating-point approximations.
  let mut lo: i64 = -400_000;
  let mut hi: i64 = 400_000;
  while lo < hi {
    let mid = lo + (hi - lo + 1) / 2;
    if time_from_year(mid) <= time {
      lo = mid;
    } else {
      hi = mid - 1;
    }
  }
  lo
}

fn make_day(year: i64, month: i64, date: i64) -> Option<i128> {
  let year = year as i128;
  let month = month as i128;
  let date = date as i128;

  let ym = year.checked_add(month.div_euclid(12))?;
  let mn = month.rem_euclid(12);
  let mn_usize: usize = usize::try_from(mn).ok()?;
  if mn_usize >= 12 {
    return None;
  }

  let ym_i64: i64 = i64::try_from(ym).ok()?;
  let leap = is_leap_year(ym_i64);

  const DAYS_BEFORE_MONTH: [i128; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

  let mut days = 365i128
    .checked_mul(ym - 1970)?
    .checked_add((ym - 1969).div_euclid(4))?
    .checked_sub((ym - 1901).div_euclid(100))?
    .checked_add((ym - 1601).div_euclid(400))?;

  days = days.checked_add(DAYS_BEFORE_MONTH[mn_usize])?;
  if leap && mn_usize >= 2 {
    days = days.checked_add(1)?;
  }
  days.checked_add(date.checked_sub(1)?)
}

fn make_time(hour: i64, min: i64, sec: i64, ms: i64) -> Option<i128> {
  let hour = hour as i128;
  let min = min as i128;
  let sec = sec as i128;
  let ms = ms as i128;

  let t = hour.checked_mul(DATE_MS_PER_HOUR as i128)?;
  let t = t.checked_add(min.checked_mul(DATE_MS_PER_MINUTE as i128)?)?;
  let t = t.checked_add(sec.checked_mul(DATE_MS_PER_SECOND as i128)?)?;
  t.checked_add(ms)
}

fn make_date(day: i128, time: i128) -> Option<i128> {
  day
    .checked_mul(DATE_MS_PER_DAY as i128)?
    .checked_add(time)
}

#[derive(Clone, Copy)]
struct DateParts {
  year: i64,
  month0: u8,
  date: u8,
  hour: u8,
  minute: u8,
  second: u8,
  millisecond: u16,
  weekday: u8,
}

fn date_parts_from_time(time: i64) -> DateParts {
  let day = time.div_euclid(DATE_MS_PER_DAY);
  let time_within_day = time.rem_euclid(DATE_MS_PER_DAY);

  let year = year_from_time(time);
  let day_within_year = day - day_from_year(year);
  let leap = is_leap_year(year);

  const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

  let mut month0: u8 = 0;
  let mut date: u8 = 1;
  let mut acc = 0i64;
  for (m, &dim0) in MONTH_DAYS.iter().enumerate() {
    let dim = if leap && m == 1 { dim0 + 1 } else { dim0 };
    if day_within_year < acc + dim {
      month0 = m as u8;
      date = (day_within_year - acc + 1) as u8;
      break;
    }
    acc += dim;
  }

  let hour = (time_within_day / DATE_MS_PER_HOUR) as u8;
  let minute = ((time_within_day % DATE_MS_PER_HOUR) / DATE_MS_PER_MINUTE) as u8;
  let second = ((time_within_day % DATE_MS_PER_MINUTE) / DATE_MS_PER_SECOND) as u8;
  let millisecond = (time_within_day % DATE_MS_PER_SECOND) as u16;

  // 1970-01-01 is a Thursday. WeekDay: (Day + 4) mod 7, where 0 = Sunday.
  let weekday = (day.saturating_add(4).rem_euclid(7)) as u8;

  DateParts {
    year,
    month0,
    date,
    hour,
    minute,
    second,
    millisecond,
    weekday,
  }
}

fn vec_try_push_ascii(buf: &mut Vec<u16>, bytes: &[u8]) -> Result<(), VmError> {
  for &b in bytes {
    vec_try_push(buf, b as u16)?;
  }
  Ok(())
}

fn vec_try_push_two_digits(buf: &mut Vec<u16>, n: u8) -> Result<(), VmError> {
  vec_try_push(buf, (b'0' + (n / 10)) as u16)?;
  vec_try_push(buf, (b'0' + (n % 10)) as u16)?;
  Ok(())
}

fn vec_try_push_three_digits(buf: &mut Vec<u16>, n: u16) -> Result<(), VmError> {
  vec_try_push(buf, (b'0' + ((n / 100) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + (((n / 10) % 10) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + ((n % 10) as u8)) as u16)?;
  Ok(())
}

fn vec_try_push_year_iso(buf: &mut Vec<u16>, year: i64) -> Result<(), VmError> {
  if (0..=9999).contains(&year) {
    let y = year as u32;
    vec_try_push(buf, (b'0' + ((y / 1000) as u8)) as u16)?;
    vec_try_push(buf, (b'0' + (((y / 100) % 10) as u8)) as u16)?;
    vec_try_push(buf, (b'0' + (((y / 10) % 10) as u8)) as u16)?;
    vec_try_push(buf, (b'0' + ((y % 10) as u8)) as u16)?;
    return Ok(());
  }

  let (sign, abs) = if year < 0 {
    (b'-', year.saturating_neg() as u32)
  } else {
    (b'+', year as u32)
  };
  vec_try_push(buf, sign as u16)?;
  let y = abs;
  vec_try_push(buf, (b'0' + ((y / 100_000) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + (((y / 10_000) % 10) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + (((y / 1000) % 10) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + (((y / 100) % 10) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + (((y / 10) % 10) as u8)) as u16)?;
  vec_try_push(buf, (b'0' + ((y % 10) as u8)) as u16)?;
  Ok(())
}

fn date_alloc_iso_string(scope: &mut Scope<'_>, time: i64) -> Result<crate::GcString, VmError> {
  let parts = date_parts_from_time(time);
  let mut out: Vec<u16> = Vec::new();
  out.try_reserve_exact(27).map_err(|_| VmError::OutOfMemory)?;

  vec_try_push_year_iso(&mut out, parts.year)?;
  vec_try_push(&mut out, b'-' as u16)?;
  vec_try_push_two_digits(&mut out, parts.month0 + 1)?;
  vec_try_push(&mut out, b'-' as u16)?;
  vec_try_push_two_digits(&mut out, parts.date)?;
  vec_try_push(&mut out, b'T' as u16)?;
  vec_try_push_two_digits(&mut out, parts.hour)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push_two_digits(&mut out, parts.minute)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push_two_digits(&mut out, parts.second)?;
  vec_try_push(&mut out, b'.' as u16)?;
  vec_try_push_three_digits(&mut out, parts.millisecond)?;
  vec_try_push(&mut out, b'Z' as u16)?;

  scope.alloc_string_from_u16_vec(out)
}

fn date_alloc_utc_string(scope: &mut Scope<'_>, time: i64) -> Result<crate::GcString, VmError> {
  const WEEKDAY: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
  const MONTH: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
  ];

  let parts = date_parts_from_time(time);
  let mut out: Vec<u16> = Vec::new();
  out.try_reserve_exact(32).map_err(|_| VmError::OutOfMemory)?;

  vec_try_push_ascii(&mut out, WEEKDAY[parts.weekday as usize])?;
  vec_try_push(&mut out, b',' as u16)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_two_digits(&mut out, parts.date)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_ascii(&mut out, MONTH[parts.month0 as usize])?;
  vec_try_push(&mut out, b' ' as u16)?;

  // Year: match ISO formatting for out-of-range years.
  vec_try_push_year_iso(&mut out, parts.year)?;

  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_two_digits(&mut out, parts.hour)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push_two_digits(&mut out, parts.minute)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push_two_digits(&mut out, parts.second)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_ascii(&mut out, b"GMT")?;

  scope.alloc_string_from_u16_vec(out)
}

fn date_alloc_string(scope: &mut Scope<'_>, time: i64) -> Result<crate::GcString, VmError> {
  const WEEKDAY: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
  const MONTH: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
  ];

  let parts = date_parts_from_time(time);
  let mut out: Vec<u16> = Vec::new();
  out.try_reserve_exact(32).map_err(|_| VmError::OutOfMemory)?;

  vec_try_push_ascii(&mut out, WEEKDAY[parts.weekday as usize])?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_ascii(&mut out, MONTH[parts.month0 as usize])?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_two_digits(&mut out, parts.date)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_year_iso(&mut out, parts.year)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_two_digits(&mut out, parts.hour)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push_two_digits(&mut out, parts.minute)?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push_two_digits(&mut out, parts.second)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_push_ascii(&mut out, b"GMT")?;

  scope.alloc_string_from_u16_vec(out)
}

fn parse_iso_date_string(vm: &mut Vm, scope: &mut Scope<'_>, s: crate::GcString) -> Result<f64, VmError> {
  let js = scope.heap().get_string(s)?;
  let units = js.as_code_units();

  let mut start = 0usize;
  let mut end = units.len();

  while start < end && is_trim_whitespace_unit(units[start]) {
    start += 1;
    if start % 1024 == 0 {
      vm.tick()?;
    }
  }
  let mut trimmed = 0usize;
  while end > start && is_trim_whitespace_unit(units[end - 1]) {
    end -= 1;
    trimmed += 1;
    if trimmed % 1024 == 0 {
      vm.tick()?;
    }
  }

  if start == end {
    return Ok(f64::NAN);
  }

  let mut i = start;

  fn digit(u: u16) -> Option<u8> {
    if (b'0' as u16..=b'9' as u16).contains(&u) {
      Some((u - b'0' as u16) as u8)
    } else {
      None
    }
  }

  fn parse_n_digits(units: &[u16], i: &mut usize, n: usize) -> Option<i64> {
    if *i + n > units.len() {
      return None;
    }
    let mut out: i64 = 0;
    for _ in 0..n {
      let d = digit(units[*i])? as i64;
      out = out.checked_mul(10)?.checked_add(d)?;
      *i += 1;
    }
    Some(out)
  }

  let year: i64;
  if units[i] == b'+' as u16 || units[i] == b'-' as u16 {
    let sign = if units[i] == b'-' as u16 { -1i64 } else { 1i64 };
    i += 1;
    let abs = match parse_n_digits(units, &mut i, 6) {
      Some(v) => v,
      None => return Ok(f64::NAN),
    };
    year = sign.saturating_mul(abs);
  } else {
    year = match parse_n_digits(units, &mut i, 4) {
      Some(v) => v,
      None => return Ok(f64::NAN),
    };
  }

  if i >= end || units[i] != b'-' as u16 {
    return Ok(f64::NAN);
  }
  i += 1;

  let month1 = match parse_n_digits(units, &mut i, 2) {
    Some(v) => v,
    None => return Ok(f64::NAN),
  };
  if !(1..=12).contains(&month1) {
    return Ok(f64::NAN);
  }
  let month0 = month1 - 1;

  if i >= end || units[i] != b'-' as u16 {
    return Ok(f64::NAN);
  }
  i += 1;

  let mut date = match parse_n_digits(units, &mut i, 2) {
    Some(v) => v,
    None => return Ok(f64::NAN),
  };
  if date < 1 {
    return Ok(f64::NAN);
  }

  let dim = match month0 {
    0 | 2 | 4 | 6 | 7 | 9 | 11 => 31,
    3 | 5 | 8 | 10 => 30,
    1 => {
      if is_leap_year(year) {
        29
      } else {
        28
      }
    }
    _ => return Ok(f64::NAN),
  };
  if date > dim {
    return Ok(f64::NAN);
  }

  let mut hour: i64 = 0;
  let mut minute: i64 = 0;
  let mut second: i64 = 0;
  let mut millisecond: i64 = 0;
  let mut tz_offset_min: i64 = 0;

  if i < end {
    if units[i] != b'T' as u16 {
      return Ok(f64::NAN);
    }
    i += 1;

    hour = match parse_n_digits(units, &mut i, 2) {
      Some(v) => v,
      None => return Ok(f64::NAN),
    };
    if hour > 24 {
      return Ok(f64::NAN);
    }
    if i >= end || units[i] != b':' as u16 {
      return Ok(f64::NAN);
    }
    i += 1;

    minute = match parse_n_digits(units, &mut i, 2) {
      Some(v) => v,
      None => return Ok(f64::NAN),
    };
    if minute > 59 {
      return Ok(f64::NAN);
    }

    if i < end && units[i] == b':' as u16 {
      i += 1;
      second = match parse_n_digits(units, &mut i, 2) {
        Some(v) => v,
        None => return Ok(f64::NAN),
      };
      if second > 59 {
        return Ok(f64::NAN);
      }
    }

    if i < end && units[i] == b'.' as u16 {
      i += 1;
      if i >= end {
        return Ok(f64::NAN);
      }
      let mut digits = 0usize;
      let mut ms = 0i64;
      let mut scanned = 0usize;
      while i < end {
        let Some(d) = digit(units[i]) else {
          break;
        };
        if digits < 3 {
          ms = ms.saturating_mul(10).saturating_add(d as i64);
          digits += 1;
        }
        i += 1;
        scanned += 1;
        if scanned % 1024 == 0 {
          vm.tick()?;
        }
      }
      if digits == 0 {
        return Ok(f64::NAN);
      }
      if digits == 1 {
        ms *= 100;
      } else if digits == 2 {
        ms *= 10;
      }
      millisecond = ms;
    }

    if i < end {
      match units[i] {
        u if u == b'Z' as u16 => {
          i += 1;
          tz_offset_min = 0;
        }
        u if u == b'+' as u16 || u == b'-' as u16 => {
          let sign = if u == b'-' as u16 { -1i64 } else { 1i64 };
          i += 1;
          let tz_h = match parse_n_digits(units, &mut i, 2) {
            Some(v) => v,
            None => return Ok(f64::NAN),
          };
          if tz_h > 23 {
            return Ok(f64::NAN);
          }
          if i < end && units[i] == b':' as u16 {
            i += 1;
          }
          let tz_m = match parse_n_digits(units, &mut i, 2) {
            Some(v) => v,
            None => return Ok(f64::NAN),
          };
          if tz_m > 59 {
            return Ok(f64::NAN);
          }
          tz_offset_min = sign.saturating_mul(tz_h.saturating_mul(60).saturating_add(tz_m));
        }
        _ => {
          // Date-time without an explicit timezone: treat as UTC for deterministic host behavior.
          tz_offset_min = 0;
        }
      }
    }
  }

  if i != end {
    return Ok(f64::NAN);
  }

  if hour == 24 {
    if minute != 0 || second != 0 || millisecond != 0 {
      return Ok(f64::NAN);
    }
    hour = 0;
    date = date.saturating_add(1);
  }

  let Some(day) = make_day(year, month0, date) else {
    return Ok(f64::NAN);
  };
  let Some(time_within_day) = make_time(hour, minute, second, millisecond) else {
    return Ok(f64::NAN);
  };
  let Some(mut ms) = make_date(day, time_within_day) else {
    return Ok(f64::NAN);
  };
  ms = ms.saturating_sub((tz_offset_min as i128).saturating_mul(DATE_MS_PER_MINUTE as i128));

  Ok(date_time_clip(ms as f64))
}

fn date_this_time_value(scope: &mut Scope<'_>, this: Value) -> Result<f64, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Date called on non-object"));
  };
  match scope.heap().date_value(obj)? {
    Some(v) => Ok(v),
    None => Err(VmError::TypeError("Date called on non-Date object")),
  }
}

/// `Date` called as a function.
pub fn date_constructor_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let tv = date_time_clip(hooks.host_current_time_millis());
  if !tv.is_finite() {
    return Ok(Value::String(scope.alloc_string("Invalid Date")?));
  }
  let s = date_alloc_string(scope, tv as i64)?;
  Ok(Value::String(s))
}

/// `new Date(...)`.
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
  let time = match args.len() {
    0 => date_time_clip(hooks.host_current_time_millis()),
    1 => {
      let v = args[0];
      let prim = if matches!(v, Value::Object(_)) {
        scope.to_primitive(vm, host, hooks, v, crate::ToPrimitiveHint::Default)?
      } else {
        v
      };
      match prim {
        Value::String(s) => parse_iso_date_string(vm, scope, s)?,
        other => date_time_clip(scope.to_number(vm, host, hooks, other)?),
      }
    }
    _ => {
      let y = scope.to_number(vm, host, hooks, args[0])?;
      let m = scope.to_number(vm, host, hooks, args[1])?;
      let dt = scope
        .to_number(vm, host, hooks, args.get(2).copied().unwrap_or(Value::Number(1.0)))?;
      let h =
        scope.to_number(vm, host, hooks, args.get(3).copied().unwrap_or(Value::Number(0.0)))?;
      let min =
        scope.to_number(vm, host, hooks, args.get(4).copied().unwrap_or(Value::Number(0.0)))?;
      let sec =
        scope.to_number(vm, host, hooks, args.get(5).copied().unwrap_or(Value::Number(0.0)))?;
      let ms =
        scope.to_number(vm, host, hooks, args.get(6).copied().unwrap_or(Value::Number(0.0)))?;

      match (
        to_integer_or_infinity_i64(y),
        to_integer_or_infinity_i64(m),
        to_integer_or_infinity_i64(dt),
        to_integer_or_infinity_i64(h),
        to_integer_or_infinity_i64(min),
        to_integer_or_infinity_i64(sec),
        to_integer_or_infinity_i64(ms),
      ) {
        (
          Some(mut year),
          Some(month),
          Some(date),
          Some(hour),
          Some(minute),
          Some(second),
          Some(millisecond),
        ) => {
          if (0..=99).contains(&year) {
            year = year.saturating_add(1900);
          }
          match (make_day(year, month, date), make_time(hour, minute, second, millisecond)) {
            (Some(day), Some(time)) => match make_date(day, time) {
              Some(ms) => date_time_clip(ms as f64),
              None => f64::NAN,
            },
            _ => f64::NAN,
          }
        }
        _ => f64::NAN,
      }
    }
  };

  // OrdinaryCreateFromConstructor(newTarget, %Date.prototype%).
  let obj = crate::spec_ops::ordinary_create_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intr.date_prototype(),
    &[],
    |scope| scope.alloc_date(time),
  )?;

  Ok(Value::Object(obj))
}

/// `Date.now()`.
pub fn date_now(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Number(date_time_clip(hooks.host_current_time_millis())))
}

/// `Date.parse(string)`.
pub fn date_parse(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = scope.to_string(vm, host, hooks, arg)?;
  Ok(Value::Number(parse_iso_date_string(vm, scope, s)?))
}

/// `Date.UTC(...)`.
pub fn date_utc(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let y = scope.to_number(vm, host, hooks, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let m = scope.to_number(vm, host, hooks, args.get(1).copied().unwrap_or(Value::Undefined))?;
  let dt = scope.to_number(vm, host, hooks, args.get(2).copied().unwrap_or(Value::Number(1.0)))?;
  let h = scope.to_number(vm, host, hooks, args.get(3).copied().unwrap_or(Value::Number(0.0)))?;
  let min = scope.to_number(vm, host, hooks, args.get(4).copied().unwrap_or(Value::Number(0.0)))?;
  let sec = scope.to_number(vm, host, hooks, args.get(5).copied().unwrap_or(Value::Number(0.0)))?;
  let ms = scope.to_number(vm, host, hooks, args.get(6).copied().unwrap_or(Value::Number(0.0)))?;

  let Some(mut year) = to_integer_or_infinity_i64(y) else {
    return Ok(Value::Number(f64::NAN));
  };
  if (0..=99).contains(&year) {
    year = year.saturating_add(1900);
  }
  let Some(month) = to_integer_or_infinity_i64(m) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(date) = to_integer_or_infinity_i64(dt) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(hour) = to_integer_or_infinity_i64(h) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(minute) = to_integer_or_infinity_i64(min) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(second) = to_integer_or_infinity_i64(sec) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(millisecond) = to_integer_or_infinity_i64(ms) else {
    return Ok(Value::Number(f64::NAN));
  };

  let Some(day) = make_day(year, month, date) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(time) = make_time(hour, minute, second, millisecond) else {
    return Ok(Value::Number(f64::NAN));
  };
  let Some(ms) = make_date(day, time) else {
    return Ok(Value::Number(f64::NAN));
  };
  Ok(Value::Number(date_time_clip(ms as f64)))
}

/// `Date.prototype.toString`.
pub fn date_prototype_to_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let tv = date_this_time_value(scope, this)?;
  if !tv.is_finite() {
    return Ok(Value::String(scope.alloc_string("Invalid Date")?));
  }
  let s = date_alloc_string(scope, tv as i64)?;
  Ok(Value::String(s))
}

/// `Date.prototype.toUTCString`.
pub fn date_prototype_to_utc_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let tv = date_this_time_value(scope, this)?;
  if !tv.is_finite() {
    return Ok(Value::String(scope.alloc_string("Invalid Date")?));
  }
  let s = date_alloc_utc_string(scope, tv as i64)?;
  Ok(Value::String(s))
}

/// `Date.prototype.toISOString`.
pub fn date_prototype_to_iso_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let tv = date_this_time_value(scope, this)?;
  if !tv.is_finite() {
    let intr = require_intrinsics(vm)?;
    let err = crate::new_range_error(scope, intr, "Invalid time value")?;
    return Err(VmError::Throw(err));
  }
  let s = date_alloc_iso_string(scope, tv as i64)?;
  Ok(Value::String(s))
}

/// `Date.prototype.getTime`.
pub fn date_prototype_get_time(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  date_prototype_value_of(_vm, scope, _host, _hooks, _callee, this, _args)
}

/// `Date.prototype.valueOf`.
pub fn date_prototype_value_of(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let tv = date_this_time_value(scope, this)?;
  Ok(Value::Number(tv))
}

/// `Date.prototype[Symbol.toPrimitive]`.
pub fn date_prototype_to_primitive(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "Date.prototype[Symbol.toPrimitive] called on non-object",
    ));
  };

  let hint = match args.get(0).copied() {
    Some(Value::String(s)) => s,
    _ => return Err(VmError::TypeError("Invalid hint")),
  };

  let units = scope.heap().get_string(hint)?.as_code_units();
  let hint = if units
    == [
      b'd' as u16,
      b'e' as u16,
      b'f' as u16,
      b'a' as u16,
      b'u' as u16,
      b'l' as u16,
      b't' as u16,
    ]
  {
    crate::ToPrimitiveHint::String
  } else if units
    == [
      b's' as u16,
      b't' as u16,
      b'r' as u16,
      b'i' as u16,
      b'n' as u16,
      b'g' as u16,
    ]
  {
    crate::ToPrimitiveHint::String
  } else if units
    == [
      b'n' as u16,
      b'u' as u16,
      b'm' as u16,
      b'b' as u16,
      b'e' as u16,
      b'r' as u16,
    ]
  {
    crate::ToPrimitiveHint::Number
  } else {
    return Err(VmError::TypeError("Invalid hint"));
  };

  scope.ordinary_to_primitive(vm, host, hooks, obj, hint)
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

/// `Symbol.for(key)`.
pub fn symbol_for(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let key_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let key = scope.to_string(vm, host, hooks, key_val)?;
  let sym = scope.heap_mut().symbol_for_with_tick(key, || vm.tick())?;
  Ok(Value::Symbol(sym))
}

/// `Symbol.keyFor(sym)`.
pub fn symbol_key_for(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let sym_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Symbol(sym) = sym_val else {
    return Err(VmError::TypeError("Symbol.keyFor argument is not a symbol"));
  };
  let key = scope.heap().symbol_key_for(sym)?;
  Ok(match key {
    Some(k) => Value::String(k),
    None => Value::Undefined,
  })
}

fn concat_with_colon_space(
  name: &[u16],
  message: &[u16],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<Vec<u16>, VmError> {
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve(name.len().saturating_add(2).saturating_add(message.len()))
    .map_err(|_| VmError::OutOfMemory)?;
  vec_try_extend_from_slice(&mut out, name, || tick())?;
  vec_try_push(&mut out, b':' as u16)?;
  vec_try_push(&mut out, b' ' as u16)?;
  vec_try_extend_from_slice(&mut out, message, || tick())?;
  Ok(out)
}

/// `Error.prototype.toString` (ECMA-262).
pub fn error_prototype_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Spec: https://tc39.es/ecma262/#sec-error.prototype.tostring
  //
  // 1. Let O be ? ToObject(this value).
  // 2. Let name be ? Get(O, "name").
  // 3. If name is undefined, set name to "Error".
  // 4. Else, set name to ? ToString(name).
  // 5. Let msg be ? Get(O, "message").
  // 6. If msg is undefined, set msg to the empty String.
  // 7. Else, set msg to ? ToString(msg).
  // 8. If name is the empty String, return msg.
  // 9. If msg is the empty String, return name.
  // 10. Return the string-concatenation of name, ": ", and msg.
  let mut scope = scope.reborrow();

  let o = scope.to_object(vm, host, hooks, this)?;
  let receiver = Value::Object(o);
  // Root `O` while performing property gets and allocating output strings.
  scope.push_root(receiver)?;

  let name_key = string_key(&mut scope, "name")?;
  let message_key = string_key(&mut scope, "message")?;

  // `Get(O, "name")`: must be Proxy-aware and invoke accessors.
  let name_value = scope.get_with_host_and_hooks(vm, host, hooks, o, name_key, receiver)?;
  scope.push_root(name_value)?;

  let name = match name_value {
    Value::Undefined => scope.alloc_string("Error")?,
    other => scope.to_string(vm, host, hooks, other)?,
  };
  scope.push_root(Value::String(name))?;

  // `Get(O, "message")`: must be Proxy-aware and invoke accessors.
  let message_value = scope.get_with_host_and_hooks(vm, host, hooks, o, message_key, receiver)?;
  scope.push_root(message_value)?;

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

  let out = concat_with_colon_space(name_units, message_units, || vm.tick())?;
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
    let mut tick = || vm.tick();
    match crate::ops::parse_ascii_decimal_to_f64_units(&self.units[start..end], &mut tick)? {
      Some(n) => Ok(n),
      None => Err(json_syntax_error(vm, scope)),
    }
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

        let key_s = alloc_string_from_usize(&mut el_scope, idx)?;
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
    let slice = js.as_code_units();
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    vec_try_extend_from_slice(&mut units, slice, || vm.tick())?;
    units
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
    scope.push_roots(&[Value::Object(holder), Value::String(name), reviver])?;

    let key = PropertyKey::from_string(name);
    let mut val =
      scope.ordinary_get_with_host_and_hooks(vm, host, hooks, holder, key, Value::Object(holder))?;
    scope.push_root(val)?;

    if let Value::Object(obj) = val {
      if crate::spec_ops::is_array_with_host_and_hooks(vm, &mut scope, host, hooks, val)? {
        let len = length_of_array_like_usize(vm, &mut scope, host, hooks, obj)?;

        for i in 0..len {
          if i % 1024 == 0 {
            vm.tick()?;
          }
          let mut idx_scope = scope.reborrow();
          idx_scope.push_root(Value::Object(obj))?;

          let idx_s = alloc_string_from_usize(&mut idx_scope, i)?;
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
          p_scope.push_roots(&[Value::Object(obj), Value::String(p)])?;

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

    fn push_units(
      &mut self,
      units: &[u16],
      tick: impl FnMut() -> Result<(), VmError>,
    ) -> Result<(), VmError> {
      self.check_grow(units.len())?;
      vec_try_extend_from_slice(&mut self.buf, units, tick)
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
      self.push_units(&digits, || Ok(()))
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
    scope.push_roots(&[Value::Object(holder), Value::String(key)])?;
    if let Some(replacer_fn) = state.replacer_function {
      scope.push_root(replacer_fn)?;
    }

    let mut value = crate::spec_ops::internal_get_with_host_and_hooks(
      vm,
      &mut scope,
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
      let to_json = crate::spec_ops::internal_get_with_host_and_hooks(
        vm,
        &mut scope,
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
        out.push_units(units, || vm.tick())
      }
      Value::String(s) => quote_json_string(vm, scope, out, s),
      Value::Object(obj) => {
        if crate::spec_ops::is_array_with_host_and_hooks(vm, scope, host, hooks, Value::Object(obj))? {
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
    vec_try_extend_from_slice(&mut state.indent, &state.gap, || Ok(()))?;

    // len = ToLength(Get(array, "length"))
    let len_value = crate::spec_ops::internal_get_with_host_and_hooks(
      vm,
      scope,
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
        let mut el_scope = scope.reborrow();
        let key_s = alloc_string_from_usize(&mut el_scope, i)?;
        el_scope.push_root(Value::String(key_s))?;
        let element =
          prepare_json_value_for_property(vm, &mut el_scope, host, hooks, state, obj, key_s)?
            .unwrap_or(Value::Null);
        el_scope.push_root(element)?;
        serialize_json_value(vm, &mut el_scope, host, hooks, state, out, element)?;
      }
      out.push_unit(b']' as u16)?;
    } else {
      out.push_unit(b'\n' as u16)?;
      out.push_units(&state.indent, || vm.tick())?;
      for i in 0..len {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        if i > 0 {
          out.push_ascii(b",\n")?;
          out.push_units(&state.indent, || vm.tick())?;
        }
        let mut el_scope = scope.reborrow();
        let key_s = alloc_string_from_usize(&mut el_scope, i)?;
        el_scope.push_root(Value::String(key_s))?;
        let element =
          prepare_json_value_for_property(vm, &mut el_scope, host, hooks, state, obj, key_s)?
            .unwrap_or(Value::Null);
        el_scope.push_root(element)?;
        serialize_json_value(vm, &mut el_scope, host, hooks, state, out, element)?;
      }
      out.push_unit(b'\n' as u16)?;
      out.push_units(&state.indent[..stepback_len], || vm.tick())?;
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
    vec_try_extend_from_slice(&mut state.indent, &state.gap, || Ok(()))?;

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
        let mut p_scope = scope.reborrow();
        p_scope.push_root(Value::String(p))?;
        let Some(prop_value) =
          prepare_json_value_for_property(vm, &mut p_scope, host, hooks, state, obj, p)?
        else {
          continue;
        };
        p_scope.push_root(prop_value)?;
        if wrote_any {
          out.push_unit(b',' as u16)?;
        }
        wrote_any = true;
        quote_json_string(vm, &mut p_scope, out, p)?;
        out.push_unit(b':' as u16)?;
        serialize_json_value(vm, &mut p_scope, host, hooks, state, out, prop_value)?;
      }
      out.push_unit(b'}' as u16)?;
    } else {
      let mut wrote_any = false;
      for (i, p) in k_list.into_iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let mut p_scope = scope.reborrow();
        p_scope.push_root(Value::String(p))?;
        let Some(prop_value) =
          prepare_json_value_for_property(vm, &mut p_scope, host, hooks, state, obj, p)?
        else {
          continue;
        };
        p_scope.push_root(prop_value)?;
        if !wrote_any {
          out.push_unit(b'\n' as u16)?;
          out.push_units(&state.indent, || vm.tick())?;
        } else {
          out.push_ascii(b",\n")?;
          out.push_units(&state.indent, || vm.tick())?;
        }
        wrote_any = true;
        quote_json_string(vm, &mut p_scope, out, p)?;
        out.push_ascii(b": ")?;
        serialize_json_value(vm, &mut p_scope, host, hooks, state, out, prop_value)?;
      }
      if wrote_any {
        out.push_unit(b'\n' as u16)?;
        out.push_units(&state.indent[..stepback_len], || vm.tick())?;
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
    if crate::spec_ops::is_array_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      Value::Object(replacer_obj),
    )? {
      // propertyList from replacer array.
      let length_key_s = scope.alloc_string("length")?;
      scope.push_root(Value::String(length_key_s))?;
      let length_key = PropertyKey::from_string(length_key_s);
      let len_value = crate::spec_ops::internal_get_with_host_and_hooks(
        vm,
        &mut scope,
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
        let idx_s = alloc_string_from_usize(&mut scope, i)?;
        scope.push_root(Value::String(idx_s))?;
        let v = crate::spec_ops::internal_get_with_host_and_hooks(
          vm,
          &mut scope,
          host,
          hooks,
          replacer_obj,
          PropertyKey::from_string(idx_s),
          Value::Object(replacer_obj),
        )?;
        scope.push_root(v)?;

        let mut needs_root = false;
        let item_string: Option<crate::GcString> = match v {
          Value::String(s) => Some(s),
          Value::Number(n) => {
            needs_root = true;
            Some(scope.heap_mut().to_string(Value::Number(n))?)
          }
          Value::Object(o) => match unbox_primitive_wrapper(&mut scope, o, wrapper_markers)? {
            Some(Value::String(s)) => Some(s),
            Some(Value::Number(n)) => {
              needs_root = true;
              Some(scope.heap_mut().to_string(Value::Number(n))?)
            }
            _ => None,
          },
          _ => None,
        };

        let Some(s) = item_string else { continue };

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
          if needs_root {
            // Strings created via `ToString(number)` are not referenced by the JS heap, so keep them
            // alive for the duration of the stringify operation.
            scope.push_root(Value::String(s))?;
          }
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

pub fn weak_map_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("WeakMap constructor requires 'new'"))
}

pub fn weak_map_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let intr = require_intrinsics(vm)?;

  let map = scope.alloc_weak_map_with_prototype(Some(intr.weak_map_prototype()))?;
  scope.push_root(Value::Object(map))?;

  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(iterable, Value::Undefined | Value::Null) {
    return Ok(Value::Object(map));
  }

  // AddEntriesFromIterable (ECMA-262) (minimal).
  scope.push_root(iterable)?;

  // adder = Get(map, "set")
  let set_key = string_key(&mut scope, "set")?;
  let adder =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, map, set_key, Value::Object(map))?;
  if !scope.heap().is_callable(adder)? {
    return Err(VmError::TypeError("WeakMap constructor set method is not callable"));
  }
  scope.push_root(adder)?;

  // iteratorRecord = GetIterator(iterable)
  let mut iterator_record = crate::iterator::get_iterator(vm, host, hooks, &mut scope, iterable)?;
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let result: Result<(), VmError> = (|| {
    loop {
      let next_value =
        crate::iterator::iterator_step_value(vm, host, hooks, &mut scope, &mut iterator_record)?;
      let Some(next_value) = next_value else {
        return Ok(());
      };

      // Use a nested scope so per-entry roots do not accumulate.
      let mut step_scope = scope.reborrow();
      step_scope.push_root(next_value)?;

      let Value::Object(entry_obj) = next_value else {
        return Err(VmError::TypeError("WeakMap constructor: iterator value is not an object"));
      };

      let zero_key = string_key(&mut step_scope, "0")?;
      // Spec: `AddEntriesFromIterable` reads entry values via `Get(entry, "0")` / `Get(entry, "1")`,
      // which must be Proxy-aware.
      let key = step_scope.get_with_host_and_hooks(
        vm,
        host,
        hooks,
        entry_obj,
        zero_key,
        next_value,
      )?;
      step_scope.push_root(key)?;

      let one_key = string_key(&mut step_scope, "1")?;
      let value = step_scope.get_with_host_and_hooks(
        vm,
        host,
        hooks,
        entry_obj,
        one_key,
        next_value,
      )?;
      step_scope.push_root(value)?;

      let _ = vm.call_with_host_and_hooks(
        host,
        &mut step_scope,
        hooks,
        adder,
        Value::Object(map),
        &[key, value],
      )?;
    }
  })();

  match result {
    Ok(()) => Ok(Value::Object(map)),
    Err(err) => {
      if !iterator_record.done {
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = err.is_throw_completion();
        let pending_root = err
          .thrown_value()
          .map(|v| scope.heap_mut().add_root(v))
          .transpose()?;
        let close_res = crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          &mut scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        );
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
        if let Err(close_err) = close_res {
          // Only propagate close errors for non-catchable failures; otherwise preserve the original
          // throw completion.
          if original_is_throw && !close_err.is_throw_completion() {
            return Err(close_err);
          }
        }
      }
      Err(err)
    }
  }
}

pub fn weak_set_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("WeakSet constructor requires 'new'"))
}

pub fn weak_set_constructor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let intr = require_intrinsics(vm)?;

  let set = scope.alloc_weak_set_with_prototype(Some(intr.weak_set_prototype()))?;
  scope.push_root(Value::Object(set))?;

  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(iterable, Value::Undefined | Value::Null) {
    return Ok(Value::Object(set));
  }

  // AddEntriesFromIterable (ECMA-262) (minimal).
  scope.push_root(iterable)?;

  // adder = Get(set, "add")
  let add_key = string_key(&mut scope, "add")?;
  let adder =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, set, add_key, Value::Object(set))?;
  if !scope.heap().is_callable(adder)? {
    return Err(VmError::TypeError("WeakSet constructor add method is not callable"));
  }
  scope.push_root(adder)?;

  // iteratorRecord = GetIterator(iterable)
  let mut iterator_record = crate::iterator::get_iterator(vm, host, hooks, &mut scope, iterable)?;
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  let result: Result<(), VmError> = (|| {
    loop {
      let next_value =
        crate::iterator::iterator_step_value(vm, host, hooks, &mut scope, &mut iterator_record)?;
      let Some(next_value) = next_value else {
        return Ok(());
      };

      // Use a nested scope so per-entry roots do not accumulate.
      let mut step_scope = scope.reborrow();
      step_scope.push_root(next_value)?;
      let _ = vm.call_with_host_and_hooks(
        host,
        &mut step_scope,
        hooks,
        adder,
        Value::Object(set),
        &[next_value],
      )?;
    }
  })();

  match result {
    Ok(()) => Ok(Value::Object(set)),
    Err(err) => {
      if !iterator_record.done {
        // Per ECMA-262 `IteratorClose`, if the original completion is a *throw completion*, errors
        // produced while getting/calling `iterator.return` are suppressed. However, vm-js also has
        // non-catchable VM failures (termination, OOM, etc) which must never be suppressed.
        let original_is_throw = err.is_throw_completion();
        let pending_root = err
          .thrown_value()
          .map(|v| scope.heap_mut().add_root(v))
          .transpose()?;
        let close_res = crate::iterator::iterator_close(
          vm,
          host,
          hooks,
          &mut scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::Throw,
        );
        if let Some(root) = pending_root {
          scope.heap_mut().remove_root(root);
        }
        if let Err(close_err) = close_res {
          // Only propagate close errors for non-catchable failures; otherwise preserve the original
          // throw completion.
          if original_is_throw && !close_err.is_throw_completion() {
            return Err(close_err);
          }
        }
      }
      Err(err)
    }
  }
}

pub fn weak_map_prototype_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let scope = scope.reborrow();
  let Value::Object(map) = this else {
    return Err(VmError::TypeError("WeakMap.prototype.get called on non-object"));
  };
  if !scope.heap().is_weak_map_object(map) {
    return Err(VmError::TypeError(
      "WeakMap.prototype.get called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Ok(Value::Undefined);
  };

  Ok(scope
    .heap()
    .weak_map_get(map, key_obj)?
    .unwrap_or(Value::Undefined))
}

pub fn weak_map_prototype_set(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let Value::Object(map) = this else {
    return Err(VmError::TypeError("WeakMap.prototype.set called on non-object"));
  };
  if !scope.heap().is_weak_map_object(map) {
    return Err(VmError::TypeError(
      "WeakMap.prototype.set called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let value = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Err(VmError::TypeError("WeakMap key must be an object"));
  };

  scope.heap_mut().weak_map_set(map, key_obj, value)?;
  Ok(Value::Object(map))
}

pub fn weak_map_prototype_has(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let scope = scope.reborrow();
  let Value::Object(map) = this else {
    return Err(VmError::TypeError("WeakMap.prototype.has called on non-object"));
  };
  if !scope.heap().is_weak_map_object(map) {
    return Err(VmError::TypeError(
      "WeakMap.prototype.has called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Ok(Value::Bool(false));
  };

  Ok(Value::Bool(scope.heap().weak_map_has(map, key_obj)?))
}

pub fn weak_map_prototype_delete(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let Value::Object(map) = this else {
    return Err(VmError::TypeError("WeakMap.prototype.delete called on non-object"));
  };
  if !scope.heap().is_weak_map_object(map) {
    return Err(VmError::TypeError(
      "WeakMap.prototype.delete called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Ok(Value::Bool(false));
  };

  Ok(Value::Bool(scope.heap_mut().weak_map_delete(map, key_obj)?))
}

pub fn weak_set_prototype_add(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let Value::Object(set) = this else {
    return Err(VmError::TypeError("WeakSet.prototype.add called on non-object"));
  };
  if !scope.heap().is_weak_set_object(set) {
    return Err(VmError::TypeError(
      "WeakSet.prototype.add called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Err(VmError::TypeError("WeakSet key must be an object"));
  };

  scope.heap_mut().weak_set_add(set, key_obj)?;
  Ok(Value::Object(set))
}

pub fn weak_set_prototype_has(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let scope = scope.reborrow();
  let Value::Object(set) = this else {
    return Err(VmError::TypeError("WeakSet.prototype.has called on non-object"));
  };
  if !scope.heap().is_weak_set_object(set) {
    return Err(VmError::TypeError(
      "WeakSet.prototype.has called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Ok(Value::Bool(false));
  };

  Ok(Value::Bool(scope.heap().weak_set_has(set, key_obj)?))
}

pub fn weak_set_prototype_delete(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let Value::Object(set) = this else {
    return Err(VmError::TypeError("WeakSet.prototype.delete called on non-object"));
  };
  if !scope.heap().is_weak_set_object(set) {
    return Err(VmError::TypeError(
      "WeakSet.prototype.delete called on incompatible receiver",
    ));
  }

  let key = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(key_obj) = key else {
    return Ok(Value::Bool(false));
  };

  Ok(Value::Bool(scope.heap_mut().weak_set_delete(set, key_obj)?))
}

#[cfg(test)]
mod date_tests {
  use crate::{Heap, HeapLimits, JsRuntime, Job, RealmId, Value, Vm, VmError, VmHostHooks, VmOptions};

  fn new_runtime() -> JsRuntime {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    JsRuntime::new(vm, heap).unwrap()
  }

  #[derive(Default)]
  struct DeterministicTimeHooks {
    now_ms: i64,
  }

  impl VmHostHooks for DeterministicTimeHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
    }

    fn host_current_time_millis(&mut self) -> f64 {
      let out = self.now_ms as f64;
      self.now_ms = self.now_ms.saturating_add(1);
      out
    }
  }

  #[test]
  fn date_epoch_string_formats() -> Result<(), VmError> {
    let mut rt = new_runtime();

    let v = rt.exec_script("new Date(0).toISOString()")?;
    let Value::String(s) = v else {
      return Err(VmError::Unimplemented("expected string"));
    };
    assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "1970-01-01T00:00:00.000Z");

    let v = rt.exec_script("new Date(0).toUTCString()")?;
    let Value::String(s) = v else {
      return Err(VmError::Unimplemented("expected string"));
    };
    assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "Thu, 01 Jan 1970 00:00:00 GMT");

    let v = rt.exec_script("new Date(0).toString()")?;
    let Value::String(s) = v else {
      return Err(VmError::Unimplemented("expected string"));
    };
    assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "Thu Jan 01 1970 00:00:00 GMT");

    let v = rt.exec_script("Date.parse('1970-01-01T00:00:00.000Z')")?;
    assert_eq!(v, Value::Number(0.0));

    let v = rt.exec_script("Date.UTC(1970, 0, 1, 0, 0, 0, 0)")?;
    assert_eq!(v, Value::Number(0.0));

    Ok(())
  }

  #[test]
  fn date_now_uses_host_time_hook() -> Result<(), VmError> {
    let mut rt = new_runtime();
    let mut hooks = DeterministicTimeHooks { now_ms: 1000 };

    let a = rt.exec_script_with_hooks(&mut hooks, "new Date().getTime()")?;
    let b = rt.exec_script_with_hooks(&mut hooks, "Date.now()")?;
    let c = rt.exec_script_with_hooks(&mut hooks, "Date.now()")?;
    assert_eq!(a, Value::Number(1000.0));
    assert_eq!(b, Value::Number(1001.0));
    assert_eq!(c, Value::Number(1002.0));
    Ok(())
  }

  #[test]
  fn date_unary_plus_matches_get_time() -> Result<(), VmError> {
    let mut rt = new_runtime();
    let v = rt.exec_script("const d = new Date(1234); +d === d.getTime()")?;
    assert_eq!(v, Value::Bool(true));
    Ok(())
  }
}

#[cfg(test)]
mod proxy_tests {
  use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

  fn new_runtime() -> JsRuntime {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    JsRuntime::new(vm, heap).unwrap()
  }

  #[test]
  fn proxy_constructor_is_not_callable() -> Result<(), VmError> {
    let mut rt = new_runtime();
    let v = rt.exec_script(
      r#"
        let threw = false;
        try { Proxy({}, {}); } catch (e) { threw = true; }
        threw
      "#,
    )?;
    assert_eq!(v, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn proxy_revocable_returns_proxy_and_revoke() -> Result<(), VmError> {
    let mut rt = new_runtime();
    let v = rt.exec_script(
      r#"
        const r = Proxy.revocable({}, {});
        typeof r.proxy === "object" && typeof r.revoke === "function"
      "#,
    )?;
    assert_eq!(v, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn proxy_revocable_revoke_revokes_proxy() -> Result<(), VmError> {
    let mut rt = new_runtime();
    let v = rt.exec_script(
      r#"
        const r = Proxy.revocable({}, {});
        r.revoke();
        let threw = false;
        try { Reflect.ownKeys(r.proxy); } catch (e) { threw = true; }
        threw
      "#,
    )?;
    assert_eq!(v, Value::Bool(true));
    Ok(())
  }
}
