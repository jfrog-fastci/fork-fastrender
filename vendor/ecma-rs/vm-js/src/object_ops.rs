use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::fallible_format;
use crate::heap::ModuleNamespaceExportValue;
use crate::function::ThisMode;
use crate::property_descriptor_ops;
use crate::{GcObject, GcString, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext};
use std::collections::{HashMap, HashSet};

fn fnv1a_hash_code_units(units: &[u16]) -> u64 {
  // 64-bit FNV-1a.
  let mut hash: u64 = 14695981039346656037;
  for &u in units {
    hash ^= u as u64;
    hash = hash.wrapping_mul(1099511628211);
  }
  hash
}

fn hash_property_key(heap: &crate::Heap, key: &PropertyKey) -> Result<u64, VmError> {
  Ok(match key {
    PropertyKey::String(s) => fnv1a_hash_code_units(heap.get_string(*s)?.as_code_units()),
    // Symbols compare by identity; hashing by handle bits is fine.
    PropertyKey::Symbol(s) => ((s.index() as u64) << 32) | (s.generation() as u64),
  })
}

/// A set of [`PropertyKey`]s using ECMAScript equality semantics.
///
/// - String keys compare by UTF-16 code units.
/// - Symbol keys compare by identity.
///
/// This is used for Proxy `ownKeys` invariants, where the spec compares keys by value (not by
/// `GcString` handle identity).
#[derive(Debug)]
struct PropertyKeySet {
  buckets: HashMap<u64, Vec<PropertyKey>>,
  len: usize,
}

impl PropertyKeySet {
  fn with_capacity(capacity: usize) -> Result<Self, VmError> {
    let mut buckets = HashMap::new();
    buckets.try_reserve(capacity).map_err(|_| VmError::OutOfMemory)?;
    Ok(Self { buckets, len: 0 })
  }

  fn is_empty(&self) -> bool {
    self.len == 0
  }

  fn insert_unique(&mut self, heap: &crate::Heap, key: PropertyKey) -> Result<bool, VmError> {
    let h = hash_property_key(heap, &key)?;
    let bucket = self.buckets.entry(h).or_default();

    for existing in bucket.iter() {
      if heap.property_key_eq(existing, &key) {
        return Ok(false);
      }
    }

    bucket.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
    bucket.push(key);
    self.len += 1;
    Ok(true)
  }

  fn remove(&mut self, heap: &crate::Heap, key: &PropertyKey) -> Result<bool, VmError> {
    let h = hash_property_key(heap, key)?;
    let Some(bucket) = self.buckets.get_mut(&h) else {
      return Ok(false);
    };

    for i in 0..bucket.len() {
      if heap.property_key_eq(&bucket[i], key) {
        bucket.swap_remove(i);
        self.len -= 1;
        if bucket.is_empty() {
          self.buckets.remove(&h);
        }
        return Ok(true);
      }
    }
    Ok(false)
  }
}

fn validate_proxy_own_keys_trap_result(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target: GcObject,
  trap_result: &[PropertyKey],
) -> Result<(), VmError> {
  // Spec: https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-ownpropertykeys
  //
  // Invariants summary:
  // - Trap result must not contain duplicates.
  // - If the target is extensible, trap result must include all non-configurable target keys.
  // - If the target is not extensible, trap result must include *all* target keys and no extras.

  const TICK_EVERY: usize = 1024;
  let mut unchecked = PropertyKeySet::with_capacity(trap_result.len())?;
  for (i, key) in trap_result.iter().enumerate() {
    if i != 0 && i % TICK_EVERY == 0 {
      vm.tick()?;
    }
    if !unchecked.insert_unique(scope.heap(), *key)? {
      return Err(VmError::TypeError("Proxy ownKeys trap returned duplicate keys"));
    }
  }

  let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
  let target_keys = scope.object_own_property_keys_with_host_and_hooks(vm, host, hooks, target)?;

  for (i, key) in target_keys.iter().enumerate() {
    if i != 0 && i % TICK_EVERY == 0 {
      vm.tick()?;
    }

    let desc = scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, *key)?;
    let is_non_configurable = desc.is_some_and(|d| !d.configurable);

    if is_non_configurable || !extensible_target {
      if !unchecked.remove(scope.heap(), key)? {
        return Err(VmError::TypeError(
          "Proxy ownKeys trap omitted a required target key",
        ));
      }
    }
  }

  if !extensible_target && !unchecked.is_empty() {
    return Err(VmError::TypeError(
      "Proxy ownKeys trap returned extra keys for a non-extensible target",
    ));
  }

  Ok(())
}

fn property_key_to_value(key: PropertyKey) -> Value {
  match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  }
}

fn validate_proxy_get_trap_result(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target: GcObject,
  key: PropertyKey,
  trap_result: Value,
) -> Result<(), VmError> {
  // Spec: https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-get-p-receiver
  //
  // Invariants:
  // - If the target has a non-configurable, non-writable data property, the trap result must be
  //   `SameValue` to the target's value.
  // - If the target has a non-configurable accessor property whose getter is undefined, the trap
  //   result must be `undefined`.
  //
  // Root the trap result across descriptor lookup and any subsequent allocations/GC. The trap is
  // allowed to return a freshly-allocated value that is not otherwise reachable.
  let key_value = property_key_to_value(key);
  scope.push_roots(&[Value::Object(target), key_value, trap_result])?;

  let target_desc = scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, key)?;
  let Some(target_desc) = &target_desc else {
    return Ok(());
  };
  if target_desc.configurable {
    return Ok(());
  }

  match target_desc.kind {
    PropertyKind::Data { writable: false, .. } => {
      // Ensure string exotic index properties materialize their actual value.
      let Some(target_desc) =
        scope.object_get_own_property_with_host_and_hooks_complete(vm, host, hooks, target, key)?
      else {
        return Err(VmError::InvariantViolation(
          "validate_proxy_get_trap_result: internal error: missing target property descriptor",
        ));
      };
      let PropertyKind::Data { value: target_value, .. } = target_desc.kind else {
        return Err(VmError::InvariantViolation(
          "validate_proxy_get_trap_result: internal error: expected data descriptor",
        ));
      };

      // Root the descriptor value across any allocations (e.g. error construction).
      scope.push_root(target_value)?;

      if !trap_result.same_value(target_value, scope.heap()) {
        return Err(VmError::TypeError(
          "Proxy get trap returned a different value for a non-writable, non-configurable data property",
        ));
      }
    }
    PropertyKind::Accessor {
      get: Value::Undefined,
      ..
    } => {
      if !matches!(trap_result, Value::Undefined) {
        return Err(VmError::TypeError(
          "Proxy get trap returned a non-undefined value for a non-configurable accessor property with an undefined getter",
        ));
      }
    }
    _ => {}
  }

  Ok(())
}

fn proxy_target_and_handler(scope: &Scope<'_>, proxy: GcObject) -> Result<(GcObject, GcObject), VmError> {
  let target = scope.heap().proxy_target(proxy)?;
  let handler = scope.heap().proxy_handler(proxy)?;
  match (target, handler) {
    (Some(t), Some(h)) => Ok((t, h)),
    _ => Err(VmError::TypeError(
      "Cannot perform operation on revoked Proxy",
    )),
  }
}

