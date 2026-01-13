use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use xtask::js_string_literal::decode_js_string_literal_to_utf16;

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

  let test262_input_dir = repo_root.join(
    "vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings",
  );
  // The test262 corpus is a heavyweight nested submodule which CI intentionally does not fetch by
  // default. Keep CI deterministic by falling back to a small vendored snapshot of the generated
  // input lists.
  let vendored_input_dir = repo_root.join("tools/unicode/regexp_unicode_string_props");
  let input_dir = if test262_input_dir.join("RGI_Emoji.js").is_file() {
    test262_input_dir
  } else if vendored_input_dir.join("RGI_Emoji.js").is_file() {
    vendored_input_dir
  } else {
    bail!(
      "missing RegExp Unicode property-of-strings inputs (expected either {:?} (test262 submodule) \
       or {:?} (vendored snapshot))",
      test262_input_dir,
      vendored_input_dir
    );
  };
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

#[derive(Debug)]
struct ParsedPropertyFile {
  match_strings: Vec<Vec<u16>>,
  non_match_strings: Vec<Vec<u16>>,
}

#[derive(Debug)]
struct PropertyData {
  name: &'static str,
  bit: u8,
  match_strings: Vec<Vec<u16>>,
  non_match_strings: Vec<Vec<u16>>,
}

