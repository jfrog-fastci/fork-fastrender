use crate::clock::Clock;
use super::event_loop::EventLoop;
use std::time::Duration;
use std::{
  collections::HashMap,
  sync::{Arc, Mutex, OnceLock},
};

use vm_js::{
  GcObject, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

/// Deterministic web time model for JavaScript APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebTime {
  /// The Unix epoch time (ms) that corresponds to `performance.now() == 0`.
  ///
  /// In tests this should default to `0` for determinism. Real hosts may set this to an actual
  /// epoch timestamp.
  pub time_origin_unix_ms: i64,
}

impl Default for WebTime {
  fn default() -> Self {
    Self {
      time_origin_unix_ms: 0,
    }
  }
}

impl WebTime {
  pub fn new(time_origin_unix_ms: i64) -> Self {
    Self {
      time_origin_unix_ms,
    }
  }

  /// Implementation of `performance.now()`.
  pub fn performance_now<Host>(&self, event_loop: &EventLoop<Host>) -> f64 {
    self.performance_now_from_duration(event_loop.now())
  }

  /// Implementation of `Date.now()`.
  pub fn date_now<Host>(&self, event_loop: &EventLoop<Host>) -> i64 {
    self.date_now_from_duration(event_loop.now())
  }

  pub(crate) fn performance_now_from_duration(&self, now: Duration) -> f64 {
    duration_to_ms_f64(now)
  }

  pub(crate) fn date_now_from_duration(&self, now: Duration) -> i64 {
    self
      .time_origin_unix_ms
      .saturating_add(duration_to_millis_i64(now))
  }
}

struct TimeContext {
  web_time: WebTime,
  clock: Arc<dyn Clock>,
  performance_entries: Vec<PerformanceEntry>,
}

static TIME_CONTEXTS: OnceLock<Mutex<HashMap<usize, TimeContext>>> = OnceLock::new();

fn time_contexts() -> &'static Mutex<HashMap<usize, TimeContext>> {
  TIME_CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An RAII guard that keeps the host clock associated with a `vm-js` heap.
///
/// `vm-js` native functions are plain function pointers, so we store the host clock in a global
/// map keyed by the heap address. Dropping this guard unregisters the mapping.
#[derive(Debug)]
#[must_use = "Time bindings are only valid while the returned TimeBindings is kept alive"]
pub struct TimeBindings {
  heap_key: usize,
}

impl Drop for TimeBindings {
  fn drop(&mut self) {
    // Best-effort cleanup; ignore lock poisoning during unwinding.
    if let Some(map) = TIME_CONTEXTS.get() {
      if let Ok(mut map) = map.lock() {
        map.remove(&self.heap_key);
      }
    }
  }
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

fn readonly_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn readonly_num_desc(value: f64) -> PropertyDescriptor {
  readonly_data_desc(Value::Number(value))
}

fn enumerable_num_desc(value: f64) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::Number(value),
      writable: true,
    },
  }
}

// HostSlots tags for platform objects installed by this module.
//
// These are only used for branding: structuredClone must reject them as platform objects.
const PERFORMANCE_HOST_TAG: u64 = 0x5045_5246_4F52_4D5F; // "PERFORM_"
const PERFORMANCE_TIMING_HOST_TAG: u64 = 0x5045_5246_5449_4D5F; // "PERFTIM_"
const PERFORMANCE_NAVIGATION_HOST_TAG: u64 = 0x5045_5246_4E41_565F; // "PERFNAV_"

// Minimal Performance Timeline / User Timing store.
//
// We intentionally keep this implementation small: many real-world scripts only need
// `performance.mark`, `performance.measure`, and `performance.getEntriesByType`.
const MAX_PERFORMANCE_ENTRIES: usize = 1024;
/// Upper bound for `PerformanceEntry.name`, measured in UTF-16 code units.
const MAX_PERFORMANCE_ENTRY_NAME_CODE_UNITS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerformanceEntryType {
  Mark,
  Measure,
}

impl PerformanceEntryType {
  fn as_str(self) -> &'static str {
    match self {
      PerformanceEntryType::Mark => "mark",
      PerformanceEntryType::Measure => "measure",
    }
  }
}

#[derive(Debug)]
struct PerformanceEntry {
  name: Box<[u16]>,
  entry_type: PerformanceEntryType,
  start_time: f64,
  duration: f64,
}

