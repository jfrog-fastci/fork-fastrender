use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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

const SPEC_BINARY_PROPS: &[&str] = &[
  "ASCII",
  "ASCII_Hex_Digit",
  "Alphabetic",
  "Any",
  "Assigned",
  "Bidi_Control",
  "Bidi_Mirrored",
  "Case_Ignorable",
  "Cased",
  "Changes_When_Casefolded",
  "Changes_When_Casemapped",
  "Changes_When_Lowercased",
  "Changes_When_NFKC_Casefolded",
  "Changes_When_Titlecased",
  "Changes_When_Uppercased",
  "Dash",
  "Default_Ignorable_Code_Point",
  "Deprecated",
  "Diacritic",
  "Emoji",
  "Emoji_Component",
  "Emoji_Modifier",
  "Emoji_Modifier_Base",
  "Emoji_Presentation",
  "Extended_Pictographic",
  "Extender",
  "Grapheme_Base",
  "Grapheme_Extend",
  "Hex_Digit",
  "IDS_Binary_Operator",
  "IDS_Trinary_Operator",
  "ID_Continue",
  "ID_Start",
  "Ideographic",
  "Join_Control",
  "Logical_Order_Exception",
  "Lowercase",
  "Math",
  "Noncharacter_Code_Point",
  "Pattern_Syntax",
  "Pattern_White_Space",
  "Quotation_Mark",
  "Radical",
  "Regional_Indicator",
  "Sentence_Terminal",
  "Soft_Dotted",
  "Terminal_Punctuation",
  "Unified_Ideograph",
  "Uppercase",
  "Variation_Selector",
  "White_Space",
  "XID_Continue",
  "XID_Start",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut check = false;
  for arg in env::args().skip(1) {
    if arg == "--check" {
      check = true;
    } else if arg == "--help" || arg == "-h" {
      eprintln!("Usage: generate_regexp_unicode_tables [--check]");
      return Ok(());
    } else {
      return Err(format!("unknown arg: {arg}").into());
    }
  }

  let vm_js_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let repo_root = vm_js_root
    .parent()
    .and_then(|p| p.parent())
    .and_then(|p| p.parent())
    .ok_or("vm-js should be nested under <repo>/vendor/ecma-rs/vm-js")?;

  let input_dir = repo_root.join("tools/unicode/ucd-17.0.0");
  let output = vm_js_root.join("src/regexp_unicode_tables.rs");
  let generated = generate_tables(&input_dir)?;

  if check {
    let existing = fs::read_to_string(&output)?;
    if existing != generated {
      return Err(format!(
        "regexp unicode tables out of date: re-run `bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin generate_regexp_unicode_tables` (diff against {output:?})"
      )
      .into());
    }
    return Ok(());
  }

  fs::write(&output, generated)?;
  Ok(())
}

