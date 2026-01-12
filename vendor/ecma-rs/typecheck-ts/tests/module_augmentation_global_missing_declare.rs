use typecheck_ts::codes;
use typecheck_ts::{Diagnostic, FileKey, MemoryHost, Program};

fn check(source: &str) -> Vec<Diagnostic> {
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key]);
  program.check()
}

fn assert_ts2669_and_ts2670(diagnostics: &[Diagnostic]) {
  let codes: Vec<_> = diagnostics.iter().map(|diag| diag.code.as_str()).collect();
  assert_eq!(
    codes,
    vec![
      codes::GLOBAL_AUGMENTATION_INVALID_CONTEXT.as_str(),
      codes::GLOBAL_AUGMENTATION_MISSING_DECLARE.as_str(),
    ],
    "unexpected diagnostics: {diagnostics:?}"
  );
}

#[test]
fn module_augmentation_global6_1() {
  let diagnostics = check("global { interface Array<T> { x } }");
  assert_ts2669_and_ts2670(&diagnostics);
}

#[test]
fn module_augmentation_global7_1() {
  let diagnostics = check("namespace A { global { interface Array<T> { x } } }");
  assert_ts2669_and_ts2670(&diagnostics);
}

#[test]
fn module_augmentation_global8_1() {
  let diagnostics = check("namespace A { global { interface Array<T> { x } } } export {}");
  assert_ts2669_and_ts2670(&diagnostics);
}