impl<'a> Scope<'a> {
  /// Internal method `[[SetPrototypeOf]]`, dispatching on Proxy objects.
  pub fn set_prototype_of_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    proto: Option<GcObject>,
  ) -> Result<bool, VmError> {
    if self.heap().is_proxy_object(obj) {
      return proxy_set_prototype_of(vm, self, host, hooks, obj, proto);
    }

    // ImmutablePrototypeExoticObject (ECMA-262 §9.4.7.1) for `%Object.prototype%`.
    //
    // `Object.prototype`'s `[[SetPrototypeOf]]` is special-cased by the spec: it returns `true` when
    // `V` is the current prototype and `false` otherwise (even if `OrdinarySetPrototypeOf` would
    // normally succeed).
    if let Some(intr) = vm.intrinsics() {
      if obj == intr.object_prototype() {
        let current = self.heap().object_prototype(obj)?;
        return Ok(current == proto);
      }
    }

    // OrdinarySetPrototypeOf (ECMA-262 §9.1.2.1).
    //
    // Note: cycle detection must stop when a non-ordinary `[[GetPrototypeOf]]` is encountered. In
    // practice this means Proxy objects: if `proto`'s chain reaches a Proxy, we stop the check and
    // allow the prototype to be set even if it could create an indirect cycle (test262:
    // `built-ins/Object/prototype/__proto__/set-cycle-shadowed.js`).
    let current = self.heap().object_prototype(obj)?;
    if current == proto {
      return Ok(true);
    }
    if !self.heap().object_is_extensible(obj)? {
      return Ok(false);
    }

    // Cycle / hostile-chain checks. This is the spec's `p` walk (OrdinarySetPrototypeOf step 6+),
    // with extra guards against deep/cyclic chains to keep the VM from looping forever on
    // invariant-violating heaps.
    let mut p = proto;
    let mut steps = 0usize;
    let mut visited: HashSet<GcObject> = HashSet::new();
    while let Some(candidate) = p {
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Ok(false);
      }
      steps += 1;

      if candidate == obj {
        return Ok(false);
      }

      // Stop if `candidate.[[GetPrototypeOf]]` is not ordinary (Proxy).
      if self.heap().is_proxy_object(candidate) {
        break;
      }

      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(candidate) {
        return Ok(false);
      }

      // Ordinary object: advance to its `[[Prototype]]` internal slot.
      p = self.heap().object_prototype(candidate)?;
    }

    // We performed the necessary checks above; set the prototype without the heap-level cycle
    // check so Proxy boundaries remain observable.
    unsafe { self.heap_mut().object_set_prototype_unchecked(obj, proto)? };
    Ok(true)
  }

  /// Internal method `[[OwnPropertyKeys]]`, dispatching on Proxy objects.
  pub fn own_property_keys_with_host_and_hooks_with_tick(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
  ) -> Result<Vec<PropertyKey>, VmError> {
    if self.heap().is_proxy_object(obj) {
      return proxy_own_property_keys_with_tick(vm, self, host, hooks, obj, tick);
    }
    let mut tick0 = || tick(vm);
    self.ordinary_own_property_keys_with_tick(obj, &mut tick0)
  }

  /// Internal method `[[GetOwnProperty]]`, dispatching on Proxy objects.
  pub fn get_own_property_with_host_and_hooks_with_tick(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    if self.heap().is_proxy_object(obj) {
      return proxy_get_own_property_with_tick(vm, self, host, hooks, obj, key, tick);
    }

    // Module Namespace Exotic Objects `[[GetOwnProperty]]` must compute `[[Value]]` via `[[Get]]`,
    // which can throw `ReferenceError` for TDZ exports. `ordinary_get_own_property_with_tick` does
    // not have access to a `Vm` to translate heap-level TDZ sentinels to real error objects, so
    // handle this case here.
    if self.heap().object_is_module_namespace(obj)? {
      if let PropertyKey::String(s) = key {
        let Some(export) = self.heap().module_namespace_export(obj, s)? else {
          return Ok(None);
        };
        let mut scope = self.reborrow();
        scope.push_roots(&[Value::Object(obj), Value::String(s)])?;
        tick(vm)?;
        let value = scope.module_namespace_get_export_value(vm, obj, export)?;
        return Ok(Some(PropertyDescriptor {
          enumerable: true,
          configurable: false,
          kind: PropertyKind::Data {
            value,
            writable: true,
          },
        }));
      }
    }

    let mut tick0 = || tick(vm);
    self.ordinary_get_own_property_with_tick(obj, key, &mut tick0)
  }

  /// Internal method `[[DefineOwnProperty]]`, dispatching on Proxy objects.
  pub fn define_own_property_with_host_and_hooks_with_tick(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
    tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    if self.heap().is_proxy_object(obj) {
      return proxy_define_own_property_with_tick(vm, self, host, hooks, obj, key, desc);
    }

    // Array exotic `length` definitions require full spec `ToNumber` coercion when `[[Value]]` is
    // an object. `ArraySetLength` applies `ToUint32(Desc.[[Value]])` and `ToNumber(Desc.[[Value]])`
    // separately, so `ToPrimitive` can run twice and invoke user code.
    //
    // The existing array `[[DefineOwnProperty]]` implementation (`array_set_length_with_tick`)
    // cannot call user code (it lacks a `Vm` + host context), so perform the coercions here and
    // replace `desc.value` with the computed numeric length before dispatching to the regular array
    // machinery.
    if self.heap().object_is_array(obj)? && self.heap().property_key_is_length(&key) {
      if let Some(value) = desc.value {
        // Ensure descriptor patch invariants hold before invoking user code via `ToNumber`.
        desc.validate()?;

        if matches!(value, Value::Object(_)) {
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

          // Root the target and value across potential GC + user-code invocation in `ToNumber`.
          let mut scope = self.reborrow();
          let key_value = match key {
            PropertyKey::String(s) => Value::String(s),
            PropertyKey::Symbol(s) => Value::Symbol(s),
          };
          scope.push_roots(&[Value::Object(obj), key_value, value])?;

          // `ArraySetLength` step 3: `ToUint32(Desc.[[Value]])`.
          let n1 = scope.to_number(vm, host, hooks, value)?;
          let new_len = to_uint32(n1);
          // `ArraySetLength` step 4: `ToNumber(Desc.[[Value]])`.
          let number_len = scope.to_number(vm, host, hooks, value)?;

          // `ArraySetLength` step 5: `newLen ≠ numberLen` -> RangeError.
          if new_len as f64 != number_len {
            return Err(VmError::RangeError("Invalid array length"));
          }

          let mut new_desc = desc;
          new_desc.value = Some(Value::Number(new_len as f64));

          let mut tick0 = || tick(vm);
          return scope.define_own_property_with_tick(obj, key, new_desc, &mut tick0);
        }
      }
    }
    let mut tick0 = || tick(vm);
    self.define_own_property_with_tick(obj, key, desc, &mut tick0)
  }

  /// Internal method `[[Set]]`, dispatching on Proxy objects.
  pub fn set_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
    receiver: Value,
  ) -> Result<bool, VmError> {
    if self.heap().is_proxy_object(obj) {
      return proxy_set(vm, self, host, hooks, obj, key, value, receiver);
    }
    self.ordinary_set_with_host_and_hooks(vm, host, hooks, obj, key, value, receiver)
  }

  /// Internal method `[[HasProperty]]`, dispatching on Proxy objects.
  pub fn has_property_with_host_and_hooks_with_tick(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    // Fully implement `[[HasProperty]]` dispatch so Proxy objects can participate in prototype
    // chains (e.g. `('x' in Object.create(new Proxy(...)))`).
    //
    // This must be host-aware because Proxy `"has"` traps can invoke user code.
    //
    // Keep all temporary roots local to this operation.
    let mut scope = self.reborrow();
    let key_value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[Value::Object(obj), key_value])?;

    let mut visited: HashSet<GcObject> = HashSet::new();
    if visited.try_reserve(1).is_err() {
      return Err(VmError::OutOfMemory);
    }
    visited.insert(obj);

    // Cache the `"has"` trap key across Proxy hops so we only allocate it once per operation.
    let mut has_trap_key: Option<PropertyKey> = None;

    let mut current = obj;
    let mut steps = 0usize;
    loop {
      // Budget prototype/proxy traversal so deep chains can't run unbounded work inside a single
      // `HasProperty` operation.
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        tick(vm)?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      // --- Proxy [[HasProperty]] dispatch (partial) ---
      if scope.heap().is_proxy_object(current) {
        let Some(target) = scope.heap().proxy_target(current)? else {
          return Err(VmError::TypeError("Cannot perform 'has' on a revoked Proxy"));
        };
        let Some(handler) = scope.heap().proxy_handler(current)? else {
          return Err(VmError::TypeError("Cannot perform 'has' on a revoked Proxy"));
        };

        // Root `target`/`handler` across trap lookup + invocation. `GetMethod(handler, "has")` can
        // invoke user code via accessors, which can revoke this Proxy and then trigger a GC.
        scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

        // Let trap be ? GetMethod(handler, "has").
        let trap_key = match has_trap_key {
          Some(k) => k,
          None => {
            let s = scope.alloc_string("has")?;
            scope.push_root(Value::String(s))?;
            let k = PropertyKey::from_string(s);
            has_trap_key = Some(k);
            k
          }
        };
        let trap =
          vm.get_method_with_host_and_hooks(host, &mut scope, hooks, Value::Object(handler), trap_key)?;

        // If trap is undefined, forward to the target.
        let Some(trap) = trap else {
          current = target;
          // Root the forwarded `target` so it remains valid even if the Proxy was revoked while
          // looking up the trap.
          scope.push_root(Value::Object(current))?;
          if visited.try_reserve(1).is_err() {
            return Err(VmError::OutOfMemory);
          }
          if !visited.insert(current) {
            return Err(VmError::PrototypeCycle);
          }
          continue;
        };
        // Root the trap: it may be the result of an accessor getter and not otherwise reachable.
        scope.push_root(trap)?;

        let trap_args = [Value::Object(target), key_value];
        let trap_result = vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          trap,
          Value::Object(handler),
          &trap_args,
        )?;
        let trap_bool = scope.heap().to_boolean(trap_result)?;
 
        // Proxy invariants: if the trap reports `false`, the target must not have a non-configurable
        // property, and if the target is non-extensible it must not have any property at all.
        //
        // Spec: https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-hasproperty-p
        if !trap_bool {
          if let Some(target_desc) =
            scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, target, key, tick)?
          {
            if !target_desc.configurable {
              return Err(VmError::TypeError(
                "Proxy has trap returned false for a non-configurable target property",
              ));
            }
            let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
            if !extensible_target {
              return Err(VmError::TypeError(
                "Proxy has trap returned false for an existing property on a non-extensible target",
              ));
            }
          }
        }

        return Ok(trap_bool);
      }

      // --- Ordinary [[HasProperty]] ---
      //
      // Integer-indexed exotic objects (typed arrays): canonical numeric index strings are handled
      // without consulting the prototype chain.
      //
      // https://tc39.es/ecma262/#sec-integer-indexed-exotic-objects-hasproperty-p
      //
      // Module Namespace Exotic Objects: string keys are present iff they are in `[[Exports]]`
      // (and `[[HasProperty]]` must not touch binding values).
      if scope.heap().object_is_module_namespace(current)? {
        match key {
          PropertyKey::Symbol(_) => {
            // Symbols use ordinary behavior.
          }
          PropertyKey::String(s) => {
            return Ok(scope.heap().module_namespace_export(current, s)?.is_some());
          }
        }
      }
      if scope.heap().is_typed_array_object(current) {
        if let PropertyKey::String(s) = key {
          if let Some(numeric_index) = scope.heap().canonical_numeric_index_string(s)? {
            // `IsValidIntegerIndex`
            if numeric_index == 0.0 && numeric_index.is_sign_negative() {
              // -0 is a canonical numeric index string but never a valid integer index.
              return Ok(false);
            }
            if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
              return Ok(false);
            }
            if numeric_index < 0.0 {
              return Ok(false);
            }
            if numeric_index > usize::MAX as f64 {
              return Ok(false);
            }
            let index = numeric_index as usize;
            let len = scope.heap().typed_array_length(current)?;
            return Ok(index < len);
          }
        }
      }

      // Module Namespace Exotic Object `[[HasProperty]]` (ECMA-262 §9.4.6).
      //
      // `[[HasProperty]]` for module namespaces is defined in terms of the `[[Exports]]` list and
      // does **not** access the live binding value (important for TDZ correctness).
      if scope.heap().object_is_module_namespace(current)? {
        if let PropertyKey::String(s) = key {
          if scope.heap().module_namespace_export(current, s)?.is_some() {
            return Ok(true);
          }
        }
      }

      // Own property check.
      if scope
        .heap()
        .object_get_own_property_with_tick(current, &key, || tick(vm))?
        .is_some()
      {
        return Ok(true);
      }
      let mut tick0 = || tick(vm);
      if scope
        .string_object_in_range_index_with_tick(current, &key, &mut tick0)?
        .is_some()
      {
        return Ok(true);
      }

      let Some(proto) = scope.heap().object_prototype(current)? else {
        return Ok(false);
      };

      current = proto;
      // Root prototypes so a `__proto__` mutation during user code (Proxy traps) cannot invalidate
      // the iterator's internal object handle before we reach it.
      scope.push_root(Value::Object(current))?;
      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(current) {
        return Err(VmError::PrototypeCycle);
      }
    }
  }

  fn module_namespace_get_export_value(
    &mut self,
    vm: &mut Vm,
    _obj: GcObject,
    export: crate::heap::ModuleNamespaceExport,
  ) -> Result<Value, VmError> {
    match export.value {
      ModuleNamespaceExportValue::Namespace { namespace } => Ok(Value::Object(namespace)),
      ModuleNamespaceExportValue::Binding { env, name } => match self.heap().env_get_binding_value_by_gc_string(env, name) {
        Ok(v) => Ok(v),
        Err(VmError::Throw(Value::Null)) => {
          let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
            "module namespace access requires intrinsics for ReferenceError",
          ))?;
          let export_js = self.heap().get_string(export.name)?;
          let (export_name, _) = crate::string::utf16_to_utf8_lossy_bounded(
            export_js.as_code_units(),
            fallible_format::MAX_ERROR_MESSAGE_BYTES,
          )?;
          let message = fallible_format::try_format_error_message(
            "Cannot access '",
            &export_name,
            "' before initialization",
          )?;
          let err_obj = crate::new_reference_error(self, intr, &message)?;
          Err(VmError::Throw(err_obj))
        }
        Err(err) => Err(err),
      },
    }
  }
  pub fn object_get_prototype(&self, obj: GcObject) -> Result<Option<GcObject>, VmError> {
    self.heap().object_prototype(obj)
  }

  /// ECMAScript `[[GetOwnProperty]]` internal method dispatch.
  ///
  /// This is Proxy-aware: Proxy objects observe the `getOwnPropertyDescriptor` trap and throw on
  /// revoked proxies.
  pub(crate) fn get_own_property_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    let mut current = obj;
    let mut steps = 0usize;
    loop {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);

      let proxy = self.heap().get_proxy_data(current)?;
      let Some(proxy) = proxy else {
        // Module Namespace Exotic Objects `[[GetOwnProperty]]` must compute `[[Value]]` via `[[Get]]`,
        // which can throw `ReferenceError` for TDZ exports. `ordinary_get_own_property_with_tick` does
        // not have access to a `Vm` to translate heap-level TDZ sentinels to real error objects, so
        // handle this case here.
        if self.heap().object_is_module_namespace(current)? {
          if let PropertyKey::String(s) = key {
            let Some(export) = self.heap().module_namespace_export(current, s)? else {
              return Ok(None);
            };
            let mut scope = self.reborrow();
            scope.push_roots(&[Value::Object(current), Value::String(s)])?;
            let value = scope.module_namespace_get_export_value(vm, current, export)?;
            return Ok(Some(PropertyDescriptor {
              enumerable: true,
              configurable: false,
              kind: PropertyKind::Data {
                value,
                writable: true,
              },
            }));
          }
        }

        return self.ordinary_get_own_property_with_tick(current, key, || vm.tick());
      };

      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'getOwnPropertyDescriptor' on a proxy that has been revoked",
        ));
      };

      let mut scope = self.reborrow();
      let key_root = match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(sym) => Value::Symbol(sym),
      };
      scope.push_roots(&[
        Value::Object(current),
        Value::Object(target),
        Value::Object(handler),
        key_root,
      ])?;

      let gopd_s = scope.alloc_string("getOwnPropertyDescriptor")?;
      scope.push_root(Value::String(gopd_s))?;
      let gopd_key = PropertyKey::from_string(gopd_s);

      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        Value::Object(handler),
        gopd_key,
      )?;
      let Some(trap) = trap else {
        // No trap: forward to the target's `[[GetOwnProperty]]`.
        current = target;
        continue;
      };

      let trap_args = [Value::Object(target), key_root];
      let trap_result = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        trap,
        Value::Object(handler),
        &trap_args,
      )?;
      scope.push_root(trap_result)?;

      if matches!(trap_result, Value::Undefined) {
        return Ok(None);
      }

      let Value::Object(desc_obj) = trap_result else {
        return Err(VmError::TypeError(
          "Proxy getOwnPropertyDescriptor trap returned a non-object value",
        ));
      };

      let desc = property_descriptor_ops::to_property_descriptor_with_host_and_hooks(
        vm,
        &mut scope,
        host,
        hooks,
        desc_obj,
      )?;
      let desc = property_descriptor_ops::complete_property_descriptor(desc);
      return Ok(Some(desc));
    }
  }

  pub fn object_set_prototype(
    &mut self,
    obj: GcObject,
    prototype: Option<GcObject>,
  ) -> Result<(), VmError> {
    self.heap_mut().object_set_prototype(obj, prototype)
  }

  pub fn object_is_extensible(&self, obj: GcObject) -> Result<bool, VmError> {
    self.heap().object_is_extensible(obj)
  }

  pub fn object_prevent_extensions(&mut self, obj: GcObject) -> Result<(), VmError> {
    self.heap_mut().object_set_extensible(obj, false)
  }

  /// ECMAScript `[[GetOwnProperty]]` for ordinary objects.
  pub fn ordinary_get_own_property(
    &self,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.ordinary_get_own_property_with_tick(obj, key, || Ok(()))
  }

  pub fn ordinary_get_own_property_with_tick(
    &self,
    obj: GcObject,
    key: PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    // Module Namespace Exotic Object `[[GetOwnProperty]]` (ECMA-262 §9.4.6).
    //
    // Note: this is implemented here (rather than in `heap.object_get_own_property_with_tick`)
    // because it depends on module binding resolution state stored in the heap object kind.
    if self.heap().object_is_module_namespace(obj)? {
      let PropertyKey::String(s) = key else {
        // Symbols use ordinary behavior.
        return self
          .heap()
          .object_get_own_property_with_tick(obj, &key, &mut tick);
      };

      let Some(export) = self.heap().module_namespace_export(obj, s)? else {
        return Ok(None);
      };

      return Ok(Some(PropertyDescriptor {
        enumerable: true,
        configurable: false,
        kind: PropertyKind::Accessor {
          get: Value::Object(export.getter),
          set: Value::Undefined,
        },
      }));
    }
    if let Some(desc) = self
      .heap()
      .object_get_own_property_with_tick(obj, &key, &mut tick)?
    {
      return Ok(Some(desc));
    }

    let Some((_string_data, _index)) = self.string_object_in_range_index_with_tick(obj, &key, &mut tick)? else {
      return Ok(None);
    };

    Ok(Some(PropertyDescriptor {
      enumerable: true,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Undefined,
        writable: false,
      },
    }))
  }

  /// ECMAScript `[[GetOwnProperty]]` internal method dispatch.
  ///
  /// This is a spec-shaped wrapper around `OrdinaryGetOwnProperty` that routes Proxy objects
  /// through the `getOwnPropertyDescriptor` trap (when present).
  pub fn object_get_own_property_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.object_get_own_property_with_host_and_hooks_impl(vm, host, hooks, obj, key, false)
  }

  /// Like [`Scope::object_get_own_property_with_host_and_hooks`], but materializes string-exotic
  /// integer-index property values (i.e. `"0"`, `"1"`, ...) into the returned descriptor.
  ///
  /// vm-js models string index properties lazily by returning `[[Value]]: undefined` from
  /// `OrdinaryGetOwnProperty` and materializing the actual character value via `Get` only when the
  /// value is observed. This avoids allocating a new 1-code-unit string for each descriptor in
  /// algorithms that only need attributes like `[[Enumerable]]`.
  ///
  /// Builtins that expose descriptors to user code (`Object.getOwnPropertyDescriptor(s)`,
  /// `Reflect.getOwnPropertyDescriptor`) and Proxy invariant checks need the fully-materialized
  /// value to be spec-correct.
  pub fn object_get_own_property_with_host_and_hooks_complete(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.object_get_own_property_with_host_and_hooks_impl(vm, host, hooks, obj, key, true)
  }

  fn object_get_own_property_with_host_and_hooks_impl(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    materialize_string_index_value: bool,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    // Root inputs so a Proxy trap can allocate freely.
    let mut scope = self.reborrow();
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[Value::Object(obj), key_root])?;

    // --- Proxy [[GetOwnProperty]] dispatch (partial) ---
    if let Some(proxy) = scope.heap().get_proxy_data(obj)? {
      vm.tick()?;

      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'getOwnPropertyDescriptor' on a proxy that has been revoked",
        ));
      };

      // Root target/handler for the duration of trap lookup + invocation.
      scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      let trap_key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
      scope.push_root(Value::String(trap_key_s))?;
      let trap_key = PropertyKey::from_string(trap_key_s);

      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        Value::Object(handler),
        trap_key,
      )?;

      // If the trap is undefined, forward to the target's `[[GetOwnProperty]]`.
      let Some(trap) = trap else {
        return scope.object_get_own_property_with_host_and_hooks_impl(
          vm,
          host,
          hooks,
          target,
          key,
          materialize_string_index_value,
        );
      };

      // Call the trap with `(target, key)`.
      let trap_args = [Value::Object(target), key_root];
      let trap_result = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        trap,
        Value::Object(handler),
        &trap_args,
      )?;
      // Root the raw trap result value across subsequent allocations/GC. In particular, the trap
      // is allowed to return a freshly-allocated descriptor object that is not reachable from any
      // other heap object.
      scope.push_root(trap_result)?;

      // Invariants require comparing the trap result with the target's actual descriptor.
      let target_desc =
        scope.object_get_own_property_with_host_and_hooks_impl(vm, host, hooks, target, key, true)?;

      // Root any values from `target_desc` across subsequent allocations/GC (especially important
      // if `target_desc` itself came from a nested Proxy trap and is not reachable from `target`'s
      // own property table).
      let mut desc_value_roots = [Value::Undefined; 2];
      let mut desc_value_root_count = 0usize;
      if let Some(desc) = &target_desc {
        match desc.kind {
          PropertyKind::Data { value, .. } => {
            desc_value_roots[desc_value_root_count] = value;
            desc_value_root_count += 1;
          }
          PropertyKind::Accessor { get, set } => {
            desc_value_roots[desc_value_root_count] = get;
            desc_value_root_count += 1;
            desc_value_roots[desc_value_root_count] = set;
            desc_value_root_count += 1;
          }
        }
      }
      if desc_value_root_count != 0 {
        scope.push_roots(&desc_value_roots[..desc_value_root_count])?;
      }

      // Per spec, `undefined` means "no own property", but the trap is not allowed to report a
      // missing property if the target has a non-configurable property or is non-extensible.
      if matches!(trap_result, Value::Undefined) {
        let Some(target_desc) = target_desc else {
          return Ok(None);
        };
        if !target_desc.configurable {
          return Err(VmError::TypeError(
            "Proxy getOwnPropertyDescriptor trap returned undefined for a non-configurable target property",
          ));
        }
        let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
        if !extensible_target {
          return Err(VmError::TypeError(
            "Proxy getOwnPropertyDescriptor trap returned undefined for an existing property on a non-extensible target",
          ));
        }
        return Ok(None);
      }

      let Value::Object(desc_obj) = trap_result else {
        return Err(VmError::TypeError(
          "Proxy getOwnPropertyDescriptor trap returned non-object",
        ));
      };

      // Spec: `extensibleTarget = IsExtensible(target)` is evaluated before `ToPropertyDescriptor`
      // (which can invoke user code via accessors on the descriptor object).
      let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;

      scope.push_root(Value::Object(desc_obj))?;
      let patch = crate::property_descriptor_ops::to_property_descriptor_with_host_and_hooks(
        vm,
        &mut scope,
        host,
        hooks,
        desc_obj,
      )?;

      let result_desc = crate::property_descriptor_ops::complete_property_descriptor(patch);

      // `IsCompatiblePropertyDescriptor(extensibleTarget, resultDesc, targetDesc)`.
      let result_patch = match result_desc.kind {
        PropertyKind::Data { value, writable } => PropertyDescriptorPatch {
          enumerable: Some(result_desc.enumerable),
          configurable: Some(result_desc.configurable),
          value: Some(value),
          writable: Some(writable),
          get: None,
          set: None,
        },
        PropertyKind::Accessor { get, set } => PropertyDescriptorPatch {
          enumerable: Some(result_desc.enumerable),
          configurable: Some(result_desc.configurable),
          value: None,
          writable: None,
          get: Some(get),
          set: Some(set),
        },
      };
      let compatible = crate::property_descriptor_ops::is_compatible_property_descriptor(
        extensible_target,
        result_patch,
        target_desc,
        scope.heap(),
      );
      if !compatible {
        return Err(VmError::TypeError(
          "Proxy getOwnPropertyDescriptor trap returned an incompatible property descriptor",
        ));
      }

      // Additional non-configurable invariants:
      // if the trap reports `configurable: false`, the target must already have a non-configurable
      // property.
      if !result_desc.configurable {
        let Some(target_desc) = target_desc else {
          return Err(VmError::TypeError(
            "Proxy getOwnPropertyDescriptor trap reported a non-configurable descriptor for a non-existent property",
          ));
        };
        if target_desc.configurable {
          return Err(VmError::TypeError(
            "Proxy getOwnPropertyDescriptor trap reported a non-configurable descriptor for a configurable target property",
          ));
        }
      }

      return Ok(Some(result_desc));
    }

    // Module Namespace Exotic Object `[[GetOwnProperty]]` (ECMA-262 §9.4.6).
    //
    // Module namespaces compute the descriptor's `[[Value]]` via `[[Get]]`, which can throw
    // (notably, a `ReferenceError` for TDZ access). Ensure we compute the value using the full VM
    // semantics rather than the heap-level environment lookup.
    let desc = if scope.heap().object_is_module_namespace(obj)? {
      match key {
        PropertyKey::Symbol(_) => scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())?,
        PropertyKey::String(s) => {
          let export = scope.heap().module_namespace_export(obj, s)?;
          if let Some(export) = export {
            let value = scope.module_namespace_get_export_value(vm, obj, export)?;
            Some(PropertyDescriptor {
              enumerable: true,
              configurable: false,
              kind: PropertyKind::Data {
                value,
                writable: true,
              },
            })
          } else {
            None
          }
        }
      }
    } else {
      scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())?
    };
    let Some(mut desc) = desc else {
      return Ok(None);
    };
    if let PropertyKind::Data {
      value: Value::Undefined,
      writable,
    } = desc.kind
    {
      // BigInt typed array element values are materialized lazily at the heap layer (since they
      // require allocating fresh BigInt handles). Materialize them here so `[[GetOwnProperty]]`
      // returns spec-correct descriptors.
      if let Some(value) = scope.typed_array_get_index_value_with_tick(obj, &key, || vm.tick())? {
        desc.kind = PropertyKind::Data { value, writable };
      }

      if materialize_string_index_value {
        if let Some(value) = scope.string_object_get_index_value_with_tick(obj, &key, || vm.tick())? {
          desc.kind = PropertyKind::Data { value, writable };
        }
      }
    }
    Ok(Some(desc))
  }

  /// ECMAScript `[[Get]]` internal method dispatch.
  ///
  /// This is a spec-shaped wrapper around `OrdinaryGet` that routes Proxy objects through the
  /// `get` trap (when present).
  pub fn object_get_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    self.get_with_host_and_hooks(vm, host, hooks, obj, key, receiver)
  }

  /// ECMAScript `[[DefineOwnProperty]]` for ordinary objects.
  pub fn ordinary_define_own_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    desc.validate()?;

    // Root all inputs that might be written into the heap before any allocation/GC.
    //
    // Important: root them *together* so if growing the root stack triggers a GC, we do not collect
    // the property key / descriptor values before they've been pushed.
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(obj);
    root_count += 1;
    roots[root_count] = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    root_count += 1;
    if let Some(v) = desc.value {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.get {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.set {
      roots[root_count] = v;
      root_count += 1;
    }
    self.push_roots(&roots[..root_count])?;

    let current = self.heap().object_get_own_property(obj, &key)?;
    let extensible = self.heap().object_is_extensible(obj)?;

    validate_and_apply_property_descriptor(self, Some(obj), key, extensible, desc, current)
  }

  /// ECMAScript `[[DefineOwnProperty]]`.
  ///
  /// This dispatches to the appropriate exotic object's `[[DefineOwnProperty]]` algorithm.
  pub fn define_own_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    self.define_own_property_with_tick(obj, key, desc, || Ok(()))
  }

  pub fn define_own_property_with_tick(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    if self.heap().object_is_module_namespace(obj)? {
      return self.module_namespace_define_own_property(obj, key, desc);
    }
    if self.heap().object_is_array(obj)? {
      self.array_define_own_property_with_tick(obj, key, desc, &mut tick)
    } else if self
      .string_object_in_range_index_with_tick(obj, &key, &mut tick)?
      .is_some()
    {
      self.string_define_own_property_index(obj, key, desc)
    } else if self.heap().is_typed_array_object(obj) {
      self.typed_array_define_own_property(obj, key, desc)
    } else {
      self.ordinary_define_own_property(obj, key, desc)
    }
  }

  /// ECMAScript `DefinePropertyOrThrow`.
  ///
  /// This is a convenience wrapper around [`Scope::define_own_property`]. If the
  /// definition is rejected (`false`), this returns a `TypeError`.
  pub fn define_property_or_throw(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<(), VmError> {
    // Root `obj`, `key`, and any `desc` values for the duration of the operation.
    //
    // This is important even for *rejected* definitions: when `gc_threshold` is low, pushing the
    // first stack root can trigger a GC, and any not-yet-rooted key/descriptor values would be
    // collected if the operation ultimately returns `false`.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(obj);
    root_count += 1;
    roots[root_count] = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    root_count += 1;
    if let Some(v) = desc.value {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.get {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.set {
      roots[root_count] = v;
      root_count += 1;
    }
    scope.push_roots(&roots[..root_count])?;

    let ok = scope.define_own_property(obj, key, desc)?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("DefinePropertyOrThrow rejected"))
    }
  }

  /// ECMAScript `DefinePropertyOrThrow`, using `[[DefineOwnProperty]]` dispatch that can invoke user
  /// code (Proxy traps).
  pub fn define_property_or_throw_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<(), VmError> {
    // Root `obj`, `key`, and any `desc` values for the duration of the operation.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(obj);
    root_count += 1;
    roots[root_count] = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    root_count += 1;
    if let Some(v) = desc.value {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.get {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.set {
      roots[root_count] = v;
      root_count += 1;
    }
    scope.push_roots(&roots[..root_count])?;

    let mut tick = Vm::tick;
    let ok =
      scope.define_own_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, desc, &mut tick)?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("DefinePropertyOrThrow rejected"))
    }
  }

  /// ECMAScript `[[HasProperty]]` for ordinary objects.
  pub fn ordinary_has_property(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<bool, VmError> {
    self.ordinary_has_property_with_tick(vm, obj, key, Vm::tick)
  }

  /// ECMAScript `[[HasProperty]]` internal method dispatch.
  ///
  /// This dispatches to Proxy objects' `[[HasProperty]]` algorithm (invoking the `"has"` trap when
  /// present).
  pub fn has_property_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<bool, VmError> {
    let mut tick = |vm: &mut Vm| vm.tick();
    self.has_property_with_host_and_hooks_with_tick(vm, host, hooks, obj, key, &mut tick)
  }

  pub fn ordinary_has_property_with_tick(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
    mut tick: impl FnMut(&mut Vm) -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    // Root inputs so Proxy traps can allocate freely.
    let mut scope = self.reborrow();
    let key_value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[Value::Object(obj), key_value])?;

    let mut visited: HashSet<GcObject> = HashSet::new();
    if visited.try_reserve(1).is_err() {
      return Err(VmError::OutOfMemory);
    }
    visited.insert(obj);

    // Cache the `"has"` trap key across Proxy hops so we only allocate it once per operation.
    let mut has_trap_key: Option<PropertyKey> = None;

    let mut current = obj;
    let mut steps = 0usize;
    loop {
      // Budget prototype/proxy traversal so deep chains can't run unbounded work inside a single
      // `HasProperty` operation.
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        tick(vm)?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      // --- Proxy [[HasProperty]] dispatch (partial) ---
      if scope.heap().is_proxy_object(current) {
        let Some(target) = scope.heap().proxy_target(current)? else {
          return Err(VmError::TypeError("Cannot perform 'has' on a revoked Proxy"));
        };
        let Some(handler) = scope.heap().proxy_handler(current)? else {
          return Err(VmError::TypeError("Cannot perform 'has' on a revoked Proxy"));
        };

        // Root `target`/`handler` across trap lookup + invocation. `GetMethod(handler, "has")` can
        // invoke user code via accessors, which can revoke this Proxy and then trigger a GC.
        scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

        // Let trap be ? GetMethod(handler, "has").
        let trap_key = match has_trap_key {
          Some(k) => k,
          None => {
            let s = scope.alloc_string("has")?;
            scope.push_root(Value::String(s))?;
            let k = PropertyKey::from_string(s);
            has_trap_key = Some(k);
            k
          }
        };
        let trap = vm.get_method(&mut scope, Value::Object(handler), trap_key)?;

        // If the trap is undefined, forward to the target.
        let Some(trap) = trap else {
          current = target;
          // Root the forwarded `target` so it remains valid even if the Proxy was revoked while
          // looking up the trap.
          scope.push_root(Value::Object(current))?;
          if visited.try_reserve(1).is_err() {
            return Err(VmError::OutOfMemory);
          }
          if !visited.insert(current) {
            return Err(VmError::PrototypeCycle);
          }
          continue;
        };
        // Root the trap: it may be the result of an accessor getter and not otherwise reachable.
        scope.push_root(trap)?;

        let trap_args = [Value::Object(target), key_value];
        let mut dummy_host = ();
        let trap_result =
          vm.call(&mut dummy_host, &mut scope, trap, Value::Object(handler), &trap_args)?;
        return scope.heap().to_boolean(trap_result);
      }

      // --- Ordinary [[HasProperty]] ---
      if scope.heap().object_is_module_namespace(current)? {
        match key {
          PropertyKey::Symbol(_) => {
            // Symbols use ordinary behavior.
          }
          PropertyKey::String(s) => {
            return Ok(scope.heap().module_namespace_export(current, s)?.is_some());
          }
        }
      }

      // Integer-indexed exotic objects (typed arrays): canonical numeric index strings are handled
      // without consulting the prototype chain.
      //
      // https://tc39.es/ecma262/#sec-integer-indexed-exotic-objects-hasproperty-p
      if scope.heap().is_typed_array_object(current) {
        if let PropertyKey::String(s) = key {
          if let Some(numeric_index) = scope.heap().canonical_numeric_index_string(s)? {
            // `IsValidIntegerIndex`
            if numeric_index == 0.0 && numeric_index.is_sign_negative() {
              // -0 is a canonical numeric index string but never a valid integer index.
              return Ok(false);
            }
            if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
              return Ok(false);
            }
            if numeric_index < 0.0 {
              return Ok(false);
            }
            if numeric_index > usize::MAX as f64 {
              return Ok(false);
            }
            let index = numeric_index as usize;
            let len = scope.heap().typed_array_length(current)?;
            return Ok(index < len);
          }
        }
      }

      // Own property check.
      if scope
        .heap()
        .object_get_own_property_with_tick(current, &key, || tick(vm))?
        .is_some()
      {
        return Ok(true);
      }
      let mut tick0 = || tick(vm);
      if scope
        .string_object_in_range_index_with_tick(current, &key, &mut tick0)?
        .is_some()
      {
        return Ok(true);
      }

      let Some(proto) = scope.heap().object_prototype(current)? else {
        return Ok(false);
      };

      current = proto;
      // Root prototypes so a `__proto__` mutation during user code (Proxy traps) cannot invalidate
      // the iterator's internal object handle before we reach it.
      scope.push_root(Value::Object(current))?;
      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(current) {
        return Err(VmError::PrototypeCycle);
      }
    }
  }

  /// ECMAScript `[[Get]]` internal method dispatch.
  ///
  /// This dispatches to Proxy `[[Get]]` when `obj` is a Proxy object; otherwise it falls back to
  /// ordinary object `[[Get]]` semantics.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// Proxy traps and accessor getters are invoked using a **dummy host context** (`()`), mirroring
  /// [`Scope::ordinary_get`]. Host embeddings that need native handlers to observe real host state
  /// should prefer [`Scope::get_with_host_and_hooks`].
  pub fn get(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    // Fast path: ordinary objects.
    if !self.heap().is_proxy_object(obj) {
      return self.ordinary_get(vm, obj, key, receiver);
    }

    // Root inputs across proxy trap lookup and invocation (which can allocate and trigger GC).
    let mut scope = self.reborrow();
    let key_value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[Value::Object(obj), key_value, receiver])?;

    // `GetMethod(handler, "get")` key.
    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);

    let mut current = obj;
    let mut steps = 0usize;
    loop {
      // Budget proxy chains to preserve interrupt/deadline responsiveness.
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !scope.heap().is_proxy_object(current) {
        return scope.ordinary_get(vm, current, key, receiver);
      }

      let (Some(target), Some(handler)) = (
        scope.heap().proxy_target(current)?,
        scope.heap().proxy_handler(current)?,
      ) else {
        return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
      };

      // Root the Proxy's `[[ProxyTarget]]` and `[[ProxyHandler]]` while we look up and invoke the
      // `get` trap.
      //
      // `GetMethod(handler, "get")` can run user code via accessor properties. That user code can
      // revoke `current` (clearing `[[ProxyTarget]]`/`[[ProxyHandler]]`) and then trigger a GC.
      // If that happens, `target` could become unreachable and collected even though the Proxy
      // algorithm is still required to use the original target object for this operation.
      scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      let trap = vm.get_method(&mut scope, Value::Object(handler), get_key)?;
      match trap {
        None => {
          current = target;
          continue;
        }
        Some(trap) => {
          let args = [Value::Object(target), key_value, receiver];
          // Like `ordinary_get`, prefer `Vm::call` so any active host hooks override is honored.
          let mut dummy_host = ();
          let trap_result = vm.call(&mut dummy_host, &mut scope, trap, Value::Object(handler), &args)?;

          // Enforce Proxy `[[Get]]` invariants (ECMA-262).
          //
          // This requires `target.[[GetOwnProperty]](P)`, which can invoke user JS via nested Proxy
          // traps, so route the check through the active host hook implementation when available.
          if let Some(hooks_ptr) = vm.active_host_hooks_ptr() {
            // SAFETY: `active_host_hooks_ptr` is only set while a host hooks implementation is
            // mutably borrowed by a VM entrypoint (e.g. `Vm::call_with_host_and_hooks` or
            // `Vm::with_host_hooks_override`).
            let hooks = unsafe { &mut *hooks_ptr };
            validate_proxy_get_trap_result(
              vm,
              &mut scope,
              &mut dummy_host,
              hooks,
              target,
              key,
              trap_result,
            )?;
          } else {
            let mut hooks = std::mem::take(vm.microtask_queue_mut());
            let result = validate_proxy_get_trap_result(
              vm,
              &mut scope,
              &mut dummy_host,
              &mut hooks,
              target,
              key,
              trap_result,
            );
            // Merge any Promise jobs that native code enqueued directly onto the VM-owned queue
            // while it was temporarily moved out.
            struct DrainCtx<'a> {
              heap: &'a mut crate::Heap,
            }
            impl VmJobContext for DrainCtx<'_> {
              fn call(
                &mut self,
                _hooks: &mut dyn VmHostHooks,
                _callee: Value,
                _this: Value,
                _args: &[Value],
              ) -> Result<Value, VmError> {
                Err(VmError::Unimplemented("DrainCtx::call"))
              }

              fn construct(
                &mut self,
                _hooks: &mut dyn VmHostHooks,
                _callee: Value,
                _args: &[Value],
                _new_target: Value,
              ) -> Result<Value, VmError> {
                Err(VmError::Unimplemented("DrainCtx::construct"))
              }

              fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
                self.heap.add_root(value)
              }

              fn remove_root(&mut self, id: RootId) {
                self.heap.remove_root(id);
              }
            }

            let mut ctx = DrainCtx {
              heap: scope.heap_mut(),
            };
            let mut drain_err: Option<VmError> = None;
            while let Some((realm, job)) = vm.microtask_queue_mut().pop_front() {
              if let Err(err) = hooks.host_enqueue_promise_job_fallible(&mut ctx, job, realm) {
                drain_err = Some(err);
                break;
              }
            }
            if drain_err.is_some() {
              vm.microtask_queue_mut().teardown(&mut ctx);
            }
            *vm.microtask_queue_mut() = hooks;
            if let Some(err) = drain_err {
              return Err(err);
            }
            result?;
          }

          return Ok(trap_result);
        }
      }
    }
  }

  /// ECMAScript `[[Get]]` internal method dispatch, using an explicit embedder host context and host
  /// hook implementation.
  pub fn get_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    // Fast path: non-Proxy objects use the ordinary `[[Get]]` internal method.
    if !self.heap().is_proxy_object(obj) {
      return self.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, receiver);
    }

    // Root the inputs so host hook implementations and Proxy traps can allocate freely.
    //
    // Note: `obj` might be a Proxy object, which does not have an `ObjectBase`; rooting it keeps
    // its `[[ProxyTarget]]` / `[[ProxyHandler]]` alive (until revoked).
    let mut scope = self.reborrow();
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[Value::Object(obj), key_root, receiver])?;

    // Follow Proxy chains iteratively to avoid recursion.
    let mut current = obj;
    let mut steps = 0usize;

    // Cache the `"get"` trap key across Proxy hops so we only allocate it once per operation.
    let mut get_trap_key: Option<PropertyKey> = None;

    loop {
      // Budget Proxy chain traversal. Mirror the property-table scan budget in
      // `Heap::object_get_own_property_with_tick`: avoid ticking on the first iteration so a simple
      // `Get` does not double-charge fuel (the surrounding expression evaluation already ticks).
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !scope.heap().is_proxy_object(current) {
        return scope.ordinary_get_with_host_and_hooks(vm, host, hooks, current, key, receiver);
      }

      let Some(target) = scope.heap().proxy_target(current)? else {
        return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
      };
      let Some(handler) = scope.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
      };
      // Root the Proxy's `[[ProxyTarget]]` and `[[ProxyHandler]]` while we look up and invoke the
      // `get` trap.
      //
      // `GetMethod(handler, "get")` can run user code via accessor properties. That user code can
      // revoke `current` (clearing `[[ProxyTarget]]`/`[[ProxyHandler]]`) and then trigger a GC.
      // If that happens, `target` could become unreachable and collected even though the Proxy
      // algorithm is still required to use the original target object for this operation.
      scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // Let trap be ? GetMethod(handler, "get").
      let get_key = match get_trap_key {
        Some(k) => k,
        None => {
          let s = scope.alloc_string("get")?;
          scope.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          get_trap_key = Some(k);
          k
        }
      };

      // `GetMethod` uses `GetV`/`ToObject`. Here `handler` is already an object.
      let trap = scope.get_with_host_and_hooks(vm, host, hooks, handler, get_key, Value::Object(handler))?;

      // If trap is undefined or null, forward to the target.
      if matches!(trap, Value::Undefined | Value::Null) {
        current = target;
        continue;
      }
      if !scope.heap().is_callable(trap)? {
        return Err(VmError::TypeError("Proxy get trap is not callable"));
      }

      let key_value = match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      };
      let args = [Value::Object(target), key_value, receiver];
      let trap_result =
        vm.call_with_host_and_hooks(host, &mut scope, hooks, trap, Value::Object(handler), &args)?;
      validate_proxy_get_trap_result(vm, &mut scope, host, hooks, target, key, trap_result)?;
      return Ok(trap_result);
    }
  }

  /// ECMAScript `[[Delete]]` internal method dispatch, using an explicit embedder host context and
  /// host hook implementation.
  ///
  /// This dispatches to Proxy objects' `[[Delete]]` algorithm (invoking the `"deleteProperty"` trap
  /// when present).
  pub fn delete_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<bool, VmError> {
    let key_value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    self.push_roots(&[Value::Object(obj), key_value])?;

    // Fast path: ordinary object.
    if !self.heap().is_proxy_object(obj) {
      return self.ordinary_delete_with_host_and_hooks(vm, host, hooks, obj, key);
    }

    let mut current = obj;
    let mut steps = 0usize;
    let mut delete_trap_key: Option<PropertyKey> = None;

    loop {
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !self.heap().is_proxy_object(current) {
        return self.ordinary_delete_with_host_and_hooks(vm, host, hooks, current, key);
      }

      let Some(target) = self.heap().proxy_target(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'deleteProperty' on a revoked Proxy",
        ));
      };
      let Some(handler) = self.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'deleteProperty' on a revoked Proxy",
        ));
      };

      // Root target/handler across trap lookup and invocation. `GetMethod(handler, "deleteProperty")`
      // can run user code via accessors which can revoke the Proxy and trigger a GC; the operation
      // must still use the original `target` afterwards.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // trap = ? GetMethod(handler, "deleteProperty")
      let trap_key = match delete_trap_key {
        Some(k) => k,
        None => {
          let s = self.alloc_string("deleteProperty")?;
          self.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          delete_trap_key = Some(k);
          k
        }
      };
      let trap =
        vm.get_method_with_host_and_hooks(host, self, hooks, Value::Object(handler), trap_key)?;
      let Some(trap) = trap else {
        current = target;
        continue;
      };
      self.push_root(trap)?;

      let trap_args = [Value::Object(target), key_value];
      let trap_result = vm.call_with_host_and_hooks(
        host,
        self,
        hooks,
        trap,
        Value::Object(handler),
        &trap_args,
      )?;
      let trap_bool = self.heap().to_boolean(trap_result)?;
      if !trap_bool {
        return Ok(false);
      }

      // Proxy invariants (ECMA-262 `Proxy.[[Delete]]`):
      // A successful trap cannot report deletion of a non-configurable property.
      let target_desc =
        self.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, key)?;
      if let Some(desc) = target_desc {
        if !desc.configurable {
          return Err(VmError::TypeError(
            "Proxy deleteProperty trap returned true for a non-configurable property",
          ));
        }
      }
      return Ok(true);
    }
  }

  /// ECMAScript `[[OwnPropertyKeys]]` internal method dispatch.
  ///
  /// This dispatches to Proxy objects' `[[OwnPropertyKeys]]` algorithm (invoking the `"ownKeys"`
  /// trap when present).
  pub fn own_property_keys_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<Vec<PropertyKey>, VmError> {
    // Root `obj` for the duration of the operation. This is important when `obj` is a Proxy: the
    // target/handler slots are not otherwise reachable.
    self.push_root(Value::Object(obj))?;

    // Fast path: ordinary object.
    if !self.heap().is_proxy_object(obj) {
      return self.ordinary_own_property_keys_with_tick(obj, || vm.tick());
    }

    let mut current = obj;
    let mut steps = 0usize;
    let mut own_keys_trap_key: Option<PropertyKey> = None;

    loop {
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !self.heap().is_proxy_object(current) {
        return self.ordinary_own_property_keys_with_tick(current, || vm.tick());
      }

      let Some(target) = self.heap().proxy_target(current)? else {
        return Err(VmError::TypeError("Cannot perform 'ownKeys' on a revoked Proxy"));
      };
      let Some(handler) = self.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError("Cannot perform 'ownKeys' on a revoked Proxy"));
      };

      // Root target/handler across trap lookup and invocation. `GetMethod(handler, "ownKeys")` can
      // run user code via accessors which can revoke the Proxy and trigger a GC; the operation must
      // still use the original `target` afterwards.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // trap = ? GetMethod(handler, "ownKeys")
      let trap_key = match own_keys_trap_key {
        Some(k) => k,
        None => {
          let s = self.alloc_string("ownKeys")?;
          self.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          own_keys_trap_key = Some(k);
          k
        }
      };
      let trap =
        vm.get_method_with_host_and_hooks(host, self, hooks, Value::Object(handler), trap_key)?;
      let Some(trap) = trap else {
        current = target;
        continue;
      };
      self.push_root(trap)?;

      let trap_result = vm.call_with_host_and_hooks(
        host,
        self,
        hooks,
        trap,
        Value::Object(handler),
        &[Value::Object(target)],
      )?;
      let Value::Object(trap_result_obj) = trap_result else {
        return Err(VmError::TypeError("Proxy ownKeys trap returned non-object"));
      };

      // Root the trap result so any string/symbol keys produced by `CreateListFromArrayLike` remain
      // reachable across allocations in callers (e.g. `Reflect.ownKeys` building a result array).
      self.push_root(Value::Object(trap_result_obj))?;

      let values = crate::spec_ops::create_list_from_array_like_with_host_and_hooks(
        vm,
        self,
        host,
        hooks,
        trap_result_obj,
      )?;

      let mut out: Vec<PropertyKey> = Vec::new();
      out
        .try_reserve_exact(values.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for (i, v) in values.into_iter().enumerate() {
        if i != 0 && i % 1024 == 0 {
          vm.tick()?;
        }
      match v {
        Value::String(s) => out.push(PropertyKey::from_string(s)),
        Value::Symbol(s) => out.push(PropertyKey::from_symbol(s)),
        _ => {
            return Err(VmError::TypeError(
              "Proxy ownKeys trap returned a value that is not a string or symbol",
            ))
          }
        }
      }

      validate_proxy_own_keys_trap_result(vm, self, host, hooks, target, &out)?;
      return Ok(out);
    }
  }

  /// ECMAScript `[[GetPrototypeOf]]` internal method dispatch.
  ///
  /// This dispatches to Proxy objects' `[[GetPrototypeOf]]` algorithm (invoking the
  /// `"getPrototypeOf"` trap when present).
  pub fn get_prototype_of_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<Option<GcObject>, VmError> {
    self.push_root(Value::Object(obj))?;

    if !self.heap().is_proxy_object(obj) {
      return self.object_get_prototype(obj);
    }

    // When walking Proxy chains, avoid recursion: attacker-controlled Proxy chains can be very deep
    // and can otherwise overflow the Rust stack.
    //
    // We mirror the recursive spec algorithm by:
    // - descending through Proxy targets when the `"getPrototypeOf"` trap is absent, and
    // - for present traps on **non-extensible targets**, recording the trap results and validating
    //   the invariants while unwinding (ECMA-262 Proxy `[[GetPrototypeOf]]`).
    //
    // Invariants (ECMA-262):
    // - If the target is extensible, any object/null trap result is allowed.
    // - If the target is non-extensible, the trap result must be the target's actual prototype.
    let mut pending_non_extensible_trap_protos: Vec<Option<GcObject>> = Vec::new();

    let mut current = obj;
    let mut steps = 0usize;
    let mut get_proto_trap_key: Option<PropertyKey> = None;

    loop {
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !self.heap().is_proxy_object(current) {
        let proto = self.object_get_prototype(current)?;
        for expected in pending_non_extensible_trap_protos.iter().rev() {
          if *expected != proto {
            return Err(VmError::TypeError(
              "Proxy getPrototypeOf trap returned invalid prototype for non-extensible target",
            ));
          }
        }
        return Ok(proto);
      }

      let Some(target) = self.heap().proxy_target(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'getPrototypeOf' on a revoked Proxy",
        ));
      };
      let Some(handler) = self.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'getPrototypeOf' on a revoked Proxy",
        ));
      };
      // Root `target`/`handler` across trap lookup + invocation. `GetMethod` can invoke user code
      // via accessors, which can revoke this Proxy and trigger GC.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // Root target/handler across trap lookup and invocation. `GetMethod(handler, "getPrototypeOf")`
      // can run user code via accessors which can revoke the Proxy and trigger a GC; the operation
      // must still use the original `target` afterwards.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      let trap_key = match get_proto_trap_key {
        Some(k) => k,
        None => {
          let s = self.alloc_string("getPrototypeOf")?;
          self.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          get_proto_trap_key = Some(k);
          k
        }
      };
      let trap =
        vm.get_method_with_host_and_hooks(host, self, hooks, Value::Object(handler), trap_key)?;
      let Some(trap) = trap else {
        current = target;
        continue;
      };
      self.push_root(trap)?;

      let trap_result = vm.call_with_host_and_hooks(
        host,
        self,
        hooks,
        trap,
        Value::Object(handler),
        &[Value::Object(target)],
      )?;
      // Root the trap result: we may need it to remain alive across further proxy traversal and/or
      // `IsExtensible(target)` checks.
      self.push_root(trap_result)?;

      let trap_proto = match trap_result {
        Value::Null => None,
        Value::Object(o) => Some(o),
        _ => {
          return Err(VmError::TypeError(
            "Proxy getPrototypeOf trap returned non-object",
          ))
        }
      };

      // Proxy invariant: only constrain trap result if the target is non-extensible.
      let extensible_target = self.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
      if extensible_target {
        for expected in pending_non_extensible_trap_protos.iter().rev() {
          if *expected != trap_proto {
            return Err(VmError::TypeError(
              "Proxy getPrototypeOf trap returned invalid prototype for non-extensible target",
            ));
          }
        }
        return Ok(trap_proto);
      }

      pending_non_extensible_trap_protos
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      pending_non_extensible_trap_protos.push(trap_proto);

      // Continue evaluating `target.[[GetPrototypeOf]]()` to validate the invariant.
      current = target;
    }
  }

  /// ECMAScript `[[IsExtensible]]` internal method dispatch.
  ///
  /// This dispatches to Proxy objects' `[[IsExtensible]]` algorithm (invoking the `"isExtensible"`
  /// trap when present).
  pub fn is_extensible_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<bool, VmError> {
    self.push_root(Value::Object(obj))?;

    if !self.heap().is_proxy_object(obj) {
      return self.object_is_extensible(obj);
    }

    // When walking Proxy chains, avoid recursion: attacker-controlled Proxy chains can be very deep
    // and can otherwise overflow the Rust stack.
    //
    // We record each trap result while descending and validate Proxy invariants while unwinding
    // (mirroring the recursive spec algorithm).
    let mut trap_results: Vec<bool> = Vec::new();

    let mut current = obj;
    let mut steps = 0usize;
    let mut is_ext_trap_key: Option<PropertyKey> = None;

    let mut result = loop {
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !self.heap().is_proxy_object(current) {
        break self.object_is_extensible(current)?;
      }

      let Some(target) = self.heap().proxy_target(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'isExtensible' on a revoked Proxy",
        ));
      };
      let Some(handler) = self.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'isExtensible' on a revoked Proxy",
        ));
      };

      // Root `target`/`handler` across trap lookup + invocation. `GetMethod` can invoke user code
      // via accessors which can revoke the proxy and trigger GC; we must still use the original
      // slots for this operation.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      let trap_key = match is_ext_trap_key {
        Some(k) => k,
        None => {
          let s = self.alloc_string("isExtensible")?;
          self.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          is_ext_trap_key = Some(k);
          k
        }
      };
      let trap =
        vm.get_method_with_host_and_hooks(host, self, hooks, Value::Object(handler), trap_key)?;
      let Some(trap) = trap else {
        current = target;
        continue;
      };
      self.push_root(trap)?;

      let trap_result = vm.call_with_host_and_hooks(
        host,
        self,
        hooks,
        trap,
        Value::Object(handler),
        &[Value::Object(target)],
      )?;
      let trap_bool = self.heap().to_boolean(trap_result)?;

      // Defer invariant checks until we know the target's actual extensibility.
      trap_results
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      trap_results.push(trap_bool);

      current = target;
    };

    // Validate Proxy invariants from inner-most Proxy to outer-most Proxy.
    while let Some(trap_bool) = trap_results.pop() {
      if trap_bool != result {
        return Err(VmError::TypeError(
          "Proxy isExtensible trap result does not reflect target extensibility",
        ));
      }
      result = trap_bool;
    }

    Ok(result)
  }

  /// ECMAScript `[[PreventExtensions]]` internal method dispatch.
  ///
  /// This dispatches to Proxy objects' `[[PreventExtensions]]` algorithm (invoking the
  /// `"preventExtensions"` trap when present).
  pub fn prevent_extensions_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<bool, VmError> {
    self.push_root(Value::Object(obj))?;

    if !self.heap().is_proxy_object(obj) {
      self.object_prevent_extensions(obj)?;
      return Ok(true);
    }

    let mut current = obj;
    let mut steps = 0usize;
    let mut prevent_ext_trap_key: Option<PropertyKey> = None;

    loop {
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !self.heap().is_proxy_object(current) {
        self.object_prevent_extensions(current)?;
        return Ok(true);
      }

      let Some(target) = self.heap().proxy_target(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'preventExtensions' on a revoked Proxy",
        ));
      };
      let Some(handler) = self.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'preventExtensions' on a revoked Proxy",
        ));
      };
      // Root `target`/`handler` across trap lookup + invocation. `GetMethod` can invoke user code
      // via accessors, which can revoke this Proxy and trigger GC.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // Root target/handler across trap lookup and invocation. `GetMethod(handler, "preventExtensions")`
      // can run user code via accessors which can revoke the Proxy and trigger a GC; the operation
      // must still use the original `target` afterwards.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      let trap_key = match prevent_ext_trap_key {
        Some(k) => k,
        None => {
          let s = self.alloc_string("preventExtensions")?;
          self.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          prevent_ext_trap_key = Some(k);
          k
        }
      };
      let trap =
        vm.get_method_with_host_and_hooks(host, self, hooks, Value::Object(handler), trap_key)?;
      let Some(trap) = trap else {
        current = target;
        continue;
      };
      self.push_root(trap)?;

      let trap_result = vm.call_with_host_and_hooks(
        host,
        self,
        hooks,
        trap,
        Value::Object(handler),
        &[Value::Object(target)],
      )?;
      let ok = self.heap().to_boolean(trap_result)?;
      if !ok {
        return Ok(false);
      }

      // Proxy invariants: a successful `preventExtensions` must make the target non-extensible.
      if self.is_extensible_with_host_and_hooks(vm, host, hooks, target)? {
        return Err(VmError::TypeError(
          "Proxy preventExtensions trap returned true but target is still extensible",
        ));
      }
      return Ok(true);
    }
  }

  /// ECMAScript `[[DefineOwnProperty]]` internal method dispatch.
  ///
  /// This dispatches to Proxy objects' `[[DefineOwnProperty]]` algorithm (invoking the
  /// `"defineProperty"` trap when present).
  pub fn define_own_property_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    desc.validate()?;
    let mut desc = desc;

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

    let key_value = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };

    // Root inputs and any descriptor values for the duration of the operation.
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(obj);
    root_count += 1;
    roots[root_count] = key_value;
    root_count += 1;
    if let Some(v) = desc.value {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.get {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.set {
      roots[root_count] = v;
      root_count += 1;
    }
    self.push_roots(&roots[..root_count])?;

    if !self.heap().is_proxy_object(obj) {
      // Array exotic `length` coercion: see `define_own_property_with_host_and_hooks_with_tick`.
      if self.heap().object_is_array(obj)? && self.heap().property_key_is_length(&key) {
        let Some(value) = desc.value else {
          return self.define_own_property_with_tick(obj, key, desc, || vm.tick());
        };
        if matches!(value, Value::Object(_)) {
          // `ArraySetLength` step 3: `ToUint32(Desc.[[Value]])`.
          let n1 = self.to_number(vm, host, hooks, value)?;
          let new_len = to_uint32(n1);
          // `ArraySetLength` step 4: `ToNumber(Desc.[[Value]])`.
          let number_len = self.to_number(vm, host, hooks, value)?;
          // `ArraySetLength` step 5: `newLen ≠ numberLen` -> RangeError.
          if new_len as f64 != number_len {
            return Err(VmError::RangeError("Invalid array length"));
          }
          desc.value = Some(Value::Number(new_len as f64));
        }
      }
      return self.define_own_property_with_tick(obj, key, desc, || vm.tick());
    }

    let mut current = obj;
    let mut steps = 0usize;
    let mut define_trap_key: Option<PropertyKey> = None;

    loop {
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !self.heap().is_proxy_object(current) {
        // Array exotic `length` coercion: see `define_own_property_with_host_and_hooks_with_tick`.
        if self.heap().object_is_array(current)? && self.heap().property_key_is_length(&key) {
          let Some(value) = desc.value else {
            return self.define_own_property_with_tick(current, key, desc, || vm.tick());
          };
          if matches!(value, Value::Object(_)) {
            // `ArraySetLength` step 3: `ToUint32(Desc.[[Value]])`.
            let n1 = self.to_number(vm, host, hooks, value)?;
            let new_len = to_uint32(n1);
            // `ArraySetLength` step 4: `ToNumber(Desc.[[Value]])`.
            let number_len = self.to_number(vm, host, hooks, value)?;
            // `ArraySetLength` step 5: `newLen ≠ numberLen` -> RangeError.
            if new_len as f64 != number_len {
              return Err(VmError::RangeError("Invalid array length"));
            }
            desc.value = Some(Value::Number(new_len as f64));
          }
        }
        return self.define_own_property_with_tick(current, key, desc, || vm.tick());
      }

      let Some(target) = self.heap().proxy_target(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'defineProperty' on a revoked Proxy",
        ));
      };
      let Some(handler) = self.heap().proxy_handler(current)? else {
        return Err(VmError::TypeError(
          "Cannot perform 'defineProperty' on a revoked Proxy",
        ));
      };

      // Root the Proxy's `[[ProxyTarget]]` and `[[ProxyHandler]]` while we look up and invoke the
      // trap.
      //
      // `GetMethod(handler, "defineProperty")` can run user code via accessors. That user code can
      // revoke `current` (clearing its internal slots) and trigger a GC, but the operation must
      // still use the original `target` afterwards.
      self.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      let trap_key = match define_trap_key {
        Some(k) => k,
        None => {
          let s = self.alloc_string("defineProperty")?;
          self.push_root(Value::String(s))?;
          let k = PropertyKey::from_string(s);
          define_trap_key = Some(k);
          k
        }
      };
      let trap =
        vm.get_method_with_host_and_hooks(host, self, hooks, Value::Object(handler), trap_key)?;
      let Some(trap) = trap else {
        current = target;
        continue;
      };
      self.push_root(trap)?;

      // Spec: `descObj = FromPropertyDescriptor(Desc)`.
      let desc_obj = crate::property_descriptor_ops::from_property_descriptor_patch(self, desc)?;
      self.push_root(Value::Object(desc_obj))?;

      let trap_args = [Value::Object(target), key_value, Value::Object(desc_obj)];
      let trap_result = vm.call_with_host_and_hooks(
        host,
        self,
        hooks,
        trap,
        Value::Object(handler),
        &trap_args,
      )?;
      let ok = self.heap().to_boolean(trap_result)?;
      if !ok {
        return Ok(false);
      }

      // Proxy invariants: if the trap reports success, the resulting definition must be compatible
      // with the target.
      let target_desc =
        self.object_get_own_property_with_host_and_hooks_complete(vm, host, hooks, target, key)?;
      let extensible = self.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
      if !crate::property_descriptor_ops::is_compatible_property_descriptor(
        extensible,
        desc,
        target_desc,
        self.heap(),
      ) {
        return Err(VmError::TypeError(
          "Proxy defineProperty trap returned true for an incompatible property descriptor",
        ));
      }

      // Additional non-configurable invariants:
      //
      // If the caller requested `configurable: false`, the trap cannot report success unless the
      // target now has a non-configurable property.
      if desc.configurable == Some(false) {
        let Some(target_desc) = target_desc else {
          return Err(VmError::TypeError(
            "Proxy defineProperty trap returned true for a missing non-configurable property",
          ));
        };
        if target_desc.configurable {
          return Err(VmError::TypeError(
            "Proxy defineProperty trap returned true for a configurable target property when defining non-configurable",
          ));
        }
      }

      return Ok(true);
    }
  }

  /// ECMAScript `[[Get]]` for ordinary objects.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// Accessor getters are invoked using a **dummy host context** (`()`). Host embeddings that need
  /// native handlers to observe real host state should prefer
  /// [`Scope::ordinary_get_with_host_and_hooks`].
  pub fn ordinary_get(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    // Built-ins and tests sometimes call `ordinary_get` on Proxy receivers. Rather than crashing
    // with `InvalidHandle`, delegate to the Proxy-aware internal-method dispatch.
    if self.heap().is_proxy_object(obj) {
      return self.get(vm, obj, key, receiver);
    }

    // Root inputs for the duration of the operation: Proxy traps/accessor getters can invoke user
    // code and allocate, so we must keep `obj`, `key`, and `receiver` alive across GC.
    let mut scope = self.reborrow();
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
      receiver,
    ];
    scope.push_roots(&roots)?;
    let key_value = roots[1];

    // Module Namespace Exotic Object `[[Get]]` (ECMA-262 §9.4.6).
    if scope.heap().object_is_module_namespace(obj)? {
      if let PropertyKey::String(s) = key {
        let Some(export) = scope.heap().module_namespace_export(obj, s)? else {
          return Ok(Value::Undefined);
        };
        return scope.module_namespace_get_export_value(vm, obj, export);
      }
    }

    // Fast path: own property.
    if let Some(desc) = scope
      .heap()
      .object_get_own_property_with_tick(obj, &key, || vm.tick())?
    {
      return match desc.kind {
        PropertyKind::Data { value, .. } => {
          if matches!(value, Value::Undefined) {
            // BigInt typed array element values are materialized lazily.
            if let Some(value) =
              scope.typed_array_get_index_value_with_tick(obj, &key, || vm.tick())?
            {
              return Ok(value);
            }
          }
          Ok(value)
        }
        PropertyKind::Accessor { get, .. } => {
          if matches!(get, Value::Undefined) {
            Ok(Value::Undefined)
          } else {
            if !scope.heap().is_callable(get)? {
              return Err(VmError::TypeError("accessor getter is not callable"));
            }
            // Use `Vm::call` (with a dummy host context) so an embedder-provided
            // `Vm::with_host_hooks_override` is respected. `call_without_host` always forces the
            // VM-owned microtask queue, bypassing any active host hooks override.
            let mut dummy_host = ();
            vm.call(&mut dummy_host, &mut scope, get, receiver, &[])
          }
        }
      };
    }

    if let Some(value) = scope.string_object_get_index_value_with_tick(obj, &key, || vm.tick())? {
      return Ok(value);
    }

    // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys do not
    // consult the prototype chain. If we didn't find an own property above, this is an invalid
    // integer index.
    if scope.heap().is_typed_array_object(obj) {
      if let PropertyKey::String(s) = key {
        if scope.heap_mut().canonical_numeric_index_string(s)?.is_some() {
          return Ok(Value::Undefined);
        }
      }
    }

    // Annex B `caller` / `arguments`: for ordinary non-strict functions, legacy `.caller`/`.arguments`
    // must not throw even though %Function.prototype% defines poison-pill accessors.
    if scope.heap().property_key_is_caller(&key) || scope.heap().property_key_is_arguments(&key) {
      if let Ok(func) = scope.heap().get_function(obj) {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let proto = scope.heap().object_prototype(obj)?;
        let is_ordinary_function = proto == Some(intr.function_prototype());
        let is_restricted = func.is_strict
          || func.bound_target.is_some()
          || func.this_mode == ThisMode::Lexical
          || !is_ordinary_function;
        if is_restricted {
          return Err(VmError::TypeError("Restricted function property"));
        }
        return Ok(Value::Null);
      }
    }

    let Some(proto) = scope.heap().object_prototype(obj)? else {
      return Ok(Value::Undefined);
    };
    // Root the initial prototype so it remains valid even if a Proxy trap later in the chain runs
    // user code that mutates the prototype chain and triggers GC.
    scope.push_root(Value::Object(proto))?;

    // Walk the prototype chain iteratively so Proxy objects in the chain are handled by their
    // `[[Get]]` internal method (including `get` traps and revoked-proxy errors).
    let mut visited: HashSet<GcObject> = HashSet::new();
    if visited.try_reserve(2).is_err() {
      return Err(VmError::OutOfMemory);
    }
    visited.insert(obj);
    if !visited.insert(proto) {
      return Err(VmError::PrototypeCycle);
    }

    let mut current = proto;
    let mut steps = 0usize;

    // Cache the `"get"` trap key across Proxy hops so we only allocate it once per operation.
    let mut get_trap_key: Option<PropertyKey> = None;

    loop {
      // Budget prototype/proxy traversal so deep chains can't run unbounded work inside a single
      // `Get(O, P)` operation.
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      // --- Proxy [[Get]] dispatch (partial) ---
      if scope.heap().is_proxy_object(current) {
        let Some(target) = scope.heap().proxy_target(current)? else {
          return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
        };
        let Some(handler) = scope.heap().proxy_handler(current)? else {
          return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
        };
        // Root `target`/`handler` across trap lookup + invocation. `Get` can run user code via
        // accessors on the handler, which can revoke this Proxy and then trigger a GC.
        scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

        // Let trap be ? GetMethod(handler, "get").
        let get_key = match get_trap_key {
          Some(k) => k,
          None => {
            let s = scope.alloc_string("get")?;
            scope.push_root(Value::String(s))?;
            let k = PropertyKey::from_string(s);
            get_trap_key = Some(k);
            k
          }
        };

        // `GetMethod` uses `GetV`/`ToObject`. Here `handler` is already an object.
        let trap = scope.get(vm, handler, get_key, Value::Object(handler))?;

        // If trap is undefined or null, forward to the target.
        if matches!(trap, Value::Undefined | Value::Null) {
          current = target;
          if visited.try_reserve(1).is_err() {
            return Err(VmError::OutOfMemory);
          }
          if !visited.insert(current) {
            return Err(VmError::PrototypeCycle);
          }
          // Root the forwarded target so it remains valid across GC while the traversal continues.
          scope.push_root(Value::Object(current))?;
          continue;
        }
        if !scope.heap().is_callable(trap)? {
          return Err(VmError::TypeError("Proxy get trap is not callable"));
        }
        // Root the trap: it may be the result of an accessor getter and not otherwise reachable.
        scope.push_root(trap)?;

        let args = [Value::Object(target), key_value, receiver];
        let mut dummy_host = ();
        return vm.call(&mut dummy_host, &mut scope, trap, Value::Object(handler), &args);
      }

      // Module Namespace Exotic Object `[[Get]]` (ECMA-262 §9.4.6).
      if scope.heap().object_is_module_namespace(current)? {
        if let PropertyKey::String(s) = key {
          let Some(export) = scope.heap().module_namespace_export(current, s)? else {
            return Ok(Value::Undefined);
          };
          return scope.module_namespace_get_export_value(vm, current, export);
        }
      }

      // --- Ordinary [[Get]] ---
      //
      // Fast path: own property.
      if let Some(desc) = scope
        .heap()
        .object_get_own_property_with_tick(current, &key, || vm.tick())?
      {
        return match desc.kind {
          PropertyKind::Data { value, .. } => Ok(value),
          PropertyKind::Accessor { get, .. } => {
            if matches!(get, Value::Undefined) {
              Ok(Value::Undefined)
            } else {
              if !scope.heap().is_callable(get)? {
                return Err(VmError::TypeError("accessor getter is not callable"));
              }
              let mut dummy_host = ();
              vm.call(&mut dummy_host, &mut scope, get, receiver, &[])
            }
          }
        };
      }

      if let Some(value) =
        scope.string_object_get_index_value_with_tick(current, &key, || vm.tick())?
      {
        return Ok(value);
      }

      // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys do not
      // consult the prototype chain. If we didn't find an own property above, this is an invalid
      // integer index.
      if scope.heap().is_typed_array_object(current) {
        if let PropertyKey::String(s) = key {
          if scope.heap_mut().canonical_numeric_index_string(s)?.is_some() {
            return Ok(Value::Undefined);
          }
        }
      }

      let Some(proto) = scope.heap().object_prototype(current)? else {
        return Ok(Value::Undefined);
      };

      current = proto;
      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(current) {
        return Err(VmError::PrototypeCycle);
      }
      // Root prototypes so a `__proto__` mutation during user code (Proxy traps) cannot invalidate
      // the iterator's internal object handle before we reach it.
      scope.push_root(Value::Object(current))?;
    }
  }

  /// ECMAScript `[[Get]]` for ordinary objects, using an explicit embedder host context and host
  /// hook implementation.
  pub fn ordinary_get_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    // Some built-ins historically call `ordinary_get_with_host_and_hooks` even when the receiver is
    // a Proxy object. Rather than crashing with `InvalidHandle`, dispatch to the Proxy-aware
    // `[[Get]]` implementation.
    if self.heap().is_proxy_object(obj) {
      return self.get_with_host_and_hooks(vm, host, hooks, obj, key, receiver);
    }

    // Root the inputs so host hook implementations can allocate freely.
    //
    // This is particularly important for `host_exotic_get`, which can allocate new JS strings when
    // synthesizing values (e.g. `DOMStringMap` / `element.dataset` style shims).
    let mut scope = self.reborrow();
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
      receiver,
    ];
    scope.push_roots(&roots)?;

    let key_value = roots[1];
    if scope.heap().object_is_module_namespace(obj)? {
      if let PropertyKey::String(s) = key {
        let Some(export) = scope.heap().module_namespace_export(obj, s)? else {
          return Ok(Value::Undefined);
        };
        return scope.module_namespace_get_export_value(vm, obj, export);
      }
    }

    // Fast path: own property.
    if let Some(desc) = scope
      .heap()
      .object_get_own_property_with_tick(obj, &key, || vm.tick())?
    {
      return match desc.kind {
        PropertyKind::Data { value, .. } => {
          if matches!(value, Value::Undefined) {
            // BigInt typed array element values are materialized lazily.
            if let Some(value) = scope.typed_array_get_index_value_with_tick(obj, &key, || vm.tick())? {
              return Ok(value);
            }
          }
          Ok(value)
        }
        PropertyKind::Accessor { get, .. } => {
          if matches!(get, Value::Undefined) {
            Ok(Value::Undefined)
          } else {
            if !scope.heap().is_callable(get)? {
              return Err(VmError::TypeError("accessor getter is not callable"));
            }
            vm.call_with_host_and_hooks(host, &mut scope, hooks, get, receiver, &[])
          }
        }
      };
    }

    if let Some(value) = scope.string_object_get_index_value_with_tick(obj, &key, || vm.tick())? {
      return Ok(value);
    }

    // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys do not
    // consult the prototype chain or host exotic hooks. If we didn't find an own property above,
    // this is an invalid integer index.
    if scope.heap().is_typed_array_object(obj) {
      if let PropertyKey::String(s) = key {
        if scope.heap_mut().canonical_numeric_index_string(s)?.is_some() {
          return Ok(Value::Undefined);
        }
      }
    }

    // Annex B `caller` / `arguments`: for ordinary non-strict functions, legacy `.caller`/`.arguments`
    // must not throw even though %Function.prototype% defines poison-pill accessors.
    if scope.heap().property_key_is_caller(&key) || scope.heap().property_key_is_arguments(&key) {
      if let Ok(func) = scope.heap().get_function(obj) {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let proto = scope.heap().object_prototype(obj)?;
        let is_ordinary_function = proto == Some(intr.function_prototype());
        let is_restricted = func.is_strict
          || func.bound_target.is_some()
          || func.this_mode == ThisMode::Lexical
          || !is_ordinary_function;
        if is_restricted {
          return Err(VmError::TypeError("Restricted function property"));
        }
        return Ok(Value::Null);
      }
    }

    // Host hook for "exotic" property getters (e.g. DOM named properties) runs before walking the
    // prototype chain.
    if let Some(value) = hooks.host_exotic_get(&mut scope, obj, key, receiver)? {
      return Ok(value);
    }

    let Some(proto) = scope.heap().object_prototype(obj)? else {
      return Ok(Value::Undefined);
    };
    // Root the initial prototype so it remains valid even if a Proxy trap later in the chain runs
    // user code that mutates the prototype chain and triggers GC.
    scope.push_root(Value::Object(proto))?;

    // Walk the prototype chain iteratively so Proxy objects in the chain are handled by their
    // `[[Get]]` internal method (including `get` traps and revoked-proxy errors).
    let mut visited: HashSet<GcObject> = HashSet::new();
    if visited.try_reserve(2).is_err() {
      return Err(VmError::OutOfMemory);
    }
    visited.insert(obj);
    if !visited.insert(proto) {
      return Err(VmError::PrototypeCycle);
    }

    let mut current = proto;
    let mut steps = 0usize;

    // Cache the `"get"` trap key across Proxy hops so we only allocate it once per operation.
    let mut get_trap_key: Option<PropertyKey> = None;

    loop {
      // Budget prototype/proxy traversal so deep chains can't run unbounded work inside a single
      // `Get(O, P)` operation.
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      // --- Proxy [[Get]] dispatch (partial) ---
      if scope.heap().is_proxy_object(current) {
        let Some(target) = scope.heap().proxy_target(current)? else {
          return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
        };
        let Some(handler) = scope.heap().proxy_handler(current)? else {
          return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
        };
        // Root `target`/`handler` across trap lookup + invocation. `Get` can run user code via
        // accessors on the handler, which can revoke this Proxy and then trigger a GC.
        scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

        // Let trap be ? GetMethod(handler, "get").
        let get_key = match get_trap_key {
          Some(k) => k,
          None => {
            let s = scope.alloc_string("get")?;
            scope.push_root(Value::String(s))?;
            let k = PropertyKey::from_string(s);
            get_trap_key = Some(k);
            k
          }
        };

        // `GetMethod` uses `GetV`/`ToObject`. Here `handler` is already an object.
        let trap = scope.get_with_host_and_hooks(
          vm,
          host,
          hooks,
          handler,
          get_key,
          Value::Object(handler),
        )?;

        // If trap is undefined or null, forward to the target.
        if matches!(trap, Value::Undefined | Value::Null) {
          current = target;
          if visited.try_reserve(1).is_err() {
            return Err(VmError::OutOfMemory);
          }
          if !visited.insert(current) {
            return Err(VmError::PrototypeCycle);
          }
          // Root the forwarded target so it remains valid across GC while the traversal continues.
          scope.push_root(Value::Object(current))?;
          continue;
        }
        if !scope.heap().is_callable(trap)? {
          return Err(VmError::TypeError("Proxy get trap is not callable"));
        }
        // Root the trap: it may be the result of an accessor getter and not otherwise reachable.
        scope.push_root(trap)?;

        let args = [Value::Object(target), key_value, receiver];
        let trap_result = vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          trap,
          Value::Object(handler),
          &args,
        )?;
        validate_proxy_get_trap_result(vm, &mut scope, host, hooks, target, key, trap_result)?;
        return Ok(trap_result);
      }

      // Fast path: own property.
      if let Some(desc) = scope
        .heap()
        .object_get_own_property_with_tick(current, &key, || vm.tick())?
      {
        return match desc.kind {
          PropertyKind::Data { value, .. } => Ok(value),
          PropertyKind::Accessor { get, .. } => {
            if matches!(get, Value::Undefined) {
              Ok(Value::Undefined)
            } else {
              if !scope.heap().is_callable(get)? {
                return Err(VmError::TypeError("accessor getter is not callable"));
              }
              vm.call_with_host_and_hooks(host, &mut scope, hooks, get, receiver, &[])
            }
          }
        };
      }

      if let Some(value) =
        scope.string_object_get_index_value_with_tick(current, &key, || vm.tick())?
      {
        return Ok(value);
      }

      // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys do not
      // consult the prototype chain or host exotic hooks. If we didn't find an own property above,
      // this is an invalid integer index.
      if scope.heap().is_typed_array_object(current) {
        if let PropertyKey::String(s) = key {
          if scope.heap_mut().canonical_numeric_index_string(s)?.is_some() {
            return Ok(Value::Undefined);
          }
        }
      }

      // Host hook for "exotic" property getters (e.g. DOM named properties) runs before walking the
      // prototype chain.
      if let Some(value) = hooks.host_exotic_get(&mut scope, current, key, receiver)? {
        return Ok(value);
      }

      let Some(proto) = scope.heap().object_prototype(current)? else {
        return Ok(Value::Undefined);
      };

      current = proto;
      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(current) {
        return Err(VmError::PrototypeCycle);
      }
      // Root prototypes so a `__proto__` mutation during user code (Proxy traps) cannot invalidate
      // the iterator's internal object handle before we reach it.
      scope.push_root(Value::Object(current))?;
    }
  }

  /// ECMAScript `[[Get]]` for ordinary objects using a custom host hook implementation.
  ///
  /// This mirrors [`Scope::ordinary_get`], but invokes accessor getters via [`Vm::call_with_host`]
  /// so any Promise jobs enqueued during the getter run are routed via `host` (instead of the
  /// VM-owned microtask queue used by [`Vm::call`]).
  pub fn ordinary_get_with_host(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    // Delegate to the full `[[Get]]` implementation so:
    // - Proxy objects (including those in the prototype chain) observe `get` traps, and
    // - accessors are invoked via `Vm::call_with_host` semantics (Promise jobs routed via `host`).
    let mut dummy_host = ();
    self.ordinary_get_with_host_and_hooks(vm, &mut dummy_host, host, obj, key, receiver)
  }

  /// ECMAScript `[[Set]]` for ordinary objects.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// Accessor setters are invoked using a **dummy host context** (`()`). Host embeddings that need
  /// native handlers to observe real host state should prefer
  /// [`Scope::ordinary_set_with_host_and_hooks`].
  pub fn ordinary_set(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
    receiver: Value,
  ) -> Result<bool, VmError> {
    // Root inputs together so GC cannot collect `key`/`value`/`receiver` while growing the root
    // stack (important when setting a new property on an object, where the key/value are not yet
    // reachable from any heap object).
    //
    // Use a temporary child scope so these roots (and any prototypes we root while traversing) are
    // released when the operation completes.
    let mut scope = self.reborrow();
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
      value,
      receiver,
    ];
    scope.push_roots(&roots)?;
    let key_value = roots[1];

    // Annex B `caller` / `arguments`: for ordinary non-strict functions, legacy `.caller`/`.arguments`
    // must not throw even though %Function.prototype% defines poison-pill accessors.
    if scope.heap().property_key_is_caller(&key) || scope.heap().property_key_is_arguments(&key) {
      if let Ok(func) = scope.heap().get_function(obj) {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let proto = scope.heap().object_prototype(obj)?;
        let is_ordinary_function = proto == Some(intr.function_prototype());
        let is_restricted = func.is_strict
          || func.bound_target.is_some()
          || func.this_mode == ThisMode::Lexical
          || !is_ordinary_function;
        if is_restricted {
          return Err(VmError::TypeError("Restricted function property"));
        }
        return Ok(true);
      }
    }

    // Spec-shaped OrdinarySet implementation: we must not scan the prototype chain as ordinary
    // objects, since the chain can contain Proxy objects. If the property is not an own property,
    // delegate to `proto.[[Set]]` so Proxy `set` traps are observed.
    //
    // Implement this by walking the chain iteratively so we can keep a visited set to guard against
    // prototype cycles.
    let mut visited: HashSet<GcObject> = HashSet::new();
    if visited.try_reserve(1).is_err() {
      return Err(VmError::OutOfMemory);
    }
    visited.insert(obj);

    // Cache the `"set"` trap key across Proxy hops so we only allocate it once per operation.
    let mut set_trap_key: Option<PropertyKey> = None;

    let mut current = obj;
    let mut steps = 0usize;

    loop {
      // Budget prototype/proxy traversal so deep chains can't run unbounded work inside a single
      // `Set(O, P, V, Receiver)` operation.
      const TICK_EVERY: usize = 1024;
      if steps != 0 && steps % TICK_EVERY == 0 {
        vm.tick()?;
      }
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      // --- Proxy [[Set]] dispatch (partial) ---
      if scope.heap().is_proxy_object(current) {
        let Some(target) = scope.heap().proxy_target(current)? else {
          return Err(VmError::TypeError("Cannot perform 'set' on a revoked Proxy"));
        };
        let Some(handler) = scope.heap().proxy_handler(current)? else {
          return Err(VmError::TypeError("Cannot perform 'set' on a revoked Proxy"));
        };

        // Root `target`/`handler` across trap lookup + invocation. `GetMethod(handler, "set")` can
        // invoke user code via accessors, which can revoke this Proxy and then trigger a GC.
        scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

        // Let trap be ? GetMethod(handler, "set").
        let trap_key = match set_trap_key {
          Some(k) => k,
          None => {
            let s = scope.alloc_string("set")?;
            scope.push_root(Value::String(s))?;
            let k = PropertyKey::from_string(s);
            set_trap_key = Some(k);
            k
          }
        };

        let trap = vm.get_method(&mut scope, Value::Object(handler), trap_key)?;

        // If the trap is undefined, forward to the target.
        let Some(trap) = trap else {
          current = target;
          if visited.try_reserve(1).is_err() {
            return Err(VmError::OutOfMemory);
          }
          if !visited.insert(current) {
            return Err(VmError::PrototypeCycle);
          }
          // Root the forwarded target so it remains valid across GC while the traversal continues.
          scope.push_root(Value::Object(current))?;
          continue;
        };

        let trap_args = [Value::Object(target), key_value, value, receiver];
        // Like `ordinary_get`, prefer `Vm::call` so any active host hook override is honored.
        let mut dummy_host = ();
        let trap_result = vm.call(
          &mut dummy_host,
          &mut scope,
          trap,
          Value::Object(handler),
          &trap_args,
        )?;
        let trap_ok = scope.heap().to_boolean(trap_result)?;
        if !trap_ok {
          return Ok(false);
        }

        // Proxy invariants (ECMA-262):
        // If the target has a non-configurable, non-writable data property, the trap cannot report
        // success unless `value` is `SameValue` to the target's current value. Similarly, for
        // non-configurable accessor properties without a setter, the trap cannot report success.
        //
        // Note: full Proxy `[[GetOwnProperty]]` dispatch for proxy targets is implemented in the
        // host-aware path. Here we enforce invariants when `target` is a non-Proxy object.
        if !scope.heap().is_proxy_object(target) {
          let target_desc = scope.ordinary_get_own_property_with_tick(target, key, || vm.tick())?;
          if let Some(target_desc) = target_desc {
            if !target_desc.configurable {
              match target_desc.kind {
                PropertyKind::Data {
                  writable: false,
                  value: mut target_value,
                  ..
                } => {
                  // Ensure string exotic index properties materialize their actual value.
                  if matches!(target_value, Value::Undefined) {
                    if let Some(v) =
                      scope.string_object_get_index_value_with_tick(target, &key, || vm.tick())?
                    {
                      target_value = v;
                    }
                  }

                  if !value.same_value(target_value, scope.heap()) {
                    return Err(VmError::TypeError(
                      "Proxy set trap returned true for a non-writable, non-configurable data property with a different value",
                    ));
                  }
                }
                PropertyKind::Accessor {
                  set: Value::Undefined,
                  ..
                } => {
                  return Err(VmError::TypeError(
                    "Proxy set trap returned true for a non-configurable accessor property with an undefined setter",
                  ));
                }
                _ => {}
              }
            }
          }
        }

        return Ok(true);
      }

      // --- Ordinary / exotic objects ---
      if scope.heap().object_is_module_namespace(current)? {
        if matches!(key, PropertyKey::String(_)) {
          return Ok(false);
        }
      }

      // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys use
      // TypedArray `[[Set]]` semantics (and do not consult the prototype chain).
      if scope.heap().is_typed_array_object(current) {
        if let PropertyKey::String(s) = key {
          if let Some(numeric_index) = scope.heap_mut().canonical_numeric_index_string(s)? {
            // `SameValue(O, Receiver)` check: element writes only happen when `receiver` is the typed
            // array object itself.
            if let Value::Object(receiver_obj) = receiver {
              if receiver_obj == current {
                // `TypedArraySetElement` always performs `ToNumber(value)` before checking
                // `IsValidIntegerIndex`, even when the numeric index is invalid (e.g. `\"-1\"`,
                // `\"1.5\"`, `\"NaN\"`, `\"Infinity\"`, `\"-0\"`). This matters because
                // `ToNumber(Symbol)` / `ToNumber(BigInt)` throw.
                //
                // Spec: https://tc39.es/ecma262/#sec-typedarraysetelement
                let index = if numeric_index.is_finite()
                  && numeric_index.fract() == 0.0
                  && !(numeric_index == 0.0 && numeric_index.is_sign_negative())
                  && numeric_index >= 0.0
                {
                  let index = numeric_index as u128;
                  if index <= usize::MAX as u128 {
                    Some(index as usize)
                  } else {
                    None
                  }
                } else {
                  None
                };

                match index {
                  Some(index) => {
                    // `TypedArraySetElement`: no-op for out-of-bounds indices or detached buffers.
                    let _ = scope
                      .heap_mut()
                      .typed_array_set_element_value(current, index, value)?;
                  }
                  None => {
                    // Invalid numeric index: still `ToNumber(value)` per spec, but no element write.
                    let _ = scope.heap_mut().to_number(value)?;
                  }
                }
                return Ok(true);
              }
            }

            // If `receiver` is not the typed array itself, only fall back to ordinary `[[Set]]` when
            // the numeric index is a valid integer index.
            //
            // Spec: https://tc39.es/ecma262/#sec-typedarray-set
            if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
              return Ok(true);
            }
            if numeric_index == 0.0 && numeric_index.is_sign_negative() {
              return Ok(true);
            }
            if numeric_index < 0.0 {
              return Ok(true);
            }
            let len = scope.heap().typed_array_length(current)?;
            if numeric_index >= len as f64 {
              return Ok(true);
            }
            // Valid integer index: continue with ordinary `[[Set]]` semantics below.
          }
        }
      }

      // Own property check.
      let mut desc = scope.ordinary_get_own_property_with_tick(current, key, || vm.tick())?;
      if desc.is_none() {
        // Delegate to the prototype's `[[Set]]` internal method (Proxy-aware).
        if let Some(proto) = scope.heap().object_prototype(current)? {
          current = proto;
          if visited.try_reserve(1).is_err() {
            return Err(VmError::OutOfMemory);
          }
          if !visited.insert(current) {
            return Err(VmError::PrototypeCycle);
          }
          // Root prototypes so a `__proto__` mutation during user code (Proxy traps) cannot
          // invalidate the iterator's internal object handle before we reach it.
          scope.push_root(Value::Object(current))?;
          continue;
        }

        // No prototype: treat as a new writable data property.
        desc = Some(PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Undefined,
            writable: true,
          },
        });
      }

      let Some(desc) = desc else {
        return Err(VmError::InvariantViolation(
          "ordinary_set: internal error: missing property descriptor",
        ));
      };

      return match desc.kind {
        PropertyKind::Data { writable, .. } => {
          if !writable {
            return Ok(false);
          }
          let Value::Object(receiver_obj) = receiver else {
            return Ok(false);
          };

          let existing_desc =
            scope.ordinary_get_own_property_with_tick(receiver_obj, key, || vm.tick())?;
          if let Some(existing_desc) = existing_desc {
            if existing_desc.is_accessor_descriptor() {
              return Ok(false);
            }
            let receiver_writable = match existing_desc.kind {
              PropertyKind::Data { writable, .. } => writable,
              PropertyKind::Accessor { .. } => return Ok(false),
            };
            if !receiver_writable {
              return Ok(false);
            }

            return scope.define_own_property_with_tick(
              receiver_obj,
              key,
              PropertyDescriptorPatch {
                value: Some(value),
                ..Default::default()
              },
              || vm.tick(),
            );
          }

          scope.create_data_property(receiver_obj, key, value)
        }
        PropertyKind::Accessor { set, .. } => {
          if matches!(set, Value::Undefined) {
            return Ok(false);
          }
          if !scope.heap().is_callable(set)? {
            return Err(VmError::TypeError("accessor setter is not callable"));
          }
          // See `ordinary_get`: prefer `Vm::call` so any active host hook override is honored.
          let mut dummy_host = ();
          let _ = vm.call(&mut dummy_host, &mut scope, set, receiver, &[value])?;
          Ok(true)
        }
      };
    }
  }

  /// ECMAScript `[[Set]]` for ordinary objects, using an explicit embedder host context and host
  /// hook implementation.
  pub fn ordinary_set_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
    receiver: Value,
  ) -> Result<bool, VmError> {
    // Like `ordinary_get_with_host_and_hooks`, this is occasionally called with Proxy receivers.
    // Delegate to the Proxy-aware `[[Set]]` implementation to avoid `InvalidHandle` and ensure
    // traps are observed.
    if self.heap().is_proxy_object(obj) {
      return self.set_with_host_and_hooks(vm, host, hooks, obj, key, value, receiver);
    }

    // Root inputs together so GC can't collect `key`/`value`/`receiver` while growing the root
    // stack.
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
      value,
      receiver,
    ];
    self.push_roots(&roots)?;

    if self.heap().object_is_module_namespace(obj)? {
      if matches!(key, PropertyKey::String(_)) {
        return Ok(false);
      }
    }

    // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys use
    // TypedArray `[[Set]]` semantics.
    if self.heap().is_typed_array_object(obj) {
      if let PropertyKey::String(s) = key {
        if let Some(numeric_index) = self.heap_mut().canonical_numeric_index_string(s)? {
          // `SameValue(O, Receiver)` check: element writes only happen when `receiver` is the typed
          // array object itself.
          if let Value::Object(receiver_obj) = receiver {
            if receiver_obj == obj {
              // `TypedArraySetElement` always performs `ToNumber(value)` before checking
              // `IsValidIntegerIndex`, even when the numeric index is invalid (e.g. `"-1"`,
              // `"1.5"`, `"NaN"`, `"Infinity"`, `"-0"`). This matters because `ToNumber(Symbol)`
              // / `ToNumber(BigInt)` throw.
              //
              // Spec: https://tc39.es/ecma262/#sec-typedarraysetelement
              let index = if numeric_index.is_finite()
                && numeric_index.fract() == 0.0
                && !(numeric_index == 0.0 && numeric_index.is_sign_negative())
                && numeric_index >= 0.0
              {
                let index = numeric_index as u128;
                if index <= usize::MAX as u128 {
                  Some(index as usize)
                } else {
                  None
                }
              } else {
                None
              };

              match index {
                Some(index) => {
                  // `TypedArraySetElement`: no-op for out-of-bounds indices or detached buffers.
                  let _ = self
                    .heap_mut()
                    .typed_array_set_element_value(obj, index, value)?;
                }
                None => {
                  // Invalid numeric index: still `ToNumber(value)` per spec, but no element write.
                  let _ = self.heap_mut().to_number(value)?;
                }
              }
              return Ok(true);
            }
          }

          // If `receiver` is not the typed array itself, only fall back to ordinary `[[Set]]` when
          // the numeric index is a valid integer index.
          //
          // Spec: https://tc39.es/ecma262/#sec-typedarray-set
          if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
            return Ok(true);
          }
          if numeric_index == 0.0 && numeric_index.is_sign_negative() {
            return Ok(true);
          }
          if numeric_index < 0.0 {
            return Ok(true);
          }
          let len = self.heap().typed_array_length(obj)?;
          if numeric_index >= len as f64 {
            return Ok(true);
          }
           // Valid integer index: continue with ordinary `[[Set]]` semantics below.
         }
      }
    }

    // Annex B `caller` / `arguments`: for ordinary non-strict functions, legacy `.caller`/`.arguments`
    // must not throw even though %Function.prototype% defines poison-pill accessors.
    if self.heap().property_key_is_caller(&key) || self.heap().property_key_is_arguments(&key) {
      if let Ok(func) = self.heap().get_function(obj) {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let proto = self.heap().object_prototype(obj)?;
        let is_ordinary_function = proto == Some(intr.function_prototype());
        let is_restricted = func.is_strict
          || func.bound_target.is_some()
          || func.this_mode == ThisMode::Lexical
          || !is_ordinary_function;
        if is_restricted {
          return Err(VmError::TypeError("Restricted function property"));
        }
        return Ok(true);
      }
    }

    // Host hook for "exotic" property setters (e.g. DOM named properties) runs before ordinary
    // `[[Set]]` processing so it can override prototype-chain properties like `constructor`.
    if let Some(result) = hooks.host_exotic_set(self, obj, key, value, receiver)? {
      return Ok(result);
    }

    // OrdinarySet (ECMA-262): we must not scan the prototype chain as ordinary objects, since the
    // prototype can contain Proxy objects. If the property is not an own property, delegate to
    // `proto.[[Set]]` so Proxy `set` traps are observed.
    let mut desc = self.ordinary_get_own_property_with_tick(obj, key, || vm.tick())?;
    if desc.is_none() {
      if let Some(proto) = self.heap().object_prototype(obj)? {
        return self.set_with_host_and_hooks(vm, host, hooks, proto, key, value, receiver);
      }
      // No prototype: treat as a new writable data property.
      desc = Some(PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Undefined,
          writable: true,
        },
      });
    }

    let Some(desc) = desc else {
      return Err(VmError::InvariantViolation(
        "ordinary_set: internal error: missing property descriptor",
      ));
    };

    match desc.kind {
      PropertyKind::Data { writable, .. } => {
        if !writable {
          return Ok(false);
        }
        let Value::Object(receiver_obj) = receiver else {
          return Ok(false);
        };

        // `receiver` can be a Proxy object; use internal-method dispatch for the receiver's
        // `[[GetOwnProperty]]` / `[[DefineOwnProperty]]` operations so traps are observed and we do
        // not attempt to treat proxies as ordinary objects.
        let existing_desc =
          self.get_own_property_with_host_and_hooks(vm, host, hooks, receiver_obj, key)?;
        if let Some(existing_desc) = existing_desc {
          if existing_desc.is_accessor_descriptor() {
            return Ok(false);
          }
          let receiver_writable = match existing_desc.kind {
            PropertyKind::Data { writable, .. } => writable,
            PropertyKind::Accessor { .. } => return Ok(false),
          };
          if !receiver_writable {
            return Ok(false);
          }

          let mut tick0 = |vm: &mut Vm| vm.tick();
          return self.define_own_property_with_host_and_hooks_with_tick(
            vm,
            host,
            hooks,
            receiver_obj,
            key,
            PropertyDescriptorPatch {
              value: Some(value),
              ..Default::default()
            },
            &mut tick0,
          );
        }

        // `CreateDataProperty(receiver, key, value)`
        let mut tick0 = |vm: &mut Vm| vm.tick();
        self.define_own_property_with_host_and_hooks_with_tick(
          vm,
          host,
          hooks,
          receiver_obj,
          key,
          PropertyDescriptorPatch {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
          },
          &mut tick0,
        )
      }
      PropertyKind::Accessor { set, .. } => {
        if matches!(set, Value::Undefined) {
          return Ok(false);
        }
        if !self.heap().is_callable(set)? {
          return Err(VmError::TypeError("accessor setter is not callable"));
        }
        let _ = vm.call_with_host_and_hooks(host, self, hooks, set, receiver, &[value])?;
        Ok(true)
      }
    }
  }

  /// ECMAScript `[[Delete]]` for ordinary objects.
  pub fn ordinary_delete(&mut self, obj: GcObject, key: PropertyKey) -> Result<bool, VmError> {
    // Root inputs together so GC cannot collect `key` while growing the root stack (important when
    // deleting a *missing* property).
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
    ];
    self.push_roots(&roots)?;

    // Integer-indexed exotic behaviour for typed arrays.
    //
    // Spec: https://tc39.es/ecma262/#sec-integer-indexed-exotic-objects-delete-p
    //
    // For numeric indices:
    // - if the index is *invalid* (includes detached/out-of-bounds), `delete` succeeds (`true`)
    // - otherwise (valid index), typed array elements are non-configurable and cannot be deleted (`false`)
    if self.string_object_in_range_index(obj, &key)?.is_some() {
      return Ok(false);
    }

    // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys are
    // non-deletable when they refer to a valid integer index.
    if self.heap().is_typed_array_object(obj) {
      if let PropertyKey::String(s) = key {
        if let Some(numeric_index) = self.heap_mut().canonical_numeric_index_string(s)? {
          // `IsValidIntegerIndex` includes a detached/out-of-bounds check.
          if self.heap().typed_array_is_out_of_bounds(obj)? {
            return Ok(true);
          }
          // `IsValidIntegerIndex`
          if numeric_index.is_finite()
            && numeric_index.fract() == 0.0
            && !(numeric_index == 0.0 && numeric_index.is_sign_negative())
            && numeric_index >= 0.0
          {
            let len = self.heap().typed_array_length(obj)?;
            if numeric_index < len as f64 {
              return Ok(false);
            }
          }
          // Invalid integer index: treated as non-existent.
          return Ok(true);
        }
      }
    }
    self.heap_mut().ordinary_delete(obj, key)
  }

  /// ECMAScript `[[Delete]]` for ordinary objects, using an explicit embedder host context and host
  /// hook implementation.
  pub fn ordinary_delete_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<bool, VmError> {
    // Built-ins sometimes call `ordinary_delete_with_host_and_hooks` on Proxy receivers. Dispatch
    // to the Proxy-aware `[[Delete]]` implementation so the `deleteProperty` trap is observed.
    if self.heap().is_proxy_object(obj) {
      return self.delete_with_host_and_hooks(vm, host, hooks, obj, key);
    }

    // Root inputs together so GC cannot collect `key` while growing the root stack (important when
    // deleting a missing property).
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
    ];
    self.push_roots(&roots)?;

    if self.heap().object_is_module_namespace(obj)? {
      match key {
        PropertyKey::Symbol(_) => {
          // Symbols use ordinary behavior (including non-configurable Symbol.toStringTag).
        }
        PropertyKey::String(s) => {
          return Ok(self.heap().module_namespace_export(obj, s)?.is_none());
        }
      }
    }

    if let Some(result) = hooks.host_exotic_delete(self, obj, key)? {
      return Ok(result);
    }

    // Integer-indexed exotic behaviour for typed arrays.
    //
    // See `ordinary_delete` for details.
    if self.string_object_in_range_index(obj, &key)?.is_some() {
      return Ok(false);
    }

    // Integer-indexed exotic objects (typed arrays): canonical numeric index string keys are
    // non-deletable when they refer to a valid integer index.
    if self.heap().is_typed_array_object(obj) {
      if let PropertyKey::String(s) = key {
        if let Some(numeric_index) = self.heap_mut().canonical_numeric_index_string(s)? {
          if self.heap().typed_array_is_out_of_bounds(obj)? {
            return Ok(true);
          }
          // `IsValidIntegerIndex`
          if numeric_index.is_finite()
            && numeric_index.fract() == 0.0
            && !(numeric_index == 0.0 && numeric_index.is_sign_negative())
            && numeric_index >= 0.0
          {
            let len = self.heap().typed_array_length(obj)?;
            if numeric_index < len as f64 {
              return Ok(false);
            }
          }
          // Invalid integer index: treated as non-existent.
          return Ok(true);
        }
      }
    }

    self.heap_mut().ordinary_delete(obj, key)
  }

  /// ECMAScript `[[OwnPropertyKeys]]` internal method dispatch.
  ///
  /// This is a spec-shaped wrapper around `OrdinaryOwnPropertyKeys` that routes Proxy objects
  /// through the `ownKeys` trap (when present).
  pub fn object_own_property_keys_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<Vec<PropertyKey>, VmError> {
    // Follow Proxy chains iteratively to avoid recursion.
    let mut current = obj;
    let mut steps = 0usize;
    loop {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);
 
      let Some(proxy) = self.heap().get_proxy_data(current)? else {
        // Important: use `self` here (rather than a temporary scope) so any newly-allocated index
        // key strings for String objects / typed arrays are rooted in the caller scope and remain
        // valid across GC while the returned key list is in use.
        return self.ordinary_own_property_keys_with_tick(current, || vm.tick());
      };
 
      vm.tick()?;
 
      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'ownKeys' on a proxy that has been revoked",
        ));
      };
 
      // Root proxy/target/handler for the duration of trap lookup + invocation.
      let mut trap_scope = self.reborrow();
      trap_scope.push_roots(&[
        Value::Object(current),
        Value::Object(target),
        Value::Object(handler),
      ])?;
 
      let trap_key_s = trap_scope.alloc_string("ownKeys")?;
      trap_scope.push_root(Value::String(trap_key_s))?;
      let trap_key = PropertyKey::from_string(trap_key_s);
 
      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut trap_scope,
        hooks,
        Value::Object(handler),
        trap_key,
      )?;
 
      // If the trap is undefined, forward to the target's `[[OwnPropertyKeys]]`.
      let Some(trap) = trap else {
        current = target;
        continue;
      };
 
      // Call the trap with `(target)`.
      let trap_args = [Value::Object(target)];
      let trap_result = vm.call_with_host_and_hooks(
        host,
        &mut trap_scope,
        hooks,
        trap,
        Value::Object(handler),
        &trap_args,
      )?;
 
      let Value::Object(trap_result_obj) = trap_result else {
        return Err(VmError::TypeError("Proxy ownKeys trap returned non-object"));
      };
      trap_scope.push_root(Value::Object(trap_result_obj))?;
 
      // Convert the trap result into a list of keys.
      //
      // Spec: `CreateListFromArrayLike(trapResult, « String, Symbol »)`.
      let values = crate::spec_ops::create_list_from_array_like_with_host_and_hooks(
        vm,
        &mut trap_scope,
        host,
        hooks,
        trap_result_obj,
      )?;
 
      // Validate and materialize the returned keys *before* dropping `trap_scope`, so keys remain
      // reachable from the rooted trap result array.
      let mut out: Vec<PropertyKey> = Vec::new();
      out
        .try_reserve_exact(values.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for (i, v) in values.iter().copied().enumerate() {
        if i != 0 && i % 1024 == 0 {
          vm.tick()?;
        }
        match v {
          Value::String(s) => out.push(PropertyKey::from_string(s)),
          Value::Symbol(s) => out.push(PropertyKey::from_symbol(s)),
          _ => {
            return Err(VmError::TypeError(
              "Proxy ownKeys trap returned non-string/non-symbol",
            ))
          }
        }
      }

      validate_proxy_own_keys_trap_result(vm, &mut trap_scope, host, hooks, target, &out)?;

      // Root the returned key values in the caller scope so they remain valid across GC while the
      // returned `Vec<PropertyKey>` is in use.
      //
      // This is necessary for Proxy `ownKeys` trap results: the returned keys are not necessarily
      // reachable from any other heap object once the trap returns (unlike ordinary object keys,
      // which are reachable from the object itself).
      drop(trap_scope);
      const KEY_ROOT_CHUNK: usize = 1024;
      let mut start = 0usize;
      while start < values.len() {
        let end = values.len().min(start.saturating_add(KEY_ROOT_CHUNK));
        let chunk = &values[start..end];
        let remaining = &values[end..];
        self.push_roots_with_extra_roots(chunk, remaining, &[])?;
        start = end;
        if start < values.len() {
          vm.tick()?;
        }
      }
 
      return Ok(out);
    }
  }

  /// ECMAScript `[[OwnPropertyKeys]]` for ordinary objects.
  pub fn ordinary_own_property_keys_with_tick(
    &mut self,
    obj: GcObject,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Vec<PropertyKey>, VmError> {
    if self.heap().object_is_module_namespace(obj)? {
      let exports = self.heap().module_namespace_exports(obj)?;
      let own_keys = self
        .heap()
        .ordinary_own_property_keys_with_tick(obj, &mut tick)?;
      let mut symbol_keys: Vec<PropertyKey> = Vec::new();
      for key in own_keys {
        if matches!(key, PropertyKey::Symbol(_)) {
          // `Vec::push` can abort the process on allocator OOM; reserve fallibly first.
          symbol_keys
            .try_reserve(1)
            .map_err(|_| VmError::OutOfMemory)?;
          symbol_keys.push(key);
        }
      }

      let out_len = exports
        .len()
        .checked_add(symbol_keys.len())
        .ok_or(VmError::OutOfMemory)?;
      let mut out: Vec<PropertyKey> = Vec::new();
      out.try_reserve_exact(out_len).map_err(|_| VmError::OutOfMemory)?;
      for export in exports {
        out.push(PropertyKey::String(export.name));
      }
      out.extend(symbol_keys);
      return Ok(out);
    }

    if let Some(string_data) = self.string_object_data_with_tick(obj, &mut tick)? {
      self.push_roots(&[Value::Object(obj), Value::String(string_data)])?;

      let len = self.heap().get_string(string_data)?.len_code_units();
      let own_keys = self
        .heap()
        .ordinary_own_property_keys_with_tick(obj, &mut tick)?;

      let out_len = len
        .checked_add(own_keys.len())
        .ok_or(VmError::OutOfMemory)?;
      let mut out: Vec<PropertyKey> = Vec::new();
      out
        .try_reserve_exact(out_len)
        .map_err(|_| VmError::OutOfMemory)?;

      const TICK_EVERY: usize = 1024;
      for i in 0..len {
        if i % TICK_EVERY == 0 {
          tick()?;
        }
        let i_u32 = u32::try_from(i).map_err(|_| VmError::OutOfMemory)?;
        let key_s = self.alloc_u32_index_string(i_u32)?;
        self.push_root(Value::String(key_s))?;
        out.push(PropertyKey::from_string(key_s));
      }

      for (i, key) in own_keys.into_iter().enumerate() {
        if i % TICK_EVERY == 0 {
          tick()?;
        }
        if let Some(idx) = self.heap().array_index(&key) {
          if idx as usize >= len {
            out.push(key);
          }
        } else {
          out.push(key);
        }
      }

      return Ok(out);
    }

    // Array exotic objects store many array-indexed properties in a dense table (see `heap.rs`).
    // `[[OwnPropertyKeys]]` must include those indices even though they are not present in the
    // ordinary property table.
    if self.heap().object_is_array(obj)? {
      // Root `obj` so any allocations for generated index key strings can't collect it.
      self.push_root(Value::Object(obj))?;

      // Keys stored in the ordinary property table (includes `"length"` and any sparse indices that
      // exceed the fast-elements limit).
      let own_keys = self
        .heap()
        .ordinary_own_property_keys_with_tick(obj, &mut tick)?;

      let mut index_keys: Vec<(u32, PropertyKey)> = Vec::new();
      let mut other_keys: Vec<PropertyKey> = Vec::new();

      // Split property-table keys into indices and non-indices.
      for (i, key) in own_keys.into_iter().enumerate() {
        if i % 1024 == 0 {
          tick()?;
        }
        if let Some(idx) = self.heap().array_index(&key) {
          index_keys.push((idx, key));
        } else {
          other_keys.push(key);
        }
      }

      // Add index keys from the fast element table.
      let fast_len = self.heap().array_fast_elements_len(obj)?;
      for i in 0..fast_len {
        if i % 1024 == 0 {
          tick()?;
        }
        let idx_u32 = u32::try_from(i).map_err(|_| VmError::OutOfMemory)?;
        if self
          .heap()
          .array_fast_own_element_descriptor(obj, idx_u32)?
          .is_none()
        {
          continue;
        }
        let key_s = self.alloc_u32_index_string(idx_u32)?;
        self.push_root(Value::String(key_s))?;
        index_keys.push((idx_u32, PropertyKey::from_string(key_s)));
      }

      if !index_keys.is_empty() {
        tick()?;
      }
      index_keys.sort_by_key(|(idx, _)| *idx);
      if !index_keys.is_empty() {
        tick()?;
      }

      let out_len = index_keys
        .len()
        .checked_add(other_keys.len())
        .ok_or(VmError::OutOfMemory)?;
      let mut out: Vec<PropertyKey> = Vec::new();
      out
        .try_reserve_exact(out_len)
        .map_err(|_| VmError::OutOfMemory)?;
      for (i, (_, key)) in index_keys.into_iter().enumerate() {
        if i % 1024 == 0 {
          tick()?;
        }
        out.push(key);
      }
      out.extend(other_keys);
      return Ok(out);
    }

    if self.heap().is_typed_array_object(obj) {
      self.push_root(Value::Object(obj))?;

      let len = self.heap().typed_array_length(obj)?;
      let own_keys = self
        .heap()
        .ordinary_own_property_keys_with_tick(obj, &mut tick)?;

      let out_len = len
        .checked_add(own_keys.len())
        .ok_or(VmError::OutOfMemory)?;
      let mut out: Vec<PropertyKey> = Vec::new();
      out
        .try_reserve_exact(out_len)
        .map_err(|_| VmError::OutOfMemory)?;

      const TICK_EVERY: usize = 1024;
      for i in 0..len {
        if i % TICK_EVERY == 0 {
          tick()?;
        }
        let i_u32 = u32::try_from(i).map_err(|_| VmError::OutOfMemory)?;
        let key_s = self.alloc_u32_index_string(i_u32)?;
        self.push_root(Value::String(key_s))?;
        out.push(PropertyKey::from_string(key_s));
      }

      for (i, key) in own_keys.into_iter().enumerate() {
        if i % TICK_EVERY == 0 {
          tick()?;
        }
        if let Some(idx) = self.heap().array_index(&key) {
          if idx as usize >= len {
            out.push(key);
          }
        } else {
          out.push(key);
        }
      }

      return Ok(out);
    }

    self.heap().ordinary_own_property_keys_with_tick(obj, tick)
  }

  pub fn ordinary_own_property_keys(&mut self, obj: GcObject) -> Result<Vec<PropertyKey>, VmError> {
    self.ordinary_own_property_keys_with_tick(obj, || Ok(()))
  }

  pub fn create_data_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
  ) -> Result<bool, VmError> {
    // Root inputs for the duration of the operation. This is particularly important for failure
    // cases (e.g. non-extensible objects) where `key`/`value` are not reachable from any existing
    // heap object.
    let mut scope = self.reborrow();
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
      value,
    ];
    scope.push_roots(&roots)?;

    scope.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        value: Some(value),
        writable: Some(true),
        enumerable: Some(true),
        configurable: Some(true),
        ..Default::default()
      },
    )
  }

  pub fn create_data_property_or_throw(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
  ) -> Result<(), VmError> {
    let ok = self.create_data_property(obj, key, value)?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("CreateDataProperty rejected"))
    }
  }

  /// ECMAScript `DeletePropertyOrThrow`.
  ///
  /// This is a convenience wrapper around [`Scope::ordinary_delete`]. If the deletion is rejected
  /// (`false`), this returns a `TypeError`.
  pub fn delete_property_or_throw(&mut self, obj: GcObject, key: PropertyKey) -> Result<(), VmError> {
    // Root `obj`/`key` for the duration of the operation. Deleting a *missing* property should not
    // require the caller to pre-root `key` even when GC is triggered while growing the root stack.
    let mut scope = self.reborrow();
    let roots = [
      Value::Object(obj),
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
    ];
    scope.push_roots(&roots)?;

    let ok = scope.ordinary_delete(obj, key)?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("DeletePropertyOrThrow rejected"))
    }
  }

  fn array_define_own_property_with_tick(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    desc.validate()?;

    // Root all inputs that might be written into the heap before any allocation/GC.
    //
    // Important: root them *together* so if growing the root stack triggers a GC, we do not collect
    // the property key / descriptor values before they've been pushed.
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(obj);
    root_count += 1;
    roots[root_count] = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    root_count += 1;
    if let Some(v) = desc.value {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.get {
      roots[root_count] = v;
      root_count += 1;
    }
    if let Some(v) = desc.set {
      roots[root_count] = v;
      root_count += 1;
    }
    self.push_roots(&roots[..root_count])?;

    if self.heap().property_key_is_length(&key) {
      let length_key = self.heap().array_length_key(obj)?;
      return self.array_set_length_with_tick(obj, length_key, desc, tick);
    }

    if let Some(index) = self.heap().array_index(&key) {
      let old_len = self.heap().array_length(obj)?;
      if index >= old_len && !self.heap().array_length_writable(obj)? {
        return Ok(false);
      }

      let succeeded = self.ordinary_define_own_property(obj, key, desc)?;
      if !succeeded {
        return Ok(false);
      }

      if index >= old_len {
        let new_len = index
          .checked_add(1)
          .ok_or(VmError::InvariantViolation("array index overflow"))?;
        self.heap_mut().array_set_length(obj, new_len)?;
      }

      return Ok(true);
    }

    self.ordinary_define_own_property(obj, key, desc)
  }

  fn array_set_length_with_tick(
    &mut self,
    obj: GcObject,
    length_key: PropertyKey,
    desc: PropertyDescriptorPatch,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    // If `Desc` does not specify a new length value, this is just a property definition on the
    // existing `length` data property (typically toggling writability).
    let Some(value) = desc.value else {
      return self.ordinary_define_own_property(obj, length_key, desc);
    };

    let new_len = array_length_from_value(self, value)?;

    let old_len = self.heap().array_length(obj)?;

    // Extending `length` is just an ordinary property definition.
    if new_len >= old_len {
      let mut new_desc = desc;
      new_desc.value = Some(Value::Number(new_len as f64));
      return self.ordinary_define_own_property(obj, length_key, new_desc);
    }

    // Shrinking: reject if `length` is not writable.
    if !self.heap().array_length_writable(obj)? {
      return Ok(false);
    }

    // If the caller is requesting `writable: false`, the spec requires performing deletions while
    // `length` is still writable so we can restore `length` on failure.
    let mut new_writable = true;
    let mut new_len_desc = desc;
    if matches!(new_len_desc.writable, Some(false)) {
      new_writable = false;
      new_len_desc.writable = Some(true);
    }
    new_len_desc.value = Some(Value::Number(new_len as f64));

    let succeeded = self.ordinary_define_own_property(obj, length_key, new_len_desc)?;
    if !succeeded {
      return Ok(false);
    }

    // Fast path: delete array index properties stored in the array's dense elements table without
    // enumerating all keys and allocating per-index key strings.
    //
    // This matters for test262's `regExpUtils.js::buildString`, which repeatedly sets
    // `codePoints.length = 0` on a temporary array with ~10k elements.
    //
    // We only take this path when the ordinary property table does not itself contain any
    // "small" array index keys, since those would need to be interleaved with dense elements in
    // descending numeric order.
    let property_table_keys = self
      .heap()
      .ordinary_own_property_keys_with_tick(obj, &mut *tick)?;
    let has_small_index_in_properties = property_table_keys.iter().any(|k| {
      self
        .heap()
        .array_index(k)
        .is_some_and(|idx| idx <= crate::heap::MAX_FAST_ARRAY_INDEX)
    });

    if has_small_index_in_properties {
      // Fallback: generic spec-shaped deletion through `[[OwnPropertyKeys]]`.
      //
      // This is slower because it needs to allocate index key strings, but should be rare.
      let keys = self.ordinary_own_property_keys_with_tick(obj, &mut *tick)?;
      for (i, key) in keys.into_iter().rev().enumerate() {
        if i % 1024 == 0 {
          tick()?;
        }
        let Some(index) = self.heap().array_index(&key) else {
          continue;
        };
        if index < new_len {
          break;
        }
        if index >= old_len {
          continue;
        }

        let delete_ok = self.ordinary_delete(obj, key)?;
        if delete_ok {
          continue;
        }

        // Failed to delete a non-configurable element: restore `length` to `index + 1` and (if
        // requested) make it non-writable.
        let restore_len = index
          .checked_add(1)
          .ok_or(VmError::InvariantViolation("array index overflow"))?;

        let ok = self.ordinary_define_own_property(
          obj,
          length_key,
          PropertyDescriptorPatch {
            value: Some(Value::Number(restore_len as f64)),
            ..Default::default()
          },
        )?;
        if !ok {
          return Err(VmError::InvariantViolation(
            "array length restoration via OrdinaryDefineOwnProperty failed",
          ));
        }
        if !new_writable {
          self.heap_mut().array_set_length_writable(obj, false)?;
        }
        return Ok(false);
      }
    } else {
      // 1) Delete sparse index properties stored in the ordinary property table (these are either
      //    > MAX_FAST_ARRAY_INDEX or otherwise not represented in the dense elements table).
      for (i, key) in property_table_keys.into_iter().rev().enumerate() {
        if i % 1024 == 0 {
          tick()?;
        }
        let Some(index) = self.heap().array_index(&key) else {
          continue;
        };
        if index < new_len {
          break;
        }
        if index >= old_len {
          continue;
        }

        let delete_ok = self.ordinary_delete(obj, key)?;
        if delete_ok {
          continue;
        }

        let restore_len = index
          .checked_add(1)
          .ok_or(VmError::InvariantViolation("array index overflow"))?;
        let ok = self.ordinary_define_own_property(
          obj,
          length_key,
          PropertyDescriptorPatch {
            value: Some(Value::Number(restore_len as f64)),
            ..Default::default()
          },
        )?;
        if !ok {
          return Err(VmError::InvariantViolation(
            "array length restoration via OrdinaryDefineOwnProperty failed",
          ));
        }
        if !new_writable {
          self.heap_mut().array_set_length_writable(obj, false)?;
        }
        return Ok(false);
      }

      // 2) Delete dense-element indices in descending order.
      let mut fail_index: Option<u32> = None;
      {
        let elements = self.heap_mut().array_fast_elements_mut(obj)?;
        let end = old_len.min(elements.len() as u32);

        // Iterate from `end - 1` down to `new_len`.
        let mut idx = end;
        let mut steps = 0usize;
        while idx > new_len {
          idx -= 1;
          if steps % 1024 == 0 {
            tick()?;
          }
          steps = steps.saturating_add(1);

          let Some(slot) = elements.get_mut(idx as usize) else {
            continue;
          };
          let Some(existing) = *slot else {
            continue;
          };
          if !existing.configurable {
            fail_index = Some(idx);
            break;
          }
          // Deleting an element does not affect `length`.
          *slot = None;
        }
      }

      if let Some(index) = fail_index {
        let restore_len = index
          .checked_add(1)
          .ok_or(VmError::InvariantViolation("array index overflow"))?;
        let ok = self.ordinary_define_own_property(
          obj,
          length_key,
          PropertyDescriptorPatch {
            value: Some(Value::Number(restore_len as f64)),
            ..Default::default()
          },
        )?;
        if !ok {
          return Err(VmError::InvariantViolation(
            "array length restoration via OrdinaryDefineOwnProperty failed",
          ));
        }
        if !new_writable {
          self.heap_mut().array_set_length_writable(obj, false)?;
        }
        return Ok(false);
      }
    }

    if !new_writable {
      self.heap_mut().array_set_length_writable(obj, false)?;
    }

    Ok(true)
  }

  #[allow(dead_code)]
  fn string_object_data(&self, obj: GcObject) -> Result<Option<GcString>, VmError> {
    let mut tick = || Ok(());
    self.string_object_data_with_tick(obj, &mut tick)
  }

  fn string_object_data_with_tick(
    &self,
    obj: GcObject,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<GcString>, VmError> {
    let Some(marker_sym) = self.heap().internal_string_data_symbol() else {
      return Ok(None);
    };
    let marker_key = PropertyKey::from_symbol(marker_sym);
    let Some(desc) = self
      .heap()
      .object_get_own_property_with_tick(obj, &marker_key, &mut *tick)?
    else {
      return Ok(None);
    };
    match desc.kind {
      PropertyKind::Data {
        value: Value::String(s),
        ..
      } => Ok(Some(s)),
      _ => Ok(None),
    }
  }

  fn string_object_in_range_index(
    &self,
    obj: GcObject,
    key: &PropertyKey,
  ) -> Result<Option<(GcString, u32)>, VmError> {
    let mut tick = || Ok(());
    self.string_object_in_range_index_with_tick(obj, key, &mut tick)
  }

  fn string_object_in_range_index_with_tick(
    &self,
    obj: GcObject,
    key: &PropertyKey,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<(GcString, u32)>, VmError> {
    // Only string-index properties are special; for other keys avoid scanning for string data.
    let Some(index) = self.heap().array_index(key) else {
      return Ok(None);
    };
    let Some(string_data) = self.string_object_data_with_tick(obj, tick)? else {
      return Ok(None);
    };
    let len = self.heap().get_string(string_data)?.len_code_units();
    if index as usize >= len {
      return Ok(None);
    }
    Ok(Some((string_data, index)))
  }

  #[allow(dead_code)]
  fn string_object_get_index_value(
    &mut self,
    obj: GcObject,
    key: &PropertyKey,
  ) -> Result<Option<Value>, VmError> {
    self.string_object_get_index_value_with_tick(obj, key, || Ok(()))
  }

  fn string_object_get_index_value_with_tick(
    &mut self,
    obj: GcObject,
    key: &PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<Value>, VmError> {
    let Some((string_data, index)) =
      self.string_object_in_range_index_with_tick(obj, key, &mut tick)?
    else {
      return Ok(None);
    };
    let idx = index as usize;
    let unit = *self
      .heap()
      .get_string(string_data)?
      .as_code_units()
      .get(idx)
      .ok_or(VmError::InvariantViolation(
        "string_object_get_index_value: index out of bounds",
      ))?;

    let mut alloc_scope = self.reborrow();
    alloc_scope.push_roots(&[Value::Object(obj), Value::String(string_data)])?;
    let s = alloc_scope.alloc_string_from_u16_vec(vec![unit])?;
    Ok(Some(Value::String(s)))
  }

  fn typed_array_get_index_value_with_tick(
    &mut self,
    obj: GcObject,
    key: &PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<Value>, VmError> {
    if !self.heap().is_typed_array_object(obj) {
      return Ok(None);
    }
    let PropertyKey::String(s) = key else {
      return Ok(None);
    };
    let Some(numeric_index) = self.heap().canonical_numeric_index_string(*s)? else {
      return Ok(None);
    };

    // `IsValidIntegerIndex`.
    if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
      return Ok(None);
    }
    if numeric_index == 0.0 && numeric_index.is_sign_negative() {
      // -0 is a canonical numeric index string but never a valid integer index.
      return Ok(None);
    }
    if numeric_index < 0.0 || numeric_index > usize::MAX as f64 {
      return Ok(None);
    }

    if self.heap().typed_array_is_out_of_bounds(obj)? {
      return Ok(None);
    }
    let index = numeric_index as usize;
    let len = self.heap().typed_array_length(obj)?;
    if index >= len {
      return Ok(None);
    }

    let kind = self.heap().typed_array_kind(obj)?;
    if kind.is_bigint() {
      let Some(bits) = self.heap().typed_array_get_element_u64_bits(obj, index)? else {
        return Ok(None);
      };
      let mut alloc_scope = self.reborrow();
      alloc_scope.push_roots(&[Value::Object(obj), Value::String(*s)])?;
      let bi = match kind {
        crate::heap::TypedArrayKind::BigInt64 => {
          alloc_scope.alloc_bigint_from_i128((bits as i64) as i128)?
        }
        crate::heap::TypedArrayKind::BigUint64 => alloc_scope.alloc_bigint_from_u128(bits as u128)?,
        _ => return Err(VmError::InvariantViolation("expected BigInt typed array kind")),
      };
      return Ok(Some(Value::BigInt(bi)));
    }

    // Numeric typed arrays.
    let value = self
      .heap()
      .typed_array_get_element_value(obj, index)?
      .ok_or(VmError::InvariantViolation(
        "typed_array_get_index_value_with_tick: missing in-bounds element value",
      ))?;
    tick()?;
    Ok(Some(value))
  }

  fn string_define_own_property_index(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    desc.validate()?;

    if self.heap().object_get_own_property(obj, &key)?.is_some() {
      return self.ordinary_define_own_property(obj, key, desc);
    }

    let Some((string_data, index)) = self.string_object_in_range_index(obj, &key)? else {
      return self.ordinary_define_own_property(obj, key, desc);
    };
    let idx = index as usize;
    let unit = *self
      .heap()
      .get_string(string_data)?
      .as_code_units()
      .get(idx)
      .ok_or(VmError::InvariantViolation(
        "string_define_own_property_index: index out of bounds",
      ))?;

    if desc.is_accessor_descriptor() {
      return Ok(false);
    }
    if matches!(desc.configurable, Some(true)) {
      return Ok(false);
    }
    if let Some(enumerable) = desc.enumerable {
      if !enumerable {
        return Ok(false);
      }
    }
    if matches!(desc.writable, Some(true)) {
      return Ok(false);
    }
    if let Some(value) = desc.value {
      let Value::String(s) = value else {
        return Ok(false);
      };
      let v_units = self.heap().get_string(s)?.as_code_units();
      if v_units.len() != 1 || v_units[0] != unit {
        return Ok(false);
      }
    }

    Ok(true)
  }

  fn typed_array_define_own_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    desc.validate()?;

    // TypedArray `[[DefineOwnProperty]]` semantics for integer-indexed keys.
    // https://tc39.es/ecma262/#sec-typedarray-defineownproperty
    let PropertyKey::String(s) = key else {
      return self.ordinary_define_own_property(obj, key, desc);
    };

    let Some(numeric_index) = self.heap().canonical_numeric_index_string(s)? else {
      return self.ordinary_define_own_property(obj, PropertyKey::String(s), desc);
    };

    // `IsValidIntegerIndex`
    if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
      return Ok(false);
    }
    if numeric_index == 0.0 && numeric_index.is_sign_negative() {
      // -0 is a canonical numeric index string but never a valid integer index.
      return Ok(false);
    }
    if numeric_index < 0.0 {
      return Ok(false);
    }
    if self.heap().typed_array_is_out_of_bounds(obj)? {
      return Ok(false);
    }

    let index = numeric_index as usize;
    let len = self.heap().typed_array_length(obj)?;
    if index >= len {
      return Ok(false);
    }

    // Descriptor invariant checks for typed array element properties.
    if matches!(desc.configurable, Some(false)) {
      return Ok(false);
    }
    if matches!(desc.enumerable, Some(false)) {
      return Ok(false);
    }
    if desc.is_accessor_descriptor() {
      return Ok(false);
    }
    if matches!(desc.writable, Some(false)) {
      return Ok(false);
    }

    if let Some(value) = desc.value {
      // `typed_array_set_element_value` performs the ToNumber conversion and element-type
      // conversion/clamping.
      let ok = self
        .heap_mut()
        .typed_array_set_element_value(obj, index, value)?;
      if !ok {
        return Ok(false);
      }
    }

    Ok(true)
  }

  fn module_namespace_define_own_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    desc.validate()?;

    match key {
      PropertyKey::Symbol(_) => return self.ordinary_define_own_property(obj, key, desc),
      PropertyKey::String(s) => {
        let Some(export) = self.heap().module_namespace_export(obj, s)? else {
          return Ok(false);
        };

        if matches!(desc.configurable, Some(true)) {
          return Ok(false);
        }
        if matches!(desc.enumerable, Some(false)) {
          return Ok(false);
        }
        if desc.is_accessor_descriptor() {
          return Ok(false);
        }
        if matches!(desc.writable, Some(false)) {
          return Ok(false);
        }

        if let Some(value) = desc.value {
          let current = match export.value {
            ModuleNamespaceExportValue::Namespace { namespace } => Value::Object(namespace),
            ModuleNamespaceExportValue::Binding { env, name } => {
              self.heap().env_get_binding_value_by_gc_string(env, name)?
            }
          };
          if !value.same_value(current, self.heap()) {
            return Ok(false);
          }
        }

        Ok(true)
      }
    }
  }
}

