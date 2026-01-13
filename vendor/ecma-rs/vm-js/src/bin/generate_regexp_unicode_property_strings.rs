use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Unicode version used by the vendored emoji sequence data.
const UNICODE_VERSION: &str = "17.0.0";

#[derive(Clone, Copy, Debug)]
struct PropertySpec {
  name: &'static str,
  bit: u8,
}

const PROPERTY_SPECS: &[PropertySpec] = &[
  PropertySpec {
    name: "Basic_Emoji",
    bit: 1 << 0,
  },
  PropertySpec {
    name: "Emoji_Keycap_Sequence",
    bit: 1 << 1,
  },
  PropertySpec {
    name: "RGI_Emoji_Modifier_Sequence",
    bit: 1 << 2,
  },
  PropertySpec {
    name: "RGI_Emoji_Flag_Sequence",
    bit: 1 << 3,
  },
  PropertySpec {
    name: "RGI_Emoji_Tag_Sequence",
    bit: 1 << 4,
  },
  PropertySpec {
    name: "RGI_Emoji_ZWJ_Sequence",
    bit: 1 << 5,
  },
];

const ALL_PROPERTY_MASK: u8 = (1 << 6) - 1;

fn main() {
  if let Err(err) = main_inner() {
    eprintln!("error: {err}");
    std::process::exit(1);
  }
}

fn main_inner() -> Result<(), String> {
  let mut check = false;
  for arg in env::args().skip(1) {
    match arg.as_str() {
      "--check" => check = true,
      other => {
        return Err(format!(
          "unknown argument {other:?}. Usage: generate_regexp_unicode_property_strings [--check]"
        ));
      }
    }
  }

  // vm-js crate lives at `<repo-root>/vendor/ecma-rs/vm-js`.
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../..")
    .canonicalize()
    .map_err(|e| format!("resolving repo root: {e}"))?;

  let emoji_dir = repo_root.join("tools/unicode/ucd-17.0.0/emoji");
  let output = repo_root.join("vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs");

  let generated = generate(&emoji_dir).map_err(|e| format!("generating tables: {e}"))?;

  if check {
    let existing = fs::read_to_string(&output).map_err(|e| format!("reading {output:?}: {e}"))?;
    if existing != generated {
      return Err(format!(
        "RegExp Unicode property-of-strings tables out of date: re-run `bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin generate_regexp_unicode_property_strings` (diff against {output:?})"
      ));
    }
    return Ok(());
  }

  fs::write(&output, generated).map_err(|e| format!("writing {output:?}: {e}"))?;
  Ok(())
}

#[derive(Debug, Default)]
struct TrieNode {
  children: BTreeMap<u16, usize>,
  terminal_mask: u8,
}

fn generate(emoji_dir: &Path) -> Result<String, String> {
  let seq_path = emoji_dir.join("emoji-sequences.txt");
  let zwj_path = emoji_dir.join("emoji-zwj-sequences.txt");

  let mut union: BTreeMap<Vec<u16>, u8> = BTreeMap::new();
  parse_emoji_sequence_file(&seq_path, &mut union)?;
  parse_emoji_sequence_file(&zwj_path, &mut union)?;

  // Validate all expected properties appear at least once.
  for spec in PROPERTY_SPECS {
    let has_any = union.values().any(|mask| (*mask & spec.bit) != 0);
    if !has_any {
      return Err(format!("property {} has no sequences (unexpected)", spec.name));
    }
  }

  // All terminal masks should be within the base property bitset.
  for (s, mask) in &union {
    if *mask == 0 || (*mask & !ALL_PROPERTY_MASK) != 0 {
      return Err(format!(
        "invalid property mask {mask:#x} for string {:?}",
        Utf16Debug(s)
      ));
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
      u32::try_from(edge_code_unit.len()).map_err(|_| "edge table too large for u32".to_string())?,
    );
    let len = node.children.len();
    let len_u16 = u16::try_from(len)
      .map_err(|_| format!("node has too many outgoing edges ({len}) for u16"))?;
    node_edge_len.push(len_u16);
    node_terminal_mask.push(node.terminal_mask);

    for (&cu, &child) in node.children.iter() {
      edge_code_unit.push(cu);
      edge_target.push(u32::try_from(child).map_err(|_| "node index too large for u32".to_string())?);
    }
  }

  let mut out = String::new();
  out.push_str("//! @generated\n");
  out.push_str("//!\n");
  out.push_str("//! Unicode property escapes for RegExp `v` flag (properties of strings).\n");
  out.push_str("//!\n");
  out.push_str(
    "//! This file is generated by `bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin generate_regexp_unicode_property_strings`.\n",
  );
  out.push_str(&format!("//! Unicode v{UNICODE_VERSION}.\n"));
  out.push_str("//!\n");
  out.push_str("//! Sources (Unicode Emoji / UTS #51):\n");
  out.push_str("//! - `tools/unicode/ucd-17.0.0/emoji/emoji-sequences.txt`\n");
  out.push_str("//! - `tools/unicode/ucd-17.0.0/emoji/emoji-zwj-sequences.txt`\n");
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
  out.push_str(&format!("const PROP_ALL: u8 = {ALL_PROPERTY_MASK};\n\n"));

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

fn parse_emoji_sequence_file(path: &Path, union: &mut BTreeMap<Vec<u16>, u8>) -> Result<(), String> {
  let contents = fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;

  for (line_no, line) in contents.lines().enumerate() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      continue;
    }
    let line = line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }

    let mut parts = line.split(';');
    let code_field = parts
      .next()
      .ok_or_else(|| format!("missing code points field at {path:?}:{}", line_no + 1))?
      .trim();
    let type_field = parts
      .next()
      .ok_or_else(|| format!("missing type_field at {path:?}:{}", line_no + 1))?
      .trim();

    let Some(spec) = PROPERTY_SPECS.iter().find(|s| s.name == type_field) else {
      return Err(format!(
        "unknown emoji sequence type_field {type_field:?} at {path:?}:{}",
        line_no + 1
      ));
    };

    for seq in parse_emoji_code_points_field(code_field).map_err(|e| {
      format!(
        "parsing code points field {code_field:?} at {path:?}:{}: {e}",
        line_no + 1
      )
    })? {
      use std::collections::btree_map::Entry;
      match union.entry(seq) {
        Entry::Vacant(v) => {
          v.insert(spec.bit);
        }
        Entry::Occupied(mut o) => {
          let mask = o.get_mut();
          if (*mask & spec.bit) != 0 {
            return Err(format!(
              "duplicate sequence for {type_field} at {path:?}:{}: {:?}",
              line_no + 1,
              Utf16Debug(o.key())
            ));
          }
          *mask |= spec.bit;
        }
      }
    }
  }

  Ok(())
}