fn generate_tables(input_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
  let property_aliases = parse_property_aliases(&input_dir.join("PropertyAliases.txt"))?;
  let value_aliases = parse_property_value_aliases(&input_dir.join("PropertyValueAliases.txt"))?;

  let gc_value_aliases = value_aliases
    .get("gc")
    .ok_or("PropertyValueAliases missing `gc` entries")?;
  let sc_value_aliases = value_aliases
    .get("sc")
    .ok_or("PropertyValueAliases missing `sc` entries")?;
  let scx_value_aliases = value_aliases
    .get("scx")
    .ok_or("PropertyValueAliases missing `scx` entries")?;

  // Binary properties.
  let mut binary_ranges: BTreeMap<String, Vec<Range>> = BTreeMap::new();
  parse_code_point_property_file(
    &input_dir.join("DerivedBinaryProperties.txt"),
    &property_aliases,
    &mut binary_ranges,
  )?;
  parse_code_point_property_file(
    &input_dir.join("emoji-data.txt"),
    &property_aliases,
    &mut binary_ranges,
  )?;

  // Non-binary properties.
  let mut gc_ranges: BTreeMap<String, Vec<Range>> = BTreeMap::new();
  parse_code_point_value_file(
    &input_dir.join("DerivedGeneralCategory.txt"),
    gc_value_aliases,
    &mut gc_ranges,
  )?;

  let mut script_ranges: BTreeMap<String, Vec<Range>> = BTreeMap::new();
  parse_code_point_value_file(
    &input_dir.join("Scripts.txt"),
    sc_value_aliases,
    &mut script_ranges,
  )?;

  let mut script_ext_ranges: BTreeMap<String, Vec<Range>> = BTreeMap::new();
  parse_script_extensions_file(
    &input_dir.join("ScriptExtensions.txt"),
    scx_value_aliases,
    &mut script_ext_ranges,
  )?;

  for (_, ranges) in binary_ranges.iter_mut() {
    merge_ranges(ranges);
  }
  for (_, ranges) in gc_ranges.iter_mut() {
    merge_ranges(ranges);
  }
  for (_, ranges) in script_ranges.iter_mut() {
    merge_ranges(ranges);
  }
  for (_, ranges) in script_ext_ranges.iter_mut() {
    merge_ranges(ranges);
  }

  // Ensure we have everything required by the spec table.
  for &prop in SPEC_BINARY_PROPS {
    if prop == "Any" || prop == "ASCII" || prop == "Assigned" {
      continue;
    }
    if binary_ranges.get(prop).is_none() {
      return Err(format!("missing binary property ranges for {prop:?}").into());
    }
  }
  if gc_ranges.get("Unassigned").is_none() {
    return Err("missing General_Category=Unassigned ranges (required for Assigned)".into());
  }

  let gc_values: Vec<String> = gc_ranges.keys().cloned().collect();
  let script_values: Vec<String> = script_ranges.keys().cloned().collect();

  // Script_Extensions uses the same value universe as Script.
  for value in script_values.iter() {
    if !script_ext_ranges.contains_key(value) {
      return Err(format!("missing Script_Extensions ranges for {value:?}").into());
    }
  }

  let gc_alias_list = sorted_unique_value_aliases(gc_value_aliases)?;
  let sc_alias_list = sorted_unique_value_aliases(sc_value_aliases)?;
  let scx_alias_list = sorted_unique_value_aliases(scx_value_aliases)?;

  let mut out = String::new();
  out.push_str("//! Unicode v17.0.0 property tables for ECMAScript RegExp `\\\\p` / `\\\\P`.\n");
  out.push_str("//!\n");
  out.push_str(
    "//! This file is @generated by `bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin generate_regexp_unicode_tables`.\n",
  );
  out.push_str("//! Source: `tools/unicode/ucd-17.0.0/*`.\n\n");
  out.push_str("#![allow(dead_code)]\n");
  out.push_str("#![allow(non_camel_case_types)]\n\n");
  out.push_str("#[derive(Clone, Copy, Debug)]\n");
  out.push_str("pub(crate) struct CodePointRange {\n");
  out.push_str("  pub(crate) start: u32,\n");
  out.push_str("  pub(crate) end: u32,\n");
  out.push_str("}\n\n");

  // Enums.
  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("#[repr(u8)]\n");
  out.push_str("pub(crate) enum BinaryProp {\n");
  for prop in SPEC_BINARY_PROPS {
    out.push_str(&format!("  {prop},\n"));
  }
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum NonBinaryProp {\n");
  out.push_str("  General_Category,\n");
  out.push_str("  Script,\n");
  out.push_str("  Script_Extensions,\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum StringProp {\n");
  out.push_str("  Basic_Emoji,\n");
  out.push_str("  Emoji_Keycap_Sequence,\n");
  out.push_str("  RGI_Emoji_Modifier_Sequence,\n");
  out.push_str("  RGI_Emoji_Flag_Sequence,\n");
  out.push_str("  RGI_Emoji_Tag_Sequence,\n");
  out.push_str("  RGI_Emoji_ZWJ_Sequence,\n");
  out.push_str("  RGI_Emoji,\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum UnicodePropertyName {\n");
  out.push_str("  Binary(BinaryProp),\n");
  out.push_str("  NonBinary(NonBinaryProp),\n");
  out.push_str("  String(StringProp),\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("#[repr(u8)]\n");
  out.push_str("pub(crate) enum GeneralCategory {\n");
  for value in gc_values.iter() {
    out.push_str(&format!("  {value},\n"));
  }
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("#[repr(u16)]\n");
  out.push_str("pub(crate) enum Script {\n");
  for value in script_values.iter() {
    out.push_str(&format!("  {value},\n"));
  }
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum NonBinaryValue {\n");
  out.push_str("  GeneralCategory(GeneralCategory),\n");
  out.push_str("  Script(Script),\n");
  out.push_str("}\n\n");

  out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
  out.push_str("pub(crate) enum ResolvedCodePointProperty {\n");
  out.push_str("  Binary(BinaryProp),\n");
  out.push_str("  GeneralCategory(GeneralCategory),\n");
  out.push_str("  Script(Script),\n");
  out.push_str("  ScriptExtensions(Script),\n");
  out.push_str("}\n\n");

  // Range tables.
  for prop in SPEC_BINARY_PROPS {
    let const_name = format!("BINARY_{}_RANGES", prop.to_ascii_uppercase());
    let ranges = match *prop {
      "Any" => vec![Range::new(0x000000, 0x10FFFF)],
      "ASCII" => vec![Range::new(0x000000, 0x00007F)],
      "Assigned" => Vec::new(),
      other => binary_ranges.get(other).cloned().unwrap_or_default(),
    };
    write_ranges(&mut out, &const_name, &ranges);
  }

  for value in gc_values.iter() {
    let const_name = format!("GC_{}_RANGES", value.to_ascii_uppercase());
    let ranges = gc_ranges.get(value).cloned().unwrap_or_default();
    write_ranges(&mut out, &const_name, &ranges);
  }

  for value in script_values.iter() {
    let const_name = format!("SC_{}_RANGES", value.to_ascii_uppercase());
    let ranges = script_ranges.get(value).cloned().unwrap_or_default();
    write_ranges(&mut out, &const_name, &ranges);
  }

  for value in script_values.iter() {
    let const_name = format!("SCX_{}_RANGES", value.to_ascii_uppercase());
    let ranges = script_ext_ranges.get(value).cloned().unwrap_or_default();
    write_ranges(&mut out, &const_name, &ranges);
  }

  // Table-of-tables.
  out.push_str("const BINARY_RANGES: &[&[CodePointRange]] = &[\n");
  for prop in SPEC_BINARY_PROPS {
    out.push_str(&format!(
      "  BINARY_{}_RANGES,\n",
      prop.to_ascii_uppercase()
    ));
  }
  out.push_str("];\n\n");

  out.push_str("const GC_RANGES: &[&[CodePointRange]] = &[\n");
  for value in gc_values.iter() {
    out.push_str(&format!("  GC_{}_RANGES,\n", value.to_ascii_uppercase()));
  }
  out.push_str("];\n\n");

  out.push_str("const SC_RANGES: &[&[CodePointRange]] = &[\n");
  for value in script_values.iter() {
    out.push_str(&format!("  SC_{}_RANGES,\n", value.to_ascii_uppercase()));
  }
  out.push_str("];\n\n");

  out.push_str("const SCX_RANGES: &[&[CodePointRange]] = &[\n");
  for value in script_values.iter() {
    out.push_str(&format!("  SCX_{}_RANGES,\n", value.to_ascii_uppercase()));
  }
  out.push_str("];\n\n");

  // Value alias resolver tables.
  out.push_str("const GC_VALUE_ALIASES: &[(&str, GeneralCategory)] = &[\n");
  for (alias, canonical) in gc_alias_list.iter() {
    out.push_str(&format!("  (\"{alias}\", GeneralCategory::{canonical}),\n"));
  }
  out.push_str("];\n\n");

  out.push_str("const SC_VALUE_ALIASES: &[(&str, Script)] = &[\n");
  for (alias, canonical) in sc_alias_list.iter() {
    out.push_str(&format!("  (\"{alias}\", Script::{canonical}),\n"));
  }
  out.push_str("];\n\n");

  out.push_str("const SCX_VALUE_ALIASES: &[(&str, Script)] = &[\n");
  for (alias, canonical) in scx_alias_list.iter() {
    out.push_str(&format!("  (\"{alias}\", Script::{canonical}),\n"));
  }
  out.push_str("];\n\n");

  // Resolvers + membership.
  out.push_str("#[inline]\n");
  out.push_str("pub(crate) fn resolve_property_name(\n");
  out.push_str("  name: &str,\n");
  out.push_str("  unicode_sets: bool,\n");
  out.push_str(") -> Option<UnicodePropertyName> {\n");
  out.push_str("  match name {\n");

  for s in [
    "Basic_Emoji",
    "Emoji_Keycap_Sequence",
    "RGI_Emoji_Modifier_Sequence",
    "RGI_Emoji_Flag_Sequence",
    "RGI_Emoji_Tag_Sequence",
    "RGI_Emoji_ZWJ_Sequence",
    "RGI_Emoji",
  ] {
    out.push_str(&format!(
      "    \"{s}\" => unicode_sets.then_some(UnicodePropertyName::String(StringProp::{s})),\n"
    ));
  }

  out.push_str("    \"General_Category\" | \"gc\" => Some(UnicodePropertyName::NonBinary(NonBinaryProp::General_Category)),\n");
  out.push_str("    \"Script\" | \"sc\" => Some(UnicodePropertyName::NonBinary(NonBinaryProp::Script)),\n");
  out.push_str("    \"Script_Extensions\" | \"scx\" => Some(UnicodePropertyName::NonBinary(NonBinaryProp::Script_Extensions)),\n");

  // Binary property names + aliases (from PropertyAliases.txt).
  let mut binary_aliases: Vec<(String, String)> = property_aliases
    .iter()
    .filter_map(|(alias, canonical)| {
      if SPEC_BINARY_PROPS.contains(&canonical.as_str()) {
        Some((alias.clone(), canonical.clone()))
      } else {
        None
      }
    })
    .collect();
  binary_aliases.sort();
  binary_aliases.dedup();
  for (alias, canonical) in binary_aliases.iter() {
    if canonical == "General_Category" || canonical == "Script" || canonical == "Script_Extensions" {
      continue;
    }
    out.push_str(&format!(
      "    \"{alias}\" => Some(UnicodePropertyName::Binary(BinaryProp::{canonical})),\n"
    ));
  }

  out.push_str("    _ => None,\n");
  out.push_str("  }\n");
  out.push_str("}\n\n");

  out.push_str("#[inline]\n");
  out.push_str(
    "pub(crate) fn resolve_property_value(prop: NonBinaryProp, value: &str) -> Option<NonBinaryValue> {\n",
  );
  out.push_str("  match prop {\n");
  out.push_str(
    "    NonBinaryProp::General_Category => resolve_gc_value(value).map(NonBinaryValue::GeneralCategory),\n",
  );
  out.push_str("    NonBinaryProp::Script => resolve_sc_value(value).map(NonBinaryValue::Script),\n");
  out.push_str(
    "    NonBinaryProp::Script_Extensions => resolve_scx_value(value).map(NonBinaryValue::Script),\n",
  );
  out.push_str("  }\n");
  out.push_str("}\n\n");

  out.push_str("#[inline]\n");
  out.push_str("fn resolve_gc_value(value: &str) -> Option<GeneralCategory> {\n");
  out.push_str("  GC_VALUE_ALIASES\n");
  out.push_str("    .binary_search_by(|(alias, _)| alias.cmp(&value))\n");
  out.push_str("    .ok()\n");
  out.push_str("    .map(|idx| GC_VALUE_ALIASES[idx].1)\n");
  out.push_str("}\n\n");

  out.push_str("#[inline]\n");
  out.push_str("fn resolve_sc_value(value: &str) -> Option<Script> {\n");
  out.push_str("  SC_VALUE_ALIASES\n");
  out.push_str("    .binary_search_by(|(alias, _)| alias.cmp(&value))\n");
  out.push_str("    .ok()\n");
  out.push_str("    .map(|idx| SC_VALUE_ALIASES[idx].1)\n");
  out.push_str("}\n\n");

  out.push_str("#[inline]\n");
  out.push_str("fn resolve_scx_value(value: &str) -> Option<Script> {\n");
  out.push_str("  SCX_VALUE_ALIASES\n");
  out.push_str("    .binary_search_by(|(alias, _)| alias.cmp(&value))\n");
  out.push_str("    .ok()\n");
  out.push_str("    .map(|idx| SCX_VALUE_ALIASES[idx].1)\n");
  out.push_str("}\n\n");

  out.push_str("#[inline]\n");
  out.push_str(
    "pub(crate) fn contains_code_point(prop: ResolvedCodePointProperty, cp: u32) -> bool {\n",
  );
  out.push_str("  if cp > 0x10FFFF {\n");
  out.push_str("    return false;\n");
  out.push_str("  }\n");
  out.push_str("  match prop {\n");
  out.push_str("    ResolvedCodePointProperty::Binary(prop) => {\n");
  out.push_str("      match prop {\n");
  out.push_str("        BinaryProp::Assigned => {\n");
  out.push_str(
    "          // `Assigned` is defined as the complement of `General_Category=Unassigned`.\n",
  );
  out.push_str(
    "          // This includes surrogate code points, since they are `General_Category=Surrogate`.\n",
  );
  out.push_str(
    "          !in_ranges(cp, GC_RANGES[GeneralCategory::Unassigned as usize])\n",
  );
  out.push_str("        }\n");
  out.push_str("        other => in_ranges(cp, BINARY_RANGES[other as usize]),\n");
  out.push_str("      }\n");
  out.push_str("    }\n");
  out.push_str(
    "    ResolvedCodePointProperty::GeneralCategory(gc) => in_ranges(cp, GC_RANGES[gc as usize]),\n",
  );
  out.push_str(
    "    ResolvedCodePointProperty::Script(sc) => in_ranges(cp, SC_RANGES[sc as usize]),\n",
  );
  out.push_str(
    "    ResolvedCodePointProperty::ScriptExtensions(sc) => in_ranges(cp, SCX_RANGES[sc as usize]),\n",
  );
  out.push_str("  }\n");
  out.push_str("}\n\n");

  out.push_str("#[inline]\n");
  out.push_str("fn in_ranges(cp: u32, ranges: &[CodePointRange]) -> bool {\n");
  out.push_str("  let mut lo = 0usize;\n");
  out.push_str("  let mut hi = ranges.len();\n");
  out.push_str("  while lo < hi {\n");
  out.push_str("    let mid = (lo + hi) / 2;\n");
  out.push_str("    let range = ranges[mid];\n");
  out.push_str("    if cp < range.start {\n");
  out.push_str("      hi = mid;\n");
  out.push_str("    } else if cp > range.end {\n");
  out.push_str("      lo = mid + 1;\n");
  out.push_str("    } else {\n");
  out.push_str("      return true;\n");
  out.push_str("    }\n");
  out.push_str("  }\n");
  out.push_str("  false\n");
  out.push_str("}\n");

  Ok(out)
}

fn sorted_unique_value_aliases<'a>(
  map: &'a HashMap<String, String>,
) -> Result<Vec<(&'a str, &'a str)>, Box<dyn std::error::Error>> {
  let mut out = Vec::with_capacity(map.len());
  for (alias, canonical) in map.iter() {
    out.push((alias.as_str(), canonical.as_str()));
  }
  out.sort_by(|(a, _), (b, _)| a.cmp(b));
  for w in out.windows(2) {
    let (a1, c1) = w[0];
    let (a2, c2) = w[1];
    if a1 == a2 && c1 != c2 {
      return Err(format!("duplicate alias {a1:?} maps to both {c1:?} and {c2:?}").into());
    }
  }
  out.dedup_by(|(a1, _), (a2, _)| a1 == a2);
  Ok(out)
}

fn parse_property_aliases(path: &Path) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
  let contents = fs::read_to_string(path)?;
  let mut map: HashMap<String, String> = HashMap::new();
  for (idx, raw_line) in contents.lines().enumerate() {
    let line_no = idx + 1;
    let line = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }
    let mut fields = line.split(';').map(|s| s.trim()).filter(|s| !s.is_empty());
    let alias = fields
      .next()
      .ok_or_else(|| format!("invalid PropertyAliases line {line_no}: missing alias"))?;
    let canonical = fields
      .next()
      .ok_or_else(|| format!("invalid PropertyAliases line {line_no}: missing canonical"))?;
    if let Some(prev) = map.insert(alias.to_string(), canonical.to_string()) {
      if prev != canonical {
        return Err(format!(
          "invalid PropertyAliases line {line_no}: alias {alias:?} maps to both {prev:?} and {canonical:?}"
        )
        .into());
      }
    }
  }
  Ok(map)
}

