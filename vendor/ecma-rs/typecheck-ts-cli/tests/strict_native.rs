use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use typecheck_ts::codes;

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn typecheck_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-cli")
}

#[test]
fn strict_native_reports_explicit_any() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: any = 1;\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TC4000"));
}

#[test]
fn native_strict_reports_any_in_unused_type_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "type T = any;\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg(entry.as_os_str())
    .assert()
    .success();

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_ANY.as_str()));
}

#[test]
fn native_strict_reports_any_in_unused_interface_member() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "interface X { x: any }\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg(entry.as_os_str())
    .assert()
    .success();

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_ANY.as_str()));
}

#[test]
fn native_strict_reports_any_hidden_in_dts_ref_object() {
  let tmp = tempdir().expect("temp dir");
  let types = tmp.path().join("types.d.ts");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &types,
    r#"
export interface Bad { x: any }
export declare const bad: Bad;
"#,
  )
  .expect("write types.d.ts");
  fs::write(
    &entry,
    r#"
import { bad } from "./types";
const v = bad;
"#,
  )
  .expect("write main.ts");

  // `.d.ts` files are intentionally skipped by the file-level `any` scan, so this
  // must be caught by walking the inferred type structure for `bad` / `v`.
  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg(entry.as_os_str())
    .assert()
    .success();

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TC4000"));
}

#[test]
fn native_strict_reports_any_hidden_in_dts_ref_callable() {
  let tmp = tempdir().expect("temp dir");
  let types = tmp.path().join("types.d.ts");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &types,
    r#"
export type Fn = (x: any) => number;
export declare const fn: Fn;
"#,
  )
  .expect("write types.d.ts");
  fs::write(
    &entry,
    r#"
import { fn } from "./types";
const f = fn;
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg(entry.as_os_str())
    .assert()
    .success();

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TC4000"));
}

#[test]
fn strict_native_reports_any_nested_in_object_type() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: { foo: any } = { foo: 1 };\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TC4000"));
}

#[test]
fn strict_native_reports_any_nested_in_type_ref() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    "type Foo = { foo: any };\nlet x: Foo = { foo: 1 };\n",
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TC4000"));
}

#[test]
fn strict_native_reports_any_nested_in_callable_type() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    "type Fn = (x: any) => void;\nconst f: Fn = () => {};\n",
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TC4000"));
}

#[test]
fn strict_native_json_includes_compiler_option() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: any = 1;\n").expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg("--json")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("valid JSON output");
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("strict_native"))
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected compiler_options.strict_native=true, got {json:?}"
  );
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("native_strict"))
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected compiler_options.native_strict=true, got {json:?}"
  );
}

#[test]
fn strict_native_reports_type_and_non_null_assertions() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    "let x = 1 as string;\nlet y: string | null = null;\nlet z = y!;\n",
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    stdout.contains("TC4005"),
    "expected TC4005 for unsafe type assertion, got {stdout}"
  );
  assert!(
    stdout.contains("TC4006"),
    "expected TC4006 for non-null assertion, got {stdout}"
  );
}

