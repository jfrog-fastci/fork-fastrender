//! Helpers for installing WebIDL-shaped bindings into a `vm-js` Realm.
//!
//! The higher-level binding generator (`xtask webidl-bindings`) targets `vm-js` by emitting
//! `NativeCall` / `NativeConstruct` handlers and using these helpers to allocate the corresponding
//! function objects with the correct `[[Call]]`/`[[Construct]]` split.

use vm_js::{GcObject, NativeCall, NativeConstruct, Realm, Scope, Value, Vm, VmError};

/// Allocate a native `vm-js` function object with explicit `[[Call]]` and optional `[[Construct]]`
/// handlers.
///
/// This helper:
/// - registers the provided handlers in `vm`,
/// - allocates a native function object in `scope`, and
/// - ensures the allocated function inherits from `%Function.prototype%`.
pub fn alloc_native_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  name: &str,
  length: u32,
  call: NativeCall,
  construct: Option<NativeConstruct>,
) -> Result<GcObject, VmError> {
  let call_id = vm.register_native_call(call)?;
  let construct_id = match construct {
    None => None,
    Some(f) => Some(vm.register_native_construct(f)?),
  };

  let name_s = scope.alloc_string(name)?;
  // Root the name string across the function allocation (which may GC).
  scope.push_root(Value::String(name_s))?;

  let func = scope.alloc_native_function(call_id, construct_id, name_s, length)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  Ok(func)
}

/// Convenience wrapper around [`alloc_native_function`] for constructable functions.
pub fn alloc_constructor_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  name: &str,
  length: u32,
  call: NativeCall,
  construct: NativeConstruct,
) -> Result<GcObject, VmError> {
  alloc_native_function(vm, scope, realm, name, length, call, Some(construct))
}
