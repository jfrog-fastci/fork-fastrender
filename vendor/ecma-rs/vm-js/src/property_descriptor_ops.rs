use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::{GcObject, Heap, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

fn has_property_proxy_aware(
  vm: &mut Vm,
  scope: &Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<bool, VmError> {
  let mut current = obj;
  let mut steps = 0usize;
  loop {
    if steps != 0 && steps % 1024 == 0 {
      vm.tick()?;
    }
    steps = steps.saturating_add(1);
    if !scope.heap().is_proxy_object(current) {
      return scope.ordinary_has_property_with_tick(current, key, || vm.tick());
    }
    let Some(target) = scope.heap().proxy_target(current)? else {
      return Err(VmError::TypeError("Cannot perform 'has' on a revoked Proxy"));
    };
    current = target;
  }
}

fn get_proxy_aware(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  receiver: Value,
) -> Result<Value, VmError> {
  // Proxy-aware `[[Get]]` internal method dispatch.
  //
  // This intentionally mirrors `spec_ops::get_with_host_and_hooks_internal`, but `ToPropertyDescriptor`
  // is used by operations like Proxy trap validation and Reflect/Object builtins and should not
  // depend on a private helper.
  let mut current = obj;
  let mut steps = 0usize;
  loop {
    if steps != 0 && steps % 1024 == 0 {
      vm.tick()?;
    }
    steps = steps.saturating_add(1);
    if !scope.heap().is_proxy_object(current) {
      return scope.ordinary_get_with_host_and_hooks(vm, host, hooks, current, key, receiver);
    }
    let Some(target) = scope.heap().proxy_target(current)? else {
      return Err(VmError::TypeError("Cannot perform 'get' on a revoked Proxy"));
    };
    current = target;
  }
}

pub fn to_property_descriptor_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  desc_obj: GcObject,
) -> Result<PropertyDescriptorPatch, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(desc_obj))?;

  let enumerable_s = scope.alloc_string("enumerable")?;
  scope.push_root(Value::String(enumerable_s))?;
  let enumerable_key = PropertyKey::from_string(enumerable_s);

  let configurable_s = scope.alloc_string("configurable")?;
  scope.push_root(Value::String(configurable_s))?;
  let configurable_key = PropertyKey::from_string(configurable_s);

  let value_s = scope.alloc_string("value")?;
  scope.push_root(Value::String(value_s))?;
  let value_key = PropertyKey::from_string(value_s);

  let writable_s = scope.alloc_string("writable")?;
  scope.push_root(Value::String(writable_s))?;
  let writable_key = PropertyKey::from_string(writable_s);

  let get_s = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_s))?;
  let get_key = PropertyKey::from_string(get_s);

  let set_s = scope.alloc_string("set")?;
  scope.push_root(Value::String(set_s))?;
  let set_key = PropertyKey::from_string(set_s);

  let mut desc = PropertyDescriptorPatch::default();

  if has_property_proxy_aware(vm, &scope, desc_obj, enumerable_key)? {
    let value = get_proxy_aware(
      vm,
      &mut scope,
      host,
      hooks,
      desc_obj,
      enumerable_key,
      Value::Object(desc_obj),
    )?;
    desc.enumerable = Some(scope.heap().to_boolean(value)?);
  }

  if has_property_proxy_aware(vm, &scope, desc_obj, configurable_key)? {
    let value = get_proxy_aware(
      vm,
      &mut scope,
      host,
      hooks,
      desc_obj,
      configurable_key,
      Value::Object(desc_obj),
    )?;
    desc.configurable = Some(scope.heap().to_boolean(value)?);
  }

  if has_property_proxy_aware(vm, &scope, desc_obj, value_key)? {
    let value = get_proxy_aware(
      vm,
      &mut scope,
      host,
      hooks,
      desc_obj,
      value_key,
      Value::Object(desc_obj),
    )?;
    scope.push_root(value)?;
    desc.value = Some(value);
  }

  if has_property_proxy_aware(vm, &scope, desc_obj, writable_key)? {
    let value = get_proxy_aware(
      vm,
      &mut scope,
      host,
      hooks,
      desc_obj,
      writable_key,
      Value::Object(desc_obj),
    )?;
    desc.writable = Some(scope.heap().to_boolean(value)?);
  }

  if has_property_proxy_aware(vm, &scope, desc_obj, get_key)? {
    let value = get_proxy_aware(
      vm,
      &mut scope,
      host,
      hooks,
      desc_obj,
      get_key,
      Value::Object(desc_obj),
    )?;
    if !matches!(value, Value::Undefined) && !scope.heap().is_callable(value)? {
      return Err(VmError::TypeError("PropertyDescriptor.get is not callable"));
    }
    scope.push_root(value)?;
    desc.get = Some(value);
  }

  if has_property_proxy_aware(vm, &scope, desc_obj, set_key)? {
    let value = get_proxy_aware(
      vm,
      &mut scope,
      host,
      hooks,
      desc_obj,
      set_key,
      Value::Object(desc_obj),
    )?;
    if !matches!(value, Value::Undefined) && !scope.heap().is_callable(value)? {
      return Err(VmError::TypeError("PropertyDescriptor.set is not callable"));
    }
    scope.push_root(value)?;
    desc.set = Some(value);
  }

  desc.validate()?;
  Ok(desc)
}

