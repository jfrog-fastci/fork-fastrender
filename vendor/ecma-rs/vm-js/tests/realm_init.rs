use std::collections::HashMap;
use vm_js::{Heap, HeapLimits, PropertyKey, Realm, Vm, VmError, VmOptions};

#[test]
fn realm_init_does_not_install_duplicate_global_property_names() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let global = realm.global_object();
  let keys = heap.own_property_keys(global)?;

  let mut counts: HashMap<Vec<u16>, usize> = HashMap::new();
  for key in keys {
    let PropertyKey::String(s) = key else {
      continue;
    };
    let units = heap.get_string(s)?.as_code_units().to_vec();
    *counts.entry(units).or_default() += 1;
  }

  let dupes: Vec<String> = counts
    .iter()
    .filter_map(|(units, &count)| {
      if count <= 1 {
        return None;
      }
      Some(format!("{} (x{count})", String::from_utf16_lossy(units)))
    })
    .collect();
  assert!(
    dupes.is_empty(),
    "duplicate globalThis own property names: {dupes:?}"
  );

  // Sanity check for the original regression: "Proxy" should exist and should not appear twice.
  let proxy_units: Vec<u16> = "Proxy".encode_utf16().collect();
  assert_eq!(counts.get(&proxy_units).copied().unwrap_or(0), 1);

  // Avoid leaking persistent roots (and tripping the Realm drop assertion).
  realm.teardown(&mut heap);
  Ok(())
}