fn proxy_get_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  handler: GcObject,
  name: &'static str,
) -> Result<Option<Value>, VmError> {
  // Root `handler` so allocating the trap name string can't collect it.
  scope.push_root(Value::Object(handler))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let value = scope.get_with_host_and_hooks(vm, host, hooks, handler, key, Value::Object(handler))?;
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(None);
  }
  if !scope.heap().is_callable(value)? {
    return Err(VmError::TypeError("Proxy trap is not callable"));
  }
  Ok(Some(value))
}

fn proxy_get_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
) -> Result<Option<GcObject>, VmError> {
  let (target, handler) = proxy_target_and_handler(scope, proxy)?;

  // Root the proxy/target/handler across allocations and trap calls.
  let mut scope = scope.reborrow();
  scope.push_roots(&[
    Value::Object(proxy),
    Value::Object(target),
    Value::Object(handler),
  ])?;

  let Some(trap) = proxy_get_method(vm, &mut scope, host, hooks, handler, "getPrototypeOf")? else {
    return scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, target);
  };

  let result = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    trap,
    Value::Object(handler),
    &[Value::Object(target)],
  )?;

  match result {
    Value::Object(o) => Ok(Some(o)),
    Value::Null => Ok(None),
    _ => Err(VmError::TypeError(
      "Proxy getPrototypeOf trap returned non-object",
    )),
  }
}

