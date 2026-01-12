use super::super::inst::InstTyp;
use crate::compile_source;
use crate::compile_source_with_cfg_options;
use crate::il::inst::{Arg, Const, UnOp};
use crate::CompileCfgOptions;
#[cfg(feature = "typed")]
use crate::compile_source_typed_cfg_options;
use crate::Program;
use crate::ProgramFunction;
use crate::TopLevelMode;
use num_bigint::BigInt;
use parse_js::num::JsNumber;

fn compile(source: &str) -> Program {
  compile_source(source, TopLevelMode::Module, false).expect("compile input")
}

fn compile_ssa_no_opt(source: &str) -> Program {
  compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      run_opt_passes: false,
    },
  )
  .expect("compile input")
}

fn inst_types(func: &ProgramFunction) -> Vec<InstTyp> {
  func
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter().map(|inst| inst.t.clone()))
    .collect()
}

#[test]
fn destructuring_assignment_to_captured_var_is_foreign() {
  let source = r#"
      let a = 0;
      const make = (obj) => {
        ({ a } = obj);
        a += 1;
        const inner = () => { a += 1; };
        inner;
      };
    "#;

  let program = compile(source);

  assert!(program.functions.len() >= 2);
  let make_insts = inst_types(&program.functions[0]);
  assert!(
    make_insts
      .iter()
      .any(|t| matches!(t, InstTyp::ForeignStore)),
    "expected destructuring assignment to use foreign store, got {:?}",
    make_insts
  );

  let other_insts: Vec<(usize, Vec<InstTyp>)> = program.functions[1..]
    .iter()
    .enumerate()
    .map(|(i, f)| (i + 1, inst_types(f)))
    .collect();
  let has_foreign_load = other_insts
    .iter()
    .flat_map(|(_, ts)| ts.iter())
    .any(|t| matches!(t, InstTyp::ForeignLoad));
  assert!(
    has_foreign_load,
    "captured read should be a foreign load: {:?}",
    other_insts
  );
}

#[test]
fn destructuring_decl_shadowing_binds_local_symbol() {
  let program = compile(
    r#"
      const a = 0;
      const make = (obj) => {
        let { a } = obj;
        a += 1;
        const inner = () => { a += 1; };
        inner;
      };
    "#,
  );

  let lowered = hir_js::lower_from_source(
    r#"
      const a = 0;
      const make = (obj) => {
        let { a } = obj;
        a += 1;
        const inner = () => { a += 1; };
        inner;
      };
    "#,
  )
  .unwrap();
  dbg!(lowered.defs.len());
  dbg!(lowered
    .defs
    .iter()
    .map(|d| {
      (
        format!("{:?}", d.path.kind),
        lowered
          .names
          .resolve(d.name)
          .unwrap_or_default()
          .to_string(),
      )
    })
    .collect::<Vec<_>>());
  dbg!(lowered
    .defs
    .iter()
    .map(|d| (d.id, d.path.kind, d.body))
    .collect::<Vec<_>>());

  for def in lowered.defs.iter() {
    if let Some(body_id) = def.body {
      let body = lowered.body(body_id).unwrap();
      dbg!(def.path.kind, lowered.names.resolve(def.name), body.kind);
      dbg!(body.root_stmts.len());
      for stmt_id in body.root_stmts.iter() {
        let stmt = &body.stmts[stmt_id.0 as usize];
        dbg!(stmt.kind.clone());
      }
    }
  }

  dbg!(lowered
    .bodies
    .iter()
    .map(|b| (b.owner, b.kind, b.root_stmts.len(), b.stmts.len()))
    .collect::<Vec<_>>());

  dbg!(program.functions.len());
  for (idx, func) in program.functions.iter().enumerate() {
    dbg!(idx, inst_types(func));
  }

  assert!(program.functions.len() >= 2);
  let make_insts = inst_types(&program.functions[0]);
  let make_unknowns: Vec<_> = program.functions[0]
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .filter(|inst| matches!(inst.t, InstTyp::UnknownLoad | InstTyp::UnknownStore))
    .map(|inst| inst.unknown.as_str())
    .collect();
  assert!(
    !make_unknowns.iter().any(|n| *n == "a"),
    "expected destructured `a` to resolve to a local symbol, got unknowns: {make_unknowns:?}"
  );
  assert!(
    make_insts
      .iter()
      .any(|t| matches!(t, InstTyp::ForeignStore)),
    "captured local should use foreign stores: {:?}",
    make_insts
  );

  let has_foreign_load = program.functions[1..]
    .iter()
    .flat_map(inst_types)
    .any(|t| matches!(t, InstTyp::ForeignLoad));
  assert!(has_foreign_load, "captured read should be a foreign load");
}

