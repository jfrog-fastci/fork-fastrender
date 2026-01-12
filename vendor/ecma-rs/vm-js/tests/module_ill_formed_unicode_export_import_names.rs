use vm_js::{SourceTextModuleRecord, VmError};

fn assert_module_syntax_error(source: &str) {
  match SourceTextModuleRecord::parse(source) {
    Err(VmError::Syntax(_)) => {}
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

// test262:
// - language/module-code/early-export-ill-formed-string.js
// - language/module-code/export-expname-*-unpaired-surrogate.js
#[test]
fn rejects_ill_formed_unicode_module_export_import_names() {
  // Lone high surrogate in string-literal module export/import names must be a parse-time SyntaxError
  // (IsStringWellFormedUnicode).
  assert_module_syntax_error(r#"export {Moon as "\uD83C",} from "./m.js";"#);
  assert_module_syntax_error(r#"export {"\uD83C"} from "./m.js";"#);
  assert_module_syntax_error(r#"import {'\uD83C' as Usagi} from "./m.js";"#);

  // export * as "<alias>" from "..."
  assert_module_syntax_error(r#"export * as "\uD83C" from "./m.js";"#);
}

#[test]
fn accepts_well_formed_unicode_module_export_import_names() {
  // 🌙 is "\uD83C\uDF19" (valid surrogate pair)
  SourceTextModuleRecord::parse(r#"export {"\uD83C\uDF19"} from "./m.js";"#).unwrap();
}

