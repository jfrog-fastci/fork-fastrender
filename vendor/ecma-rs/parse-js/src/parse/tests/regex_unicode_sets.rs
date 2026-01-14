use crate::error::SyntaxErrorType;
use crate::parse_with_options;
use crate::Dialect;
use crate::ParseOptions;
use crate::SourceType;

fn ecma_script_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  }
}

fn is_test262_parse_negative(src: &str) -> bool {
  let Some(frontmatter_start) = src.find("/*---") else {
    return false;
  };
  let Some(frontmatter_end_rel) = src[frontmatter_start..].find("---*/") else {
    return false;
  };
  let frontmatter_end = frontmatter_start + frontmatter_end_rel;
  let frontmatter = &src[frontmatter_start + "/*---".len()..frontmatter_end];

  let mut in_negative = false;
  let mut negative_indent: usize = 0;
  for line in frontmatter.lines() {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if !in_negative {
      if trimmed == "negative:" {
        in_negative = true;
        negative_indent = indent;
      }
      continue;
    }

    // Leave the `negative:` block when indentation decreases.
    if indent <= negative_indent && !trimmed.is_empty() {
      in_negative = false;
      continue;
    }

    if let Some(rest) = trimmed.strip_prefix("phase:") {
      if rest.trim() == "parse" {
        return true;
      }
    }
  }

  false
}

#[test]
fn rejects_regex_literal_with_both_u_and_v_flags() {
  let opts = ecma_script_opts();
  let err = parse_with_options("/./uv;", opts).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regex flags")
  );
  let err = parse_with_options("/./vu;", opts).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("valid regex flags")
  );
}

#[test]
fn accepts_unicode_property_escape_shape() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\p{ASCII_Hex_Digit}/u;", opts).is_ok());
  assert!(parse_with_options("let r = /\\p{Script=Han}/u;", opts).is_ok());
}

#[test]
fn rejects_unicode_property_of_strings_without_v() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\p{RGI_Emoji}/u;", opts).is_err());
}

#[test]
fn rejects_unicode_property_of_strings_in_p_negated_in_v() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\P{RGI_Emoji}/v;", opts).is_err());
  assert!(parse_with_options("let r = /[^\\p{RGI_Emoji}]/v;", opts).is_err());
}

#[test]
fn rejects_unicode_sets_breaking_change_patterns() {
  let opts = ecma_script_opts();
  for pat in [
    "[(]", "[)]", "[[]", "[{]", "[}]", "[/]", "[-]", "[|]", "[&&]", "[!!]", "[##]", "[$$]", "[%%]",
    "[**]", "[++]", "[,,]", "[..]", "[::]", "[;;]", "[<<]", "[==]", "[>>]", "[??]", "[@@]", "[``]",
    "[~~]", "[^^^]", "[_^^]",
  ] {
    let src = format!("let r = /{pat}/v;");
    assert!(
      parse_with_options(&src, opts).is_err(),
      "expected {src} to fail"
    );
  }
}

#[test]
fn accepts_breaking_change_patterns_in_u_mode() {
  // These patterns are explicitly called out by test262 as previously being valid with `/u`, and
  // only becoming early errors with `/v`.
  let opts = ecma_script_opts();
  for pat in [
    "[(]", "[)]", "[[]", "[{]", "[}]", "[/]", "[-]", "[|]", "[&&]", "[!!]", "[##]", "[$$]", "[%%]",
    "[**]", "[++]", "[,,]", "[..]", "[::]", "[;;]", "[<<]", "[==]", "[>>]", "[??]", "[@@]", "[``]",
    "[~~]", "[^^^]", "[_^^]",
  ] {
    let src = format!("let r = /{pat}/u;");
    assert!(
      parse_with_options(&src, opts).is_ok(),
      "expected {src} to parse in /u mode",
    );
  }
}

#[test]
fn test262_breaking_change_files_are_parse_errors() {
  // Validate against the vendored test262 fixtures directly so we catch any additions without
  // having to manually update the pattern list above.
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp/prototype/unicodeSets");
  if !root.is_dir() {
    return;
  }

  let opts = ecma_script_opts();
  for entry in std::fs::read_dir(&root).expect("read unicodeSets fixture dir") {
    let entry = entry.expect("read_dir entry");
    let path = entry.path();
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
      continue;
    };
    if !name.starts_with("breaking-change-from-u-to-v-") || !name.ends_with(".js") {
      continue;
    }
    let src = std::fs::read_to_string(&path).expect("read test262 file");
    let err = parse_with_options(&src, opts).expect_err("expected parse error");
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression"),
      "expected {name} to fail with an invalid-pattern SyntaxError",
    );
  }
}