fn proxy_set_prototype_of(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
  proto: Option<GcObject>,
) -> Result<bool, VmError> {
  let proto_val = match proto {
    Some(p) => Value::Object(p),
    None => Value::Null,
  };

  // Root `proxy` + `proto` across Proxy chain traversal and trap calls so `current` stays valid even
  // if a `GetMethod`/trap call triggers GC.
  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::Object(proxy), proto_val])?;

  // Allocate the trap key once per operation (rather than once per proxy hop).
  let trap_key_s = scope.alloc_string("setPrototypeOf")?;
  scope.push_root(Value::String(trap_key_s))?;
  let trap_key = PropertyKey::from_string(trap_key_s);

  // Follow Proxy chains iteratively to avoid recursion: attacker-controlled Proxy chains can be
  // very deep and can otherwise overflow the Rust stack.
  let mut current = proxy;
  let mut steps = 0usize;

  loop {
    const TICK_EVERY: usize = 1024;
    if steps != 0 && steps % TICK_EVERY == 0 {
      vm.tick()?;
    }
    if steps >= crate::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }
    steps += 1;

    if !scope.heap().is_proxy_object(current) {
      // OrdinarySetPrototypeOf (ECMA-262).
      let current_proto = scope.heap().object_prototype(current)?;
      if current_proto == proto {
        return Ok(true);
      }
      if !scope.heap().object_is_extensible(current)? {
        return Ok(false);
      }
      return match scope.heap_mut().object_set_prototype(current, proto) {
        Ok(()) => Ok(true),
        Err(VmError::PrototypeCycle | VmError::PrototypeChainTooDeep) => Ok(false),
        Err(e) => Err(e),
      };
    }

    let (target, handler) = proxy_target_and_handler(&scope, current)?;
    scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

    let trap =
      vm.get_method_with_host_and_hooks(host, &mut scope, hooks, Value::Object(handler), trap_key)?;
    let Some(trap) = trap else {
      // No trap: forward to the target's `[[SetPrototypeOf]]`.
      current = target;
      continue;
    };
    scope.push_root(trap)?;

    let result = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      trap,
      Value::Object(handler),
      &[Value::Object(target), proto_val],
    )?;

    // ECMAScript Proxy `[[SetPrototypeOf]]` invariants (ECMA-262):
    // - If the trap reports failure, propagate `false`.
    // - If the trap reports success:
    //   - If the target is extensible, success is accepted.
    //   - Otherwise (non-extensible), the requested prototype must match the target's actual
    //     prototype, or we throw a TypeError.
    let trap_result = scope.heap().to_boolean(result)?;
    if !trap_result {
      return Ok(false);
    }

    let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
    if extensible_target {
      return Ok(true);
    }

    let target_proto = scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, target)?;
    if target_proto == proto {
      return Ok(true);
    }

    return Err(VmError::TypeError(
      "Proxy setPrototypeOf trap returned true for non-extensible target with different prototype",
    ));
  }
}

