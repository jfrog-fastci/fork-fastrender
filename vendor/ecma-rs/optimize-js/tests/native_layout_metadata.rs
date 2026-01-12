#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::Inst;
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter().cloned())
    .collect()
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

