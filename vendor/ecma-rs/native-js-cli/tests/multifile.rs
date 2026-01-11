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
