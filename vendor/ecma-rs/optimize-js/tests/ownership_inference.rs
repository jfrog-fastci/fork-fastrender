#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::escape::analyze_cfg_escapes;
use optimize_js::analysis::ownership::{infer_ownership, UseMode, ValueOwnership};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Const, InstTyp};
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

fn find_unique_prop_assign_value_var(cfg: &Cfg, prop: &str) -> u32 {
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label).iter() {
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
      found = Some(v);
    }
  }
  found.expect("expected PropAssign with a variable RHS")
}

#[test]
fn last_use_consumption_moves_to_return() {
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
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let cfg = &program.functions[0].body;

  let escapes = analyze_cfg_escapes(cfg);
  let ownership = infer_ownership(cfg, &escapes);

  // The returned value should be consumed as an ownership transfer out of the function.
  let (ret_label, ret_idx, ret_var) = find_unique_return_var(cfg);
  assert_eq!(ownership.arg_use.get(&(ret_label, ret_idx)).unwrap()[0], UseMode::Consume);
  assert_eq!(ownership.var_ownership.get(&ret_var), Some(&ValueOwnership::Owned));

  // The move `b = a` is represented after SSA deconstruction as a `VarAssign` writing to the
  // returned temp (`ret_var`) in the `cond` true branch. It should consume the source value when
  // that value is not used afterwards.
  let entry_defs: std::collections::HashSet<u32> = cfg
    .bblocks
    .get(cfg.entry)
    .iter()
    .flat_map(|inst| inst.tgts.iter().copied())
    .collect();
  let mut move_site = None;
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
      if ownership.var_ownership.get(src) != Some(&ValueOwnership::Owned) {
        continue;
      }
      assert!(
        move_site.is_none(),
        "expected a single VarAssign that moves `a` into returned value"
      );
      move_site = Some((label, idx));
    }
  }
  let (label, idx) = move_site.expect("expected VarAssign that moves `a` into returned value");
  assert_eq!(
    ownership.arg_use.get(&(label, idx)).unwrap()[0],
    UseMode::Consume,
    "expected `b = a` to consume `a` when `a` is not used afterwards"
  );
}

#[test]
fn non_last_use_assignment_does_not_consume_source() {
  // Ensure `a` remains live after `b = a` by using it again in a side-effecting prop store. This
  // prevents trivial DCE from removing the binding chain.
  let src = r#"
    ((cond) => {
      let a = {};
      let b;
      if (cond) {
        b = a;
      } else {
        b = {};
      }
      let c = {};
      c.x = b;
      c.y = a;
    })(cond);
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let cfg = &program.functions[0].body;

  let escapes = analyze_cfg_escapes(cfg);
  let ownership = infer_ownership(cfg, &escapes);

  // Find the merged `b` temp from `c.x = b` and the source `a` temp from `c.y = a`.
  let b_var = find_unique_prop_assign_value_var(cfg, "x");
  let a_var = find_unique_prop_assign_value_var(cfg, "y");

  // Find the VarAssign that writes the merged `b` value from `a` in the `cond` true branch.
  let mut assign_site = None::<(u32, usize)>;
  for label in cfg.graph.labels_sorted() {
    for (idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      if inst.tgts.get(0) != Some(&b_var) {
        continue;
      }
      if inst.args.get(0).is_some_and(|arg| matches!(arg, Arg::Var(v) if *v == a_var)) {
        assign_site = Some((label, idx));
        break;
      }
    }
  }
  let (label, idx) = assign_site.expect("expected VarAssign that assigns b = a in one branch");

  assert_eq!(
    ownership.arg_use.get(&(label, idx)).unwrap()[0],
    UseMode::Borrow,
    "expected `b = a` to borrow when `a` is used again later"
  );
  assert_eq!(
    ownership.var_ownership.get(&a_var),
    Some(&ValueOwnership::Shared),
    "expected `a` to be shared due to aliasing with `b` along some paths"
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
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let cfg = &program.functions[0].body;

  let escapes = analyze_cfg_escapes(cfg);
  let ownership = infer_ownership(cfg, &escapes);

  let (label, idx, var) = find_unique_throw_var(cfg);
  assert_eq!(
    ownership.arg_use.get(&(label, idx)).unwrap()[0],
    UseMode::Consume,
    "expected thrown value to be consumed (ownership transferred out of function)"
  );
  assert_eq!(
    ownership.var_ownership.get(&var),
    Some(&ValueOwnership::Owned),
    "expected thrown value to be owned"
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
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);
  let cfg = &program.functions[0].body;

  let escapes = analyze_cfg_escapes(cfg);
  let ownership = infer_ownership(cfg, &escapes);

  // Locate the call `unknown(a)` and assert the argument is shared (forced by GlobalEscape).
  let mut escaped_arg = None;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label) {
      if inst.t != InstTyp::Call {
        continue;
      }
      if matches!(&inst.args[0], Arg::Builtin(_)) {
        continue; // internal alloc builders
      }
      // args layout: [callee, this, arg0, ...]
      if inst.args.len() < 3 {
        continue;
      }
      if let Arg::Var(v) = &inst.args[2] {
        escaped_arg = Some(*v);
        break;
      }
    }
  }
  let escaped_arg = escaped_arg.expect("expected to find unknown(a) call arg");
  assert_eq!(
    ownership.var_ownership.get(&escaped_arg),
    Some(&ValueOwnership::Shared),
    "expected argument passed to unknown call to be shared"
  );
}