fn parse_property_value_aliases(
  path: &Path,
) -> Result<HashMap<String, HashMap<String, String>>, Box<dyn std::error::Error>> {
  let contents = fs::read_to_string(path)?;
  let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
  for (idx, raw_line) in contents.lines().enumerate() {
    let line_no = idx + 1;
    let line = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }
    let mut fields = line.split(';').map(|s| s.trim()).filter(|s| !s.is_empty());
    let prop = fields
      .next()
      .ok_or_else(|| format!("invalid PropertyValueAliases line {line_no}: missing property"))?;
    let alias = fields
      .next()
      .ok_or_else(|| format!("invalid PropertyValueAliases line {line_no}: missing alias"))?;
    let canonical = fields
      .next()
      .ok_or_else(|| format!("invalid PropertyValueAliases line {line_no}: missing canonical"))?;
    let map = out.entry(prop.to_string()).or_default();
    if let Some(prev) = map.insert(alias.to_string(), canonical.to_string()) {
      if prev != canonical {
        return Err(format!(
          "invalid PropertyValueAliases line {line_no}: {prop:?} alias {alias:?} maps to both {prev:?} and {canonical:?}"
        )
        .into());
      }
    }
  }
  Ok(out)
}

fn parse_range(raw: &str) -> Result<(u32, u32), Box<dyn std::error::Error>> {
  if let Some((start, end)) = raw.split_once("..") {
    let start = u32::from_str_radix(start.trim(), 16)?;
    let end = u32::from_str_radix(end.trim(), 16)?;
    if start > end {
      return Err(format!("range start > end: {raw:?}").into());
    }
    return Ok((start, end));
  }

  let value = u32::from_str_radix(raw.trim(), 16)?;
  Ok((value, value))
}

