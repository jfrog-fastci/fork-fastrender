use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
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
    self
      .files
      .insert(key.clone(), Arc::from(source.to_string()));
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
    native_js::llvm::init_native_target().expect("init native target");
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
  let ts_main_sym = native_js::llvm_symbol_for_def(&program, entrypoint.main_def);

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

  // JIT compilation uses the module data layout for ABI decisions (including
  // atomic alignment). Explicitly set it to the host target so loop-backedge GC
  // polls (atomic `i64` loads) are well-formed under LLVM 18's JIT.
  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("no target for default triple");
  let tm = target
    .create_target_machine(
      &triple,
      "generic",
      "",
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("failed to create target machine");
  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  // The HIR codegen inserts explicit GC polls in loop backedges. Those polls are
  // designed to call into `runtime-native` in AOT builds, but this test runs the
  // module via LLVM JIT. Provide in-module stubs so the JIT does not need to
  // resolve external symbols.
  if let Some(epoch) = module.get_global("RT_GC_EPOCH") {
    epoch.set_initializer(&context.i64_type().const_zero());
  }
  if let Some(slow) = module.get_function("rt_gc_safepoint_slow") {
    if slow.count_basic_blocks() == 0 {
      let bb = context.append_basic_block(slow, "entry");
      let b = context.create_builder();
      b.position_at_end(bb);
      b.build_return(None).expect("ret void");
    }
  }

  let engine = module
    .create_jit_execution_engine(OptimizationLevel::None)
    .expect("create ee");

  unsafe {
    let main = engine
      // native-js codegen emits a C `main` wrapper that prints the TS return value and returns an
      // exit code. For these JIT-level statement tests we want the raw TS result value, so call the
      // TS `main` function directly.
      .get_function::<unsafe extern "C" fn() -> i32>(&ts_main_sym)
      .expect("get main");
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
fn for_loop_postfix_update_op() {
  let value = run_main(
    r#"
    export function main(): number {
      let sum = 0;
      for (let i = 0; i < 4; i++) {
        sum = sum + i;
      }
      return sum;
    }
    "#,
  );
  assert_eq!(value, 6);
}

#[test]
fn for_loop_let_shadows_outer_binding_only_within_loop() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 10;
      let y = 0;
      for (let x = 0; x < 3; x = x + 1) {
        y = y + 1;
      }
      return x * 10 + y;
    }
    "#,
  );
  assert_eq!(value, 103);
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
fn logical_and_or_short_circuit() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 0;
      let y = 0;

      if (0 && (x = x + 1)) {
        y = 1000;
      }

      if (1 && (x = x + 2)) {
        y = y + 10;
      }

      if (1 || (y = y + 999)) {
        y = y + 1;
      }

      if (0 || (y = y + 3)) {
        y = y + 1;
      }

      return x * 100 + y;
    }
    "#,
  );
  assert_eq!(value, 215);
}

#[test]
fn comma_operator_evaluates_left_then_returns_right() {
  let value = run_main(
    r#"
    export function main(): number {
      let x = 0;
      let y = (x = 5, x + 1);
      return y * 10 + x;
    }
    "#,
  );
  assert_eq!(value, 65);
}

#[test]
fn unsigned_shift_right() {
  let value = run_main(
    r#"
    export function main(): number {
      return (-1) >>> 1;
    }
    "#,
  );
  assert_eq!(value, 2147483647);
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
