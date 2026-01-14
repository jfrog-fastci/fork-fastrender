//! HTML structured cloning API: `structuredClone(value, options?)`.
//!
//! This is a spec-shaped MVP intended to support real-world libraries that depend on structured
//! cloning for deep-copying data structures and for transferring `ArrayBuffer` backing stores.
//!
//! Supported (and tested) types:
//! - primitives (except `symbol`)
//! - Array
//! - Map
//! - Set
//! - ordinary objects (MVP: enumerable own *string* keys only)
//! - ArrayBuffer (copy or transfer)
//! - TypedArrays (clones underlying ArrayBuffer and re-creates the view)
//! - DataView
//! - Date
//! - RegExp
//! - boxed primitives (`new Boolean(...)`, `new Number(...)`, `new String(...)`, `Object(1n)`)
//! - Error objects (preserves intrinsic error prototypes + `name`/`message`)
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
  GcObject, GcString, GcSymbol, Intrinsics, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  RealmId, RegExpFlags, RegExpProgram, Scope, TypedArrayKind, Value, Vm, VmError, VmHost,
  VmHostHooks,
};
use vm_js::iterator::{get_iterator, iterator_step_value};

/// Native slot index for the `structuredClone` function's realm global object.
pub(crate) const STRUCTURED_CLONE_GLOBAL_SLOT: usize = 0;
/// Native slot index for the `structuredClone` function's realm ID.
pub(crate) const STRUCTURED_CLONE_REALM_ID_SLOT: usize = 1;

// --- DoS resistance limits ---
//
// These are not spec-defined; they are hard caps to keep structured cloning safe under hostile
// inputs.
const MAX_VISITED_NODES: usize = 100_000;
const MAX_ENUMERABLE_PROPS: usize = 1_000_000;
const MAX_COPIED_BYTES: usize = 32 * 1024 * 1024; // 32MiB (copied ArrayBuffer/Blob bytes only)

#[derive(Clone, Copy)]
struct MarkerSymbols {
  boolean_data: GcSymbol,
  number_data: GcSymbol,
  string_data: GcSymbol,
  bigint_data: GcSymbol,
  symbol_data: GcSymbol,
  regexp_string_iterator_iterating_regexp: GcSymbol,
  regexp_string_iterator_iterated_string: GcSymbol,
  regexp_string_iterator_done: GcSymbol,
}

impl MarkerSymbols {
  fn new(scope: &mut Scope<'_>) -> Result<Self, VmError> {
    let boolean_key = scope.alloc_string("vm-js.internal.BooleanData")?;
    scope.push_root(Value::String(boolean_key))?;
    let boolean_data = scope.heap_mut().symbol_for(boolean_key)?;

    let number_key = scope.alloc_string("vm-js.internal.NumberData")?;
    scope.push_root(Value::String(number_key))?;
    let number_data = scope.heap_mut().symbol_for(number_key)?;

    let string_key = scope.alloc_string("vm-js.internal.StringData")?;
    scope.push_root(Value::String(string_key))?;
    let string_data = scope.heap_mut().symbol_for(string_key)?;

    let bigint_key = scope.alloc_string("vm-js.internal.BigIntData")?;
    scope.push_root(Value::String(bigint_key))?;
    let bigint_data = scope.heap_mut().symbol_for(bigint_key)?;

    let symbol_key = scope.alloc_string("vm-js.internal.SymbolData")?;
    scope.push_root(Value::String(symbol_key))?;
    let symbol_data = scope.heap_mut().symbol_for(symbol_key)?;
    let regexp_string_iterator_iterating_regexp_key =
      scope.alloc_string("vm-js.internal.RegExpStringIteratorIteratingRegExp")?;
    scope.push_root(Value::String(regexp_string_iterator_iterating_regexp_key))?;
    let regexp_string_iterator_iterating_regexp =
      scope.heap_mut().symbol_for(regexp_string_iterator_iterating_regexp_key)?;

    let regexp_string_iterator_iterated_string_key =
      scope.alloc_string("vm-js.internal.RegExpStringIteratorIteratedString")?;
    scope.push_root(Value::String(regexp_string_iterator_iterated_string_key))?;
    let regexp_string_iterator_iterated_string =
      scope.heap_mut().symbol_for(regexp_string_iterator_iterated_string_key)?;

    let regexp_string_iterator_done_key = scope.alloc_string("vm-js.internal.RegExpStringIteratorDone")?;
    scope.push_root(Value::String(regexp_string_iterator_done_key))?;
    let regexp_string_iterator_done = scope.heap_mut().symbol_for(regexp_string_iterator_done_key)?;

    Ok(Self {
      boolean_data,
      number_data,
      string_data,
      bigint_data,
      symbol_data,
      regexp_string_iterator_iterating_regexp,
      regexp_string_iterator_iterated_string,
      regexp_string_iterator_done,
    })
  }
}

#[derive(Clone, Copy, Debug)]
enum ErrorTag {
  Error,
  EvalError,
  RangeError,
  ReferenceError,
  SyntaxError,
  TypeError,
  URIError,
}

impl ErrorTag {
  fn as_str(self) -> &'static str {
    match self {
      Self::Error => "Error",
      Self::EvalError => "EvalError",
      Self::RangeError => "RangeError",
      Self::ReferenceError => "ReferenceError",
      Self::SyntaxError => "SyntaxError",
      Self::TypeError => "TypeError",
      Self::URIError => "URIError",
    }
  }

  fn prototype(self, intr: Intrinsics) -> GcObject {
    match self {
      Self::Error => intr.error_prototype(),
      Self::EvalError => intr.eval_error_prototype(),
      Self::RangeError => intr.range_error_prototype(),
      Self::ReferenceError => intr.reference_error_prototype(),
      Self::SyntaxError => intr.syntax_error_prototype(),
      Self::TypeError => intr.type_error_prototype(),
      Self::URIError => intr.uri_error_prototype(),
    }
  }
}

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
  Map {
    entries: Vec<(EncodedValue, EncodedValue)>,
  },
  Set {
    entries: Vec<EncodedValue>,
  },
  BooleanObject {
    value: bool,
  },
  NumberObject {
    value: f64,
  },
  StringObject {
    value: GcString,
  },
  BigIntObject {
    value: Value,
  },
  Error {
    name: ErrorTag,
    /// `Some` if the serialized message is a string, `None` if the serialized message is
    /// `undefined`.
    message: Option<Vec<u16>>,
  },
  ArrayBuffer {
    source: GcObject,
    /// If `transferred` is true, the backing store of `source` is moved into a fresh ArrayBuffer
    /// via `Heap::transfer_array_buffer(..)` after serialization succeeds.
    transferred: bool,
    /// Present only when `transferred == false`.
    bytes: Option<Vec<u8>>,
  },
  TypedArray {
    kind: TypedArrayKind,
    buffer: NodeId,
    byte_offset: usize,
    length: usize,
  },
  DataView {
    buffer: NodeId,
    byte_offset: usize,
    byte_length: usize,
  },
  Date {
    time: f64,
  },
  RegExp {
    original_source: GcString,
    original_flags: GcString,
    flags: RegExpFlags,
    program: Option<RegExpProgram>,
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
  error_name_key: PropertyKey,
  error_message_key: PropertyKey,
  markers: MarkerSymbols,
}