fn proxy_own_property_keys_with_tick(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
  tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
) -> Result<Vec<PropertyKey>, VmError> {
  let (target, handler) = proxy_target_and_handler(scope, proxy)?;

  let mut scope = scope.reborrow();
  scope.push_roots(&[
    Value::Object(proxy),
    Value::Object(target),
    Value::Object(handler),
  ])?;

  let Some(trap) = proxy_get_method(vm, &mut scope, host, hooks, handler, "ownKeys")? else {
    return scope.own_property_keys_with_host_and_hooks_with_tick(vm, host, hooks, target, tick);
  };

  let trap_result = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    trap,
    Value::Object(handler),
    &[Value::Object(target)],
  )?;
  let Value::Object(trap_result_obj) = trap_result else {
    return Err(VmError::TypeError(
      "Proxy ownKeys trap returned non-object",
    ));
  };
  scope.push_root(trap_result)?;

  let list = crate::spec_ops::create_list_from_array_like_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    trap_result_obj,
  )?;

  let mut out: Vec<PropertyKey> = Vec::new();
  out.try_reserve_exact(list.len()).map_err(|_| VmError::OutOfMemory)?;

  for (i, v) in list.into_iter().enumerate() {
    if i % 1024 == 0 {
      tick(vm)?;
    }
    match v {
      Value::String(s) => out.push(PropertyKey::from_string(s)),
      Value::Symbol(s) => out.push(PropertyKey::from_symbol(s)),
      _ => return Err(VmError::TypeError("Proxy ownKeys trap returned invalid key")),
    }
  }

  validate_proxy_own_keys_trap_result(vm, &mut scope, host, hooks, target, &out)?;
  Ok(out)
}

