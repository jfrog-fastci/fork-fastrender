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

  let store = type_program.interned_type_store();
  let expected_x = types_ts_interned::PropKey::String(store.intern_name_ref("x"));
  let expected_y = types_ts_interned::PropKey::String(store.intern_name_ref("y"));

  let mut fields = Vec::new();
  for inst in prop_assigns {
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
      EffectLocation::Field { shape, key } => fields.push((*shape, key.clone())),
      other => panic!("expected EffectLocation::Field but got {other:?}"),
    }
  }

  assert_eq!(
    fields[0].0, fields[1].0,
    "expected both writes to use the same shape"
  );
  assert_ne!(
    fields[0].1, fields[1].1,
    "expected writes to have different field keys"
  );

  let keys: Vec<_> = fields.into_iter().map(|(_, key)| key).collect();
  assert!(
    keys.contains(&expected_x) && keys.contains(&expected_y),
    "expected keys {expected_x:?} and {expected_y:?} but got {keys:?}"
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

  assert_eq!(
    write_loc, read_x_loc,
    "expected write to obj.x to conflict with read of obj.x"
  );
  assert_ne!(
    write_loc, read_y_loc,
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
      .any(|loc| matches!(loc, EffectLocation::Field { .. })),
    "expected no Field locations in non-strict-native mode but got {:?}",
    assign.meta.effects.writes
  );
}