impl SerializeState {
  fn new(
    global: GcObject,
    transfer_set: HashSet<GcObject>,
    uint8_buffer_key: PropertyKey,
    uint8_byte_offset_key: PropertyKey,
    uint8_length_key: PropertyKey,
    error_name_key: PropertyKey,
    error_message_key: PropertyKey,
    markers: MarkerSymbols,
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
      error_name_key,
      error_message_key,
      markers,
    }
  }

  fn push_node(&mut self, node: Node, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<NodeId, VmError> {
    if self.nodes.len() >= MAX_VISITED_NODES {
      return Err(throw_range_error(vm, scope, "structuredClone: max object count exceeded"));
    }
    self.nodes.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
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
  markers: MarkerSymbols,

  nodes: Vec<Node>,
  clones: HashMap<NodeId, GcObject>,
  /// Maps transferred ArrayBuffer source objects to their destination ArrayBuffer objects.
  ///
  /// The destination buffers are pre-created (and rooted) after serialization succeeds to preserve
  /// `vm-js` external-memory accounting and to avoid holding backing-store bytes outside the heap.
  transfer_data: HashMap<GcObject, GcObject>,
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

  let markers = MarkerSymbols::new(&mut scope)?;
  let error_name_key = alloc_key(&mut scope, "name")?;
  let error_message_key = alloc_key(&mut scope, "message")?;

  // --- Serialize/validate the input graph ---
  let mut state = SerializeState::new(
    global,
    transfer_set,
    uint8_buffer_key,
    uint8_byte_offset_key,
    uint8_length_key,
    error_name_key,
    error_message_key,
    markers,
  );
  let root = serialize_value_iterative(vm, &mut scope, host, hooks, &mut state, value)?;

  // --- Transfer/detach transfer list buffers (must not run on DataCloneError paths) ---
  let transfer_data = prepare_transfer_list_buffers(vm, &mut scope, global, &transfer_list, &state.nodes)?;

  // --- Deserialize into fresh JS objects ---
  let mut deser = DeserializeState {
    global,
    realm_id,
    markers,
    nodes: state.nodes,
    clones: HashMap::new(),
    transfer_data,
  };
  deserialize_value_iterative(vm, &mut scope, host, hooks, &mut deser, callee, root)
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

  // `transfer_val` can be an arbitrary iterable. Root it (and the iterator record) for the
  // duration of parsing so GC during `vm.tick()` can't collect the iterator mid-iteration.
  let mut iter_scope = scope.reborrow();
  iter_scope.push_root(transfer_val)?;

  let mut iter = get_iterator(vm, host, hooks, &mut iter_scope, transfer_val)?;
  // Root iterator record values, since the Rust stack isn't traced by the GC.
  iter_scope.push_roots(&[iter.iterator, iter.next_method])?;

  let mut seen: HashSet<GcObject> = HashSet::new();
  let mut out: Vec<GcObject> = Vec::new();

  let mut count: usize = 0;
  loop {
    // Tick before pulling the next value so we don't hold an unrooted entry across a potential GC.
    if count % 1024 == 0 {
      vm.tick()?;
    }

    let Some(entry) = iterator_step_value(vm, host, hooks, &mut iter_scope, &mut iter)? else {
      break;
    };

    count = count.saturating_add(1);
    if count > MAX_VISITED_NODES {
      return Err(throw_range_error(
        vm,
        &mut iter_scope,
        "structuredClone: transfer list too large",
      ));
    }

    // Root the yielded entry while validating it (error construction can allocate/GC).
    let obj = {
      let mut entry_scope = iter_scope.reborrow();
      entry_scope.push_root(entry)?;

      let Value::Object(obj) = entry else {
        return Err(throw_data_clone_error(
          vm,
          &mut entry_scope,
          global,
          "structuredClone: transfer list contains non-ArrayBuffer",
        ));
      };
      if !entry_scope.heap().is_array_buffer_object(obj) {
        return Err(throw_data_clone_error(
          vm,
          &mut entry_scope,
          global,
          "structuredClone: transfer list contains non-ArrayBuffer",
        ));
      }
      let detached = entry_scope.heap().is_detached_array_buffer(obj).unwrap_or(false);
      if detached {
        return Err(throw_data_clone_error(
          vm,
          &mut entry_scope,
          global,
          "structuredClone: transfer list contains detached ArrayBuffer",
        ));
      }
      if !seen.insert(obj) {
        return Err(throw_data_clone_error(
          vm,
          &mut entry_scope,
          global,
          "structuredClone: transfer list contains duplicates",
        ));
      }

      obj
    };

    // Root accepted buffers for the duration of parsing so later `vm.tick()` calls can't collect
    // them even if the iterator yields freshly-created objects.
    iter_scope.push_root(Value::Object(obj))?;
    out.push(obj);
  }

  Ok(out)
}

#[derive(Debug)]
enum SerializeFrameKind {
  Props {
    keys: Vec<PropertyKey>,
    next_key_idx: usize,
  },
  MapEntries {
    next_entry_idx: usize,
    entry_len: usize,
  },
  SetEntries {
    next_entry_idx: usize,
    entry_len: usize,
  },
}

#[derive(Debug)]
struct SerializeFrame {
  obj: GcObject,
  node_id: NodeId,
  kind: SerializeFrameKind,
}

fn serialize_value_iterative(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut SerializeState,
  value: Value,
) -> Result<EncodedValue, VmError> {
  let (root, root_frame) = serialize_value_shallow(vm, scope, host, hooks, state, value)?;
  let mut stack: Vec<SerializeFrame> = Vec::new();
  if let Some(frame) = root_frame {
    stack.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    stack.push(frame);
  }

  'frames: while let Some(mut frame) = stack.pop() {
    // Tick once per frame so even empty objects participate in budget/interrupt checks.
    vm.tick()?;

    match &mut frame.kind {
      SerializeFrameKind::Props { keys, next_key_idx } => {
        while *next_key_idx < keys.len() {
          // Tick at least once per processed property so a large clone can't bypass budgets.
          vm.tick()?;

          let key = keys[*next_key_idx];
          *next_key_idx += 1;

          let PropertyKey::String(key_s) = key else {
            continue;
          };

          let Some(desc) = scope
            .heap()
            .object_get_own_property_with_tick(frame.obj, &key, || vm.tick())?
          else {
            continue;
          };
          if !desc.enumerable {
            continue;
          }

          state.count_prop(vm, scope)?;

          // `Get` can invoke user code, but `vm-js` roots the key for the duration of the operation.
          let prop_val = vm.get_with_host_and_hooks(host, scope, hooks, frame.obj, key)?;
          let (encoded, child_frame) = serialize_value_shallow(vm, scope, host, hooks, state, prop_val)?;

          match state.nodes.get_mut(frame.node_id) {
            Some(Node::Array { props, .. } | Node::Object { props }) => props.push((key_s, encoded)),
            _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
          }

          if let Some(child_frame) = child_frame {
            // Depth-first traversal: resume this frame after we serialize the nested object.
            stack.try_reserve(2).map_err(|_| VmError::OutOfMemory)?;
            stack.push(frame);
            stack.push(child_frame);
            continue 'frames;
          }
        }
      }
      SerializeFrameKind::MapEntries {
        next_entry_idx,
        entry_len,
      } => {
        while *next_entry_idx < *entry_len {
          // Tick at least once per processed entry so a large clone can't bypass budgets.
          vm.tick()?;

          let idx = *next_entry_idx;
          *next_entry_idx += 1;

          let Some((key, value)) = scope.heap().map_entry_at(frame.obj, idx)? else {
            continue;
          };

          state.count_prop(vm, scope)?;

          let (key_encoded, key_frame) = serialize_value_shallow(vm, scope, host, hooks, state, key)?;
          let (value_encoded, value_frame) =
            serialize_value_shallow(vm, scope, host, hooks, state, value)?;

          match state.nodes.get_mut(frame.node_id) {
            Some(Node::Map { entries }) => entries.push((key_encoded, value_encoded)),
            _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
          }

          if key_frame.is_some() || value_frame.is_some() {
            // Depth-first traversal: serialize nested objects before continuing with sibling entries.
            stack
              .try_reserve(1 + (key_frame.is_some() as usize) + (value_frame.is_some() as usize))
              .map_err(|_| VmError::OutOfMemory)?;
            stack.push(frame);
            if let Some(value_frame) = value_frame {
              stack.push(value_frame);
            }
            if let Some(key_frame) = key_frame {
              stack.push(key_frame);
            }
            continue 'frames;
          }
        }
      }
      SerializeFrameKind::SetEntries {
        next_entry_idx,
        entry_len,
      } => {
        while *next_entry_idx < *entry_len {
          // Tick at least once per processed entry so a large clone can't bypass budgets.
          vm.tick()?;

          let idx = *next_entry_idx;
          *next_entry_idx += 1;

          let Some(value) = scope.heap().set_entry_at(frame.obj, idx)? else {
            continue;
          };

          state.count_prop(vm, scope)?;

          let (encoded, child_frame) = serialize_value_shallow(vm, scope, host, hooks, state, value)?;

          match state.nodes.get_mut(frame.node_id) {
            Some(Node::Set { entries }) => entries.push(encoded),
            _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
          }

          if let Some(child_frame) = child_frame {
            // Depth-first traversal: serialize nested objects before continuing with sibling entries.
            stack.try_reserve(2).map_err(|_| VmError::OutOfMemory)?;
            stack.push(frame);
            stack.push(child_frame);
            continue 'frames;
          }
        }
      }
    }
  }

  Ok(root)
}

fn serialize_value_shallow(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut SerializeState,
  value: Value,
) -> Result<(EncodedValue, Option<SerializeFrame>), VmError> {
  match value {
    Value::Undefined
    | Value::Null
    | Value::Bool(_)
    | Value::Number(_)
    | Value::BigInt(_)
    | Value::String(_) => Ok((EncodedValue::Primitive(value), None)),
    Value::Symbol(_) => Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone Symbol",
    )),
    Value::Object(obj) => {
      let (id, frame) = serialize_object_shallow(vm, scope, host, hooks, state, obj)?;
      Ok((EncodedValue::Object(id), frame))
    }
  }
}

fn is_platform_object(scope: &Scope<'_>, obj: GcObject) -> Result<bool, VmError> {
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  Ok(slots.is_some())
}

fn utf16_units_eq_str(units: &[u16], expected: &str) -> bool {
  let mut it = expected.encode_utf16();
  for &u in units {
    match it.next() {
      Some(e) if e == u => {}
      _ => return false,
    }
  }
  it.next().is_none()
}

fn gc_string_eq_str(scope: &Scope<'_>, s: GcString, expected: &str) -> Result<bool, VmError> {
  let units = scope.heap().get_string(s)?.as_code_units();
  Ok(utf16_units_eq_str(units, expected))
}

fn copy_gc_string_utf16(scope: &Scope<'_>, s: GcString) -> Result<Vec<u16>, VmError> {
  let units = scope.heap().get_string(s)?.as_code_units();
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(units);
  Ok(out)
}

fn error_tag_from_utf16(units: &[u16]) -> Option<ErrorTag> {
  if utf16_units_eq_str(units, "Error") {
    Some(ErrorTag::Error)
  } else if utf16_units_eq_str(units, "EvalError") {
    Some(ErrorTag::EvalError)
  } else if utf16_units_eq_str(units, "RangeError") {
    Some(ErrorTag::RangeError)
  } else if utf16_units_eq_str(units, "ReferenceError") {
    Some(ErrorTag::ReferenceError)
  } else if utf16_units_eq_str(units, "SyntaxError") {
    Some(ErrorTag::SyntaxError)
  } else if utf16_units_eq_str(units, "TypeError") {
    Some(ErrorTag::TypeError)
  } else if utf16_units_eq_str(units, "URIError") {
    Some(ErrorTag::URIError)
  } else {
    None
  }
}

