//! HTML structured cloning API: `structuredClone(value, options?)`.
//!
//! This is a spec-shaped MVP intended to support real-world libraries that depend on structured
//! cloning for deep-copying data structures and for transferring `ArrayBuffer` backing stores.
//!
//! Supported (and tested) types:
//! - primitives (except `symbol`)
//! - Array
//! - ordinary objects (MVP: enumerable own *string* keys only)
//! - ArrayBuffer (copy or transfer)
//! - Uint8Array (clones underlying ArrayBuffer and re-creates the view)
//! - Date
//! - Blob (FastRender host implementation)
//!
//! Unsupported values throw a `DataCloneError` DOMException.

use crate::js::bindings::DomExceptionClassVmJs;
use crate::js::window_blob::{
  blob_prototype_for_realm, clone_blob_data_for_fetch, create_blob_with_proto, BlobData,
};
use crate::js::window_realm::make_dom_exception;
use std::collections::{HashMap, HashSet};
use vm_js::{
  GcObject, GcString, GcSymbol, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId,
  Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

/// Native slot index for the `structuredClone` function's realm global object.
pub(crate) const STRUCTURED_CLONE_GLOBAL_SLOT: usize = 0;
/// Native slot index for the `structuredClone` function's realm ID.
pub(crate) const STRUCTURED_CLONE_REALM_ID_SLOT: usize = 1;

// --- DoS resistance limits ---
//
// These are not spec-defined; they are hard caps to keep structured cloning safe under hostile
// inputs.
const MAX_RECURSION_DEPTH: usize = 256;
const MAX_VISITED_NODES: usize = 100_000;
const MAX_ENUMERABLE_PROPS: usize = 1_000_000;
const MAX_COPIED_BYTES: usize = 32 * 1024 * 1024; // 32MiB (copied ArrayBuffer/Blob bytes only)

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
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

pub(crate) fn install_window_structured_clone(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  global: GcObject,
) -> Result<(), VmError> {
  // Install `Object.getOwnPropertySymbols` if the engine doesn't provide it yet.
  //
  // `structuredClone` (correctly) ignores enumerable symbol-keyed properties per
  // `EnumerableOwnProperties` semantics. Tests validate this behaviour.
  {
    let object_ctor = realm.intrinsics().object_constructor();
    scope.push_root(Value::Object(object_ctor))?;
    let key = alloc_key(scope, "getOwnPropertySymbols")?;
    if scope
      .heap()
      .object_get_own_property(object_ctor, &key)?
      .is_none()
    {
      let call_id = vm.register_native_call(object_get_own_property_symbols_native)?;
      let name_s = scope.alloc_string("getOwnPropertySymbols")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(call_id, None, name_s, 1)?;
      scope.heap_mut().object_set_prototype(
        func,
        Some(realm.intrinsics().function_prototype()),
      )?;
      scope.push_root(Value::Object(func))?;
      scope.define_property(object_ctor, key, data_desc(Value::Object(func)))?;
    }
  }

  let call_id = vm.register_native_call(structured_clone_native)?;
  let name_s = scope.alloc_string("structuredClone")?;
  scope.push_root(Value::String(name_s))?;
  let func = scope.alloc_native_function_with_slots(
    call_id,
    None,
    name_s,
    1,
    &[
      Value::Object(global),
      Value::Number(realm.id().to_raw() as f64),
    ],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(func))?;

  let key = alloc_key(scope, "structuredClone")?;
  scope.define_property(global, key, data_desc(Value::Object(func)))?;
  Ok(())
}

fn object_get_own_property_symbols_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("Object.getOwnPropertySymbols requires intrinsics"))?;

  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let own_keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  let mut syms: Vec<GcSymbol> = Vec::new();
  syms
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if let PropertyKey::Symbol(sym) = key {
      syms.push(sym);
    }
  }

  let array = scope.alloc_array(syms.len())?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(intr.array_prototype()))?;

  for (i, sym) in syms.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let idx_s = scope.alloc_string(&i.to_string())?;
    scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);
    scope.define_property(
      array,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Symbol(sym),
          writable: true,
        },
      },
    )?;
  }

  Ok(Value::Object(array))
}

