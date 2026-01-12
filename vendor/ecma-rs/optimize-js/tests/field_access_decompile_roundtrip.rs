#![cfg(feature = "typed")]

use emit_js::EmitOptions;
use optimize_js::decompile::program_to_js;
use optimize_js::{compile_source_with_typecheck_cfg_options, CompileCfgOptions, DecompileOptions, TopLevelMode};
use std::sync::Arc;

#[test]
fn typed_field_access_decompiles_through_program_to_js() {
  let src = r#"
    /// <reference no-default-lib="true" />
    let obj = { x: 1, y: 2 };
    obj.x = 3;
    obj.x;
  "#;

  let mut host = typecheck_ts::MemoryHost::new();
  let input = typecheck_ts::FileKey::new("input.ts");
  host.insert(input.clone(), src);
  let tc_program = Arc::new(typecheck_ts::Program::new(host, vec![input.clone()]));
  let _ = tc_program.check();
  let file_id = tc_program.file_id(&input).expect("typecheck file id");

  let cfg_options = CompileCfgOptions {
    keep_ssa: false,
    run_opt_passes: false,
    ..Default::default()
  };
  let program = compile_source_with_typecheck_cfg_options(
    src,
    TopLevelMode::Module,
    false,
    Arc::clone(&tc_program),
    file_id,
    cfg_options,
  )
  .expect("compile typed source");

  let bytes = program_to_js(&program, &DecompileOptions::default(), EmitOptions::minified())
    .expect("decompile typed program to JS");
  let js = std::str::from_utf8(&bytes).expect("emitted JS should be UTF-8");

  assert!(
    js.contains(".x"),
    "expected emitted JS to contain `.x` access, got: {js}"
  );
}
