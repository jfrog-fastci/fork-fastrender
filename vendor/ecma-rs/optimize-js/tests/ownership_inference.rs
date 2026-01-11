#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, ArgUseMode, InPlaceHint, InstTyp};
use optimize_js::TopLevelMode;

fn find_unique_return_var(cfg: &Cfg) -> (u32, usize, u32) {
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for (idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::Return {
        continue;
      }
      let v = match &inst.args[0] {
        Arg::Var(v) => *v,
        _ => continue,
      };
      assert!(found.is_none(), "expected a single Return in test CFG");
      found = Some((label, idx, v));
    }
  }
  found.expect("expected Return instruction with a variable argument")
}

#[test]
fn ssa_body_has_arg_use_modes_for_consuming_return() {
  let src = r#"
    const out = (() => {
      const a = {};
      return a;
    })();
    void out;
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let cfg = program.functions[0]
    .ssa_body
    .as_ref()
    .expect("expected SSA body to be preserved for backend analyses");

  let (ret_label, ret_idx, _ret_var) = find_unique_return_var(cfg);
  let ret_inst = &cfg.bblocks.get(ret_label)[ret_idx];
  assert_eq!(
    ret_inst.meta.arg_use_modes.get(0),
    Some(&ArgUseMode::Consume),
    "expected return of owned value to be annotated as Consume"
  );
}

#[test]
fn annotate_program_writes_arg_use_modes_and_in_place_hint() {
  let src = r#"
    const out = ((cond) => {
      let a = {};
      let b;
      if (cond) {
        b = a;
      } else {
        b = {};
      }
      return b;
    })(cond);
    void out;
  "#;
  let mut program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);

  // Attach ownership + consumption metadata to the deconstructed CFG (`body`) so we can assert on
  // the VarAssign move site that appears after SSA deconstruction.
  let _analyses = annotate_program(&mut program);
  let cfg = &program.functions[0].body;

  let (_ret_label, _ret_idx, ret_var) = find_unique_return_var(cfg);
  let entry_defs: std::collections::HashSet<u32> = cfg
    .bblocks
    .get(cfg.entry)
    .iter()
    .flat_map(|inst| inst.tgts.iter().copied())
    .collect();

  // Find the VarAssign that moves the entry-allocated object (`a`) into the merged return value.
  let mut move_site = None::<(u32, usize, u32)>;
  for label in cfg.graph.labels_sorted() {
    for (idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      if inst.tgts.get(0) != Some(&ret_var) {
        continue;
      }
      let Some(Arg::Var(src)) = inst.args.get(0) else {
        continue;
      };
      if !entry_defs.contains(src) {
        continue;
      }
      assert!(move_site.is_none(), "expected a single move VarAssign from entry-allocated value");
      move_site = Some((label, idx, *src));
    }
  }
  let (label, idx, src) = move_site.expect("expected VarAssign that moves entry allocation into return");
  let inst = &cfg.bblocks.get(label)[idx];

  assert_eq!(
    inst.meta.arg_use_modes.get(0),
    Some(&ArgUseMode::Consume),
    "expected move VarAssign to record Consume use mode for its source argument"
  );
  assert_eq!(
    inst.meta.in_place_hint,
    Some(InPlaceHint::MoveNoClone { src, tgt: ret_var }),
    "expected move VarAssign to record an in-place move hint"
  );
}

#[test]
fn arg_use_modes_is_empty_when_all_args_are_borrowed() {
  let src = r#"
    ((cond) => {
      let x = cond;
      if (x) {
        sink(1);
      }
    })(cond);
  "#;
  let mut program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let _analyses = annotate_program(&mut program);
  let cfg = &program.functions[0].body;

  let mut found_cond_goto = None;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t == InstTyp::CondGoto {
        found_cond_goto = Some(inst);
        break;
      }
    }
  }
  let cond_goto = found_cond_goto.expect("expected at least one CondGoto in test CFG");
  assert!(
    cond_goto.meta.arg_use_modes.is_empty(),
    "expected arg_use_modes to be omitted when all args are Borrow"
  );
}