#[derive(Clone, Copy, Debug)]
enum EncodedValue {
  Primitive(Value),
  Object(NodeId),
}

type NodeId = usize;

#[derive(Debug)]
enum Node {
  Array {
    length: u32,
    props: Vec<(GcString, EncodedValue)>,
  },
  Object {
    props: Vec<(GcString, EncodedValue)>,
  },
  ArrayBuffer {
    source: GcObject,
    /// If `transferred` is true, the data is moved from `source` after validation and stored in the
    /// `transfer_data` map during detachment.
    transferred: bool,
    /// Present only when `transferred == false`.
    bytes: Option<Vec<u8>>,
  },
  Uint8Array {
    buffer: NodeId,
    byte_offset: usize,
    length: usize,
  },
  Date {
    time: f64,
  },
  Blob {
    data: Option<BlobData>,
  },
}

struct SerializeState {
  global: GcObject,
  transfer_set: HashSet<GcObject>,
  object_to_id: HashMap<GcObject, NodeId>,
  nodes: Vec<Node>,

  total_props: usize,
  total_copied_bytes: usize,

  // Cached helpers.
  uint8_buffer_key: PropertyKey,
  uint8_byte_offset_key: PropertyKey,
  uint8_length_key: PropertyKey,
}

impl SerializeState {
  fn new(
    global: GcObject,
    transfer_set: HashSet<GcObject>,
    uint8_buffer_key: PropertyKey,
    uint8_byte_offset_key: PropertyKey,
    uint8_length_key: PropertyKey,
  ) -> Self {
    Self {
      global,
      transfer_set,
      object_to_id: HashMap::new(),
      nodes: Vec::new(),
      total_props: 0,
      total_copied_bytes: 0,
      uint8_buffer_key,
      uint8_byte_offset_key,
      uint8_length_key,
    }
  }

  fn push_node(&mut self, node: Node, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<NodeId, VmError> {
    if self.nodes.len() >= MAX_VISITED_NODES {
      return Err(throw_range_error(vm, scope, "structuredClone: max object count exceeded"));
    }
    self.nodes.push(node);
    Ok(self.nodes.len() - 1)
  }

  fn count_prop(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<(), VmError> {
    self.total_props = self.total_props.saturating_add(1);
    if self.total_props > MAX_ENUMERABLE_PROPS {
      return Err(throw_range_error(vm, scope, "structuredClone: max property count exceeded"));
    }
    Ok(())
  }

  fn add_copied_bytes(&mut self, vm: &mut Vm, scope: &mut Scope<'_>, bytes: usize) -> Result<(), VmError> {
    self.total_copied_bytes = self.total_copied_bytes.saturating_add(bytes);
    if self.total_copied_bytes > MAX_COPIED_BYTES {
      return Err(throw_range_error(vm, scope, "structuredClone: max copied bytes exceeded"));
    }
    Ok(())
  }
}

struct DeserializeState {
  global: GcObject,
  realm_id: Option<RealmId>,

  nodes: Vec<Node>,
  clones: HashMap<NodeId, GcObject>,
  transfer_data: HashMap<GcObject, Box<[u8]>>,
}

fn structured_clone_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let options = args.get(1).copied().unwrap_or(Value::Undefined);

  let slots = scope.heap().get_function_native_slots(callee)?;
  let global = match slots
    .get(STRUCTURED_CLONE_GLOBAL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("structuredClone missing global slot")),
  };
  let realm_id = slots
    .get(STRUCTURED_CLONE_REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot);

  // Allocate commonly used keys (rooted for the duration of this call).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(global))?;
  scope.push_root(Value::Object(callee))?;
  scope.push_root(value)?;
  scope.push_root(options)?;

