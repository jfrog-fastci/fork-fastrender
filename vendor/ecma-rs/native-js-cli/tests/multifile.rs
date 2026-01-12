use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::tempdir;

// These tests spawn `native-js-cli`, which performs LLVM object emission and system linking.
// Under heavy CI/agent contention this can take tens of seconds per invocation, so keep the
// timeout generous to avoid flaky `<interrupted>` failures.
const CLI_TIMEOUT: Duration = Duration::from_secs(180);

const MAX_CONCURRENT_NATIVE_JS_CLI_TESTS: usize = 4;
static NATIVE_JS_CLI_TESTS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct CodegenPermit;

impl CodegenPermit {
  fn acquire() -> Self {
    loop {
      let current = NATIVE_JS_CLI_TESTS_IN_FLIGHT.load(Ordering::Acquire);
      if current < MAX_CONCURRENT_NATIVE_JS_CLI_TESTS {
        if NATIVE_JS_CLI_TESTS_IN_FLIGHT
          .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          return Self;
        }
      }
      std::thread::sleep(Duration::from_millis(10));
    }
  }
}

impl Drop for CodegenPermit {
  fn drop(&mut self) {
    NATIVE_JS_CLI_TESTS_IN_FLIGHT.fetch_sub(1, Ordering::Release);
  }
}

struct PermitCommand {
  _permit: CodegenPermit,
  inner: Command,
}

impl Deref for PermitCommand {
  type Target = Command;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl DerefMut for PermitCommand {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.inner
  }
}

fn native_js_cli() -> PermitCommand {
  PermitCommand {
    _permit: CodegenPermit::acquire(),
    inner: assert_cmd::cargo::cargo_bin_cmd!("native-js-cli"),
  }
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
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("5\n"));
}

#[test]
fn supports_default_imports() {
  let dir = tempdir().unwrap();
  let math = dir.path().join("math.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &math,
    "export default function add(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import add from './math';\nexport function main(){console.log(add(1,2));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("3\n"));
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
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn runs_module_initializers_in_import_order() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let b = dir.path().join("b.ts");
  let c = dir.path().join("c.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "console.log(\"dep\");\n").unwrap();
  fs::write(&b, "import './dep';\nconsole.log(\"b\");\n").unwrap();
  fs::write(&c, "import './dep';\nconsole.log(\"c\");\n").unwrap();
  fs::write(
    &main,
    "import './b';\nimport './c';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nb\nc\nmain\n"));
}

#[test]
fn runs_module_initializers_in_import_order_with_transitive_deps() {
  let dir = tempdir().unwrap();
  let a = dir.path().join("a.ts");
  let b = dir.path().join("b.ts");
  let c = dir.path().join("c.ts");
  let main = dir.path().join("main.ts");

  fs::write(&c, "console.log(\"c\");\n").unwrap();
  fs::write(&b, "import './c';\nconsole.log(\"b\");\n").unwrap();
  fs::write(&a, "console.log(\"a\");\n").unwrap();
  fs::write(
    &main,
    "import './b';\nimport './a';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("c\nb\na\nmain\n"));
}

#[test]
fn runs_reexports_and_imports_in_declaration_order() {
  let dir = tempdir().unwrap();
  let b = dir.path().join("b.ts");
  let c = dir.path().join("c.ts");
  let main = dir.path().join("main.ts");

  fs::write(&b, "console.log(\"b\");\n").unwrap();
  fs::write(&c, "console.log(\"c\");\n").unwrap();
  fs::write(
    &main,
    "export {} from './b';\nimport './c';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("b\nc\nmain\n"));
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
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("true hi\n"));
}

#[test]
fn errors_on_unsupported_namespace_import_syntax() {
  let dir = tempdir().unwrap();
  let math = dir.path().join("math.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &math,
    "export function add(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  // Namespace imports are out of scope for the minimal module subset.
  fs::write(
    &main,
    "import * as math from './math';\nexport function main(){console.log(1);}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
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

  fs::write(
    &a,
    "import {b} from './b';\nexport function a(){return b()}\n",
  )
  .unwrap();
  fs::write(
    &b,
    "import {a} from './a';\nexport function b(){return a()}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&a)
    .assert()
    .failure()
    .stderr(predicate::str::contains("cyclic module dependency"));
}

#[test]
fn errors_on_cycles_through_reexports_deterministically() {
  let dir = tempdir().unwrap();
  let a = dir.path().join("a.ts");
  let b = dir.path().join("b.ts");
  let main = dir.path().join("main.ts");

  fs::write(&a, "export {} from './b';\n").unwrap();
  fs::write(&b, "export {} from './a';\n").unwrap();
  fs::write(
    &main,
    "import './a';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .failure()
    .stderr(predicate::str::contains("cyclic module dependency"));
}

#[test]
fn errors_on_cycles_through_export_all_deterministically() {
  let dir = tempdir().unwrap();
  let a = dir.path().join("a.ts");
  let b = dir.path().join("b.ts");
  let main = dir.path().join("main.ts");

  fs::write(&a, "export * from './b';\n").unwrap();
  fs::write(&b, "export * from './a';\n").unwrap();
  fs::write(
    &main,
    "import './a';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .failure()
    .stderr(predicate::str::contains("cyclic module dependency"));
}

#[test]
fn reexports_create_runtime_module_dependencies() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "console.log(\"dep\");\n").unwrap();
  fs::write(&reexport, "export * from './dep';\n").unwrap();
  fs::write(
    &main,
    "import './reexport';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn export_all_namespace_reexport_creates_runtime_module_dependencies() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(&dep, "console.log(\"dep\");\n").unwrap();
  fs::write(
    &entry,
    "export * as ns from './dep';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn supports_importing_from_reexport_modules() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(&reexport, "export { value } from './dep';\n").unwrap();
  fs::write(
    &main,
    "import {value} from './reexport';\nexport function main(){console.log(value(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\n42\n"));
}

#[test]
fn supports_importing_from_local_reexports() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(
    &reexport,
    "import { value } from './dep';\nconsole.log(\"reexport\");\nexport { value };\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import {value} from './reexport';\nexport function main(){console.log(value(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nreexport\n42\n"));
}

#[test]
fn supports_importing_from_renamed_local_reexports() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(
    &reexport,
    "import { value } from './dep';\nconsole.log(\"reexport\");\nexport { value as other };\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import {other} from './reexport';\nexport function main(){console.log(other(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nreexport\n42\n"));
}

#[test]
fn supports_importing_from_local_reexports_with_import_alias() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(
    &reexport,
    "import { value as other } from './dep';\nconsole.log(\"reexport\");\nexport { other };\n",
  )
  .unwrap();
  fs::write(
    &main,
    "import {other} from './reexport';\nexport function main(){console.log(other(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nreexport\n42\n"));
}