fn parse_code_point_property_file(
  path: &Path,
  property_aliases: &HashMap<String, String>,
  out: &mut BTreeMap<String, Vec<Range>>,
) -> Result<(), Box<dyn std::error::Error>> {
  let contents = fs::read_to_string(path)?;
  for (idx, raw_line) in contents.lines().enumerate() {
    let line_no = idx + 1;
    let line = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }

    let (range_raw, prop_raw) = line
      .split_once(';')
      .ok_or_else(|| format!("invalid file line {line_no}: missing ';'"))?;
    let (start, end) = parse_range(range_raw.trim())
      .map_err(|e| format!("invalid file line {line_no}: unable to parse range: {e}"))?;
    let prop_raw = prop_raw.trim();
    let canonical = property_aliases
      .get(prop_raw)
      .map(|s| s.as_str())
      .unwrap_or(prop_raw);
    if !SPEC_BINARY_PROPS.contains(&canonical) {
      continue;
    }
    out
      .entry(canonical.to_string())
      .or_default()
      .push(Range::new(start, end));
  }
  Ok(())
}

fn parse_code_point_value_file(
  path: &Path,
  value_aliases: &HashMap<String, String>,
  out: &mut BTreeMap<String, Vec<Range>>,
) -> Result<(), Box<dyn std::error::Error>> {
  let contents = fs::read_to_string(path)?;
  for (idx, raw_line) in contents.lines().enumerate() {
    let line_no = idx + 1;
    let line = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }

    let (range_raw, value_raw) = line
      .split_once(';')
      .ok_or_else(|| format!("invalid file line {line_no}: missing ';'"))?;
    let (start, end) = parse_range(range_raw.trim())
      .map_err(|e| format!("invalid file line {line_no}: unable to parse range: {e}"))?;
    let value_raw = value_raw.trim();
    let canonical = value_aliases
      .get(value_raw)
      .map(|s| s.as_str())
      .unwrap_or(value_raw);
    out
      .entry(canonical.to_string())
      .or_default()
      .push(Range::new(start, end));
  }
  Ok(())
}