  let (uint8_buffer_key, uint8_byte_offset_key, uint8_length_key) = prepare_cached_keys(&mut scope)?;

  // Parse the transfer list first; per spec we must validate it before cloning.
  let transfer_list = parse_transfer_list(vm, &mut scope, host, hooks, global, options)?;
  // Root transfer list entries for the duration of this call so script-side mutations of the
  // transfer array don't allow GC to collect buffers we still need to detach.
  for &buf in &transfer_list {
    scope.push_root(Value::Object(buf))?;
  }
  let transfer_set: HashSet<GcObject> = transfer_list.iter().copied().collect();

  // --- Serialize/validate the input graph ---
  let mut state = SerializeState::new(
    global,
    transfer_set,
    uint8_buffer_key,
    uint8_byte_offset_key,
    uint8_length_key,
  );
  let root = serialize_value(vm, &mut scope, host, hooks, &mut state, value, 0)?;

  // --- Detach transfer list buffers (must not run on DataCloneError paths) ---
  let transfer_data = detach_transfer_list(vm, &mut scope, global, &transfer_list)?;

  // --- Deserialize into fresh JS objects ---
  let mut deser = DeserializeState {
    global,
    realm_id,
    nodes: state.nodes,
    clones: HashMap::new(),
    transfer_data,
  };
  deserialize_value(vm, &mut scope, &mut deser, callee, root, 0)
}

fn prepare_cached_keys(scope: &mut Scope<'_>) -> Result<(PropertyKey, PropertyKey, PropertyKey), VmError> {
  // Uint8Array view metadata keys.
  let buffer_s = scope.alloc_string("buffer")?;
  scope.push_root(Value::String(buffer_s))?;
  let byte_offset_s = scope.alloc_string("byteOffset")?;
  scope.push_root(Value::String(byte_offset_s))?;
  let length_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_s))?;

  Ok((
    PropertyKey::from_string(buffer_s),
    PropertyKey::from_string(byte_offset_s),
    PropertyKey::from_string(length_s),
  ))
}

fn parse_transfer_list(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global: GcObject,
  options: Value,
) -> Result<Vec<GcObject>, VmError> {
  if matches!(options, Value::Undefined | Value::Null) {
    return Ok(Vec::new());
  }

  // WebIDL dictionary semantics: ToObject.
  let options_obj = scope.to_object(vm, host, hooks, options)?;
  scope.push_root(Value::Object(options_obj))?;

  let transfer_key = alloc_key(scope, "transfer")?;
  let transfer_val = vm.get_with_host_and_hooks(host, scope, hooks, options_obj, transfer_key)?;

  if matches!(transfer_val, Value::Undefined | Value::Null) {
    return Ok(Vec::new());
  }

  let Value::Object(transfer_obj) = transfer_val else {
    return Err(throw_data_clone_error(
      vm,
      scope,
      global,
      "structuredClone: transfer must be an array",
    ));
  };
  let is_array = scope.heap().object_is_array(transfer_obj)?;
  if !is_array {
    return Err(throw_data_clone_error(
      vm,
      scope,
      global,
      "structuredClone: transfer must be an array",
    ));
  }

  let length_key = alloc_key(scope, "length")?;
  let len_val = vm.get_with_host_and_hooks(host, scope, hooks, transfer_obj, length_key)?;
  let Value::Number(len_n) = len_val else {
    return Err(throw_data_clone_error(
      vm,
      scope,
      global,
      "structuredClone: transfer array has invalid length",
    ));
  };
  if !len_n.is_finite() || len_n < 0.0 || len_n.fract() != 0.0 || len_n > (u32::MAX as f64) {
    return Err(throw_data_clone_error(
      vm,
      scope,
      global,
      "structuredClone: transfer array has invalid length",
    ));
  }
  let len = len_n as usize;

  let mut seen: HashSet<GcObject> = HashSet::new();
  let mut out: Vec<GcObject> = Vec::new();
  out.try_reserve(len).map_err(|_| VmError::OutOfMemory)?;

  for i in 0..len {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let key_s = scope.alloc_string(&i.to_string())?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    let entry = vm.get_with_host_and_hooks(host, scope, hooks, transfer_obj, key)?;

    let Value::Object(obj) = entry else {
      return Err(throw_data_clone_error(
        vm,
        scope,
        global,
        "structuredClone: transfer list contains non-ArrayBuffer",
      ));
    };
    if !scope.heap().is_array_buffer_object(obj) {
      return Err(throw_data_clone_error(
        vm,
        scope,
        global,
        "structuredClone: transfer list contains non-ArrayBuffer",
      ));
    }
    let detached = scope.heap().is_detached_array_buffer(obj).unwrap_or(false);
    if detached {
      return Err(throw_data_clone_error(
        vm,
        scope,
        global,
        "structuredClone: transfer list contains detached ArrayBuffer",
      ));
    }
    if !seen.insert(obj) {
      return Err(throw_data_clone_error(
        vm,
        scope,
        global,
        "structuredClone: transfer list contains duplicates",
      ));
    }
    out.push(obj);
  }

  Ok(out)
}

