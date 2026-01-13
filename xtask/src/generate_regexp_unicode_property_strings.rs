use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct GenerateRegExpUnicodePropertyStringsArgs {
  /// Verify that the checked-in tables are up-to-date (do not modify files).
  #[arg(long)]
  pub check: bool,
}

#[derive(Debug, Clone, Copy)]
struct PropertySpec {
  js_name: &'static str,
  bit: u8,
}

const PROPERTY_SPECS: &[PropertySpec] = &[
  PropertySpec {
    js_name: "Basic_Emoji",
    bit: 1 << 0,
  },
  PropertySpec {
    js_name: "Emoji_Keycap_Sequence",
    bit: 1 << 1,
  },
  PropertySpec {
    js_name: "RGI_Emoji_Modifier_Sequence",
    bit: 1 << 2,
  },
  PropertySpec {
    js_name: "RGI_Emoji_Flag_Sequence",
    bit: 1 << 3,
  },
  PropertySpec {
    js_name: "RGI_Emoji_Tag_Sequence",
    bit: 1 << 4,
  },
  PropertySpec {
    js_name: "RGI_Emoji_ZWJ_Sequence",
    bit: 1 << 5,
  },
];

const ALL_PROPERTY_MASK: u8 = (1 << 6) - 1;

const UNICODE_VERSION: &str = "17.0.0";

pub fn run_generate_regexp_unicode_property_strings(
  args: GenerateRegExpUnicodePropertyStringsArgs,
) -> Result<()> {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .context("xtask should live one directory below the repo root")?
    .to_path_buf();

  let input_dir = repo_root.join("vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings");
  let output = repo_root.join("vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs");

  let generated =
    generate(&input_dir).with_context(|| format!("generating tables from {input_dir:?}"))?;

  if args.check {
    let existing = fs::read_to_string(&output).with_context(|| format!("reading {output:?}"))?;
    if existing != generated {
      bail!(
        "RegExp Unicode property-of-strings tables out of date: re-run `bash scripts/cargo_agent.sh xtask generate-regexp-unicode-property-strings` (diff against {output:?})"
      );
    }
    return Ok(());
  }

  fs::write(&output, generated).with_context(|| format!("writing {output:?}"))?;
  Ok(())
}

#[derive(Debug, Default)]
struct TrieNode {
  children: BTreeMap<u16, usize>,
  terminal_mask: u8,
}

