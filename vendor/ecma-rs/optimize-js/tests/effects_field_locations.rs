#![cfg(feature = "typed")]

use optimize_js::analysis::effect::{annotate_cfg_effects_typed, compute_program_effects_typed};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, EffectLocation, Inst, InstTyp};
use optimize_js::{CompileCfgOptions, TopLevelMode};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost};

fn compile_with_typecheck(
  source: &str,
  native_strict: bool,
) -> (optimize_js::Program, Arc<typecheck_ts::Program>) {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    native_strict,
    ..Default::default()
  });
  let file_key = FileKey::new("input.ts");
  host.insert(file_key.clone(), source);
  let type_program = Arc::new(typecheck_ts::Program::new(host, vec![file_key.clone()]));
  let diagnostics = type_program.check();
  assert!(
    diagnostics.is_empty(),
    "expected typecheck diagnostics to be empty but got: {diagnostics:?}"
  );

  let file_id = type_program
    .file_id(&file_key)
    .expect("typecheck program should know the inserted file");
  let program = optimize_js::compile_file_with_typecheck_cfg_options(
    Arc::clone(&type_program),
    file_id,
    TopLevelMode::Module,
    false,
    // Keep the IL close to the source for effect-location assertions.
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile typed source");

  (program, type_program)
}

fn collect_insts(cfg: &Cfg) -> Vec<&Inst> {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .collect()
}

fn annotate_top_level_effects(
  program: &mut optimize_js::Program,
  type_program: &typecheck_ts::Program,
) {
  let summaries = compute_program_effects_typed(program, type_program);
  if let Some(cfg) = program.top_level.ssa_body.as_mut() {
    annotate_cfg_effects_typed(cfg, &summaries, type_program);
  } else {
    annotate_cfg_effects_typed(&mut program.top_level.body, &summaries, type_program);
  }
}

#[test]
fn two_writes_to_different_fields_produce_distinct_field_locations() {
  let (mut program, type_program) = compile_with_typecheck(
    r#"
      interface Obj { x: number; y: number; }
      const obj: Obj = { x: 0, y: 0 };
      obj.x = 1;
      obj.y = 2;
    "#,
    true,
  );

  annotate_top_level_effects(&mut program, &type_program);

  let cfg = program
    .top_level
    .ssa_body
    .as_ref()
    .unwrap_or(&program.top_level.body);
  let insts = collect_insts(cfg);

  let prop_assigns: Vec<_> = insts
    .into_iter()
    .filter(|inst| inst.t == InstTyp::PropAssign)
    .collect();
  assert_eq!(
    prop_assigns.len(),
    2,
    "expected exactly two PropAssign instructions"
  );

  let mut fields = Vec::new();
  for inst in &prop_assigns {
    assert!(
      !inst.meta.effects.writes.contains(&EffectLocation::Heap),
      "expected field-level modeling (no Heap) but got: {:?}",
      inst.meta.effects.writes
    );
    assert_eq!(
      inst.meta.effects.writes.len(),
      1,
      "expected exactly one write location but got {:?}",
      inst.meta.effects.writes
    );
    let loc = inst.meta.effects.writes.iter().next().unwrap();
    match loc {
      EffectLocation::AllocField { alloc, key } => fields.push((alloc.clone(), key.clone())),
      other => panic!("expected EffectLocation::AllocField but got {other:?}"),
    }
  }

  assert_eq!(
    fields[0].0, fields[1].0,
    "expected both writes to use the same allocation site"
  );
  assert_ne!(
    fields[0].1, fields[1].1,
    "expected writes to have different field keys"
  );

  let keys: Vec<_> = fields.into_iter().map(|(_, key)| key).collect();
  assert!(
    keys.contains(&"x".to_string()) && keys.contains(&"y".to_string()),
    "expected keys \"x\" and \"y\" but got {keys:?}"
  );

  assert!(
    !prop_assigns[0]
      .meta
      .effects
      .conflicts_with(&prop_assigns[1].meta.effects),
    "expected writes to different fields to not conflict"
  );
}

#[test]
fn write_conflicts_with_read_of_same_field_but_not_other_field() {
  let (mut program, type_program) = compile_with_typecheck(
    r#"
      interface Obj { x: number; y: number; }
      const obj: Obj = { x: 0, y: 0 };
      obj.x = 1;
      const a = obj.x;
      const b = obj.y;
      void a; void b;
    "#,
    true,
  );

  annotate_top_level_effects(&mut program, &type_program);

  let cfg = program
    .top_level
    .ssa_body
    .as_ref()
    .unwrap_or(&program.top_level.body);
  let insts = collect_insts(cfg);

  let write_x = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::PropAssign
        && matches!(inst.args.get(1), Some(Arg::Const(Const::Str(s))) if s == "x")
    })
    .expect("expected PropAssign to obj.x");
  let read_x = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Bin
        && inst.bin_op == BinOp::GetProp
        && matches!(inst.args.get(1), Some(Arg::Const(Const::Str(s))) if s == "x")
    })
    .expect("expected GetProp for obj.x");
  let read_y = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Bin
        && inst.bin_op == BinOp::GetProp
        && matches!(inst.args.get(1), Some(Arg::Const(Const::Str(s))) if s == "y")
    })
    .expect("expected GetProp for obj.y");

  let write_loc = write_x
    .meta
    .effects
    .writes
    .iter()
    .next()
    .expect("write should have at least one location")
    .clone();
  let read_x_loc = read_x
    .meta
    .effects
    .reads
    .iter()
    .next()
    .expect("read x should have at least one location")
    .clone();
  let read_y_loc = read_y
    .meta
    .effects
    .reads
    .iter()
    .next()
    .expect("read y should have at least one location")
    .clone();

  assert_eq!(write_loc, read_x_loc, "expected same field location for x");
  assert_ne!(write_loc, read_y_loc, "expected different field locations for x vs y");

  assert!(
    write_x.meta.effects.conflicts_with(&read_x.meta.effects),
    "expected write to obj.x to conflict with read of obj.x"
  );
  assert!(
    !write_x.meta.effects.conflicts_with(&read_y.meta.effects),
    "expected write to obj.x to not conflict with read of obj.y"
  );
}