#[test]
fn strict_native_reports_forbidden_eval() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
eval("1+1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_destructuring_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval(code: string): unknown };
const { eval: e } = globalThis;
e("1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_destructuring_alias_call() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: { call(thisArg: unknown, code: string): unknown } };
const { eval: e } = globalThis;
e.call(null, "1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_reflect_apply_with_destructuring_alias_target() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { eval: e } = globalThis;
Reflect.apply(e, null, ["1"]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_reflect_apply_with_call_invoker_and_destructuring_alias_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { eval: e } = globalThis;
Reflect.apply(Function.prototype.call, e, [null, "1"]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_prototype_mutation_via_reflect_apply_with_call_invoker_and_destructuring_alias_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { defineProperty: dp } = Object;
Reflect.apply(Function.prototype.call, dp, [null, Foo, "prototype", {}]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_forbidden_eval_via_reflect_apply_call_with_destructuring_alias_target() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { eval: e } = globalThis;
Reflect.apply.call(Reflect, e, null, ["1"]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_prototype_mutation_via_reflect_apply_call_with_destructuring_alias_target() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { defineProperty: dp } = Object;
Reflect.apply.call(Reflect, dp, null, [Foo, "prototype", {}]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_forbidden_eval_via_reflect_apply_destructuring_alias_call_with_destructuring_alias_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { eval: e } = globalThis;
const { apply: a } = Reflect;
a.call(Reflect, e, null, ["1"]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_prototype_mutation_via_reflect_apply_destructuring_alias_call_with_destructuring_alias_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { defineProperty: dp } = Object;
const { apply: a } = Reflect;
a.call(Reflect, dp, null, [Foo, "prototype", {}]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_forbidden_eval_via_reflect_apply_destructuring_alias_with_call_invoker_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { eval: e } = globalThis;
const { apply: a } = Reflect;
a(Function.prototype.call, e, [null, "1"]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_prototype_mutation_via_reflect_apply_destructuring_alias_with_call_invoker_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { defineProperty: dp } = Object;
const { apply: a } = Reflect;
a(Function.prototype.call, dp, [null, Foo, "prototype", {}]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_forbidden_eval_via_reflect_apply_destructuring_alias_with_destructuring_alias_target(
) {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { eval: e } = globalThis;
const { apply: a } = Reflect;
a(e, null, ["1"]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_function_call_invoker_with_destructuring_alias_target() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
const { eval: e } = globalThis;
Function.prototype.call.call(e, null, "1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_function_call_invoker_destructuring_alias_receiver() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
const { eval: e } = globalThis;
const { call: c } = Function.prototype;
c.call(e, null, "1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_function_call_invoker_destructuring_alias_from_object() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
const { eval: e } = globalThis;
const { call: c } = Object;
c.call(e, null, "1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_forbidden_eval_via_function_call_invoker_destructuring_alias_from_function() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval: unknown };
const { eval: e } = globalThis;
const f = () => 1;
const { call: c } = f;
c.call(e, null, "1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_eval_through_outer_destructuring_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval(code: string): unknown };
const { eval: e } = globalThis;
export function main() {
  e("1");
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_eval_through_outer_destructuring_alias_then_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const globalThis: { eval(code: string): unknown };
const { eval: e } = globalThis;
const x = e;
export function main() {
  x("1");
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_eval_through_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare function eval(code: string): unknown;
const e = eval;
function f() {
  e("1+1");
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_function_through_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
const F = Function;
F("return 1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn strict_native_reports_proxy_through_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const Proxy: {
  new (target: object, handler: object): object;
};
const P = Proxy;
new P({}, {});
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_PROXY.as_str()));
}

#[test]
fn strict_native_reports_proxy_revocable_via_destructuring_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const Proxy: { revocable(target: object, handler: object): object };
const { revocable } = Proxy;
revocable({}, {});
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_PROXY.as_str()));
}

#[test]
fn strict_native_reports_proxy_revocable_through_outer_destructuring_alias_then_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const Proxy: { revocable(target: object, handler: object): object };
const { revocable } = Proxy;
const r = revocable;
export function main() {
  r({}, {});
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_PROXY.as_str()));
}

#[test]
fn strict_native_reports_prototype_mutation_via_destructuring_define_property() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const { defineProperty: dp } = Object;
dp(Foo, "prototype", {});
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_via_destructuring_define_property_call() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const { defineProperty: dp } = Object;
dp.call(null, Foo, "prototype", {});
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_via_destructuring_define_property_apply() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const { defineProperty: dp } = Object;
dp.apply(null, [Foo, "prototype", {}]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_via_reflect_apply_with_destructuring_alias_target() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
declare const Reflect: { apply(target: unknown, thisArg: unknown, args: unknown[]): unknown };
const { defineProperty: dp } = Object;
Reflect.apply(dp, null, [Foo, "prototype", {}]);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_through_outer_destructuring_define_property() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const { defineProperty: dp } = Object;
export function main() {
  dp(Foo, "prototype", {});
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_through_outer_destructuring_define_property_then_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const { defineProperty: dp } = Object;
const f = dp;
export function main() {
  f(Foo, "prototype", {});
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_via_destructuring_set_prototype_of() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
const { setPrototypeOf: sp } = Object;
sp({}, null);
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_reports_prototype_mutation_through_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const dp = Object.defineProperty;
function f() {
  dp(Foo, "prototype", { value: 1 });
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn strict_native_destructuring_alias_respects_block_shadowing() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
 declare const globalThis: { eval(code: string): unknown };
 const { eval: e } = globalThis;
 {
  const e: () => number = () => 1;
  e();
 }
"#,
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    !stdout.contains(codes::NATIVE_STRICT_EVAL.as_str()),
    "expected no {} diagnostics, got {stdout}",
    codes::NATIVE_STRICT_EVAL.as_str()
  );
}

#[test]
fn strict_native_aliases_respect_block_scoping() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
 declare const Proxy: {
   new (target: object, handler: object): object;
 };
 const P: () => number = () => 1;
 function f() {
   {
     const P = Proxy;
   }
   P();
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .success();
}

#[test]
fn native_strict_reports_forbidden_eval_via_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare function eval(code: string): unknown;
const e = eval;
export function main() {
  e("1+1");
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn native_strict_reports_forbidden_function_via_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
const F = Function;
export function main() {
  F("return 1")();
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn native_strict_reports_forbidden_proxy_via_outer_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const Proxy: { new (target: object, handler: object): object };
const P = Proxy;
export function main() {
  new P({}, {});
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_PROXY.as_str()));
}

#[test]
fn native_strict_reports_prototype_mutation_via_outer_const_define_property_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const dp = Object.defineProperty;
export function main() {
  dp(Foo, "prototype", {});
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn native_strict_reports_prototype_mutation_via_outer_const_set_prototype_of_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const sp = Object.setPrototypeOf;
export function main() {
  sp(Foo.prototype, null);
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn native_strict_reports_forbidden_eval_via_outer_const_alias_in_class_method() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare function eval(code: string): unknown;
const e = eval;
export class Foo {
  m(): void {
    e("1+1");
  }
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn native_strict_reports_forbidden_function_via_outer_const_alias_in_class_method() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
const F = Function;
export class Foo {
  m(): void {
    F("return 1")();
  }
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn native_strict_reports_forbidden_proxy_via_outer_const_alias_in_class_method() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare const Proxy: { new (target: object, handler: object): object };
const P = Proxy;
export class Foo {
  m(): void {
    new P({}, {});
  }
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_PROXY.as_str()));
}

#[test]
fn native_strict_reports_prototype_mutation_via_outer_const_define_property_alias_in_class_method() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const dp = Object.defineProperty;
export class Bar {
  m(): void {
    dp(Foo, "prototype", {});
  }
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn native_strict_reports_prototype_mutation_via_outer_const_set_prototype_of_alias_in_class_method() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
class Foo {}
const sp = Object.setPrototypeOf;
export class Bar {
  m(): void {
    sp(Foo.prototype, null);
  }
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_PROTOTYPE_MUTATION.as_str(),
    ));
}

#[test]
fn native_strict_outer_alias_respects_inner_shadowing_in_class_method() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare function eval(code: string): unknown;
const e = eval;
export class Foo {
  m(): void {
    {
      const e: () => number = () => 1;
      e();
    }
  }
}
"#,
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    !stdout.contains(codes::NATIVE_STRICT_EVAL.as_str()),
    "expected no {} diagnostics, got {stdout}",
    codes::NATIVE_STRICT_EVAL.as_str()
  );
}

#[test]
fn native_strict_outer_alias_respects_inner_shadowing() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
 declare function eval(code: string): unknown;
 const e = eval;
 export function main() {
   {
    const e: () => number = () => 1;
    e();
   }
 }
"#,
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg(entry.as_os_str())
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    !stdout.contains(codes::NATIVE_STRICT_EVAL.as_str()),
    "expected no {} diagnostics, got {stdout}",
    codes::NATIVE_STRICT_EVAL.as_str()
  );
}

#[test]
fn strict_native_reports_non_constant_computed_key() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
const dict: { [k: string]: number } = { x: 1 };
let key: string = "x";
dict[key];
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str(),
    ));
}

#[test]
fn native_strict_json_includes_legacy_compiler_option() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: any = 1;\n").expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--native-strict")
    .arg("--json")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("valid JSON output");
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("native_strict"))
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected compiler_options.native_strict=true, got {json:?}"
  );
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("strict_native"))
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected compiler_options.strict_native=true, got {json:?}"
  );
}

#[test]
fn native_strict_tsconfig_enables_strict_diagnostics() {
  let tmp = tempdir().expect("temp dir");
  let tsconfig = tmp.path().join("tsconfig.json");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &tsconfig,
    r#"{
  "compilerOptions": { "nativeStrict": true },
  "files": ["main.ts"]
}
"#,
  )
  .expect("write tsconfig.json");
  fs::write(&entry, "eval(\"1+1\");\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--project")
    .arg(tsconfig.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn tsconfig_native_strict_aliases_override_across_extends() {
  let tmp = tempdir().expect("temp dir");
  let base = tmp.path().join("base.json");
  let tsconfig = tmp.path().join("tsconfig.json");
  let entry = tmp.path().join("main.ts");

  fs::write(
    &base,
    r#"{
  "compilerOptions": { "nativeStrict": true },
  "files": ["main.ts"]
}
"#,
  )
  .expect("write base.json");
  fs::write(
    &tsconfig,
    r#"{
  "extends": "./base.json",
  "compilerOptions": { "strictNative": false }
}
"#,
  )
  .expect("write tsconfig.json");
  fs::write(
    &entry,
    r#"
eval("1+1");
"#,
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--project")
    .arg(tsconfig.as_os_str())
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("valid JSON output");
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("native_strict"))
      .and_then(|v| v.as_bool()),
    Some(false),
    "expected compiler_options.native_strict=false, got {json:?}"
  );
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("strict_native"))
      .and_then(|v| v.as_bool()),
    Some(false),
    "expected compiler_options.strict_native=false, got {json:?}"
  );
}