/// Installs `Date.now()` and `performance.now()` into a `vm-js` realm.
///
/// ## Determinism
/// The returned values are derived solely from `clock.now()`. Tests can pass a [`crate::clock::VirtualClock`]
/// (via `Arc<dyn Clock>`) to ensure these APIs do not observe wall-clock time.
pub fn install_time_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  clock: Arc<dyn Clock>,
  web_time: WebTime,
) -> Result<TimeBindings, VmError> {
  let heap_key = heap as *const Heap as usize;
  let insert_result = {
    let mut map = time_contexts()
      .lock()
      .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
    if map.contains_key(&heap_key) {
      return Err(VmError::Unimplemented(
        "install_time_bindings called more than once for the same heap",
      ));
    }
    map.insert(
      heap_key,
      TimeContext {
        web_time,
        clock,
        performance_entries: Vec::new(),
      },
    );
    Ok(())
  };

  // If inserting the context failed, bubble up early (nothing to clean up).
  insert_result?;

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // --- Date ---
    let date_key_s = scope.alloc_string("Date")?;
    scope.push_root(Value::String(date_key_s))?;
    let date_key = PropertyKey::from_string(date_key_s);
    // `vm-js` provides a minimal `%Date%` constructor, but `new Date()` currently defaults to the
    // epoch (`0`) instead of "now". Many real pages still do `new Date().getTime()`, so install a
    // thin wrapper that delegates to the intrinsic constructor while mapping the zero-arg case to
    // the deterministic host clock.
    let intrinsic_date = match vm.get(&mut scope, global, date_key)? {
      Value::Object(obj) => obj,
      _ => {
        // Fall back to creating a minimal object if the realm doesn't provide `Date`.
        let date = scope.alloc_object()?;
        scope.push_root(Value::Object(date))?;
        scope.define_property(global, date_key, global_data_desc(Value::Object(date)))?;
        date
      }
    };
    scope.push_root(Value::Object(intrinsic_date))?;

    let date_obj = if scope.heap().is_constructor(Value::Object(intrinsic_date))? {
      let date_call_id = vm.register_native_call(date_constructor_call_native)?;
      let date_construct_id = vm.register_native_construct(date_constructor_construct_native)?;
      let date_name = scope.alloc_string("Date")?;
      scope.push_root(Value::String(date_name))?;
      let date_wrapper = scope.alloc_native_function_with_slots(
        date_call_id,
        Some(date_construct_id),
        date_name,
        7,
        &[Value::Object(intrinsic_date)],
      )?;
      scope
        .heap_mut()
        .object_set_prototype(date_wrapper, Some(realm.intrinsics().function_prototype()))?;
      scope.push_root(Value::Object(date_wrapper))?;

      // Ensure `Date.prototype` is the intrinsic Date prototype so `instanceof Date` works and the
      // realm keeps the minimal methods (`toString`, `valueOf`, ...).
      let date_prototype = realm.intrinsics().date_prototype();
      scope.push_root(Value::Object(date_prototype))?;
      let prototype_key_s = scope.alloc_string("prototype")?;
      scope.push_root(Value::String(prototype_key_s))?;
      let prototype_key = PropertyKey::from_string(prototype_key_s);
      let set_ok = scope.ordinary_set(
        vm,
        date_wrapper,
        prototype_key,
        Value::Object(date_prototype),
        Value::Object(date_wrapper),
      )?;
      if !set_ok {
        return Err(VmError::Unimplemented("failed to set Date.prototype"));
      }

      // `Date.prototype.constructor` should point back to the wrapper `Date` function so
      // `Date.prototype.constructor === Date` holds.
      let constructor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(constructor_key_s))?;
      let constructor_key = PropertyKey::from_string(constructor_key_s);
      let _ = scope.ordinary_set(
        vm,
        date_prototype,
        constructor_key,
        Value::Object(date_wrapper),
        Value::Object(date_prototype),
      )?;

      // Replace the global binding so `new Date()` hits the wrapper.
      scope.define_property(
        global,
        date_key,
        global_data_desc(Value::Object(date_wrapper)),
      )?;

      date_wrapper
    } else {
      intrinsic_date
    };

    // --- Date.now() ---
    let date_now_id = vm.register_native_call(date_now_native)?;
    let date_now_name = scope.alloc_string("now")?;
    let date_now = scope.alloc_native_function(date_now_id, None, date_now_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(date_now, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(date_now))?;

    let date_now_key_s = scope.alloc_string("now")?;
    scope.push_root(Value::String(date_now_key_s))?;
    let date_now_key = PropertyKey::from_string(date_now_key_s);
    scope.define_property(
      date_obj,
      date_now_key,
      global_data_desc(Value::Object(date_now)),
    )?;

    // --- Date.prototype.getTime() ---
    //
    // `vm-js` provides a minimal Date constructor/prototype pair, but it intentionally omits many
    // real-world methods for test262 bootstrapping. Many pages still call `new Date().getTime()`;
    // defining `getTime` in terms of the internal `DateData` slot preserves compatibility without
    // requiring a full Date implementation.
    let date_prototype = realm.intrinsics().date_prototype();
    scope.push_root(Value::Object(date_prototype))?;
    let date_get_time_id = vm.register_native_call(date_get_time_native)?;
    let date_get_time_name = scope.alloc_string("getTime")?;
    let date_get_time =
      scope.alloc_native_function(date_get_time_id, None, date_get_time_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(date_get_time, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(date_get_time))?;

    let date_get_time_key_s = scope.alloc_string("getTime")?;
    scope.push_root(Value::String(date_get_time_key_s))?;
    let date_get_time_key = PropertyKey::from_string(date_get_time_key_s);
    scope.define_property(
      date_prototype,
      date_get_time_key,
      global_data_desc(Value::Object(date_get_time)),
    )?;

    // --- performance.now() ---
    let performance = scope.alloc_object()?;
    scope.push_root(Value::Object(performance))?;
    scope.heap_mut().object_set_host_slots(
      performance,
      HostSlots {
        a: PERFORMANCE_HOST_TAG,
        b: 0,
      },
    )?;

    let perf_now_id = vm.register_native_call(performance_now_native)?;
    let perf_now_name = scope.alloc_string("now")?;
    let perf_now = scope.alloc_native_function(perf_now_id, None, perf_now_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_now, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_now))?;

    let perf_now_key_s = scope.alloc_string("now")?;
    scope.push_root(Value::String(perf_now_key_s))?;
    let perf_now_key = PropertyKey::from_string(perf_now_key_s);
    scope.define_property(
      performance,
      perf_now_key,
      global_data_desc(Value::Object(perf_now)),
    )?;

    // `Performance.timeOrigin` is the epoch timestamp (ms) that corresponds to `performance.now() == 0`.
    // This is derived from the deterministic `WebTime` configuration so tests can control it.
    let time_origin_key_s = scope.alloc_string("timeOrigin")?;
    scope.push_root(Value::String(time_origin_key_s))?;
    let time_origin_key = PropertyKey::from_string(time_origin_key_s);
    scope.define_property(
      performance,
      time_origin_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(web_time.time_origin_unix_ms as f64),
          writable: false,
        },
      },
    )?;

    // --- performance.interactionCount (Event Timing API) ---
    //
    // Real-world web-vitals snippets often feature-detect this field (`"interactionCount" in performance`)
    // to decide whether they should install a `PerformanceObserver` for Event Timing entries. FastRender
    // does not currently implement Event Timing, so expose a deterministic stub to keep those snippets
    // on the cheap, synchronous code path.
    let interaction_count_key_s = scope.alloc_string("interactionCount")?;
    scope.push_root(Value::String(interaction_count_key_s))?;
    let interaction_count_key = PropertyKey::from_string(interaction_count_key_s);
    scope.define_property(performance, interaction_count_key, readonly_num_desc(0.0))?;

    // --- performance.mark / measure / getEntries* (User Timing / Performance Timeline) ---
    //
    // Many analytics libraries call these APIs unguarded. Provide a minimal deterministic store
    // to prevent runtime TypeErrors on real-world pages.
    let perf_mark_id = vm.register_native_call(performance_mark_native)?;
    let perf_mark_name = scope.alloc_string("mark")?;
    let perf_mark = scope.alloc_native_function(perf_mark_id, None, perf_mark_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_mark, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_mark))?;
    let perf_mark_key_s = scope.alloc_string("mark")?;
    scope.push_root(Value::String(perf_mark_key_s))?;
    let perf_mark_key = PropertyKey::from_string(perf_mark_key_s);
    scope.define_property(
      performance,
      perf_mark_key,
      global_data_desc(Value::Object(perf_mark)),
    )?;

    let perf_measure_id = vm.register_native_call(performance_measure_native)?;
    let perf_measure_name = scope.alloc_string("measure")?;
    let perf_measure = scope.alloc_native_function(perf_measure_id, None, perf_measure_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_measure, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_measure))?;
    let perf_measure_key_s = scope.alloc_string("measure")?;
    scope.push_root(Value::String(perf_measure_key_s))?;
    let perf_measure_key = PropertyKey::from_string(perf_measure_key_s);
    scope.define_property(
      performance,
      perf_measure_key,
      global_data_desc(Value::Object(perf_measure)),
    )?;

    let perf_get_entries_id = vm.register_native_call(performance_get_entries_native)?;
    let perf_get_entries_name = scope.alloc_string("getEntries")?;
    let perf_get_entries =
      scope.alloc_native_function(perf_get_entries_id, None, perf_get_entries_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_get_entries, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_get_entries))?;
    let perf_get_entries_key_s = scope.alloc_string("getEntries")?;
    scope.push_root(Value::String(perf_get_entries_key_s))?;
    let perf_get_entries_key = PropertyKey::from_string(perf_get_entries_key_s);
    scope.define_property(
      performance,
      perf_get_entries_key,
      global_data_desc(Value::Object(perf_get_entries)),
    )?;

    let perf_get_entries_by_type_id = vm.register_native_call(performance_get_entries_by_type_native)?;
    let perf_get_entries_by_type_name = scope.alloc_string("getEntriesByType")?;
    let perf_get_entries_by_type = scope.alloc_native_function(
      perf_get_entries_by_type_id,
      None,
      perf_get_entries_by_type_name,
      1,
    )?;
    scope.heap_mut().object_set_prototype(
      perf_get_entries_by_type,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(perf_get_entries_by_type))?;
    let perf_get_entries_by_type_key_s = scope.alloc_string("getEntriesByType")?;
    scope.push_root(Value::String(perf_get_entries_by_type_key_s))?;
    let perf_get_entries_by_type_key = PropertyKey::from_string(perf_get_entries_by_type_key_s);
    scope.define_property(
      performance,
      perf_get_entries_by_type_key,
      global_data_desc(Value::Object(perf_get_entries_by_type)),
    )?;

    let perf_get_entries_by_name_id = vm.register_native_call(performance_get_entries_by_name_native)?;
    let perf_get_entries_by_name_name = scope.alloc_string("getEntriesByName")?;
    let perf_get_entries_by_name = scope.alloc_native_function(
      perf_get_entries_by_name_id,
      None,
      perf_get_entries_by_name_name,
      1,
    )?;
    scope.heap_mut().object_set_prototype(
      perf_get_entries_by_name,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(perf_get_entries_by_name))?;
    let perf_get_entries_by_name_key_s = scope.alloc_string("getEntriesByName")?;
    scope.push_root(Value::String(perf_get_entries_by_name_key_s))?;
    let perf_get_entries_by_name_key = PropertyKey::from_string(perf_get_entries_by_name_key_s);
    scope.define_property(
      performance,
      perf_get_entries_by_name_key,
      global_data_desc(Value::Object(perf_get_entries_by_name)),
    )?;

    let perf_clear_marks_id = vm.register_native_call(performance_clear_marks_native)?;
    let perf_clear_marks_name = scope.alloc_string("clearMarks")?;
    let perf_clear_marks =
      scope.alloc_native_function(perf_clear_marks_id, None, perf_clear_marks_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_clear_marks, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_clear_marks))?;
    let perf_clear_marks_key_s = scope.alloc_string("clearMarks")?;
    scope.push_root(Value::String(perf_clear_marks_key_s))?;
    let perf_clear_marks_key = PropertyKey::from_string(perf_clear_marks_key_s);
    scope.define_property(
      performance,
      perf_clear_marks_key,
      global_data_desc(Value::Object(perf_clear_marks)),
    )?;

    let perf_clear_measures_id = vm.register_native_call(performance_clear_measures_native)?;
    let perf_clear_measures_name = scope.alloc_string("clearMeasures")?;
    let perf_clear_measures =
      scope.alloc_native_function(perf_clear_measures_id, None, perf_clear_measures_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_clear_measures, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_clear_measures))?;
    let perf_clear_measures_key_s = scope.alloc_string("clearMeasures")?;
    scope.push_root(Value::String(perf_clear_measures_key_s))?;
    let perf_clear_measures_key = PropertyKey::from_string(perf_clear_measures_key_s);
    scope.define_property(
      performance,
      perf_clear_measures_key,
      global_data_desc(Value::Object(perf_clear_measures)),
    )?;

    let perf_clear_resource_timings_id =
      vm.register_native_call(performance_clear_resource_timings_native)?;
    let perf_clear_resource_timings_name = scope.alloc_string("clearResourceTimings")?;
    let perf_clear_resource_timings = scope.alloc_native_function(
      perf_clear_resource_timings_id,
      None,
      perf_clear_resource_timings_name,
      0,
    )?;
    scope.heap_mut().object_set_prototype(
      perf_clear_resource_timings,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(perf_clear_resource_timings))?;
    let perf_clear_resource_timings_key_s = scope.alloc_string("clearResourceTimings")?;
    scope.push_root(Value::String(perf_clear_resource_timings_key_s))?;
    let perf_clear_resource_timings_key =
      PropertyKey::from_string(perf_clear_resource_timings_key_s);
    scope.define_property(
      performance,
      perf_clear_resource_timings_key,
      global_data_desc(Value::Object(perf_clear_resource_timings)),
    )?;

    let perf_set_resource_timing_buffer_size_id =
      vm.register_native_call(performance_set_resource_timing_buffer_size_native)?;
    let perf_set_resource_timing_buffer_size_name =
      scope.alloc_string("setResourceTimingBufferSize")?;
    let perf_set_resource_timing_buffer_size = scope.alloc_native_function(
      perf_set_resource_timing_buffer_size_id,
      None,
      perf_set_resource_timing_buffer_size_name,
      1,
    )?;
    scope.heap_mut().object_set_prototype(
      perf_set_resource_timing_buffer_size,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(perf_set_resource_timing_buffer_size))?;
    let perf_set_resource_timing_buffer_size_key_s =
      scope.alloc_string("setResourceTimingBufferSize")?;
    scope.push_root(Value::String(perf_set_resource_timing_buffer_size_key_s))?;
    let perf_set_resource_timing_buffer_size_key =
      PropertyKey::from_string(perf_set_resource_timing_buffer_size_key_s);
    scope.define_property(
      performance,
      perf_set_resource_timing_buffer_size_key,
      global_data_desc(Value::Object(perf_set_resource_timing_buffer_size)),
    )?;

    // --- performance.timing (legacy Navigation Timing Level 1) ---
    //
    // Many analytics libraries still probe `performance.timing.navigationStart` even though the
    // API is deprecated. Provide a deterministic stub so pages can feature-detect without
    // throwing.
    //
    // All timestamps are in ms since Unix epoch; we map all fields to `navigationStart` for an
    // MVP deterministic model (durations become 0).
    let timing = scope.alloc_object()?;
    scope.push_root(Value::Object(timing))?;
    scope.heap_mut().object_set_host_slots(
      timing,
      HostSlots {
        a: PERFORMANCE_TIMING_HOST_TAG,
        b: 0,
      },
    )?;
    let navigation_start_ms = web_time.time_origin_unix_ms as f64;

    let timing_fields: [(&str, f64); 21] = [
      ("navigationStart", navigation_start_ms),
      ("unloadEventStart", navigation_start_ms),
      ("unloadEventEnd", navigation_start_ms),
      ("redirectStart", navigation_start_ms),
      ("redirectEnd", navigation_start_ms),
      ("fetchStart", navigation_start_ms),
      ("domainLookupStart", navigation_start_ms),
      ("domainLookupEnd", navigation_start_ms),
      ("connectStart", navigation_start_ms),
      ("connectEnd", navigation_start_ms),
      // Per spec this is 0 when not applicable; keep deterministic 0 for now.
      ("secureConnectionStart", 0.0),
      ("requestStart", navigation_start_ms),
      ("responseStart", navigation_start_ms),
      ("responseEnd", navigation_start_ms),
      ("domLoading", navigation_start_ms),
      ("domInteractive", navigation_start_ms),
      ("domContentLoadedEventStart", navigation_start_ms),
      ("domContentLoadedEventEnd", navigation_start_ms),
      ("domComplete", navigation_start_ms),
      ("loadEventStart", navigation_start_ms),
      ("loadEventEnd", navigation_start_ms),
    ];

    for (name, value) in timing_fields {
      let key_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      scope.define_property(timing, key, readonly_num_desc(value))?;
    }

    // `PerformanceTiming.toJSON()` exists in all major browsers and is used by analytics libraries
    // to serialize legacy navigation timing data. Our stub timing object has non-enumerable
    // properties, so without `toJSON` it would stringify as `{}`.
    let timing_to_json_id = vm.register_native_call(performance_timing_to_json_native)?;
    let timing_to_json_name = scope.alloc_string("toJSON")?;
    scope.push_root(Value::String(timing_to_json_name))?;
    let timing_to_json =
      scope.alloc_native_function(timing_to_json_id, None, timing_to_json_name, 0)?;
    scope.heap_mut().object_set_prototype(
      timing_to_json,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(timing_to_json))?;

    let timing_to_json_key_s = scope.alloc_string("toJSON")?;
    scope.push_root(Value::String(timing_to_json_key_s))?;
    let timing_to_json_key = PropertyKey::from_string(timing_to_json_key_s);
    scope.define_property(
      timing,
      timing_to_json_key,
      global_data_desc(Value::Object(timing_to_json)),
    )?;

    let timing_key_s = scope.alloc_string("timing")?;
    scope.push_root(Value::String(timing_key_s))?;
    let timing_key = PropertyKey::from_string(timing_key_s);
    scope.define_property(
      performance,
      timing_key,
      readonly_data_desc(Value::Object(timing)),
    )?;

    // --- performance.navigation (legacy Navigation Timing Level 1) ---
    //
    // Minimal stub for libraries that still read `performance.navigation.type`.
    let navigation = scope.alloc_object()?;
    scope.push_root(Value::Object(navigation))?;
    scope.heap_mut().object_set_host_slots(
      navigation,
      HostSlots {
        a: PERFORMANCE_NAVIGATION_HOST_TAG,
        b: 0,
      },
    )?;

    let nav_type_key_s = scope.alloc_string("type")?;
    scope.push_root(Value::String(nav_type_key_s))?;
    let nav_type_key = PropertyKey::from_string(nav_type_key_s);
    scope.define_property(navigation, nav_type_key, readonly_num_desc(0.0))?;

    let redirect_count_key_s = scope.alloc_string("redirectCount")?;
    scope.push_root(Value::String(redirect_count_key_s))?;
    let redirect_count_key = PropertyKey::from_string(redirect_count_key_s);
    scope.define_property(navigation, redirect_count_key, readonly_num_desc(0.0))?;

    // Like `performance.timing`, `performance.navigation` is commonly serialized via `toJSON`.
    let navigation_to_json_id = vm.register_native_call(performance_navigation_to_json_native)?;
    let navigation_to_json_name = scope.alloc_string("toJSON")?;
    scope.push_root(Value::String(navigation_to_json_name))?;
    let navigation_to_json = scope.alloc_native_function(
      navigation_to_json_id,
      None,
      navigation_to_json_name,
      0,
    )?;
    scope.heap_mut().object_set_prototype(
      navigation_to_json,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(navigation_to_json))?;
    let navigation_to_json_key_s = scope.alloc_string("toJSON")?;
    scope.push_root(Value::String(navigation_to_json_key_s))?;
    let navigation_to_json_key = PropertyKey::from_string(navigation_to_json_key_s);
    scope.define_property(
      navigation,
      navigation_to_json_key,
      global_data_desc(Value::Object(navigation_to_json)),
    )?;

    let navigation_key_s = scope.alloc_string("navigation")?;
    scope.push_root(Value::String(navigation_key_s))?;
    let navigation_key = PropertyKey::from_string(navigation_key_s);
    scope.define_property(
      performance,
      navigation_key,
      readonly_data_desc(Value::Object(navigation)),
    )?;

    let perf_key_s = scope.alloc_string("performance")?;
    scope.push_root(Value::String(perf_key_s))?;
    let perf_key = PropertyKey::from_string(perf_key_s);
    scope.define_property(
      global,
      perf_key,
      global_data_desc(Value::Object(performance)),
    )?;

    Ok(())
  })();

  if let Err(err) = result {
    // If JS-side installation failed, ensure we don't leave a stale context entry behind.
    if let Ok(mut map) = time_contexts().lock() {
      map.remove(&heap_key);
    }
    return Err(err);
  }

  Ok(TimeBindings { heap_key })
}