fn serialize_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut SerializeState,
  value: Value,
  depth: usize,
) -> Result<EncodedValue, VmError> {
  if depth > MAX_RECURSION_DEPTH {
    return Err(throw_range_error(
      vm,
      scope,
      "structuredClone: max recursion depth exceeded",
    ));
  }

  match value {
    Value::Undefined
    | Value::Null
    | Value::Bool(_)
    | Value::Number(_)
    | Value::BigInt(_)
    | Value::String(_) => Ok(EncodedValue::Primitive(value)),
    Value::Symbol(_) => Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone Symbol",
    )),
    Value::Object(obj) => serialize_object(vm, scope, host, hooks, state, obj, depth),
  }
}

fn serialize_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut SerializeState,
  obj: GcObject,
  depth: usize,
) -> Result<EncodedValue, VmError> {
  if let Some(id) = state.object_to_id.get(&obj).copied() {
    return Ok(EncodedValue::Object(id));
  }

  // Reject callable objects and Promises.
  if scope.heap().is_callable(Value::Object(obj))? {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone function",
    ));
  }
  if scope.heap().is_promise_object(obj) {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone Promise",
    ));
  }

  // ArrayBuffer.
  if scope.heap().is_array_buffer_object(obj) {
    if scope.heap().is_detached_array_buffer(obj).unwrap_or(false) {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: cannot clone detached ArrayBuffer",
      ));
    }

    let transferred = state.transfer_set.contains(&obj);

    // Insert the node ID eagerly so cycles/duplicates resolve.
    let id = state.push_node(
      Node::ArrayBuffer {
        source: obj,
        transferred,
        bytes: None,
      },
      vm,
      scope,
    )?;
    state.object_to_id.insert(obj, id);

    if !transferred {
      const INVALID_BACKING_STORE_MSG: &str = "structuredClone: invalid ArrayBuffer backing store";
      // Avoid holding a borrowed slice from `heap` across `state.add_copied_bytes`, which needs a
      // mutable borrow of `scope` to allocate errors.
      let bytes_len = match scope.heap().array_buffer_data(obj) {
        Ok(bytes) => bytes.len(),
        Err(_) => {
          return Err(throw_data_clone_error(
            vm,
            scope,
            state.global,
            INVALID_BACKING_STORE_MSG,
          ));
        }
      };
      state.add_copied_bytes(vm, scope, bytes_len)?;

      let mut out: Vec<u8> = Vec::new();
      out
        .try_reserve_exact(bytes_len)
        .map_err(|_| VmError::OutOfMemory)?;
      let bytes = match scope.heap().array_buffer_data(obj) {
        Ok(bytes) => bytes,
        Err(_) => {
          return Err(throw_data_clone_error(
            vm,
            scope,
            state.global,
            INVALID_BACKING_STORE_MSG,
          ));
        }
      };
      out.extend_from_slice(bytes);

      // Populate the node payload.
      if let Some(Node::ArrayBuffer { bytes: slot, .. }) = state.nodes.get_mut(id) {
        *slot = Some(out);
      }
    }

    return Ok(EncodedValue::Object(id));
  }

  // Uint8Array.
  if scope.heap().is_uint8_array_object(obj) {
    // Uint8Array.buffer / byteOffset / length are built-in accessors.
    let buffer_val = vm.get_with_host_and_hooks(host, scope, hooks, obj, state.uint8_buffer_key)?;
    let Value::Object(buffer_obj) = buffer_val else {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: Uint8Array.buffer is not an object",
      ));
    };
    if !scope.heap().is_array_buffer_object(buffer_obj) {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: Uint8Array.buffer is not an ArrayBuffer",
      ));
    }

    let byte_offset_val =
      vm.get_with_host_and_hooks(host, scope, hooks, obj, state.uint8_byte_offset_key)?;
    let length_val = vm.get_with_host_and_hooks(host, scope, hooks, obj, state.uint8_length_key)?;
    let (Value::Number(byte_offset_n), Value::Number(length_n)) = (byte_offset_val, length_val) else {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: Uint8Array has invalid view metadata",
      ));
    };
    if !byte_offset_n.is_finite() || byte_offset_n < 0.0 || byte_offset_n.fract() != 0.0 {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: Uint8Array has invalid byteOffset",
      ));
    }
    if !length_n.is_finite() || length_n < 0.0 || length_n.fract() != 0.0 {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: Uint8Array has invalid length",
      ));
    }

    // Ensure the buffer is serialized before creating the view node (for easier deserialization).
    let buffer_encoded = serialize_value(
      vm,
      scope,
      host,
      hooks,
      state,
      Value::Object(buffer_obj),
      depth + 1,
    )?;
    let EncodedValue::Object(buffer_id) = buffer_encoded else {
      return Err(VmError::InvariantViolation(
        "ArrayBuffer must serialize to an object node",
      ));
    };

    let id = state.push_node(
      Node::Uint8Array {
        buffer: buffer_id,
        byte_offset: byte_offset_n as usize,
        length: length_n as usize,
      },
      vm,
      scope,
    )?;
    state.object_to_id.insert(obj, id);

    return Ok(EncodedValue::Object(id));
  }

  // Date.
  if let Some(time) = scope.heap().date_value(obj)? {
    let id = state.push_node(Node::Date { time }, vm, scope)?;
    state.object_to_id.insert(obj, id);
    return Ok(EncodedValue::Object(id));
  }

  // Blob.
  if let Some(data) = clone_blob_data_for_fetch(vm, scope.heap(), Value::Object(obj))? {
    state.add_copied_bytes(vm, scope, data.bytes.len())?;
    let id = state.push_node(Node::Blob { data: Some(data) }, vm, scope)?;
    state.object_to_id.insert(obj, id);
    return Ok(EncodedValue::Object(id));
  }

  // Array / ordinary object.
  let is_array = scope.heap().object_is_array(obj)?;

  let placeholder_id = if is_array {
    let length_val = scope
      .heap()
      .object_get_own_data_property_value(obj, &state.uint8_length_key)?
      .ok_or_else(|| {
        throw_data_clone_error(
          vm,
          scope,
          state.global,
          "structuredClone: invalid array length",
        )
      })?;
    let Value::Number(length_n) = length_val else {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: invalid array length",
      ));
    };
    if !length_n.is_finite() || length_n < 0.0 || length_n.fract() != 0.0 || length_n > (u32::MAX as f64) {
      return Err(throw_data_clone_error(
        vm,
        scope,
        state.global,
        "structuredClone: invalid array length",
      ));
    }
    let length = length_n as u32;
    state.push_node(Node::Array { length, props: Vec::new() }, vm, scope)?
  } else {
    state.push_node(Node::Object { props: Vec::new() }, vm, scope)?
  };
  state.object_to_id.insert(obj, placeholder_id);

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let keys = scope.heap().own_property_keys(obj)?;
  for (i, key) in keys.iter().copied().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let PropertyKey::String(key_s) = key else {
      continue;
    };

    let Some(desc) = scope
      .heap()
      .object_get_own_property_with_tick(obj, &key, || vm.tick())?
    else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    state.count_prop(vm, &mut scope)?;

    // Root key while we perform `Get`, which can invoke user code.
    scope.push_root(Value::String(key_s))?;
    let prop_val = vm.get_with_host_and_hooks(host, &mut scope, hooks, obj, key)?;
    let encoded = serialize_value(vm, &mut scope, host, hooks, state, prop_val, depth + 1)?;

    match state.nodes.get_mut(placeholder_id) {
      Some(Node::Array { props, .. } | Node::Object { props }) => props.push((key_s, encoded)),
      _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
    }
  }

  Ok(EncodedValue::Object(placeholder_id))
}

