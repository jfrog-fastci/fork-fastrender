use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

fn native_js() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("native-js")
}

static AOT_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn aot_runs_module_initializers_in_dependency_order() {
  let _guard = AOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "print(1);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import \"./dep\";\nexport function main(): number { print(2); return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(Duration::from_secs(120))
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n"));
}

#[test]
fn aot_supports_import_and_call_across_modules() {
  let _guard = AOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  let tmp = TempDir::new().unwrap();

  let math = tmp.path().join("math.ts");
  fs::write(&math, "export function add(a:number,b:number): number { return a+b; }\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import {add} from \"./math\";\nexport function main(): number { print(add(20, 22)); return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(Duration::from_secs(120))
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("42\n"));
}

#[test]
fn aot_supports_import_aliases() {
  let _guard = AOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  let tmp = TempDir::new().unwrap();

  let math = tmp.path().join("math.ts");
  fs::write(&math, "export function add(a:number,b:number): number { return a+b; }\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import {add as plus} from \"./math\";\nexport function main(): number { print(plus(20, 22)); return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(Duration::from_secs(120))
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("42\n"));
}

#[test]
fn aot_rejects_cyclic_module_dependencies_deterministically() {
  let _guard = AOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  let tmp = TempDir::new().unwrap();

  let a = tmp.path().join("a.ts");
  let b = tmp.path().join("b.ts");

  fs::write(
    &a,
    "import \"./b\";\nexport function main(): number { return 0; }\n",
  )
  .unwrap();
  fs::write(&b, "import \"./a\";\nexport const unused: number = 0;\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(Duration::from_secs(120))
    .arg("build")
    .arg(&a)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .stderr(predicate::str::contains("NJS0125"))
    .stderr(predicate::str::contains("cyclic module dependency"));
}
