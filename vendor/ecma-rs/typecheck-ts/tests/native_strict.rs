use diagnostics::TextRange;
use std::sync::Arc;
use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{FileId, FileKey, MemoryHost, Program};

const STRICT_LIB_KEY: &str = "native_strict_globals.d.ts";
const STRICT_LIB: &str = r#"
interface Array<T> {
  [index: number]: T;
}
interface Boolean {}
interface IArguments {}
interface Number {}
interface Object {}
interface RegExp {}
interface String {}

declare const arguments: IArguments;

declare function eval(x: string): unknown;

interface Function {
  call: (this_arg: unknown, ...args: unknown[]) => unknown;
  apply: (this_arg: unknown, args: unknown[]) => unknown;
  bind: (this_arg: unknown, ...args: unknown[]) => Function;
}
declare const Function: { new (...args: string[]): Function; (...args: string[]): Function; prototype: Function };

declare const Object: {
  setPrototypeOf: (o: object, p: object) => void;
  defineProperty: (o: object, key: string, desc: object) => void;
  defineProperties: (o: object, props: object) => void;
  assign: (...args: object[]) => void;
};

declare const Reflect: {
  setPrototypeOf: (o: object, p: object) => void;
  defineProperty: (o: object, key: string, desc: object) => void;
  apply: (target: Function, this_arg: unknown, args: unknown[]) => unknown;
  construct: (target: Function, args: unknown[]) => unknown;
};

declare const Proxy: {
  new <T extends object>(target: T, handler: object): T;
  revocable: (target: object, handler: object) => { proxy: object };
};

interface GlobalThis {
  eval: typeof eval;
  Function: typeof Function;
  Object: typeof Object;
  Reflect: typeof Reflect;
  Proxy: typeof Proxy;
  globalThis: GlobalThis;
};
declare const globalThis: GlobalThis;
"#;

fn check(source: &str, native_strict: bool) -> (Vec<typecheck_ts::Diagnostic>, FileId) {
  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    native_strict,
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(LibFile {
    key: FileKey::new(STRICT_LIB_KEY),
    name: Arc::from(STRICT_LIB_KEY),
    kind: FileKind::Dts,
    text: Arc::from(STRICT_LIB),
  });
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  (program.check(), file_id)
}