fn detach_transfer_list(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global: GcObject,
  transfer_list: &[GcObject],
) -> Result<HashMap<GcObject, Box<[u8]>>, VmError> {
  let mut out: HashMap<GcObject, Box<[u8]>> = HashMap::new();
  out
    .try_reserve(transfer_list.len())
    .map_err(|_| VmError::OutOfMemory)?;

  // Validate detachment preconditions for the whole list first so DataCloneError doesn't leave
  // partially-detached buffers behind.
  for (i, &buf) in transfer_list.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if scope.heap().is_detached_array_buffer(buf).unwrap_or(false) {
      return Err(throw_data_clone_error(
        vm,
        scope,
        global,
        "structuredClone: transfer list contains detached ArrayBuffer",
      ));
    }
  }

  for (i, &buf) in transfer_list.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let data = match scope.heap_mut().detach_array_buffer_take_data(buf) {
      Ok(Some(data)) => data,
      Ok(None) => {
        return Err(throw_data_clone_error(
          vm,
          scope,
          global,
          "structuredClone: transfer list contains detached ArrayBuffer",
        ));
      }
      Err(_) => {
        return Err(throw_data_clone_error(
          vm,
          scope,
          global,
          "structuredClone: failed to detach ArrayBuffer",
        ));
      }
    };
    out.insert(buf, data);
  }

  Ok(out)
}

