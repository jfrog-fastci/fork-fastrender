use regex::Regex;
use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CallOperationSite {
  interface: String,
  operation: String,
  overload: usize,
}

impl CallOperationSite {
  fn new(interface: &str, operation: &str, overload: usize) -> Self {
    Self {
      interface: interface.to_string(),
      operation: operation.to_string(),
      overload,
    }
  }
}

impl fmt::Display for CallOperationSite {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "(\"{}\", \"{}\", {})",
      self.interface, self.operation, self.overload
    )
  }
}

fn parse_usize_literal(lit: &str) -> usize {
  lit
    .chars()
    .filter(|c| *c != '_')
    .collect::<String>()
    .parse()
    .unwrap_or_else(|e| panic!("expected usize literal, got {lit:?}: {e}"))
}

fn parse_next_string_literal(src: &str, cursor: &mut usize) -> Option<String> {
  let bytes = src.as_bytes();
  while *cursor < bytes.len() && bytes[*cursor] != b'"' {
    *cursor += 1;
  }
  if *cursor >= bytes.len() {
    return None;
  }
  let start = *cursor + 1;
  *cursor = start;
  while *cursor < bytes.len() && bytes[*cursor] != b'"' {
    *cursor += 1;
  }
  if *cursor >= bytes.len() {
    return None;
  }
  let end = *cursor;
  *cursor += 1;
  Some(src[start..end].to_string())
}

fn parse_next_usize_literal(src: &str, cursor: &mut usize) -> Option<usize> {
  let bytes = src.as_bytes();
  while *cursor < bytes.len() && !bytes[*cursor].is_ascii_digit() {
    *cursor += 1;
  }
  if *cursor >= bytes.len() {
    return None;
  }
  let start = *cursor;
  while *cursor < bytes.len() && (bytes[*cursor].is_ascii_digit() || bytes[*cursor] == b'_') {
    *cursor += 1;
  }
  Some(parse_usize_literal(&src[start..*cursor]))
}

fn extract_generated_call_operation_sites(src: &str) -> BTreeSet<CallOperationSite> {
  let mut sites = BTreeSet::new();
  let mut cursor = 0;
  let needle = "bindings_host.call_operation(";
  while let Some(found) = src[cursor..].find(needle) {
    cursor += found + needle.len();

    let interface = parse_next_string_literal(src, &mut cursor)
      .unwrap_or_else(|| panic!("failed to parse interface string for {needle}"));
    let operation = parse_next_string_literal(src, &mut cursor)
      .unwrap_or_else(|| panic!("failed to parse operation string for {needle}"));
    let overload = parse_next_usize_literal(src, &mut cursor)
      .unwrap_or_else(|| panic!("failed to parse overload literal for {needle}"));

    sites.insert(CallOperationSite::new(&interface, &operation, overload));
  }

  sites
}

#[derive(Debug, Default)]
struct HostDispatchCoverage {
  exact: BTreeSet<CallOperationSite>,
  wildcard_overload: BTreeSet<(String, String)>,
}

impl HostDispatchCoverage {
  fn covers(&self, site: &CallOperationSite) -> bool {
    if self.exact.contains(site) {
      return true;
    }
    self
      .wildcard_overload
      .contains(&(site.interface.clone(), site.operation.clone()))
  }
}

