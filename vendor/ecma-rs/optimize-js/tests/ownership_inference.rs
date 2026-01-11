#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::annotate_program;
use optimize_js::analysis::driver::FunctionKey;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, ArgUseMode, Const, InPlaceHint, InstTyp, OwnershipState};
use optimize_js::TopLevelMode;

fn mode_at(inst: &optimize_js::il::inst::Inst, idx: usize) -> ArgUseMode {
  inst
    .meta
    .arg_use_modes
    .get(idx)
    .copied()
    .unwrap_or(ArgUseMode::Borrow)
}

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

fn find_unique_throw_var(cfg: &Cfg) -> (u32, usize, u32) {
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for (idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::Throw {
        continue;
      }
      let v = match &inst.args[0] {
        Arg::Var(v) => *v,
        _ => continue,
      };
      assert!(found.is_none(), "expected a single Throw in test CFG");
      found = Some((label, idx, v));
    }
  }
  found.expect("expected Throw instruction with a variable argument")
}

fn find_unique_prop_assign(cfg: &Cfg, prop: &str) -> (u32, usize, u32) {
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for (idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::PropAssign {
        continue;
      }
      if !matches!(&inst.args[1], Arg::Const(Const::Str(name)) if name == prop) {
        continue;
      }
      let v = match &inst.args[2] {
        Arg::Var(v) => *v,
        _ => continue,
      };
      assert!(
        found.is_none(),
        "expected a single PropAssign to property {prop:?} in test CFG"
      );
      found = Some((label, idx, v));
    }
  }
  found.expect("expected PropAssign with a variable RHS")
}

fn find_unique_non_internal_call_arg0(cfg: &Cfg) -> (u32, usize, u32) {
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for (idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::Call {
        continue;
      }
      // Skip internal literal/allocation helpers.
      if matches!(&inst.args[0], Arg::Builtin(name) if name.starts_with("__optimize_js_")) {
        continue;
      }
      // args layout: [callee, this, arg0, ...]
      let Some(Arg::Var(v)) = inst.args.get(2) else {
        continue;
      };
      assert!(found.is_none(), "expected a single non-internal call with one arg");
      found = Some((label, idx, *v));
    }
  }
  found.expect("expected non-internal call with a variable arg0")
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
    .cfg_ssa()
    .expect("expected SSA body to be preserved for backend analyses");

  let (ret_label, ret_idx, _ret_var) = find_unique_return_var(cfg);
  let ret_inst = &cfg.bblocks.get(ret_label)[ret_idx];
  assert_eq!(
    mode_at(ret_inst, 0),
    ArgUseMode::Consume,
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
      assert!(
        move_site.is_none(),
        "expected a single move VarAssign from entry-allocated value"
      );
      move_site = Some((label, idx, *src));
    }
  }
  let (label, idx, src) =
    move_site.expect("expected VarAssign that moves entry allocation into return");
  let inst = &cfg.bblocks.get(label)[idx];

  assert_eq!(
    mode_at(inst, 0),
    ArgUseMode::Consume,
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

#[test]
fn returned_value_is_consumed_as_an_ownership_transfer() {
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
  let analyses = annotate_program(&mut program);
  let cfg = &program.functions[0].body;
  let ownership = analyses
    .ownership
    .get(&FunctionKey::Fn(0))
    .expect("missing ownership results for nested function");

  let (ret_label, ret_idx, ret_var) = find_unique_return_var(cfg);
  let ret_inst = &cfg.bblocks.get(ret_label)[ret_idx];
  assert_eq!(
    mode_at(ret_inst, 0),
    ArgUseMode::Consume,
    "expected returned value to be consumed (ownership transferred out of function)"
  );
  assert_eq!(
    ownership.get(&ret_var),
    Some(&OwnershipState::Owned),
    "expected returned value to be owned"
  );
}

#[test]
fn earlier_prop_assign_borrows_when_value_is_used_again() {
  let src = r#"
    (() => {
      let a = {};
      let c = {};
      c.y = a;
      c.x = a;
    })();
  "#;
  let mut program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let analyses = annotate_program(&mut program);
  let cfg = &program.functions[0].body;
  let ownership = analyses
    .ownership
    .get(&FunctionKey::Fn(0))
    .expect("missing ownership results for nested function");

  let (label_y, idx_y, var_y) = find_unique_prop_assign(cfg, "y");
  let (label_x, idx_x, var_x) = find_unique_prop_assign(cfg, "x");
  assert_eq!(var_x, var_y, "expected both prop stores to write the same value");

  let inst_y = &cfg.bblocks.get(label_y)[idx_y];
  let inst_x = &cfg.bblocks.get(label_x)[idx_x];

  assert_eq!(
    mode_at(inst_y, 2),
    ArgUseMode::Borrow,
    "expected earlier prop store to borrow when value is used again later"
  );
  assert_eq!(
    mode_at(inst_x, 2),
    ArgUseMode::Consume,
    "expected last prop store to consume owned value"
  );
  assert_eq!(
    ownership.get(&var_x),
    Some(&OwnershipState::Owned),
    "expected stored value to be owned"
  );
}

#[test]
fn thrown_value_is_consumed_as_an_ownership_transfer() {
  let src = r#"
    const fail = () => {
      let a = {};
      throw a;
    };
    fail();
  "#;
  let mut program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let analyses = annotate_program(&mut program);
  let cfg = &program.functions[0].body;
  let ownership = analyses
    .ownership
    .get(&FunctionKey::Fn(0))
    .expect("missing ownership results for nested function");

  let (label, idx, var) = find_unique_throw_var(cfg);
  let inst = &cfg.bblocks.get(label)[idx];
  assert_eq!(mode_at(inst, 0), ArgUseMode::Consume);
  assert_eq!(
    ownership.get(&var),
    Some(&OwnershipState::Owned),
    "expected thrown value to be owned (it escapes via throw)"
  );
}

#[test]
fn global_escape_forces_shared() {
  let src = r#"
    (() => {
      let a = {};
      unknown(a);
    })();
  "#;
  let mut program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let analyses = annotate_program(&mut program);
  let cfg = &program.functions[0].body;
  let ownership = analyses
    .ownership
    .get(&FunctionKey::Fn(0))
    .expect("missing ownership results for nested function");

  let (label, idx, escaped_arg) = find_unique_non_internal_call_arg0(cfg);
  let inst = &cfg.bblocks.get(label)[idx];
  assert_eq!(
    ownership.get(&escaped_arg),
    Some(&OwnershipState::Shared),
    "expected argument passed to unknown call to be shared (it escapes)"
  );
  assert_eq!(
    mode_at(inst, 2),
    ArgUseMode::Borrow,
    "shared values should not be consumed at call sites"
  );
}
