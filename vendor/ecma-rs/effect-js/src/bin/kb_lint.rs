use std::process::exit;

use effect_js::{validate, ApiDatabase};

fn main() {
  let db = ApiDatabase::from_embedded().unwrap_or_else(|err| {
    eprintln!("failed to load bundled knowledge base: {err}");
    exit(1);
  });

  match validate::validate(&db) {
    Ok(()) => {}
    Err(errors) => {
      eprintln!("knowledge base validation failed ({} error(s)):", errors.len());
      for err in errors {
        eprintln!("  - {err}");
      }
      exit(1);
    }
  }
}