fn error_kind_for_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  intr: Intrinsics,
  obj: GcObject,
) -> Result<Option<ErrorTag>, VmError> {
  let mut current = scope.heap().object_prototype(obj)?;
  let mut steps = 0usize;
  while let Some(proto) = current {
    vm.tick()?;
    steps += 1;
    if steps > 1024 {
      return Err(VmError::PrototypeChainTooDeep);
    }

    if proto == intr.eval_error_prototype() {
      return Ok(Some(ErrorTag::EvalError));
    }
    if proto == intr.range_error_prototype() {
      return Ok(Some(ErrorTag::RangeError));
    }
    if proto == intr.reference_error_prototype() {
      return Ok(Some(ErrorTag::ReferenceError));
    }
    if proto == intr.syntax_error_prototype() {
      return Ok(Some(ErrorTag::SyntaxError));
    }
    if proto == intr.type_error_prototype() {
      return Ok(Some(ErrorTag::TypeError));
    }
    if proto == intr.uri_error_prototype() {
      return Ok(Some(ErrorTag::URIError));
    }
    if proto == intr.error_prototype() {
      return Ok(Some(ErrorTag::Error));
    }

    current = scope.heap().object_prototype(proto)?;
  }
  Ok(None)
}

fn serialize_array_buffer_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  state: &mut SerializeState,
  obj: GcObject,
) -> Result<NodeId, VmError> {
  if let Some(id) = state.object_to_id.get(&obj).copied() {
    return Ok(id);
  }

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
  state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
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

  Ok(id)
}