#[test]
fn tsconfig_strict_native_aliases_override_across_extends() {
  let tmp = tempdir().expect("temp dir");
  let base = tmp.path().join("base.json");
  let tsconfig = tmp.path().join("tsconfig.json");
  let entry = tmp.path().join("main.ts");

  fs::write(
    &base,
    r#"{
  "compilerOptions": { "strictNative": true },
  "files": ["main.ts"]
}
"#,
  )
  .expect("write base.json");
  fs::write(
    &tsconfig,
    r#"{
  "extends": "./base.json",
  "compilerOptions": { "nativeStrict": false }
}
"#,
  )
  .expect("write tsconfig.json");
  fs::write(
    &entry,
    r#"
eval("1+1");
"#,
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--project")
    .arg(tsconfig.as_os_str())
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("valid JSON output");
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("native_strict"))
      .and_then(|v| v.as_bool()),
    Some(false),
    "expected compiler_options.native_strict=false, got {json:?}"
  );
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("strict_native"))
      .and_then(|v| v.as_bool()),
    Some(false),
    "expected compiler_options.strict_native=false, got {json:?}"
  );
}

#[test]
fn native_strict_requires_strict_null_checks() {
  let tmp = tempdir().expect("temp dir");
  let tsconfig = tmp.path().join("tsconfig.json");
  let entry = tmp.path().join("main.ts");

  fs::write(
    &tsconfig,
    r#"{
  "compilerOptions": { "nativeStrict": true, "strictNullChecks": false },
  "files": ["main.ts"]
}
"#,
  )
  .expect("write tsconfig.json");
  fs::write(&entry, "export const x = 1;\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--project")
    .arg(tsconfig.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_REQUIRES_STRICT_NULL_CHECKS.as_str(),
    ));
}