fn proxy_get_own_property_with_tick(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
  key: PropertyKey,
  _tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
) -> Result<Option<PropertyDescriptor>, VmError> {
  // Delegate to the spec-shaped `[[GetOwnProperty]]` wrapper, which implements Proxy trap
  // invariants.
  scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, proxy, key)
}

fn proxy_define_own_property_with_tick(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
  key: PropertyKey,
  desc: PropertyDescriptorPatch,
) -> Result<bool, VmError> {
  let (target, handler) = proxy_target_and_handler(scope, proxy)?;

  desc.validate()?;
  let key_val = property_key_to_value(key);

  let mut scope = scope.reborrow();
  // Root all values that might otherwise only exist in local variables while we allocate call the
  // trap or allocate the descriptor object.
  let mut roots = [Value::Undefined; 7];
  let mut root_count = 0usize;
  roots[root_count] = Value::Object(proxy);
  root_count += 1;
  roots[root_count] = Value::Object(target);
  root_count += 1;
  roots[root_count] = Value::Object(handler);
  root_count += 1;
  roots[root_count] = key_val;
  root_count += 1;
  if let Some(v) = desc.value {
    roots[root_count] = v;
    root_count += 1;
  }
  if let Some(v) = desc.get {
    roots[root_count] = v;
    root_count += 1;
  }
  if let Some(v) = desc.set {
    roots[root_count] = v;
    root_count += 1;
  }
  scope.push_roots(&roots[..root_count])?;

  let Some(trap) = proxy_get_method(vm, &mut scope, host, hooks, handler, "defineProperty")? else {
    let mut tick = Vm::tick;
    return scope.define_own_property_with_host_and_hooks_with_tick(
      vm,
      host,
      hooks,
      target,
      key,
      desc,
      &mut tick,
    );
  };
  // `trap` can come from an accessor on `handler`, so root it across allocations while building the
  // descriptor argument object.
  scope.push_root(trap)?;

  // Spec: `descObj = FromPropertyDescriptor(desc)` (partial): create a fresh descriptor object with
  // only present fields.
  let desc_obj = property_descriptor_ops::from_property_descriptor_patch(&mut scope, desc)?;
  scope.push_root(Value::Object(desc_obj))?;

  let trap_result = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    trap,
    Value::Object(handler),
    &[Value::Object(target), key_val, Value::Object(desc_obj)],
  )?;

  let ok = scope.heap().to_boolean(trap_result)?;
  if !ok {
    return Ok(false);
  }

  // Spec invariant checks:
  // - the trap cannot report success when the target is non-extensible and the property does not
  //   exist,
  // - it cannot violate non-configurable/non-writable constraints,
  // - and if the caller requested `configurable: false`, the target must now have a non-configurable
  //   property.
  let target_desc =
    scope.object_get_own_property_with_host_and_hooks_complete(vm, host, hooks, target, key)?;

  // Root any values from `target_desc` across `IsExtensible(target)` (which can invoke Proxy traps
  // and allocate).
  let mut desc_value_roots = [Value::Undefined; 2];
  let mut desc_value_root_count = 0usize;
  if let Some(d) = &target_desc {
    match d.kind {
      PropertyKind::Data { value, .. } => {
        desc_value_roots[desc_value_root_count] = value;
        desc_value_root_count += 1;
      }
      PropertyKind::Accessor { get, set } => {
        desc_value_roots[desc_value_root_count] = get;
        desc_value_root_count += 1;
        desc_value_roots[desc_value_root_count] = set;
        desc_value_root_count += 1;
      }
    }
  }
  if desc_value_root_count != 0 {
    scope.push_roots(&desc_value_roots[..desc_value_root_count])?;
  }
  let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;

  let compatible = property_descriptor_ops::is_compatible_property_descriptor(
    extensible_target,
    desc,
    target_desc,
    scope.heap(),
  );
  if !compatible {
    return Err(VmError::TypeError(
      "Proxy defineProperty trap returned an incompatible property descriptor",
    ));
  }

  // Spec: if `Desc.[[Configurable]]` is `false`, the target must already have a non-configurable
  // property.
  if desc.configurable == Some(false) {
    let Some(target_desc) = target_desc else {
      return Err(VmError::TypeError(
        "Proxy defineProperty trap returned true for a missing non-configurable property",
      ));
    };
    if target_desc.configurable {
      return Err(VmError::TypeError(
        "Proxy defineProperty trap returned true for a configurable target property when defining non-configurable",
      ));
    }
  }
  Ok(true)
}

