use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

fn native_js_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("native-js-cli")
}

#[test]
fn compiles_and_runs_two_file_project() {
  let dir = tempdir().unwrap();
  let math = dir.path().join("math.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &math,
    "export function add(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import {add} from './math';\nexport function main(){console.log(add(1,2));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("3\n"));
}

#[test]
fn supports_import_aliases() {
  let dir = tempdir().unwrap();
  let math = dir.path().join("math.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &math,
    "export function add(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import {add as plus} from './math';\nexport function main(){console.log(plus(2,3));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("5\n"));
}

#[test]
fn runs_module_initializers_in_dependency_order() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "console.log(\"dep\");\n").unwrap();
  fs::write(
    &main,
    "import './dep';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn supports_non_number_function_signatures_across_modules() {
  let dir = tempdir().unwrap();
  let util = dir.path().join("util.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &util,
    "export function not(x: boolean): boolean { return !x }\nexport function hello(): string { return \"hi\" }\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import {not, hello} from './util';\nexport function main(){console.log(not(false), hello());}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("true hi\n"));
}

#[test]
fn errors_on_unsupported_import_syntax() {
  let dir = tempdir().unwrap();
  let math = dir.path().join("math.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &math,
    "export function add(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  // Default imports are out of scope for the minimal module subset.
  fs::write(
    &main,
    "import add from './math';\nexport function main(){console.log(add(1,2));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .failure()
    .stderr(predicate::str::contains("unsupported import syntax"));
}

#[test]
fn resolves_node_modules_package_exports() {
  let dir = tempdir().unwrap();

  let pkg_dir = dir.path().join("node_modules").join("pkg");
  fs::create_dir_all(pkg_dir.join("src")).unwrap();
  fs::write(
    pkg_dir.join("package.json"),
    r#"{ "name": "pkg", "exports": { ".": { "types": "./src/index.ts" } } }"#,
  )
  .unwrap();
  fs::write(
    pkg_dir.join("src").join("index.ts"),
    "export function add(a:number,b:number){return a+b}\n",
  )
  .unwrap();

  let main = dir.path().join("main.ts");
  fs::write(
    &main,
    "import {add} from 'pkg';\nexport function main(){console.log(add(10,32));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("42\n"));
}

#[test]
fn errors_on_cycles_deterministically() {
  let dir = tempdir().unwrap();
  let a = dir.path().join("a.ts");
  let b = dir.path().join("b.ts");

  fs::write(&a, "import {b} from './b';\nexport function a(){return b()}\n").unwrap();
  fs::write(&b, "import {a} from './a';\nexport function b(){return a()}\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&a)
    .assert()
    .failure()
    .stderr(predicate::str::contains("cyclic module dependency"));
}
