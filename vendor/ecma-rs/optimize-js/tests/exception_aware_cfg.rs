#[path = "common/mod.rs"]
mod common;

use optimize_js::cfg::cfg::{Cfg, CfgEdgeKind};
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

fn find_first_invoke(cfg: &Cfg) -> Option<(u32, u32, u32)> {
  for (label, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      if inst.t == InstTyp::Invoke {
        let (_tgt, _callee, _this, _args, _spreads, normal, exceptional) = inst.as_invoke();
        return Some((label, normal, exceptional));
      }
    }
  }
  None
}

#[test]
fn try_catch_call_has_exception_edge() {
  let source = r#"
    function mayThrow() {}
    try {
      mayThrow();
    } catch (e) {}
  "#;

  let program = compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: false,
      run_opt_passes: false,
      ..Default::default()
    },
  )
  .expect("compile");

  let cfg = program.top_level.cfg_ssa().expect("ssa cfg should be present");
  let (invoke_label, normal, exceptional) = find_first_invoke(cfg).expect("expected an Invoke inst");

  assert_eq!(
    cfg.graph.edge_kind(invoke_label, normal),
    Some(CfgEdgeKind::Normal)
  );
  assert_eq!(
    cfg.graph.edge_kind(invoke_label, exceptional),
    Some(CfgEdgeKind::Exceptional)
  );

  let handler_block = cfg.bblocks.get(exceptional);
  assert!(
    handler_block.iter().any(|inst| inst.t == InstTyp::Catch),
    "expected catch handler to contain a Catch inst, got {handler_block:?}"
  );
}

#[test]
fn try_finally_enters_finally_on_both_paths() {
  let source = r#"
    function test(cond) {
      let x = 0;
      try {
        if (cond) throw 1;
        x = 2;
      } finally {
        x = 3;
      }
      return x;
    }
  "#;

  let program = compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: false,
      run_opt_passes: false,
      ..Default::default()
    },
  )
  .expect("compile");

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let cfg = program.functions[0].cfg_ssa().expect("ssa cfg should be present");

  let catch_labels: Vec<u32> = cfg
    .bblocks
    .all()
    .filter_map(|(label, block)| {
      if block.iter().any(|inst| inst.t == InstTyp::Catch) {
        Some(label)
      } else {
        None
      }
    })
    .collect();

  assert_eq!(
    catch_labels.len(),
    1,
    "expected exactly one Catch block for try/finally, got {catch_labels:?}"
  );
  let unwind_entry = catch_labels[0];

  // The exceptional entry should be reached via at least one exceptional edge (from `throw` in the
  // try block).
  assert!(
    !cfg.graph.exceptional_parents_sorted(unwind_entry).is_empty(),
    "expected finally unwind entry {unwind_entry} to have an exceptional predecessor"
  );

  // The unwind entry and the normal-completion entry should both flow into the shared finally
  // body block.
  let unwind_children = cfg.graph.normal_children_sorted(unwind_entry);
  assert_eq!(
    unwind_children.len(),
    1,
    "expected unwind entry {unwind_entry} to have exactly one normal successor"
  );
  let finally_body = unwind_children[0];

  let parents = cfg.graph.parents_sorted(finally_body);
  assert!(
    parents.contains(&unwind_entry),
    "expected finally body {finally_body} to have unwind parent {unwind_entry}, got {parents:?}"
  );
  let other_parent = parents
    .iter()
    .copied()
    .find(|p| *p != unwind_entry && cfg.graph.edge_kind(*p, finally_body) == Some(CfgEdgeKind::Normal));
  assert!(
    other_parent.is_some(),
    "expected finally body {finally_body} to have a second normal parent besides unwind entry; parents={parents:?}"
  );
  let other_parent = other_parent.unwrap();
  assert!(
    !cfg
      .bblocks
      .get(other_parent)
      .iter()
      .any(|inst| inst.t == InstTyp::Catch),
    "expected normal finally entry {other_parent} to not contain Catch"
  );
}

#[test]
fn no_throw_builtin_prunes_exception_edge() {
  let source = r#"
    try {
      Math.abs(1);
    } catch (e) {}
  "#;

  let no_opt = compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: false,
      run_opt_passes: false,
      ..Default::default()
    },
  )
  .expect("compile");

  let cfg_no_opt = no_opt
    .top_level
    .cfg_ssa()
    .expect("ssa cfg should be present");
  let (_invoke_label, _normal, _exceptional) =
    find_first_invoke(cfg_no_opt).expect("expected an Invoke before opt passes");

  let opt = common::compile_source(source, TopLevelMode::Module, false);
  let cfg_opt = opt.top_level.cfg_ssa().expect("ssa cfg should be present");

  assert!(
    find_first_invoke(cfg_opt).is_none(),
    "expected Invoke to be pruned to Call when builtin is proven no-throw"
  );

  // After pruning, there should be no exceptional edges remaining in this tiny program.
  let mut saw_exception_edge = false;
  for label in cfg_opt.graph.labels_sorted() {
    if !cfg_opt.graph.exceptional_children_sorted(label).is_empty() {
      saw_exception_edge = true;
      break;
    }
  }
  assert!(
    !saw_exception_edge,
    "expected no exceptional edges after pruning no-throw invoke"
  );
}