#[test]
fn native_strict_bans_any() {
  let source = "const x = 1 as any;";
  let (diagnostics, file_id) = check(source, true);
  let needle = "1 as any";
  let start = source.find(needle).expect("needle") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_ANY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict any diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval() {
  let source = "eval(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_comma_callee() {
  let source = "(0, eval)(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_const_alias() {
  let source = "const e = eval;\ne(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "e(\"1\")";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + 1);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_global_this_const_alias() {
  let source = "const g = globalThis;\ng.eval(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "g.eval";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_type_assertion_callee() {
  let source = "(eval as typeof eval)(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  // The callee expression span includes the closing `)` (but not the opening `(`).
  let needle = "eval as typeof eval)";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_non_null_callee() {
  let source = "eval!(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "eval!";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_const_alias_type_assertion_init() {
  let source = "const e = eval as typeof eval;\ne(\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "e(\"1\")";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + 1);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_call_via_comma_object() {
  let source = "(0, eval).call(null, \"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "0, eval).call";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_call() {
  let source = "eval.call(null, \"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "eval.call";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_function_prototype_call_call() {
  let source = "Function.prototype.call.call(eval, null, \"1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_function_call_call() {
  let source = "Function.call.call(eval, null, \"1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_function_prototype_call_apply() {
  let source = "Function.prototype.call.apply(eval, [null, \"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_function_prototype_apply_call() {
  let source = "Function.prototype.apply.call(eval, null, [\"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_function_prototype_call_bind_then_call() {
  let source = "Function.prototype.call.bind(eval)(null, \"1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_function_prototype_bind_call() {
  let source = "Function.prototype.bind.call(eval, null, \"1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply() {
  let source = "Reflect.apply(eval, null, [\"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_bind() {
  let source = "Reflect.apply.bind(Reflect, eval, null, [\"1\"])();";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_function_prototype_bind() {
  let source = "Reflect.apply(Function.prototype.bind, eval, [null, \"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_function_prototype_call() {
  let source = "Reflect.apply(Function.prototype.call, eval, [null, \"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_call_function_prototype_call() {
  let source = "Reflect.apply.call(Reflect, Function.prototype.call, eval, [null, \"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_call_function_prototype_bind() {
  let source = "Reflect.apply.call(Reflect, Function.prototype.bind, eval, [null, \"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_function_prototype_apply() {
  let source = "Reflect.apply(Function.prototype.apply, eval, [null, [\"1\"]]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_function_call() {
  let source = "Reflect.apply(Function.call, eval, [null, \"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_function_apply() {
  let source = "Reflect.apply(Function.apply, eval, [null, [\"1\"]]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_call() {
  let source = "Reflect.apply.call(Reflect, eval, null, [\"1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_eval_via_reflect_apply_apply() {
  let source = "Reflect.apply.apply(Reflect, [eval, null, [\"1\"]]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("eval").expect("eval") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_new_function() {
  let source = "new Function(\"return 1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict new Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_comma_callee() {
  let source = "(0, Function)(\"return 1\");";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_type_assertion_callee() {
  let source = "(Function as typeof Function)(\"return 1\");";
  let (diagnostics, file_id) = check(source, true);
  // The callee expression span includes the closing `)` (but not the opening `(`).
  let needle = "Function as typeof Function)";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_call() {
  let source = "Function.call(null, \"return 1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Function.call";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_reflect_apply() {
  let source = "Reflect.apply(Function, null, [\"return 1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_reflect_apply_function_prototype_bind() {
  let source = "Reflect.apply(Function.prototype.bind, Function, [null, \"return 1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function,").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_reflect_construct() {
  let source = "Reflect.construct(Function, [\"return 1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_reflect_construct_call() {
  let source = "Reflect.construct.call(Reflect, Function, [\"return 1\"]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_reflect_construct_apply() {
  let source =
    "Reflect.construct.apply(Reflect, [Function, [\"return 1\"]]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_function_via_reflect_construct_bind() {
  let source = "Reflect.construct.bind(Reflect, Function, [\"return 1\"])();";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Function").expect("Function") as u32;
  let span = TextRange::new(start, start + "Function".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_this_function() {
  let source = "globalThis.Function(\"return 1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis.Function";
  let start = source.find(needle).expect(needle) as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict globalThis.Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_with_statement() {
  let source = "with ({}) { }";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("with").expect("with") as u32;
  let span = TextRange::new(start, start + 4);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_WITH.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict with diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_arguments_in_non_arrow_function() {
  let source = "function f() { return arguments; }";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("arguments").expect("arguments") as u32;
  let span = TextRange::new(start, start + "arguments".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_ARGUMENTS.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict arguments diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_arguments_bindings() {
  let source = "function f(arguments: number) { return 0; }";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("arguments").expect("arguments") as u32;
  let span = TextRange::new(start, start + "arguments".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_ARGUMENTS.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict arguments diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_unsafe_type_assertions() {
  let source = "const x = {} as { a: number };";
  let (diagnostics, file_id) = check(source, true);
  let needle = "{} as { a: number }";
  let start = source.find(needle).expect("assertion") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_UNSAFE_ASSERTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict unsafe assertion diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_non_null_assertions_on_maybe_nullish_values() {
  let source = "const x: string | null = null; x!.length;";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("x!.length").expect("x!.length") as u32;
  let span = TextRange::new(start, start + 2);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NONNULL_ASSERTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict non-null assertion diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_non_constant_computed_member_keys() {
  let source = r#"
  const obj: { [key: string]: number } = { a: 1 };
  const k = 'a';
  obj[k];
  obj[`a`];
  obj["a"];
"#;
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("obj[k]").expect("obj[k]") as u32 + 4;
  let span = TextRange::new(start, start + 1);
  let computed: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str())
    .collect();
  assert_eq!(
    computed.len(),
    1,
    "expected exactly one computed-key diagnostic, got {computed:?} (all={diagnostics:?})",
  );
  assert_eq!(computed[0].primary.file, file_id);
  assert_eq!(computed[0].primary.range, span);
}

#[test]
fn native_strict_bans_non_constant_computed_class_member_keys() {
  let source = r#"
  const key = 'a';
  class C {
    [key](): number { return 1; }
    ["a"](): number { return 2; }
    [`a`](): number { return 3; }
  }
"#;
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("[key]").expect("[key]") as u32 + 1;
  let span = TextRange::new(start, start + "key".len() as u32);
  let computed: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str())
    .collect();
  assert_eq!(
    computed.len(),
    1,
    "expected exactly one computed-key diagnostic, got {computed:?} (all={diagnostics:?})",
  );
  assert_eq!(computed[0].primary.file, file_id);
  assert_eq!(computed[0].primary.range, span);
}

#[test]
fn native_strict_bans_non_constant_computed_object_literal_keys() {
  let source = "const key = \"a\"; const obj = { [key]: 1 };";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("[key]").expect("[key]") as u32 + 1;
  let span = TextRange::new(start, start + "key".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected computed-key diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_new_proxy() {
  let source = "new Proxy({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Proxy").expect("Proxy") as u32;
  let span = TextRange::new(start, start + "Proxy".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict new Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_new_proxy_via_comma_callee() {
  let source = "new (0, Proxy)({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Proxy").expect("Proxy") as u32;
  let span = TextRange::new(start, start + "Proxy".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_new_proxy_via_type_assertion_callee() {
  let source = "new (Proxy as typeof Proxy)({}, {});";
  let (diagnostics, file_id) = check(source, true);
  // The callee expression span includes the closing `)` (but not the opening `(`).
  let needle = "Proxy as typeof Proxy)";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_proxy_revocable_via_comma_object() {
  let source = "(0, Proxy).revocable({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "0, Proxy).revocable";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_proxy_revocable_call() {
  let source = "Proxy.revocable.call(null, {}, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Proxy.revocable.call";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_proxy_revocable_via_reflect_apply() {
  let source = "Reflect.apply(Proxy.revocable, null, [{}, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Proxy.revocable";
  let start = source.find(needle).expect("target") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_proxy_via_reflect_construct() {
  let source = "Reflect.construct(Proxy, [{}, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let start = source.find("Proxy").expect("Proxy") as u32;
  let span = TextRange::new(start, start + "Proxy".len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_on_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty(Foo.prototype, \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty(Foo.prototype, \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_on_prototype_via_const_alias() {
  let source = "declare const Foo: { prototype: object };\nconst p = Foo.prototype;\nObject.defineProperty(p, \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty(p, \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_call_on_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty.call(Object, Foo.prototype, \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.call(Object, Foo.prototype, \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_bind_on_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty.bind(Object, Foo.prototype, \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.bind(Object, Foo.prototype, \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_bind_then_call_on_prototype() {
  let source =
    "declare const Foo: { prototype: object };\nObject.defineProperty.bind(Object)(Foo.prototype, \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.bind(Object)(Foo.prototype, \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_bound_call_on_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty.bind(Object).call(null, Foo.prototype, \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.bind(Object).call(null, Foo.prototype, \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_bound_apply_on_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty.bind(Object).apply(null, [Foo.prototype, \"x\", {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.bind(Object).apply(null, [Foo.prototype, \"x\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_apply_on_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty.apply(Object, [Foo.prototype, \"x\", {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.apply(Object, [Foo.prototype, \"x\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_via_reflect_apply() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply(Object.defineProperty, Object, [Foo.prototype, \"x\", {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.defineProperty, Object, [Foo.prototype, \"x\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_bound_via_reflect_apply() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply(Object.defineProperty.bind(Object), Object, [Foo.prototype, \"x\", {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.defineProperty.bind(Object), Object, [Foo.prototype, \"x\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_update_on_prototype() {
  let source = "declare const Foo: { prototype: { x: number } };\nFoo.prototype.x++;";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Foo.prototype.x++";
  let start = source.find(needle).expect("expr") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_update_on_const_alias_prototype() {
  let source = "declare const Foo: { prototype: { x: number } };\nconst p = Foo.prototype;\np.x++;";
  let (diagnostics, file_id) = check(source, true);
  let needle = "p.x++";
  let start = source.find(needle).expect("expr") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_destructuring_assign_to_prototype() {
  let source =
    "declare const Foo: { prototype: object };\n({ x: Foo.prototype } = { x: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "{ x: Foo.prototype } = { x: {} }";
  let start = source.find(needle).expect("expr") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_delete_on_prototype() {
  let source = "declare const Foo: { prototype: { x: number } };\ndelete Foo.prototype.x;";
  let (diagnostics, file_id) = check(source, true);
  let needle = "delete Foo.prototype.x";
  let start = source.find(needle).expect("expr") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty(Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty(Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_template_literal() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty(Foo, `prototype`, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty(Foo, `prototype`, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_const_alias() {
  let source =
    "declare const Foo: { prototype: object };\nconst dp = Object.defineProperty;\ndp(Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "dp(Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_bind_then_call_zero_args() {
  let source =
    "declare const Foo: { prototype: object };\nObject.defineProperty.bind()(Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty.bind()(Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_function_prototype_call_call() {
  let source = "declare const Foo: { prototype: object };\nFunction.prototype.call.call(Object.defineProperty, Object, Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Function.prototype.call.call(Object.defineProperty, Object, Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_function_prototype_call_call_bound() {
  let source = "declare const Foo: { prototype: object };\nFunction.prototype.call.call(Object.defineProperty.bind(Object), Object, Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle =
    "Function.prototype.call.call(Object.defineProperty.bind(Object), Object, Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_function_prototype_call_bind() {
  let source = "declare const Foo: { prototype: object };\nFunction.prototype.call.bind(Object.defineProperty, Object, Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Function.prototype.call.bind(Object.defineProperty, Object, Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_function_prototype_bind_bind() {
  let source = "declare const Foo: { prototype: object };\nFunction.prototype.bind.bind(Object.defineProperty, Object, Foo, \"prototype\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Function.prototype.bind.bind(Object.defineProperty, Object, Foo, \"prototype\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_reflect_apply_bind() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply.bind(Reflect, Object.defineProperty, Object, [Foo, \"prototype\", {}])();";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply.bind(Reflect, Object.defineProperty, Object, [Foo, \"prototype\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_reflect_apply_function_prototype_call() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply(Function.prototype.call, Object.defineProperty, [Object, Foo, \"prototype\", {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Function.prototype.call, Object.defineProperty, [Object, Foo, \"prototype\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_of_prototype_key_via_reflect_apply_function_prototype_call_bound() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply(Function.prototype.call, Object.defineProperty.bind(Object), [Object, Foo, \"prototype\", {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Function.prototype.call, Object.defineProperty.bind(Object), [Object, Foo, \"prototype\", {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperties(Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties(Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_call_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperties.call(Object, Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties.call(Object, Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_bind_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperties.bind(Object, Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties.bind(Object, Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_bind_then_call_of_prototype_key() {
  let source =
    "declare const Foo: { prototype: object };\nObject.defineProperties.bind(Object)(Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties.bind(Object)(Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_bound_call_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperties.bind(Object).call(null, Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties.bind(Object).call(null, Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_bound_apply_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperties.bind(Object).apply(null, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties.bind(Object).apply(null, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_apply_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperties.apply(Object, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties.apply(Object, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_via_reflect_apply() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply(Object.defineProperties, Object, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.defineProperties, Object, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_bound_via_reflect_apply() {
  let source = "declare const Foo: { prototype: object };\nReflect.apply(Object.defineProperties.bind(Object), Object, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.defineProperties.bind(Object), Object, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_properties_of_prototype_key_template_literal() {
  let source =
    "declare const Foo: { prototype: object };\nObject.defineProperties(Foo, { [`prototype`]: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperties(Foo, { [`prototype`]: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.assign(Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign(Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_call_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.assign.call(Object, Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign.call(Object, Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_bind_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.assign.bind(Object, Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign.bind(Object, Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_bind_then_call_of_prototype_key() {
  let source =
    "declare const Foo: { prototype: object };\nObject.assign.bind(Object)(Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign.bind(Object)(Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_bound_call_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.assign.bind(Object).call(null, Foo, { prototype: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign.bind(Object).call(null, Foo, { prototype: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_bound_apply_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.assign.bind(Object).apply(null, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign.bind(Object).apply(null, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_apply_of_prototype_key() {
  let source = "declare const Foo: { prototype: object };\nObject.assign.apply(Object, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign.apply(Object, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_via_reflect_apply() {
  let source =
    "declare const Foo: { prototype: object };\nReflect.apply(Object.assign, Object, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.assign, Object, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_bound_via_reflect_apply() {
  let source =
    "declare const Foo: { prototype: object };\nReflect.apply(Object.assign.bind(Object), Object, [Foo, { prototype: {} }]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.assign.bind(Object), Object, [Foo, { prototype: {} }])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_assign_of_prototype_key_template_literal() {
  let source =
    "declare const Foo: { prototype: object };\nObject.assign(Foo, { [`prototype`]: {} });";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.assign(Foo, { [`prototype`]: {} })";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_eval_computed_property() {
  let source = "globalThis[\"eval\"](\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis[\"eval\"]";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_eval_template_literal_computed_property() {
  let source = "globalThis[`eval`](\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis[`eval`]";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_eval_via_globalthis_chain() {
  let source = "globalThis.globalThis[\"eval\"](\"1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis.globalThis[\"eval\"]";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_EVAL.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict eval diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_proxy_revocable_computed_property() {
  let source = "Proxy[\"revocable\"]({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Proxy[\"revocable\"]";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_proxy_revocable_template_literal_computed_property() {
  let source = "Proxy[`revocable`]({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Proxy[`revocable`]";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_computed_property() {
  let source = "const value: object = {};\nObject[\"setPrototypeOf\"](value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object[\"setPrototypeOf\"](value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_template_literal_computed_property() {
  let source = "const value: object = {};\nObject[`setPrototypeOf`](value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object[`setPrototypeOf`](value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_define_property_on_template_literal_prototype() {
  let source = "declare const Foo: { prototype: object };\nObject.defineProperty(Foo[`prototype`], \"x\", {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.defineProperty(Foo[`prototype`], \"x\", {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_function_computed_property() {
  let source = "globalThis[\"Function\"](\"return 1\");";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis[\"Function\"]";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Function diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_new_proxy() {
  let source = "new globalThis.Proxy({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis.Proxy";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_proxy_revocable() {
  let source = "globalThis.Proxy.revocable({}, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis.Proxy.revocable";
  let start = source.find(needle).expect("callee") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROXY.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected native_strict Proxy diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_global_object_set_prototype_of() {
  let source = "const value: object = {};\nglobalThis.Object.setPrototypeOf(value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "globalThis.Object.setPrototypeOf(value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_call() {
  let source =
    "const value: object = {};\nObject.setPrototypeOf.call(Object, value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.setPrototypeOf.call(Object, value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_comma_callee() {
  let source =
    "const value: object = {};\n(0, Object.setPrototypeOf)(value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "0, Object.setPrototypeOf)(value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_apply() {
  let source = "const value: object = {};\nObject.setPrototypeOf.apply(Object, [value, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.setPrototypeOf.apply(Object, [value, {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_function_prototype_call_call() {
  let source = "const value: object = {};\nFunction.prototype.call.call(Object.setPrototypeOf, Object, value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Function.prototype.call.call(Object.setPrototypeOf, Object, value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_function_prototype_call_bind_then_call() {
  let source = "const value: object = {};\nFunction.prototype.call.bind(Object.setPrototypeOf, Object)(value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Function.prototype.call.bind(Object.setPrototypeOf, Object)";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_bind() {
  let source = "const value: object = {};\nObject.setPrototypeOf.bind(Object, value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.setPrototypeOf.bind(Object, value, {})";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_bind_then_call() {
  let source =
    "const value: object = {};\nObject.setPrototypeOf.bind(Object)(value, {});";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Object.setPrototypeOf.bind(Object)";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_reflect_apply() {
  let source = "const value: object = {};\nReflect.apply(Object.setPrototypeOf, Object, [value, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply(Object.setPrototypeOf, Object, [value, {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_reflect_apply_bind() {
  let source = "const value: object = {};\nReflect.apply.bind(Reflect, Object.setPrototypeOf, Object, [value, {}])();";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply.bind(Reflect, Object.setPrototypeOf, Object, [value, {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_reflect_apply_call() {
  let source =
    "const value: object = {};\nReflect.apply.call(Reflect, Object.setPrototypeOf, Object, [value, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle = "Reflect.apply.call(Reflect, Object.setPrototypeOf, Object, [value, {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_reflect_apply_function_prototype_call() {
  let source = "const value: object = {};\nReflect.apply(Function.prototype.call, Object.setPrototypeOf, [Object, value, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle =
    "Reflect.apply(Function.prototype.call, Object.setPrototypeOf, [Object, value, {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_bans_set_prototype_of_via_reflect_apply_function_call() {
  let source = "const value: object = {};\nReflect.apply(Function.call, Object.setPrototypeOf, [Object, value, {}]);";
  let (diagnostics, file_id) = check(source, true);
  let needle =
    "Reflect.apply(Function.call, Object.setPrototypeOf, [Object, value, {}])";
  let start = source.find(needle).expect("call") as u32;
  let span = TextRange::new(start, start + needle.len() as u32);
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()
        && diag.primary.file == file_id
        && diag.primary.range == span
    }),
    "expected prototype mutation diagnostic at {span:?}, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_allows_array_indexing_with_number_key() {
  let source = "const xs: number[] = [1, 2]; const i = 0; void xs[i];";
  let (diagnostics, _) = check(source, true);
  assert!(
    !diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str()
    }),
    "did not expect computed-key diagnostic for array indexing, got {diagnostics:?}"
  );
}

#[test]
fn native_strict_is_opt_in() {
  let source = r#"
  const obj: { [key: string]: number } = { a: 1 };
  const k = 'a';
obj[k];

 function f() { return arguments; }
 eval("1");
 globalThis["Function"]("return 1");
 new Function("return 1");
 new globalThis.Proxy({}, {});
 new Proxy({}, {});
 const value: object = {};
 globalThis.Object.setPrototypeOf(value, {});
 const x = 1 as any;
 const y = {} as { a: number };
 const z: string | null = null;
 z!.length;
with ({}) { }
"#;
  let (diagnostics, _) = check(source, false);
  let strict_codes = [
    codes::NATIVE_STRICT_ANY.as_str(),
    codes::NATIVE_STRICT_EVAL.as_str(),
    codes::NATIVE_STRICT_NEW_FUNCTION.as_str(),
    codes::NATIVE_STRICT_WITH.as_str(),
    codes::NATIVE_STRICT_ARGUMENTS.as_str(),
    codes::NATIVE_STRICT_UNSAFE_ASSERTION.as_str(),
    codes::NATIVE_STRICT_NONNULL_ASSERTION.as_str(),
    codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str(),
    codes::NATIVE_STRICT_PROXY.as_str(),
  ];
  assert!(
    !diagnostics
      .iter()
      .any(|diag| strict_codes.contains(&diag.code.as_str())),
    "native_strict=false should not emit native strict diagnostics, got {diagnostics:?}"
  );
}