/// Updates the clock used by previously installed time bindings for a given `vm-js` heap.
///
/// This is useful for embeddings that create the JS realm before they have access to the final
/// event loop instance (and its clock), but still want `Date.now()` / `performance.now()` to track
/// the event loop clock once scripts execute.
pub(crate) fn update_time_bindings_clock(
  heap: &Heap,
  clock: Arc<dyn Clock>,
) -> Result<(), VmError> {
  let heap_key = heap as *const Heap as usize;
  let mut map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get_mut(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  ctx.clock = clock;
  Ok(())
}

/// Returns the deterministic value used by `Date.now()` for the current `vm-js` heap.
///
/// This is intended for native bindings (e.g. `File` default `lastModified`) that need a stable
/// timestamp without invoking any user-observable JavaScript (like reading `globalThis.Date.now`,
/// which scripts can overwrite).
pub(crate) fn date_now_ms(scope: &Scope<'_>) -> Result<i64, VmError> {
  let (web_time, clock) = with_time_context(scope, |ctx| (ctx.web_time, ctx.clock.clone()))?;
  Ok(web_time.time_origin_unix_ms.saturating_add(duration_to_millis_i64(clock.now())))
}

fn with_time_context<T>(
  scope: &Scope<'_>,
  f: impl FnOnce(&TimeContext) -> T,
) -> Result<T, VmError> {
  let heap_key = scope.heap() as *const Heap as usize;
  let map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  Ok(f(ctx))
}

fn with_time_context_mut<T>(
  scope: &Scope<'_>,
  f: impl FnOnce(&mut TimeContext) -> Result<T, VmError>,
) -> Result<T, VmError> {
  let heap_key = scope.heap() as *const Heap as usize;
  let mut map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get_mut(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  f(ctx)
}

/// Returns the monotonic clock timestamp for the given `vm-js` heap/scope.
///
/// This is the same underlying clock used by `performance.now()` and `Date.now()` via the time
/// bindings installed by [`install_time_bindings`]. Native bindings can use it as a deterministic
/// monotonic time source (e.g. `console.time()`).
pub(crate) fn clock_now(scope: &Scope<'_>) -> Result<Duration, VmError> {
  with_time_context(scope, |ctx| ctx.clock.now())
}

fn date_constructor_call_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Keep vm-js's minimal `Date()` behavior: return a deterministic placeholder string.
  //
  // Real pages typically use `Date.now()` / `new Date()` rather than calling `Date()` as a
  // function; returning a stable placeholder avoids relying on wall-clock time without attempting
  // to format a full date string.
  Ok(Value::String(scope.alloc_string("[object Date]")?))
}

fn date_constructor_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let Some(Value::Object(intrinsic_date)) = slots.first().copied() else {
    return Err(VmError::Unimplemented(
      "Date wrapper missing intrinsic Date constructor slot",
    ));
  };

  if args.is_empty() {
    let now_ms = date_now_ms(scope)?;
    let args = [Value::Number(now_ms as f64)];
    return vm.construct_with_host_and_hooks(
      host,
      scope,
      hooks,
      Value::Object(intrinsic_date),
      &args,
      new_target,
    );
  }

  vm.construct_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(intrinsic_date),
    args,
    new_target,
  )
}

fn date_now_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let ms = date_now_ms(scope)?;
  Ok(Value::Number(ms as f64))
}

fn performance_now_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let clock = with_time_context(scope, |ctx| ctx.clock.clone())?;
  Ok(Value::Number(duration_to_ms_f64(clock.now())))
}

fn performance_timing_to_json_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(timing_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = scope
    .heap()
    .object_host_slots(timing_obj)?
    .ok_or(VmError::TypeError("Illegal invocation"))?;
  if slots.a != PERFORMANCE_TIMING_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }

  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intrinsics.object_prototype()))?;

  // Keep this list in sync with the fields defined on `performance.timing` during installation.
  let fields: [&str; 21] = [
    "navigationStart",
    "unloadEventStart",
    "unloadEventEnd",
    "redirectStart",
    "redirectEnd",
    "fetchStart",
    "domainLookupStart",
    "domainLookupEnd",
    "connectStart",
    "connectEnd",
    "secureConnectionStart",
    "requestStart",
    "responseStart",
    "responseEnd",
    "domLoading",
    "domInteractive",
    "domContentLoadedEventStart",
    "domContentLoadedEventEnd",
    "domComplete",
    "loadEventStart",
    "loadEventEnd",
  ];

  for name in fields {
    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    // Avoid invoking user-defined code: read only own *data* properties.
    let value = scope
      .heap()
      .object_get_own_data_property_value(timing_obj, &key)?;
    let num = match value {
      Some(Value::Number(n)) => n,
      _ => 0.0,
    };
    scope.define_property(out, key, enumerable_num_desc(num))?;
  }

  Ok(Value::Object(out))
}

fn performance_navigation_to_json_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(nav_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = scope
    .heap()
    .object_host_slots(nav_obj)?
    .ok_or(VmError::TypeError("Illegal invocation"))?;
  if slots.a != PERFORMANCE_NAVIGATION_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }

  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intrinsics.object_prototype()))?;

  let fields: [&str; 2] = ["type", "redirectCount"];
  for name in fields {
    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    let value = scope
      .heap()
      .object_get_own_data_property_value(nav_obj, &key)?;
    let num = match value {
      Some(Value::Number(n)) => n,
      _ => 0.0,
    };
    scope.define_property(out, key, enumerable_num_desc(num))?;
  }

  Ok(Value::Object(out))
}

fn performance_entry_type_from_units(units: &[u16]) -> Option<PerformanceEntryType> {
  // `mark` / `measure` ASCII in UTF-16.
  const MARK: &[u16] = &[0x006D, 0x0061, 0x0072, 0x006B];
  const MEASURE: &[u16] = &[
    0x006D, 0x0065, 0x0061, 0x0073, 0x0075, 0x0072, 0x0065,
  ];
  if units == MARK {
    Some(PerformanceEntryType::Mark)
  } else if units == MEASURE {
    Some(PerformanceEntryType::Measure)
  } else {
    None
  }
}

// `navigation` ASCII in UTF-16.
const PERFORMANCE_ENTRY_TYPE_NAVIGATION: &[u16] = &[
  0x006E, 0x0061, 0x0076, 0x0069, 0x0067, 0x0061, 0x0074, 0x0069, 0x006F, 0x006E,
];

// Commonly probed `PerformanceNavigationTiming` fields (ASCII in UTF-16).
const TIMING_FETCH_START: &[u16] = &[
  0x0066, 0x0065, 0x0074, 0x0063, 0x0068, 0x0053, 0x0074, 0x0061, 0x0072, 0x0074,
];
const TIMING_REQUEST_START: &[u16] = &[
  0x0072, 0x0065, 0x0071, 0x0075, 0x0065, 0x0073, 0x0074, 0x0053, 0x0074, 0x0061, 0x0072,
  0x0074,
];
const TIMING_RESPONSE_START: &[u16] = &[
  0x0072, 0x0065, 0x0073, 0x0070, 0x006F, 0x006E, 0x0073, 0x0065, 0x0053, 0x0074, 0x0061,
  0x0072, 0x0074,
];
const TIMING_RESPONSE_END: &[u16] = &[
  0x0072, 0x0065, 0x0073, 0x0070, 0x006F, 0x006E, 0x0073, 0x0065, 0x0045, 0x006E, 0x0064,
];
const TIMING_DOM_INTERACTIVE: &[u16] = &[
  0x0064, 0x006F, 0x006D, 0x0049, 0x006E, 0x0074, 0x0065, 0x0072, 0x0061, 0x0063, 0x0074,
  0x0069, 0x0076, 0x0065,
];
const TIMING_DOM_CONTENT_LOADED_EVENT_START: &[u16] = &[
  0x0064, 0x006F, 0x006D, 0x0043, 0x006F, 0x006E, 0x0074, 0x0065, 0x006E, 0x0074, 0x004C,
  0x006F, 0x0061, 0x0064, 0x0065, 0x0064, 0x0045, 0x0076, 0x0065, 0x006E, 0x0074, 0x0053,
  0x0074, 0x0061, 0x0072, 0x0074,
];
const TIMING_DOM_COMPLETE: &[u16] = &[
  0x0064, 0x006F, 0x006D, 0x0043, 0x006F, 0x006D, 0x0070, 0x006C, 0x0065, 0x0074, 0x0065,
];
const TIMING_LOAD_EVENT_END: &[u16] = &[
  0x006C, 0x006F, 0x0061, 0x0064, 0x0045, 0x0076, 0x0065, 0x006E, 0x0074, 0x0045, 0x006E,
  0x0064,
];

fn alloc_bounded_string_units(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  what: &'static str,
) -> Result<Box<[u16]>, VmError> {
  let s = scope.to_string(vm, host, hooks, value)?;
  scope.push_root(Value::String(s))?;
  let units = scope.heap().get_string(s)?.as_code_units();
  if units.len() > MAX_PERFORMANCE_ENTRY_NAME_CODE_UNITS {
    return Err(VmError::TypeError(what));
  }
  let mut buf: Vec<u16> = Vec::new();
  buf
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;
  buf.extend_from_slice(units);
  Ok(buf.into_boxed_slice())
}

fn timing_offset_from_performance_timing(
  scope: &mut Scope<'_>,
  performance_obj: GcObject,
  field_units: &[u16],
) -> Result<Option<f64>, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(performance_obj))?;

  let timing_key_s = scope.alloc_string("timing")?;
  scope.push_root(Value::String(timing_key_s))?;
  let timing_key = PropertyKey::from_string(timing_key_s);
  let Some(Value::Object(timing_obj)) = scope
    .heap()
    .object_get_own_data_property_value(performance_obj, &timing_key)?
  else {
    return Ok(None);
  };
  scope.push_root(Value::Object(timing_obj))?;

  let nav_start_key_s = scope.alloc_string("navigationStart")?;
  scope.push_root(Value::String(nav_start_key_s))?;
  let nav_start_key = PropertyKey::from_string(nav_start_key_s);

  let field_key_s = scope.alloc_string_from_code_units(field_units)?;
  scope.push_root(Value::String(field_key_s))?;
  let field_key = PropertyKey::from_string(field_key_s);

  let nav_start = scope
    .heap()
    .object_get_own_data_property_value(timing_obj, &nav_start_key)?;
  let field = scope
    .heap()
    .object_get_own_data_property_value(timing_obj, &field_key)?;

  let (Some(Value::Number(nav_start)), Some(Value::Number(field))) = (nav_start, field) else {
    return Ok(None);
  };
  if !nav_start.is_finite() || !field.is_finite() {
    return Ok(None);
  }
  let offset = field - nav_start;
  if !offset.is_finite() || offset.is_nan() {
    return Ok(None);
  }
  Ok(Some(if offset < 0.0 { 0.0 } else { offset }))
}

