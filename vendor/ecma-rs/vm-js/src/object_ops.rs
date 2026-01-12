use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::property_descriptor_ops;
use crate::{GcObject, GcString, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use std::collections::HashSet;

fn proxy_own_keys_result_to_property_keys(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  array_like: GcObject,
) -> Result<Vec<PropertyKey>, VmError> {
  // Mirror `CreateListFromArrayLike(trapResult, « String, Symbol »)` closely:
  // - read `length` (ToLength)
  // - read indices 0..len-1 via `Get`
  // - require each element to be a String or Symbol
  //
  // This is host-aware because `Get` can invoke user code via accessors.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(array_like))?;

  let length_key_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_key_s))?;
  let length_key = PropertyKey::from_string(length_key_s);

  let length_value = scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    array_like,
    length_key,
    Value::Object(array_like),
  )?;
  let len = scope.to_length(vm, host, hooks, length_value)?;

  let mut out: Vec<PropertyKey> = Vec::new();
  out
    .try_reserve_exact(len)
    .map_err(|_| VmError::OutOfMemory)?;

  const TICK_EVERY: usize = 1024;
  for idx in 0..len {
    if idx % TICK_EVERY == 0 {
      vm.tick()?;
    }
    let idx_s = scope.alloc_string(&idx.to_string())?;
    let key = PropertyKey::from_string(idx_s);
    let value = scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      array_like,
      key,
      Value::Object(array_like),
    )?;
    match value {
      Value::String(s) => out.push(PropertyKey::from_string(s)),
      Value::Symbol(sym) => out.push(PropertyKey::from_symbol(sym)),
      _ => {
        return Err(VmError::TypeError(
          "Proxy ownKeys trap returned a key that is not a String or Symbol",
        ))
      }
    }
  }

  Ok(out)
}

impl<'a> Scope<'a> {
  pub fn object_get_prototype(&self, obj: GcObject) -> Result<Option<GcObject>, VmError> {
    self.heap().object_prototype(obj)
  }

