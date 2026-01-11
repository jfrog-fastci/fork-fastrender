#![cfg(feature = "typed")]

use optimize_js::il::inst::{BinOp, InstTyp};
use optimize_js::{compile_source_with_typecheck, TopLevelMode};
use std::sync::Arc;

#[test]
fn typed_lowering_attaches_type_ids_to_value_insts() {
  let source = r#"
    const sink = (x: number) => x;
    const outer = () => {
      const add2 = (a: number) => sink(a + 2);
      add2(3);
    };
    outer();
  "#;

  let mut host = typecheck_ts::MemoryHost::new();
  let file_key = typecheck_ts::FileKey::new("input.ts");
  host.insert(file_key.clone(), source);
  let type_program = Arc::new(typecheck_ts::Program::new(host, vec![file_key.clone()]));
  // Ensure we have body/type tables populated.
  let _ = type_program.check();
  let file_id = type_program
    .file_id(&file_key)
    .expect("typecheck program should know the inserted file");

  let program = compile_source_with_typecheck(
    source,
    TopLevelMode::Module,
    false,
    Arc::clone(&type_program),
    file_id,
  )
  .expect("compile typed input");

  let mut saw_add = false;
  let mut found: Option<typecheck_ts::TypeId> = None;
  for func in &program.functions {
    for (_, insts) in func.body.bblocks.all() {
      for inst in insts {
        if inst.t == InstTyp::Bin && inst.bin_op == BinOp::Add {
          saw_add = true;
          found = inst.meta.type_id;
          break;
        }
      }
      if found.is_some() {
        break;
      }
    }
    if found.is_some() {
      break;
    }
  }

  assert!(saw_add, "expected at least one BinOp::Add instruction in a nested function");
  let type_id =
    found.expect("expected a BinOp::Add instruction with a type id in a nested function");
  let rendered = type_program.display_type(type_id).to_string();
  assert_eq!(rendered, "number");
}