fn generate(input_dir: &Path) -> Result<String> {
  let mut union: BTreeMap<Vec<u16>, u8> = BTreeMap::new();
  let mut property_data: Vec<PropertyData> = Vec::new();

  for spec in PROPERTY_SPECS {
    let path = input_dir.join(format!("{}.js", spec.js_name));
    let parsed = parse_property_file(&path)
      .with_context(|| format!("parsing property-of-strings list from {path:?}"))?;

    let mut set: BTreeSet<Vec<u16>> = BTreeSet::new();
    for s in &parsed.match_strings {
      if !set.insert(s.clone()) {
        bail!("duplicate string literal in {}: {:?}", spec.js_name, Utf16Debug(s));
      }
    }

    for s in &set {
      union.entry(s.clone()).and_modify(|mask| *mask |= spec.bit).or_insert(spec.bit);
    }

    property_data.push(PropertyData {
      name: spec.js_name,
      bit: spec.bit,
      match_strings: set.iter().cloned().collect(),
      non_match_strings: parsed.non_match_strings,
    });
  }

  // Validate that `RGI_Emoji` (the union property) matches the union of the base properties.
  let rgi_path = input_dir.join("RGI_Emoji.js");
  let rgi_parsed =
    parse_property_file(&rgi_path).with_context(|| format!("parsing {rgi_path:?}"))?;
  let mut rgi_set: BTreeSet<Vec<u16>> = BTreeSet::new();
  for s in &rgi_parsed.match_strings {
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

  property_data.push(PropertyData {
    name: "RGI_Emoji",
    bit: ALL_PROPERTY_MASK,
    match_strings: rgi_set.iter().cloned().collect(),
    non_match_strings: rgi_parsed.non_match_strings,
  });

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

  validate_flat_trie(
    &property_data,
    &union,
    &node_edge_start,
    &node_edge_len,
    &node_terminal_mask,
    &edge_code_unit,
    &edge_target,
  )?;

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

  out.push_str(
    r#"/// Lookup a RegExp `v` flag Unicode property of strings by exact name (no aliases).
///
/// Note: Unicode string properties cannot be negated (`\P{...}` / `[^...]`) per ECMA-262.
pub(crate) fn is_string_property_name(name: &str) -> Option<UnicodeStringProperty> {
  match name {
    "Basic_Emoji" => Some(UnicodeStringProperty::BasicEmoji),
    "Emoji_Keycap_Sequence" => Some(UnicodeStringProperty::EmojiKeycapSequence),
    "RGI_Emoji_Flag_Sequence" => Some(UnicodeStringProperty::RgiEmojiFlagSequence),
    "RGI_Emoji_Modifier_Sequence" => Some(UnicodeStringProperty::RgiEmojiModifierSequence),
    "RGI_Emoji_Tag_Sequence" => Some(UnicodeStringProperty::RgiEmojiTagSequence),
    "RGI_Emoji_ZWJ_Sequence" => Some(UnicodeStringProperty::RgiEmojiZwjSequence),
    "RGI_Emoji" => Some(UnicodeStringProperty::RgiEmoji),
    _ => None,
  }
}

"#,
  );

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

  out.push_str(
    r#"/// Call `f(len)` for each possible UTF-16 match length of `prop` starting at `start`.
///
/// This is allocation-free: match lengths are computed into a fixed-size stack array.
pub(crate) fn for_each_match_len(
  prop: UnicodeStringProperty,
  haystack: &[u16],
  start: usize,
  mut f: impl FnMut(usize),
) {
  let mut out = [0usize; MAX_MATCHES_PER_POSITION];
  let n = match_property_at(prop, haystack, start, &mut out);
  for &len in &out[..n] {
    f(len);
  }
}

/// Return the longest UTF-16 match length of `prop` at `start` (if any).
pub(crate) fn longest_match_len(
  prop: UnicodeStringProperty,
  haystack: &[u16],
  start: usize,
) -> Option<usize> {
  let mut out = [0usize; MAX_MATCHES_PER_POSITION];
  let n = match_property_at(prop, haystack, start, &mut out);
  if n == 0 { None } else { Some(out[n - 1]) }
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

fn validate_flat_trie(
  props: &[PropertyData],
  union: &BTreeMap<Vec<u16>, u8>,
  node_edge_start: &[u32],
  node_edge_len: &[u16],
  node_terminal_mask: &[u8],
  edge_code_unit: &[u16],
  edge_target: &[u32],
) -> Result<()> {
  if node_edge_start.len() != node_edge_len.len()
    || node_edge_start.len() != node_terminal_mask.len()
  {
    bail!(
      "invalid trie table: node arrays have mismatched lengths (start={}, len={}, mask={})",
      node_edge_start.len(),
      node_edge_len.len(),
      node_terminal_mask.len()
    );
  }
  if edge_code_unit.len() != edge_target.len() {
    bail!(
      "invalid trie table: edge arrays have mismatched lengths (code_unit={}, target={})",
      edge_code_unit.len(),
      edge_target.len()
    );
  }

  // Validate that the flattened trie encodes every union entry with the expected terminal mask.
  for (seq, expected_mask) in union {
    let Some(found) = lookup_terminal_mask(
      node_edge_start,
      node_edge_len,
      node_terminal_mask,
      edge_code_unit,
      edge_target,
      seq,
    )? else {
      bail!("trie missing union entry {:?} (expected mask {expected_mask:#x})", Utf16Debug(seq));
    };
    if found != *expected_mask {
      bail!(
        "trie mask mismatch for {:?}: expected {expected_mask:#x}, got {found:#x}",
        Utf16Debug(seq)
      );
    }
  }

  // Validate matchStrings / nonMatchStrings for each property file.
  for prop in props {
    for seq in &prop.match_strings {
      let Some(found) = lookup_terminal_mask(
        node_edge_start,
        node_edge_len,
        node_terminal_mask,
        edge_code_unit,
        edge_target,
        seq,
      )? else {
        bail!(
          "trie missing matchStrings entry for {}: {:?}",
          prop.name,
          Utf16Debug(seq)
        );
      };
      if (found & prop.bit) == 0 {
        bail!(
          "trie terminal mask missing bit for {} (bit={:#x}): {:?} (mask={found:#x})",
          prop.name,
          prop.bit,
          Utf16Debug(seq),
        );
      }
    }

    for seq in &prop.non_match_strings {
      if let Some(found) = lookup_terminal_mask(
        node_edge_start,
        node_edge_len,
        node_terminal_mask,
        edge_code_unit,
        edge_target,
        seq,
      )? {
        if (found & prop.bit) != 0 {
          bail!(
            "trie unexpectedly matches nonMatchStrings entry for {}: {:?} (mask={found:#x})",
            prop.name,
            Utf16Debug(seq)
          );
        }
      }
    }
  }

  Ok(())
}

fn lookup_terminal_mask(
  node_edge_start: &[u32],
  node_edge_len: &[u16],
  node_terminal_mask: &[u8],
  edge_code_unit: &[u16],
  edge_target: &[u32],
  seq: &[u16],
) -> Result<Option<u8>> {
  let mut node = 0usize;
  for (depth, &cu) in seq.iter().enumerate() {
    if node >= node_edge_start.len() {
      bail!("invalid trie node index {node} at depth {depth} for {:?}", Utf16Debug(seq));
    }
    let start = node_edge_start[node] as usize;
    let len = node_edge_len[node] as usize;
    let end = start.saturating_add(len);
    if end > edge_code_unit.len() || end > edge_target.len() {
      bail!(
        "invalid trie edge range [{start}, {end}) for node {node} (edges={}); input {:?}",
        edge_code_unit.len(),
        Utf16Debug(seq)
      );
    }

    let mut lo = 0usize;
    let mut hi = len;
    let mut next: Option<usize> = None;
    while lo < hi {
      let mid = (lo + hi) / 2;
      let idx = start + mid;
      let mid_cu = edge_code_unit[idx];
      if cu < mid_cu {
        hi = mid;
      } else if cu > mid_cu {
        lo = mid + 1;
      } else {
        next = Some(edge_target[idx] as usize);
        break;
      }
    }

    match next {
      Some(n) => node = n,
      None => return Ok(None),
    }
  }

  node_terminal_mask
    .get(node)
    .copied()
    .ok_or_else(|| anyhow!("invalid trie node index {node} after traversing {:?}", Utf16Debug(seq)))
    .map(Some)
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

fn parse_property_file(path: &Path) -> Result<ParsedPropertyFile> {
  let contents = fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
  let match_strings = parse_strings_array(&contents, "matchStrings")?;
  let non_match_strings = parse_strings_array(&contents, "nonMatchStrings")?;
  Ok(ParsedPropertyFile {
    match_strings,
    non_match_strings,
  })
}

fn parse_strings_array(contents: &str, key: &str) -> Result<Vec<Vec<u16>>> {
  let anchor = format!("{key}:");
  let start = contents
    .find(&anchor)
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
      bail!("unexpected EOF while parsing {key} array");
    }
    match bytes[idx] {
      b']' => break,
      b',' => {
        idx += 1;
      }
      b'"' => {
        let s = parse_js_string_literal(contents, &mut idx)?;
        out.push(s);
      }
      other => bail!("unexpected byte {other:?} while parsing {key} array"),
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
  let start = *idx;
  if start >= bytes.len() || bytes[start] != b'"' {
    bail!("expected opening '\"' for string literal");
  }

  let mut i = start + 1;
  while i < bytes.len() {
    match bytes[i] {
      b'\\' => {
        i += 1;
        if i >= bytes.len() {
          bail!("unexpected EOF in string escape");
        }
        i += 1;
      }
      b'"' => {
        let lit = input
          .get(start..=i)
          .ok_or_else(|| anyhow!("slice string literal"))?;
        *idx = i + 1;
        return decode_js_string_literal_to_utf16(lit)
          .with_context(|| format!("decoding JS string literal {lit:?}"));
      }
      _ => i += 1,
    }
  }

  bail!("unterminated string literal");
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
