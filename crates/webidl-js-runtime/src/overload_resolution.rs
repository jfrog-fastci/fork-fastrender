//! Runtime error helpers for WebIDL overload resolution.
//!
//! When an overload set cannot be resolved for a given set of ECMAScript arguments, WebIDL requires
//! throwing a `TypeError`. For stable testing and easier debugging, the bindings layer should emit a
//! deterministic message that includes:
//! - the operation name,
//! - the provided argument count,
//! - and the candidate signatures.

use crate::WebIdlJsRuntime;

/// Create and return the engine error for an overload resolution failure.
///
/// Callers typically use this as:
///
/// ```ignore
/// return Err(throw_no_matching_overload(rt, "op", args.len(), &candidates));
/// ```
pub fn throw_no_matching_overload<R: WebIdlJsRuntime>(
  rt: &mut R,
  operation_name: &str,
  provided_argc: usize,
  candidate_signatures: &[&str],
) -> R::Error {
  let mut candidates = candidate_signatures
    .iter()
    .copied()
    .map(str::to_string)
    .collect::<Vec<_>>();
  // Deterministic output for golden tests regardless of how codegen collects candidates.
  candidates.sort();
  candidates.dedup();

  let mut message = format!(
    "No matching overload for {operation_name} with {provided_argc} arguments."
  );
  if !candidates.is_empty() {
    message.push_str("\nCandidates:");
    for cand in candidates {
      message.push_str("\n  - ");
      message.push_str(&cand);
    }
  }

  rt.throw_type_error(&message)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::JsRuntime;
  use crate::VmJsRuntime;
  use vm_js::{Value, VmError};

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn overload_mismatch_error_message_includes_candidates() {
    let mut rt = VmJsRuntime::new();

    let err = throw_no_matching_overload(
      &mut rt,
      "doThing",
      2,
      &["doThing(DOMString)", "doThing()", "doThing(long, long)"],
    );

    let VmError::Throw(thrown) = err else {
      panic!("expected VmError::Throw, got {err:?}");
    };

    let s = rt.to_string(thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);

    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
    assert!(msg.contains("doThing"));
    assert!(msg.contains("2"));
    assert!(msg.contains("Candidates:"));

    let idx_empty = msg.find("doThing()").expect("missing doThing() signature");
    let idx_dom = msg
      .find("doThing(DOMString)")
      .expect("missing doThing(DOMString) signature");
    let idx_ll = msg
      .find("doThing(long, long)")
      .expect("missing doThing(long, long) signature");

    assert!(
      idx_empty < idx_dom && idx_dom < idx_ll,
      "expected lexicographically sorted candidates, got {msg:?}"
    );
  }
}