#[test]
fn direct_eval_is_unsupported() {
  let source = r#"const f = () => { let x = 1; eval("x"); };"#;
  let err = compile_source(source, TopLevelMode::Module, false)
    .expect_err("direct eval should be rejected");

  assert!(
    err
      .iter()
      .any(|diag| diag.code == "OPT0002" && diag.message.contains("direct eval")),
    "expected OPT0002 diagnostic mentioning direct eval, got {err:?}"
  );
}

#[test]
fn shadowed_eval_is_allowed() {
  let source = r#"const f = (eval) => { let x = 1; eval("x"); };"#;
  compile_source(source, TopLevelMode::Global, false).expect("shadowed eval should compile");
}

#[test]
fn with_statement_is_rejected() {
  let source = r#"with (obj) { answer = 42; }"#;
  let err = compile_source(source, TopLevelMode::Global, false)
    .expect_err("with statements are unsupported");
  assert!(
    err
      .iter()
      .any(|diag| diag.code == "OPT0002" && diag.message.contains("with statements")),
    "expected OPT0002 about with statement, got {err:?}"
  );
}

#[test]
fn spread_call_indices_include_callee_and_this() {
  let program = compile(
    r#"
      let f;
      let obj;
      let xs;
      let ys;
      f(...xs);
      obj.m(...ys);
    "#,
  );

  let spread_calls: Vec<_> = program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .filter(|inst| matches!(inst.t, InstTyp::Call) && !inst.spreads.is_empty())
    .map(|inst| (inst.spreads.clone(), inst.args.len()))
    .collect();

  assert_eq!(
    spread_calls.len(),
    2,
    "expected spread calls for both statements, got {spread_calls:?}"
  );
  for (spreads, args_len) in spread_calls {
    assert_eq!(
      spreads,
      vec![2],
      "spread indices should account for callee and this prefix"
    );
    assert!(
      spreads.iter().all(|&i| i < args_len),
      "spread indices must be in bounds of args (len={args_len})"
    );
  }
}

#[test]
fn update_expr_on_captured_var_uses_foreign_store() {
  let program = compile(
    r#"
      let x = 0;
      const f = () => { x++; };
      f();
    "#,
  );

  assert!(
    program.functions.iter().any(|func| {
      inst_types(func)
        .iter()
        .any(|t| matches!(t, InstTyp::ForeignStore))
    }),
    "expected x++ on captured variable to emit a ForeignStore"
  );
}

#[test]
fn update_expr_on_unknown_var_uses_unknown_store() {
  let program = compile("x++;");
  let insts = inst_types(&program.top_level);
  assert!(
    insts.iter().any(|t| matches!(t, InstTyp::UnknownStore)),
    "expected x++ on unknown variable to emit UnknownStore, got {insts:?}"
  );
}

#[test]
fn update_expr_on_member_emits_prop_assign() {
  let program = compile(
    r#"
      let obj = { x: 0 };
      obj.x++;
    "#,
  );
  let insts = inst_types(&program.top_level);
  assert!(
    insts.iter().any(|t| matches!(t, InstTyp::PropAssign)),
    "expected obj.x++ to emit PropAssign, got {insts:?}"
  );
}

#[test]
fn update_expr_increments_use_numeric_coercion() {
  // In JavaScript, `x++` always performs `ToNumeric(x)` (number or bigint) before adding 1.
  // In particular, `"1"++` must produce `2`, not `"11"`.
  let program = compile_ssa_no_opt(
    r#"
      let x = "1";
      x++;
    "#,
  );
  let has_unary_plus = program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .any(|inst| inst.t == InstTyp::Un && inst.un_op == UnOp::Plus);
  assert!(
    has_unary_plus,
    "expected update lowering to include unary plus ToNumber coercion"
  );
}

