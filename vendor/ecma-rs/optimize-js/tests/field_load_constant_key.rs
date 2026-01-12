#![cfg(feature = "typed")]

use optimize_js::il::inst::{FieldRef, InstTyp};
use optimize_js::{compile_source_with_typecheck_cfg_options, CompileCfgOptions, TopLevelMode};
use std::sync::Arc;
use types_ts_interned::{FieldKey, Layout, PropKey};

fn compile_with_typecheck(source: &str) -> (optimize_js::Program, Arc<typecheck_ts::Program>) {
  let mut host = typecheck_ts::MemoryHost::new();
  let input = typecheck_ts::FileKey::new("input.ts");
  host.insert(input.clone(), source);
  let tc_program = Arc::new(typecheck_ts::Program::new(host, vec![input.clone()]));
  let _ = tc_program.check();
  let file_id = tc_program.file_id(&input).expect("typecheck file id");

  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: false,
    ..Default::default()
  };
  let program = compile_source_with_typecheck_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    Arc::clone(&tc_program),
    file_id,
    cfg_options,
  )
  .expect("compile typed source");

  (program, tc_program)
}

#[test]
fn lowers_object_member_load_to_field_load_with_offset_meta() {
  let src = r#"
    /// <reference no-default-lib="true" />
    declare var console: { log: (...args: any[]) => void };

    let obj = { x: 1, y: 2 };
    console.log(obj.x);
  "#;

  let (program, tc_program) = compile_with_typecheck(src);
  let store = tc_program.interned_type_store();

  let mut matches = Vec::new();
  for (_label, block) in program.top_level.body.bblocks.all() {
    for inst in block.iter() {
      if inst.t != InstTyp::FieldLoad {
        continue;
      }
      if inst.field != FieldRef::Prop("x".to_string()) {
        continue;
      }
      matches.push(inst);
    }
  }

  assert_eq!(
    matches.len(),
    1,
    "expected exactly one FieldLoad for obj.x, got {}",
    matches.len()
  );
  let inst = matches[0];
  let meta = inst
    .meta
    .field_access
    .as_ref()
    .expect("FieldLoad should carry field_access meta");

  let Layout::Struct { fields, .. } = store.layout(meta.receiver_payload_layout) else {
    panic!(
      "expected receiver_payload_layout to be Struct, got {:?}",
      store.layout(meta.receiver_payload_layout)
    );
  };
  let field = fields
    .iter()
    .find(|field| match &field.key {
      FieldKey::Prop(PropKey::String(name)) => store.name(*name) == "x",
      _ => false,
    })
    .expect("expected struct layout to contain field x");

  assert_eq!(field.offset, meta.offset, "offset should match layout");
  assert_eq!(
    field.layout, meta.field_layout,
    "field layout id should match layout"
  );
  assert!(
    !meta.requires_write_barrier,
    "numeric field loads should not require a write barrier"
  );
}
