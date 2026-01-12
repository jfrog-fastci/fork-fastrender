#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp};
use optimize_js::{compile_file_native_ready, NativeReadyOptions, TopLevelMode};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  blocks
    .into_iter()
    .flat_map(|(_, block)| block.iter().cloned())
    .collect()
}

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
fn typed_lowering_populates_native_layout_ids() {
  let program = compile_source_typed(
    r#"
      type Foo = { s: string };

      function getMsg(): string {
        return "hi" + Math.random();
      }

      const foo: Foo = { s: getMsg() };
      console.log(foo);
      console.log(foo.s);
    "#,
    TopLevelMode::Module,
    false,
  );

  let insts = collect_insts(program.top_level.analyzed_cfg());
  assert!(
    insts
      .iter()
      .any(|inst| inst.meta.type_id.is_some() && inst.meta.native_layout.is_some()),
    "expected at least one IL instruction to carry both a TypeId and native layout id"
  );
}

#[test]
fn native_layout_ids_are_deterministic_across_compiles() {
  let source = r#"
    type Foo = { s: string };

    function getMsg(): string {
      return "hi" + Math.random();
    }

    const foo: Foo = { s: getMsg() };
    console.log(foo);
    console.log(foo.s);
  "#;

  let first = compile_source_typed(source, TopLevelMode::Module, false);
  let second = compile_source_typed(source, TopLevelMode::Module, false);

  let insts_first = collect_insts(first.top_level.analyzed_cfg());
  let insts_second = collect_insts(second.top_level.analyzed_cfg());

  assert_eq!(
    insts_first, insts_second,
    "expected IL instruction stream to be deterministic (ignoring metadata)"
  );

  let layouts_first: Vec<_> = insts_first.iter().map(|inst| inst.meta.native_layout).collect();
  let layouts_second: Vec<_> = insts_second
    .iter()
    .map(|inst| inst.meta.native_layout)
    .collect();

  assert_eq!(
    layouts_first, layouts_second,
    "expected native layout ids to be deterministic across compilation runs"
  );
}

#[test]
fn layout_propagation_recovers_phi_layouts_after_ssa_and_opts() {
  // In this example the source-level variable `x` is assigned both `number` and `string` values,
  // so SSA construction's best-effort `Phi` metadata is forced to drop the `type_id`/layout.
  // However, the `if` join phi only ever merges numeric values (it is used before the string
  // assignment), and the typed layout propagation pass should recover that single native layout
  // deterministically.
  let source = r#"
    declare function unknown_cond(): boolean;
    declare function side_effect_true(): void;
    declare function side_effect_false(): void;
    declare function sink_number(x: number): void;
    declare function sink_string(x: string): void;

    let x: number | string = 0;
    if (unknown_cond()) {
      side_effect_true();
      x = 1;
    } else {
      side_effect_false();
      x = 2;
    }
    // `x` is only ever a number on this path, but it is later assigned a string value, forcing
    // SSA construction's per-var metadata to drop the `type_id`/layout on inserted Phi nodes.
    sink_number(x as number);

    x = "done";
    sink_string(x as string);
  "#;

  let (tc_program, file_id) = build_type_program(source);
  let native = compile_file_native_ready(
    tc_program,
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions {
      run_opt_passes: true,
      verify_strict_native: false,
      ..NativeReadyOptions::default()
    },
  )
  .expect("compile native-ready");

  let insts = collect_insts(&native.program.top_level.body);
  let phis: Vec<_> = insts
    .iter()
    .filter(|inst| inst.t == InstTyp::Phi)
    .cloned()
    .collect();
  assert!(
    !phis.is_empty(),
    "expected at least one Phi instruction; insts={insts:?}"
  );
  assert!(
    phis
      .iter()
      .any(|phi| phi.meta.type_id.is_none() && phi.meta.native_layout.is_some()),
    "expected at least one Phi to have no TypeId but a propagated native layout; phis={phis:?}"
  );
}