#[test]
fn update_expr_supports_bigint_by_using_bigint_one() {
  // Use `2n` for initialization so we can specifically assert that `1n` is used for the update.
  let program = compile_ssa_no_opt(
    r#"
      let x = 2n;
      x++;
    "#,
  );
  let has_bigint_one = program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .flat_map(|inst| inst.args.iter())
    .any(|arg| matches!(arg, Arg::Const(Const::BigInt(v)) if v == &BigInt::from(1)));
  assert!(
    has_bigint_one,
    "expected update lowering to include BigInt(1) constant for BigInt increments"
  );
}

#[test]
fn update_expr_on_builtin_is_rejected() {
  let err = compile_source("undefined++;", TopLevelMode::Module, false)
    .expect_err("expected update of builtin to be rejected");
  assert!(
    err.iter().any(|diag| {
      diag.code == "OPT0002" && diag.message.contains("assignment to builtin")
    }),
    "expected OPT0002 assignment-to-builtin diagnostic, got {err:?}"
  );
}

#[cfg(feature = "typed")]
#[test]
fn typed_postfix_update_updates_original_local_symbol() {
  let source = r#"
    let x = 0;
    x++;
    x;
  "#;

  let program = compile_source_typed_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      run_opt_passes: false,
    },
  )
  .expect("typed compile input");

  let cfg = &program.top_level.body;
  let mut insts = Vec::new();
  for label in cfg.reverse_postorder() {
    insts.extend(cfg.bblocks.get(label).iter());
  }

  let mut init_tgt = None;
  let mut updated_tgt = None;
  let mut last_preserved_rhs = None;

  for inst in insts {
    match inst.t {
      InstTyp::VarAssign => {
        let (tgt, arg) = inst.as_var_assign();
        if matches!(arg, Arg::Const(Const::Num(JsNumber(0.0)))) {
          init_tgt = Some(tgt);
        }
        if inst.meta.preserve_var_assign {
          if let Arg::Var(rhs) = arg {
            last_preserved_rhs = Some(*rhs);
          }
        }
      }
      InstTyp::Bin => {
        let (tgt, _left, op, right) = inst.as_bin();
        if op == crate::il::inst::BinOp::Add
          && matches!(right, Arg::Const(Const::Num(JsNumber(1.0))))
        {
          updated_tgt = Some(tgt);
        }
      }
      _ => {}
    }
  }

  let init_tgt = init_tgt.expect("missing `let x = 0` initialization");
  let updated_tgt = updated_tgt.expect("missing `x++` update bin instruction");
  let last_preserved_rhs = last_preserved_rhs.expect("missing trailing identifier read");

  assert_eq!(
    last_preserved_rhs, updated_tgt,
    "expected final read of x to use the updated SSA value"
  );
  assert_ne!(
    last_preserved_rhs, init_tgt,
    "expected final read of x to not use the pre-update SSA value"
  );
}

#[cfg(feature = "typed")]
#[test]
fn typed_native_layout_uses_layout_of_interned_for_ref_types() {
  use std::sync::Arc;

  use typecheck_ts::FileKey;
  use types_ts_interned::{Layout, PtrKind, TypeKind};

  let source = r#"
    type Foo = { x: string };
    declare function use(x: Foo): void;
    const v: Foo = { x: "hi" };
    use(v);
  "#;

  // Mirror `compile_source_typed_cfg_options` but retain the type program so we
  // can inspect the interned store layouts.
  let mut host = crate::typed_memory_host_for_source(source);
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), source);
  let type_program = Arc::new(typecheck_ts::Program::new(host, vec![file.clone()]));
  let diagnostics = type_program.check();
  assert!(diagnostics.is_empty(), "typecheck failed: {diagnostics:?}");
  let type_file = type_program
    .file_id(&file)
    .expect("typecheck program should know the inserted file");

  let program = crate::compile_file_with_typecheck_cfg_options(
    Arc::clone(&type_program),
    type_file,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      run_opt_passes: false,
    },
  )
  .expect("compile typed input");

  let store = type_program.interned_type_store();

  // Ensure `optimize-js` attaches evaluated layouts (via
  // `Program::layout_of_interned`) for expression types that are still represented
  // as `TypeKind::Ref` in the interned store.
  let mut found_ref = false;
  for (_, block) in program.top_level.body.bblocks.all() {
    for inst in block.iter() {
      let Some(ty) = inst.meta.type_id else {
        continue;
      };
      if !matches!(store.type_kind(ty), TypeKind::Ref { .. }) {
        continue;
      }
      found_ref = true;
      let layout_id = inst
        .meta
        .native_layout
        .expect("expected typed inst to include native_layout");
      if matches!(store.layout(layout_id), Layout::Ptr { to: PtrKind::Opaque }) {
        panic!("expected ref type to lower to a concrete layout, got PtrKind::Opaque");
      }
    }
  }

  assert!(
    found_ref,
    "expected typed lowering to produce at least one instruction whose type is TypeKind::Ref"
  );
}

