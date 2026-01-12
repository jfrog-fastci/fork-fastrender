#![cfg(feature = "typed")]

use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp};
use optimize_js::TopLevelMode;

fn cfg_contains_getprop(cfg: &Cfg, prop: &str) -> Option<Arg> {
  for (_, bb) in cfg.bblocks.all() {
    for inst in bb {
      if inst.t != InstTyp::Bin {
        continue;
      }
      let (_tgt, left, op, right) = inst.as_bin();
      if op != BinOp::GetProp {
        continue;
      }
      if matches!(right, Arg::Const(Const::Str(s)) if s == prop) {
        return Some(left.clone());
      }
    }
  }
  None
}

fn cfg_has_nullcheck_for(cfg: &Cfg, value: &Arg) -> bool {
  for (_, bb) in cfg.bblocks.all() {
    for inst in bb {
      if inst.t != InstTyp::NullCheck {
        continue;
      }
      let (_tgt, checked) = inst.as_null_check();
      if checked == value {
        return true;
      }
    }
  }
  false
}

fn function_cfg_with_getprop<'a>(
  program: &'a optimize_js::Program,
  prop: &str,
) -> (&'a Cfg, Arg) {
  for func in &program.functions {
    let cfg = func.analyzed_cfg();
    if let Some(receiver) = cfg_contains_getprop(cfg, prop) {
      return (cfg, receiver);
    }
  }
  panic!("missing function containing GetProp({prop})");
}

#[test]
fn guarded_branch_eliminates_nullcheck() {
  let src = r#"
    function get(): { a: number } | null { return null as any; }

    export function guarded() {
      const x = get();
      if (x === null) return;
      return x.a;
    }
  "#;

  let program = common::compile_source_typed(src, TopLevelMode::Module, false);
  let (cfg, receiver) = function_cfg_with_getprop(&program, "a");

  assert!(
    !cfg_has_nullcheck_for(cfg, &receiver),
    "expected NullCheck on receiver {receiver:?} to be eliminated"
  );
}

#[test]
fn unguarded_keeps_nullcheck() {
  let src = r#"
    function get(): { a: number } | null { return null as any; }

    export function unguarded() {
      const x = get();
      return x.a;
    }
  "#;

  let program = common::compile_source_typed(src, TopLevelMode::Module, false);
  let (cfg, receiver) = function_cfg_with_getprop(&program, "a");

  assert!(
    cfg_has_nullcheck_for(cfg, &receiver),
    "expected NullCheck on receiver {receiver:?} to remain"
  );
}

mod common;