pub fn from_property_descriptor(scope: &mut Scope<'_>, desc: PropertyDescriptor) -> Result<GcObject, VmError> {
  let mut scope = scope.reborrow();

  // Root values from `desc` during allocations of property keys and the output object.
  let mut roots = [Value::Undefined; 2];
  let mut root_count = 0usize;
  match desc.kind {
    PropertyKind::Data { value, .. } => {
      roots[root_count] = value;
      root_count += 1;
    }
    PropertyKind::Accessor { get, set } => {
      roots[root_count] = get;
      root_count += 1;
      roots[root_count] = set;
      root_count += 1;
    }
  }
  scope.push_roots(&roots[..root_count])?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  // Spec: `FromPropertyDescriptor` creates a new ordinary object. If a realm has been initialized,
  // use the heap's default `%Object.prototype%` so callers (e.g. `Reflect.getOwnPropertyDescriptor`)
  // produce spec-shaped results. In low-level tests without a realm, leave the prototype as `null`.
  if let Some(proto) = scope.heap().default_object_prototype() {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }

  let enumerable_key = PropertyKey::from_string(scope.alloc_string("enumerable")?);
  scope.create_data_property_or_throw(obj, enumerable_key, Value::Bool(desc.enumerable))?;

  let configurable_key = PropertyKey::from_string(scope.alloc_string("configurable")?);
  scope.create_data_property_or_throw(obj, configurable_key, Value::Bool(desc.configurable))?;

  match desc.kind {
    PropertyKind::Data { value, writable } => {
      let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
      scope.create_data_property_or_throw(obj, value_key, value)?;

      let writable_key = PropertyKey::from_string(scope.alloc_string("writable")?);
      scope.create_data_property_or_throw(obj, writable_key, Value::Bool(writable))?;
    }
    PropertyKind::Accessor { get, set } => {
      let get_key = PropertyKey::from_string(scope.alloc_string("get")?);
      scope.create_data_property_or_throw(obj, get_key, get)?;

      let set_key = PropertyKey::from_string(scope.alloc_string("set")?);
      scope.create_data_property_or_throw(obj, set_key, set)?;
    }
  }

  Ok(obj)
}