  /// ECMAScript `[[OwnPropertyKeys]]` internal method dispatch.
  ///
  /// This is Proxy-aware: Proxy objects observe the `ownKeys` trap and throw on revoked proxies.
  pub(crate) fn own_property_keys_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<Vec<PropertyKey>, VmError> {
    let mut current = obj;
    let mut steps = 0usize;
    loop {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);

      let proxy = self.heap().get_proxy_data(current)?;
      let Some(proxy) = proxy else {
        return self.ordinary_own_property_keys_with_tick(current, || vm.tick());
      };

      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'ownKeys' on a proxy that has been revoked",
        ));
      };

      let mut scope = self.reborrow();
      scope.push_roots(&[
        Value::Object(current),
        Value::Object(target),
        Value::Object(handler),
      ])?;

      let own_keys_s = scope.alloc_string("ownKeys")?;
      scope.push_root(Value::String(own_keys_s))?;
      let own_keys_key = PropertyKey::from_string(own_keys_s);

      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        Value::Object(handler),
        own_keys_key,
      )?;
      let Some(trap) = trap else {
        // No trap: forward to the target's `[[OwnPropertyKeys]]`.
        current = target;
        continue;
      };

      let trap_result = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        trap,
        Value::Object(handler),
        &[Value::Object(target)],
      )?;
      scope.push_root(trap_result)?;
      let Value::Object(array_like) = trap_result else {
        return Err(VmError::TypeError(
          "Proxy ownKeys trap returned a non-object value",
        ));
      };

      return proxy_own_keys_result_to_property_keys(vm, &mut scope, host, hooks, array_like);
    }
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

  /// ECMAScript `[[GetPrototypeOf]]` internal method dispatch.
  ///
  /// This is Proxy-aware: Proxy objects observe the `getPrototypeOf` trap and throw on revoked
  /// proxies.
  pub(crate) fn get_prototype_of_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
  ) -> Result<Option<GcObject>, VmError> {
    let mut current = obj;
    let mut steps = 0usize;
    loop {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);

      let proxy = self.heap().get_proxy_data(current)?;
      let Some(proxy) = proxy else {
        return self.heap().object_prototype(current);
      };

      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'getPrototypeOf' on a proxy that has been revoked",
        ));
      };

      let mut scope = self.reborrow();
      scope.push_roots(&[
        Value::Object(current),
        Value::Object(target),
        Value::Object(handler),
      ])?;

      let get_proto_s = scope.alloc_string("getPrototypeOf")?;
      scope.push_root(Value::String(get_proto_s))?;
      let get_proto_key = PropertyKey::from_string(get_proto_s);

      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        Value::Object(handler),
        get_proto_key,
      )?;
      let Some(trap) = trap else {
        // No trap: forward to the target's `[[GetPrototypeOf]]`.
        current = target;
        continue;
      };

      let trap_result = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        trap,
        Value::Object(handler),
        &[Value::Object(target)],
      )?;
      scope.push_root(trap_result)?;

      return match trap_result {
        Value::Null => Ok(None),
        Value::Object(o) => Ok(Some(o)),
        _ => Err(VmError::TypeError(
          "Proxy getPrototypeOf trap returned a non-object value",
        )),
      };
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
        return scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, key);
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
      let target_desc = scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, target, key)?;

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
        // Minimal `IsExtensible` semantics: `vm-js` does not yet implement the full Proxy
        // `isExtensible` trap, but the invariants require observing the target's extensibility.
        let extensible_target = scope.object_is_extensible(target)?;
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
      let extensible_target = scope.object_is_extensible(target)?;

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

    scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())
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
    // Root inputs so a Proxy trap can allocate freely.
    let mut scope = self.reborrow();
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[Value::Object(obj), key_root, receiver])?;
 
    // Allocate the trap key once (rather than once per proxy hop).
    let trap_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(trap_key_s))?;
    let trap_key = PropertyKey::from_string(trap_key_s);
 
    // Follow Proxy chains iteratively to avoid recursion.
    let mut current = obj;
    let mut steps = 0usize;
    loop {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);
 
      let Some(proxy) = scope.heap().get_proxy_data(current)? else {
        return scope.ordinary_get_with_host_and_hooks(vm, host, hooks, current, key, receiver);
      };
 
      vm.tick()?;
 
      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError("Cannot perform 'get' on a proxy that has been revoked"));
      };
 
      // Root target/handler for the duration of trap lookup + invocation.
      let mut trap_scope = scope.reborrow();
      trap_scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;
 
      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut trap_scope,
        hooks,
        Value::Object(handler),
        trap_key,
      )?;
 
      // If the trap is undefined, forward to the target's `[[Get]]`.
      let Some(trap) = trap else {
        current = target;
        continue;
      };
 
      // Call the trap with `(target, key, receiver)`.
      let trap_args = [Value::Object(target), key_root, receiver];
      return vm.call_with_host_and_hooks(
        host,
        &mut trap_scope,
        hooks,
        trap,
        Value::Object(handler),
        &trap_args,
      );
    }
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

  /// ECMAScript `[[HasProperty]]` for ordinary objects.
  pub fn ordinary_has_property(&self, obj: GcObject, key: PropertyKey) -> Result<bool, VmError> {
    self.ordinary_has_property_with_tick(obj, key, || Ok(()))
  }

  pub fn ordinary_has_property_with_tick(
    &self,
    obj: GcObject,
    key: PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    // Integer-indexed exotic objects (typed arrays): numeric index keys are handled without
    // consulting the prototype chain.
    if self.heap().is_typed_array_object(obj) {
      if let Some(index) = self.heap().array_index(&key) {
        return Ok((index as usize) < self.heap().typed_array_length(obj)?);
      }
    }

    if self
      .heap()
      .get_property_with_tick(obj, &key, &mut tick)?
      .is_some()
    {
      return Ok(true);
    }
    Ok(
      self
        .string_object_in_range_index_with_tick(obj, &key, &mut tick)?
        .is_some(),
    )
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
          return vm.call(&mut dummy_host, &mut scope, trap, Value::Object(handler), &args);
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
      return vm.call_with_host_and_hooks(host, &mut scope, hooks, trap, Value::Object(handler), &args);
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
    if let Some(desc) = self
      .heap()
      .object_get_own_property_with_tick(obj, &key, || vm.tick())?
    {
      return match desc.kind {
        PropertyKind::Data { value, .. } => Ok(value),
        PropertyKind::Accessor { get, .. } => {
          if matches!(get, Value::Undefined) {
            Ok(Value::Undefined)
          } else {
            if !self.heap().is_callable(get)? {
              return Err(VmError::TypeError("accessor getter is not callable"));
            }
            // Use `Vm::call` (with a dummy host context) so an embedder-provided
            // `Vm::with_host_hooks_override` is respected. `call_without_host` always forces the
            // VM-owned microtask queue, bypassing any active host hooks override.
            let mut dummy_host = ();
            vm.call(&mut dummy_host, self, get, receiver, &[])
          }
        }
      };
    }

    if let Some(value) = self.string_object_get_index_value_with_tick(obj, &key, || vm.tick())? {
      return Ok(value);
    }

    // Integer-indexed exotic objects (typed arrays): numeric index keys do not consult the
    // prototype chain. If we didn't find an own property above, this is an out-of-bounds index.
    if self.heap().is_typed_array_object(obj) && self.heap().array_index(&key).is_some() {
      return Ok(Value::Undefined);
    }

    let Some(desc) = self
      .heap()
      .get_property_from_prototype_with_tick(obj, &key, || vm.tick())?
    else {
      return Ok(Value::Undefined);
    };

    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          Ok(Value::Undefined)
        } else {
          if !self.heap().is_callable(get)? {
            return Err(VmError::TypeError("accessor getter is not callable"));
          }
          let mut dummy_host = ();
          vm.call(&mut dummy_host, self, get, receiver, &[])
        }
      }
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

    // Fast path: own property.
    if let Some(desc) = scope
      .heap()
      .object_get_own_property_with_tick(obj, &key, || vm.tick())?
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

    if let Some(value) = scope.string_object_get_index_value_with_tick(obj, &key, || vm.tick())? {
      return Ok(value);
    }

    // Integer-indexed exotic objects (typed arrays): numeric index keys do not consult the
    // prototype chain or host exotic hooks. If we didn't find an own property above, this is an
    // out-of-bounds index.
    if scope.heap().is_typed_array_object(obj) && scope.heap().array_index(&key).is_some() {
      return Ok(Value::Undefined);
    }

    // Host hook for "exotic" property getters (e.g. DOM named properties) runs before walking the
    // prototype chain.
    if let Some(value) = hooks.host_exotic_get(&mut scope, obj, key, receiver)? {
      return Ok(value);
    }

    let Some(proto) = scope.heap().object_prototype(obj)? else {
      return Ok(Value::Undefined);
    };

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
          continue;
        }
        if !scope.heap().is_callable(trap)? {
          return Err(VmError::TypeError("Proxy get trap is not callable"));
        }

        let args = [Value::Object(target), key_value, receiver];
        return vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          trap,
          Value::Object(handler),
          &args,
        );
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

      // Integer-indexed exotic objects (typed arrays): numeric index keys do not consult the
      // prototype chain or host exotic hooks. If we didn't find an own property above, this is an
      // out-of-bounds index.
      if scope.heap().is_typed_array_object(current) && scope.heap().array_index(&key).is_some() {
        return Ok(Value::Undefined);
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
    // Integer-indexed exotic objects (typed arrays): numeric index keys do not consult the
    // prototype chain.
    if self.heap().is_typed_array_object(obj) && self.heap().array_index(&key).is_some() {
      if let Some(desc) = self
        .heap()
        .object_get_own_property_with_tick(obj, &key, || vm.tick())?
      {
        return match desc.kind {
          PropertyKind::Data { value, .. } => Ok(value),
          PropertyKind::Accessor { get, .. } => {
            if matches!(get, Value::Undefined) {
              Ok(Value::Undefined)
            } else {
              if !self.heap().is_callable(get)? {
                return Err(VmError::TypeError("accessor getter is not callable"));
              }
              vm.call_with_host(self, host, get, receiver, &[])
            }
          }
        };
      }
      return Ok(Value::Undefined);
    }

    let Some(desc) = self
      .heap()
      .get_property_with_tick(obj, &key, || vm.tick())?
    else {
      return Ok(Value::Undefined);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          Ok(Value::Undefined)
        } else {
          if !self.heap().is_callable(get)? {
            return Err(VmError::TypeError("accessor getter is not callable"));
          }
          vm.call_with_host(self, host, get, receiver, &[])
        }
      }
    }
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

    // Integer-indexed exotic objects (typed arrays): numeric index writes update the view's backing
    // buffer. Out-of-bounds / detached writes are silently ignored, but still considered
    // successful so strict-mode assignments do not throw.
    if self.heap().is_typed_array_object(obj) {
      if let Some(index) = self.heap().array_index(&key) {
        let Value::Object(receiver_obj) = receiver else {
          return Ok(false);
        };
        if receiver_obj != obj {
          return Ok(false);
        }
        let _ = self
          .heap_mut()
          .typed_array_set_element_value(obj, index as usize, value)?;
        return Ok(true);
      }
    }

    let mut desc = self
      .heap()
      .get_property_with_tick(obj, &key, || vm.tick())?;
    if desc.is_none() {
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

        let existing_desc = self.ordinary_get_own_property_with_tick(receiver_obj, key, || vm.tick())?;
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

          return self.define_own_property_with_tick(
            receiver_obj,
            key,
            PropertyDescriptorPatch {
              value: Some(value),
              ..Default::default()
            },
            || vm.tick(),
          );
        }

        self.create_data_property(receiver_obj, key, value)
      }
      PropertyKind::Accessor { set, .. } => {
        if matches!(set, Value::Undefined) {
          return Ok(false);
        }
        if !self.heap().is_callable(set)? {
          return Err(VmError::TypeError("accessor setter is not callable"));
        }
        // See `ordinary_get`: prefer `Vm::call` so any active host hook override is honored.
        let mut dummy_host = ();
        let _ = vm.call(&mut dummy_host, self, set, receiver, &[value])?;
        Ok(true)
      }
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

    // Integer-indexed exotic objects (typed arrays): numeric index keys never consult host exotic
    // setters or the prototype chain.
    if self.heap().is_typed_array_object(obj) {
      if let Some(index) = self.heap().array_index(&key) {
        let Value::Object(receiver_obj) = receiver else {
          return Ok(false);
        };
        if receiver_obj != obj {
          return Ok(false);
        }
        let _ = self
          .heap_mut()
          .typed_array_set_element_value(obj, index as usize, value)?;
        return Ok(true);
      }
    }

    // Host hook for "exotic" property setters (e.g. DOM named properties) runs before ordinary
    // `[[Set]]` processing so it can override prototype-chain properties like `constructor`.
    if let Some(result) = hooks.host_exotic_set(self, obj, key, value, receiver)? {
      return Ok(result);
    }

    let mut desc = self
      .heap()
      .get_property_with_tick(obj, &key, || vm.tick())?;
    if desc.is_none() {
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

        let existing_desc = self.ordinary_get_own_property_with_tick(receiver_obj, key, || vm.tick())?;
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

          return self.define_own_property_with_tick(
            receiver_obj,
            key,
            PropertyDescriptorPatch {
              value: Some(value),
              ..Default::default()
            },
            || vm.tick(),
          );
        }

        self.create_data_property(receiver_obj, key, value)
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

    if self.string_object_in_range_index(obj, &key)?.is_some() {
      return Ok(false);
    }

    // Integer-indexed exotic objects (typed arrays): in-range numeric index properties are
    // non-configurable and cannot be deleted.
    if self.heap().is_typed_array_object(obj) {
      if let Some(idx) = self.heap().array_index(&key) {
        if (idx as usize) < self.heap().typed_array_length(obj)? {
          return Ok(false);
        }
      }
    }
    self.heap_mut().ordinary_delete(obj, key)
  }

  /// ECMAScript `[[Delete]]` for ordinary objects, using an explicit embedder host context and host
  /// hook implementation.
  pub fn ordinary_delete_with_host_and_hooks(
    &mut self,
    _vm: &mut Vm,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<bool, VmError> {
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

    if let Some(result) = hooks.host_exotic_delete(self, obj, key)? {
      return Ok(result);
    }

    if self.string_object_in_range_index(obj, &key)?.is_some() {
      return Ok(false);
    }

    // Integer-indexed exotic objects (typed arrays): in-range numeric index properties are
    // non-configurable and cannot be deleted.
    if self.heap().is_typed_array_object(obj) {
      if let Some(idx) = self.heap().array_index(&key) {
        if (idx as usize) < self.heap().typed_array_length(obj)? {
          return Ok(false);
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
    // Root the input so a Proxy trap can allocate freely.
    let mut scope = self.reborrow();
    scope.push_root(Value::Object(obj))?;
 
    // Allocate the trap key once (rather than once per proxy hop).
    let trap_key_s = scope.alloc_string("ownKeys")?;
    scope.push_root(Value::String(trap_key_s))?;
    let trap_key = PropertyKey::from_string(trap_key_s);
 
    // Follow Proxy chains iteratively to avoid recursion.
    let mut current = obj;
    let mut steps = 0usize;
    loop {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);
 
      let Some(proxy) = scope.heap().get_proxy_data(current)? else {
        return scope.ordinary_own_property_keys_with_tick(current, || vm.tick());
      };
 
      vm.tick()?;
 
      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'ownKeys' on a proxy that has been revoked",
        ));
      };
 
      // Root target/handler for the duration of trap lookup + invocation.
      let mut trap_scope = scope.reborrow();
      trap_scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;
 
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
        Value::Object(trap_result_obj),
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
              "Proxy ownKeys trap returned non-string/non-symbol",
            ))
          }
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
        let key_s = self.alloc_string(&i.to_string())?;
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
        let key_s = self.alloc_string(&i.to_string())?;
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

    let Some(new_len) = array_length_from_value(value) else {
      return Ok(false);
    };

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

    // Delete existing array index properties >= newLen, in descending order.
    //
    // `OrdinaryOwnPropertyKeys` already sorts indices numerically, so iterating the resulting list
    // in reverse deletes indices from high to low.
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

    // Fast path: typed array integer index properties.
    let Some(index) = self.heap().array_index(&key) else {
      return self.ordinary_define_own_property(obj, key, desc);
    };

    // Non-canonical numeric strings are not integer indices and should be treated as ordinary.
    //
    // `array_index` already enforces the canonical form.
    let len = self.heap().typed_array_length(obj)?;
    if index as usize >= len {
      return Ok(false);
    }

    if desc.is_accessor_descriptor() {
      return Ok(false);
    }
    // Typed array index properties are always:
    // - enumerable: true
    // - configurable: false
    // - writable: true
    if matches!(desc.configurable, Some(true)) {
      return Ok(false);
    }
    if let Some(enumerable) = desc.enumerable {
      if !enumerable {
        return Ok(false);
      }
    }
    if let Some(writable) = desc.writable {
      if !writable {
        return Ok(false);
      }
    }

    if let Some(value) = desc.value {
      // `typed_array_set_element_value` performs the ToNumber conversion and element-type
      // conversion/clamping.
      //
      // This should always return `true` for an in-bounds `index`, but treat a `false` return as a
      // rejection to preserve spec-like behaviour under internal invariant violations.
      let ok = self
        .heap_mut()
        .typed_array_set_element_value(obj, index as usize, value)?;
      if !ok {
        return Ok(false);
      }
    }

    Ok(true)
  }
}

fn array_length_from_value(value: Value) -> Option<u32> {
  let Value::Number(n) = value else {
    return None;
  };
  if !n.is_finite() {
    return None;
  }
  if n < 0.0 {
    return None;
  }
  if n.fract() != 0.0 {
    return None;
  }
  if n > u32::MAX as f64 {
    return None;
  }
  Some(n as u32)
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