fn extract_host_dispatch_coverage(src: &str) -> HostDispatchCoverage {
  // Extract match arms from the `call_operation` implementation:
  //
  //   match (interface, operation, overload) {
  //     ("Document", "getElementById", 0) => { ... }
  //     ("URLSearchParams", "entries", 0)
  //       | ("URLSearchParams", "keys", 0)
  //       | ("URLSearchParams", "values", 0) => { ... }
  //     ("Window", "alert", _) => { ... }
  //     ("Element", op @ ("append" | "prepend"), 0) => { ... }
  //   }
  //
  // We treat `("X", "y", _)` as covering all overloads.
  //
  // Note: We intentionally scope parsing to the `call_operation` method body to avoid false
  // positives from other match statements in this file.
  let call_operation_src = {
    let start = src
      .find("fn call_operation")
      .expect("expected WebIdlBindingsHost::call_operation implementation in vmjs_host_dispatch.rs");
    let end = src[start..]
      .find("fn call_constructor")
      .map(|rel| start + rel)
      .expect("expected call_constructor after call_operation in vmjs_host_dispatch.rs");
    &src[start..end]
  };

  let literal_arm_re = Regex::new(
    r#"(?m)^\s*(?:\|\s*)?\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*([0-9_]+|_)\s*\)"#,
  )
  .expect("valid regex");

  // Some operations are dispatched via a single match arm that binds `operation` and matches
  // multiple string literals, e.g.:
  //   ("Element", op @ ("append" | "prepend"), 0) => { ... }
  //
  // Treat these as coverage for each string literal in the group.
  let op_group_arm_re = Regex::new(
    r#"(?m)^\s*(?:\|\s*)?\(\s*"([^"]+)"\s*,\s*\w+\s*@\s*\(([^)]*)\)\s*,\s*([0-9_]+|_)\s*\)"#,
  )
  .expect("valid regex");

  let string_lit_re = Regex::new(r#""([^"]+)""#).expect("valid regex");

  let mut coverage = HostDispatchCoverage::default();
  for caps in literal_arm_re.captures_iter(call_operation_src) {
    let interface = caps.get(1).unwrap().as_str();
    let operation = caps.get(2).unwrap().as_str();
    let overload = caps.get(3).unwrap().as_str();
    if overload == "_" {
      coverage
        .wildcard_overload
        .insert((interface.to_string(), operation.to_string()));
    } else {
      coverage.exact.insert(CallOperationSite::new(
        interface,
        operation,
        parse_usize_literal(overload),
      ));
    }
  }

  for caps in op_group_arm_re.captures_iter(call_operation_src) {
    let interface = caps.get(1).unwrap().as_str();
    let group = caps.get(2).unwrap().as_str();
    let overload = caps.get(3).unwrap().as_str();

    let operations: Vec<&str> = string_lit_re
      .captures_iter(group)
      .map(|cap| cap.get(1).unwrap().as_str())
      .collect();

    assert!(
      !operations.is_empty(),
      "failed to extract operation string literals from grouped match arm: {group:?}"
    );

    if overload == "_" {
      for op in operations {
        coverage
          .wildcard_overload
          .insert((interface.to_string(), op.to_string()));
      }
    } else {
      let overload = parse_usize_literal(overload);
      for op in operations {
        coverage
          .exact
          .insert(CallOperationSite::new(interface, op, overload));
      }
    }
  }

  coverage
}

fn compute_missing_sites(
  generated: &BTreeSet<CallOperationSite>,
  coverage: &HostDispatchCoverage,
) -> BTreeSet<CallOperationSite> {
  generated
    .iter()
    .filter(|site| !coverage.covers(site))
    .cloned()
    .collect()
}

fn allowlisted_missing_sites() -> BTreeSet<CallOperationSite> {
  // Intentionally-unimplemented `WebIdlBindingsHost::call_operation` routes.
  //
  // When a missing operation is implemented in `VmJsWebIdlBindingsHostDispatch`, remove it from
  // this allowlist.
  [
    // TODO(browser_ui): JS API for controlling the native browser chrome is generated but not yet
    // supported by the default vm-js host dispatch.
    CallOperationSite::new("FastRenderChrome", "navigation", 0),
    CallOperationSite::new("FastRenderChrome", "tabs", 0),
    CallOperationSite::new("FastRenderNavigation", "goBack", 0),
    CallOperationSite::new("FastRenderNavigation", "goForward", 0),
    CallOperationSite::new("FastRenderNavigation", "navigate", 0),
    CallOperationSite::new("FastRenderNavigation", "reload", 0),
    CallOperationSite::new("FastRenderNavigation", "stop", 0),
    CallOperationSite::new("FastRenderTabs", "activateTab", 0),
    CallOperationSite::new("FastRenderTabs", "closeTab", 0),
    CallOperationSite::new("FastRenderTabs", "getAll", 0),
    CallOperationSite::new("FastRenderTabs", "newTab", 0),
  ]
  .into_iter()
  .collect()
}

#[test]
fn vmjs_host_dispatch_covers_generated_call_operation_sites() {
  let generated_src = include_str!("bindings/generated/mod.rs");
  let dispatch_src = include_str!("vmjs_host_dispatch.rs");

  let generated = extract_generated_call_operation_sites(generated_src);
  assert!(
    !generated.is_empty(),
    "expected to find generated call_operation sites in bindings snapshot"
  );

  let coverage = extract_host_dispatch_coverage(dispatch_src);
  assert!(
    !coverage.exact.is_empty() || !coverage.wildcard_overload.is_empty(),
    "expected to find call_operation match arms in VmJsWebIdlBindingsHostDispatch"
  );

  let missing = compute_missing_sites(&generated, &coverage);
  let allowlist = allowlisted_missing_sites();

  let unexpected_missing: Vec<_> = missing.difference(&allowlist).cloned().collect();
  let stale_allowlist: Vec<_> = allowlist.difference(&missing).cloned().collect();

  if unexpected_missing.is_empty() && stale_allowlist.is_empty() {
    return;
  }

  let mut msg = String::new();
  msg.push_str("VmJsWebIdlBindingsHostDispatch coverage mismatch for generated WebIDL bindings.\n\n");

  if !unexpected_missing.is_empty() {
    msg.push_str("Missing dispatch arms (add match arms or allowlist intentionally-unimplemented ops):\n");
    for site in &unexpected_missing {
      msg.push_str(&format!("  - {site}\n"));
    }
    msg.push('\n');
  }

  if !stale_allowlist.is_empty() {
    msg.push_str("Stale allowlist entries (remove these; dispatch now covers them):\n");
    for site in &stale_allowlist {
      msg.push_str(&format!("  - {site}\n"));
    }
    msg.push('\n');
  }

  msg.push_str("Hint: generated sites are parsed from `src/js/webidl/bindings/generated/mod.rs`.\n");
  msg.push_str("      dispatch arms are parsed from `src/js/webidl/vmjs_host_dispatch.rs`.\n");
  panic!("{msg}");
}

#[test]
fn vmjs_host_dispatch_coverage_detects_new_generated_entry() {
  // This is a safety-net test: if the bindings generator adds a new `call_operation` site but the
  // host dispatch isn't updated, the coverage audit should detect it.
  let mut generated_src = include_str!("bindings/generated/mod.rs").to_string();
  generated_src.push_str(
    r#"
// -- injected by test --
bindings_host.call_operation(
  &mut *rt.vm,
  &mut rt.scope,
  receiver,
  "FakeInterface",
  "fakeOperation",
  0,
  &converted_args,
)?;
"#,
  );

  let dispatch_src = include_str!("vmjs_host_dispatch.rs");
  let generated = extract_generated_call_operation_sites(&generated_src);
  let coverage = extract_host_dispatch_coverage(dispatch_src);
  let missing = compute_missing_sites(&generated, &coverage);

  assert!(
    missing.contains(&CallOperationSite::new("FakeInterface", "fakeOperation", 0)),
    "expected injected call_operation site to be reported as missing"
  );
}
