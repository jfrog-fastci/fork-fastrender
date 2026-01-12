//! Minimal `FormData` implementation for `vm-js` Window realms.
//!
//! This is a spec-shaped MVP intended to unblock real-world scripts that build `FormData` payloads
//! for `fetch()`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use vm_js::{
  GcObject, Heap, HostSlots, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

use crate::js::window_blob::{self, BlobData};
use crate::js::{time, window_file};

const REALM_ID_SLOT: usize = 0;
const ITER_PROTO_SLOT: usize = 1;

const MAX_FORM_DATA_BYTES: usize = 10 * 1024 * 1024;

const FORM_DATA_HOST_TAG: u64 = 0x464F_524D_4441_5441; // "FORMDATA"
const FORM_DATA_ITERATOR_HOST_TAG: u64 = 0x4644_4954_4552_4154; // "FDITERAT"

#[derive(Clone, Debug)]
pub(crate) enum FormDataValue {
  String(String),
  File {
    data: BlobData,
    filename: String,
    last_modified: i64,
  },
}

#[derive(Clone, Debug)]
pub(crate) struct FormDataEntry {
  pub(crate) name: String,
  pub(crate) value: FormDataValue,
}

#[derive(Default)]
struct FormDataRegistry {
  realms: HashMap<RealmId, FormDataRealmState>,
}

struct FormDataRealmState {
  form_data_proto: GcObject,
  file_proto: Option<GcObject>,
  forms: HashMap<WeakGcObject, Vec<FormDataEntry>>,
  iterators: HashMap<WeakGcObject, FormDataIteratorState>,
  last_gc_runs: u64,
}

#[derive(Clone)]
struct FormDataIteratorState {
  items: Vec<FormDataEntry>,
  index: usize,
  kind: u8,
}

const ITER_KIND_ENTRIES: u8 = 0;
const ITER_KIND_KEYS: u8 = 1;
const ITER_KIND_VALUES: u8 = 2;

static REGISTRY: OnceLock<Mutex<FormDataRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<FormDataRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(FormDataRegistry::default()))
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn realm_id_from_slot(value: Value) -> Option<RealmId> {
  let Value::Number(n) = value else {
    return None;
  };
  if !n.is_finite() || n < 0.0 {
    return None;
  }
  let raw = n as u64;
  if raw as f64 != n {
    return None;
  }
  Some(RealmId::from_raw(raw))
}

fn realm_id_for_binding_call(
  vm: &Vm,
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }
  let slots = scope.heap().get_function_native_slots(callee)?;
  let realm_id = slots
    .get(REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "FormData bindings invoked without an active realm",
    ))?;
  Ok(realm_id)
}

fn iter_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(ITER_PROTO_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "FormData binding missing iterator prototype native slot",
    )),
  }
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut FormDataRealmState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "FormData bindings used before install_window_form_data_bindings",
    ))?;

  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    state.forms.retain(|k, _| k.upgrade(heap).is_some());
    state.iterators.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state)
}

fn require_form_data<'a>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<(GcObject, Vec<FormDataEntry>), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("FormData: illegal invocation"));
  };
  match scope.heap().object_host_slots(obj)? {
    Some(slots) if slots.a == FORM_DATA_HOST_TAG => {}
    _ => return Err(VmError::TypeError("FormData: illegal invocation")),
  };

  let entries = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .forms
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("FormData: illegal invocation"))
  })?;

  Ok((obj, entries))
}

