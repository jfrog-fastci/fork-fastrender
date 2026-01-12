#![cfg(feature = "typed")]

use optimize_js::{compile_file_native_ready, verify_program_strict_native, NativeReadyOptions, TopLevelMode, VerifyOptions};
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst};
use optimize_js::{FileId, OptimizationStats, Program, ProgramFunction};
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

#[test]
fn forbidden_unknown_load_triggers_diagnostic() {
  let source = "var a = 1; a = a + 2;";
  let (tc_program, file_id) = build_type_program(source);

  // Compile without verifier so we can run it directly.
  let native = compile_file_native_ready(
    Arc::clone(&tc_program),
    file_id,
    TopLevelMode::Global,
    false,
    NativeReadyOptions {
      run_opt_passes: true,
      verify_strict_native: false,
      ..NativeReadyOptions::default()
    },
  )
  .expect("compile without strict-native verification");

  let err = verify_program_strict_native(
    &native.program,
    &VerifyOptions {
      file: file_id,
      ..Default::default()
    },
  )
  .expect_err("expected strict-native verifier to reject program with unknown loads");

  let unknown_load = err
    .iter()
    .find(|diag| diag.code == "OPTN0001" && diag.message.contains("UnknownLoad"))
    .unwrap_or_else(|| panic!("expected UnknownLoad diagnostic, got {err:?}"));

  assert!(
    unknown_load.message.contains("fn=")
      && unknown_load.message.contains("block=")
      && unknown_load.message.contains("inst="),
    "expected deterministic location info in message, got: {}",
    unknown_load.message
  );

  let range = unknown_load.primary.range;
  assert!(
    range.start < range.end,
    "expected non-empty source range for UnknownLoad diagnostic, got {range:?}"
  );
  assert_eq!(
    &source[range.start as usize..range.end as usize],
    "a",
    "expected diagnostic to point at identifier span"
  );

  // The native-ready API should also run verification by default.
  let err = compile_file_native_ready(
    tc_program,
    file_id,
    TopLevelMode::Global,
    false,
    NativeReadyOptions::default(),
  )
  .expect_err("expected compile_file_native_ready to fail with strict-native verification enabled");

  assert!(
    err.iter().any(|diag| diag.code == "OPTN0001"),
    "expected strict-native diagnostic OPTN0001, got {err:?}"
  );
}

#[test]
fn known_good_typed_snippet_passes() {
  let source = r#"
    function add_one(x: number): number {
      return x + 1;
    }
    add_one(2);
  "#;

  let (tc_program, file_id) = build_type_program(source);

  // Compile without verifier; verify manually.
  let native = compile_file_native_ready(
    Arc::clone(&tc_program),
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions {
      run_opt_passes: true,
      verify_strict_native: false,
      ..NativeReadyOptions::default()
    },
  )
  .expect("compile without strict-native verification");

  verify_program_strict_native(
    &native.program,
    &VerifyOptions {
      file: file_id,
      ..Default::default()
    },
  )
  .expect("expected strict-native verifier to accept program");

  // And the native-ready API should accept it with verification enabled.
  compile_file_native_ready(
    tc_program,
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions::default(),
  )
  .expect("compile with strict-native verification enabled");
}

#[test]
fn forbidden_template_marker_builtin_triggers_diagnostic() {
  // Typed builds lower template literals to `InstTyp::StringConcat`. If a legacy marker builtin leaks
  // into the IL, the strict-native verifier should reject it deterministically.
  let mut graph = CfgGraph::default();
  graph.ensure_label(0);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(
    0,
    vec![
      Inst::call(
        None,
        Arg::Builtin("__optimize_js_template".to_string()),
        Arg::Const(Const::Undefined),
        vec![Arg::Const(Const::Str("hi".to_string()))],
        Vec::new(),
      ),
      Inst::ret(None),
    ],
  );
  let cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let top_level = ProgramFunction {
    debug: None,
    meta: Default::default(),
    body: cfg.clone(),
    params: Vec::new(),
    ssa_body: Some(cfg),
    stats: OptimizationStats::default(),
  };
  let program = Program {
    source_file: FileId(0),
    source_len: 0,
    functions: Vec::new(),
    top_level,
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let err = verify_program_strict_native(
    &program,
    &VerifyOptions {
      file: FileId(0),
      ..Default::default()
    },
  )
  .expect_err("expected verifier to reject marker builtin in typed strict-native mode");

  assert!(
    err.iter().any(|diag| diag.code == "OPTN0005" && diag.message.contains("__optimize_js_template")),
    "expected OPTN0005 banned-builtin diagnostic, got {err:?}"
  );
}
