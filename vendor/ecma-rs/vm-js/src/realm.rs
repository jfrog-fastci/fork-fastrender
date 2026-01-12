use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::{GcEnv, GcObject, Heap, Intrinsics, RealmId, RootId, Value, Vm, VmError, WellKnownSymbols};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REALM_ID: AtomicU64 = AtomicU64::new(1);

/// An ECMAScript realm: global object + intrinsics.
///
/// This type owns a set of **persistent GC roots** registered with the [`Heap`]. Call
/// [`Realm::teardown`] to unregister those roots when the embedding is finished with the realm (for
/// example, when running many `test262` tests by creating a fresh realm per test while reusing a
/// single heap).
#[derive(Debug)]
pub struct Realm {
  id: RealmId,
  global_object: GcObject,
  global_lexical_env: GcEnv,
  intrinsics: Intrinsics,
  roots: Vec<RootId>,
  torn_down: bool,
}

fn set_intrinsic_function_realm_metadata(
  heap: &mut Heap,
  roots: &[RootId],
  global_object: GcObject,
  realm_id: RealmId,
) -> Result<(), VmError> {
  // Traverse the intrinsic object graph starting from the realm's persistent roots and populate
  // `[[Realm]]` + `[[JobRealm]]` on all intrinsic function objects. Most intrinsic functions are
  // reachable only via prototype/property links and are not directly included in `roots`, so this
  // must walk the graph rather than just iterating root values.

  let mut worklist: Vec<GcObject> = Vec::new();
  worklist
    .try_reserve_exact(roots.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for &root in roots {
    if let Some(Value::Object(obj)) = heap.get_root(root) {
      worklist.push(obj);
    }
  }

  let mut visited: HashSet<GcObject> = HashSet::new();

  while let Some(obj) = worklist.pop() {
    if visited.try_reserve(1).is_err() {
      return Err(VmError::OutOfMemory);
    }
    if !visited.insert(obj) {
      continue;
    }

    if heap.is_callable(Value::Object(obj))? {
      heap.set_function_realm(obj, global_object)?;
      heap.set_function_job_realm(obj, realm_id)?;
    }

    if let Some(proto) = heap.object_prototype(obj)? {
      if worklist.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      worklist.push(proto);
    }

    let keys = heap.own_property_keys(obj)?;
    for key in keys {
      let Some(desc) = heap.object_get_own_property(obj, &key)? else {
        continue;
      };
      match desc.kind {
        PropertyKind::Data { value, .. } => {
          if let Value::Object(child) = value {
            if worklist.try_reserve(1).is_err() {
              return Err(VmError::OutOfMemory);
            }
            worklist.push(child);
          }
        }
        PropertyKind::Accessor { get, set } => {
          if let Value::Object(child) = get {
            if worklist.try_reserve(1).is_err() {
              return Err(VmError::OutOfMemory);
            }
            worklist.push(child);
          }
          if let Value::Object(child) = set {
            if worklist.try_reserve(1).is_err() {
              return Err(VmError::OutOfMemory);
            }
            worklist.push(child);
          }
        }
      }
    }
  }

  Ok(())
}

fn global_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

impl Realm {
  /// Returns the host-facing [`RealmId`] token for this realm.
  ///
  /// This is used to tag Promise jobs and other host-scheduled work with the realm they should run
  /// in.
  pub fn id(&self) -> RealmId {
    self.id
  }

  /// Creates a new realm on `heap`.
  pub fn new(vm: &mut Vm, heap: &mut Heap) -> Result<Self, VmError> {
    let id = RealmId::from_raw(NEXT_REALM_ID.fetch_add(1, Ordering::Relaxed));
    let mut roots = Vec::new();

    let mut scope = heap.scope();
    let global_object = scope.alloc_object()?;
    scope.push_root(Value::Object(global_object))?;
    roots.push(scope.heap_mut().add_root(Value::Object(global_object))?);

    let intrinsics = match Intrinsics::init(vm, &mut scope, &mut roots) {
      Ok(intrinsics) => intrinsics,
      Err(err) => {
        // Avoid leaking persistent roots when realm initialization fails.
        for root in roots.drain(..) {
          scope.heap_mut().remove_root(root);
        }
        return Err(err);
      }
    };

    // Ensure objects created as `F.prototype` for functions/classes allocated in this realm inherit
    // from `%Object.prototype%`.
    scope
      .heap_mut()
      .set_default_object_prototype(Some(intrinsics.object_prototype()));

    // Any error after this point should also unregister roots to avoid leaks.
    let mut global_lexical_env: Option<GcEnv> = None;
    if let Err(err) = (|| -> Result<(), VmError> {
      // Create the realm's global lexical environment and store it on the intrinsic Function
      // constructor's captured environment slot, matching `CreateDynamicFunction` semantics.
      //
      // This environment record is kept alive by the Function constructor (which is itself kept
      // alive by the realm's persistent roots), so we do not need a separate persistent env root at
      // the realm layer.
      let env = scope.env_create(None)?;
      global_lexical_env = Some(env);
      scope
        .heap_mut()
        .set_function_closure_env(intrinsics.function_constructor(), Some(env))?;
      scope
        .heap_mut()
        .set_function_closure_env(intrinsics.eval(), Some(env))?;
      scope
        .heap_mut()
        .set_function_closure_env(intrinsics.generator_function_constructor(), Some(env))?;

      // Populate `[[Realm]]` + `[[JobRealm]]` on all intrinsic function objects.
      set_intrinsic_function_realm_metadata(scope.heap_mut(), &roots, global_object, id)?;

      // Make the global object spec-shaped:
      // `%GlobalObject%.[[Prototype]]` is `%Object.prototype%`.
      scope.heap_mut().object_set_prototype(
        global_object,
        Some(intrinsics.object_prototype()),
      )?;

      // `globalThis` is a writable, configurable, non-enumerable data property whose value is the
      // global object itself.
      let global_this_key = PropertyKey::from_string(scope.alloc_string("globalThis")?);
      scope.define_property(
        global_object,
        global_this_key,
        global_data_desc(Value::Object(global_object)),
      )?;

      // --- Global value properties ---
      let infinity_key = PropertyKey::from_string(scope.alloc_string("Infinity")?);
      scope.define_property(
        global_object,
        infinity_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Number(f64::INFINITY),
            writable: false,
          },
        },
      )?;

      let nan_key = PropertyKey::from_string(scope.alloc_string("NaN")?);
      scope.define_property(
        global_object,
        nan_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Number(f64::NAN),
            writable: false,
          },
        },
      )?;

      // (Optional but useful) Define a global `undefined` binding. In the spec this property is
      // non-writable, non-enumerable, non-configurable.
      let undefined_key = PropertyKey::from_string(scope.alloc_string("undefined")?);
      scope.define_property(
        global_object,
        undefined_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Undefined,
            writable: false,
          },
        },
      )?;

      // Install baseline global bindings as non-enumerable global properties.
      let object_key = PropertyKey::from_string(scope.alloc_string("Object")?);
      scope.define_property(
        global_object,
        object_key,
        global_data_desc(Value::Object(intrinsics.object_constructor())),
      )?;

      let function_key = PropertyKey::from_string(scope.alloc_string("Function")?);
      scope.define_property(
        global_object,
        function_key,
        global_data_desc(Value::Object(intrinsics.function_constructor())),
      )?;
 
      let proxy_key = PropertyKey::from_string(scope.alloc_string("Proxy")?);
      scope.define_property(
        global_object,
        proxy_key,
        global_data_desc(Value::Object(intrinsics.proxy_constructor())),
      )?;

      let array_key = PropertyKey::from_string(scope.alloc_string("Array")?);
      scope.define_property(
        global_object,
        array_key,
        global_data_desc(Value::Object(intrinsics.array_constructor())),
      )?;

      let proxy_key = PropertyKey::from_string(scope.alloc_string("Proxy")?);
      scope.define_property(
        global_object,
        proxy_key,
        global_data_desc(Value::Object(intrinsics.proxy_constructor())),
      )?;

      let string_key = PropertyKey::from_string(scope.alloc_string("String")?);
      scope.define_property(
        global_object,
        string_key,
        global_data_desc(Value::Object(intrinsics.string_constructor())),
      )?;

      let number_key = PropertyKey::from_string(scope.alloc_string("Number")?);
      scope.define_property(
        global_object,
        number_key,
        global_data_desc(Value::Object(intrinsics.number_constructor())),
      )?;

      let boolean_key = PropertyKey::from_string(scope.alloc_string("Boolean")?);
      scope.define_property(
        global_object,
        boolean_key,
        global_data_desc(Value::Object(intrinsics.boolean_constructor())),
      )?;

      let date_key = PropertyKey::from_string(scope.alloc_string("Date")?);
      scope.define_property(
        global_object,
        date_key,
        global_data_desc(Value::Object(intrinsics.date_constructor())),
      )?;

      let symbol_key = PropertyKey::from_string(scope.alloc_string("Symbol")?);
      scope.define_property(
        global_object,
        symbol_key,
        global_data_desc(Value::Object(intrinsics.symbol_constructor())),
      )?;

      let array_buffer_key = PropertyKey::from_string(scope.alloc_string("ArrayBuffer")?);
      scope.define_property(
        global_object,
        array_buffer_key,
        global_data_desc(Value::Object(intrinsics.array_buffer())),
      )?;

      let uint8_array_key = PropertyKey::from_string(scope.alloc_string("Uint8Array")?);
      scope.define_property(
        global_object,
        uint8_array_key,
        global_data_desc(Value::Object(intrinsics.uint8_array())),
      )?;

      let int8_array_key = PropertyKey::from_string(scope.alloc_string("Int8Array")?);
      scope.define_property(
        global_object,
        int8_array_key,
        global_data_desc(Value::Object(intrinsics.int8_array())),
      )?;

      let uint8_clamped_array_key =
        PropertyKey::from_string(scope.alloc_string("Uint8ClampedArray")?);
      scope.define_property(
        global_object,
        uint8_clamped_array_key,
        global_data_desc(Value::Object(intrinsics.uint8_clamped_array())),
      )?;

      let int16_array_key = PropertyKey::from_string(scope.alloc_string("Int16Array")?);
      scope.define_property(
        global_object,
        int16_array_key,
        global_data_desc(Value::Object(intrinsics.int16_array())),
      )?;

      let uint16_array_key = PropertyKey::from_string(scope.alloc_string("Uint16Array")?);
      scope.define_property(
        global_object,
        uint16_array_key,
        global_data_desc(Value::Object(intrinsics.uint16_array())),
      )?;

      let int32_array_key = PropertyKey::from_string(scope.alloc_string("Int32Array")?);
      scope.define_property(
        global_object,
        int32_array_key,
        global_data_desc(Value::Object(intrinsics.int32_array())),
      )?;

      let uint32_array_key = PropertyKey::from_string(scope.alloc_string("Uint32Array")?);
      scope.define_property(
        global_object,
        uint32_array_key,
        global_data_desc(Value::Object(intrinsics.uint32_array())),
      )?;

      let float32_array_key = PropertyKey::from_string(scope.alloc_string("Float32Array")?);
      scope.define_property(
        global_object,
        float32_array_key,
        global_data_desc(Value::Object(intrinsics.float32_array())),
      )?;

      let float64_array_key = PropertyKey::from_string(scope.alloc_string("Float64Array")?);
      scope.define_property(
        global_object,
        float64_array_key,
        global_data_desc(Value::Object(intrinsics.float64_array())),
      )?;

      let data_view_key = PropertyKey::from_string(scope.alloc_string("DataView")?);
      scope.define_property(
        global_object,
        data_view_key,
        global_data_desc(Value::Object(intrinsics.data_view())),
      )?;

      let eval_key = PropertyKey::from_string(scope.alloc_string("eval")?);
      scope.define_property(
        global_object,
        eval_key,
        global_data_desc(Value::Object(intrinsics.eval())),
      )?;

      let is_nan_key = PropertyKey::from_string(scope.alloc_string("isNaN")?);
      scope.define_property(
        global_object,
        is_nan_key,
        global_data_desc(Value::Object(intrinsics.is_nan())),
      )?;

      let is_finite_key = PropertyKey::from_string(scope.alloc_string("isFinite")?);
      scope.define_property(
        global_object,
        is_finite_key,
        global_data_desc(Value::Object(intrinsics.is_finite())),
      )?;

      let parse_int_key = PropertyKey::from_string(scope.alloc_string("parseInt")?);
      scope.define_property(
        global_object,
        parse_int_key,
        global_data_desc(Value::Object(intrinsics.parse_int())),
      )?;

      let parse_float_key = PropertyKey::from_string(scope.alloc_string("parseFloat")?);
      scope.define_property(
        global_object,
        parse_float_key,
        global_data_desc(Value::Object(intrinsics.parse_float())),
      )?;

      let encode_uri_key = PropertyKey::from_string(scope.alloc_string("encodeURI")?);
      scope.define_property(
        global_object,
        encode_uri_key,
        global_data_desc(Value::Object(intrinsics.encode_uri())),
      )?;

      let encode_uri_component_key =
        PropertyKey::from_string(scope.alloc_string("encodeURIComponent")?);
      scope.define_property(
        global_object,
        encode_uri_component_key,
        global_data_desc(Value::Object(intrinsics.encode_uri_component())),
      )?;

      let decode_uri_key = PropertyKey::from_string(scope.alloc_string("decodeURI")?);
      scope.define_property(
        global_object,
        decode_uri_key,
        global_data_desc(Value::Object(intrinsics.decode_uri())),
      )?;

      let decode_uri_component_key =
        PropertyKey::from_string(scope.alloc_string("decodeURIComponent")?);
      scope.define_property(
        global_object,
        decode_uri_component_key,
        global_data_desc(Value::Object(intrinsics.decode_uri_component())),
      )?;

      let math_key = PropertyKey::from_string(scope.alloc_string("Math")?);
      scope.define_property(
        global_object,
        math_key,
        global_data_desc(Value::Object(intrinsics.math())),
      )?;

      let json_key = PropertyKey::from_string(scope.alloc_string("JSON")?);
      scope.define_property(
        global_object,
        json_key,
        global_data_desc(Value::Object(intrinsics.json())),
      )?;

      let reflect_key = PropertyKey::from_string(scope.alloc_string("Reflect")?);
      scope.define_property(
        global_object,
        reflect_key,
        global_data_desc(Value::Object(intrinsics.reflect())),
      )?;

      let error_key = PropertyKey::from_string(scope.alloc_string("Error")?);
      scope.define_property(
        global_object,
        error_key,
        global_data_desc(Value::Object(intrinsics.error())),
      )?;

      let type_error_key = PropertyKey::from_string(scope.alloc_string("TypeError")?);
      scope.define_property(
        global_object,
        type_error_key,
        global_data_desc(Value::Object(intrinsics.type_error())),
      )?;

      let range_error_key = PropertyKey::from_string(scope.alloc_string("RangeError")?);
      scope.define_property(
        global_object,
        range_error_key,
        global_data_desc(Value::Object(intrinsics.range_error())),
      )?;

      let reference_error_key =
        PropertyKey::from_string(scope.alloc_string("ReferenceError")?);
      scope.define_property(
        global_object,
        reference_error_key,
        global_data_desc(Value::Object(intrinsics.reference_error())),
      )?;

      let syntax_error_key = PropertyKey::from_string(scope.alloc_string("SyntaxError")?);
      scope.define_property(
        global_object,
        syntax_error_key,
        global_data_desc(Value::Object(intrinsics.syntax_error())),
      )?;

      let eval_error_key = PropertyKey::from_string(scope.alloc_string("EvalError")?);
      scope.define_property(
        global_object,
        eval_error_key,
        global_data_desc(Value::Object(intrinsics.eval_error())),
      )?;

      let uri_error_key = PropertyKey::from_string(scope.alloc_string("URIError")?);
      scope.define_property(
        global_object,
        uri_error_key,
        global_data_desc(Value::Object(intrinsics.uri_error())),
      )?;

      let aggregate_error_key =
        PropertyKey::from_string(scope.alloc_string("AggregateError")?);
      scope.define_property(
        global_object,
        aggregate_error_key,
        global_data_desc(Value::Object(intrinsics.aggregate_error())),
      )?;

      // Promise
      let promise_key = PropertyKey::from_string(scope.alloc_string("Promise")?);
      scope.define_property(
        global_object,
        promise_key,
        global_data_desc(Value::Object(intrinsics.promise())),
      )?;

      // WeakMap / WeakSet
      let weak_map_key = PropertyKey::from_string(scope.alloc_string("WeakMap")?);
      scope.define_property(
        global_object,
        weak_map_key,
        global_data_desc(Value::Object(intrinsics.weak_map())),
      )?;

      let weak_set_key = PropertyKey::from_string(scope.alloc_string("WeakSet")?);
      scope.define_property(
        global_object,
        weak_set_key,
        global_data_desc(Value::Object(intrinsics.weak_set())),
      )?;

      Ok(())
    })() {
      for root in roots.drain(..) {
        scope.heap_mut().remove_root(root);
      }
      return Err(err);
    }

    vm.set_intrinsics(intrinsics);

    let global_lexical_env = global_lexical_env.ok_or(VmError::InvariantViolation(
      "global lexical environment missing after successful Realm::new",
    ))?;

    Ok(Self {
      id,
      global_object,
      global_lexical_env,
      intrinsics,
      roots,
      torn_down: false,
    })
  }

  /// The realm's global object.
  pub fn global_object(&self) -> GcObject {
    self.global_object
  }

  /// The realm's intrinsic objects.
  pub fn intrinsics(&self) -> &Intrinsics {
    &self.intrinsics
  }

  pub(crate) fn global_lexical_env(&self) -> GcEnv {
    self.global_lexical_env
  }

  pub fn well_known_symbols(&self) -> &WellKnownSymbols {
    self.intrinsics.well_known_symbols()
  }

  /// Unregisters all realm roots from the heap.
  ///
  /// # Safety contract
  ///
  /// After teardown, the realm must not be used for execution. Any GC handles retained by the
  /// realm (including the global object and intrinsics) may become invalid after the next GC cycle.
  ///
  /// This method is **idempotent**.
  pub fn teardown(&mut self, heap: &mut Heap) {
    if self.torn_down {
      return;
    }
    self.torn_down = true;

    for root in self.roots.drain(..) {
      heap.remove_root(root);
    }

    // Clear the heap-level default `Object.prototype` pointer. After teardown the realm's GC
    // handles may become invalid, and the embedding must not execute scripts without constructing a
    // fresh realm.
    heap.set_default_object_prototype(None);
  }

  /// Alias for [`Realm::teardown`].
  pub fn remove_roots(&mut self, heap: &mut Heap) {
    self.teardown(heap);
  }
}

impl Drop for Realm {
  fn drop(&mut self) {
    if !std::thread::panicking() {
      debug_assert!(
        self.torn_down,
        "Realm dropped without calling teardown(); this can leak persistent GC roots if the Heap is reused"
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{HeapLimits, VmOptions};

  #[test]
  fn realm_id_is_not_derived_from_global_object_heap_id() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    // On a fresh heap the global object should be the first allocation, making its packed HeapId
    // raw value `0`. Realm IDs are host-facing tokens and must not be derived from GC handles.
    assert_eq!(realm.global_object().id().0, 0);
    assert_ne!(realm.id().to_raw(), realm.global_object().id().0);

    realm.teardown(&mut heap);
    Ok(())
  }
}
