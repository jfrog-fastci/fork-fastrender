use native_oracle_harness::run_fixture_ts_module_dir;

#[test]
fn module_dir_runs_static_import_graph() {
  let dir = tempfile::tempdir().expect("create temp dir");
  std::fs::write(
    dir.path().join("dep.ts"),
    r#"
export const value = "ok";
"#,
  )
  .expect("write dep.ts");

  std::fs::write(
    dir.path().join("entry.ts"),
    r#"
import { value } from "./dep.ts";
globalThis.__native_result = "res:" + value;
"#,
  )
  .expect("write entry.ts");

  let out = run_fixture_ts_module_dir(dir.path()).expect("module fixture should run");
  assert_eq!(out, "res:ok");
}

#[test]
fn module_dir_pending_top_level_await_fails_deterministically() {
  let dir = tempfile::tempdir().expect("create temp dir");
  std::fs::write(
    dir.path().join("entry.ts"),
    r#"
await new Promise(() => {});
globalThis.__native_result = "x";
"#,
  )
  .expect("write entry.ts");

  let err = run_fixture_ts_module_dir(dir.path()).expect_err("expected deterministic TLA failure");
  assert!(err.message.contains("module evaluation promise did not settle"));
}