fn form_data_total_bytes(entries: &[FormDataEntry]) -> Result<usize, VmError> {
  let mut total: usize = 0;
  for entry in entries {
    total = total
      .checked_add(entry.name.len())
      .ok_or(VmError::OutOfMemory)?;
    match &entry.value {
      FormDataValue::String(s) => {
        total = total.checked_add(s.len()).ok_or(VmError::OutOfMemory)?;
      }
      FormDataValue::File { data, filename, .. } => {
        total = total
          .checked_add(data.bytes.len())
          .and_then(|t| t.checked_add(filename.len()))
          .and_then(|t| t.checked_add(data.r#type.len()))
          .ok_or(VmError::OutOfMemory)?;
      }
    }
  }
  Ok(total)
}

fn js_value_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  err: &'static str,
) -> Result<String, VmError> {
  let s = scope.to_string(vm, host, hooks, value)?;
  let out = scope.heap().get_string(s)?.to_utf8_lossy();
  if out.len() > MAX_FORM_DATA_BYTES {
    return Err(VmError::TypeError(err));
  }
  Ok(out)
}

fn js_form_data_value_to_js(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  file_proto: Option<GcObject>,
  value: &FormDataValue,
) -> Result<Value, VmError> {
  match value {
    FormDataValue::String(s) => {
      let v = scope.alloc_string(s)?;
      Ok(Value::String(v))
    }
    FormDataValue::File {
      data,
      filename,
      last_modified,
    } => {
      let proto = file_proto.ok_or(VmError::Unimplemented(
        "FormData File values require File to be installed",
      ))?;
      let obj = window_file::create_file_with_proto(
        vm,
        scope,
        callee,
        proto,
        data.clone(),
        window_file::FileMeta {
          name: filename.clone(),
          last_modified: *last_modified,
        },
      )?;
      Ok(Value::Object(obj))
    }
  }
}

fn form_data_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("FormData constructor requires 'new'"))
}

fn form_data_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "FormData requires intrinsics (create a Realm first)",
  ))?;

  // Accept any argument and treat as empty for now (FastRender partial DOM is not extracted yet).
  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(proto) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FORM_DATA_HOST_TAG,
      b: 0,
    },
  )?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state.forms.insert(WeakGcObject::from(obj), Vec::new());
    Ok(())
  })?;

  Ok(Value::Object(obj))
}

fn form_data_append_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (form_obj, mut entries) = require_form_data(vm, scope, callee, this)?;

  let name = js_value_to_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "FormData entry name exceeds maximum length",
  )?;

  let value_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let file_meta = window_file::clone_file_metadata_for_object(vm, scope.heap(), value_val)?;
  let blob = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), value_val)?;
  let value = if let Some(blob) = blob {
    let filename = match args.get(2).copied() {
      None | Some(Value::Undefined) => file_meta
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "blob".to_string()),
      Some(v) => js_value_to_string(
        vm,
        scope,
        host,
        hooks,
        v,
        "FormData filename exceeds maximum length",
      )?,
    };
    let last_modified = file_meta
      .as_ref()
      .map(|m| m.last_modified)
      .unwrap_or(time::date_now_ms(scope)?);
    FormDataValue::File {
      data: blob,
      filename,
      last_modified,
    }
  } else {
    let s = js_value_to_string(
      vm,
      scope,
      host,
      hooks,
      value_val,
      "FormData entry value exceeds maximum length",
    )?;
    FormDataValue::String(s)
  };

  entries.push(FormDataEntry { name, value });
  if form_data_total_bytes(&entries)? > MAX_FORM_DATA_BYTES {
    return Err(VmError::TypeError("FormData exceeds maximum length"));
  }

  with_realm_state_mut(vm, scope, callee, |state| {
    state.forms.insert(WeakGcObject::from(form_obj), entries);
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn form_data_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (form_obj, mut entries) = require_form_data(vm, scope, callee, this)?;

  let name = js_value_to_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "FormData entry name exceeds maximum length",
  )?;

  let value_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let file_meta = window_file::clone_file_metadata_for_object(vm, scope.heap(), value_val)?;
  let blob = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), value_val)?;
  let value = if let Some(blob) = blob {
    let filename = match args.get(2).copied() {
      None | Some(Value::Undefined) => file_meta
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "blob".to_string()),
      Some(v) => js_value_to_string(
        vm,
        scope,
        host,
        hooks,
        v,
        "FormData filename exceeds maximum length",
      )?,
    };
    let last_modified = file_meta
      .as_ref()
      .map(|m| m.last_modified)
      .unwrap_or(time::date_now_ms(scope)?);
    FormDataValue::File {
      data: blob,
      filename,
      last_modified,
    }
  } else {
    let s = js_value_to_string(
      vm,
      scope,
      host,
      hooks,
      value_val,
      "FormData entry value exceeds maximum length",
    )?;
    FormDataValue::String(s)
  };

  if let Some(first) = entries.iter().position(|e| e.name == name) {
    entries[first].value = value;
    let mut i = first.saturating_add(1);
    while i < entries.len() {
      if entries.get(i).is_some_and(|e| e.name == name) {
        entries.remove(i);
      } else {
        i = i.saturating_add(1);
      }
    }
  } else {
    entries.push(FormDataEntry { name, value });
  }

  if form_data_total_bytes(&entries)? > MAX_FORM_DATA_BYTES {
    return Err(VmError::TypeError("FormData exceeds maximum length"));
  }

  with_realm_state_mut(vm, scope, callee, |state| {
    state.forms.insert(WeakGcObject::from(form_obj), entries);
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn form_data_delete_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (form_obj, mut entries) = require_form_data(vm, scope, callee, this)?;
  let name = js_value_to_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "FormData entry name exceeds maximum length",
  )?;
  entries.retain(|e| e.name != name);
  with_realm_state_mut(vm, scope, callee, |state| {
    state.forms.insert(WeakGcObject::from(form_obj), entries);
    Ok(())
  })?;
  Ok(Value::Undefined)
}