#[test]
fn return_statement_emits_return_inst_with_value() {
  let program = compile("(() => { return 1; })();");

  let mut found = false;
  for func in &program.functions {
    for (_, block) in func.body.bblocks.all() {
      for inst in block {
        if inst.t == InstTyp::Return
          && inst.as_return() == Some(&Arg::Const(Const::Num(JsNumber(1.0))))
        {
          found = true;
        }
      }
    }
  }

  assert!(
    found,
    "expected to find Return(Const::Num(1)) in nested function IL"
  );
}

#[test]
fn destructured_parameter_binding_temps_follow_pattern_traversal_order() {
  let source = r#"const f = ({ x, y }) => { y; x; };"#;
  let first = compile(source);
  let second = compile(source);

  assert_eq!(first.functions.len(), 1);
  assert_eq!(second.functions.len(), 1);

  assert_eq!(first.functions[0].params, vec![0, 1]);
  assert_eq!(second.functions[0].params, vec![0, 1]);
}

#[test]
fn parameter_temps_follow_parameter_order() {
  let source = r#"const f = (a, b) => { b; a; };"#;
  let first = compile(source);
  let second = compile(source);

  assert_eq!(first.functions.len(), 1);
  assert_eq!(second.functions.len(), 1);

  assert_eq!(first.functions[0].params, vec![0, 1]);
  assert_eq!(second.functions[0].params, vec![0, 1]);
}

#[cfg(feature = "semantic-ops")]
#[test]
fn known_api_call_lowers_to_structured_il_inst() {
  use hir_js::{lower_from_source_with_kind, ExprKind, FileKind, StmtKind};
  use std::sync::Arc;

  let src = "JSON.parse(x);";
  let lowered = lower_from_source_with_kind(FileKind::Js, src).expect("lower");

  // Rewrite the first expression statement to a semantic-ops KnownApiCall.
  let body_id = lowered.root_body();
  let body = lowered.body(body_id).expect("root body");
  let stmt_id = *body.root_stmts.first().expect("root stmt");
  let stmt = &body.stmts[stmt_id.0 as usize];
  let expr_id = match stmt.kind {
    StmtKind::Expr(expr) => expr,
    _ => panic!("expected expression statement"),
  };

  let ExprKind::Call(call) = &body.exprs[expr_id.0 as usize].kind else {
    panic!("expected Call expression");
  };
  let args = call.args.iter().map(|arg| arg.expr).collect();
  let api = hir_js::ApiId::from_name("JSON.parse");

  let mut rewritten = lowered.clone();
  let body_idx = *rewritten.body_index.get(&body_id).expect("root body index");
  let mut new_body = rewritten.bodies[body_idx].as_ref().clone();
  new_body.exprs[expr_id.0 as usize].kind = ExprKind::KnownApiCall { api, args };
  rewritten.bodies[body_idx] = Arc::new(new_body);

  let program = Program::compile_lowered(src, rewritten, TopLevelMode::Module, false)
    .expect("compile lowered KnownApiCall");

  // Assert IL uses the structured KnownApiCall instruction and preserves the ApiId.
  let expected_api = hir_js::ApiId::from_name("JSON.parse");
  let mut found = false;
  for (_, block) in program.top_level.body.bblocks.all() {
    for inst in block.iter() {
      if let InstTyp::KnownApiCall { api } = &inst.t {
        assert_eq!(*api, expected_api);
        found = true;
      }
      for arg in inst.args.iter() {
        if let Arg::Builtin(name) = arg {
          assert!(
            !name.starts_with("known_api:"),
            "KnownApiCall should not be encoded as a stringly builtin: {name:?}"
          );
        }
      }
    }
  }
  assert!(
    found,
    "expected to find a KnownApiCall instruction, got {:?}",
    inst_types(&program.top_level)
  );
}