fn deserialize_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  state: &mut DeserializeState,
  callee: GcObject,
  value: EncodedValue,
  depth: usize,
) -> Result<Value, VmError> {
  if depth > MAX_RECURSION_DEPTH {
    return Err(throw_range_error(
      vm,
      scope,
      "structuredClone: max recursion depth exceeded",
    ));
  }

  match value {
    EncodedValue::Primitive(v) => Ok(v),
    EncodedValue::Object(id) => Ok(Value::Object(deserialize_node(vm, scope, state, callee, id, depth)?)),
  }
}

fn deserialize_node(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  state: &mut DeserializeState,
  callee: GcObject,
  id: NodeId,
  depth: usize,
) -> Result<GcObject, VmError> {
  if let Some(obj) = state.clones.get(&id).copied() {
    return Ok(obj);
  }

  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "structuredClone requires intrinsics (create a Realm first)",
  ))?;

  // Allocate a placeholder object first (for cycle support), then populate.
  let kind_tag = match state.nodes.get(id) {
    Some(Node::Array { .. }) => 1u8,
    Some(Node::Object { .. }) => 2u8,
    Some(Node::ArrayBuffer { .. }) => 3u8,
    Some(Node::Uint8Array { .. }) => 4u8,
    Some(Node::Date { .. }) => 5u8,
    Some(Node::Blob { .. }) => 6u8,
    None => return Err(VmError::InvariantViolation("structuredClone node id out of bounds")),
  };

  let obj = match kind_tag {
    1 => {
      let length = match state.nodes.get(id) {
        Some(Node::Array { length, .. }) => *length,
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let arr = scope.alloc_array(length as usize)?;
      scope.push_root(Value::Object(arr))?;
      scope
        .heap_mut()
        .object_set_prototype(arr, Some(intr.array_prototype()))?;
      state.clones.insert(id, arr);

      let props = match state.nodes.get_mut(id) {
        Some(Node::Array { props, .. }) => std::mem::take(props),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      for (i, (key_s, val)) in props.into_iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let cloned_val = deserialize_value(vm, scope, state, callee, val, depth + 1)?;
        let key = PropertyKey::from_string(key_s);
        scope.define_property(
          arr,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data {
              value: cloned_val,
              writable: true,
            },
          },
        )?;
      }
      arr
    }
    2 => {
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.object_prototype()))?;
      state.clones.insert(id, obj);

      let props = match state.nodes.get_mut(id) {
        Some(Node::Object { props }) => std::mem::take(props),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      for (i, (key_s, val)) in props.into_iter().enumerate() {
        if i % 1024 == 0 {
          vm.tick()?;
        }
        let cloned_val = deserialize_value(vm, scope, state, callee, val, depth + 1)?;
        let key = PropertyKey::from_string(key_s);
        scope.define_property(
          obj,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data {
              value: cloned_val,
              writable: true,
            },
          },
        )?;
      }
      obj
    }
    3 => {
      // ArrayBuffer.
      let (source, transferred, bytes) = match state.nodes.get_mut(id) {
        Some(Node::ArrayBuffer {
          source,
          transferred,
          bytes,
        }) => (*source, *transferred, bytes.take()),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };

      let data_vec = if transferred {
        let data = state
          .transfer_data
          .remove(&source)
          .ok_or(VmError::InvariantViolation(
            "structuredClone missing transferred ArrayBuffer data",
          ))?;
        data.into_vec()
      } else {
        bytes.ok_or(VmError::InvariantViolation("structuredClone missing ArrayBuffer bytes"))?
      };

      let ab = scope.alloc_array_buffer_from_u8_vec(data_vec)?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      state.clones.insert(id, ab);
      ab
    }
    4 => {
      // Uint8Array.
      let (buffer_id, byte_offset, length) = match state.nodes.get(id) {
        Some(Node::Uint8Array {
          buffer,
          byte_offset,
          length,
        }) => (*buffer, *byte_offset, *length),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let buffer_obj = deserialize_node(vm, scope, state, callee, buffer_id, depth + 1)?;

      let view = scope.alloc_uint8_array(buffer_obj, byte_offset, length)?;
      scope.push_root(Value::Object(view))?;
      scope
        .heap_mut()
        .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
      state.clones.insert(id, view);
      view
    }
    5 => {
      // Date.
      let time = match state.nodes.get(id) {
        Some(Node::Date { time }) => *time,
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let date = scope.alloc_date(time)?;
      scope.push_root(Value::Object(date))?;
      scope
        .heap_mut()
        .object_set_prototype(date, Some(intr.date_prototype()))?;
      state.clones.insert(id, date);
      date
    }
    6 => {
      // Blob.
      let data = match state.nodes.get_mut(id) {
        Some(Node::Blob { data }) => data.take(),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      }
      .ok_or(VmError::InvariantViolation("structuredClone missing Blob data"))?;

      let realm_id = state
        .realm_id
        .or_else(|| vm.current_realm())
        .ok_or(VmError::Unimplemented(
          "structuredClone: missing realm id for Blob",
        ))?;
      let proto = blob_prototype_for_realm(realm_id).ok_or(VmError::Unimplemented(
        "structuredClone: Blob bindings not installed",
      ))?;

      let blob_obj = create_blob_with_proto(vm, scope, callee, proto, data)?;
      scope.push_root(Value::Object(blob_obj))?;
      state.clones.insert(id, blob_obj);
      blob_obj
    }
    _ => return Err(VmError::InvariantViolation("structuredClone invalid node kind tag")),
  };

  Ok(obj)
}