#[test]
fn non_strict_native_falls_back_to_heap_effects() {
  let (mut program, type_program) = compile_with_typecheck(
    r#"
      interface Obj { x: number; y: number; }
      const obj: Obj = { x: 0, y: 0 };
      obj.x = 1;
    "#,
    false,
  );

  annotate_top_level_effects(&mut program, &type_program);

  let cfg = program
    .top_level
    .ssa_body
    .as_ref()
    .unwrap_or(&program.top_level.body);
  let insts = collect_insts(cfg);

  let assign = insts
    .iter()
    .find(|inst| inst.t == InstTyp::PropAssign)
    .expect("expected PropAssign");
  assert!(
    assign.meta.effects.writes.contains(&EffectLocation::Heap),
    "expected Heap write in non-strict-native mode but got {:?}",
    assign.meta.effects.writes
  );
  assert!(
    !assign
      .meta
      .effects
      .writes
      .iter()
      .any(|loc| matches!(loc, EffectLocation::AllocField { .. })),
    "expected no AllocField locations in non-strict-native mode but got {:?}",
    assign.meta.effects.writes
  );
}

#[test]
fn two_allocations_same_field_do_not_conflict() {
  let (mut program, type_program) = compile_with_typecheck(
    r#"
      interface Obj { x: number; }
      const a: Obj = { x: 0 };
      const b: Obj = { x: 1 };
      a.x = 1;
      b.x = 2;
    "#,
    true,
  );

  annotate_top_level_effects(&mut program, &type_program);

  let cfg = program
    .top_level
    .ssa_body
    .as_ref()
    .unwrap_or(&program.top_level.body);
  let insts = collect_insts(cfg);

  let prop_assigns: Vec<_> = insts
    .into_iter()
    .filter(|inst| {
      inst.t == InstTyp::PropAssign
        && matches!(inst.args.get(1), Some(Arg::Const(Const::Str(s))) if s == "x")
    })
    .collect();
  assert_eq!(
    prop_assigns.len(),
    2,
    "expected exactly two PropAssign instructions to .x"
  );

  let loc_a = prop_assigns[0]
    .meta
    .effects
    .writes
    .iter()
    .next()
    .expect("expected a write location")
    .clone();
  let loc_b = prop_assigns[1]
    .meta
    .effects
    .writes
    .iter()
    .next()
    .expect("expected a write location")
    .clone();
  assert_ne!(loc_a, loc_b, "expected different allocation sites to differ");
  assert!(
    !prop_assigns[0]
      .meta
      .effects
      .conflicts_with(&prop_assigns[1].meta.effects),
    "expected writes to different allocations to not conflict"
  );
}

#[test]
fn same_allocation_same_field_conflicts() {
  let (mut program, type_program) = compile_with_typecheck(
    r#"
      interface Obj { x: number; }
      const a: Obj = { x: 0 };
      a.x = 1;
      a.x = 2;
    "#,
    true,
  );

  annotate_top_level_effects(&mut program, &type_program);

  let cfg = program
    .top_level
    .ssa_body
    .as_ref()
    .unwrap_or(&program.top_level.body);
  let insts = collect_insts(cfg);

  let prop_assigns: Vec<_> = insts
    .into_iter()
    .filter(|inst| inst.t == InstTyp::PropAssign)
    .collect();
  assert_eq!(
    prop_assigns.len(),
    2,
    "expected exactly two PropAssign instructions"
  );
  let loc1 = prop_assigns[0]
    .meta
    .effects
    .writes
    .iter()
    .next()
    .expect("expected a write location")
    .clone();
  let loc2 = prop_assigns[1]
    .meta
    .effects
    .writes
    .iter()
    .next()
    .expect("expected a write location")
    .clone();
  assert_eq!(loc1, loc2, "expected same field location for two writes");
  assert!(
    prop_assigns[0]
      .meta
      .effects
      .conflicts_with(&prop_assigns[1].meta.effects),
    "expected writes to same field to conflict"
  );
}

#[test]
fn dynamic_key_falls_back_to_heap_effects_even_in_strict_native() {
  let (mut program, type_program) = compile_with_typecheck(
    r#"
      const arr = [0, 1, 2];
      let i = 0;
      arr[i] = 1;
    "#,
    true,
  );

  annotate_top_level_effects(&mut program, &type_program);

  let cfg = program
    .top_level
    .ssa_body
    .as_ref()
    .unwrap_or(&program.top_level.body);
  let insts = collect_insts(cfg);

  let assign = insts
    .iter()
    .find(|inst| inst.t == InstTyp::PropAssign && matches!(inst.args.get(1), Some(Arg::Var(_))))
    .expect("expected PropAssign with a dynamic key");
  assert!(
    assign.meta.effects.writes.contains(&EffectLocation::Heap),
    "expected Heap write for dynamic key but got {:?}",
    assign.meta.effects.writes
  );
  assert!(
    !assign
      .meta
      .effects
      .writes
      .iter()
      .any(|loc| matches!(loc, EffectLocation::AllocField { .. })),
    "expected no AllocField locations for dynamic key but got {:?}",
    assign.meta.effects.writes
  );
}
