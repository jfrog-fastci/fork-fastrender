use inkwell::context::Context;
use native_js::{codegen, strict};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn hir_codegen_emits_cycle_path_for_runtime_module_cycles() {
  let a_key = FileKey::new("a.ts");
  let b_key = FileKey::new("b.ts");

  let a_src = r#"
import { x } from "./b.ts";

export const a: number = 1;

export function main(): number {
  return x + a;
}
"#;

  let b_src = r#"
import { a } from "./a.ts";

export const x: number = a;
"#;

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.insert(a_key.clone(), a_src);
  host.insert(b_key.clone(), b_src);
  host.link(a_key.clone(), "./b.ts", b_key.clone());
  host.link(b_key.clone(), "./a.ts", a_key.clone());

  let program = Program::new(host, vec![a_key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file_a = program.file_id(&a_key).expect("file a id");
  let file_b = program.file_id(&b_key).expect("file b id");
  let strict_diags = strict::validate(&program, &[file_a, file_b]);
  assert!(strict_diags.is_empty(), "{strict_diags:#?}");
  let entrypoint = strict::entrypoint(&program, file_a).expect("valid entrypoint");

  let context = Context::create();
  let err = codegen::codegen(
    &context,
    &program,
    file_a,
    entrypoint,
    codegen::CodegenOptions::default(),
  )
  .unwrap_err();

  let diag = err
    .iter()
    .find(|d| d.code == "NJS0146")
    .expect("expected NJS0146 cyclic dependency diagnostic");

  assert!(
    diag.message.contains("a.ts -> b.ts -> a.ts"),
    "unexpected cycle message: {}",
    diag.message
  );

  // The primary span should point at an import edge in the cycle.
  assert_eq!(diag.primary.file, file_b);
}

