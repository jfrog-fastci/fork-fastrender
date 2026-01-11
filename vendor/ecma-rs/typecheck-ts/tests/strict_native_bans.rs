use typecheck_ts::codes;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn diagnostics_codes(diagnostics: &[typecheck_ts::Diagnostic]) -> Vec<&str> {
  let mut codes: Vec<&str> = diagnostics.iter().map(|d| d.code.as_str()).collect();
  codes.sort();
  codes.dedup();
  codes
}

#[test]
fn strict_native_emits_bans() {
  let mut options = CompilerOptions::default();
  options.strict_native = true;

  let mut host = MemoryHost::with_options(options);
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
declare const eval: (code: string) => any;
declare const Proxy: any;
declare const arguments: { length: number };

const obj: any = {};

with (obj) {}
eval("1+1");

Function("return 1");
new Function("return 1");

new Proxy({}, {});
Proxy.revocable({}, {});

function f() {
  return arguments.length;
}

obj.__proto__ = null;
Object.setPrototypeOf(obj, null);

const dict: { [k: string]: number } = { x: 1 };
let key: string = "x";
dict[key];
dict["x"];
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  let seen = diagnostics_codes(&diagnostics);

  assert!(
    seen.contains(&codes::STRICT_NATIVE_FORBIDDEN_WITH.as_str()),
    "expected TN0102 with diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::STRICT_NATIVE_FORBIDDEN_EVAL.as_str()),
    "expected TN0100 eval diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::STRICT_NATIVE_FORBIDDEN_FUNCTION_CONSTRUCTOR.as_str()),
    "expected TN0101 Function constructor diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::STRICT_NATIVE_FORBIDDEN_PROXY.as_str()),
    "expected TN0103 Proxy diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::STRICT_NATIVE_FORBIDDEN_ARGUMENTS.as_str()),
    "expected TN0104 arguments diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::STRICT_NATIVE_FORBIDDEN_PROTOTYPE_MUTATION.as_str()),
    "expected TN0105 prototype mutation diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::STRICT_NATIVE_COMPUTED_KEY_NOT_CONSTANT.as_str()),
    "expected TN0106 computed key diagnostic, got {seen:?}"
  );
}

#[test]
fn strict_native_disabled_suppresses_bans() {
  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
declare const eval: (code: string) => any;
declare const Proxy: any;
declare const arguments: { length: number };

const obj: any = {};

with (obj) {}
eval("1+1");

Function("return 1");
new Function("return 1");

new Proxy({}, {});
Proxy.revocable({}, {});

function f() {
  return arguments.length;
}

obj.__proto__ = null;
Object.setPrototypeOf(obj, null);

const dict: { [k: string]: number } = { x: 1 };
let key: string = "x";
dict[key];
dict["x"];
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  let seen = diagnostics_codes(&diagnostics);

  for forbidden in [
    codes::STRICT_NATIVE_FORBIDDEN_WITH.as_str(),
    codes::STRICT_NATIVE_FORBIDDEN_EVAL.as_str(),
    codes::STRICT_NATIVE_FORBIDDEN_FUNCTION_CONSTRUCTOR.as_str(),
    codes::STRICT_NATIVE_FORBIDDEN_PROXY.as_str(),
    codes::STRICT_NATIVE_FORBIDDEN_ARGUMENTS.as_str(),
    codes::STRICT_NATIVE_FORBIDDEN_PROTOTYPE_MUTATION.as_str(),
    codes::STRICT_NATIVE_COMPUTED_KEY_NOT_CONSTANT.as_str(),
  ] {
    assert!(
      !seen.contains(&forbidden),
      "did not expect strict_native diagnostic {forbidden}, got {seen:?}"
    );
  }
}