fn form_data_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_form_obj, entries) = require_form_data(vm, scope, callee, this)?;
  let name = js_value_to_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "FormData entry name exceeds maximum length",
  )?;

  let file_proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.file_proto))?;
  for entry in entries {
    if entry.name == name {
      return js_form_data_value_to_js(vm, scope, callee, file_proto, &entry.value);
    }
  }
  Ok(Value::Null)
}

fn form_data_get_all_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_form_obj, entries) = require_form_data(vm, scope, callee, this)?;
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "FormData.getAll requires intrinsics (create a Realm first)",
  ))?;
  let name = js_value_to_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "FormData entry name exceeds maximum length",
  )?;

  let file_proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.file_proto))?;
  let mut out: Vec<FormDataValue> = Vec::new();
  for entry in entries {
    if entry.name == name {
      out.push(entry.value);
    }
  }

  let arr = scope.alloc_array(out.len())?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;

  for (i, v) in out.iter().enumerate() {
    let js_val = js_form_data_value_to_js(vm, scope, callee, file_proto, v)?;
    let key = alloc_key(scope, &i.to_string())?;
    scope.ordinary_set(vm, arr, key, js_val, Value::Object(arr))?;
  }

  Ok(Value::Object(arr))
}

fn form_data_has_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_form_obj, entries) = require_form_data(vm, scope, callee, this)?;
  let name = js_value_to_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "FormData entry name exceeds maximum length",
  )?;
  Ok(Value::Bool(entries.iter().any(|e| e.name == name)))
}

fn form_data_for_each_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback).unwrap_or(false) {
    return Err(VmError::TypeError(
      "FormData.forEach callback is not callable",
    ));
  }

  let (form_obj, entries) = require_form_data(vm, scope, callee, this)?;
  let file_proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.file_proto))?;
  for entry in entries {
    let value = js_form_data_value_to_js(vm, scope, callee, file_proto, &entry.value)?;
    let name_s = scope.alloc_string(&entry.name)?;
    scope.push_root(Value::String(name_s))?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      hooks,
      callback,
      this_arg,
      &[value, Value::String(name_s), Value::Object(form_obj)],
    )?;
  }

  Ok(Value::Undefined)
}

