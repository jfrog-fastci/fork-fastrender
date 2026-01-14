use crate::property::PropertyKey;
use crate::heap::{Trace, Tracer};
use crate::{GcObject, GcString, Heap, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use std::collections::HashMap;

/// Internal iterator state for `for..in` enumeration.
///
/// This is a spec-shaped model of ECMA-262 `EnumerateObjectProperties`:
/// - Walks the prototype chain.
/// - For each object, snapshots `[[OwnPropertyKeys]]` when that object is reached.
/// - Tracks a `visited` set to suppress duplicates (including shadowing by non-enumerable keys).
/// - Re-checks `[[GetOwnProperty]]` at yield time so deleted/reconfigured properties are observed.
#[derive(Debug)]
pub(crate) struct ForInEnumerator {
  original_object: GcObject,
  current_obj: Option<GcObject>,
  current_keys: Vec<PropertyKey>,
  next_key_index: usize,
  visited: VisitedStringKeys,
  scanned_key_count: usize,
}

impl ForInEnumerator {
  pub(crate) fn new(object: GcObject) -> Self {
    Self {
      original_object: object,
      current_obj: Some(object),
      current_keys: Vec::new(),
      next_key_index: 0,
      visited: VisitedStringKeys::default(),
      scanned_key_count: 0,
    }
  }

  /// Returns the next enumerable string key, or `None` if enumeration is complete.
  pub(crate) fn next_key(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<Option<GcString>, VmError> {
    // Budget scanning work even when the loop body is empty.
    const KEY_SCAN_TICK_EVERY: usize = 1024;
    const KEY_ROOT_TICK_EVERY: usize = 1024;

    loop {
      let Some(obj) = self.current_obj else {
        return Ok(None);
      };

      // If we've already snapshotted keys for this object (i.e. `current_keys` is non-empty) and
      // exhausted them in a previous `next_key` call, advance to the next prototype.
      //
      // This ensures we only snapshot `[[OwnPropertyKeys]]` once per object as required by the
      // spec-shaped `EnumerateObjectProperties` model, and (critically) prevents observable
      // re-entry into Proxy `ownKeys` traps after yielding the last key from an object.
      if self.next_key_index >= self.current_keys.len() && !self.current_keys.is_empty() {
        vm.tick()?;
        self.current_obj = scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)?;
        self.current_keys.clear();
        self.next_key_index = 0;

        if let Some(proto) = self.current_obj {
          // Root visited prototypes so a `__proto__` mutation during iteration cannot invalidate
          // the iterator's internal object handle before we finish scanning it.
          scope.push_root(Value::Object(proto))?;
        }
        continue;
      }

      if self.next_key_index >= self.current_keys.len() {
        // Snapshot `[[OwnPropertyKeys]]` for the current object.
        self.current_keys =
          scope.own_property_keys_with_host_and_hooks(vm, host, hooks, obj)?;

        // Root string keys while they are held in `current_keys` so a key handle cannot be
        // invalidated by GC if the property is deleted before we reach it.
        //
        // Important: root them in chunks using `push_roots_with_extra_roots` so that if growing the
        // root stack triggers a GC, *all* collected keys are treated as roots. This matters for
        // Proxy `ownKeys` trap results, where keys are not necessarily reachable from any other
        // heap object once the trap returns.
        let mut key_roots: Vec<Value> = Vec::new();
        key_roots
          .try_reserve_exact(self.current_keys.len())
          .map_err(|_| VmError::OutOfMemory)?;
        for key in &self.current_keys {
          let PropertyKey::String(s) = key else {
            continue;
          };
          key_roots.push(Value::String(*s));
        }
        let mut start = 0usize;
        while start < key_roots.len() {
          let end = key_roots
            .len()
            .min(start.saturating_add(KEY_ROOT_TICK_EVERY));
          let chunk = &key_roots[start..end];
          let remaining = &key_roots[end..];
          scope.push_roots_with_extra_roots(chunk, remaining, &[])?;
          start = end;
          if start < key_roots.len() {
            vm.tick()?;
          }
        }

        self.next_key_index = 0;
      }

      while self.next_key_index < self.current_keys.len() {
        self.scanned_key_count = self.scanned_key_count.wrapping_add(1);
        if (self.scanned_key_count & (KEY_SCAN_TICK_EVERY - 1)) == 0 {
          vm.tick()?;
        }

        let key = self.current_keys[self.next_key_index];
        self.next_key_index += 1;
        let PropertyKey::String(key_s) = key else {
          continue;
        };

        // If a property is deleted before it is processed it is ignored and does *not* count as
        // visited, so an enumerable prototype property with the same key may still be returned.
        if self.visited.contains(vm, scope.heap(), key_s)? {
          continue;
        }

        // Re-check the property descriptor at yield time so deletions and enumerability changes are
        // observable during iteration.
        let Some(desc) = scope.get_own_property_with_host_and_hooks(vm, host, hooks, obj, key)? else {
          continue;
        };

        // Suppress duplicates across own keys/prototypes, including shadowing by non-enumerable
        // keys.
        self.visited.insert(scope.heap(), key_s)?;

        if !desc.enumerable {
          continue;
        }

        // Typed arrays have integer-indexed exotic `[[HasProperty]]` semantics: for numeric index
        // keys they do **not** consult prototypes. This means prototype numeric keys must be
        // skipped when the typed array does not actually have a valid index (e.g. length 0,
        // detached/out-of-bounds).
        //
        // Avoid a general `HasProperty` call here so `for..in` over Proxy objects (and objects with
        // Proxy objects in their prototype chain) remains trap-driven; we only need this filtering
        // for typed array numeric indices.
        if obj != self.original_object
          && scope.heap().is_typed_array_object(self.original_object)
          && scope.heap().canonical_numeric_index_string(key_s)?.is_some()
          && !scope.ordinary_has_property_with_tick(vm, self.original_object, key, Vm::tick)?
        {
          continue;
        }

        return Ok(Some(key_s));
      }

      // Exhausted this object's keys; move to its prototype.
      vm.tick()?;
      self.current_obj = scope.get_prototype_of_with_host_and_hooks(vm, host, hooks, obj)?;
      self.current_keys.clear();
      self.next_key_index = 0;

      if let Some(proto) = self.current_obj {
        // Root visited prototypes so a `__proto__` mutation during iteration cannot invalidate the
        // iterator's internal object handle before we finish scanning it.
        scope.push_root(Value::Object(proto))?;
      }
    }
  }

  /// Number of GC-managed `Value`s held in this enumerator that must be treated as roots when the
  /// enumerator is stored outside of the heap (e.g. inside a generator continuation during
  /// resumption).
  pub(crate) fn root_values_len(&self) -> usize {
    1usize // `original_object`
      .saturating_add(usize::from(self.current_obj.is_some()))
      .saturating_add(
        self
          .current_keys
          .iter()
          .filter(|k| matches!(k, PropertyKey::String(_)))
          .count(),
      )
      .saturating_add(self.visited.total_len())
  }

  /// Pushes all GC-managed values held by this enumerator into `out`.
  ///
  /// Callers should reserve at least [`ForInEnumerator::root_values_len`] elements in `out` before
  /// calling this.
  pub(crate) fn push_root_values(&self, out: &mut Vec<Value>) {
    out.push(Value::Object(self.original_object));
    if let Some(obj) = self.current_obj {
      out.push(Value::Object(obj));
    }
    for key in &self.current_keys {
      match key {
        PropertyKey::String(s) => out.push(Value::String(*s)),
        // `for..in` ignores symbol keys; do not treat them as roots so we don't require symbol
        // handles in `current_keys` to remain GC-valid across yields.
        PropertyKey::Symbol(_) => {}
      }
    }
    self.visited.push_root_values(out);
  }
}

#[derive(Debug, Default)]
struct VisitedStringKeys {
  // Bucketed by the stable hash of the UTF-16 code units; collisions are resolved by comparing the
  // full code-unit sequence.
  by_hash: HashMap<u64, Vec<GcString>>,
}

impl VisitedStringKeys {
  fn total_len(&self) -> usize {
    self
      .by_hash
      .values()
      .fold(0usize, |acc, bucket| acc.saturating_add(bucket.len()))
  }

  fn push_root_values(&self, out: &mut Vec<Value>) {
    for bucket in self.by_hash.values() {
      for s in bucket {
        out.push(Value::String(*s));
      }
    }
  }

  fn contains(&self, vm: &mut Vm, heap: &Heap, key: GcString) -> Result<bool, VmError> {
    const BUCKET_SCAN_TICK_EVERY: usize = 256;

    let hash = heap.get_string(key)?.stable_hash64();

    let Some(bucket) = self.by_hash.get(&hash) else {
      return Ok(false);
    };

    let needle = heap.get_string(key)?.as_code_units();
    for (i, existing) in bucket.iter().enumerate() {
      if i % BUCKET_SCAN_TICK_EVERY == 0 {
        vm.tick()?;
      }
      let existing_units = heap.get_string(*existing)?.as_code_units();
      if existing_units == needle {
        return Ok(true);
      }
    }

    Ok(false)
  }

  fn insert(&mut self, heap: &Heap, key: GcString) -> Result<(), VmError> {
    let hash = heap.get_string(key)?.stable_hash64();

    if let Some(bucket) = self.by_hash.get_mut(&hash) {
      bucket.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      bucket.push(key);
      return Ok(());
    }

    self.by_hash.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    let mut bucket: Vec<GcString> = Vec::new();
    bucket.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
    bucket.push(key);
    self.by_hash.insert(hash, bucket);
    Ok(())
  }
}

impl Trace for ForInEnumerator {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    tracer.trace_value(Value::Object(self.original_object));
    if let Some(obj) = self.current_obj {
      tracer.trace_value(Value::Object(obj));
    }
    for key in &self.current_keys {
      match key {
        PropertyKey::String(s) => tracer.trace_value(Value::String(*s)),
        // `for..in` ignores symbol keys; avoid tracing them so we don't require symbol handles in
        // `current_keys` to remain GC-valid across yields.
        PropertyKey::Symbol(_) => {}
      }
    }
    self.visited.trace(tracer);
  }
}

impl Trace for VisitedStringKeys {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    for bucket in self.by_hash.values() {
      for s in bucket {
        tracer.trace_value(Value::String(*s));
      }
    }
  }
}
