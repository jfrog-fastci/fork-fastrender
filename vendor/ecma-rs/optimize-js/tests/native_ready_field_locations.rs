#![cfg(feature = "typed")]

use optimize_js::analysis::alias::AbstractLoc;
use optimize_js::il::inst::{EffectLocation, InstTyp};
use optimize_js::{compile_file_native_ready, NativeReadyOptions, TopLevelMode};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost};

fn compile_native_ready(source: &str, native_strict: bool) -> optimize_js::NativeReadyProgram {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    native_strict,
    ..Default::default()
  });
  let file_key = FileKey::new("input.ts");
  host.insert(file_key.clone(), source);
  let program = Arc::new(typecheck_ts::Program::new(host, vec![file_key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected typecheck diagnostics to be empty but got: {diagnostics:?}"
  );
  let file_id = program
    .file_id(&file_key)
    .expect("typecheck program should know the inserted file");

  compile_file_native_ready(
    program,
    file_id,
    TopLevelMode::Module,
    false,
    NativeReadyOptions {
      run_opt_passes: false,
      // When compiling without `native_strict`, strict-native validation may reject programs that
      // are still useful for analysis coverage (e.g. effect-location fallback behaviour).
      verify_strict_native: native_strict,
      ..NativeReadyOptions::default()
    },
  )
  .expect("compile_file_native_ready")
}

fn collect_prop_assign_effects(
  program: &optimize_js::Program,
) -> Vec<optimize_js::il::inst::EffectSet> {
  let cfg = program.top_level.analyzed_cfg();
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .filter(|inst| inst.t == InstTyp::PropAssign)
    .map(|inst| inst.meta.effects.clone())
    .collect()
}

#[test]
fn native_ready_strict_native_populates_field_locations() {
  let native = compile_native_ready(
    r#"
      declare function sink(x: unknown): void;
      interface Obj { x: number; y: number; }
      const obj: Obj = { x: 0, y: 0 };
      obj.x = 1;
      obj.y = 2;
      sink(obj);
    "#,
    true,
  );

  let effects = collect_prop_assign_effects(&native.program);
  assert_eq!(effects.len(), 2, "expected two PropAssign instructions");

  let mut fields: Vec<(AbstractLoc, String)> = Vec::new();
  for effects in effects {
    assert!(
      !effects.writes.contains(&EffectLocation::Heap),
      "expected strict-native to avoid Heap writes but got {:?}",
      effects.writes
    );
    assert_eq!(
      effects.writes.len(),
      1,
      "expected exactly one write location but got {:?}",
      effects.writes
    );
    let loc = effects.writes.iter().next().unwrap();
    match loc {
      // In strict-native mode, effect analysis should model constant-key property writes precisely.
      // When alias analysis can identify the receiver allocation site, this uses `AllocField`.
      EffectLocation::AllocField { alloc, key } => fields.push((alloc.clone(), key.clone())),
      other => panic!("expected EffectLocation::AllocField but got {other:?}"),
    }
  }

  assert_eq!(
    fields[0].0, fields[1].0,
    "expected both writes to target the same allocation site"
  );
  assert_ne!(
    fields[0].1, fields[1].1,
    "expected writes to have different field keys"
  );
}

#[test]
fn native_ready_non_strict_native_falls_back_to_heap() {
  let native = compile_native_ready(
    r#"
      declare function sink(x: unknown): void;
      interface Obj { x: number; }
      const obj: Obj = { x: 0 };
      obj.x = 1;
      sink(obj);
    "#,
    false,
  );

  let effects = collect_prop_assign_effects(&native.program);
  assert_eq!(effects.len(), 1, "expected one PropAssign instruction");
  let effects = &effects[0];

  assert!(
    effects.writes.contains(&EffectLocation::Heap),
    "expected Heap write in non-strict-native mode but got {:?}",
    effects.writes
  );
  assert!(
    !effects
      .writes
      .iter()
      .any(|loc| matches!(loc, EffectLocation::AllocField { .. } | EffectLocation::Field { .. })),
    "expected no field-sensitive locations in non-strict-native mode but got {:?}",
    effects.writes
  );
}