#[test]
fn strict_native_reports_function_constructor_via_object_constructor() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "Object.constructor.call(null, \"return 1\");\n")
    .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn strict_native_reports_function_constructor_via_chained_constructor_access() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    "({}).constructor.constructor.call(null, \"return 1\");\n",
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn strict_native_allows_non_function_constructor_call_sanity() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "({}).constructor.call(null, {});\n").expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    !stdout.contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()),
    "did not expect {}, got {stdout}",
    codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
  );
}

#[test]
fn strict_native_reports_function_constructor_via_destructuring_constructor() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
export {};
declare const Object: { constructor: FunctionConstructor };
const { constructor: F } = Object;
F("return 1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn strict_native_reports_function_constructor_through_outer_destructuring_constructor_then_const_alias() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
export {};
declare const Object: { constructor: FunctionConstructor };
const { constructor: F } = Object;
const G = F;
export function main() {
  G("return 1");
}
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()));
}

#[test]
fn strict_native_allows_non_function_constructor_destructuring_sanity() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
export {};
const { constructor: C } = {};
C.call(null, {});
"#,
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck", "--lib", "es5"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    !stdout.contains(codes::NATIVE_STRICT_NEW_FUNCTION.as_str()),
    "did not expect {}, got {stdout}",
    codes::NATIVE_STRICT_NEW_FUNCTION.as_str()
  );
}