fn make_iterator(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  this: Value,
  iter_proto: GcObject,
  kind: u8,
) -> Result<Value, VmError> {
  let (_form_obj, entries) = require_form_data(vm, scope, callee, this)?;

  let iter_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(iter_obj, Some(iter_proto))?;
  scope.heap_mut().object_set_host_slots(
    iter_obj,
    HostSlots {
      a: FORM_DATA_ITERATOR_HOST_TAG,
      b: kind as u64,
    },
  )?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state.iterators.insert(
      WeakGcObject::from(iter_obj),
      FormDataIteratorState {
        items: entries,
        index: 0,
        kind,
      },
    );
    Ok(())
  })?;

  Ok(Value::Object(iter_obj))
}

fn form_data_entries_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let iter_proto = iter_proto_from_callee(scope, callee)?;
  make_iterator(vm, scope, callee, this, iter_proto, ITER_KIND_ENTRIES)
}

fn form_data_keys_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let iter_proto = iter_proto_from_callee(scope, callee)?;
  make_iterator(vm, scope, callee, this, iter_proto, ITER_KIND_KEYS)
}

fn form_data_values_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let iter_proto = iter_proto_from_callee(scope, callee)?;
  make_iterator(vm, scope, callee, this, iter_proto, ITER_KIND_VALUES)
}

fn iterator_next_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "FormData iterator requires intrinsics (create a Realm first)",
  ))?;

  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError("FormData iterator: illegal invocation"));
  };
  match scope.heap().object_host_slots(iter_obj)? {
    Some(slots) if slots.a == FORM_DATA_ITERATOR_HOST_TAG => {}
    _ => return Err(VmError::TypeError("FormData iterator: illegal invocation")),
  };

  let result_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(result_obj))?;

  let (next, kind, file_proto) = with_realm_state_mut(vm, scope, callee, |state| {
    let iter = state
      .iterators
      .get_mut(&WeakGcObject::from(iter_obj))
      .ok_or(VmError::TypeError("FormData iterator: illegal invocation"))?;
    let kind = iter.kind;
    if iter.index >= iter.items.len() {
      return Ok((None, kind, state.file_proto));
    }
    let entry = iter
      .items
      .get(iter.index)
      .cloned()
      .ok_or(VmError::InvariantViolation(
        "FormData iterator index out of bounds",
      ))?;
    iter.index = iter.index.saturating_add(1);
    Ok((Some(entry), kind, state.file_proto))
  })?;

  let data_desc = |value| PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  };

  let value_key = alloc_key(scope, "value")?;
  let done_key = alloc_key(scope, "done")?;

  match next {
    None => {
      scope.define_property(result_obj, value_key, data_desc(Value::Undefined))?;
      scope.define_property(result_obj, done_key, data_desc(Value::Bool(true)))?;
      Ok(Value::Object(result_obj))
    }
    Some(entry) => {
      let out_value = match kind {
        ITER_KIND_ENTRIES => {
          let pair = scope.alloc_array(2)?;
          scope.push_root(Value::Object(pair))?;
          scope
            .heap_mut()
            .object_set_prototype(pair, Some(intr.array_prototype()))?;

          let name_s = scope.alloc_string(&entry.name)?;
          scope.push_root(Value::String(name_s))?;
          let k0 = alloc_key(scope, "0")?;
          scope.define_property(pair, k0, data_desc(Value::String(name_s)))?;

          let k1 = alloc_key(scope, "1")?;
          let value_js = js_form_data_value_to_js(vm, scope, callee, file_proto, &entry.value)?;
          scope.define_property(pair, k1, data_desc(value_js))?;

          Value::Object(pair)
        }
        ITER_KIND_KEYS => {
          let name_s = scope.alloc_string(&entry.name)?;
          Value::String(name_s)
        }
        ITER_KIND_VALUES => js_form_data_value_to_js(vm, scope, callee, file_proto, &entry.value)?,
        _ => return Err(VmError::InvariantViolation("FormData iterator invalid kind")),
      };

      scope.define_property(result_obj, value_key, data_desc(out_value))?;
      scope.define_property(result_obj, done_key, data_desc(Value::Bool(false)))?;
      Ok(Value::Object(result_obj))
    }
  }
}

