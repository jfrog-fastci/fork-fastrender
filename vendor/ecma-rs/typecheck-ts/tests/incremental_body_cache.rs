use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{
  body_check_call_count, reset_body_check_call_count, FileKey, MemoryHost, Program,
};

// The body-check counter is global across the process. Rust's test harness runs tests in parallel
// by default, so guard these tests to avoid cross-talk between separate `Program` instances.
static BODY_CHECK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn stable_ids_across_unrelated_edits() {
  let _guard = BODY_CHECK_TEST_LOCK.lock().expect("test lock");
  // Disable bundled libs so the stable-id assertions only reflect the source files.
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  let file0 = FileKey::new("file0.ts");
  let file1 = FileKey::new("file1.ts");
  host.insert(
    file0.clone(),
    "import { foo } from \"./file1\";\nexport function main(): number { return foo(); }\n",
  );
  host.insert(
    file1.clone(),
    "export function foo(): number { return 1; }\n",
  );
  host.link(file0.clone(), "./file1", file1.clone());

  let mut program = Program::new(host, vec![file0.clone(), file1.clone()]);
  let _ = program.check();

  let file0_id = program.file_id(&file0).expect("file0 id");
  let file1_id = program.file_id(&file1).expect("file1 id");
  let defs_before = program.definitions_in_file(file1_id);
  let bodies_before = program.bodies_in_file(file1_id);

  program.set_file_text(
    file0_id,
    Arc::from(
      "import { foo } from \"./file1\";\nexport function main(): number { return foo(); }\n// edit\n",
    ),
  );
  let _ = program.check();

  let defs_after = program.definitions_in_file(file1_id);
  let bodies_after = program.bodies_in_file(file1_id);

  assert_eq!(defs_before, defs_after);
  assert_eq!(bodies_before, bodies_after);
}

#[test]
fn reuses_body_results_for_unchanged_files() {
  let _guard = BODY_CHECK_TEST_LOCK.lock().expect("test lock");
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  let file0 = FileKey::new("file0.ts");
  let file1 = FileKey::new("file1.ts");
  host.insert(
    file0.clone(),
    "import { foo } from \"./file1\";\nexport function main(): number { return foo(); }\n",
  );
  host.insert(
    file1.clone(),
    "export function foo(): number { return 1; }\n",
  );
  host.link(file0.clone(), "./file1", file1.clone());

  let mut program = Program::new(host, vec![file0.clone(), file1.clone()]);

  reset_body_check_call_count();
  let _ = program.check();
  assert!(body_check_call_count() > 0);

  reset_body_check_call_count();
  let _ = program.check();
  assert_eq!(body_check_call_count(), 0);

  let file0_id = program.file_id(&file0).expect("file0 id");
  let expected_rechecks = program.bodies_in_file(file0_id).len();
  assert!(expected_rechecks > 0);

  reset_body_check_call_count();
  program.set_file_text(
    file0_id,
    Arc::from(
      "import { foo } from \"./file1\";\nexport function main(): number { return foo() + 1; }\n",
    ),
  );
  let _ = program.check();
  assert_eq!(
    body_check_call_count(),
    expected_rechecks,
    "editing a leaf file should only re-check bodies owned by that file"
  );
}
