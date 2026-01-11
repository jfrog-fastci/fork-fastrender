use inkwell::context::Context;
use inkwell::targets::{InitializationConfig, Target};
use inkwell::OptimizationLevel;
use native_js::{codegen, strict};
use std::collections::HashMap;
use std::sync::{Arc, Once};
use typecheck_ts::lib_support::FileKind;
use typecheck_ts::{FileKey, Host, HostError, Program};

#[derive(Clone, Default)]
struct TestHost {
  files: HashMap<FileKey, Arc<str>>,
  kinds: HashMap<FileKey, FileKind>,
}

impl TestHost {
  fn insert(&mut self, key: FileKey, kind: FileKind, source: &str) {
    self.files.insert(key.clone(), Arc::from(source.to_string()));
    self.kinds.insert(key, kind);
  }
}

impl Host for TestHost {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    self
      .files
      .get(file)
      .cloned()
      .ok_or_else(|| HostError::new(format!("missing file {file:?}")))
  }

  fn resolve(&self, _from: &FileKey, _specifier: &str) -> Option<FileKey> {
    None
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    self.kinds.get(file).copied().unwrap_or(FileKind::Ts)
  }
}

fn run_main(source: &str) -> i32 {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    Target::initialize_native(&InitializationConfig::default()).expect("init native target");
  });

  let mut host = TestHost::default();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), FileKind::Ts, source);

  let program = Program::new(host, vec![key.clone()]);
  let tc_diags = program.check();
  assert!(
    tc_diags.is_empty(),
    "expected sample to typecheck cleanly, got: {tc_diags:#?}"
  );

  let file = program.file_id(&key).expect("file id");
  let strict_diags = strict::validate(&program, &[file]);
  assert!(
    strict_diags.is_empty(),
    "expected sample to pass strict validation, got: {strict_diags:#?}"
  );
  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file,
    entrypoint,
    codegen::CodegenOptions::default(),
  )
  .expect("codegen");
  if let Err(err) = module.verify() {
    panic!(
      "LLVM module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let engine = module
    .create_jit_execution_engine(OptimizationLevel::None)
    .expect("create ee");

  unsafe {
    let main = engine
      .get_function::<unsafe extern "C" fn() -> i32>("ts_main")
      .expect("get ts_main");
    main.call()
  }
}

#[test]
fn if_branches_compute_correct_value() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 0;
      if (1 < 2) {
        x = 10;
      } else {
        x = 20;
      }
      return x;
    }
    "#,
  );
  assert_eq!(value, 10);
}

#[test]
fn while_loop_sums_numbers() {
  let value = run_main(
    r#"
    export function main(): number {
      let i = 0;
      let sum = 0;
      while (i < 5) {
        sum = sum + i;
        i = i + 1;
      }
      return sum;
    }
    "#,
  );
  assert_eq!(value, 10);
}

#[test]
fn do_while_executes_at_least_once() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 0;
      do {
        x = x + 1;
      } while (0);
      return x;
    }
    "#,
  );
  assert_eq!(value, 1);
}

#[test]
fn for_loop_init_test_update() {
  let value = run_main(
    r#"
    export function main(): number {
      let sum = 0;
      for (let i = 0; i < 4; i = i + 1) {
        sum = sum + i;
      }
      return sum;
    }
    "#,
  );
  assert_eq!(value, 6);
}

#[test]
fn shadowing_in_nested_blocks() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 1;
      let y = 0;
      {
        let x = 2;
        x = x + 1;
        y = x;
      }
      return y * 10 + x;
    }
    "#,
  );
  assert_eq!(value, 31);
}

#[test]
fn break_exits_while_loop() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 0;
      while (1) {
        if (x == 3) {
          break;
        }
        x = x + 1;
      }
      return x;
    }
    "#,
  );
  assert_eq!(value, 3);
}

#[test]
fn continue_skips_for_iteration() {
  let value = run_main(
    r#"
    export function main(): number {
      let sum = 0;
      for (let i = 0; i < 5; i = i + 1) {
        if (i == 2) {
          continue;
        }
        sum = sum + i;
      }
      return sum;
    }
    "#,
  );
  assert_eq!(value, 8);
}

#[test]
fn void_main_allows_fallthrough() {
  let value = run_main(
    r#"
    export function main(): void {
      let x = 0;
      x = x + 1;
    }
    "#,
  );
  assert_eq!(value, 0);
}

#[test]
fn void_main_allows_return_without_value() {
  let value = run_main(
    r#"
    export function main(): void {
      let x = 0;
      if (1) {
        x = x + 1;
        return;
      }
      x = x + 100;
    }
    "#,
  );
  assert_eq!(value, 0);
}

#[test]
fn labeled_break_exits_outer_loop() {
  let value = run_main(
    r#"
    export function main(): number {
      let count = 0;
      outer: for (let i = 0; i < 5; i = i + 1) {
        for (let j = 0; j < 5; j = j + 1) {
          if (j == 2) {
            break outer;
          }
          count = count + 1;
        }
      }
      return count;
    }
    "#,
  );
  assert_eq!(value, 2);
}

#[test]
fn labeled_continue_jumps_outer_loop() {
  let value = run_main(
    r#"
    export function main(): number {
      let count = 0;
      outer: for (let i = 0; i < 3; i = i + 1) {
        for (let j = 0; j < 3; j = j + 1) {
          if (j == 1) {
            continue outer;
          }
          count = count + 1;
        }
      }
      return count;
    }
    "#,
  );
  assert_eq!(value, 3);
}