fn iterator_symbol_iterator_native(
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

pub fn install_window_form_data_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let realm_id = realm.id();

  let file_proto = window_file::file_prototype_for_realm(realm_id);

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let call_id: NativeFunctionId = vm.register_native_call(form_data_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(form_data_ctor_construct)?;

  let name = scope.alloc_string("FormData")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(intr.function_prototype()))?;

  let proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "FormData constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(intr.object_prototype()))?;

  // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
  let tag_value = scope.alloc_string("FormData")?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(
    proto,
    PropertyKey::from_symbol(intr.well_known_symbols().to_string_tag),
    data_desc(Value::String(tag_value), false),
  )?;

  // Iterator prototype (shared by entries/keys/values).
  let iter_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(iter_proto, Some(intr.object_prototype()))?;

  let next_call_id: NativeFunctionId = vm.register_native_call(iterator_next_native)?;
  let next_name = scope.alloc_string("next")?;
  scope.push_root(Value::String(next_name))?;
  let next_fn = scope.alloc_native_function_with_slots(
    next_call_id,
    None,
    next_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(next_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(next_fn, Some(intr.function_prototype()))?;
  let next_key = alloc_key(&mut scope, "next")?;
  scope.define_property(
    iter_proto,
    next_key,
    data_desc(Value::Object(next_fn), true),
  )?;

  let sym_iter_call_id: NativeFunctionId =
    vm.register_native_call(iterator_symbol_iterator_native)?;
  let sym_iter_name = scope.alloc_string("[Symbol.iterator]")?;
  scope.push_root(Value::String(sym_iter_name))?;
  let sym_iter_fn = scope.alloc_native_function_with_slots(
    sym_iter_call_id,
    None,
    sym_iter_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(sym_iter_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(sym_iter_fn, Some(intr.function_prototype()))?;
  let iter_sym = intr.well_known_symbols().iterator;
  let iter_key = PropertyKey::from_symbol(iter_sym);
  scope.define_property(
    iter_proto,
    iter_key,
    data_desc(Value::Object(sym_iter_fn), true),
  )?;

  // --- FormData prototype methods -------------------------------------------
  let append_call_id: NativeFunctionId = vm.register_native_call(form_data_append_native)?;
  let append_name = scope.alloc_string("append")?;
  scope.push_root(Value::String(append_name))?;
  let append_fn = scope.alloc_native_function_with_slots(
    append_call_id,
    None,
    append_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(append_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(append_fn, Some(intr.function_prototype()))?;
  let append_key = alloc_key(&mut scope, "append")?;
  scope.define_property(proto, append_key, data_desc(Value::Object(append_fn), true))?;

  let set_call_id: NativeFunctionId = vm.register_native_call(form_data_set_native)?;
  let set_name = scope.alloc_string("set")?;
  scope.push_root(Value::String(set_name))?;
  let set_fn = scope.alloc_native_function_with_slots(
    set_call_id,
    None,
    set_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(set_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(set_fn, Some(intr.function_prototype()))?;
  let set_key = alloc_key(&mut scope, "set")?;
  scope.define_property(proto, set_key, data_desc(Value::Object(set_fn), true))?;

  let delete_call_id: NativeFunctionId = vm.register_native_call(form_data_delete_native)?;
  let delete_name = scope.alloc_string("delete")?;
  scope.push_root(Value::String(delete_name))?;
  let delete_fn = scope.alloc_native_function_with_slots(
    delete_call_id,
    None,
    delete_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(delete_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(delete_fn, Some(intr.function_prototype()))?;
  let delete_key = alloc_key(&mut scope, "delete")?;
  scope.define_property(proto, delete_key, data_desc(Value::Object(delete_fn), true))?;

  let get_call_id: NativeFunctionId = vm.register_native_call(form_data_get_native)?;
  let get_name = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_name))?;
  let get_fn = scope.alloc_native_function_with_slots(
    get_call_id,
    None,
    get_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(get_fn, Some(intr.function_prototype()))?;
  let get_key = alloc_key(&mut scope, "get")?;
  scope.define_property(proto, get_key, data_desc(Value::Object(get_fn), true))?;

  let get_all_call_id: NativeFunctionId = vm.register_native_call(form_data_get_all_native)?;
  let get_all_name = scope.alloc_string("getAll")?;
  scope.push_root(Value::String(get_all_name))?;
  let get_all_fn = scope.alloc_native_function_with_slots(
    get_all_call_id,
    None,
    get_all_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(get_all_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(get_all_fn, Some(intr.function_prototype()))?;
  let get_all_key = alloc_key(&mut scope, "getAll")?;
  scope.define_property(
    proto,
    get_all_key,
    data_desc(Value::Object(get_all_fn), true),
  )?;

  let has_call_id: NativeFunctionId = vm.register_native_call(form_data_has_native)?;
  let has_name = scope.alloc_string("has")?;
  scope.push_root(Value::String(has_name))?;
  let has_fn = scope.alloc_native_function_with_slots(
    has_call_id,
    None,
    has_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(has_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(has_fn, Some(intr.function_prototype()))?;
  let has_key = alloc_key(&mut scope, "has")?;
  scope.define_property(proto, has_key, data_desc(Value::Object(has_fn), true))?;

  let for_each_call_id: NativeFunctionId = vm.register_native_call(form_data_for_each_native)?;
  let for_each_name = scope.alloc_string("forEach")?;
  scope.push_root(Value::String(for_each_name))?;
  let for_each_fn = scope.alloc_native_function_with_slots(
    for_each_call_id,
    None,
    for_each_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(for_each_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(for_each_fn, Some(intr.function_prototype()))?;
  let for_each_key = alloc_key(&mut scope, "forEach")?;
  scope.define_property(
    proto,
    for_each_key,
    data_desc(Value::Object(for_each_fn), true),
  )?;

  let entries_call_id: NativeFunctionId = vm.register_native_call(form_data_entries_native)?;
  let entries_name = scope.alloc_string("entries")?;
  scope.push_root(Value::String(entries_name))?;
  let entries_fn = scope.alloc_native_function_with_slots(
    entries_call_id,
    None,
    entries_name,
    0,
    &[
      Value::Number(realm_id.to_raw() as f64),
      Value::Object(iter_proto),
    ],
  )?;
  scope.push_root(Value::Object(entries_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(entries_fn, Some(intr.function_prototype()))?;
  let entries_key = alloc_key(&mut scope, "entries")?;
  scope.define_property(
    proto,
    entries_key,
    data_desc(Value::Object(entries_fn), true),
  )?;

  let keys_call_id: NativeFunctionId = vm.register_native_call(form_data_keys_native)?;
  let keys_name = scope.alloc_string("keys")?;
  scope.push_root(Value::String(keys_name))?;
  let keys_fn = scope.alloc_native_function_with_slots(
    keys_call_id,
    None,
    keys_name,
    0,
    &[
      Value::Number(realm_id.to_raw() as f64),
      Value::Object(iter_proto),
    ],
  )?;
  scope.push_root(Value::Object(keys_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(keys_fn, Some(intr.function_prototype()))?;
  let keys_key = alloc_key(&mut scope, "keys")?;
  scope.define_property(proto, keys_key, data_desc(Value::Object(keys_fn), true))?;

  let values_call_id: NativeFunctionId = vm.register_native_call(form_data_values_native)?;
  let values_name = scope.alloc_string("values")?;
  scope.push_root(Value::String(values_name))?;
  let values_fn = scope.alloc_native_function_with_slots(
    values_call_id,
    None,
    values_name,
    0,
    &[
      Value::Number(realm_id.to_raw() as f64),
      Value::Object(iter_proto),
    ],
  )?;
  scope.push_root(Value::Object(values_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(values_fn, Some(intr.function_prototype()))?;
  let values_key = alloc_key(&mut scope, "values")?;
  scope.define_property(proto, values_key, data_desc(Value::Object(values_fn), true))?;

  let sym_iter_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    proto,
    sym_iter_key,
    data_desc(Value::Object(entries_fn), true),
  )?;

  let ctor_key = alloc_key(&mut scope, "FormData")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    FormDataRealmState {
      form_data_proto: proto,
      file_proto,
      forms: HashMap::new(),
      iterators: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_form_data_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.remove(&realm_id);
}

pub(crate) fn create_form_data_with_entries(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  entries: Vec<FormDataEntry>,
) -> Result<GcObject, VmError> {
  if form_data_total_bytes(&entries)? > MAX_FORM_DATA_BYTES {
    return Err(VmError::TypeError("FormData exceeds maximum length"));
  }

  let proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.form_data_proto))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FORM_DATA_HOST_TAG,
      b: 0,
    },
  )?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state.forms.insert(WeakGcObject::from(obj), entries);
    Ok(())
  })?;

  Ok(obj)
}

pub(crate) fn clone_form_data_entries_for_fetch(
  vm: &Vm,
  heap: &Heap,
  value: Value,
) -> Result<Option<Vec<FormDataEntry>>, VmError> {
  let Value::Object(obj) = value else {
    return Ok(None);
  };
  let Some(realm_id) = vm.current_realm() else {
    return Ok(None);
  };

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let Some(state) = registry.realms.get_mut(&realm_id) else {
    return Ok(None);
  };

  let gc_runs = heap.gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    state.forms.retain(|k, _| k.upgrade(heap).is_some());
    state.iterators.retain(|k, _| k.upgrade(heap).is_some());
  }

  Ok(state.forms.get(&WeakGcObject::from(obj)).cloned())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use crate::js::WebTime;
  use std::sync::Arc;
  use std::time::Duration;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn object_prototype_to_string_uses_form_data_to_string_tag() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let v = realm.exec_script("Object.prototype.toString.call(new FormData())")?;
    assert_eq!(get_string(realm.heap(), v), "[object FormData]");
    realm.teardown();
    Ok(())
  }

  #[test]
  fn form_data_basic_methods_and_iteration_order() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script(
      "(() => {\
       const fd = new FormData();\
       fd.append('a', '1');\
       fd.append('b', '2');\
       fd.append('a', '3');\
       return fd.get('a');\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), v), "1");

    let v = realm.exec_script(
      "(() => {\
       const fd = new FormData();\
       fd.append('a', '1');\
       fd.append('a', '2');\
       return fd.getAll('a').join(',');\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), v), "1,2");

    let v = realm.exec_script(
      "(() => {\
       const fd = new FormData();\
       fd.append('a', '1');\
       fd.set('a', '2');\
       return fd.getAll('a').join(',');\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), v), "2");

    let v = realm.exec_script(
      "(() => {\
       const fd = new FormData();\
       fd.append('a', '1');\
       fd.append('b', '2');\
       fd.append('a', '3');\
       const out = [];\
       for (var pair of fd) { out.push(pair[0] + '=' + pair[1]); }\
       return out.join('&');\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), v), "a=1&b=2&a=3");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn form_data_blob_values_are_files_with_deterministic_last_modified() -> Result<(), VmError> {
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(1234));
    let config = WindowRealmConfig::new("https://example.com/")
      .with_clock(clock)
      .with_web_time(WebTime::new(1000));
    let mut realm = WindowRealm::new(config)?;

    let v = realm.exec_script(
      "(() => {\
        Date.now = () => 5;\
        const fd = new FormData();\
        fd.append('file', new Blob(['hi'], { type: 'text/plain' }), 'f.txt');\
        return fd.get('file').lastModified;\
      })()",
    )?;
    assert_eq!(v, Value::Number(2234.0));

    realm.teardown();
    Ok(())
  }
}