fn parse_emoji_code_points_field(field: &str) -> Result<Vec<Vec<u16>>, String> {
  let tokens: Vec<&str> = field.split_whitespace().collect();
  if tokens.is_empty() {
    return Err("empty code points field".to_string());
  }

  // Ranges in the UTS #51 emoji sequence files only appear for single-code-point entries.
  if tokens.len() == 1 {
    if let Some((start, end)) = tokens[0].split_once("..") {
      let start =
        u32::from_str_radix(start, 16).map_err(|e| format!("invalid range start {start:?}: {e}"))?;
      let end =
        u32::from_str_radix(end, 16).map_err(|e| format!("invalid range end {end:?}: {e}"))?;
      if start > end {
        return Err(format!("invalid range {start:04X}..{end:04X}"));
      }

      let mut out: Vec<Vec<u16>> = Vec::new();
      for cp in start..=end {
        if cp > 0x10FFFF {
          return Err(format!("code point out of range: {cp:#x}"));
        }
        let mut seq = Vec::new();
        push_code_point_utf16(&mut seq, cp);
        out.push(seq);
      }
      return Ok(out);
    }
  }

  let mut seq: Vec<u16> = Vec::new();
  for tok in tokens {
    if tok.contains("..") {
      return Err(format!(
        "unsupported range token {tok:?} in multi-code-point sequence"
      ));
    }
    let cp = u32::from_str_radix(tok, 16).map_err(|e| format!("invalid code point {tok:?}: {e}"))?;
    if cp > 0x10FFFF {
      return Err(format!("code point out of range: {cp:#x}"));
    }
    push_code_point_utf16(&mut seq, cp);
  }

  Ok(vec![seq])
}

fn push_code_point_utf16(out: &mut Vec<u16>, cp: u32) {
  if let Some(ch) = char::from_u32(cp) {
    let mut buf = [0u16; 2];
    let encoded = ch.encode_utf16(&mut buf);
    out.extend_from_slice(encoded);
    return;
  }

  // Should not happen for the Unicode Emoji files (which only contain scalar values), but keep the
  // implementation total.
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generator_matches_checked_in_tables() {
    // This test runs in the vendor/ecma-rs workspace, so compute the monorepo root the same way as
    // the binary entrypoint.
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("../../..")
      .canonicalize()
      .unwrap();
    let emoji_dir = repo_root.join("tools/unicode/ucd-17.0.0/emoji");
    let output = repo_root.join("vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs");

    let generated = generate(&emoji_dir).unwrap();
    let existing = fs::read_to_string(output).unwrap();
    assert_eq!(
      existing, generated,
      "checked-in regexp_unicode_property_strings.rs is out of date; re-run generator"
    );
  }
}