#[test]
fn parses_test262_regexp_prototype_unicode_sets_tests() {
  // Ensure the rest of the `RegExp.prototype.unicodeSets` test corpus parses (so it can reach
  // runtime), while keeping the known negative fixtures as early errors.
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp/prototype/unicodeSets");
  if !root.is_dir() {
    return;
  }

  let opts = ecma_script_opts();
  for entry in std::fs::read_dir(&root).expect("read unicodeSets fixture dir") {
    let entry = entry.expect("read_dir entry");
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("js") {
      continue;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
      continue;
    };
    let src = std::fs::read_to_string(&path).expect("read test262 file");

    let is_breaking_change = name.starts_with("breaking-change-from-u-to-v-");
    let is_uv_flags = name == "uv-flags.js";
    if is_breaking_change || is_uv_flags {
      let err = parse_with_options(&src, opts).expect_err("expected parse error");
      let expected = if is_uv_flags {
        SyntaxErrorType::ExpectedSyntax("valid regex flags")
      } else {
        SyntaxErrorType::ExpectedSyntax("valid regular expression")
      };
      assert_eq!(err.typ, expected, "unexpected error type for {name}");
    } else {
      assert!(
        parse_with_options(&src, opts).is_ok(),
        "expected {name} to parse",
      );
    }
  }
}

#[test]
fn parses_test262_property_escapes_generated_strings_corpus() {
  // The test262 Unicode property escape generator also emits a corpus for properties of strings,
  // which are only valid when using the `v` flag. Ensure the positive fixtures parse and the
  // negative fixtures are classified as invalid patterns.
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings");
  if !root.is_dir() {
    return;
  }

  let opts = ecma_script_opts();
  for entry in std::fs::read_dir(&root).expect("read property-escapes corpus dir") {
    let entry = entry.expect("read_dir entry");
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("js") {
      continue;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
      continue;
    };
    let src = std::fs::read_to_string(&path).expect("read test262 file");
    let expects_parse_error = is_test262_parse_negative(&src);
    if expects_parse_error {
      let err = parse_with_options(&src, opts).expect_err("expected parse error");
      assert_eq!(
        err.typ,
        SyntaxErrorType::ExpectedSyntax("valid regular expression"),
        "unexpected error type for {name}",
      );
    } else {
      assert!(
        parse_with_options(&src, opts).is_ok(),
        "expected {name} to parse",
      );
    }
  }
}

#[test]
fn accepts_unicode_sets_examples() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /^[[0-9]_]+$/v;", opts).is_ok());
  assert!(parse_with_options("let r = /^[\\q{0|2|4|9\\uFE0F\\u20E3}_]+$/v;", opts).is_ok());
  assert!(parse_with_options("let r = /^[\\p{ASCII_Hex_Digit}_]+$/v;", opts).is_ok());
  assert!(parse_with_options(r"let r = /[\!!]/v;", opts).is_ok());
  assert!(parse_with_options(r"let r = /[\&\&]/v;", opts).is_ok());
  assert!(parse_with_options("let r = /[]/v;", opts).is_ok());
}

#[test]
fn rejects_q_escape_outside_unicode_sets_class() {
  // `\q{...}` is only defined within `[...]` when using the `v` flag.
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /\\q{a}/v;", opts).is_err());
}

#[test]
fn rejects_unicode_sets_and_operator_lookahead_early_errors() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /[(a)]/v;", opts).is_err());
  // `]` is a ClassSetSyntaxCharacter in UnicodeSets mode and cannot appear unescaped inside a
  // parenthesized subexpression.
  assert!(parse_with_options("let r = /[(a])]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[a&&&b]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[ab&&c]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[a&&bc]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[ab--c]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[a--bc]/v;", opts).is_err());
}

#[test]
fn accepts_q_disjunction_in_negated_class_when_non_stringy() {
  let opts = ecma_script_opts();
  assert!(parse_with_options("let r = /[^\\q{a|b}]/v;", opts).is_ok());
  assert!(parse_with_options("let r = /[^\\q{ab}]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[^\\q{}]/v;", opts).is_err());
}

#[test]
fn parses_test262_unicode_sets_generated_files() {
  // Smoke-test the vendored test262 `unicodeSets/generated` corpus: these should be parseable so
  // tests can reach runtime (even if RegExp v-mode execution is not implemented yet).
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp/unicodeSets/generated");
  if !root.is_dir() {
    // Some distributions of `parse-js` may not vendor the test262 corpus.
    return;
  }

  let opts = ecma_script_opts();
  for entry in std::fs::read_dir(&root).expect("read unicodeSets/generated dir") {
    let entry = entry.expect("read_dir entry");
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("js") {
      continue;
    }
    let src = std::fs::read_to_string(&path).expect("read test file");
    if let Err(err) = parse_with_options(&src, opts) {
      panic!("failed to parse {}: {err}", path.display());
    }
  }
}