fn serialize_object_shallow(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut SerializeState,
  obj: GcObject,
) -> Result<(NodeId, Option<SerializeFrame>), VmError> {
  if let Some(id) = state.object_to_id.get(&obj).copied() {
    return Ok((id, None));
  }

  // Root the source object for the remainder of the clone so any serialized `Gc*` handles (e.g.
  // property keys) remain valid through deserialization.
  scope.push_root(Value::Object(obj))?;

  fn try_serialize_blob_object(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    state: &mut SerializeState,
    obj: GcObject,
  ) -> Result<Option<NodeId>, VmError> {
    let Some(data) = clone_blob_data_for_fetch(vm, scope.heap(), Value::Object(obj))? else {
      return Ok(None);
    };
    state.add_copied_bytes(vm, scope, data.bytes.len())?;
    let id = state.push_node(Node::Blob { data: Some(data) }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    Ok(Some(id))
  }

  // Reject platform objects branded via `HostSlots` (e.g. DOM helper interface objects).
  //
  // Note: `object_host_slots` is only implemented for a subset of heap object kinds (notably not
  // `RegExp`), so use a defensive wrapper.
  if is_platform_object(scope, obj)? {
    // `Blob` is a platform object but structured-cloneable; allow it even if `HostSlots`-branded.
    if let Some(id) = try_serialize_blob_object(vm, scope, state, obj)? {
      return Ok((id, None));
    }
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone platform object",
    ));
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

  // Reject exotic / internal-slot objects that HTML structured cloning does not serialize.
  if scope.heap().is_proxy_object(obj) {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone Proxy",
    ));
  }
  if scope.heap().is_weak_map_object(obj) {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone WeakMap",
    ));
  }
  if scope.heap().is_weak_set_object(obj) {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone WeakSet",
    ));
  }
  if scope.heap().is_generator_object(obj) {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone Generator",
    ));
  }

  // Map.
  if scope.heap().is_map_object(obj) {
    let entry_len = scope.heap().map_entries_len(obj)?;
    let size = scope.heap().map_size(obj)?;
    let mut entries: Vec<(EncodedValue, EncodedValue)> = Vec::new();
    entries
      .try_reserve_exact(size)
      .map_err(|_| VmError::OutOfMemory)?;
    let id = state.push_node(Node::Map { entries }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    let frame = SerializeFrame {
      obj,
      node_id: id,
      kind: SerializeFrameKind::MapEntries {
        next_entry_idx: 0,
        entry_len,
      },
    };
    return Ok((id, Some(frame)));
  }

  // Set.
  if scope.heap().is_set_object(obj) {
    let entry_len = scope.heap().set_entries_len(obj)?;
    let size = scope.heap().set_size(obj)?;
    let mut entries: Vec<EncodedValue> = Vec::new();
    entries
      .try_reserve_exact(size)
      .map_err(|_| VmError::OutOfMemory)?;
    let id = state.push_node(Node::Set { entries }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    let frame = SerializeFrame {
      obj,
      node_id: id,
      kind: SerializeFrameKind::SetEntries {
        next_entry_idx: 0,
        entry_len,
      },
    };
    return Ok((id, Some(frame)));
  }

  // ArrayBuffer.
  if scope.heap().is_array_buffer_object(obj) {
    let id = serialize_array_buffer_object(vm, scope, state, obj)?;
    return Ok((id, None));
  }

  // Typed arrays.
  if scope.heap().is_typed_array_object(obj) {
    // Use typed array internal slots directly (not JS-visible properties) to avoid invoking user
    // code and to ignore shadowed properties on the instance/prototype chain.
    let kind = scope.heap().typed_array_kind(obj)?;
    let buffer_obj = scope.heap().typed_array_buffer(obj)?;
    let byte_offset = scope.heap().typed_array_byte_offset(obj)?;
    let length = scope.heap().typed_array_length(obj)?;

    // Ensure the backing buffer is serialized before creating the view node (for easier
    // deserialization).
    if !state.object_to_id.contains_key(&buffer_obj) {
      scope.push_root(Value::Object(buffer_obj))?;
    }
    let buffer_id = serialize_array_buffer_object(vm, scope, state, buffer_obj)?;

    let id = state.push_node(
      Node::TypedArray {
        kind,
        buffer: buffer_id,
        byte_offset,
        length,
      },
      vm,
      scope,
    )?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  // DataView.
  if scope.heap().is_data_view_object(obj) {
    // Use internal slots directly (not JS-visible properties) to avoid invoking user code and to
    // ignore shadowed properties on the instance/prototype chain.
    let buffer_obj = scope.heap().data_view_buffer(obj)?;
    let byte_offset = scope.heap().data_view_byte_offset(obj)?;
    let byte_length = scope.heap().data_view_byte_length(obj)?;

    if !state.object_to_id.contains_key(&buffer_obj) {
      scope.push_root(Value::Object(buffer_obj))?;
    }
    let buffer_id = serialize_array_buffer_object(vm, scope, state, buffer_obj)?;

    let id = state.push_node(
      Node::DataView {
        buffer: buffer_id,
        byte_offset,
        byte_length,
      },
      vm,
      scope,
    )?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  // Date.
  if let Some(time) = scope.heap().date_value(obj)? {
    let id = state.push_node(Node::Date { time }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  // RegExp.
  if scope.heap().is_regexp_object(obj) {
    let original_source = scope.heap().regexp_original_source(obj)?;
    let original_flags = scope.heap().regexp_original_flags(obj)?;
    let flags = scope.heap().regexp_flags(obj)?;
    let program = scope.heap().regexp_program(obj)?.try_clone()?;

    let id = state.push_node(
      Node::RegExp {
        original_source,
        original_flags,
        flags,
        program: Some(program),
      },
      vm,
      scope,
    )?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  // Blob.
  if let Some(id) = try_serialize_blob_object(vm, scope, state, obj)? {
    return Ok((id, None));
  }

  // Boxed Symbol objects.
  //
  // `vm-js` represents `Object(Symbol('x'))` using a non-enumerable internal marker symbol property
  // holding the primitive Symbol value. HTML structured cloning rejects Symbols, including boxed
  // wrapper objects.
  let symbol_marker_key = PropertyKey::from_symbol(state.markers.symbol_data);
  if scope
    .heap()
    .object_get_own_data_property_value(obj, &symbol_marker_key)?
    .is_some()
  {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone Symbol object",
    ));
  }

  // Boxed primitives.
  let boolean_marker_key = PropertyKey::from_symbol(state.markers.boolean_data);
  if let Some(Value::Bool(b)) = scope
    .heap()
    .object_get_own_data_property_value(obj, &boolean_marker_key)?
  {
    let id = state.push_node(Node::BooleanObject { value: b }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  let number_marker_key = PropertyKey::from_symbol(state.markers.number_data);
  if let Some(Value::Number(n)) = scope
    .heap()
    .object_get_own_data_property_value(obj, &number_marker_key)?
  {
    let id = state.push_node(Node::NumberObject { value: n }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  let string_marker_key = PropertyKey::from_symbol(state.markers.string_data);
  if let Some(Value::String(s)) = scope
    .heap()
    .object_get_own_data_property_value(obj, &string_marker_key)?
  {
    let id = state.push_node(Node::StringObject { value: s }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  let bigint_marker_key = PropertyKey::from_symbol(state.markers.bigint_data);
  if let Some(v @ Value::BigInt(_)) = scope
    .heap()
    .object_get_own_data_property_value(obj, &bigint_marker_key)?
  {
    let id = state.push_node(Node::BigIntObject { value: v }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    return Ok((id, None));
  }

  // Error objects.
  if let Some(intr) = vm.intrinsics() {
    if error_kind_for_object(vm, scope, intr, obj)?.is_some() {
      let name_value = vm.get_with_host_and_hooks(host, scope, hooks, obj, state.error_name_key)?;
      let name = match name_value {
        Value::Undefined | Value::Null => ErrorTag::Error,
        Value::String(s) => error_tag_from_utf16(scope.heap().get_string(s)?.as_code_units())
          .unwrap_or(ErrorTag::Error),
        other => {
          let s = scope.to_string(vm, host, hooks, other)?;
          scope.push_root(Value::String(s))?;
          error_tag_from_utf16(scope.heap().get_string(s)?.as_code_units()).unwrap_or(ErrorTag::Error)
        }
      };

      // HTML structured clone reads the message from the *own* "message" data property descriptor,
      // and does not invoke an accessor.
      let message_desc = scope
        .heap()
        .object_get_own_property(obj, &state.error_message_key)?;
      let message = match message_desc {
        Some(PropertyDescriptor {
          kind: PropertyKind::Data { value, .. },
          ..
        }) => {
          let s = scope.to_string(vm, host, hooks, value)?;
          Some(copy_gc_string_utf16(scope, s)?)
        }
        _ => None,
      };

      let id = state.push_node(
        Node::Error {
          name,
          message,
        },
        vm,
        scope,
      )?;
      state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.object_to_id.insert(obj, id);
      return Ok((id, None));
    }
  }

  // RegExp String Iterator (from `RegExp.prototype[@@matchAll]`) is not structured-cloneable.
  // `vm-js` represents the iterator's internal slots as `Symbol.for(..)`-keyed data properties.
  let regexp_iterating_regexp_key =
    PropertyKey::from_symbol(state.markers.regexp_string_iterator_iterating_regexp);
  let regexp_iterated_string_key =
    PropertyKey::from_symbol(state.markers.regexp_string_iterator_iterated_string);
  let regexp_done_key = PropertyKey::from_symbol(state.markers.regexp_string_iterator_done);
  if scope
    .heap()
    .object_get_own_data_property_value(obj, &regexp_iterating_regexp_key)?
    .is_some()
    || scope
      .heap()
      .object_get_own_data_property_value(obj, &regexp_iterated_string_key)?
      .is_some()
    || scope
      .heap()
      .object_get_own_data_property_value(obj, &regexp_done_key)?
      .is_some()
  {
    return Err(throw_data_clone_error(
      vm,
      scope,
      state.global,
      "structuredClone: cannot clone RegExp String Iterator",
    ));
  }

  // Array / ordinary object.
  let is_array = scope.heap().object_is_array(obj)?;

  if is_array {
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
    let keys = scope.heap().own_property_keys(obj)?;
    let mut props: Vec<(GcString, EncodedValue)> = Vec::new();
    props
      .try_reserve_exact(keys.len())
      .map_err(|_| VmError::OutOfMemory)?;
    let id = state.push_node(Node::Array { length, props }, vm, scope)?;
    state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.object_to_id.insert(obj, id);
    let frame = SerializeFrame {
      obj,
      node_id: id,
      kind: SerializeFrameKind::Props {
        keys,
        next_key_idx: 0,
      },
    };
    return Ok((id, Some(frame)));
  };

  let keys = scope.heap().own_property_keys(obj)?;
  let mut props: Vec<(GcString, EncodedValue)> = Vec::new();
  props
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  let id = state.push_node(Node::Object { props }, vm, scope)?;
  state.object_to_id.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  state.object_to_id.insert(obj, id);
  let frame = SerializeFrame {
    obj,
    node_id: id,
    kind: SerializeFrameKind::Props {
      keys,
      next_key_idx: 0,
    },
  };
  Ok((id, Some(frame)))
}

fn prepare_transfer_list_buffers(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global: GcObject,
  transfer_list: &[GcObject],
  nodes: &[Node],
) -> Result<HashMap<GcObject, GcObject>, VmError> {
  // Only transfer buffers that are actually referenced by the serialized node graph. Per HTML
  // semantics, *all* transfer list items must be detached, even if they don't appear in `value`.
  let mut referenced: HashSet<GcObject> = HashSet::new();
  referenced
    .try_reserve(transfer_list.len().min(nodes.len()))
    .map_err(|_| VmError::OutOfMemory)?;
  for (i, node) in nodes.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let Node::ArrayBuffer {
      source,
      transferred: true,
      ..
    } = node
    else {
      continue;
    };
    referenced.insert(*source);
  }

  let mut out: HashMap<GcObject, GcObject> = HashMap::new();
  out
    .try_reserve(referenced.len())
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

    if referenced.contains(&buf) {
      let dst = match scope.heap_mut().transfer_array_buffer(buf) {
        Ok(dst) => dst,
        Err(VmError::OutOfMemory) => return Err(VmError::OutOfMemory),
        Err(_) => {
          return Err(throw_data_clone_error(
            vm,
            scope,
            global,
            "structuredClone: failed to transfer ArrayBuffer",
          ));
        }
      };
      // Root the destination buffer until it is attached into the cloned object graph.
      scope.push_root(Value::Object(dst))?;
      out.insert(buf, dst);
    } else {
      // Unreferenced transfer list items must still be detached (postMessage semantics).
      //
      // We detach-and-drop the backing store immediately to avoid holding bytes outside the heap
      // (and outside `external_bytes` accounting) during the remainder of the clone.
      match scope.heap_mut().detach_array_buffer_take_data(buf) {
        Ok(Some(_data)) => {}
        Ok(None) => {
          return Err(throw_data_clone_error(
            vm,
            scope,
            global,
            "structuredClone: transfer list contains detached ArrayBuffer",
          ));
        }
        Err(VmError::OutOfMemory) => return Err(VmError::OutOfMemory),
        Err(_) => {
          return Err(throw_data_clone_error(
            vm,
            scope,
            global,
            "structuredClone: failed to detach ArrayBuffer",
          ));
        }
      }
    }
  }

  Ok(out)
}

#[derive(Debug)]
enum DeserializeFrameKind {
  Props {
    props: Vec<(GcString, EncodedValue)>,
    next_prop_idx: usize,
  },
  MapEntries {
    entries: Vec<(EncodedValue, EncodedValue)>,
    next_entry_idx: usize,
  },
  SetEntries {
    entries: Vec<EncodedValue>,
    next_entry_idx: usize,
  },
}

#[derive(Debug)]
struct DeserializeFrame {
  dst: GcObject,
  kind: DeserializeFrameKind,
}

fn deserialize_value_iterative(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut DeserializeState,
  callee: GcObject,
  value: EncodedValue,
) -> Result<Value, VmError> {
  let (root, root_frame) = deserialize_value_shallow(vm, scope, host, hooks, state, callee, value)?;
  let mut stack: Vec<DeserializeFrame> = Vec::new();
  if let Some(frame) = root_frame {
    stack.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    stack.push(frame);
  }

  'frames: while let Some(mut frame) = stack.pop() {
    vm.tick()?;

    match &mut frame.kind {
      DeserializeFrameKind::Props {
        props,
        next_prop_idx,
      } => {
        while *next_prop_idx < props.len() {
          vm.tick()?;
          let (key_s, val) = props[*next_prop_idx];
          *next_prop_idx += 1;

          let (cloned_val, child_frame) =
            deserialize_value_shallow(vm, scope, host, hooks, state, callee, val)?;
          let key = PropertyKey::from_string(key_s);
          scope.define_property(
            frame.dst,
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

          if let Some(child_frame) = child_frame {
            // Depth-first traversal: populate nested objects before continuing with sibling properties.
            stack.try_reserve(2).map_err(|_| VmError::OutOfMemory)?;
            stack.push(frame);
            stack.push(child_frame);
            continue 'frames;
          }
        }
      }
      DeserializeFrameKind::MapEntries {
        entries,
        next_entry_idx,
      } => {
        while *next_entry_idx < entries.len() {
          vm.tick()?;
          let (key, value) = entries[*next_entry_idx];
          *next_entry_idx += 1;

          let (cloned_key, key_frame) =
            deserialize_value_shallow(vm, scope, host, hooks, state, callee, key)?;
          let (cloned_value, value_frame) =
            deserialize_value_shallow(vm, scope, host, hooks, state, callee, value)?;

          scope
            .heap_mut()
            .map_set_with_tick(frame.dst, cloned_key, cloned_value, || vm.tick())?;

          if key_frame.is_some() || value_frame.is_some() {
            // Depth-first traversal: populate nested objects before continuing with sibling entries.
            stack
              .try_reserve(1 + (key_frame.is_some() as usize) + (value_frame.is_some() as usize))
              .map_err(|_| VmError::OutOfMemory)?;
            stack.push(frame);
            if let Some(value_frame) = value_frame {
              stack.push(value_frame);
            }
            if let Some(key_frame) = key_frame {
              stack.push(key_frame);
            }
            continue 'frames;
          }
        }
      }
      DeserializeFrameKind::SetEntries {
        entries,
        next_entry_idx,
      } => {
        while *next_entry_idx < entries.len() {
          vm.tick()?;
          let value = entries[*next_entry_idx];
          *next_entry_idx += 1;

          let (cloned_value, child_frame) =
            deserialize_value_shallow(vm, scope, host, hooks, state, callee, value)?;
          scope
            .heap_mut()
            .set_add_with_tick(frame.dst, cloned_value, || vm.tick())?;

          if let Some(child_frame) = child_frame {
            // Depth-first traversal: populate nested objects before continuing with sibling entries.
            stack.try_reserve(2).map_err(|_| VmError::OutOfMemory)?;
            stack.push(frame);
            stack.push(child_frame);
            continue 'frames;
          }
        }
      }
    }
  }

  Ok(root)
}

fn deserialize_value_shallow(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut DeserializeState,
  callee: GcObject,
  value: EncodedValue,
) -> Result<(Value, Option<DeserializeFrame>), VmError> {
  match value {
    EncodedValue::Primitive(v) => Ok((v, None)),
    EncodedValue::Object(id) => {
      let (obj, frame) = deserialize_node_shallow(vm, scope, host, hooks, state, callee, id)?;
      Ok((Value::Object(obj), frame))
    }
  }
}

fn deserialize_node_shallow(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  state: &mut DeserializeState,
  callee: GcObject,
  id: NodeId,
) -> Result<(GcObject, Option<DeserializeFrame>), VmError> {
  if let Some(obj) = state.clones.get(&id).copied() {
    return Ok((obj, None));
  }

  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "structuredClone requires intrinsics (create a Realm first)",
  ))?;

  // Allocate a placeholder object first (for cycle support), then (if needed) return a frame to
  // populate its enumerable properties.
  let kind_tag = match state.nodes.get(id) {
    Some(Node::Array { .. }) => 1u8,
    Some(Node::Object { .. }) => 2u8,
    Some(Node::ArrayBuffer { .. }) => 3u8,
    Some(Node::TypedArray { .. }) => 4u8,
    Some(Node::Date { .. }) => 5u8,
    Some(Node::Blob { .. }) => 6u8,
    Some(Node::BooleanObject { .. }) => 7u8,
    Some(Node::NumberObject { .. }) => 8u8,
    Some(Node::StringObject { .. }) => 9u8,
    Some(Node::BigIntObject { .. }) => 10u8,
    Some(Node::Error { .. }) => 11u8,
    Some(Node::RegExp { .. }) => 12u8,
    Some(Node::Map { .. }) => 13u8,
    Some(Node::Set { .. }) => 14u8,
    Some(Node::DataView { .. }) => 15u8,
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
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, arr);

      let props = match state.nodes.get_mut(id) {
        Some(Node::Array { props, .. }) => std::mem::take(props),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let frame = DeserializeFrame {
        dst: arr,
        kind: DeserializeFrameKind::Props {
          props,
          next_prop_idx: 0,
        },
      };
      return Ok((arr, Some(frame)));
    }
    2 => {
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.object_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, obj);

      let props = match state.nodes.get_mut(id) {
        Some(Node::Object { props }) => std::mem::take(props),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let frame = DeserializeFrame {
        dst: obj,
        kind: DeserializeFrameKind::Props {
          props,
          next_prop_idx: 0,
        },
      };
      return Ok((obj, Some(frame)));
    }
    13 => {
      // Map.
      let map = scope.alloc_map()?;
      scope.push_root(Value::Object(map))?;
      scope
        .heap_mut()
        .object_set_prototype(map, Some(intr.map_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, map);

      let entries = match state.nodes.get_mut(id) {
        Some(Node::Map { entries }) => std::mem::take(entries),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let frame = DeserializeFrame {
        dst: map,
        kind: DeserializeFrameKind::MapEntries {
          entries,
          next_entry_idx: 0,
        },
      };
      return Ok((map, Some(frame)));
    }
    14 => {
      // Set.
      let set = scope.alloc_set()?;
      scope.push_root(Value::Object(set))?;
      scope
        .heap_mut()
        .object_set_prototype(set, Some(intr.set_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, set);

      let entries = match state.nodes.get_mut(id) {
        Some(Node::Set { entries }) => std::mem::take(entries),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let frame = DeserializeFrame {
        dst: set,
        kind: DeserializeFrameKind::SetEntries {
          entries,
          next_entry_idx: 0,
        },
      };
      return Ok((set, Some(frame)));
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

      let ab = if transferred {
        let dst = state
          .transfer_data
          .remove(&source)
          .ok_or(VmError::InvariantViolation(
            "structuredClone missing transferred ArrayBuffer destination",
          ))?;
        scope
          .heap_mut()
          .object_set_prototype(dst, Some(intr.array_buffer_prototype()))?;
        // Root defensively: transfer list buffers were pre-rooted, but keeping this local invariant
        // makes deserialization safer if transfer handling changes in the future.
        scope.push_root(Value::Object(dst))?;
        dst
      } else {
        let data_vec =
          bytes.ok_or(VmError::InvariantViolation("structuredClone missing ArrayBuffer bytes"))?;

        let ab = scope.alloc_array_buffer_from_u8_vec(data_vec)?;
        scope.push_root(Value::Object(ab))?;
        scope
          .heap_mut()
          .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
        ab
      };

      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, ab);
      ab
    }
    4 => {
      // TypedArray.
      let (kind, buffer_id, byte_offset, length) = match state.nodes.get(id) {
        Some(Node::TypedArray {
          kind,
          buffer,
          byte_offset,
          length,
        }) => (*kind, *buffer, *byte_offset, *length),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let (buffer_obj, buffer_frame) =
        deserialize_node_shallow(vm, scope, host, hooks, state, callee, buffer_id)?;
      if buffer_frame.is_some() {
        return Err(VmError::InvariantViolation(
          "structuredClone typed array buffer must not require property population",
        ));
      }

      let (ctor, proto) = match kind {
        TypedArrayKind::Int8 => (intr.int8_array(), intr.int8_array_prototype()),
        TypedArrayKind::Uint8 => (intr.uint8_array(), intr.uint8_array_prototype()),
        TypedArrayKind::Uint8Clamped => (
          intr.uint8_clamped_array(),
          intr.uint8_clamped_array_prototype(),
        ),
        TypedArrayKind::Int16 => (intr.int16_array(), intr.int16_array_prototype()),
        TypedArrayKind::Uint16 => (intr.uint16_array(), intr.uint16_array_prototype()),
        TypedArrayKind::Int32 => (intr.int32_array(), intr.int32_array_prototype()),
        TypedArrayKind::Uint32 => (intr.uint32_array(), intr.uint32_array_prototype()),
        TypedArrayKind::Float32 => (intr.float32_array(), intr.float32_array_prototype()),
        TypedArrayKind::Float64 => (intr.float64_array(), intr.float64_array_prototype()),
        TypedArrayKind::BigInt64 => (intr.bigint64_array(), intr.bigint64_array_prototype()),
        TypedArrayKind::BigUint64 => (intr.biguint64_array(), intr.biguint64_array_prototype()),
      };

      let args = [
        Value::Object(buffer_obj),
        Value::Number(byte_offset as f64),
        Value::Number(length as f64),
      ];
      let view_val = vm.construct_with_host_and_hooks(
        host,
        scope,
        hooks,
        Value::Object(ctor),
        &args,
        Value::Object(ctor),
      )?;
      let Value::Object(view) = view_val else {
        return Err(VmError::InvariantViolation(
          "structuredClone typed array constructor returned non-object",
        ));
      };
      scope.push_root(Value::Object(view))?;
      // Defensively set the prototype to the intrinsic typed array prototype to ensure the clone is
      // correct even if user code tampered with the constructor or its `prototype` property.
      scope.heap_mut().object_set_prototype(view, Some(proto))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
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
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
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
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, blob_obj);
      blob_obj
    }
    7 => {
      // Boolean object.
      let value = match state.nodes.get(id) {
        Some(Node::BooleanObject { value }) => *value,
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };

      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.boolean_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, obj);

      let marker_key = PropertyKey::from_symbol(state.markers.boolean_data);
      scope.define_property(
        obj,
        marker_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Bool(value),
            writable: true,
          },
        },
      )?;

      obj
    }
    8 => {
      // Number object.
      let value = match state.nodes.get(id) {
        Some(Node::NumberObject { value }) => *value,
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };

      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.number_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, obj);

      let marker_key = PropertyKey::from_symbol(state.markers.number_data);
      scope.define_property(
        obj,
        marker_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Number(value),
            writable: true,
          },
        },
      )?;

      obj
    }
    9 => {
      // String object.
      let value = match state.nodes.get(id) {
        Some(Node::StringObject { value }) => *value,
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };

      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.string_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, obj);

      let marker_key = PropertyKey::from_symbol(state.markers.string_data);
      scope.define_property(
        obj,
        marker_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::String(value),
            writable: true,
          },
        },
      )?;

      obj
    }
    10 => {
      // BigInt object.
      let value = match state.nodes.get(id) {
        Some(Node::BigIntObject { value }) => *value,
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };

      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.bigint_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, obj);

      let marker_key = PropertyKey::from_symbol(state.markers.bigint_data);
      scope.define_property(
        obj,
        marker_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value,
            writable: true,
          },
        },
      )?;

      obj
    }
    11 => {
      // Error object.
      let (name, message) = match state.nodes.get_mut(id) {
        Some(Node::Error { name, message }) => (*name, message.take()),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };

      let proto = name.prototype(intr);
      let obj = scope.alloc_error()?;
      scope.push_root(Value::Object(obj))?;
      scope.heap_mut().object_set_prototype(obj, Some(proto))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, obj);

      let name_s = scope.alloc_string(name.as_str())?;
      scope.push_root(Value::String(name_s))?;
      let name_key = alloc_key(scope, "name")?;
      scope.define_property(obj, name_key, data_desc(Value::String(name_s)))?;

      if let Some(units) = message {
        let s = scope.alloc_string_from_u16_vec(units)?;
        scope.push_root(Value::String(s))?;
        let message_key = alloc_key(scope, "message")?;
        scope.define_property(obj, message_key, data_desc(Value::String(s)))?;
      }

      obj
    }
    12 => {
      // RegExp.
      let (original_source, original_flags, flags, program) = match state.nodes.get_mut(id) {
        Some(Node::RegExp {
          original_source,
          original_flags,
          flags,
          program,
        }) => (
          *original_source,
          *original_flags,
          *flags,
          program.take(),
        ),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let program =
        program.ok_or(VmError::InvariantViolation("structuredClone missing RegExp program"))?;

      let re = scope.alloc_regexp(original_source, original_flags, flags, program)?;
      scope.push_root(Value::Object(re))?;
      scope
        .heap_mut()
        .object_set_prototype(re, Some(intr.regexp_prototype()))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, re);
      re
    }
    15 => {
      // DataView.
      let (buffer_id, byte_offset, byte_length) = match state.nodes.get(id) {
        Some(Node::DataView {
          buffer,
          byte_offset,
          byte_length,
        }) => (*buffer, *byte_offset, *byte_length),
        _ => return Err(VmError::InvariantViolation("structuredClone node kind mismatch")),
      };
      let (buffer_obj, buffer_frame) =
        deserialize_node_shallow(vm, scope, host, hooks, state, callee, buffer_id)?;
      if buffer_frame.is_some() {
        return Err(VmError::InvariantViolation(
          "structuredClone DataView buffer must not require property population",
        ));
      }

      let ctor = intr.data_view();
      let proto = intr.data_view_prototype();
      let args = [
        Value::Object(buffer_obj),
        Value::Number(byte_offset as f64),
        Value::Number(byte_length as f64),
      ];
      let view_val = vm.construct_with_host_and_hooks(
        host,
        scope,
        hooks,
        Value::Object(ctor),
        &args,
        Value::Object(ctor),
      )?;
      let Value::Object(view) = view_val else {
        return Err(VmError::InvariantViolation(
          "structuredClone DataView constructor returned non-object",
        ));
      };
      scope.push_root(Value::Object(view))?;
      // Defensively set the prototype to the intrinsic DataView prototype to ensure the clone is
      // correct even if user code tampered with the constructor or its `prototype` property.
      scope.heap_mut().object_set_prototype(view, Some(proto))?;
      state.clones.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      state.clones.insert(id, view);
      view
    }
    _ => return Err(VmError::InvariantViolation("structuredClone invalid node kind tag")),
  };

  Ok((obj, None))
}