#[test]
fn supports_importing_from_export_all_reexports() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(&reexport, "export * from './dep';\n").unwrap();
  fs::write(
    &main,
    "import {value} from './reexport';\nexport function main(){console.log(value(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\n42\n"));
}

#[test]
fn supports_importing_through_multi_hop_reexport_chain() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let b = dir.path().join("b.ts");
  let c = dir.path().join("c.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(&b, "export * from './dep';\n").unwrap();
  fs::write(&c, "export { value as other } from './b';\n").unwrap();
  fs::write(
    &main,
    "import {other} from './c';\nexport function main(){console.log(other(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\n42\n"));
}

#[test]
fn supports_importing_from_renamed_reexports() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let reexport = dir.path().join("reexport.ts");
  let main = dir.path().join("main.ts");

  fs::write(
    &dep,
    "console.log(\"dep\");\nexport function value(a:number,b:number){return a+b}\n",
  )
  .unwrap();
  fs::write(&reexport, "export { value as other } from './dep';\n").unwrap();
  fs::write(
    &main,
    "import {other} from './reexport';\nexport function main(){console.log(other(20,22));}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("dep\n42\n"));
}

#[test]
fn auto_calls_reexported_main() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(&entry, "export { main } from './impl';\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn auto_calls_local_reexported_main() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(
    &entry,
    "import { main } from './impl';\nconsole.log(\"entry\");\nexport { main };\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nentry\nmain\n"));
}

#[test]
fn auto_calls_renamed_local_reexported_main() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function run(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(
    &entry,
    "import { run } from './impl';\nconsole.log(\"entry\");\nexport { run as main };\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nentry\nmain\n"));
}

#[test]
fn auto_calls_renamed_reexported_main() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function run(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(&entry, "export { run as main } from './impl';\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn auto_calls_export_all_reexported_main() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(&entry, "export * from './impl';\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn auto_calls_reexported_main_through_reexport_chain() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let mid = dir.path().join("mid.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(
    &mid,
    "console.log(\"mid\");\nexport { main } from './impl';\n",
  )
  .unwrap();
  fs::write(&entry, "console.log(\"entry\");\nexport * from './mid';\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmid\nentry\nmain\n"));
}

#[test]
fn entry_fn_can_target_reexported_export() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function run(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(&entry, "export { run as start } from './impl';\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("start")
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn entry_fn_can_target_export_all_reexported_export() {
  let dir = tempdir().unwrap();
  let impl_file = dir.path().join("impl.ts");
  let entry = dir.path().join("entry.ts");

  fs::write(
    &impl_file,
    "console.log(\"dep\");\nexport function run(){console.log(\"main\");}\n",
  )
  .unwrap();
  fs::write(&entry, "export * from './impl';\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("run")
    .arg(entry)
    .assert()
    .success()
    .stdout(predicate::eq("dep\nmain\n"));
}

#[test]
fn type_only_import_does_not_execute_module() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "export type T = number;\nconsole.log(\"dep\");\n").unwrap();
  fs::write(
    &main,
    "import { type T } from './dep';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("main\n"));
}

#[test]
fn type_only_reexport_does_not_execute_module() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "export type T = number;\nconsole.log(\"dep\");\n").unwrap();
  fs::write(
    &main,
    "export { type T } from './dep';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("main\n"));
}

#[test]
fn type_only_export_all_reexport_does_not_execute_module() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "export type T = number;\nconsole.log(\"dep\");\n").unwrap();
  fs::write(
    &main,
    "export type * from './dep';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("main\n"));
}

#[test]
fn type_only_export_all_namespace_reexport_does_not_execute_module() {
  let dir = tempdir().unwrap();
  let dep = dir.path().join("dep.ts");
  let main = dir.path().join("main.ts");

  fs::write(&dep, "export type T = number;\nconsole.log(\"dep\");\n").unwrap();
  fs::write(
    &main,
    "export type * as ns from './dep';\nexport function main(){console.log(\"main\");}\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--entry-fn")
    .arg("main")
    .arg(main)
    .assert()
    .success()
    .stdout(predicate::eq("main\n"));
}