fn timing_offset_or_zero(
  scope: &mut Scope<'_>,
  performance_obj: Option<GcObject>,
  field_units: &[u16],
) -> Result<f64, VmError> {
  Ok(
    match performance_obj {
      Some(obj) => timing_offset_from_performance_timing(scope, obj, field_units)?.unwrap_or(0.0),
      None => 0.0,
    },
  )
}

fn perf_entry_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn alloc_performance_navigation_timing_entry(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  performance_obj: Option<GcObject>,
) -> Result<GcObject, VmError> {
  let entry_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(entry_obj))?;

  let entry_type_key_s = scope.alloc_string("entryType")?;
  scope.push_root(Value::String(entry_type_key_s))?;
  let entry_type_key = PropertyKey::from_string(entry_type_key_s);

  let name_key_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(name_key_s))?;
  let name_key = PropertyKey::from_string(name_key_s);

  let start_time_key_s = scope.alloc_string("startTime")?;
  scope.push_root(Value::String(start_time_key_s))?;
  let start_time_key = PropertyKey::from_string(start_time_key_s);

  let duration_key_s = scope.alloc_string("duration")?;
  scope.push_root(Value::String(duration_key_s))?;
  let duration_key = PropertyKey::from_string(duration_key_s);

  let type_key_s = scope.alloc_string("type")?;
  scope.push_root(Value::String(type_key_s))?;
  let type_key = PropertyKey::from_string(type_key_s);

  let dom_interactive_key_s = scope.alloc_string("domInteractive")?;
  scope.push_root(Value::String(dom_interactive_key_s))?;
  let dom_interactive_key = PropertyKey::from_string(dom_interactive_key_s);

  let dom_content_loaded_event_start_key_s = scope.alloc_string("domContentLoadedEventStart")?;
  scope.push_root(Value::String(dom_content_loaded_event_start_key_s))?;
  let dom_content_loaded_event_start_key =
    PropertyKey::from_string(dom_content_loaded_event_start_key_s);

  let dom_complete_key_s = scope.alloc_string("domComplete")?;
  scope.push_root(Value::String(dom_complete_key_s))?;
  let dom_complete_key = PropertyKey::from_string(dom_complete_key_s);

  let load_event_end_key_s = scope.alloc_string("loadEventEnd")?;
  scope.push_root(Value::String(load_event_end_key_s))?;
  let load_event_end_key = PropertyKey::from_string(load_event_end_key_s);

  let response_start_key_s = scope.alloc_string("responseStart")?;
  scope.push_root(Value::String(response_start_key_s))?;
  let response_start_key = PropertyKey::from_string(response_start_key_s);

  let response_end_key_s = scope.alloc_string("responseEnd")?;
  scope.push_root(Value::String(response_end_key_s))?;
  let response_end_key = PropertyKey::from_string(response_end_key_s);

  let fetch_start_key_s = scope.alloc_string("fetchStart")?;
  scope.push_root(Value::String(fetch_start_key_s))?;
  let fetch_start_key = PropertyKey::from_string(fetch_start_key_s);

  let request_start_key_s = scope.alloc_string("requestStart")?;
  scope.push_root(Value::String(request_start_key_s))?;
  let request_start_key = PropertyKey::from_string(request_start_key_s);

  let entry_type_s = scope.alloc_string("navigation")?;
  scope.push_root(Value::String(entry_type_s))?;
  let name_s = scope.alloc_string("")?;
  scope.push_root(Value::String(name_s))?;
  let nav_type_s = scope.alloc_string("navigate")?;
  scope.push_root(Value::String(nav_type_s))?;

  let dom_interactive = timing_offset_or_zero(scope, performance_obj, TIMING_DOM_INTERACTIVE)?;
  let dom_content_loaded_event_start =
    timing_offset_or_zero(scope, performance_obj, TIMING_DOM_CONTENT_LOADED_EVENT_START)?;
  let dom_complete = timing_offset_or_zero(scope, performance_obj, TIMING_DOM_COMPLETE)?;
  let load_event_end = timing_offset_or_zero(scope, performance_obj, TIMING_LOAD_EVENT_END)?;
  let response_start = timing_offset_or_zero(scope, performance_obj, TIMING_RESPONSE_START)?;
  let response_end = timing_offset_or_zero(scope, performance_obj, TIMING_RESPONSE_END)?;
  let fetch_start = timing_offset_or_zero(scope, performance_obj, TIMING_FETCH_START)?;
  let request_start = timing_offset_or_zero(scope, performance_obj, TIMING_REQUEST_START)?;

  // Use `loadEventEnd` as a coarse duration since `startTime` is always 0 for navigation entries.
  let duration = if load_event_end.is_finite() && load_event_end >= 0.0 {
    load_event_end
  } else {
    0.0
  };

  scope.define_property(
    entry_obj,
    entry_type_key,
    perf_entry_desc(Value::String(entry_type_s)),
  )?;
  scope.define_property(entry_obj, name_key, perf_entry_desc(Value::String(name_s)))?;
  scope.define_property(entry_obj, start_time_key, perf_entry_desc(Value::Number(0.0)))?;
  scope.define_property(entry_obj, duration_key, perf_entry_desc(Value::Number(duration)))?;
  scope.define_property(entry_obj, type_key, perf_entry_desc(Value::String(nav_type_s)))?;

  scope.define_property(
    entry_obj,
    dom_interactive_key,
    perf_entry_desc(Value::Number(dom_interactive)),
  )?;
  scope.define_property(
    entry_obj,
    dom_content_loaded_event_start_key,
    perf_entry_desc(Value::Number(dom_content_loaded_event_start)),
  )?;
  scope.define_property(
    entry_obj,
    dom_complete_key,
    perf_entry_desc(Value::Number(dom_complete)),
  )?;
  scope.define_property(
    entry_obj,
    load_event_end_key,
    perf_entry_desc(Value::Number(load_event_end)),
  )?;
  scope.define_property(
    entry_obj,
    response_start_key,
    perf_entry_desc(Value::Number(response_start)),
  )?;
  scope.define_property(
    entry_obj,
    response_end_key,
    perf_entry_desc(Value::Number(response_end)),
  )?;
  scope.define_property(
    entry_obj,
    fetch_start_key,
    perf_entry_desc(Value::Number(fetch_start)),
  )?;
  scope.define_property(
    entry_obj,
    request_start_key,
    perf_entry_desc(Value::Number(request_start)),
  )?;

  Ok(entry_obj)
}

fn alloc_performance_navigation_entries_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  performance_obj: Option<GcObject>,
) -> Result<Value, VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let arr = scope.alloc_array(1)?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intrinsics.array_prototype()))?;

  let entry_obj = alloc_performance_navigation_timing_entry(vm, scope, performance_obj)?;

  let idx_s = scope.alloc_u32_index_string(0)?;
  scope.push_root(Value::String(idx_s))?;
  let idx_key = PropertyKey::from_string(idx_s);
  scope.define_property(arr, idx_key, perf_entry_desc(Value::Object(entry_obj)))?;

  Ok(Value::Object(arr))
}

fn alloc_performance_entries_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  entries: &[PerformanceEntry],
  name_filter: Option<&[u16]>,
  type_filter: Option<PerformanceEntryType>,
) -> Result<Value, VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Performance Timeline spec: `getEntries*` must return entries in chronological order (increasing
  // `startTime`). Our internal store is insertion-ordered, but the host clock can be manipulated by
  // tests/embedders, so defensively sort the matches here.
  //
  // Use a stable sort so ties preserve insertion order (deterministic).
  let mut matching: Vec<&PerformanceEntry> = entries
    .iter()
    .filter(|e| {
      let name_ok = match name_filter {
        Some(name) => e.name.as_ref() == name,
        None => true,
      };
      let type_ok = match type_filter {
        Some(ty) => e.entry_type == ty,
        None => true,
      };
      name_ok && type_ok
    })
    .collect();

  matching.sort_by(|a, b| a.start_time.total_cmp(&b.start_time));

  let arr = scope.alloc_array(matching.len())?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intrinsics.array_prototype()))?;

  // Entry object keys (shared by all entries in the returned array).
  let key_name_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(key_name_s))?;
  let key_name = PropertyKey::from_string(key_name_s);

  let key_entry_type_s = scope.alloc_string("entryType")?;
  scope.push_root(Value::String(key_entry_type_s))?;
  let key_entry_type = PropertyKey::from_string(key_entry_type_s);

  let key_start_time_s = scope.alloc_string("startTime")?;
  scope.push_root(Value::String(key_start_time_s))?;
  let key_start_time = PropertyKey::from_string(key_start_time_s);

  let key_duration_s = scope.alloc_string("duration")?;
  scope.push_root(Value::String(key_duration_s))?;
  let key_duration = PropertyKey::from_string(key_duration_s);

  // EntryType values.
  let mark_type_s = scope.alloc_string(PerformanceEntryType::Mark.as_str())?;
  scope.push_root(Value::String(mark_type_s))?;
  let measure_type_s = scope.alloc_string(PerformanceEntryType::Measure.as_str())?;
  scope.push_root(Value::String(measure_type_s))?;

  let mut out_index: u32 = 0;
  for entry in matching {
    let mut scope2 = scope.reborrow();

    let entry_obj = scope2.alloc_object()?;
    scope2.push_root(Value::Object(entry_obj))?;

    let name_s = scope2.alloc_string_from_code_units(entry.name.as_ref())?;
    scope2.push_root(Value::String(name_s))?;
    let entry_type_value = match entry.entry_type {
      PerformanceEntryType::Mark => Value::String(mark_type_s),
      PerformanceEntryType::Measure => Value::String(measure_type_s),
    };

    scope2.define_property(entry_obj, key_name, perf_entry_desc(Value::String(name_s)))?;
    scope2.define_property(entry_obj, key_entry_type, perf_entry_desc(entry_type_value))?;
    scope2.define_property(
      entry_obj,
      key_start_time,
      perf_entry_desc(Value::Number(entry.start_time)),
    )?;
    scope2.define_property(
      entry_obj,
      key_duration,
      perf_entry_desc(Value::Number(entry.duration)),
    )?;

    let idx_s = scope2.alloc_u32_index_string(out_index)?;
    scope2.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);
    scope2.define_property(arr, idx_key, perf_entry_desc(Value::Object(entry_obj)))?;

    out_index += 1;
  }

  Ok(Value::Object(arr))
}