fn throw_data_clone_error(vm: &mut Vm, scope: &mut Scope<'_>, global: GcObject, message: &str) -> VmError {
  if let Some(intr) = vm.intrinsics() {
    if let Ok(dom_exception) = DomExceptionClassVmJs::install_for_global(vm, scope, global, intr) {
      if let Ok(err) = dom_exception.new_instance(scope, "DataCloneError", message) {
        return VmError::Throw(err);
      }
    }
  }
  match make_dom_exception(vm, scope, "DataCloneError", message) {
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
  use crate::dom2;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use crate::js::window::WindowHost;
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;
  use vm_js::{HostSlots, Value};

  fn get_string(realm: &WindowRealm, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    realm.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> crate::error::Result<WindowHost> {
    WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
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
  fn structured_clone_maps_sets_use_intrinsics_under_global_tampering() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const RealMap = Map;\
         const RealSet = Set;\
         const m = new RealMap([[1, 2]]);\
         const s = new RealSet([1, 2]);\
         globalThis.Map = function () { throw new Error('tampered Map'); };\
         globalThis.Set = function () { throw new Error('tampered Set'); };\
         const cm = structuredClone(m);\
         if (cm === m) return false;\
         if (!(cm instanceof RealMap)) return false;\
         if (Object.getPrototypeOf(cm) !== RealMap.prototype) return false;\
         if (cm.size !== 1) return false;\
         if (cm.get(1) !== 2) return false;\
         const cs = structuredClone(s);\
         if (cs === s) return false;\
         if (!(cs instanceof RealSet)) return false;\
         if (Object.getPrototypeOf(cs) !== RealSet.prototype) return false;\
         if (cs.size !== 2) return false;\
         if (!cs.has(1) || !cs.has(2)) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_maps_sets_prototype_and_instanceof() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const m = new Map();\
         const cm = structuredClone(m);\
         if (cm === m) return false;\
         if (!(cm instanceof Map)) return false;\
         if (Object.getPrototypeOf(cm) !== Map.prototype) return false;\
         const s = new Set();\
         const cs = structuredClone(s);\
         if (cs === s) return false;\
         if (!(cs instanceof Set)) return false;\
         if (Object.getPrototypeOf(cs) !== Set.prototype) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_map_preserves_shared_identity() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const shared = {x: 1};\
         const m = new Map([['a', shared], ['b', shared]]);\
         const c = structuredClone(m);\
         if (c === m) return false;\
         if (!(c instanceof Map)) return false;\
         if (Object.getPrototypeOf(c) !== Map.prototype) return false;\
         const a = c.get('a');\
         const b = c.get('b');\
         if (a !== b) return false;\
         if (a === shared) return false;\
         if (b === shared) return false;\
         return a.x === 1;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_maps_sets_support_cycles() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const m = new Map();\
         m.set('self', m);\
         const cm = structuredClone(m);\
         if (cm === m) return false;\
         if (!(cm instanceof Map)) return false;\
         if (Object.getPrototypeOf(cm) !== Map.prototype) return false;\
         if (cm.get('self') !== cm) return false;\
         const s = new Set();\
         s.add(s);\
         const cs = structuredClone(s);\
         if (cs === s) return false;\
         if (!(cs instanceof Set)) return false;\
         if (Object.getPrototypeOf(cs) !== Set.prototype) return false;\
         if (!cs.has(cs)) return false;\
         return !cs.has(s);\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_map_clones_object_keys() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const k = {k: 1};\
         const m = new Map([[k, 2]]);\
         const c = structuredClone(m);\
         if (!(c instanceof Map)) return false;\
         if (Object.getPrototypeOf(c) !== Map.prototype) return false;\
         const ck = Array.from(c.keys())[0];\
         if (ck === k) return false;\
         if (ck.k !== 1) return false;\
         if (c.get(ck) !== 2) return false;\
         return c.get(k) === undefined;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_clones_regexp() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Regression test: platform-object checks in `structuredClone` must not call APIs that return
    // `VmError::InvalidHandle` for `RegExp` objects.
    match realm.exec_script("structuredClone(/a+/g).test('aa')") {
      Ok(v) => assert_eq!(v, Value::Bool(true)),
      Err(VmError::InvalidHandle { .. }) => {
        panic!("structuredClone(/a+/g) must not throw VmError::InvalidHandle")
      }
      Err(err) => return Err(err),
    }

    let ok = realm.exec_script(
      "(() => {\
         const r = /a+/gi;\
         const c = structuredClone(r);\
         if (!(c instanceof RegExp)) return false;\
         if (c.source !== r.source) return false;\
         if (c.flags !== r.flags) return false;\
         const r2 = /x/;\
         r2.foo = 1;\
         if (structuredClone(r2).foo !== undefined) return false;\
         return true;\
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
         if (Object.getPrototypeOf(c) !== ArrayBuffer.prototype) return false;\
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
         if (Object.prototype.toString.call(c) !== '[object ArrayBuffer]') return false;\
         if (Object.getPrototypeOf(c) !== ArrayBuffer.prototype) return false;\
         let threw = false;\
         try { new Uint8Array(ab); } catch (e) { threw = e.name === 'TypeError'; }\
         if (!threw) return false;\
         if (c.byteLength !== 2) return false;\
         return new Uint8Array(c)[0] === 7;\
       })()",
    )?;
    assert_eq!(transfer_ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn structured_clone_array_buffer_subclass_clones_as_array_buffer() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         let MyAB;\
         let ab;\
         try {\
           MyAB = class MyAB extends ArrayBuffer {};\
           ab = new MyAB(2);\
         } catch (_e) {\
           /* If `class extends ArrayBuffer` isn't supported, emulate via prototype mutation. */\
           MyAB = function MyAB(len) {\
             const ab = new ArrayBuffer(len);\
             Object.setPrototypeOf(ab, MyAB.prototype);\
             return ab;\
           };\
           MyAB.prototype = Object.create(ArrayBuffer.prototype);\
           Object.defineProperty(MyAB.prototype, 'constructor', { value: MyAB });\
           ab = new MyAB(2);\
         }\
         new Uint8Array(ab).set([1, 2]);\
         if (!(ab instanceof MyAB)) return false;\
         if (Object.getPrototypeOf(ab) !== MyAB.prototype) return false;\
         const c = structuredClone(ab);\
         if (Object.prototype.toString.call(c) !== '[object ArrayBuffer]') return false;\
         if (Object.getPrototypeOf(c) !== ArrayBuffer.prototype) return false;\
         if (!(c instanceof ArrayBuffer)) return false;\
         if (c instanceof MyAB) return false;\
         const v = new Uint8Array(c);\
         return v[0] === 1 && v[1] === 2;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_array_buffer_uses_intrinsics_under_global_tampering() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const RealArrayBuffer = ArrayBuffer;\
         const ab = new RealArrayBuffer(2);\
         new Uint8Array(ab).set([1, 2]);\
         globalThis.ArrayBuffer = function () { throw new Error('tampered'); };\
         const c = structuredClone(ab);\
         if (!(c instanceof RealArrayBuffer)) return false;\
         if (Object.getPrototypeOf(c) !== RealArrayBuffer.prototype) return false;\
         const v = new Uint8Array(c);\
         return v[0] === 1 && v[1] === 2;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_typed_array_and_data_view_use_intrinsics() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Regression test: cloning ArrayBuffer views must not consult global `Int16Array` / `DataView`
    // bindings, which user code can overwrite.
    let ok = realm.exec_script(
      "(() => {\
         const OriginalInt16Array = Int16Array;\
         const OriginalDataView = DataView;\
         globalThis.Int16Array = function() { throw 1 };\
         globalThis.DataView = function() { throw 1 };\
         const ta = new OriginalInt16Array(new ArrayBuffer(4));\
         ta[0] = 1234;\
         const ta2 = structuredClone(ta);\
         if (!(ta2 instanceof OriginalInt16Array)) return false;\
         if (Object.getPrototypeOf(ta2) !== OriginalInt16Array.prototype) return false;\
         if (Object.prototype.toString.call(ta2) !== '[object Int16Array]') return false;\
         if (ta2.length !== ta.length) return false;\
         if (ta2[0] !== 1234) return false;\
         const dv = new OriginalDataView(new ArrayBuffer(4));\
         dv.setInt16(0, 0x1234, true);\
         const dv2 = structuredClone(dv);\
         if (!(dv2 instanceof OriginalDataView)) return false;\
         if (Object.getPrototypeOf(dv2) !== OriginalDataView.prototype) return false;\
         if (Object.prototype.toString.call(dv2) !== '[object DataView]') return false;\
         if (dv2.byteLength !== dv.byteLength) return false;\
         if (dv2.getInt16(0, true) !== 0x1234) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_bigint_typed_arrays_use_intrinsics() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const OriginalBigInt64Array = BigInt64Array;\
         const OriginalBigUint64Array = BigUint64Array;\
         globalThis.BigInt64Array = function() { throw 1 };\
         globalThis.BigUint64Array = function() { throw 2 };\
         const a = new OriginalBigInt64Array(2);\
         a[0] = 123n;\
         a[1] = -456n;\
         const ca = structuredClone(a);\
         if (!(ca instanceof OriginalBigInt64Array)) return false;\
         if (Object.getPrototypeOf(ca) !== OriginalBigInt64Array.prototype) return false;\
         if (Object.prototype.toString.call(ca) !== '[object BigInt64Array]') return false;\
         if (ca.length !== a.length) return false;\
         if (ca[0] !== 123n || ca[1] !== -456n) return false;\
         if (ca.buffer === a.buffer) return false;\
         const b = new OriginalBigUint64Array(2);\
         b[0] = 123n;\
         b[1] = 456n;\
         const cb = structuredClone(b);\
         if (!(cb instanceof OriginalBigUint64Array)) return false;\
         if (Object.getPrototypeOf(cb) !== OriginalBigUint64Array.prototype) return false;\
         if (Object.prototype.toString.call(cb) !== '[object BigUint64Array]') return false;\
         if (cb.length !== b.length) return false;\
         if (cb[0] !== 123n || cb[1] !== 456n) return false;\
         if (cb.buffer === b.buffer) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_uint8_array_does_not_invoke_shadowed_view_metadata_getters() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(8);\
         const u8 = new Uint8Array(ab, 2, 4);\
         u8[0] = 1; u8[1] = 2; u8[2] = 3; u8[3] = 4;\
         const origByteOffset = u8.byteOffset;\
         const origLength = u8.length;\
         const origByteLength = u8.byteLength;\
         Object.defineProperty(u8, 'buffer', { get() { throw 1; } });\
         Object.defineProperty(u8, 'byteOffset', { get() { throw 2; } });\
         Object.defineProperty(u8, 'length', { get() { throw 3; } });\
         Object.defineProperty(u8, 'byteLength', { get() { throw 4; } });\
         let threwBuffer = false;\
         let threwByteOffset = false;\
         let threwLength = false;\
         let threwByteLength = false;\
         try { u8.buffer; } catch (e) { threwBuffer = e === 1; }\
         try { u8.byteOffset; } catch (e) { threwByteOffset = e === 2; }\
         try { u8.length; } catch (e) { threwLength = e === 3; }\
         try { u8.byteLength; } catch (e) { threwByteLength = e === 4; }\
         if (!threwBuffer || !threwByteOffset || !threwLength || !threwByteLength) return false;\
         const c = structuredClone(u8);\
         if (!(c instanceof Uint8Array)) return false;\
         if (Object.getPrototypeOf(c) !== Uint8Array.prototype) return false;\
         if (c.length !== origLength) return false;\
         if (c.byteOffset !== origByteOffset) return false;\
         if (c.byteLength !== origByteLength) return false;\
         if (c[0] !== 1 || c[1] !== 2 || c[2] !== 3 || c[3] !== 4) return false;\
         if (c.buffer === ab) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_int16_array_does_not_invoke_shadowed_view_metadata_getters() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(12);\
         const a = new Int16Array(ab, 2, 2);\
         a[0] = 1234;\
         a[1] = -5678;\
         const origByteOffset = a.byteOffset;\
         const origLength = a.length;\
         const origByteLength = a.byteLength;\
         Object.defineProperty(a, 'buffer', { get() { throw 1; } });\
         Object.defineProperty(a, 'byteOffset', { get() { throw 2; } });\
         Object.defineProperty(a, 'length', { get() { throw 3; } });\
         Object.defineProperty(a, 'byteLength', { get() { throw 4; } });\
         let threwBuffer = false;\
         let threwByteOffset = false;\
         let threwLength = false;\
         let threwByteLength = false;\
         try { a.buffer; } catch (e) { threwBuffer = e === 1; }\
         try { a.byteOffset; } catch (e) { threwByteOffset = e === 2; }\
         try { a.length; } catch (e) { threwLength = e === 3; }\
         try { a.byteLength; } catch (e) { threwByteLength = e === 4; }\
         if (!threwBuffer || !threwByteOffset || !threwLength || !threwByteLength) return false;\
         const c = structuredClone(a);\
         if (!(c instanceof Int16Array)) return false;\
         if (Object.getPrototypeOf(c) !== Int16Array.prototype) return false;\
         if (c.length !== origLength) return false;\
         if (c.byteOffset !== origByteOffset) return false;\
         if (c.byteLength !== origByteLength) return false;\
         if (c[0] !== 1234 || c[1] !== -5678) return false;\
         if (c.buffer === ab) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_data_view_does_not_invoke_shadowed_view_metadata_getters() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(8);\
         const dv = new DataView(ab, 1, 4);\
         dv.setUint8(0, 9);\
         dv.setUint8(1, 8);\
         dv.setUint8(2, 7);\
         dv.setUint8(3, 6);\
         const origByteOffset = dv.byteOffset;\
         const origByteLength = dv.byteLength;\
         Object.defineProperty(dv, 'buffer', { get() { throw 1; } });\
         Object.defineProperty(dv, 'byteOffset', { get() { throw 2; } });\
         Object.defineProperty(dv, 'byteLength', { get() { throw 3; } });\
         let threwBuffer = false;\
         let threwByteOffset = false;\
         let threwByteLength = false;\
         try { dv.buffer; } catch (e) { threwBuffer = e === 1; }\
         try { dv.byteOffset; } catch (e) { threwByteOffset = e === 2; }\
         try { dv.byteLength; } catch (e) { threwByteLength = e === 3; }\
         if (!threwBuffer || !threwByteOffset || !threwByteLength) return false;\
         const c = structuredClone(dv);\
         if (!(c instanceof DataView)) return false;\
         if (Object.getPrototypeOf(c) !== DataView.prototype) return false;\
         if (c.byteOffset !== origByteOffset || c.byteLength !== origByteLength) return false;\
         if (c.getUint8(0) !== 9 || c.getUint8(1) !== 8 || c.getUint8(2) !== 7 || c.getUint8(3) !== 6) return false;\
         if (c.buffer === ab) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_transfer_list_detach_even_if_unreferenced() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(1);\
         const view = new Uint8Array(ab);\
         view[0] = 42;\
         const v = structuredClone(1, { transfer: [ab] });\
         if (v !== 1) return false;\
         if (ab.byteLength !== 0) return false;\
         if (view.length !== 0) return false;\
         return true;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_does_not_detach_on_data_clone_error() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(1);\
         try { structuredClone(Symbol('x'), { transfer: [ab] }); return false; }\
         catch (e) {\
           if (e.name !== 'DataCloneError') return false;\
           return ab.byteLength === 1;\
         }\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
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

    let detached = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(1);\
         structuredClone(ab, { transfer: [ab] });\
         try { structuredClone(ab, { transfer: [ab] }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, detached), "DataCloneError");

    let non_ab = realm.exec_script(
      "(() => {\
         try { structuredClone(1, { transfer: [1] }); return 'no'; }\
         catch (e) { return e.name; }\
        })()",
    )?;
    assert_eq!(get_string(&realm, non_ab), "DataCloneError");

    let non_ab_stream = realm.exec_script(
      "(() => {\
         try { structuredClone(1, { transfer: [new ReadableStream()] }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, non_ab_stream), "DataCloneError");

    let iterable = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(2);\
         new Uint8Array(ab)[0] = 7;\
         const transfer = { [Symbol.iterator]: function* () { yield ab; } };\
         const c = structuredClone(ab, { transfer });\
         return ab.byteLength === 0 && c.byteLength === 2 && new Uint8Array(c)[0] === 7;\
       })()",
    )?;
    assert_eq!(iterable, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_symbol_and_function() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let sym_ok = realm.exec_script(
      "(() => {\
         try { structuredClone(Symbol('x')); return false; }\
         catch (e) {\
           return e.name === 'DataCloneError' && typeof DOMException !== 'undefined' && e instanceof DOMException;\
         }\
       })()",
    )?;
    assert_eq!(sym_ok, Value::Bool(true));

    let fun_ok = realm.exec_script(
      "(() => {\
         try { structuredClone(function(){}); return false; }\
         catch (e) {\
           return e.name === 'DataCloneError' && typeof DOMException !== 'undefined' && e instanceof DOMException;\
         }\
       })()",
    )?;
    assert_eq!(fun_ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_boxed_symbol() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ok = realm.exec_script(
      "(() => {\
         try { structuredClone(Object(Symbol('x'))); return 'no'; }\
         catch (e) { return e.name; }\
       })() === 'DataCloneError'",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_does_not_detach_transfer_list_on_serialize_error() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const ab = new ArrayBuffer(4);\
         const v = new Uint8Array(ab);\
         v[0] = 123;\
         const len = ab.byteLength;\
         const vlen = v.length;\
         let threw = false;\
         try { structuredClone({ bad: Symbol('x') }, { transfer: [ab] }); }\
         catch (e) { threw = e.name === 'DataCloneError'; }\
         return threw && ab.byteLength === len && v.length === vlen && v[0] === 123;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn structured_clone_clones_boxed_primitives() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const b = new Boolean(true); b.extra = 1;\
         const n = new Number(5); n.extra = 2;\
         const s = new String('hi'); s.extra = 3;\
         const bi = Object(1n); bi.extra = 4;\
         const cb = structuredClone(b);\
         const cn = structuredClone(n);\
         const cs = structuredClone(s);\
         const cbi = structuredClone(bi);\
         return cb !== b && Object.getPrototypeOf(cb) === Boolean.prototype && cb.valueOf() === true && cb.extra === undefined &&\
                cn !== n && Object.getPrototypeOf(cn) === Number.prototype && cn.valueOf() === 5 && cn.extra === undefined &&\
                cs !== s && Object.getPrototypeOf(cs) === String.prototype && cs.valueOf() === 'hi' && cs.extra === undefined &&\
                cbi !== bi && Object.getPrototypeOf(cbi) === BigInt.prototype && cbi.valueOf() === 1n && cbi.extra === undefined;\
        })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_clones_error_objects() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const e = new TypeError('boom');\
         e.extra = 1;\
         const c = structuredClone(e);\
         if (c === e) return false;\
         if (!(c instanceof TypeError)) return false;\
         if (c.name !== 'TypeError') return false;\
         if (c.message !== 'boom') return false;\
         if (c.extra !== undefined) return false;\
         const cyc = new Error('x');\
         cyc.self = cyc;\
         const cyc2 = structuredClone(cyc);\
         return cyc2 !== cyc && cyc2.name === 'Error' && cyc2.message === 'x' && cyc2.self === undefined;\
        })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_error_missing_own_message_property() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\
         const e = new Error();\
         if (Object.prototype.hasOwnProperty.call(e, 'message') !== false) return false;\
         const c = structuredClone(e);\
         return Object.prototype.hasOwnProperty.call(c, 'message') === false;\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn structured_clone_rejects_platform_objects() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let name = realm.exec_script(
      "(() => {\
         try { structuredClone(document.createElement('div')); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, name), "DataCloneError");

    let controller = realm.exec_script(
      "(() => {\
         try { structuredClone(new AbortController()); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, controller), "DataCloneError");

    let signal = realm.exec_script(
      "(() => {\
         try { structuredClone(new AbortController().signal); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, signal), "DataCloneError");

    let broadcast_channel = realm.exec_script(
      "(() => {\
         try { structuredClone(new BroadcastChannel('x')); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, broadcast_channel), "DataCloneError");

    let intersection_observer = realm.exec_script(
      "(() => {\
         try { structuredClone(new IntersectionObserver(() => {})); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, intersection_observer), "DataCloneError");

    let intersection_observer_entry = realm.exec_script(
      "(() => {\
         const obs = new IntersectionObserver(() => {});\
         obs.observe({});\
         const entry = obs.takeRecords()[0];\
         try { structuredClone(entry); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, intersection_observer_entry), "DataCloneError");
    Ok(())
  }

  #[test]
  fn structured_clone_rejects_internal_slot_objects() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let proxy = realm.exec_script(
      "(() => {\
         try { structuredClone(new Proxy({}, {})); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, proxy), "DataCloneError");

    let weak_map = realm.exec_script(
      "(() => {\
         try { structuredClone(new WeakMap()); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, weak_map), "DataCloneError");

    let weak_set = realm.exec_script(
      "(() => {\
         try { structuredClone(new WeakSet()); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, weak_set), "DataCloneError");

    let generator = realm.exec_script(
      "(() => {\
         function* g(){ yield 1; }\
         const it = g();\
         try { structuredClone(it); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, generator), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_regexp_string_iterator() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let v = realm.exec_script(
      "(() => {\
         const it = /a/g[Symbol.matchAll]('aa');\
         try { structuredClone(it); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, v), "DataCloneError");
    Ok(())
  }

  #[test]
  fn structured_clone_rejects_url_and_url_search_params() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let url = realm.exec_script(
      "try { structuredClone(new URL('https://example.com')); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, url), "DataCloneError");

    let params = realm.exec_script(
      "try { structuredClone(new URLSearchParams('a=1')); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, params), "DataCloneError");

    let iter = realm.exec_script(
      "try { structuredClone(new URLSearchParams('a=1').entries()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, iter), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_streams_and_readers() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let stream = realm.exec_script(
      "try { structuredClone(new ReadableStream()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, stream), "DataCloneError");

    let iter = realm.exec_script(
      "try { structuredClone(new ReadableStream().values()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, iter), "DataCloneError");

    let async_iter = realm.exec_script(
      "try { structuredClone((new ReadableStream())[Symbol.asyncIterator]()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, async_iter), "DataCloneError");

    let reader = realm.exec_script(
      "try { structuredClone(new ReadableStream().getReader()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, reader), "DataCloneError");

    let controller = realm.exec_script(
      "(() => {\
         let controller = null;\
         new ReadableStream({ start(c) { controller = c; } });\
         try { structuredClone(controller); return 'no'; }\
         catch (e) { return e.name; }\
        })()",
    )?;
    assert_eq!(get_string(&realm, controller), "DataCloneError");

    let writable_stream = realm.exec_script(
      "try { structuredClone(new WritableStream()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, writable_stream), "DataCloneError");

    let writable_stream_writer = realm.exec_script(
      "try { structuredClone(new WritableStream().getWriter()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, writable_stream_writer), "DataCloneError");

    let transform_stream = realm.exec_script(
      "try { structuredClone(new TransformStream()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, transform_stream), "DataCloneError");

    let transform_stream_writer = realm.exec_script(
      "try { structuredClone(new TransformStream().writable.getWriter()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, transform_stream_writer), "DataCloneError");

    let transform_controller = realm.exec_script(
      "(() => {\
         let controller = null;\
         const ts = new TransformStream({\
           transform(chunk, c) { controller = c; }\
         });\
         const writer = ts.writable.getWriter();\
         writer.write(new Uint8Array([1]));\
         if (controller === null) return 'missing';\
         try { structuredClone(controller); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, transform_controller), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_queuing_strategies() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let blqs = realm.exec_script(
      "try { structuredClone(new ByteLengthQueuingStrategy({ highWaterMark: 1 })); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, blqs), "DataCloneError");

    let cqs = realm.exec_script(
      "try { structuredClone(new CountQueuingStrategy({ highWaterMark: 1 })); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, cqs), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_window_realm_platform_objects() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    for expr in [
      "globalThis",
      "document",
      "location",
      "history",
      "console",
      "localStorage",
      "sessionStorage",
      "navigator",
      "screen",
      "matchMedia('(min-width: 1px)')",
      "crypto",
      "performance",
      // Encoding API objects are HostSlots-branded platform objects.
      // Ensure future structuredClone refactors don't accidentally treat them as plain objects.
      "new TextEncoder()",
      "new TextDecoder()",
      "new TextEncoderStream()",
      "new TextDecoderStream()",
    ] {
      let script = format!("try {{ structuredClone({expr}); 'no' }} catch (e) {{ e.name }}");
      let v = realm.exec_script(&script)?;
      assert_eq!(get_string(&realm, v), "DataCloneError", "structuredClone({expr})");
    }

    Ok(())
  }

  #[test]
  fn structured_clone_clones_ecmascript_error_objects() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let type_error_ok = realm.exec_script(
      "(() => {\
         const e = structuredClone(new TypeError('x'));\
         if (!(e instanceof TypeError)) return false;\
         if (e.name !== 'TypeError') return false;\
         return e.message === 'x';\
       })()",
    )?;
    assert_eq!(type_error_ok, Value::Bool(true));

    let accessor_message_ok = realm.exec_script(
      "(() => {\
         const e = new Error('x');\
         Object.defineProperty(e, 'message', { get() { throw 1 }, enumerable: false });\
         const c = structuredClone(e);\
         return Object.prototype.hasOwnProperty.call(c, 'message') === false && c.message === '';\
        })()",
    )?;
    assert_eq!(accessor_message_ok, Value::Bool(true));

    let custom_props_ignored_ok = realm.exec_script(
      "(() => {\
         const e = new Error('x');\
         e.foo = 1;\
         const c = structuredClone(e);\
         return c.foo === undefined;\
       })()",
    )?;
    assert_eq!(custom_props_ignored_ok, Value::Bool(true));

    let dom_exception_name = realm.exec_script(
      "(() => {\
         try { structuredClone(new DOMException('x', 'NotSupportedError')); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(&realm, dom_exception_name), "DataCloneError");

    Ok(())
  }

  #[test]
  fn structured_clone_clones_blob() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Brand the Blob with `HostSlots` to ensure `structuredClone` still clones it.
    let blob = realm.exec_script(
      "globalThis.__sc_blob = new Blob(['hi'], { type: 'text/plain' }); globalThis.__sc_blob",
    )?;
    let Value::Object(blob_obj) = blob else {
      return Err(VmError::InvariantViolation("expected Blob object"));
    };
    realm
      .heap_mut()
      .object_set_host_slots(blob_obj, HostSlots { a: 1, b: 2 })?;

    let ok = realm.exec_script(
      "(() => {\
         const b = globalThis.__sc_blob;\
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

  #[test]
  fn structured_clone_rejects_form_data() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let v = realm.exec_script("try { structuredClone(new FormData()); 'no' } catch (e) { e.name }")?;
    assert_eq!(get_string(&realm, v), "DataCloneError");
    Ok(())
  }

  #[test]
  fn structured_clone_rejects_form_data_iterator() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let v = realm.exec_script(
      "try { structuredClone(new FormData().entries()); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(&realm, v), "DataCloneError");
    Ok(())
  }

  #[test]
  fn structured_clone_rejects_fetch_api_objects_and_web_socket() -> crate::error::Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    let headers = host.exec_script(
      "(() => {\
         try { structuredClone(new Headers()); return false; }\
         catch (e) { return !!(e && e.name === 'DataCloneError'); }\
       })()",
    )?;
    assert_eq!(headers, Value::Bool(true));

    let request = host.exec_script(
      "(() => {\
         try { structuredClone(new Request('https://example.com/')); return false; }\
         catch (e) { return !!(e && e.name === 'DataCloneError'); }\
       })()",
    )?;
    assert_eq!(request, Value::Bool(true));

    let response = host.exec_script(
      "(() => {\
         try { structuredClone(new Response('x')); return false; }\
         catch (e) { return !!(e && e.name === 'DataCloneError'); }\
       })()",
    )?;
    assert_eq!(response, Value::Bool(true));

    let websocket = host.exec_script(
      "(() => {\
         if (typeof WebSocket !== 'function') return true;\
         let ws;\
         try { ws = new WebSocket('wss://127.0.0.1:1/'); } catch (_e) { return true; }\
         try { structuredClone(ws); return false; }\
         catch (e) { return !!(e && e.name === 'DataCloneError'); }\
       })()",
    )?;
    assert_eq!(websocket, Value::Bool(true));

    Ok(())
  }
}
