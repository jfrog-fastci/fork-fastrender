use anyhow::{bail, Context, Result};
use clap::Args;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Generate the Unicode binary property tables required by ECMA-262 RegExp Unicode property escapes.
///
/// The canonical property list is taken from `specs/tc39-ecma262/table-binary-unicode-properties.html`.
/// We intentionally keep this generator limited to that set (do not accidentally emit extra UCD
/// properties).
#[derive(Debug, Args)]
pub struct GenerateRegExpUnicodeTablesArgs {
  /// Verify that the checked-in tables are up-to-date (do not modify files).
  #[arg(long)]
  pub check: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
  start: u32,
  end: u32,
}

impl Range {
  fn new(start: u32, end: u32) -> Self {
    debug_assert!(start <= end);
    Self { start, end }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum UcdFile {
  PropList,
  DerivedCoreProperties,
  DerivedBinaryProperties,
  DerivedNormalizationProps,
  EmojiData,
}

impl UcdFile {
  fn filename(self) -> &'static str {
    match self {
      Self::PropList => "PropList.txt",
      Self::DerivedCoreProperties => "DerivedCoreProperties.txt",
      Self::DerivedBinaryProperties => "DerivedBinaryProperties.txt",
      Self::DerivedNormalizationProps => "DerivedNormalizationProps.txt",
      Self::EmojiData => "emoji-data.txt",
    }
  }
}

impl fmt::Display for UcdFile {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.filename())
  }
}

#[derive(Debug, Clone, Copy)]
struct CanonicalBinaryPropertyManifest {
  name: &'static str,
  /// Properties that are not sourced from UCD files.
  special_cased: bool,
  /// UCD file(s) where we expect this property to be defined.
  expected_sources: &'static [UcdFile],
}

