use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Frontmatter {
  #[serde(default, deserialize_with = "string_or_seq")]
  pub includes: Vec<String>,
  #[serde(default, deserialize_with = "string_or_seq")]
  pub flags: Vec<String>,
  #[serde(default, deserialize_with = "string_or_seq")]
  pub features: Vec<String>,
  #[serde(default)]
  pub negative: Option<Negative>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Negative {
  pub phase: String,
  #[serde(rename = "type")]
  pub typ: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedTestSource {
  pub frontmatter: Option<Frontmatter>,
  pub body: String,
}

pub fn parse_test_source(source: &str) -> Result<ParsedTestSource> {
  let source_no_bom = source.strip_prefix('\u{feff}').unwrap_or(source);
  // Most test262 files place the `/*--- ... ---*/` YAML frontmatter block at the top of the file,
  // after optional whitespace/comments (and optionally a literal `#!` hashbang line).
  //
  // However, some `flags: [raw]` tests deliberately start with source text that is *not* valid JS
  // (e.g. hashbang escape tests beginning with `#\\u0021`). Those tests still include YAML
  // frontmatter later in the file, and we need to discover it to classify the test correctly.
  //
  // We handle this by first trying the strict "prefix-only" scan, then falling back to an
  // "anywhere" scan that only accepts frontmatter blocks marked `raw`.
  let Some(start) =
    find_frontmatter_start(source_no_bom).or_else(|| find_raw_frontmatter_start(source_no_bom))
  else {
    return Ok(ParsedTestSource {
      frontmatter: None,
      body: source_no_bom.to_string(),
    });
  };

  let yaml_start = start + "/*---".len();
  let Some(end_rel) = source_no_bom[yaml_start..].find("---*/") else {
    bail!("frontmatter begins with `/*---` but is missing terminating `---*/`");
  };
  let yaml_end = yaml_start + end_rel;
  let yaml = &source_no_bom[yaml_start..yaml_end];
  let after = yaml_end + "---*/".len();

  let frontmatter: Frontmatter =
    serde_yaml::from_str(yaml).context("deserialize test262 YAML frontmatter")?;
  let is_raw = frontmatter.flags.iter().any(|flag| flag == "raw");

  Ok(ParsedTestSource {
    frontmatter: Some(frontmatter),
    body: if is_raw {
      // `raw` tests must not have their source modified, which includes
      // preserving the `/*--- ... ---*/` frontmatter comment.
      source.to_string()
    } else {
      let after = source_no_bom
        .get(after..)
        .ok_or_else(|| anyhow!("frontmatter terminator offset out of bounds"))?;

      // Preserve any leading whitespace/comments before the frontmatter block, but remove the
      // frontmatter block itself.
      let mut body = String::with_capacity(start + after.len());
      body.push_str(&source_no_bom[..start]);
      body.push_str(after);
      body
    },
  })
}

/// Return the byte offset of the `/*---` test262 frontmatter block, if the prefix of the file is
/// only whitespace and comments.
///
/// The prefix may contain:
/// - Unicode whitespace
/// - line comments (`// ... \n`)
/// - block comments (`/* ... */`) that are *not* the frontmatter block
/// - (optional) a hashbang/shebang line at byte 0 (`#!...\n`)
///
/// If an unterminated block comment is encountered before reaching the frontmatter, this returns
/// `None` (treat as "no frontmatter").
fn find_frontmatter_start(source: &str) -> Option<usize> {
  let mut i = 0usize;

  // Hashbang grammar (aka shebang). Only valid at the start of the file, after any BOM (already
  // stripped by `parse_test_source`).
  if source.as_bytes().starts_with(b"#!") {
    let Some(newline) = source.find('\n') else {
      return None;
    };
    i = newline + 1;
  }

  while i < source.len() {
    // Skip whitespace.
    while i < source.len() {
      let ch = source[i..].chars().next()?;
      if ch.is_whitespace() {
        i += ch.len_utf8();
      } else {
        break;
      }
    }

    if i >= source.len() {
      return None;
    }

    let rest = &source[i..];
    if rest.starts_with("//") {
      // Line comment, consume through newline (or EOF).
      let Some(newline_rel) = rest.find('\n') else {
        return None;
      };
      i += newline_rel + 1;
      continue;
    }

    if rest.starts_with("/*---") {
      return Some(i);
    }

    if rest.starts_with("/*") {
      // Block comment, consume through terminator.
      let Some(end_rel) = rest[2..].find("*/") else {
        // Unterminated comment: treat as "no frontmatter".
        return None;
      };
      i += 2 + end_rel + 2;
      continue;
    }

    // Non-whitespace / non-comment token before frontmatter => not frontmatter.
    return None;
  }

  None
}

/// Fallback frontmatter scan that searches the entire file and returns the offset of the first
/// `/*--- ... ---*/` block whose YAML metadata includes `flags: [raw]`.
///
/// This is primarily needed for test262 `raw` tests that must begin with non-comment source text
/// (for example, hashbang escape sequences).
fn find_raw_frontmatter_start(source: &str) -> Option<usize> {
  let mut search_start = 0usize;
  while search_start < source.len() {
    let Some(rel) = source[search_start..].find("/*---") else {
      return None;
    };
    let start = search_start + rel;
    let yaml_start = start + "/*---".len();
    let Some(end_rel) = source[yaml_start..].find("---*/") else {
      // No terminator: treat as "not frontmatter" and keep searching.
      search_start = yaml_start;
      continue;
    };

    let yaml_end = yaml_start + end_rel;
    let yaml = &source[yaml_start..yaml_end];

    if let Ok(frontmatter) = serde_yaml::from_str::<Frontmatter>(yaml) {
      if frontmatter.flags.iter().any(|flag| flag == "raw") {
        return Some(start);
      }
    }

    // Continue searching after this block.
    search_start = yaml_end + "---*/".len();
  }
  None
}

fn string_or_seq<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
  D: serde::de::Deserializer<'de>,
{
  struct Visitor;

  impl<'de> serde::de::Visitor<'de> for Visitor {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
      formatter.write_str("string or sequence of strings")
    }

    fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(vec![v.to_string()])
    }

    fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(vec![v])
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
    where
      A: serde::de::SeqAccess<'de>,
    {
      let mut out = Vec::new();
      while let Some(value) = seq.next_element::<String>()? {
        out.push(value);
      }
      Ok(out)
    }
  }

  deserializer.deserialize_any(Visitor)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_yaml_frontmatter_and_strips_body() {
    let src = r#"/*---
includes: [assert.js, helper.js]
flags: [onlyStrict]
features: [BigInt]
negative:
  phase: runtime
  type: TypeError
---*/
throw new TypeError();
"#;

    let parsed = parse_test_source(src).unwrap();
    let fm = parsed.frontmatter.unwrap();
    assert_eq!(fm.includes, vec!["assert.js", "helper.js"]);
    assert_eq!(fm.flags, vec!["onlyStrict"]);
    assert_eq!(fm.features, vec!["BigInt"]);
    let negative = fm.negative.unwrap();
    assert_eq!(negative.phase, "runtime");
    assert_eq!(negative.typ, "TypeError");
    assert!(parsed.body.contains("throw new TypeError()"));
    assert!(!parsed.body.contains("includes:"));
  }

  #[test]
  fn missing_frontmatter_is_ok() {
    let src = "let x = 1;";
    let parsed = parse_test_source(src).unwrap();
    assert!(parsed.frontmatter.is_none());
    assert_eq!(parsed.body, src);
  }

  #[test]
  fn parses_frontmatter_after_line_comment() {
    let src = r#"// comment
/*---
flags: [onlyStrict]
---*/
let x = 1;
"#;

    let parsed = parse_test_source(src).unwrap();
    assert_eq!(parsed.frontmatter.unwrap().flags, vec!["onlyStrict"]);
    assert!(parsed.body.contains("// comment"));
    assert!(parsed.body.contains("let x = 1;"));
    assert!(!parsed.body.contains("flags:"));
  }

  #[test]
  fn parses_frontmatter_after_block_comment() {
    let src = r#"/* block comment */
/*---
flags: [noStrict]
---*/
let x = 1;
"#;

    let parsed = parse_test_source(src).unwrap();
    assert_eq!(parsed.frontmatter.unwrap().flags, vec!["noStrict"]);
    assert!(parsed.body.contains("block comment"));
    assert!(parsed.body.contains("let x = 1;"));
    assert!(!parsed.body.contains("flags:"));
  }

  #[test]
  fn does_not_parse_frontmatter_after_code() {
    let src = r#"let x = 1;
/*---
flags: [onlyStrict]
---*/
"#;
    let parsed = parse_test_source(src).unwrap();
    assert!(parsed.frontmatter.is_none());
    assert_eq!(parsed.body, src);
  }

  #[test]
  fn parses_frontmatter_after_hashbang() {
    let src = r#"#!/usr/bin/env node
/*---
flags: [onlyStrict]
---*/
let x = 1;
"#;

    let parsed = parse_test_source(src).unwrap();
    assert_eq!(parsed.frontmatter.unwrap().flags, vec!["onlyStrict"]);
    assert!(parsed.body.starts_with("#!/usr/bin/env node\n"));
    assert!(!parsed.body.contains("flags:"));
  }

  #[test]
  fn raw_flag_preserves_frontmatter_comment_in_body() {
    let src = r#"/*---
flags: [raw]
---*/
let x = 1;
"#;

    let parsed = parse_test_source(src).unwrap();
    assert_eq!(parsed.frontmatter.unwrap().flags, vec!["raw"]);
    assert_eq!(parsed.body, src);
  }

  #[test]
  fn parses_raw_frontmatter_after_non_comment_prefix() {
    // Some raw test262 files intentionally begin with non-comment source text that would not be
    // valid JavaScript (e.g. hashbang escape tests). Ensure we still detect their frontmatter so
    // they can be classified/executed correctly.
    let src = r#"#\u0021

/*---
flags: [raw]
negative:
  phase: parse
  type: SyntaxError
---*/
throw "should not run";
"#;

    let parsed = parse_test_source(src).unwrap();
    let fm = parsed.frontmatter.unwrap();
    assert_eq!(fm.flags, vec!["raw"]);
    assert!(fm.negative.is_some());
    // Raw tests must preserve the original source, including the frontmatter block.
    assert_eq!(parsed.body, src);
  }
}