pub fn from_property_descriptor_patch(
  scope: &mut Scope<'_>,
  desc: PropertyDescriptorPatch,
) -> Result<GcObject, VmError> {
  desc.validate()?;
  let mut scope = scope.reborrow();

  // Root any descriptor values across allocations.
  let mut roots = [Value::Undefined; 3];
  let mut root_count = 0usize;
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

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  if let Some(proto) = scope.heap().default_object_prototype() {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }

  if let Some(enumerable) = desc.enumerable {
    let key = PropertyKey::from_string(scope.alloc_string("enumerable")?);
    scope.create_data_property_or_throw(obj, key, Value::Bool(enumerable))?;
  }
  if let Some(configurable) = desc.configurable {
    let key = PropertyKey::from_string(scope.alloc_string("configurable")?);
    scope.create_data_property_or_throw(obj, key, Value::Bool(configurable))?;
  }
  if let Some(value) = desc.value {
    let key = PropertyKey::from_string(scope.alloc_string("value")?);
    scope.create_data_property_or_throw(obj, key, value)?;
  }
  if let Some(writable) = desc.writable {
    let key = PropertyKey::from_string(scope.alloc_string("writable")?);
    scope.create_data_property_or_throw(obj, key, Value::Bool(writable))?;
  }
  if let Some(get) = desc.get {
    let key = PropertyKey::from_string(scope.alloc_string("get")?);
    scope.create_data_property_or_throw(obj, key, get)?;
  }
  if let Some(set) = desc.set {
    let key = PropertyKey::from_string(scope.alloc_string("set")?);
    scope.create_data_property_or_throw(obj, key, set)?;
  }

  Ok(obj)
}

pub fn complete_property_descriptor(desc: PropertyDescriptorPatch) -> PropertyDescriptor {
  debug_assert!(
    desc.validate().is_ok(),
    "invalid property descriptor patch passed to complete_property_descriptor"
  );

  let enumerable = desc.enumerable.unwrap_or(false);
  let configurable = desc.configurable.unwrap_or(false);

  if desc.is_accessor_descriptor() {
    PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Accessor {
        get: desc.get.unwrap_or(Value::Undefined),
        set: desc.set.unwrap_or(Value::Undefined),
      },
    }
  } else {
    PropertyDescriptor {
      enumerable,
      configurable,
      kind: PropertyKind::Data {
        value: desc.value.unwrap_or(Value::Undefined),
        writable: desc.writable.unwrap_or(false),
      },
    }
  }
}

pub fn is_compatible_property_descriptor(
  extensible: bool,
  desc: PropertyDescriptorPatch,
  current: Option<PropertyDescriptor>,
  heap: &Heap,
) -> bool {
  if desc.validate().is_err() {
    return false;
  }

  let Some(current_desc) = current else {
    return extensible;
  };

  if desc.is_empty() {
    return true;
  }

  if !current_desc.configurable {
    if matches!(desc.configurable, Some(true)) {
      return false;
    }
    if let Some(enumerable) = desc.enumerable {
      if enumerable != current_desc.enumerable {
        return false;
      }
    }
  }

  let desc_is_generic = desc.is_generic_descriptor();
  let desc_is_data = desc.is_data_descriptor();
  let desc_is_accessor = desc.is_accessor_descriptor();

  let current_is_data = current_desc.is_data_descriptor();
  let current_is_accessor = current_desc.is_accessor_descriptor();

  if !current_desc.configurable && !desc_is_generic {
    if (current_is_data && desc_is_accessor) || (current_is_accessor && desc_is_data) {
      return false;
    }
  }

  if !desc_is_generic {
    match (&current_desc.kind, current_desc.configurable) {
      (PropertyKind::Data { value, writable }, false) if desc_is_data => {
        if !writable {
          if desc.writable == Some(true) {
            return false;
          }
          if let Some(new_value) = desc.value {
            if !new_value.same_value(*value, heap) {
              return false;
            }
          }
        }
      }
      (PropertyKind::Accessor { get, set }, false) if desc_is_accessor => {
        if let Some(new_get) = desc.get {
          if !new_get.same_value(*get, heap) {
            return false;
          }
        }
        if let Some(new_set) = desc.set {
          if !new_set.same_value(*set, heap) {
            return false;
          }
        }
      }
      _ => {}
    }
  }

  true
}