fn proxy_has_property_with_tick(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
  key: PropertyKey,
  tick: &mut impl FnMut(&mut Vm) -> Result<(), VmError>,
) -> Result<bool, VmError> {
  tick(vm)?;
  let (target, handler) = proxy_target_and_handler(scope, proxy)?;

  let key_val = property_key_to_value(key);
  let mut scope = scope.reborrow();
  scope.push_roots(&[
    Value::Object(proxy),
    Value::Object(target),
    Value::Object(handler),
    key_val,
  ])?;

  let Some(trap) = proxy_get_method(vm, &mut scope, host, hooks, handler, "has")? else {
    return scope.has_property_with_host_and_hooks_with_tick(vm, host, hooks, target, key, tick);
  };

  let trap_result = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    trap,
    Value::Object(handler),
    &[Value::Object(target), key_val],
  )?;

  let trap_bool = scope.heap().to_boolean(trap_result)?;
  if trap_bool {
    return Ok(true);
  }
 
  // Proxy invariants (same as `Proxy.[[HasProperty]]`):
  // - the trap cannot report `false` if the target has a non-configurable own property,
  // - and it cannot report `false` for an existing property on a non-extensible target.
  let target_desc = scope.get_own_property_with_host_and_hooks_with_tick(vm, host, hooks, target, key, tick)?;
  let Some(target_desc) = target_desc else {
    return Ok(false);
  };
  if !target_desc.configurable {
    return Err(VmError::TypeError(
      "Proxy has trap returned false for a non-configurable target property",
    ));
  }
  let extensible_target = scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)?;
  if !extensible_target {
    return Err(VmError::TypeError(
      "Proxy has trap returned false for an existing property on a non-extensible target",
    ));
  }
  Ok(false)
}

