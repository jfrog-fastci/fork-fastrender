use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::{GcEnv, GcObject, Heap, Intrinsics, RealmId, RootId, Value, Vm, VmError, WellKnownSymbols};
use crate::Scope;
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

fn define_global_property_once(
  scope: &mut Scope<'_>,
  global_object: GcObject,
  installed: &mut Vec<&'static str>,
  name: &'static str,
  desc: PropertyDescriptor,
) -> Result<(), VmError> {
  // Realm initialization is engine-controlled: duplicate globals indicate a bug (often from a bad
  // merge) and should fail fast rather than silently replacing the existing property.
  if installed.iter().any(|&n| n == name) {
    return Err(VmError::InvariantViolation(
      "duplicate global binding during realm initialization",
    ));
  }
  // Avoid `Vec` growth panics on allocator OOM.
  installed.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  installed.push(name);

  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.define_property(global_object, key, desc)
}

fn define_global_data_property_once(
  scope: &mut Scope<'_>,
  global_object: GcObject,
  installed: &mut Vec<&'static str>,
  name: &'static str,
  value: Value,
) -> Result<(), VmError> {
  define_global_property_once(scope, global_object, installed, name, global_data_desc(value))
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
    // A new realm implies a fresh global environment record; clear any tracked global `var`/function
    // declaration names from a prior realm.
    vm.global_var_names_clear();

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
      scope
        .heap_mut()
        .set_function_closure_env(intrinsics.async_generator_function(), Some(env))?;

      // Populate `[[Realm]]` + `[[JobRealm]]` on all intrinsic function objects.
      set_intrinsic_function_realm_metadata(scope.heap_mut(), &roots, global_object, id)?;

      // Make the global object spec-shaped:
      // `%GlobalObject%.[[Prototype]]` is `%Object.prototype%`.
      scope.heap_mut().object_set_prototype(
        global_object,
        Some(intrinsics.object_prototype()),
      )?;

      let mut installed_globals: Vec<&'static str> = Vec::new();

      // `globalThis` is a writable, configurable, non-enumerable data property whose value is the
      // global object itself.
      define_global_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "globalThis",
        global_data_desc(Value::Object(global_object)),
      )?;

      // --- Global value properties ---
      define_global_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Infinity",
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Number(f64::INFINITY),
            writable: false,
          },
        },
      )?;

      define_global_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "NaN",
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
      define_global_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "undefined",
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
      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Object",
        Value::Object(intrinsics.object_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Function",
        Value::Object(intrinsics.function_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Proxy",
        Value::Object(intrinsics.proxy_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Array",
        Value::Object(intrinsics.array_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "String",
        Value::Object(intrinsics.string_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "RegExp",
        Value::Object(intrinsics.regexp_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Number",
        Value::Object(intrinsics.number_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Boolean",
        Value::Object(intrinsics.boolean_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "BigInt",
        Value::Object(intrinsics.bigint_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Date",
        Value::Object(intrinsics.date_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Symbol",
        Value::Object(intrinsics.symbol_constructor()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "ArrayBuffer",
        Value::Object(intrinsics.array_buffer()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Uint8Array",
        Value::Object(intrinsics.uint8_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Int8Array",
        Value::Object(intrinsics.int8_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Uint8ClampedArray",
        Value::Object(intrinsics.uint8_clamped_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Int16Array",
        Value::Object(intrinsics.int16_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Uint16Array",
        Value::Object(intrinsics.uint16_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Int32Array",
        Value::Object(intrinsics.int32_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Uint32Array",
        Value::Object(intrinsics.uint32_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Float32Array",
        Value::Object(intrinsics.float32_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Float64Array",
        Value::Object(intrinsics.float64_array()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "DataView",
        Value::Object(intrinsics.data_view()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "eval",
        Value::Object(intrinsics.eval()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "isNaN",
        Value::Object(intrinsics.is_nan()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "isFinite",
        Value::Object(intrinsics.is_finite()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "parseInt",
        Value::Object(intrinsics.parse_int()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "parseFloat",
        Value::Object(intrinsics.parse_float()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "encodeURI",
        Value::Object(intrinsics.encode_uri()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "encodeURIComponent",
        Value::Object(intrinsics.encode_uri_component()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "decodeURI",
        Value::Object(intrinsics.decode_uri()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "decodeURIComponent",
        Value::Object(intrinsics.decode_uri_component()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Math",
        Value::Object(intrinsics.math()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "JSON",
        Value::Object(intrinsics.json()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Reflect",
        Value::Object(intrinsics.reflect()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Error",
        Value::Object(intrinsics.error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "TypeError",
        Value::Object(intrinsics.type_error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "RangeError",
        Value::Object(intrinsics.range_error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "ReferenceError",
        Value::Object(intrinsics.reference_error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "SyntaxError",
        Value::Object(intrinsics.syntax_error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "EvalError",
        Value::Object(intrinsics.eval_error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "URIError",
        Value::Object(intrinsics.uri_error()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "AggregateError",
        Value::Object(intrinsics.aggregate_error()),
      )?;

      // Promise
      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "Promise",
        Value::Object(intrinsics.promise()),
      )?;

      // Map / Set
      let map_key = PropertyKey::from_string(scope.alloc_string("Map")?);
      scope.define_property(
        global_object,
        map_key,
        global_data_desc(Value::Object(intrinsics.map())),
      )?;

      let set_key = PropertyKey::from_string(scope.alloc_string("Set")?);
      scope.define_property(
        global_object,
        set_key,
        global_data_desc(Value::Object(intrinsics.set())),
      )?;

      // WeakMap / WeakSet
      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "WeakMap",
        Value::Object(intrinsics.weak_map()),
      )?;

      define_global_data_property_once(
        &mut scope,
        global_object,
        &mut installed_globals,
        "WeakSet",
        Value::Object(intrinsics.weak_set()),
      )?;

      let weak_ref_key = PropertyKey::from_string(scope.alloc_string("WeakRef")?);
      scope.define_property(
        global_object,
        weak_ref_key,
        global_data_desc(Value::Object(intrinsics.weak_ref())),
      )?;

      let finalization_registry_key =
        PropertyKey::from_string(scope.alloc_string("FinalizationRegistry")?);
      scope.define_property(
        global_object,
        finalization_registry_key,
        global_data_desc(Value::Object(intrinsics.finalization_registry())),
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

  #[test]
  fn well_known_symbols_are_agent_wide_and_survive_realm_teardown() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let mut realm_a = Realm::new(&mut vm, &mut heap)?;
    let wks_a = *realm_a.well_known_symbols();
    let mut realm_b = Realm::new(&mut vm, &mut heap)?;
    let wks_b = *realm_b.well_known_symbols();
    assert_eq!(wks_a, wks_b);

    // Tear down both realms and ensure the symbols remain live solely due to heap-level storage.
    realm_b.teardown(&mut heap);
    realm_a.teardown(&mut heap);
    heap.collect_garbage();

    let mut realm_c = Realm::new(&mut vm, &mut heap)?;
    let wks_c = *realm_c.well_known_symbols();
    assert_eq!(wks_a, wks_c);
    realm_c.teardown(&mut heap);
    Ok(())
  }
}