fn parse_script_extensions_file(
  path: &Path,
  value_aliases: &HashMap<String, String>,
  out: &mut BTreeMap<String, Vec<Range>>,
) -> Result<(), Box<dyn std::error::Error>> {
  let contents = fs::read_to_string(path)?;
  for (idx, raw_line) in contents.lines().enumerate() {
    let line_no = idx + 1;
    let line = raw_line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }

    let (range_raw, value_raw) = line
      .split_once(';')
      .ok_or_else(|| format!("invalid file line {line_no}: missing ';'"))?;
    let (start, end) = parse_range(range_raw.trim())
      .map_err(|e| format!("invalid file line {line_no}: unable to parse range: {e}"))?;
    let value_raw = value_raw.trim();
    for token in value_raw.split_whitespace() {
      let canonical = value_aliases.get(token).map(|s| s.as_str()).unwrap_or(token);
      out
        .entry(canonical.to_string())
        .or_default()
        .push(Range::new(start, end));
    }
  }
  Ok(())
}

fn merge_ranges(ranges: &mut Vec<Range>) {
  ranges.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
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

fn write_ranges(out: &mut String, name: &str, ranges: &[Range]) {
  out.push_str(&format!("const {name}: &[CodePointRange] = &[\n"));
  for range in ranges {
    out.push_str(&format!(
      "  CodePointRange {{ start: 0x{start:06X}, end: 0x{end:06X} }},\n",
      start = range.start,
      end = range.end,
    ));
  }
  out.push_str("];\n\n");
}