fn generate(input_dir: &Path) -> Result<String> {
  let mut union: BTreeMap<Vec<u16>, u8> = BTreeMap::new();

  for spec in PROPERTY_SPECS {
    let path = input_dir.join(format!("{}.js", spec.js_name));
    let strings = parse_property_file(&path)
      .with_context(|| format!("parsing property-of-strings list from {path:?}"))?;

    let mut set: BTreeSet<Vec<u16>> = BTreeSet::new();
    for s in &strings {
      if !set.insert(s.clone()) {
        bail!("duplicate string literal in {}: {:?}", spec.js_name, Utf16Debug(s));
      }
    }

    for s in &set {
      union.entry(s.clone()).and_modify(|mask| *mask |= spec.bit).or_insert(spec.bit);
    }

  }

  // Validate that `RGI_Emoji` (the union property) matches the union of the base properties.
  let rgi_path = input_dir.join("RGI_Emoji.js");
  let rgi_strings =
    parse_property_file(&rgi_path).with_context(|| format!("parsing {rgi_path:?}"))?;
  let mut rgi_set: BTreeSet<Vec<u16>> = BTreeSet::new();
  for s in &rgi_strings {
    if !rgi_set.insert(s.clone()) {
      bail!("duplicate string literal in RGI_Emoji: {:?}", Utf16Debug(s));
    }
  }

  let union_set: BTreeSet<Vec<u16>> = union.keys().cloned().collect();
  if union_set != rgi_set {
    let mut missing_from_rgi = union_set.difference(&rgi_set);
    let mut extra_in_rgi = rgi_set.difference(&union_set);
    let missing_example = missing_from_rgi.next().map(|s| format!("{:?}", Utf16Debug(s)));
    let extra_example = extra_in_rgi.next().map(|s| format!("{:?}", Utf16Debug(s)));
    bail!(
      "RGI_Emoji union mismatch: base-union={}, rgi_emoji={}. missing_from_rgi={:?} extra_in_rgi={:?}",
      union_set.len(),
      rgi_set.len(),
      missing_example,
      extra_example
    );
  }

  // All terminal masks should be within the base property bitset.
  for (s, mask) in &union {
    if *mask == 0 || (*mask & !ALL_PROPERTY_MASK) != 0 {
      bail!("invalid property mask {mask:#x} for string {:?}", Utf16Debug(s));
    }
  }

  // Build a shared trie over UTF-16 code units.
  let mut nodes: Vec<TrieNode> = Vec::new();
  nodes.push(TrieNode::default()); // root

  for (seq, mask) in union.iter() {
    let mut node_idx = 0usize;
    for &cu in seq {
      let next = if let Some(&child) = nodes[node_idx].children.get(&cu) {
        child
      } else {
        let child = nodes.len();
        nodes.push(TrieNode::default());
        nodes[node_idx].children.insert(cu, child);
        child
      };
      node_idx = next;
    }
    nodes[node_idx].terminal_mask |= *mask;
  }

  let max_matches_per_start = compute_max_terminal_prefixes(&nodes);

  // Emit compact node/edge arrays.
  let mut node_edge_start: Vec<u32> = Vec::with_capacity(nodes.len());
  let mut node_edge_len: Vec<u16> = Vec::with_capacity(nodes.len());
  let mut node_terminal_mask: Vec<u8> = Vec::with_capacity(nodes.len());
  let mut edge_code_unit: Vec<u16> = Vec::new();
  let mut edge_target: Vec<u32> = Vec::new();

  for node in &nodes {
    node_edge_start.push(
      u32::try_from(edge_code_unit.len())
        .map_err(|_| anyhow!("edge table too large for u32"))?,
    );
    let len = node.children.len();
    let len_u16 = u16::try_from(len)
      .map_err(|_| anyhow!("node has too many outgoing edges ({len}) for u16"))?;
    node_edge_len.push(len_u16);
    node_terminal_mask.push(node.terminal_mask);

    for (&cu, &child) in node.children.iter() {
      edge_code_unit.push(cu);
      edge_target.push(
        u32::try_from(child).map_err(|_| anyhow!("node index too large for u32"))?,
      );
    }
  }

  let mut out = String::new();
  out.push_str("//! @generated\n");
  out.push_str("//!\n");
  out.push_str("//! Unicode property escapes for RegExp `v` flag (properties of strings).\n");
  out.push_str("//!\n");
  out.push_str(
    "//! This file is generated by `bash scripts/cargo_agent.sh xtask generate-regexp-unicode-property-strings`.\n",
  );
  out.push_str(&format!("//! Unicode v{UNICODE_VERSION}.\n"));
  out.push_str("//!\n");
  out.push_str("//! Sources (test262):\n");
  for spec in PROPERTY_SPECS {
    out.push_str(&format!(
      "//! - `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/{}.js`\n",
      spec.js_name
    ));
  }
  out.push_str(
    "//! - `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/RGI_Emoji.js`\n",
  );
  out.push_str("\n");
  out.push_str("#![allow(dead_code)]\n");
  out.push_str("\n");

  out.push_str("/// RegExp `v` flag Unicode properties of strings.\n");
  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum UnicodeStringProperty {\n");
  out.push_str("  BasicEmoji,\n");
  out.push_str("  EmojiKeycapSequence,\n");
  out.push_str("  RgiEmojiFlagSequence,\n");
  out.push_str("  RgiEmojiModifierSequence,\n");
  out.push_str("  RgiEmojiTagSequence,\n");
  out.push_str("  RgiEmojiZwjSequence,\n");
  out.push_str("  /// Union of all RGI emoji properties of strings.\n");
  out.push_str("  RgiEmoji,\n");
  out.push_str("}\n\n");

  out.push_str(&format!(
    "/// Maximum number of terminal matches possible at a single input position (prefix matches).\n\
pub(crate) const MAX_MATCHES_PER_POSITION: usize = {max_matches_per_start};\n\n"
  ));

  out.push_str("const PROP_BASIC_EMOJI: u8 = 1 << 0;\n");
  out.push_str("const PROP_EMOJI_KEYCAP_SEQUENCE: u8 = 1 << 1;\n");
  out.push_str("const PROP_RGI_EMOJI_MODIFIER_SEQUENCE: u8 = 1 << 2;\n");
  out.push_str("const PROP_RGI_EMOJI_FLAG_SEQUENCE: u8 = 1 << 3;\n");
  out.push_str("const PROP_RGI_EMOJI_TAG_SEQUENCE: u8 = 1 << 4;\n");
  out.push_str("const PROP_RGI_EMOJI_ZWJ_SEQUENCE: u8 = 1 << 5;\n");
  out.push_str(&format!(
    "const PROP_ALL: u8 = {ALL_PROPERTY_MASK};\n\n"
  ));

  out.push_str(
    r#"/// Traverse the Unicode property-of-strings trie from `start`, returning the UTF-16 lengths of
/// every matching string (prefix matches).
///
/// The returned lengths are ordered by increasing length.
pub(crate) fn match_property_at(
  prop: UnicodeStringProperty,
  haystack: &[u16],
  start: usize,
  out: &mut [usize; MAX_MATCHES_PER_POSITION],
) -> usize {
  if start >= haystack.len() {
    return 0;
  }

  let wanted = match prop {
    UnicodeStringProperty::BasicEmoji => PROP_BASIC_EMOJI,
    UnicodeStringProperty::EmojiKeycapSequence => PROP_EMOJI_KEYCAP_SEQUENCE,
    UnicodeStringProperty::RgiEmojiFlagSequence => PROP_RGI_EMOJI_FLAG_SEQUENCE,
    UnicodeStringProperty::RgiEmojiModifierSequence => PROP_RGI_EMOJI_MODIFIER_SEQUENCE,
    UnicodeStringProperty::RgiEmojiTagSequence => PROP_RGI_EMOJI_TAG_SEQUENCE,
    UnicodeStringProperty::RgiEmojiZwjSequence => PROP_RGI_EMOJI_ZWJ_SEQUENCE,
    UnicodeStringProperty::RgiEmoji => PROP_ALL,
  };

  let mut count = 0usize;
  let mut node = 0usize;
  for i in start..haystack.len() {
    let cu = haystack[i];
    match edge_lookup(node, cu) {
      Some(next) => {
        node = next;
      }
      None => break,
    }

    if (NODE_TERMINAL_MASK[node] & wanted) != 0 {
      if count == out.len() {
        break;
      }
      out[count] = i + 1 - start;
      count += 1;
    }
  }

  count
}

#[inline]
fn edge_lookup(node: usize, cu: u16) -> Option<usize> {
  let start = NODE_EDGE_START[node] as usize;
  let len = NODE_EDGE_LEN[node] as usize;
  let mut lo = 0usize;
  let mut hi = len;
  while lo < hi {
    let mid = (lo + hi) / 2;
    let idx = start + mid;
    let mid_cu = EDGE_CODE_UNIT[idx];
    if cu < mid_cu {
      hi = mid;
    } else if cu > mid_cu {
      lo = mid + 1;
    } else {
      return Some(EDGE_TARGET[idx] as usize);
    }
  }
  None
}

"#,
  );

  write_array_u32(&mut out, "NODE_EDGE_START", &node_edge_start);
  write_array_u16(&mut out, "NODE_EDGE_LEN", &node_edge_len);
  write_array_u8(&mut out, "NODE_TERMINAL_MASK", &node_terminal_mask);
  write_array_u16(&mut out, "EDGE_CODE_UNIT", &edge_code_unit);
  write_array_u32(&mut out, "EDGE_TARGET", &edge_target);

  Ok(out)
}

fn compute_max_terminal_prefixes(nodes: &[TrieNode]) -> usize {
  let mut max_matches = 0usize;
  let mut stack: Vec<(usize, usize)> = Vec::new();
  stack.push((0, 0));

  while let Some((node_idx, seen)) = stack.pop() {
    let node = &nodes[node_idx];
    let next_seen = seen + usize::from(node.terminal_mask != 0);
    max_matches = max_matches.max(next_seen);
    for &child in node.children.values() {
      stack.push((child, next_seen));
    }
  }

  // Exclude the root node (it should never be terminal anyway).
  max_matches.saturating_sub(usize::from(nodes.first().map_or(false, |n| n.terminal_mask != 0)))
}

fn write_array_u8(out: &mut String, name: &str, values: &[u8]) {
  out.push_str("#[rustfmt::skip]\n");
  out.push_str(&format!("const {name}: &[u8] = &[\n"));
  write_values(out, values, 24);
  out.push_str("];\n\n");
}

fn write_array_u16(out: &mut String, name: &str, values: &[u16]) {
  out.push_str("#[rustfmt::skip]\n");
  out.push_str(&format!("const {name}: &[u16] = &[\n"));
  write_values(out, values, 16);
  out.push_str("];\n\n");
}

fn write_array_u32(out: &mut String, name: &str, values: &[u32]) {
  out.push_str("#[rustfmt::skip]\n");
  out.push_str(&format!("const {name}: &[u32] = &[\n"));
  write_values(out, values, 12);
  out.push_str("];\n\n");
}

fn write_values<T: std::fmt::Display>(out: &mut String, values: &[T], per_line: usize) {
  for (idx, value) in values.iter().enumerate() {
    if idx % per_line == 0 {
      out.push_str("  ");
    }
    out.push_str(&format!("{value}"));
    out.push_str(", ");
    if idx % per_line == per_line - 1 {
      out.push('\n');
    }
  }
  if !values.is_empty() && values.len() % per_line != 0 {
    out.push('\n');
  }
}

fn parse_property_file(path: &Path) -> Result<Vec<Vec<u16>>> {
  let contents = fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
  parse_match_strings_array(&contents)
}

fn parse_match_strings_array(contents: &str) -> Result<Vec<Vec<u16>>> {
  let anchor = "matchStrings:";
  let start = contents
    .find(anchor)
    .ok_or_else(|| anyhow!("missing {anchor:?}"))?;
  let after = &contents[start + anchor.len()..];
  let open = after
    .find('[')
    .ok_or_else(|| anyhow!("missing '[' after {anchor:?}"))?;
  let mut idx = start + anchor.len() + open + 1;

  let bytes = contents.as_bytes();
  let mut out = Vec::new();

  loop {
    skip_ws(bytes, &mut idx);
    if idx >= bytes.len() {
      bail!("unexpected EOF while parsing matchStrings array");
    }
    match bytes[idx] {
      b']' => {
        break;
      }
      b',' => {
        idx += 1;
        continue;
      }
      b'"' => {
        let s = parse_js_string_literal(contents, &mut idx)?;
        out.push(s);
      }
      other => {
        bail!("unexpected byte {other:?} while parsing matchStrings array");
      }
    }
  }

  Ok(out)
}

fn skip_ws(bytes: &[u8], idx: &mut usize) {
  while *idx < bytes.len() {
    match bytes[*idx] {
      b' ' | b'\n' | b'\r' | b'\t' => *idx += 1,
      _ => break,
    }
  }
}

fn parse_js_string_literal(input: &str, idx: &mut usize) -> Result<Vec<u16>> {
  let bytes = input.as_bytes();
  if *idx >= bytes.len() || bytes[*idx] != b'"' {
    bail!("expected opening '\"' for string literal");
  }
  *idx += 1;

  let mut out: Vec<u16> = Vec::new();
  while *idx < bytes.len() {
    let b = bytes[*idx];
    match b {
      b'"' => {
        *idx += 1;
        return Ok(out);
      }
      b'\\' => {
        *idx += 1;
        if *idx >= bytes.len() {
          bail!("unexpected EOF in string escape");
        }
        let esc = bytes[*idx];
        *idx += 1;
        match esc {
          b'u' => parse_unicode_escape(input, idx, &mut out)?,
          b'x' => parse_hex_escape(input, idx, &mut out)?,
          b'\\' => out.push(b'\\' as u16),
          b'"' => out.push(b'"' as u16),
          b'n' => out.push(b'\n' as u16),
          b'r' => out.push(b'\r' as u16),
          b't' => out.push(b'\t' as u16),
          b'b' => out.push(0x08),
          b'f' => out.push(0x0C),
          b'v' => out.push(0x0B),
          b'0' => out.push(0x0000),
          b'\n' => {}
          b'\r' => {
            // Handle Windows-style line continuation: `\\\r\n`.
            if *idx < bytes.len() && bytes[*idx] == b'\n' {
              *idx += 1;
            }
          }
          other => bail!("unsupported JS escape sequence: \\{}", other as char),
        }
      }
      _ => {
        let ch = input[*idx..]
          .chars()
          .next()
          .ok_or_else(|| anyhow!("unexpected EOF decoding UTF-8"))?;
        *idx += ch.len_utf8();
        let mut buf = [0u16; 2];
        let encoded = ch.encode_utf16(&mut buf);
        out.extend_from_slice(encoded);
      }
    }
  }

  bail!("unexpected EOF in string literal");
}

fn parse_hex_escape(input: &str, idx: &mut usize, out: &mut Vec<u16>) -> Result<()> {
  let bytes = input.as_bytes();
  if *idx + 2 > bytes.len() {
    bail!("unexpected EOF in \\x escape");
  }
  let hex = &input[*idx..*idx + 2];
  let value = u8::from_str_radix(hex, 16).with_context(|| format!("invalid \\x escape {hex:?}"))?;
  out.push(value as u16);
  *idx += 2;
  Ok(())
}

fn parse_unicode_escape(input: &str, idx: &mut usize, out: &mut Vec<u16>) -> Result<()> {
  let bytes = input.as_bytes();
  if *idx >= bytes.len() {
    bail!("unexpected EOF in \\u escape");
  }

  if bytes[*idx] == b'{' {
    *idx += 1;
    let start = *idx;
    while *idx < bytes.len() && bytes[*idx] != b'}' {
      *idx += 1;
    }
    if *idx >= bytes.len() {
      bail!("unexpected EOF in \\u{{...}} escape");
    }
    let hex = &input[start..*idx];
    *idx += 1; // consume '}'
    let cp = u32::from_str_radix(hex, 16).with_context(|| format!("invalid \\u{{...}} escape {hex:?}"))?;
    if cp > 0x10FFFF {
      bail!("code point out of range in \\u{{...}} escape: {cp:#x}");
    }
    push_code_point_utf16(out, cp);
    return Ok(());
  }

  if *idx + 4 > bytes.len() {
    bail!("unexpected EOF in \\uXXXX escape");
  }
  let hex = &input[*idx..*idx + 4];
  let value = u16::from_str_radix(hex, 16).with_context(|| format!("invalid \\u escape {hex:?}"))?;
  out.push(value);
  *idx += 4;
  Ok(())
}

fn push_code_point_utf16(out: &mut Vec<u16>, cp: u32) {
  if let Some(ch) = char::from_u32(cp) {
    let mut buf = [0u16; 2];
    let encoded = ch.encode_utf16(&mut buf);
    out.extend_from_slice(encoded);
    return;
  }

  // `\u{...}` allows any code point up to 0x10FFFF, including surrogate code points. Encode those
  // directly as a UTF-16 code unit (spec matches JS behaviour).
  out.push(cp as u16);
}

struct Utf16Debug<'a>(&'a [u16]);

impl std::fmt::Debug for Utf16Debug<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "utf16[")?;
    for (i, cu) in self.0.iter().enumerate() {
      if i != 0 {
        write!(f, " ")?;
      }
      write!(f, "{cu:04X}")?;
    }
    write!(f, "]")
  }
}
