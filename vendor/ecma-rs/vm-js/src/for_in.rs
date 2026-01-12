use crate::property::PropertyKey;
use crate::{GcObject, GcString, Heap, Scope, Value, VmError};
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
  current_obj: Option<GcObject>,
  current_keys: Vec<PropertyKey>,
  next_key_index: usize,
  visited: VisitedStringKeys,
  scanned_key_count: usize,
}

impl ForInEnumerator {
  pub(crate) fn new(object: GcObject) -> Self {
    Self {
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
    scope: &mut Scope<'_>,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<GcString>, VmError> {
    // Budget scanning work even when the loop body is empty.
    const KEY_SCAN_TICK_EVERY: usize = 1024;
    const KEY_ROOT_TICK_EVERY: usize = 1024;

    loop {
      let Some(obj) = self.current_obj else {
        return Ok(None);
      };

      if self.next_key_index >= self.current_keys.len() {
        // Snapshot `[[OwnPropertyKeys]]` for the current object.
        self.current_keys = scope.ordinary_own_property_keys_with_tick(obj, &mut *tick)?;

        // Root string keys while they are held in `current_keys` so a key handle cannot be
        // invalidated by GC if the property is deleted before we reach it.
        let mut rooted: usize = 0;
        for key in &self.current_keys {
          let PropertyKey::String(s) = key else {
            continue;
          };
          rooted = rooted.wrapping_add(1);
          if (rooted & (KEY_ROOT_TICK_EVERY - 1)) == 0 {
            tick()?;
          }
          scope.push_root(Value::String(*s))?;
        }

        self.next_key_index = 0;
      }

      while self.next_key_index < self.current_keys.len() {
        self.scanned_key_count = self.scanned_key_count.wrapping_add(1);
        if (self.scanned_key_count & (KEY_SCAN_TICK_EVERY - 1)) == 0 {
          tick()?;
        }

        let key = self.current_keys[self.next_key_index];
        self.next_key_index += 1;
        let PropertyKey::String(key_s) = key else {
          continue;
        };

        // Suppress duplicates across own keys/prototypes, including shadowing by non-enumerable
        // keys.
        if self
          .visited
          .check_and_mark_visited(scope.heap(), key_s, &mut *tick)?
        {
          continue;
        }

        // Re-check the property descriptor at yield time so deletions and enumerability changes are
        // observable during iteration.
        let Some(desc) =
          scope.ordinary_get_own_property_with_tick(obj, key, &mut *tick)?
        else {
          continue;
        };
        if !desc.enumerable {
          continue;
        }

        return Ok(Some(key_s));
      }

      // Exhausted this object's keys; move to its prototype.
      tick()?;
      self.current_obj = scope.object_get_prototype(obj)?;
      self.current_keys.clear();
      self.next_key_index = 0;

      if let Some(proto) = self.current_obj {
        // Root visited prototypes so a `__proto__` mutation during iteration cannot invalidate the
        // iterator's internal object handle before we finish scanning it.
        scope.push_root(Value::Object(proto))?;
      }
    }
  }
}

#[derive(Debug, Default)]
struct VisitedStringKeys {
  // Bucketed by the stable hash of the UTF-16 code units; collisions are resolved by comparing the
  // full code-unit sequence.
  by_hash: HashMap<u64, Vec<GcString>>,
}

impl VisitedStringKeys {
  fn check_and_mark_visited(
    &mut self,
    heap: &Heap,
    key: GcString,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    const BUCKET_SCAN_TICK_EVERY: usize = 256;

    let hash = heap.get_string(key)?.stable_hash64();

    if let Some(bucket) = self.by_hash.get_mut(&hash) {
      let needle = heap.get_string(key)?.as_code_units();
      for (i, existing) in bucket.iter().enumerate() {
        if i % BUCKET_SCAN_TICK_EVERY == 0 {
          tick()?;
        }
        let existing_units = heap.get_string(*existing)?.as_code_units();
        if existing_units == needle {
          return Ok(true);
        }
      }

      // New key for this hash bucket.
      bucket.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      bucket.push(key);
      return Ok(false);
    }

    // First key for this hash.
    self.by_hash.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    let mut bucket: Vec<GcString> = Vec::new();
    bucket.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
    bucket.push(key);
    self.by_hash.insert(hash, bucket);
    Ok(false)
  }
}

