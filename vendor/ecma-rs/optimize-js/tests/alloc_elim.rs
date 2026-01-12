#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

fn count_object_allocs(cfg: &Cfg) -> usize {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .filter(|inst| inst.t == InstTyp::ObjectLit)
    .count()
}

fn find_first_object_alloc(cfg: &Cfg) -> Option<&optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| inst.t == InstTyp::ObjectLit)
}

#[test]
fn local_object_literal_is_scalar_replaced() {
  let program = compile_source(
    r#"
      function f() {
        const o = { a: 1, b: 2 };
        return o.a + o.b;
      }
      f();
    "#,
    TopLevelMode::Module,
    false,
  );

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let cfg = program.functions[0]
    .cfg_ssa()
    .expect("expected SSA body to be preserved");

  assert_eq!(
    count_object_allocs(cfg),
    0,
    "expected object literal allocation to be scalar replaced"
  );
}

#[test]
fn escaping_object_literal_is_not_eliminated() {
  let program = compile_source(
    r#"
      function f() {
        const o = { a: 1, b: 2 };
        return o;
      }
      f();
    "#,
    TopLevelMode::Module,
    false,
  );

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let cfg = program.functions[0]
    .cfg_ssa()
    .expect("expected SSA body to be preserved");

  assert!(
    count_object_allocs(cfg) >= 1,
    "expected escaping object allocation to remain in CFG"
  );
}

#[test]
fn no_escape_allocation_is_marked_stack_allocatable_when_not_scalar_replaced() {
  let program = compile_source(
    r#"
      function id(x) { return x; }
      function f() {
        const o = {};
        id(o);
        return 0;
      }
      f();
    "#,
    TopLevelMode::Module,
    false,
  );

  let func = program
    .functions
    .iter()
    .find(|func| find_first_object_alloc(func.analyzed_cfg()).is_some())
    .expect("expected one function to contain an object allocation");
  let alloc = find_first_object_alloc(func.analyzed_cfg()).expect("allocation should exist");

  assert!(
    alloc.meta.stack_alloc_candidate,
    "expected NoEscape allocation to be marked stack_alloc_candidate"
  );
}