const CANONICAL_BINARY_PROPERTY_MANIFEST: &[CanonicalBinaryPropertyManifest] = &[
  // NOTE: Keep this list in the exact order from
  // `specs/tc39-ecma262/table-binary-unicode-properties.html`.
  CanonicalBinaryPropertyManifest {
    name: "ASCII",
    special_cased: true,
    expected_sources: &[],
  },
  CanonicalBinaryPropertyManifest {
    name: "ASCII_Hex_Digit",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Alphabetic",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Any",
    special_cased: true,
    expected_sources: &[],
  },
  CanonicalBinaryPropertyManifest {
    name: "Assigned",
    special_cased: true,
    expected_sources: &[],
  },
  CanonicalBinaryPropertyManifest {
    name: "Bidi_Control",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Bidi_Mirrored",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedBinaryProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Case_Ignorable",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Cased",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Changes_When_Casefolded",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Changes_When_Casemapped",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Changes_When_Lowercased",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Changes_When_NFKC_Casefolded",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedNormalizationProps],
  },
  CanonicalBinaryPropertyManifest {
    name: "Changes_When_Titlecased",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Changes_When_Uppercased",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Dash",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Default_Ignorable_Code_Point",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Deprecated",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Diacritic",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Emoji",
    special_cased: false,
    expected_sources: &[UcdFile::EmojiData],
  },
  CanonicalBinaryPropertyManifest {
    name: "Emoji_Component",
    special_cased: false,
    expected_sources: &[UcdFile::EmojiData],
  },
  CanonicalBinaryPropertyManifest {
    name: "Emoji_Modifier",
    special_cased: false,
    expected_sources: &[UcdFile::EmojiData],
  },
  CanonicalBinaryPropertyManifest {
    name: "Emoji_Modifier_Base",
    special_cased: false,
    expected_sources: &[UcdFile::EmojiData],
  },
  CanonicalBinaryPropertyManifest {
    name: "Emoji_Presentation",
    special_cased: false,
    expected_sources: &[UcdFile::EmojiData],
  },
  CanonicalBinaryPropertyManifest {
    name: "Extended_Pictographic",
    special_cased: false,
    expected_sources: &[UcdFile::EmojiData],
  },
  CanonicalBinaryPropertyManifest {
    name: "Extender",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Grapheme_Base",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Grapheme_Extend",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Hex_Digit",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "IDS_Binary_Operator",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "IDS_Trinary_Operator",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "ID_Continue",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "ID_Start",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Ideographic",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Join_Control",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Logical_Order_Exception",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Lowercase",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Math",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Noncharacter_Code_Point",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Pattern_Syntax",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Pattern_White_Space",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Quotation_Mark",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Radical",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Regional_Indicator",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Sentence_Terminal",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Soft_Dotted",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Terminal_Punctuation",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Unified_Ideograph",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "Uppercase",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "Variation_Selector",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "White_Space",
    special_cased: false,
    expected_sources: &[UcdFile::PropList],
  },
  CanonicalBinaryPropertyManifest {
    name: "XID_Continue",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
  CanonicalBinaryPropertyManifest {
    name: "XID_Start",
    special_cased: false,
    expected_sources: &[UcdFile::DerivedCoreProperties],
  },
];

pub fn run_generate_regexp_unicode_tables(args: GenerateRegExpUnicodeTablesArgs) -> Result<()> {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .context("xtask should live one directory below the repo root")?
    .to_path_buf();

  let ucd_dir = repo_root.join("tools/unicode/ucd-17.0.0");
  let output = repo_root.join("vendor/ecma-rs/vm-js/src/regexp_unicode_property_tables.rs");

  let generated =
    generate_tables(&ucd_dir).with_context(|| format!("generating tables from {}", ucd_dir.display()))?;

  if args.check {
    let existing = fs::read_to_string(&output).with_context(|| format!("reading {output:?}"))?;
    if existing != generated {
      bail!(
        "RegExp Unicode tables out of date: re-run `bash scripts/cargo_agent.sh xtask generate-regexp-unicode-tables` (diff against {output:?})"
      );
    }
    return Ok(());
  }

  fs::write(&output, generated).with_context(|| format!("writing {output:?}"))?;
  Ok(())
}

fn generate_tables(ucd_dir: &Path) -> Result<String> {
  // Ensure all required inputs exist. If a file is missing we want to fail fast, loudly.
  let input_paths: &[(UcdFile, &str)] = &[
    (UcdFile::PropList, "PropList.txt"),
    (UcdFile::DerivedCoreProperties, "DerivedCoreProperties.txt"),
    (UcdFile::DerivedBinaryProperties, "DerivedBinaryProperties.txt"),
    (
      UcdFile::DerivedNormalizationProps,
      "DerivedNormalizationProps.txt",
    ),
    (UcdFile::EmojiData, "emoji-data.txt"),
  ];

  for (_, filename) in input_paths {
    let p = ucd_dir.join(filename);
    if !p.is_file() {
      bail!(
        "missing required Unicode 17.0.0 UCD file: {} (expected at {})",
        filename,
        p.display()
      );
    }
  }

  let canonical_set: BTreeSet<&'static str> = CANONICAL_BINARY_PROPERTY_MANIFEST
    .iter()
    .map(|p| p.name)
    .collect();

  let mut props: BTreeMap<String, Vec<Range>> = BTreeMap::new();

  parse_ucd_file(
    &ucd_dir.join(UcdFile::PropList.filename()),
    UcdFile::PropList,
    &canonical_set,
    &mut props,
  )?;
  parse_ucd_file(
    &ucd_dir.join(UcdFile::DerivedCoreProperties.filename()),
    UcdFile::DerivedCoreProperties,
    &canonical_set,
    &mut props,
  )?;
  parse_ucd_file(
    &ucd_dir.join(UcdFile::DerivedBinaryProperties.filename()),
    UcdFile::DerivedBinaryProperties,
    &canonical_set,
    &mut props,
  )?;
  parse_ucd_file(
    &ucd_dir.join(UcdFile::DerivedNormalizationProps.filename()),
    UcdFile::DerivedNormalizationProps,
    &canonical_set,
    &mut props,
  )?;
  parse_ucd_file(
    &ucd_dir.join(UcdFile::EmojiData.filename()),
    UcdFile::EmojiData,
    &canonical_set,
    &mut props,
  )?;

  // Canonicalise range lists.
  for ranges in props.values_mut() {
    ranges.sort_by(range_sort);
    merge_ranges(ranges);
  }

  // Coverage assertions: every canonical property must be either special-cased or present in the
  // parsed property map. If any are missing, fail generation (do not silently omit).
  let mut missing: Vec<(&'static str, &'static [UcdFile])> = Vec::new();
  for entry in CANONICAL_BINARY_PROPERTY_MANIFEST {
    if entry.special_cased {
      continue;
    }
    match props.get(entry.name) {
      Some(ranges) if !ranges.is_empty() => {}
      _ => missing.push((entry.name, entry.expected_sources)),
    }
  }

  if !missing.is_empty() {
    let mut msg = String::new();
    msg.push_str("Missing required ECMA-262 binary Unicode property data:\n");
    for (name, sources) in missing {
      msg.push_str("  - ");
      msg.push_str(name);
      msg.push_str(" (expected in ");
      for (i, src) in sources.iter().enumerate() {
        if i != 0 {
          msg.push_str(", ");
        }
        msg.push_str(src.filename());
      }
      msg.push_str(")\n");
    }
    bail!(msg);
  }

  // Emit Rust module.
  let mut out = String::new();
  out.push_str("//! ECMA-262 RegExp Unicode binary property tables.\n");
  out.push_str("//!\n");
  out.push_str(
    "//! This file is @generated by `bash scripts/cargo_agent.sh xtask generate-regexp-unicode-tables`.\n",
  );
  out.push_str("//! Source: `tools/unicode/ucd-17.0.0/` (Unicode 17.0.0 UCD).\n");
  out.push_str("//!\n");
  out.push_str(
    "//! This module is intentionally limited to the 53 canonical binary Unicode properties\n",
  );
  out.push_str(
    "//! required by ECMA-262 (see `specs/tc39-ecma262/table-binary-unicode-properties.html`).\n",
  );
  out.push_str("\n");
  out.push_str("#![allow(dead_code)]\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) struct CodePointRange {\n");
  out.push_str("  pub(crate) start: u32,\n");
  out.push_str("  pub(crate) end: u32,\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum BinaryPropertyKind {\n");
  out.push_str("  SpecialCase(BinaryPropertySpecialCase),\n");
  out.push_str("  Ranges(&'static [CodePointRange]),\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum BinaryPropertySpecialCase {\n");
  out.push_str("  Any,\n");
  out.push_str("  ASCII,\n");
  out.push_str("  Assigned,\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) struct CanonicalBinaryProperty {\n");
  out.push_str("  pub(crate) name: &'static str,\n");
  out.push_str("  pub(crate) kind: BinaryPropertyKind,\n");
  out.push_str("}\n\n");

  // Emit range tables as named constants.
  for entry in CANONICAL_BINARY_PROPERTY_MANIFEST {
    if entry.special_cased {
      continue;
    }

    let Some(ranges) = props.get(entry.name) else {
      // Covered by the coverage assertion above.
      continue;
    };
    let const_name = format!("{}_RANGES", entry.name.to_ascii_uppercase());
    out.push_str(&format!(
      "pub(crate) const {const_name}: &[CodePointRange] = &[\n"
    ));
    for range in ranges {
      out.push_str(&format!(
        "  CodePointRange {{ start: 0x{start:04X}, end: 0x{end:04X} }},\n",
        start = range.start,
        end = range.end
      ));
    }
    out.push_str("];\n\n");
  }

  // Emit the canonical property list in spec order.
  out.push_str("pub(crate) const CANONICAL_BINARY_PROPERTIES: &[CanonicalBinaryProperty] = &[\n");
  for entry in CANONICAL_BINARY_PROPERTY_MANIFEST {
    out.push_str("  CanonicalBinaryProperty { name: ");
    out.push_str(&format!("{:?}", entry.name));
    out.push_str(", kind: ");
    if entry.special_cased {
      let sc = match entry.name {
        "Any" => "BinaryPropertySpecialCase::Any",
        "ASCII" => "BinaryPropertySpecialCase::ASCII",
        "Assigned" => "BinaryPropertySpecialCase::Assigned",
        other => {
          bail!(
            "internal error: property {other} marked special-cased but no BinaryPropertySpecialCase variant exists"
          );
        }
      };
      out.push_str("BinaryPropertyKind::SpecialCase(");
      out.push_str(sc);
      out.push_str(")");
    } else {
      let const_name = format!("{}_RANGES", entry.name.to_ascii_uppercase());
      out.push_str("BinaryPropertyKind::Ranges(");
      out.push_str(&const_name);
      out.push_str(")");
    }
    out.push_str(" },\n");
  }
  out.push_str("];\n\n");

  out.push_str("#[cfg(test)]\n");
  out.push_str("mod tests {\n");
  out.push_str("  use super::*;\n");
  out.push_str("  use std::collections::HashSet;\n\n");
  out.push_str("  #[test]\n");
  out.push_str("  fn exposes_exactly_53_canonical_binary_properties() {\n");
  out.push_str("    let expected: &[&str] = &[\n");
  for entry in CANONICAL_BINARY_PROPERTY_MANIFEST {
    out.push_str(&format!("      {:?},\n", entry.name));
  }
  out.push_str("    ];\n");
  out.push_str("    assert_eq!(CANONICAL_BINARY_PROPERTIES.len(), expected.len());\n");
  out.push_str("    assert_eq!(expected.len(), 53);\n");
  out.push_str("    let mut seen = HashSet::new();\n");
  out.push_str("    for (idx, prop) in CANONICAL_BINARY_PROPERTIES.iter().enumerate() {\n");
  out.push_str("      assert_eq!(prop.name, expected[idx]);\n");
  out.push_str("      assert!(seen.insert(prop.name), \"duplicate property {name}\", name = prop.name);\n");
  out.push_str("      match prop.kind {\n");
  out.push_str("        BinaryPropertyKind::Ranges(ranges) => {\n");
  out.push_str("          assert!(!ranges.is_empty(), \"property {name} has empty table\", name = prop.name);\n");
  out.push_str("        }\n");
  out.push_str("        BinaryPropertyKind::SpecialCase(sc) => match (prop.name, sc) {\n");
  out.push_str("          (\"Any\", BinaryPropertySpecialCase::Any)\n");
  out.push_str("          | (\"ASCII\", BinaryPropertySpecialCase::ASCII)\n");
  out.push_str("          | (\"Assigned\", BinaryPropertySpecialCase::Assigned) => {}\n");
  out.push_str("          _ => panic!(\"unexpected special case mapping for {name}: {sc:?}\", name = prop.name),\n");
  out.push_str("        },\n");
  out.push_str("      }\n");
  out.push_str("    }\n");
  out.push_str("  }\n");
  out.push_str("}\n");

  Ok(out)
}

fn parse_ucd_file(
  path: &Path,
  file: UcdFile,
  canonical_set: &BTreeSet<&'static str>,
  out: &mut BTreeMap<String, Vec<Range>>,
) -> Result<()> {
  let contents = fs::read_to_string(path)
    .with_context(|| format!("reading Unicode data from {} ({})", path.display(), file))?;

  for (idx, raw_line) in contents.lines().enumerate() {
    let line_no = idx + 1;
    let line = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }

    let (range_raw, property_raw) = line.split_once(';').with_context(|| {
      format!(
        "invalid {} line {}: missing ';' in {:?}",
        file,
        line_no,
        raw_line
      )
    })?;
    let range_raw = range_raw.trim();
    let property_raw = property_raw.trim();

    if !canonical_set.contains(property_raw) {
      continue;
    }

    // Some Unicode files (notably emoji-data) can contain sequences. The ECMA-262 binary properties
    // we consume are code point properties, so ignore sequence rows.
    if range_raw.contains(char::is_whitespace) {
      continue;
    }

    let (start, end) = parse_range(range_raw).with_context(|| {
      format!(
        "invalid {} line {}: unable to parse codepoint/range {:?}",
        file, line_no, range_raw
      )
    })?;

    out
      .entry(property_raw.to_string())
      .or_default()
      .push(Range::new(start, end));
  }

  Ok(())
}

fn parse_range(raw: &str) -> Result<(u32, u32)> {
  if let Some((start, end)) = raw.split_once("..") {
    let start = u32::from_str_radix(start.trim(), 16)?;
    let end = u32::from_str_radix(end.trim(), 16)?;
    if start > end {
      bail!("range start > end: {raw:?}");
    }
    return Ok((start, end));
  }

  let value = u32::from_str_radix(raw.trim(), 16)?;
  Ok((value, value))
}

fn range_sort(a: &Range, b: &Range) -> Ordering {
  match a.start.cmp(&b.start) {
    Ordering::Equal => a.end.cmp(&b.end),
    other => other,
  }
}

fn merge_ranges(ranges: &mut Vec<Range>) {
  ranges.dedup();
  let mut out: Vec<Range> = Vec::with_capacity(ranges.len());
  for range in ranges.iter().copied() {
    if let Some(last) = out.last_mut() {
      if range.start <= last.end.saturating_add(1) {
        last.end = last.end.max(range.end);
        continue;
      }
    }
    out.push(range);
  }
  *ranges = out;
}
