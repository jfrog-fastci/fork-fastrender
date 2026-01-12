#![cfg(feature = "serde")]

use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn snapshot_matches_checked_program_for_exports_and_type_at() {
  let mut host = MemoryHost::default();
  let entry = FileKey::new("entry.ts");
  let dep = FileKey::new("dep.ts");
  let entry_source = "import { add } from \"./dep\";\nexport const total = add(1, 2);\n";
  let dep_source = "export function add(a: number, b: number) { return a + b; }\n";

  host.insert(entry.clone(), Arc::from(entry_source.to_string()));
  host.insert(dep.clone(), Arc::from(dep_source.to_string()));
  host.link(entry.clone(), "./dep", dep.clone());

  let checked = Program::new(host.clone(), vec![entry.clone()]);
  let diags = checked.check();
  assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
  let entry_id = checked.file_id(&entry).expect("entry id");
  let exports = checked.exports_of(entry_id);
  let call_offset = entry_source
    .find("add(1, 2)")
    .expect("call offset") as u32;
  let ty = checked.type_at(entry_id, call_offset).expect("type at call");

  let snap_source = Program::new(host.clone(), vec![entry.clone()]);
  let snapshot = snap_source.snapshot();
  let restored = Program::from_snapshot(host, snapshot);

  let restored_entry_id = restored.file_id(&entry).expect("restored entry id");
  assert_eq!(restored.exports_of(restored_entry_id), exports);
  assert_eq!(restored.type_at(restored_entry_id, call_offset), Some(ty));
}