fn proxy_set(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  proxy: GcObject,
  key: PropertyKey,
  value: Value,
  receiver: Value,
) -> Result<bool, VmError> {
  let key_val = property_key_to_value(key);
  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::Object(proxy), receiver, value, key_val])?;

  // Allocate the trap key once (rather than once per proxy hop).
  let mut trap_key: Option<PropertyKey> = None;

  // Follow Proxy chains iteratively to avoid recursion (deep attacker-controlled Proxy chains
  // should not be able to overflow the Rust stack).
  let mut current = proxy;
  let mut steps = 0usize;
  loop {
    const TICK_EVERY: usize = 1024;
    if steps != 0 && steps % TICK_EVERY == 0 {
      vm.tick()?;
    }
    if steps >= crate::MAX_PROTOTYPE_CHAIN {
      return Err(VmError::PrototypeChainTooDeep);
    }
    steps += 1;

    // If we've reached a non-proxy target (i.e. all Proxy objects in the chain had an undefined
    // `set` trap), fall back to the target's ordinary `[[Set]]` behaviour.
    let Some(proxy_data) = scope.heap().get_proxy_data(current)? else {
      return scope.ordinary_set_with_host_and_hooks(vm, host, hooks, current, key, value, receiver);
    };
    let (Some(target), Some(handler)) = (proxy_data.target, proxy_data.handler) else {
      return Err(VmError::TypeError("Cannot perform 'set' on a revoked Proxy"));
    };

    // Root the Proxy's `[[ProxyTarget]]` and `[[ProxyHandler]]` while we look up and invoke the
    // trap.
    //
    // `GetMethod(handler, "set")` can run user code via accessors. That user code can revoke this
    // Proxy (clearing `[[ProxyTarget]]`/`[[ProxyHandler]]`) and then trigger a GC. If that happens,
    // `target` could become unreachable and collected even though the Proxy algorithm is still
    // required to use the original target object for this operation.
    scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

    let trap_key = match trap_key {
      Some(k) => k,
      None => {
        let s = scope.alloc_string("set")?;
        scope.push_root(Value::String(s))?;
        let k = PropertyKey::from_string(s);
        trap_key = Some(k);
        k
      }
    };

    let trap = vm.get_method_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      Value::Object(handler),
      trap_key,
    )?;

    // If the trap is undefined, forward to the target.
    let Some(trap) = trap else {
      current = target;
      continue;
    };
    // `trap` can come from an accessor on `handler`, so root it across allocations/GC.
    scope.push_root(trap)?;

    let trap_args = [Value::Object(target), key_val, value, receiver];
    let trap_result = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      trap,
      Value::Object(handler),
      &trap_args,
    )?;
    let trap_bool = scope.heap().to_boolean(trap_result)?;
    if !trap_bool {
      return Ok(false);
    }

    // Proxy invariants (ECMA-262 `Proxy.[[Set]]`):
    // - The trap cannot report success when the target has a non-configurable, non-writable data
    //   property with a different value.
    // - The trap cannot report success when the target has a non-configurable accessor with an
    //   undefined setter.
    // - If the property does not exist, the trap cannot report success when the target is
    //   non-extensible.
    let target_desc =
      scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, key)?;
    if let Some(target_desc) = &target_desc {
      if !target_desc.configurable {
        match target_desc.kind {
          PropertyKind::Data { writable: false, .. } => {
            // Ensure string exotic index properties materialize their actual value.
            let Some(target_desc) = scope.object_get_own_property_with_host_and_hooks_complete(
              vm, host, hooks, target, key,
            )? else {
              return Err(VmError::InvariantViolation(
                "proxy_set: internal error: missing target property descriptor",
              ));
            };
            let PropertyKind::Data {
              value: target_value,
              ..
            } = target_desc.kind else {
              return Err(VmError::InvariantViolation(
                "proxy_set: internal error: expected data descriptor",
              ));
            };
            if !value.same_value(target_value, scope.heap()) {
              return Err(VmError::TypeError(
                "Proxy set trap returned true for a non-writable, non-configurable data property with a different value",
              ));
            }
          }
          PropertyKind::Accessor {
            set: Value::Undefined,
            ..
          } => {
            return Err(VmError::TypeError(
              "Proxy set trap returned true for a non-configurable accessor property with an undefined setter",
            ));
          }
          _ => {}
        }
      }
      return Ok(true);
    }

    if !scope.is_extensible_with_host_and_hooks(vm, host, hooks, target)? {
      return Err(VmError::TypeError(
        "Proxy set trap returned true for a non-extensible target and a non-existent property",
      ));
    }

    return Ok(true);
  }
}

fn array_length_from_value(scope: &mut Scope<'_>, value: Value) -> Result<u32, VmError> {
  // ECMA-262 `ArraySetLength` converts the incoming value via `ToNumber` and then rejects the
  // definition if `ToUint32(numberLen) !== numberLen` by throwing a RangeError.
  //
  // `vm-js` implements `ToNumber` for primitives here (and also supports unboxing the VM's primitive
  // wrapper objects) so `[].length = "1"` and `[].length = new Number(1)` behave per spec.
  //
  // Full `ToNumber` for arbitrary objects requires `ToPrimitive` which can invoke user code and
  // therefore needs a `Vm` + host context; those cases remain unimplemented for now.
  let prim = match value {
    Value::Object(obj) => {
      let heap = scope.heap();
      let unbox = |sym: Option<crate::GcSymbol>| -> Result<Option<Value>, VmError> {
        let Some(sym) = sym else {
          return Ok(None);
        };
        heap.object_get_own_data_property_value(obj, &PropertyKey::from_symbol(sym))
      };

      // Note: order matches `ToNumber` / `ToPrimitive` behaviour for the builtin wrappers.
      if let Some(v) = unbox(heap.internal_number_data_symbol())? {
        if matches!(v, Value::Number(_)) {
          v
        } else {
          value
        }
      } else if let Some(v) = unbox(heap.internal_string_data_symbol())? {
        if matches!(v, Value::String(_)) {
          v
        } else {
          value
        }
      } else if let Some(v) = unbox(heap.internal_boolean_data_symbol())? {
        if matches!(v, Value::Bool(_)) {
          v
        } else {
          value
        }
      } else {
        value
      }
    }
    other => other,
  };

  let n = crate::ops::to_number(scope.heap_mut(), prim)?;
  if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
    return Err(VmError::RangeError("Invalid array length"));
  }
  Ok(n as u32)
}

fn validate_and_apply_property_descriptor(
  scope: &mut Scope<'_>,
  obj: Option<GcObject>,
  key: PropertyKey,
  extensible: bool,
  desc: PropertyDescriptorPatch,
  current: Option<PropertyDescriptor>,
) -> Result<bool, VmError> {
  desc.validate()?;

  let Some(current_desc) = current else {
    if !extensible {
      return Ok(false);
    }

    // Create new property with default attributes for missing fields.
    let enumerable = desc.enumerable.unwrap_or(false);
    let configurable = desc.configurable.unwrap_or(false);
    let new_desc = if desc.is_accessor_descriptor() {
      PropertyDescriptor {
        enumerable,
        configurable,
        kind: PropertyKind::Accessor {
          get: desc.get.unwrap_or(Value::Undefined),
          set: desc.set.unwrap_or(Value::Undefined),
        },
      }
    } else {
      // Generic descriptors create data properties.
      PropertyDescriptor {
        enumerable,
        configurable,
        kind: PropertyKind::Data {
          value: desc.value.unwrap_or(Value::Undefined),
          writable: desc.writable.unwrap_or(false),
        },
      }
    };

    if let Some(obj) = obj {
      scope.define_property(obj, key, new_desc)?;
    }
    return Ok(true);
  };

  // If `Desc` has no fields, no change is requested.
  if desc.is_empty() {
    return Ok(true);
  }

  // Non-configurable invariants.
  if !current_desc.configurable {
    if matches!(desc.configurable, Some(true)) {
      return Ok(false);
    }
    if let Some(enumerable) = desc.enumerable {
      if enumerable != current_desc.enumerable {
        return Ok(false);
      }
    }
  }

  let desc_is_generic = desc.is_generic_descriptor();
  let desc_is_data = desc.is_data_descriptor();
  let desc_is_accessor = desc.is_accessor_descriptor();

  let current_is_data = current_desc.is_data_descriptor();
  let current_is_accessor = current_desc.is_accessor_descriptor();

  // Reject kind switches when not configurable.
  if !current_desc.configurable && !desc_is_generic {
    if (current_is_data && desc_is_accessor) || (current_is_accessor && desc_is_data) {
      return Ok(false);
    }
  }

  if !desc_is_generic {
    match (&current_desc.kind, current_desc.configurable) {
      (PropertyKind::Data { value, writable }, false) if desc_is_data => {
        if !writable {
          if desc.writable == Some(true) {
            return Ok(false);
          }
          if let Some(new_value) = desc.value {
            if !new_value.same_value(*value, scope.heap()) {
              return Ok(false);
            }
          }
        }
      }
      (PropertyKind::Accessor { get, set }, false) if desc_is_accessor => {
        if let Some(new_get) = desc.get {
          if !new_get.same_value(*get, scope.heap()) {
            return Ok(false);
          }
        }
        if let Some(new_set) = desc.set {
          if !new_set.same_value(*set, scope.heap()) {
            return Ok(false);
          }
        }
      }
      _ => {}
    }
  }

  if let Some(obj) = obj {
    let new_desc = apply_descriptor_patch(current_desc, desc);
    scope.define_property(obj, key, new_desc)?;
  }

  Ok(true)
}

fn apply_descriptor_patch(current: PropertyDescriptor, desc: PropertyDescriptorPatch) -> PropertyDescriptor {
  let enumerable = desc.enumerable.unwrap_or(current.enumerable);
  let configurable = desc.configurable.unwrap_or(current.configurable);

  if desc.is_generic_descriptor() {
    return PropertyDescriptor {
      enumerable,
      configurable,
      kind: current.kind,
    };
  }

  match (current.kind, desc.is_accessor_descriptor()) {
    (PropertyKind::Data { value, writable }, false) => PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Data {
        value: desc.value.unwrap_or(value),
        writable: desc.writable.unwrap_or(writable),
      },
    },
    (PropertyKind::Accessor { get, set }, true) => PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Accessor {
        get: desc.get.unwrap_or(get),
        set: desc.set.unwrap_or(set),
      },
    },
    // Kind conversions. Default values are per `ValidateAndApplyPropertyDescriptor`.
    (PropertyKind::Data { .. }, true) => PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Accessor {
        get: desc.get.unwrap_or(Value::Undefined),
        set: desc.set.unwrap_or(Value::Undefined),
      },
    },
    (PropertyKind::Accessor { .. }, false) => PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Data {
        value: desc.value.unwrap_or(Value::Undefined),
        writable: desc.writable.unwrap_or(false),
      },
    },
  }
}
