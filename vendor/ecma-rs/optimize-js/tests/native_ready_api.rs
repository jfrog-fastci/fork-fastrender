#![cfg(feature = "typed")]

use optimize_js::il::inst::{Inst, InstTyp, OwnershipState};
use optimize_js::{compile_file_native_ready, NativeReadyOptions, TopLevelMode};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};

fn build_type_program(source: &str) -> (Arc<typecheck_ts::Program>, typecheck_ts::FileId) {
  let mut host = typecheck_ts::MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  });
  let file = typecheck_ts::FileKey::new("input.ts");
  host.insert(file.clone(), source);
  let program = Arc::new(typecheck_ts::Program::new(host, vec![file.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected typecheck to succeed; diagnostics: {diagnostics:?}"
  );
  let file_id = program.file_id(&file).expect("typecheck file id");
  (program, file_id)
}

fn any_inst(program: &optimize_js::Program, pred: impl Fn(&Inst) -> bool) -> bool {
  let scan_cfg = |cfg: &optimize_js::cfg::cfg::Cfg| {
    for (_label, block) in cfg.bblocks.all() {
      for inst in block.iter() {
        if pred(inst) {
          return true;
        }
      }
    }
    false
  };

  if scan_cfg(&program.top_level.body) {
    return true;
  }
  for func in &program.functions {
    if scan_cfg(&func.body) {
      return true;
    }
  }
  false
}

#[test]
fn native_ready_retains_ssa_phi_nodes() {
  let source = r#"
    declare function unknown_cond(): boolean;
    declare function side_effect_true(): void;
    declare function side_effect_false(): void;
    declare function unknown_func(x: number): void;
    let x = 0;
    if (unknown_cond()) {
      side_effect_true();
      x = 1;
    } else {
      side_effect_false();
      x = 2;
    }
    unknown_func(x);
  "#;

  let (tc_program, file_id) = build_type_program(source);
  let native = compile_file_native_ready(
    tc_program,
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions::default(),
  )
  .expect("compile native-ready");

  assert!(
    any_inst(&native.program, |inst| inst.t == InstTyp::Phi),
    "expected at least one Phi instruction in returned CFGs"
  );
}

#[test]
fn native_ready_populates_inst_meta_escape_and_ownership() {
  let source = r#"
    declare function sink(x: unknown): void;
    const obj = { x: 1 };
    sink(obj);
  "#;

  let (tc_program, file_id) = build_type_program(source);
  let native = compile_file_native_ready(
    tc_program,
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions::default(),
  )
  .expect("compile native-ready");

  assert!(
    any_inst(&native.program, |inst| inst.meta.result_escape.is_some()),
    "expected at least one instruction to have InstMeta.result_escape"
  );
  assert!(
    any_inst(&native.program, |inst| inst.meta.ownership != OwnershipState::Unknown),
    "expected at least one instruction to have non-default InstMeta.ownership"
  );
}

#[cfg(feature = "serde")]
#[test]
fn native_ready_program_analyses_are_deterministic() {
  let source = r#"
    declare function unknown_cond(): boolean;
    declare function side_effect_true(): void;
    declare function side_effect_false(): void;
    declare function unknown_func(x: number): void;
    let x = 0;
    if (unknown_cond()) {
      side_effect_true();
      x = 1;
    } else {
      side_effect_false();
      x = 2;
    }
    unknown_func(x);
  "#;

  let (tc_program, file_id) = build_type_program(source);

  let first = compile_file_native_ready(
    Arc::clone(&tc_program),
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions {
      run_opt_passes: true,
    },
  )
  .expect("compile native-ready");
  let second = compile_file_native_ready(
    Arc::clone(&tc_program),
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions {
      run_opt_passes: true,
    },
  )
  .expect("compile native-ready");

  let first_json = serde_json::to_string(&first.analyses).expect("serialize analyses");
  let second_json = serde_json::to_string(&second.analyses).expect("serialize analyses");
  assert_eq!(
    first_json, second_json,
    "ProgramAnalyses serialization should be deterministic"
  );
}