fn performance_mark_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Some(name_value) = args.first().copied() else {
    return Err(VmError::TypeError("performance.mark requires a name"));
  };

  let name_units = alloc_bounded_string_units(
    vm,
    scope,
    host,
    hooks,
    name_value,
    "performance.mark name too long",
  )?;

  let clock = with_time_context(scope, |ctx| ctx.clock.clone())?;
  let start_time = duration_to_ms_f64(clock.now());

  with_time_context_mut(scope, |ctx| {
    if ctx.performance_entries.len() >= MAX_PERFORMANCE_ENTRIES {
      // Evict the oldest entry to enforce a hard memory bound.
      ctx.performance_entries.remove(0);
    }
    ctx
      .performance_entries
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    ctx.performance_entries.push(PerformanceEntry {
      name: name_units,
      entry_type: PerformanceEntryType::Mark,
      start_time,
      duration: 0.0,
    });
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn performance_measure_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Some(name_value) = args.first().copied() else {
    return Err(VmError::TypeError("performance.measure requires a name"));
  };

  let name_units = alloc_bounded_string_units(
    vm,
    scope,
    host,
    hooks,
    name_value,
    "performance.measure name too long",
  )?;

  // Minimal argument handling:
  // - measure(name)
  // - measure(name, startMark)
  // - measure(name, startMark, endMark)
  let start_mark_units = if args.len() >= 2 {
    // In browsers, `startOrOptions` can be an object; we only support ToString coercion here.
    Some(alloc_bounded_string_units(
      vm,
      scope,
      host,
      hooks,
      args[1],
      "performance.measure startMark too long",
    )?)
  } else {
    None
  };

  let end_mark_units = if args.len() >= 3 {
    Some(alloc_bounded_string_units(
      vm,
      scope,
      host,
      hooks,
      args[2],
      "performance.measure endMark too long",
    )?)
  } else {
    None
  };

  let clock = with_time_context(scope, |ctx| ctx.clock.clone())?;
  let now = duration_to_ms_f64(clock.now());

  let (start_mark_time, end_mark_time) = with_time_context(scope, |ctx| {
    let lookup_mark = |name: &[u16]| {
      ctx
        .performance_entries
        .iter()
        .rev()
        .find(|e| e.entry_type == PerformanceEntryType::Mark && e.name.as_ref() == name)
        .map(|e| e.start_time)
    };

    let start_time = start_mark_units.as_deref().and_then(lookup_mark);
    let end_time = end_mark_units.as_deref().and_then(lookup_mark);
    (start_time, end_time)
  })?;

  // Compatibility: treat missing user marks like "fetchStart" as Navigation Timing offsets when
  // available. This keeps real-world analytics snippets from throwing on SSR.
  let perf_obj = match this {
    Value::Object(o) => Some(o),
    _ => None,
  };

  let start_time = if let Some(t) = start_mark_time {
    t
  } else if let (Some(obj), Some(units)) = (perf_obj, start_mark_units.as_deref()) {
    timing_offset_from_performance_timing(scope, obj, units)?.unwrap_or(0.0)
  } else {
    0.0
  };

  let end_time = if let Some(units) = end_mark_units.as_deref() {
    if let Some(t) = end_mark_time {
      t
    } else if let Some(obj) = perf_obj {
      timing_offset_from_performance_timing(scope, obj, units)?.unwrap_or(now)
    } else {
      now
    }
  } else {
    now
  };

  let mut duration = end_time - start_time;
  if !duration.is_finite() || duration.is_nan() || duration < 0.0 {
    duration = 0.0;
  }

  with_time_context_mut(scope, |ctx| {
    if ctx.performance_entries.len() >= MAX_PERFORMANCE_ENTRIES {
      ctx.performance_entries.remove(0);
    }
    ctx
      .performance_entries
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    ctx.performance_entries.push(PerformanceEntry {
      name: name_units,
      entry_type: PerformanceEntryType::Measure,
      start_time,
      duration,
    });
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn performance_get_entries_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let perf_obj = match this {
    Value::Object(o) => Some(o),
    _ => None,
  };

  let heap_key = scope.heap() as *const Heap as usize;
  let map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;

  // Include a minimal `PerformanceNavigationTiming` entry (navigation) alongside user timing
  // entries for better compatibility with real-world analytics.
  let user_entries = alloc_performance_entries_array(vm, scope, &ctx.performance_entries, None, None)?;
  let Value::Object(user_arr) = user_entries else {
    return Err(VmError::Unimplemented("expected performance entries array"));
  };
  scope.push_root(Value::Object(user_arr))?;

  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let out_len = ctx
    .performance_entries
    .len()
    .saturating_add(1)
    .min(u32::MAX as usize);

  let out_arr = scope.alloc_array(out_len)?;
  scope.push_root(Value::Object(out_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(out_arr, Some(intrinsics.array_prototype()))?;

  let nav_entry = alloc_performance_navigation_timing_entry(vm, scope, perf_obj)?;
  let idx0_s = scope.alloc_u32_index_string(0)?;
  scope.push_root(Value::String(idx0_s))?;
  let idx0_key = PropertyKey::from_string(idx0_s);
  scope.define_property(
    out_arr,
    idx0_key,
    perf_entry_desc(Value::Object(nav_entry)),
  )?;

  for i in 0..(out_len.saturating_sub(1)) {
    let from_idx_s = scope.alloc_u32_index_string(i as u32)?;
    scope.push_root(Value::String(from_idx_s))?;
    let from_idx_key = PropertyKey::from_string(from_idx_s);
    let v = scope
      .heap()
      .object_get_own_data_property_value(user_arr, &from_idx_key)?
      .unwrap_or(Value::Undefined);

    let to_idx_s = scope.alloc_u32_index_string((i + 1) as u32)?;
    scope.push_root(Value::String(to_idx_s))?;
    let to_idx_key = PropertyKey::from_string(to_idx_s);
    scope.define_property(out_arr, to_idx_key, perf_entry_desc(v))?;
  }

  Ok(Value::Object(out_arr))
}

fn performance_get_entries_by_type_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let perf_obj = match this {
    Value::Object(o) => Some(o),
    _ => None,
  };

  let type_value = args.first().copied().unwrap_or(Value::Undefined);
  let type_s = scope.to_string(vm, host, hooks, type_value)?;
  scope.push_root(Value::String(type_s))?;
  let units = scope.heap().get_string(type_s)?.as_code_units();
  if units == PERFORMANCE_ENTRY_TYPE_NAVIGATION {
    return alloc_performance_navigation_entries_array(vm, scope, perf_obj);
  }
  let Some(type_filter) = performance_entry_type_from_units(units) else {
    // Unknown entry types should return an empty array (not throw).
    return alloc_performance_entries_array(vm, scope, &[], None, None);
  };
  let heap_key = scope.heap() as *const Heap as usize;
  let map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  alloc_performance_entries_array(vm, scope, &ctx.performance_entries, None, Some(type_filter))
}

fn performance_get_entries_by_name_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Some(name_value) = args.first().copied() else {
    return Err(VmError::TypeError("performance.getEntriesByName requires a name"));
  };
  let name_units = alloc_bounded_string_units(
    vm,
    scope,
    host,
    hooks,
    name_value,
    "performance.getEntriesByName name too long",
  )?;

  let type_filter = if args.len() >= 2 {
    let type_s = scope.to_string(vm, host, hooks, args[1])?;
    scope.push_root(Value::String(type_s))?;
    let units = scope.heap().get_string(type_s)?.as_code_units();
    match performance_entry_type_from_units(units) {
      Some(t) => Some(t),
      None => {
        // Unknown types yield no matches.
        return alloc_performance_entries_array(vm, scope, &[], None, None);
      }
    }
  } else {
    None
  };

  let heap_key = scope.heap() as *const Heap as usize;
  let map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  alloc_performance_entries_array(
    vm,
    scope,
    &ctx.performance_entries,
    Some(name_units.as_ref()),
    type_filter,
  )
}

fn performance_clear_marks_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let name_units = if let Some(name_value) = args.first().copied() {
    Some(alloc_bounded_string_units(
      vm,
      scope,
      host,
      hooks,
      name_value,
      "performance.clearMarks name too long",
    )?)
  } else {
    None
  };

  with_time_context_mut(scope, |ctx| {
    if let Some(name_units) = &name_units {
      ctx.performance_entries.retain(|e| {
        !(e.entry_type == PerformanceEntryType::Mark && e.name.as_ref() == name_units.as_ref())
      });
    } else {
      ctx
        .performance_entries
        .retain(|e| e.entry_type != PerformanceEntryType::Mark);
    }
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn performance_clear_measures_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let name_units = if let Some(name_value) = args.first().copied() {
    Some(alloc_bounded_string_units(
      vm,
      scope,
      host,
      hooks,
      name_value,
      "performance.clearMeasures name too long",
    )?)
  } else {
    None
  };

  with_time_context_mut(scope, |ctx| {
    if let Some(name_units) = &name_units {
      ctx.performance_entries.retain(|e| {
        !(e.entry_type == PerformanceEntryType::Measure && e.name.as_ref() == name_units.as_ref())
      });
    } else {
      ctx
        .performance_entries
        .retain(|e| e.entry_type != PerformanceEntryType::Measure);
    }
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn performance_clear_resource_timings_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // We do not currently store resource timing entries. Provide a no-op stub for compatibility.
  Ok(Value::Undefined)
}

fn performance_set_resource_timing_buffer_size_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Compatibility stub: browsers expose this for configuring resource timing buffer sizes.
  // We do not implement resource timing storage yet, so this is a non-throwing no-op.
  Ok(Value::Undefined)
}

fn date_get_time_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "Date.prototype.getTime called on non-object",
    ));
  };
  // `vm-js` models Date as a branded object kind (`ObjectKind::Date`) with an internal `[[DateValue]]`
  // slot, not as a plain object with a hidden symbol-keyed property. Read the internal slot so
  // `new Date().getTime()` works with both intrinsic and wrapped Date constructors.
  match scope.heap().date_value(obj)? {
    Some(v) => Ok(Value::Number(v)),
    None => Err(VmError::TypeError(
      "Date.prototype.getTime called on non-Date object",
    )),
  }
}

pub(crate) fn duration_to_ms_f64(duration: Duration) -> f64 {
  let nanos = duration.as_nanos();
  let millis = nanos / 1_000_000;
  let rem_nanos = nanos % 1_000_000;
  millis as f64 + rem_nanos as f64 / 1_000_000.0
}

fn duration_to_millis_i64(duration: Duration) -> i64 {
  let millis = duration.as_millis();
  if millis > i64::MAX as u128 {
    i64::MAX
  } else {
    millis as i64
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::clock::VirtualClock;
  use std::sync::Arc;

  fn get_global_property(heap: &mut Heap, realm: &Realm, name: &str) -> Value {
    let mut scope = heap.scope();
    let key_s = scope.alloc_string(name).expect("alloc key string");
    scope
      .push_root(Value::String(key_s))
      .expect("push_root key string");
    let key = PropertyKey::from_string(key_s);
    let global = realm.global_object();
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get global property")
      .unwrap_or_else(|| panic!("missing global property {name}"))
  }

  fn get_object_property(heap: &mut Heap, obj: vm_js::GcObject, name: &str) -> Value {
    let mut scope = heap.scope();
    let key_s = scope.alloc_string(name).expect("alloc key string");
    scope
      .push_root(Value::String(key_s))
      .expect("push_root key string");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .expect("get object property")
      .unwrap_or_else(|| panic!("missing property {name}"))
  }

  fn call0(vm: &mut Vm, heap: &mut Heap, callee: Value, this: Value) -> Value {
    #[derive(Default)]
    struct NoopHostHooks;

    impl vm_js::VmHostHooks for NoopHostHooks {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
        panic!("unexpected Promise job enqueued during time bindings test");
      }
    }

    let mut host_hooks = NoopHostHooks::default();
    let mut scope = heap.scope();
    scope.push_root(callee).unwrap();
    scope.push_root(this).unwrap();

    // Host-created native functions must inherit from `Function.prototype` so `.call` works.
    let Value::Object(func) = callee else {
      panic!("expected function object");
    };
    let call_key_s = scope.alloc_string("call").expect("alloc key string");
    scope.push_root(Value::String(call_key_s)).unwrap();
    let call_key = PropertyKey::from_string(call_key_s);
    let call = vm.get(&mut scope, func, call_key).expect("get call");
    scope.push_root(call).unwrap();

    vm.call_with_host(&mut scope, &mut host_hooks, call, callee, &[this])
      .expect("call should succeed")
  }

  fn call(vm: &mut Vm, heap: &mut Heap, callee: Value, this: Value, args: &[Value]) -> Value {
    #[derive(Default)]
    struct NoopHostHooks;
    impl vm_js::VmHostHooks for NoopHostHooks {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
        panic!("unexpected Promise job enqueued during time bindings test");
      }
    }

    let mut host_hooks = NoopHostHooks::default();
    let mut scope = heap.scope();
    scope.push_root(callee).unwrap();
    scope.push_root(this).unwrap();
    for &arg in args {
      scope.push_root(arg).unwrap();
    }

    let Value::Object(func) = callee else {
      panic!("expected function object");
    };
    let call_key_s = scope.alloc_string("call").expect("alloc key string");
    scope.push_root(Value::String(call_key_s)).unwrap();
    let call_key = PropertyKey::from_string(call_key_s);
    let call_prop = vm.get(&mut scope, func, call_key).expect("get call");
    scope.push_root(call_prop).unwrap();

    let mut argv: Vec<Value> = Vec::new();
    argv.push(this);
    argv.extend_from_slice(args);
    vm.call_with_host(&mut scope, &mut host_hooks, call_prop, callee, &argv)
      .expect("call should succeed")
  }

  fn call_result(
    vm: &mut Vm,
    heap: &mut Heap,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    #[derive(Default)]
    struct NoopHostHooks;
    impl vm_js::VmHostHooks for NoopHostHooks {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
        panic!("unexpected Promise job enqueued during time bindings test");
      }
    }

    let mut host_hooks = NoopHostHooks::default();
    let mut scope = heap.scope();
    scope.push_root(callee).unwrap();
    scope.push_root(this).unwrap();
    for &arg in args {
      scope.push_root(arg).unwrap();
    }

    let Value::Object(func) = callee else {
      panic!("expected function object");
    };
    let call_key_s = scope.alloc_string("call").expect("alloc key string");
    scope.push_root(Value::String(call_key_s)).unwrap();
    let call_key = PropertyKey::from_string(call_key_s);
    let call_prop = vm.get(&mut scope, func, call_key).expect("get call");
    scope.push_root(call_prop).unwrap();

    let mut argv: Vec<Value> = Vec::new();
    argv.push(this);
    argv.extend_from_slice(args);
    vm.call_with_host(&mut scope, &mut host_hooks, call_prop, callee, &argv)
  }

  fn get_array_len(heap: &mut Heap, arr: vm_js::GcObject) -> usize {
    let mut scope = heap.scope();
    let key_s = scope.alloc_string("length").expect("alloc key string");
    scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    let v = scope
      .heap()
      .object_get_own_data_property_value(arr, &key)
      .expect("get length")
      .expect("length should exist");
    let Value::Number(n) = v else {
      panic!("length should be a number");
    };
    n as usize
  }

  fn get_array_elem(heap: &mut Heap, arr: vm_js::GcObject, idx: u32) -> Value {
    let mut scope = heap.scope();
    let idx_s = scope.alloc_u32_index_string(idx).expect("alloc index string");
    scope.push_root(Value::String(idx_s)).unwrap();
    let key = PropertyKey::from_string(idx_s);
    scope
      .heap()
      .object_get_own_data_property_value(arr, &key)
      .expect("get array element")
      .unwrap_or(Value::Undefined)
  }

  fn string_value_to_utf8_lossy(heap: &Heap, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    heap
      .get_string(s)
      .expect("get string")
      .to_utf8_lossy()
  }

  fn alloc_string_value(heap: &mut Heap, value: &str) -> Value {
    let mut scope = heap.scope();
    let s = scope.alloc_string(value).expect("alloc string");
    Value::String(s)
  }

  fn alloc_string_values(heap: &mut Heap, values: &[&str]) -> Vec<Value> {
    let mut scope = heap.scope();
    let mut out = Vec::with_capacity(values.len());
    for v in values {
      let s = scope.alloc_string(v).expect("alloc string");
      // Root each string across subsequent allocations in this helper.
      scope.push_root(Value::String(s)).expect("push root");
      out.push(Value::String(s));
    }
    out
  }

  #[test]
  fn performance_entries_are_sorted_by_start_time() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::default(),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let perf_mark = get_object_property(&mut heap, performance_obj, "mark");
    let perf_get_entries = get_object_property(&mut heap, performance_obj, "getEntries");
    let perf_get_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");
    let perf_get_by_name = get_object_property(&mut heap, performance_obj, "getEntriesByName");

    // Insert marks with non-monotonic `startTime` (rewind the virtual clock).
    clock.set_now(Duration::from_millis(30));
    let arg_late = alloc_string_value(&mut heap, "late");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_late],
    );

    clock.set_now(Duration::from_millis(10));
    let arg_early1 = alloc_string_value(&mut heap, "early1");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_early1],
    );
    // Same timestamp to validate stable ordering.
    let arg_early2 = alloc_string_value(&mut heap, "early2");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_early2],
    );

    // Duplicate names out of order to validate `getEntriesByName` ordering.
    clock.set_now(Duration::from_millis(25));
    let arg_dup = alloc_string_value(&mut heap, "dup");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_dup],
    );
    clock.set_now(Duration::from_millis(5));
    let arg_dup2 = alloc_string_value(&mut heap, "dup");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_dup2],
    );

    // getEntriesByType('mark') should be sorted by startTime, stable for equal startTime.
    let arg_mark = alloc_string_value(&mut heap, "mark");
    let marks = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_mark],
    );
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks_arr), 5);

    let entry0 = get_array_elem(&mut heap, marks_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let name0 = get_object_property(&mut heap, entry0_obj, "name");
    assert_eq!(string_value_to_utf8_lossy(&heap, name0), "dup");
    let start0 = get_object_property(&mut heap, entry0_obj, "startTime");
    assert_eq!(start0, Value::Number(5.0));

    let entry1 = get_array_elem(&mut heap, marks_arr, 1);
    let Value::Object(entry1_obj) = entry1 else {
      panic!("expected entry object");
    };
    let name1 = get_object_property(&mut heap, entry1_obj, "name");
    assert_eq!(string_value_to_utf8_lossy(&heap, name1), "early1");
    let start1 = get_object_property(&mut heap, entry1_obj, "startTime");
    assert_eq!(start1, Value::Number(10.0));

    let entry2 = get_array_elem(&mut heap, marks_arr, 2);
    let Value::Object(entry2_obj) = entry2 else {
      panic!("expected entry object");
    };
    let name2 = get_object_property(&mut heap, entry2_obj, "name");
    assert_eq!(string_value_to_utf8_lossy(&heap, name2), "early2");
    let start2 = get_object_property(&mut heap, entry2_obj, "startTime");
    assert_eq!(start2, Value::Number(10.0));

    let entry3 = get_array_elem(&mut heap, marks_arr, 3);
    let Value::Object(entry3_obj) = entry3 else {
      panic!("expected entry object");
    };
    let start3 = get_object_property(&mut heap, entry3_obj, "startTime");
    assert_eq!(start3, Value::Number(25.0));

    let entry4 = get_array_elem(&mut heap, marks_arr, 4);
    let Value::Object(entry4_obj) = entry4 else {
      panic!("expected entry object");
    };
    let name4 = get_object_property(&mut heap, entry4_obj, "name");
    assert_eq!(string_value_to_utf8_lossy(&heap, name4), "late");
    let start4 = get_object_property(&mut heap, entry4_obj, "startTime");
    assert_eq!(start4, Value::Number(30.0));

    // getEntries() includes navigation (startTime=0) and must remain sorted overall.
    let all_entries = call0(
      &mut vm,
      &mut heap,
      perf_get_entries,
      Value::Object(performance_obj),
    );
    let Value::Object(all_arr) = all_entries else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, all_arr), 6);
    let e0 = get_array_elem(&mut heap, all_arr, 0);
    let Value::Object(e0_obj) = e0 else {
      panic!("expected entry object");
    };
    let entry_type0 = get_object_property(&mut heap, e0_obj, "entryType");
    assert_eq!(string_value_to_utf8_lossy(&heap, entry_type0), "navigation");
    let start_time0 = get_object_property(&mut heap, e0_obj, "startTime");
    assert_eq!(start_time0, Value::Number(0.0));

    let e1 = get_array_elem(&mut heap, all_arr, 1);
    let Value::Object(e1_obj) = e1 else {
      panic!("expected entry object");
    };
    let start_time1 = get_object_property(&mut heap, e1_obj, "startTime");
    assert_eq!(start_time1, Value::Number(5.0));

    // getEntriesByName('dup') must also be startTime-sorted.
    let args = alloc_string_values(&mut heap, &["dup"]);
    let dup_entries = call(
      &mut vm,
      &mut heap,
      perf_get_by_name,
      Value::Object(performance_obj),
      &args,
    );
    let Value::Object(dup_arr) = dup_entries else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, dup_arr), 2);
    let d0 = get_array_elem(&mut heap, dup_arr, 0);
    let Value::Object(d0_obj) = d0 else {
      panic!("expected entry object");
    };
    let d0_start = get_object_property(&mut heap, d0_obj, "startTime");
    assert_eq!(d0_start, Value::Number(5.0));
    let d1 = get_array_elem(&mut heap, dup_arr, 1);
    let Value::Object(d1_obj) = d1 else {
      panic!("expected entry object");
    };
    let d1_start = get_object_property(&mut heap, d1_obj, "startTime");
    assert_eq!(d1_start, Value::Number(25.0));

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_legacy_to_json_and_stubs_are_available() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::default(),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    // performance.setResourceTimingBufferSize exists and does not throw.
    let set_buf =
      get_object_property(&mut heap, performance_obj, "setResourceTimingBufferSize");
    {
      let scope = heap.scope();
      assert!(
        scope.heap().is_callable(set_buf).unwrap_or(false),
        "expected setResourceTimingBufferSize to be callable"
      );
    }
    let _ = call(
      &mut vm,
      &mut heap,
      set_buf,
      Value::Object(performance_obj),
      &[Value::Number(123.0)],
    );

    // performance.interactionCount is a deterministic read-only number (0).
    let interaction_count = get_object_property(&mut heap, performance_obj, "interactionCount");
    assert_eq!(interaction_count, Value::Number(0.0));

    // performance.timing.toJSON() exists and returns a plain object with numeric fields.
    let timing = get_object_property(&mut heap, performance_obj, "timing");
    let timing_obj = match timing {
      Value::Object(o) => o,
      _ => panic!("performance.timing should be an object"),
    };
    let timing_to_json = get_object_property(&mut heap, timing_obj, "toJSON");
    {
      let scope = heap.scope();
      assert!(
        scope.heap().is_callable(timing_to_json).unwrap_or(false),
        "expected performance.timing.toJSON to be callable"
      );
    }
    let timing_json =
      call0(&mut vm, &mut heap, timing_to_json, Value::Object(timing_obj));
    let Value::Object(timing_json_obj) = timing_json else {
      panic!("expected toJSON() to return an object");
    };
    for field in ["navigationStart", "fetchStart", "domComplete", "loadEventEnd"] {
      let v = get_object_property(&mut heap, timing_json_obj, field);
      let Value::Number(n) = v else {
        panic!("expected timing.toJSON().{field} to be a number");
      };
      assert!(n.is_finite(), "expected {field} to be finite");
    }

    // performance.navigation.toJSON() exists and returns { type, redirectCount }.
    let navigation = get_object_property(&mut heap, performance_obj, "navigation");
    let navigation_obj = match navigation {
      Value::Object(o) => o,
      _ => panic!("performance.navigation should be an object"),
    };
    let navigation_to_json = get_object_property(&mut heap, navigation_obj, "toJSON");
    {
      let scope = heap.scope();
      assert!(
        scope.heap().is_callable(navigation_to_json).unwrap_or(false),
        "expected performance.navigation.toJSON to be callable"
      );
    }
    let nav_json =
      call0(&mut vm, &mut heap, navigation_to_json, Value::Object(navigation_obj));
    let Value::Object(nav_json_obj) = nav_json else {
      panic!("expected navigation.toJSON() to return an object");
    };
    for field in ["type", "redirectCount"] {
      let v = get_object_property(&mut heap, nav_json_obj, field);
      let Value::Number(n) = v else {
        panic!("expected navigation.toJSON().{field} to be a number");
      };
      assert!(n.is_finite(), "expected {field} to be finite");
    }

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_entries_reject_overlong_names() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::default(),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };
    let perf_mark = get_object_property(&mut heap, performance_obj, "mark");
    let perf_get_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");

    // Call `performance.mark` with a name exceeding the hard cap. This should fail and should not
    // store an entry (prevents attacker-controlled memory growth).
    let too_long_name: String =
      std::iter::repeat('a').take(MAX_PERFORMANCE_ENTRY_NAME_CODE_UNITS + 1).collect();
    let arg_too_long = alloc_string_value(&mut heap, &too_long_name);
    let res = call_result(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_too_long],
    );
    assert!(res.is_err(), "expected overlong performance.mark to error");

    let arg_mark = alloc_string_value(&mut heap, "mark");
    let marks = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_mark],
    );
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks_arr), 0);

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_measure_can_reference_performance_timing_fields() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::default(),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    // Override one of the `performance.timing` fields to ensure `performance.measure` can treat it
    // as a start mark (many analytics snippets use `fetchStart` without explicitly marking it).
    let timing = get_object_property(&mut heap, performance_obj, "timing");
    let timing_obj = match timing {
      Value::Object(o) => o,
      _ => panic!("performance.timing should be an object"),
    };
    {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(timing_obj)).unwrap();
      let key_s = scope.alloc_string("fetchStart").expect("alloc fetchStart");
      scope.push_root(Value::String(key_s)).unwrap();
      let key = PropertyKey::from_string(key_s);
      scope
        .define_property(timing_obj, key, readonly_num_desc(10.0))
        .expect("define fetchStart");
    }

    let perf_mark = get_object_property(&mut heap, performance_obj, "mark");
    let perf_measure = get_object_property(&mut heap, performance_obj, "measure");
    let perf_get_by_name = get_object_property(&mut heap, performance_obj, "getEntriesByName");

    // `mark("self-tti")` at t=50ms.
    clock.set_now(Duration::from_millis(50));
    let arg_self_tti = alloc_string_value(&mut heap, "self-tti");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_self_tti],
    );

    // `measure("tti", "fetchStart", "self-tti")` should interpret `fetchStart` via
    // `performance.timing.fetchStart` even if there is no user mark with that name.
    clock.set_now(Duration::from_millis(60));
    let args = alloc_string_values(&mut heap, &["tti", "fetchStart", "self-tti"]);
    let _ = call(
      &mut vm,
      &mut heap,
      perf_measure,
      Value::Object(performance_obj),
      &args,
    );

    // Expect duration to be `endMark(50ms) - fetchStart(10ms) == 40ms`.
    let args = alloc_string_values(&mut heap, &["tti", "measure"]);
    let measures = call(
      &mut vm,
      &mut heap,
      perf_get_by_name,
      Value::Object(performance_obj),
      &args,
    );
    let Value::Object(measures_arr) = measures else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, measures_arr), 1);
    let entry0 = get_array_elem(&mut heap, measures_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let entry_type = get_object_property(&mut heap, entry0_obj, "entryType");
    assert_eq!(string_value_to_utf8_lossy(&heap, entry_type), "measure");
    let duration = get_object_property(&mut heap, entry0_obj, "duration");
    let Value::Number(duration) = duration else {
      panic!("expected duration number");
    };
    assert!((duration - 40.0).abs() < 1e-9, "unexpected duration {duration}");

    realm.teardown(&mut heap);
  }

  #[test]
  fn date_now_and_performance_now_follow_virtual_clock() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let event_loop = EventLoop::<()>::with_clock(clock_for_loop);

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let web_time = WebTime::new(1_000);
    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
      .expect("install time bindings");

    // Start at a deterministic time.
    clock.set_now(Duration::from_millis(0));
    assert_eq!(event_loop.now(), Duration::from_millis(0));

    let date = get_global_property(&mut heap, &realm, "Date");
    let performance = get_global_property(&mut heap, &realm, "performance");

    let date_obj = match date {
      Value::Object(o) => o,
      _ => panic!("Date should be an object"),
    };
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let date_now = get_object_property(&mut heap, date_obj, "now");
    let perf_now = get_object_property(&mut heap, performance_obj, "now");
    let time_origin = get_object_property(&mut heap, performance_obj, "timeOrigin");
    let timing = get_object_property(&mut heap, performance_obj, "timing");

    assert_eq!(
      time_origin,
      Value::Number(web_time.time_origin_unix_ms as f64),
      "performance.timeOrigin should reflect WebTime origin"
    );

    let timing_obj = match timing {
      Value::Object(o) => o,
      _ => panic!("performance.timing should be an object"),
    };
    let navigation_start = get_object_property(&mut heap, timing_obj, "navigationStart");
    let Value::Number(_) = navigation_start else {
      panic!("performance.timing.navigationStart should be a number");
    };
    assert_eq!(
      navigation_start, time_origin,
      "performance.timing.navigationStart should match performance.timeOrigin"
    );

    let v = call0(&mut vm, &mut heap, date_now, Value::Object(date_obj));
    assert_eq!(
      v,
      Value::Number(web_time.date_now(&event_loop) as f64),
      "Date.now should incorporate WebTime origin + EventLoop clock"
    );

    let v = call0(&mut vm, &mut heap, perf_now, Value::Object(performance_obj));
    assert_eq!(
      v,
      Value::Number(web_time.performance_now(&event_loop)),
      "performance.now should track EventLoop clock"
    );

    // Advance the virtual clock to a non-integer millisecond.
    clock.set_now(Duration::from_nanos(1_234_567_890)); // 1234.56789ms
    assert_eq!(event_loop.now(), Duration::from_nanos(1_234_567_890));

    let v = call0(&mut vm, &mut heap, date_now, Value::Object(date_obj));
    // Date.now() is millisecond-granularity.
    assert_eq!(v, Value::Number(web_time.date_now(&event_loop) as f64));

    let v = call0(&mut vm, &mut heap, perf_now, Value::Object(performance_obj));
    let Value::Number(n) = v else {
      panic!("performance.now should return a number");
    };
    assert!((n - web_time.performance_now(&event_loop)).abs() < 1e-9);

    // `vm-js` realms own persistent GC roots that must be explicitly removed.
    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_interaction_count_is_stubbed() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings =
      install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, WebTime::default())
        .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    // `'interactionCount' in performance` should be true.
    {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(performance_obj)).unwrap();
      let key_s = scope
        .alloc_string("interactionCount")
        .expect("alloc interactionCount");
      scope.push_root(Value::String(key_s)).unwrap();
      let key = PropertyKey::from_string(key_s);
      assert!(
        scope
          .heap()
          .has_property(performance_obj, &key)
          .expect("has_property"),
        "expected 'interactionCount' in performance to be true"
      );
    }

    // It should be a deterministic constant.
    assert_eq!(
      get_object_property(&mut heap, performance_obj, "interactionCount"),
      Value::Number(0.0)
    );

    // Sloppy-mode assignment should not change it.
    {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(performance_obj)).unwrap();
      let key_s = scope
        .alloc_string("interactionCount")
        .expect("alloc interactionCount");
      scope.push_root(Value::String(key_s)).unwrap();
      let key = PropertyKey::from_string(key_s);
      let set_ok = scope
        .ordinary_set(
          &mut vm,
          performance_obj,
          key,
          Value::Number(10.0),
          Value::Object(performance_obj),
        )
        .expect("ordinary_set");
      assert!(
        !set_ok,
        "expected assigning to performance.interactionCount to fail"
      );
    }

    assert_eq!(
      get_object_property(&mut heap, performance_obj, "interactionCount"),
      Value::Number(0.0)
    );

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_mark_and_entries_are_available() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, WebTime::default())
      .expect("install time bindings");

    clock.set_now(Duration::from_millis(10));

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let mark = get_object_property(&mut heap, performance_obj, "mark");
    let get_entries_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");
    let clear_marks = get_object_property(&mut heap, performance_obj, "clearMarks");

    // mark('foo') should not throw.
    {
      let mut scope = heap.scope();
      let foo = Value::String(scope.alloc_string("foo").expect("alloc foo"));
      drop(scope);
      let _ = call(&mut vm, &mut heap, mark, Value::Object(performance_obj), &[foo]);
    }

    // getEntriesByType('mark') should return an Array containing the mark.
    let marks = {
      let mut scope = heap.scope();
      let mark_str = Value::String(scope.alloc_string("mark").expect("alloc mark"));
      drop(scope);
      call(
        &mut vm,
        &mut heap,
        get_entries_by_type,
        Value::Object(performance_obj),
        &[mark_str],
      )
    };
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert!(
      heap.object_is_array(marks_arr).expect("object_is_array"),
      "expected getEntriesByType to return an array"
    );
    assert_eq!(get_array_len(&mut heap, marks_arr), 1);
    let entry0 = get_array_elem(&mut heap, marks_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let entry_type = get_object_property(&mut heap, entry0_obj, "entryType");
    assert_eq!(string_value_to_utf8_lossy(&heap, entry_type), "mark");

    // clearMarks() should remove it.
    let _ = call(&mut vm, &mut heap, clear_marks, Value::Object(performance_obj), &[]);

    let marks2 = {
      let mut scope = heap.scope();
      let mark_str = Value::String(scope.alloc_string("mark").expect("alloc mark"));
      drop(scope);
      call(
        &mut vm,
        &mut heap,
        get_entries_by_type,
        Value::Object(performance_obj),
        &[mark_str],
      )
    };
    let Value::Object(marks2_arr) = marks2 else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks2_arr), 0);

    // getEntriesByType('navigation') should return an array (possibly empty) without throwing.
    let nav_entries = {
      let mut scope = heap.scope();
      let nav_str = Value::String(scope.alloc_string("navigation").expect("alloc navigation"));
      drop(scope);
      call(
        &mut vm,
        &mut heap,
        get_entries_by_type,
        Value::Object(performance_obj),
        &[nav_str],
      )
    };
    let Value::Object(nav_arr) = nav_entries else {
      panic!("expected array");
    };
    assert!(
      heap.object_is_array(nav_arr).expect("object_is_array"),
      "expected navigation entries to be an array"
    );

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_navigation_entries_are_usable() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings =
      install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, WebTime::default())
        .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let get_entries_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");

    let nav_entries = {
      let mut scope = heap.scope();
      let nav_str = Value::String(scope.alloc_string("navigation").expect("alloc navigation"));
      drop(scope);
      call(
        &mut vm,
        &mut heap,
        get_entries_by_type,
        Value::Object(performance_obj),
        &[nav_str],
      )
    };

    // Assert `Array.isArray(...)` and `length === 1`.
    let array = get_global_property(&mut heap, &realm, "Array");
    let array_obj = match array {
      Value::Object(o) => o,
      _ => panic!("Array should be an object"),
    };
    let is_array = get_object_property(&mut heap, array_obj, "isArray");
    let is_arr = call(
      &mut vm,
      &mut heap,
      is_array,
      Value::Object(array_obj),
      &[nav_entries],
    );
    assert_eq!(is_arr, Value::Bool(true));

    let Value::Object(nav_arr) = nav_entries else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, nav_arr), 1);

    let entry0 = get_array_elem(&mut heap, nav_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected navigation entry object");
    };

    let entry_type = get_object_property(&mut heap, entry0_obj, "entryType");
    assert_eq!(string_value_to_utf8_lossy(&heap, entry_type), "navigation");
    let nav_type = get_object_property(&mut heap, entry0_obj, "type");
    assert_eq!(string_value_to_utf8_lossy(&heap, nav_type), "navigate");

    for field in [
      "domInteractive",
      "domContentLoadedEventStart",
      "domComplete",
      "loadEventEnd",
      "responseStart",
      "responseEnd",
      "fetchStart",
      "requestStart",
    ] {
      let v = get_object_property(&mut heap, entry0_obj, field);
      let Value::Number(n) = v else {
        panic!("expected {field} to be a number");
      };
      assert!(n.is_finite(), "expected {field} to be finite");
    }

    // Overwrite `performance.timing.domInteractive` and ensure the navigation entry reflects the
    // offset relative to `navigationStart`.
    let timing = get_object_property(&mut heap, performance_obj, "timing");
    let timing_obj = match timing {
      Value::Object(o) => o,
      _ => panic!("performance.timing should be an object"),
    };
    let nav_start = get_object_property(&mut heap, timing_obj, "navigationStart");
    let Value::Number(nav_start_ms) = nav_start else {
      panic!("navigationStart should be a number");
    };

    {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(timing_obj)).unwrap();
      let key_s = scope.alloc_string("domInteractive").expect("alloc domInteractive");
      scope.push_root(Value::String(key_s)).unwrap();
      let key = PropertyKey::from_string(key_s);
      scope
        .define_property(timing_obj, key, readonly_num_desc(nav_start_ms + 123.0))
        .expect("define domInteractive");
    }

    let nav_entries2 = {
      let mut scope = heap.scope();
      let nav_str = Value::String(scope.alloc_string("navigation").expect("alloc navigation"));
      drop(scope);
      call(
        &mut vm,
        &mut heap,
        get_entries_by_type,
        Value::Object(performance_obj),
        &[nav_str],
      )
    };
    let Value::Object(nav_arr2) = nav_entries2 else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, nav_arr2), 1);
    let entry0 = get_array_elem(&mut heap, nav_arr2, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected navigation entry object");
    };
    let dom_interactive = get_object_property(&mut heap, entry0_obj, "domInteractive");
    let Value::Number(dom_interactive) = dom_interactive else {
      panic!("expected domInteractive to be number");
    };
    assert!(
      (dom_interactive - 123.0).abs() < 1e-9,
      "unexpected domInteractive offset {dom_interactive}"
    );

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_get_entries_by_type_is_sorted_by_start_time() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings =
      install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, WebTime::default())
        .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let perf_mark = get_object_property(&mut heap, performance_obj, "mark");
    let perf_get_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");

    // Create marks out of insertion order by moving the clock backwards.
    clock.set_now(Duration::from_millis(50));
    let arg_late = alloc_string_value(&mut heap, "late");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_late],
    );

    clock.set_now(Duration::from_millis(10));
    let arg_early = alloc_string_value(&mut heap, "early");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_early],
    );

    let arg_mark = alloc_string_value(&mut heap, "mark");
    let marks = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_mark],
    );
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks_arr), 2);

    let entry0 = get_array_elem(&mut heap, marks_arr, 0);
    let entry1 = get_array_elem(&mut heap, marks_arr, 1);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let Value::Object(entry1_obj) = entry1 else {
      panic!("expected entry object");
    };

    let name0 = get_object_property(&mut heap, entry0_obj, "name");
    let name1 = get_object_property(&mut heap, entry1_obj, "name");
    assert_eq!(string_value_to_utf8_lossy(&heap, name0), "early");
    assert_eq!(string_value_to_utf8_lossy(&heap, name1), "late");

    let start0 = get_object_property(&mut heap, entry0_obj, "startTime");
    let start1 = get_object_property(&mut heap, entry1_obj, "startTime");
    let Value::Number(start0) = start0 else {
      panic!("expected startTime number");
    };
    let Value::Number(start1) = start1 else {
      panic!("expected startTime number");
    };
    assert!((start0 - 10.0).abs() < 1e-9, "unexpected startTime {start0}");
    assert!((start1 - 50.0).abs() < 1e-9, "unexpected startTime {start1}");
    assert!(start0 <= start1, "expected chronological ordering");

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_mark_measure_get_entries_and_clears_work() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::default(),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let perf_mark = get_object_property(&mut heap, performance_obj, "mark");
    let perf_measure = get_object_property(&mut heap, performance_obj, "measure");
    let perf_get_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");
    let perf_get_by_name = get_object_property(&mut heap, performance_obj, "getEntriesByName");
    let perf_clear_marks = get_object_property(&mut heap, performance_obj, "clearMarks");
    let perf_clear_measures = get_object_property(&mut heap, performance_obj, "clearMeasures");

    // performance.mark("a") should not throw.
    clock.set_now(Duration::from_millis(10));
    let arg_a = alloc_string_value(&mut heap, "a");
    let _ = call(
      &mut vm,
      &mut heap,
      perf_mark,
      Value::Object(performance_obj),
      &[arg_a],
    );

    // performance.measure("m", "a") should create a measure entry.
    clock.set_now(Duration::from_millis(25));
    let args = alloc_string_values(&mut heap, &["m", "a"]);
    let _ = call(
      &mut vm,
      &mut heap,
      perf_measure,
      Value::Object(performance_obj),
      &args,
    );

    // getEntriesByType('mark') contains the mark.
    let arg_mark = alloc_string_value(&mut heap, "mark");
    let marks = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_mark],
    );
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks_arr), 1);
    let entry0 = get_array_elem(&mut heap, marks_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let entry_type = get_object_property(&mut heap, entry0_obj, "entryType");
    assert_eq!(string_value_to_utf8_lossy(&heap, entry_type), "mark");

    // getEntriesByName('m', 'measure')[0].duration is finite.
    let args = alloc_string_values(&mut heap, &["m", "measure"]);
    let measures = call(
      &mut vm,
      &mut heap,
      perf_get_by_name,
      Value::Object(performance_obj),
      &args,
    );
    let Value::Object(measures_arr) = measures else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, measures_arr), 1);
    let entry0 = get_array_elem(&mut heap, measures_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let entry_type = get_object_property(&mut heap, entry0_obj, "entryType");
    assert_eq!(string_value_to_utf8_lossy(&heap, entry_type), "measure");
    let duration = get_object_property(&mut heap, entry0_obj, "duration");
    let Value::Number(duration) = duration else {
      panic!("duration should be a number");
    };
    assert!(duration.is_finite());

    // A measure using "fetchStart" as startMark should not throw and should yield a numeric duration.
    clock.set_now(Duration::from_millis(50));
    let args = alloc_string_values(&mut heap, &["mf", "fetchStart"]);
    let _ = call(
      &mut vm,
      &mut heap,
      perf_measure,
      Value::Object(performance_obj),
      &args,
    );
    let args = alloc_string_values(&mut heap, &["mf", "measure"]);
    let mf = call(
      &mut vm,
      &mut heap,
      perf_get_by_name,
      Value::Object(performance_obj),
      &args,
    );
    let Value::Object(mf_arr) = mf else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, mf_arr), 1);
    let entry0 = get_array_elem(&mut heap, mf_arr, 0);
    let Value::Object(entry0_obj) = entry0 else {
      panic!("expected entry object");
    };
    let duration = get_object_property(&mut heap, entry0_obj, "duration");
    let Value::Number(duration) = duration else {
      panic!("duration should be a number");
    };
    assert!(duration.is_finite());

    // clearMarks/clearMeasures remove entries.
    let _ = call(
      &mut vm,
      &mut heap,
      perf_clear_marks,
      Value::Object(performance_obj),
      &[],
    );
    let arg_mark = alloc_string_value(&mut heap, "mark");
    let marks = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_mark],
    );
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks_arr), 0);

    let _ = call(
      &mut vm,
      &mut heap,
      perf_clear_measures,
      Value::Object(performance_obj),
      &[],
    );
    let arg_measure = alloc_string_value(&mut heap, "measure");
    let measures = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_measure],
    );
    let Value::Object(measures_arr) = measures else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, measures_arr), 0);

    realm.teardown(&mut heap);
  }

  #[test]
  fn performance_entries_are_bounded() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::default(),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let perf_mark = get_object_property(&mut heap, performance_obj, "mark");
    let perf_get_by_type = get_object_property(&mut heap, performance_obj, "getEntriesByType");
    let perf_clear_marks = get_object_property(&mut heap, performance_obj, "clearMarks");

    let _ = call(
      &mut vm,
      &mut heap,
      perf_clear_marks,
      Value::Object(performance_obj),
      &[],
    );

    for i in 0..(MAX_PERFORMANCE_ENTRIES + 5) {
      clock.set_now(Duration::from_millis(i as u64));
      let arg_x = alloc_string_value(&mut heap, "x");
      let _ = call(
        &mut vm,
        &mut heap,
        perf_mark,
        Value::Object(performance_obj),
        &[arg_x],
      );
    }

    let arg_mark = alloc_string_value(&mut heap, "mark");
    let marks = call(
      &mut vm,
      &mut heap,
      perf_get_by_type,
      Value::Object(performance_obj),
      &[arg_mark],
    );
    let Value::Object(marks_arr) = marks else {
      panic!("expected array");
    };
    assert_eq!(get_array_len(&mut heap, marks_arr), MAX_PERFORMANCE_ENTRIES);

    realm.teardown(&mut heap);
  }

  #[test]
  fn install_time_bindings_is_idempotent_per_heap_via_timebindings_drop() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();
    let web_time = WebTime::default();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings.clone(),
      web_time,
    )
    .expect("first install_time_bindings should succeed");

    let err = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings.clone(),
      web_time,
    )
    .expect_err("second install_time_bindings should fail for the same heap");
    assert!(
      matches!(
        err,
        VmError::Unimplemented(msg)
          if msg == "install_time_bindings called more than once for the same heap"
      ),
      "unexpected error: {err:?}"
    );

    // Dropping the bindings must unregister the heap mapping so another realm on the same heap
    // can install time bindings again.
    drop(bindings);

    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
      .expect("install_time_bindings after dropping the previous bindings should succeed");

    realm.teardown(&mut heap);
  }

  #[test]
  fn date_now_ms_matches_installed_time_bindings() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let web_time = WebTime::new(1_000);
    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
      .expect("install time bindings");

    clock.set_now(Duration::from_millis(2_345));
    {
      let scope = heap.scope();
      let ms = date_now_ms(&scope).expect("date_now_ms should succeed");
      assert_eq!(ms, 3_345);
    }

    realm.teardown(&mut heap);
  }

  #[test]
  fn legacy_navigation_timing_objects_have_tojson() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      WebTime::new(1_000),
    )
    .expect("install time bindings");

    let performance = get_global_property(&mut heap, &realm, "performance");
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    // --- performance.timing.toJSON ---
    let timing = get_object_property(&mut heap, performance_obj, "timing");
    let timing_obj = match timing {
      Value::Object(o) => o,
      _ => panic!("performance.timing should be an object"),
    };

    let timing_to_json = get_object_property(&mut heap, timing_obj, "toJSON");
    assert!(
      heap
        .is_callable(timing_to_json)
        .expect("is_callable should succeed"),
      "expected typeof performance.timing.toJSON === 'function'"
    );

    let timing_nav_start = get_object_property(&mut heap, timing_obj, "navigationStart");
    let timing_json = call0(
      &mut vm,
      &mut heap,
      timing_to_json,
      Value::Object(timing_obj),
    );
    let Value::Object(timing_json_obj) = timing_json else {
      panic!("expected performance.timing.toJSON() to return an object");
    };
    let timing_json_nav_start = get_object_property(&mut heap, timing_json_obj, "navigationStart");
    assert_eq!(
      timing_json_nav_start, timing_nav_start,
      "performance.timing.toJSON().navigationStart should match performance.timing.navigationStart"
    );

    // JSON.stringify(performance.timing) should include legacy timing fields via toJSON.
    let json = get_global_property(&mut heap, &realm, "JSON");
    let json_obj = match json {
      Value::Object(o) => o,
      _ => panic!("JSON should be an object"),
    };
    let stringify = get_object_property(&mut heap, json_obj, "stringify");
    let s = call(
      &mut vm,
      &mut heap,
      stringify,
      Value::Object(json_obj),
      &[Value::Object(timing_obj)],
    );
    let s_utf8 = string_value_to_utf8_lossy(&heap, s);
    assert!(
      s_utf8.contains("navigationStart"),
      "expected JSON.stringify(performance.timing) to include navigationStart, got: {s_utf8}"
    );

    // --- performance.navigation.toJSON ---
    let navigation = get_object_property(&mut heap, performance_obj, "navigation");
    let navigation_obj = match navigation {
      Value::Object(o) => o,
      _ => panic!("performance.navigation should be an object"),
    };
    let navigation_to_json = get_object_property(&mut heap, navigation_obj, "toJSON");
    assert!(
      heap
        .is_callable(navigation_to_json)
        .expect("is_callable should succeed"),
      "expected typeof performance.navigation.toJSON === 'function'"
    );

    let navigation_type = get_object_property(&mut heap, navigation_obj, "type");
    let nav_json = call0(
      &mut vm,
      &mut heap,
      navigation_to_json,
      Value::Object(navigation_obj),
    );
    let Value::Object(nav_json_obj) = nav_json else {
      panic!("expected performance.navigation.toJSON() to return an object");
    };
    let nav_json_type = get_object_property(&mut heap, nav_json_obj, "type");
    assert_eq!(
      nav_json_type, navigation_type,
      "performance.navigation.toJSON().type should match performance.navigation.type"
    );

    let s = call(
      &mut vm,
      &mut heap,
      stringify,
      Value::Object(json_obj),
      &[Value::Object(navigation_obj)],
    );
    let s_utf8 = string_value_to_utf8_lossy(&heap, s);
    assert!(
      s_utf8.contains("redirectCount"),
      "expected JSON.stringify(performance.navigation) to include redirectCount, got: {s_utf8}"
    );

    realm.teardown(&mut heap);
  }
}
