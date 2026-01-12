#![cfg(feature = "semantic-ops")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::TopLevelMode;

#[test]
fn known_api_call_lowers_to_structured_inst() {
  let src = r#"__known_api_call("JSON.parse","[]");"#;
  let program = compile_source(src, TopLevelMode::Module, false);

  let mut seen_known_api_call = false;
  for (_, insts) in program.top_level.body.bblocks.all() {
    for inst in insts {
      if matches!(&inst.t, InstTyp::KnownApiCall { .. }) {
        seen_known_api_call = true;
      }

      for arg in &inst.args {
        if let Arg::Builtin(path) = arg {
          assert!(
            !path.starts_with("known_api:"),
            "stringly known api builtin should not be present in IL: {path}"
          );
        }
      }
    }
  }

  assert!(
    seen_known_api_call,
    "expected at least one KnownApiCall instruction in the lowered IL"
  );
}
