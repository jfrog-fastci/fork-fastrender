use native_js::eval::Evaluator;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program, Severity};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

#[test]
fn nested_let_shadowing_resolves_correctly() {
  let key = FileKey::new("main.ts");
  let src = r#"
export function run() {
  let x = 1;
  let y = 0;
  {
    let x = 2;
    y = x;
  }
  return y + x;
}
"#;

  let mut host = es5_host();
  host.insert(key.clone(), src);
  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|diag| diag.severity != Severity::Error),
    "{diagnostics:#?}"
  );

  let file = program.file_id(&key).unwrap();
  let mut evaluator = Evaluator::new(&program);
  let value = evaluator
    .run_exported_function_i64(file, "run")
    .expect("evaluation succeeds");
  assert_eq!(value, 3);
}

#[test]
fn param_shadowing_outer_binding_resolves_correctly() {
  let key = FileKey::new("main.ts");
  let src = r#"
let x = 1;

export function f(x) {
  return x + 1;
}

export function run() {
  return f(2) + x;
}
"#;

  let mut host = es5_host();
  host.insert(key.clone(), src);
  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|diag| diag.severity != Severity::Error),
    "{diagnostics:#?}"
  );

  let file = program.file_id(&key).unwrap();
  let mut evaluator = Evaluator::new(&program);
  let value = evaluator
    .run_exported_function_i64(file, "run")
    .expect("evaluation succeeds");
  assert_eq!(value, 4);
}

#[test]
fn import_binding_and_local_shadowing_resolve_correctly() {
  let a_key = FileKey::new("a.ts");
  let b_key = FileKey::new("b.ts");

  let a_src = r#"
export const x = 10;
"#;

  let b_src = r#"
import { x } from "./a.ts";

export function run() {
  let y = x;
  {
    let x = 2;
    y += x;
  }
  return y;
}
"#;

  let mut host = es5_host();
  host.insert(a_key.clone(), a_src);
  host.insert(b_key.clone(), b_src);
  host.link(b_key.clone(), "./a.ts", a_key.clone());

  let program = Program::new(host, vec![b_key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|diag| diag.severity != Severity::Error),
    "{diagnostics:#?}"
  );

  let file = program.file_id(&b_key).unwrap();
  let mut evaluator = Evaluator::new(&program);
  let value = evaluator
    .run_exported_function_i64(file, "run")
    .expect("evaluation succeeds");
  assert_eq!(value, 12);
}

#[test]
fn import_from_reexport_module_resolves_correctly() {
  let a_key = FileKey::new("a.ts");
  let b_key = FileKey::new("b.ts");
  let main_key = FileKey::new("main.ts");

  let a_src = r#"
export const x = 10;
"#;

  let b_src = r#"
export { x } from "./a.ts";
"#;

  let main_src = r#"
import { x } from "./b.ts";
export function run() {
  return x;
}
"#;

  let mut host = es5_host();
  host.insert(a_key.clone(), a_src);
  host.insert(b_key.clone(), b_src);
  host.insert(main_key.clone(), main_src);
  host.link(b_key.clone(), "./a.ts", a_key.clone());
  host.link(main_key.clone(), "./b.ts", b_key.clone());

  let program = Program::new(host, vec![main_key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|diag| diag.severity != Severity::Error),
    "{diagnostics:#?}"
  );

  let file = program.file_id(&main_key).unwrap();
  let mut evaluator = Evaluator::new(&program);
  let value = evaluator
    .run_exported_function_i64(file, "run")
    .expect("evaluation succeeds");
  assert_eq!(value, 10);
}

#[test]
fn import_from_export_all_reexport_module_resolves_correctly() {
  let a_key = FileKey::new("a.ts");
  let b_key = FileKey::new("b.ts");
  let main_key = FileKey::new("main.ts");

  let a_src = r#"
export const x = 10;
"#;

  let b_src = r#"
export * from "./a.ts";
"#;

  let main_src = r#"
import { x } from "./b.ts";
export function run() {
  return x;
}
"#;

  let mut host = es5_host();
  host.insert(a_key.clone(), a_src);
  host.insert(b_key.clone(), b_src);
  host.insert(main_key.clone(), main_src);
  host.link(b_key.clone(), "./a.ts", a_key.clone());
  host.link(main_key.clone(), "./b.ts", b_key.clone());

  let program = Program::new(host, vec![main_key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|diag| diag.severity != Severity::Error),
    "{diagnostics:#?}"
  );

  let file = program.file_id(&main_key).unwrap();
  let mut evaluator = Evaluator::new(&program);
  let value = evaluator
    .run_exported_function_i64(file, "run")
    .expect("evaluation succeeds");
  assert_eq!(value, 10);
}
