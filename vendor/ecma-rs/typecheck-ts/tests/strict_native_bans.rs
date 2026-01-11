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
    seen.contains(&codes::NATIVE_STRICT_WITH.as_str()),
    "expected TC4003 with diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::NATIVE_STRICT_EVAL.as_str()),
    "expected TC4001 eval diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::NATIVE_STRICT_NEW_FUNCTION.as_str()),
    "expected TC4002 Function constructor diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::NATIVE_STRICT_PROXY.as_str()),
    "expected TC4008 Proxy diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::NATIVE_STRICT_ARGUMENTS.as_str()),
    "expected TC4004 arguments diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str()),
    "expected TC4009 prototype mutation diagnostic, got {seen:?}"
  );
  assert!(
    seen.contains(&codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str()),
    "expected TC4007 computed key diagnostic, got {seen:?}"
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
    codes::NATIVE_STRICT_WITH.as_str(),
    codes::NATIVE_STRICT_EVAL.as_str(),
    codes::NATIVE_STRICT_NEW_FUNCTION.as_str(),
    codes::NATIVE_STRICT_PROXY.as_str(),
    codes::NATIVE_STRICT_ARGUMENTS.as_str(),
    codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str(),
  ] {
    assert!(
      !seen.contains(&forbidden),
      "did not expect strict_native diagnostic {forbidden}, got {seen:?}"
    );
  }
}
