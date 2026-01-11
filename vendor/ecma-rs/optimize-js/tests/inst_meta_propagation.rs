use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst, InstTyp, StringEncoding};
use optimize_js::ssa::phi_simplify::simplify_phis;
use optimize_js::ssa::ssa_deconstruct::deconstruct_ssa;
#[cfg(feature = "typed")]
use optimize_js::types::ValueTypeSummary;
use optimize_js::util::counter::Counter;
use parse_js::num::JsNumber;
#[cfg(feature = "typed")]
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};

#[test]
fn phi_simplify_copies_inst_meta_to_lowered_var_assign() {
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);

  let mut phi = Inst::phi_empty(10);
  phi.meta.result_type.string_encoding = Some(StringEncoding::Ascii);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(1.0))));

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![]);
  bblocks.add(1, vec![phi]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  assert!(simplify_phis(&mut cfg), "expected simplify_phis to change CFG");
  let inst = &cfg.bblocks.get(1)[0];
  assert_eq!(inst.t, InstTyp::VarAssign);
  assert_eq!(inst.meta.result_type.string_encoding, Some(StringEncoding::Ascii));
}

#[test]
fn ssa_deconstruct_copies_inst_meta_to_inserted_var_assign() {
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);

  let mut phi = Inst::phi_empty(10);
  phi.meta.result_type.string_encoding = Some(StringEncoding::Ascii);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(1.0))));

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![]);
  bblocks.add(1, vec![phi]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let mut c_label = Counter::new(2);
  deconstruct_ssa(&mut cfg, &mut c_label);

  // A new edge block should have been inserted between 0 and 1.
  let parents: Vec<u32> = cfg.graph.parents_sorted(1);
  assert_eq!(parents.len(), 1);
  let inserted_label = parents[0];
  assert_ne!(inserted_label, 0);
  assert_ne!(inserted_label, 1);

  let inserted_block = cfg.bblocks.get(inserted_label);
  assert!(
    inserted_block.iter().any(|inst| inst.t == InstTyp::VarAssign),
    "expected inserted SSA-deconstruct block to contain a VarAssign"
  );
  let assign = inserted_block
    .iter()
    .find(|inst| inst.t == InstTyp::VarAssign)
    .expect("VarAssign should exist");
  assert_eq!(assign.meta.result_type.string_encoding, Some(StringEncoding::Ascii));
}

#[cfg(feature = "typed")]
#[test]
fn typed_type_id_survives_ssa_phi_lowering() {
  let src = r#"
    const f = (): number => (Math.random() > 0.5) ? Math.random() : Math.random();
    void f;
  "#;

  let mut host = typecheck_ts::MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  });
  let input = typecheck_ts::FileKey::new("input.ts");
  host.insert(input.clone(), src);
  let tc_program = std::sync::Arc::new(typecheck_ts::Program::new(host, vec![input.clone()]));
  let diagnostics = tc_program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck should succeed, got {diagnostics:?}"
  );
  let file_id = tc_program.file_id(&input).expect("typecheck file id");

  let program = optimize_js::compile_source_with_typecheck(
    src,
    optimize_js::TopLevelMode::Module,
    false,
    tc_program,
    file_id,
  )
  .expect("compile with typecheck");

  fn cfg_has_typed_return_value(cfg: &Cfg) -> bool {
    // Collect vars returned from this function.
    let mut returned_vars = Vec::new();
    for (_, block) in cfg.bblocks.all() {
      for inst in block.iter() {
        if inst.t == InstTyp::Return {
          if let Some(Arg::Var(v)) = inst.as_return() {
            returned_vars.push(*v);
          }
        }
      }
    }
    if returned_vars.is_empty() {
      return false;
    }

    for returned in returned_vars {
      for (_, block) in cfg.bblocks.all() {
        for inst in block.iter() {
          if inst.tgts.iter().any(|&tgt| tgt == returned)
            && inst.meta.type_id.is_some()
            && inst.meta.hir_expr.is_some()
            && inst.meta.type_summary == Some(ValueTypeSummary::NUMBER)
            && inst.meta.excludes_nullish
          {
            return true;
          }
        }
      }
    }
    false
  }

  let mut ok = cfg_has_typed_return_value(&program.top_level.body);
  for func in &program.functions {
    ok |= cfg_has_typed_return_value(&func.body);
  }
  assert!(
    ok,
    "expected at least one return value in final CFG to be defined by an instruction with type_id metadata"
  );
}