fn throw_data_clone_error(vm: &mut Vm, scope: &mut Scope<'_>, global: GcObject, message: &str) -> VmError {
  if let Some(intr) = vm.intrinsics() {
    if let Ok(dom_exception) = DomExceptionClassVmJs::install_for_global(vm, scope, global, intr) {
      if let Ok(err) = dom_exception.new_instance(scope, "DataCloneError", message) {
        return VmError::Throw(err);
      }
    }
  }
  match make_dom_exception(scope, "DataCloneError", message) {
    Ok(v) => VmError::Throw(v),
    Err(err) => err,
  }
}

fn throw_range_error(vm: &mut Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
  if let Some(intr) = vm.intrinsics() {
    match vm_js::new_range_error(scope, intr, message) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  } else {
    VmError::TypeError("RangeError")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use vm_js::Value;

  fn get_string(realm: &WindowRealm, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    realm.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn structured_clone_is_installed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let v = realm.exec_script("typeof structuredClone")?;
    assert_eq!(get_string(&realm, v), "function");
    Ok(())
  }

  #[test]
  fn structured_clone_basic_object_and_cycles() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const x = {a:1};\
         const y = structuredClone(x);\
         if (y === x) return false;\
         if (y.a !== 1) return false;\
         const c = {}; c.self = c;\
         const d = structuredClone(c);\
         return d !== c && d.self === d;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_arrays_preserve_identity_and_holes() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const a = {};\
         const x = [a, , a];\
         const y = structuredClone(x);\
         if (y.length !== 3) return false;\
         if (1 in y) return false;\
         return y[0] === y[2] && y[0] !== a;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_ignores_symbol_keys() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const sym = Symbol('k');\
         const x = {a: 1};\
         x[sym] = 2;\
         const y = structuredClone(x);\
         if (y.a !== 1) return false;\
         const syms = Object.getOwnPropertySymbols(y);\
         return syms.length === 0;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_array_buffer_copy_and_transfer() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let copy_ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(2);\
         const v = new Uint8Array(ab);\
         v[0] = 1; v[1] = 2;\
         const c = structuredClone(ab);\
         if (c === ab) return false;\
         if (c.byteLength !== 2) return false;\
         const v2 = new Uint8Array(c);\
         return v2[0] === 1 && v2[1] === 2;\
       })()",
    )?;
    assert_eq!(copy_ok, Value::Bool(true));

    let transfer_ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(2);\
         const view = new Uint8Array(ab);\
         view[0] = 7;\
         const c = structuredClone(ab, { transfer: [ab] });\
         if (ab.byteLength !== 0) return false;\
         if (view.length !== 0) return false;\
         if (c.byteLength !== 2) return false;\
         return new Uint8Array(c)[0] === 7;\
       })()",
    )?;
    assert_eq!(transfer_ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn structured_clone_transfer_list_validation() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let dup = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(1);\
         try { structuredClone(ab, { transfer: [ab, ab] }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, dup), "DataCloneError");

    let non_ab = realm.exec_script(
      "(() => {\
         try { structuredClone(1, { transfer: [1] }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, non_ab), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_symbol_and_function() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let sym = realm.exec_script("try { structuredClone(Symbol('x')); 'no' } catch (e) { e.name }")?;
    assert_eq!(get_string(&realm, sym), "DataCloneError");

    let fun = realm.exec_script("try { structuredClone(function(){}); 'no' } catch (e) { e.name }")?;
    assert_eq!(get_string(&realm, fun), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_clones_blob() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const b = new Blob(['hi'], { type: 'text/plain' });\
         const c = structuredClone(b);\
         if (c === b) return false;\
         if (c.size !== 2) return false;\
         if (c.type !== 'text/plain') return false;\
         globalThis.__sc_blob_text = null;\
         c.text().then(t => { globalThis.__sc_blob_text = t; });\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    realm.perform_microtask_checkpoint()?;
    let text = realm.exec_script("__sc_blob_text")?;
    assert_eq!(get_string(&realm, text), "hi");

    Ok(())
  }
}