#[test]
fn parses_test262_unicode_property_escape_files() {
  // Validate the vendored test262 `property-escapes` corpus:
  // - tests with `negative: { phase: parse }` must fail during parsing, and
  // - all other tests should be parseable so they can reach runtime.
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp/property-escapes");
  if !root.is_dir() {
    // Some distributions of `parse-js` may not vendor the test262 corpus.
    return;
  }

  fn collect_js_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("read_dir");
    for entry in entries {
      let entry = entry.expect("read_dir entry");
      let path = entry.path();
      if path.is_dir() {
        collect_js_files(&path, out);
        continue;
      }
      if path.extension().and_then(|s| s.to_str()) != Some("js") {
        continue;
      }
      out.push(path);
    }
  }

  let mut files = Vec::new();
  collect_js_files(&root, &mut files);
  files.sort();

  let opts = ecma_script_opts();
  for path in files {
    let src = std::fs::read_to_string(&path).expect("read test file");
    let is_parse_negative = is_test262_parse_negative(&src);
    match (is_parse_negative, parse_with_options(&src, opts)) {
      (true, Ok(_)) => panic!("expected {} to fail parsing", path.display()),
      (true, Err(err)) => {
        // The `property-escapes` corpus exercises invalid Unicode property escape grammar and
        // should be classified as invalid regular expression patterns (as opposed to flag parsing
        // errors).
        assert_eq!(
          err.typ,
          SyntaxErrorType::ExpectedSyntax("valid regular expression"),
          "unexpected error type for {}",
          path.display(),
        );
      }
      (false, Err(err)) => panic!("failed to parse {}: {err}", path.display()),
      (false, Ok(_)) => {}
    }
  }
}

#[test]
fn accepts_unicode_sets_escaped_reserved_punctuators() {
  // UnicodeSets mode introduces a number of reserved punctuators that become early errors when
  // used unescaped inside `[...]`. Escaping them should still be accepted.
  let opts = ecma_script_opts();
  for pat in [
    r"[\(]", r"[\)]", r"[\[]", r"[\{]", r"[\}]", r"[\/]", r"[\-]", r"[\|]",
  ] {
    let src = format!("let r = /{pat}/v;");
    assert!(
      parse_with_options(&src, opts).is_ok(),
      "expected {src} to parse"
    );
  }
}

#[test]
fn may_contain_strings_rules_for_q_escape() {
  let opts = ecma_script_opts();
  // A `\q{...}` alternative that is exactly one code point does not count as "containing strings".
  assert!(parse_with_options("let r = /[^\\q{a}]/v;", opts).is_ok());
  // Empty and multi-code-point alternatives do.
  assert!(parse_with_options("let r = /[^\\q{}]/v;", opts).is_err());
  assert!(parse_with_options("let r = /[^\\q{ab}]/v;", opts).is_err());
}

#[test]
fn may_contain_strings_respects_set_operators() {
  let opts = ecma_script_opts();
  // Subtraction and intersection can remove strings from the overall set, so they are permitted in
  // negated classes even if one operand might contain strings.
  assert!(parse_with_options("let r = /[^a--\\q{ab}]/v;", opts).is_ok());
  assert!(parse_with_options("let r = /[^\\p{RGI_Emoji}&&a]/v;", opts).is_ok());
}

#[test]
fn parses_other_test262_regexp_v_flag_files() {
  // Remaining `regexp-v-flag` tests that live outside the `unicodeSets` and `property-escapes`
  // directories should also parse (they exercise basic `/v` usage like dot and property escapes).
  let opts = ecma_script_opts();
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test262-semantic/data/test");
  if !root.is_dir() {
    return;
  }
  for rel in [
    "built-ins/String/prototype/match/regexp-prototype-match-v-u-flag.js",
    "built-ins/String/prototype/matchAll/regexp-prototype-matchAll-v-u-flag.js",
    "built-ins/String/prototype/replace/regexp-prototype-replace-v-u-flag.js",
    "built-ins/String/prototype/search/regexp-prototype-search-v-flag.js",
    "built-ins/String/prototype/search/regexp-prototype-search-v-u-flag.js",
    "built-ins/RegExp/prototype/flags/this-val-regexp.js",
    "built-ins/RegExp/prototype/exec/regexp-builtin-exec-v-u-flag.js",
  ] {
    let path = root.join(rel);
    let src = std::fs::read_to_string(&path).expect("read test262 file");
    if let Err(err) = parse_with_options(&src, opts) {
      panic!("failed to parse {}: {err}", path.display());
    }
  }
}

#[test]
fn parses_test262_regexp_v_flag_files() {
  // Ensure that all test262 RegExp tests that mention `/v` are at least parseable.
  //
  // This is intentionally parse-only; vm-js does not yet implement full `/v` execution semantics,
  // but the broader harness needs these files to reach runtime.
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../test262-semantic/data/test/built-ins/RegExp");
  if !root.is_dir() {
    return;
  }

  fn collect_js_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("read_dir");
    for entry in entries {
      let entry = entry.expect("read_dir entry");
      let path = entry.path();
      if path.is_dir() {
        collect_js_files(&path, out);
        continue;
      }
      if path.extension().and_then(|s| s.to_str()) != Some("js") {
        continue;
      }
      out.push(path);
    }
  }

  let mut files = Vec::new();
  collect_js_files(&root, &mut files);
  files.sort();

  let opts = ecma_script_opts();
  for path in files {
    let src = std::fs::read_to_string(&path).expect("read test262 file");
    if !src.contains("/v") {
      continue;
    }
    let is_parse_negative = is_test262_parse_negative(&src);
    match (is_parse_negative, parse_with_options(&src, opts)) {
      (true, Ok(_)) => panic!("expected {} to fail parsing", path.display()),
      (false, Err(err)) => panic!("failed to parse {}: {err}", path.display()),
      _ => {}
    }
  }
}
