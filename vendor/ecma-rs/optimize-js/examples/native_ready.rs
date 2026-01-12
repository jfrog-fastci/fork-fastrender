//! Compile a small TypeScript snippet into a "native-backend ready" artifact.
//!
//! Run:
//! ```bash
//! bash scripts/cargo_agent.sh run -p optimize-js --features typed --example native_ready
//! ```

use optimize_js::analysis::FunctionKey;
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_file_native_ready, NativeReadyOptions, TopLevelMode};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};

fn main() {
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

  let mut host = typecheck_ts::MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  });
  let file = typecheck_ts::FileKey::new("input.ts");
  host.insert(file.clone(), source);

  let tc_program = Arc::new(typecheck_ts::Program::new(host, vec![file.clone()]));
  let diagnostics = tc_program.check();
  assert!(diagnostics.is_empty(), "typecheck failed: {diagnostics:?}");
  let file_id = tc_program.file_id(&file).expect("typecheck file id");

  let native = compile_file_native_ready(
    tc_program,
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions::default(),
  )
  .expect("compile native-ready");

  let mut phi_count = 0usize;
  let mut count_phis = |cfg: &optimize_js::cfg::cfg::Cfg| {
    for label in cfg.graph.labels_sorted() {
      for inst in cfg.bblocks.get(label).iter() {
        if inst.t == InstTyp::Phi {
          phi_count += 1;
        }
      }
    }
  };
  count_phis(&native.program.top_level.body);
  for func in &native.program.functions {
    count_phis(&func.body);
  }
  println!("phi nodes: {phi_count}");

  // Print a compact effect/purity summary.
  let mut keys: Vec<FunctionKey> = native.analyses.effects_summary.keys().copied().collect();
  keys.sort();
  for key in keys {
    let effects = &native.analyses.effects_summary[&key];
    let purity = native.analyses.purity[&key];
    println!(
      "{key:?}: purity={purity:?}, unknown_effects={}, reads={}, writes={}",
      effects.unknown,
      effects.reads.len(),
      effects.writes.len()
    );
  }
}
