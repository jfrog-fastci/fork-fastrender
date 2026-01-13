//! ICU4X-backed spike for ECMAScript RegExp Unicode property escapes (`\p{..}` / `\P{..}`).
//!
//! This module is a **prototype / adapter layer**. It is *not* wired into the RegExp parser/VM
//! yet; it only answers whether we can resolve names/values and test membership using ICU4X
//! property data (`icu_properties`) without vendoring/parsing UCD text files.
//!
//! # Scope
//! Code-point properties required by ECMA-262 RegExp Unicode property escapes:
//! - Binary properties (ECMA-262 `table-binary-unicode-properties`)
//! - `General_Category` (`gc`)
//! - `Script` (`sc`)
//! - `Script_Extensions` (`scx`)
//!
//! Not in scope: string/sequence properties (notably emoji sequences).
//!
//! # Strict matching
//! This spike intentionally does **strict matching**:
//! - Case-sensitive
//! - No loose matching (no `_`/`-`/space folding)
//!
//! ICU4X *does* support loose matching via `PropertyParser::get_loose()`, but using it here would
//! blur the question we're trying to answer (data coverage). For a full ECMA-262 implementation,
//! we'd likely wrap the ICU parsers with additional ECMA-specific constraints/tests.
//!
//! # Surrogates
//! ECMAScript RegExp Unicode property escapes operate on Unicode *code points* and must handle
//! surrogate code points (`0xD800..=0xDFFF`) when they occur as isolated UTF-16 code units.
//!
//! ICU4X's `CodePoint*` APIs support `u32`-based lookup (`contains32` / `get32` / `has_script32`),
//! so we can query surrogate code points without going through Rust `char`.
//!
//! # Notes / findings
//! See `docs/regexp_unicode_properties_icu4x.md` for a short feasibility summary, including the
//! Unicode version baked into ICU4X's `compiled_data`.

use icu_properties::{CodePointMapData, CodePointSetData, PropertyParser};

/// Resolved property for a `\p{..}` / `\P{..}` escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedProperty {
  /// Binary property membership (the `bool` is the expected value, e.g. `=false`).
  Binary(BinaryProperty, bool),
  /// `gc=...` (or `General_Category=...`).
  GeneralCategory(icu_properties::props::GeneralCategoryGroup),
  /// `sc=...` (or `Script=...`).
  Script(icu_properties::props::Script),
  /// `scx=...` (or `Script_Extensions=...`).
  ScriptExtensions(icu_properties::props::Script),
}

/// Property "name" position in `\p{<name>=<value>}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyName {
  /// A binary property like `Alphabetic` / `Emoji`.
  Binary(BinaryProperty),
  /// `General_Category` / `gc`
  GeneralCategory,
  /// `Script` / `sc`
  Script,
  /// `Script_Extensions` / `scx`
  ScriptExtensions,
}

/// Resolve a property name with strict ECMA-262 spelling (case-sensitive).
pub(crate) fn resolve_property_name(name: &str) -> Option<PropertyName> {
  match name {
    "General_Category" | "gc" => Some(PropertyName::GeneralCategory),
    "Script" | "sc" => Some(PropertyName::Script),
    "Script_Extensions" | "scx" => Some(PropertyName::ScriptExtensions),
    _ => resolve_binary_property(name).map(PropertyName::Binary),
  }
}

/// Resolve a property value with strict ECMA-262 spelling (case-sensitive).
pub(crate) fn resolve_property_value(
  prop_name: PropertyName,
  value: &str,
) -> Option<ResolvedProperty> {
  match prop_name {
    PropertyName::Binary(bin) => match value {
      "true" => Some(ResolvedProperty::Binary(bin, true)),
      "false" => Some(ResolvedProperty::Binary(bin, false)),
      _ => None,
    },
    PropertyName::GeneralCategory => {
      // Note: `GeneralCategoryGroup` supports both atomic categories (Lu) and groups (L/Letter).
      PropertyParser::<icu_properties::props::GeneralCategoryGroup>::new()
        .get_strict(value)
        .map(ResolvedProperty::GeneralCategory)
    }
    PropertyName::Script => PropertyParser::<icu_properties::props::Script>::new()
      .get_strict(value)
      .map(ResolvedProperty::Script),
    PropertyName::ScriptExtensions => PropertyParser::<icu_properties::props::Script>::new()
      .get_strict(value)
      .map(ResolvedProperty::ScriptExtensions),
  }
}

/// Check whether the resolved property contains the given Unicode code point.
pub(crate) fn contains_code_point(prop: ResolvedProperty, cp: u32) -> bool {
  if cp > 0x10FFFF {
    return false;
  }

  match prop {
    ResolvedProperty::Binary(p, expected) => {
      let contained = contains_binary_property(p, cp);
      if expected { contained } else { !contained }
    }
    ResolvedProperty::GeneralCategory(group) => {
      let gc = CodePointMapData::<icu_properties::props::GeneralCategory>::new().get32(cp);
      group.contains(gc)
    }
    ResolvedProperty::Script(wanted) => {
      let sc = CodePointMapData::<icu_properties::props::Script>::new().get32(cp);
      sc == wanted
    }
    ResolvedProperty::ScriptExtensions(wanted) => {
      icu_properties::script::ScriptWithExtensions::new().has_script32(cp, wanted)
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Binary properties
// ---------------------------------------------------------------------------------------------

/// The 53 binary Unicode properties required by ECMA-262 `table-binary-unicode-properties`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryProperty {
  Alphabetic,
  Ascii,
  AsciiHexDigit,
  Any,
  Assigned,
  BidiControl,
  BidiMirrored,
  CaseIgnorable,
  Cased,
  ChangesWhenCasefolded,
  ChangesWhenCasemapped,
  ChangesWhenLowercased,
  ChangesWhenNfkcCasefolded,
  ChangesWhenTitlecased,
  ChangesWhenUppercased,
  Dash,
  DefaultIgnorableCodePoint,
  Deprecated,
  Diacritic,
  Emoji,
  EmojiComponent,
  EmojiModifier,
  EmojiModifierBase,
  EmojiPresentation,
  ExtendedPictographic,
  Extender,
  GraphemeBase,
  GraphemeExtend,
  HexDigit,
  IdsBinaryOperator,
  IdsTrinaryOperator,
  IdContinue,
  IdStart,
  Ideographic,
  JoinControl,
  LogicalOrderException,
  Lowercase,
  Math,
  NoncharacterCodePoint,
  PatternSyntax,
  PatternWhiteSpace,
  QuotationMark,
  Radical,
  RegionalIndicator,
  SentenceTerminal,
  SoftDotted,
  TerminalPunctuation,
  UnifiedIdeograph,
  Uppercase,
  VariationSelector,
  WhiteSpace,
  XidContinue,
  XidStart,
}

fn resolve_binary_property(name: &str) -> Option<BinaryProperty> {
  use icu_properties::props;
  use BinaryProperty::*;

  // Special cases: ICU4X doesn't expose these as standalone binary properties.
  match name {
    "Any" => return Some(Any),
    "ASCII" => return Some(Ascii),
    "Assigned" => return Some(Assigned),
    // ECMA-262 / UCD alias for `White_Space` (note: ICU4X's short name for `White_Space` is
    // `WSpace`, but the UCD alias list used by ECMA-262 uses `space`).
    "space" => return Some(WhiteSpace),
    _ => {}
  }

  let bytes = name.as_bytes();

  // `White_Space` is another special case: see the `"space"` comment above. We accept the canonical
  // name (`White_Space`) but intentionally do *not* accept ICU4X's `WSpace` alias, since it is not
  // part of the ECMA-262 alias set.
  if bytes == <props::WhiteSpace as props::BinaryProperty>::NAME {
    return Some(WhiteSpace);
  }

  macro_rules! m {
    ($variant:ident, $marker:ty) => {
      if bytes == <$marker as props::BinaryProperty>::NAME
        || bytes == <$marker as props::BinaryProperty>::SHORT_NAME
      {
        return Some($variant);
      }
    };
  }

  // Canonical + short alias name matching comes directly from ICU4X's property name data.
  m!(Alphabetic, props::Alphabetic);
  m!(AsciiHexDigit, props::AsciiHexDigit);
  m!(BidiControl, props::BidiControl);
  m!(BidiMirrored, props::BidiMirrored);
  m!(CaseIgnorable, props::CaseIgnorable);
  m!(Cased, props::Cased);
  m!(ChangesWhenCasefolded, props::ChangesWhenCasefolded);
  m!(ChangesWhenCasemapped, props::ChangesWhenCasemapped);
  m!(ChangesWhenLowercased, props::ChangesWhenLowercased);
  m!(ChangesWhenNfkcCasefolded, props::ChangesWhenNfkcCasefolded);
  m!(ChangesWhenTitlecased, props::ChangesWhenTitlecased);
  m!(ChangesWhenUppercased, props::ChangesWhenUppercased);
  m!(Dash, props::Dash);
  m!(DefaultIgnorableCodePoint, props::DefaultIgnorableCodePoint);
  m!(Deprecated, props::Deprecated);
  m!(Diacritic, props::Diacritic);
  m!(Emoji, props::Emoji);
  m!(EmojiComponent, props::EmojiComponent);
  m!(EmojiModifier, props::EmojiModifier);
  m!(EmojiModifierBase, props::EmojiModifierBase);
  m!(EmojiPresentation, props::EmojiPresentation);
  m!(ExtendedPictographic, props::ExtendedPictographic);
  m!(Extender, props::Extender);
  m!(GraphemeBase, props::GraphemeBase);
  m!(GraphemeExtend, props::GraphemeExtend);
  m!(HexDigit, props::HexDigit);
  m!(IdsBinaryOperator, props::IdsBinaryOperator);
  m!(IdsTrinaryOperator, props::IdsTrinaryOperator);
  m!(IdContinue, props::IdContinue);
  m!(IdStart, props::IdStart);
  m!(Ideographic, props::Ideographic);
  m!(JoinControl, props::JoinControl);
  m!(LogicalOrderException, props::LogicalOrderException);
  m!(Lowercase, props::Lowercase);
  m!(Math, props::Math);
  m!(NoncharacterCodePoint, props::NoncharacterCodePoint);
  m!(PatternSyntax, props::PatternSyntax);
  m!(PatternWhiteSpace, props::PatternWhiteSpace);
  m!(QuotationMark, props::QuotationMark);
  m!(Radical, props::Radical);
  m!(RegionalIndicator, props::RegionalIndicator);
  m!(SentenceTerminal, props::SentenceTerminal);
  m!(SoftDotted, props::SoftDotted);
  m!(TerminalPunctuation, props::TerminalPunctuation);
  m!(UnifiedIdeograph, props::UnifiedIdeograph);
  m!(Uppercase, props::Uppercase);
  m!(VariationSelector, props::VariationSelector);
  m!(XidContinue, props::XidContinue);
  m!(XidStart, props::XidStart);

  None
}

fn contains_binary_property(prop: BinaryProperty, cp: u32) -> bool {
  use icu_properties::props;
  use BinaryProperty::*;

  match prop {
    Any => true,
    Ascii => cp <= 0x7F,
    Assigned => {
      CodePointMapData::<props::GeneralCategory>::new().get32(cp) != props::GeneralCategory::Unassigned
    }

    // ICU4X-backed binary properties.
    Alphabetic => CodePointSetData::new::<props::Alphabetic>().contains32(cp),
    AsciiHexDigit => CodePointSetData::new::<props::AsciiHexDigit>().contains32(cp),
    BidiControl => CodePointSetData::new::<props::BidiControl>().contains32(cp),
    BidiMirrored => CodePointSetData::new::<props::BidiMirrored>().contains32(cp),
    CaseIgnorable => CodePointSetData::new::<props::CaseIgnorable>().contains32(cp),
    Cased => CodePointSetData::new::<props::Cased>().contains32(cp),
    ChangesWhenCasefolded => CodePointSetData::new::<props::ChangesWhenCasefolded>().contains32(cp),
    ChangesWhenCasemapped => CodePointSetData::new::<props::ChangesWhenCasemapped>().contains32(cp),
    ChangesWhenLowercased => CodePointSetData::new::<props::ChangesWhenLowercased>().contains32(cp),
    ChangesWhenNfkcCasefolded => {
      CodePointSetData::new::<props::ChangesWhenNfkcCasefolded>().contains32(cp)
    }
    ChangesWhenTitlecased => CodePointSetData::new::<props::ChangesWhenTitlecased>().contains32(cp),
    ChangesWhenUppercased => CodePointSetData::new::<props::ChangesWhenUppercased>().contains32(cp),
    Dash => CodePointSetData::new::<props::Dash>().contains32(cp),
    DefaultIgnorableCodePoint => CodePointSetData::new::<props::DefaultIgnorableCodePoint>().contains32(cp),
    Deprecated => CodePointSetData::new::<props::Deprecated>().contains32(cp),
    Diacritic => CodePointSetData::new::<props::Diacritic>().contains32(cp),
    Emoji => CodePointSetData::new::<props::Emoji>().contains32(cp),
    EmojiComponent => CodePointSetData::new::<props::EmojiComponent>().contains32(cp),
    EmojiModifier => CodePointSetData::new::<props::EmojiModifier>().contains32(cp),
    EmojiModifierBase => CodePointSetData::new::<props::EmojiModifierBase>().contains32(cp),
    EmojiPresentation => CodePointSetData::new::<props::EmojiPresentation>().contains32(cp),
    ExtendedPictographic => CodePointSetData::new::<props::ExtendedPictographic>().contains32(cp),
    Extender => CodePointSetData::new::<props::Extender>().contains32(cp),
    GraphemeBase => CodePointSetData::new::<props::GraphemeBase>().contains32(cp),
    GraphemeExtend => CodePointSetData::new::<props::GraphemeExtend>().contains32(cp),
    HexDigit => CodePointSetData::new::<props::HexDigit>().contains32(cp),
    IdsBinaryOperator => CodePointSetData::new::<props::IdsBinaryOperator>().contains32(cp),
    IdsTrinaryOperator => CodePointSetData::new::<props::IdsTrinaryOperator>().contains32(cp),
    IdContinue => CodePointSetData::new::<props::IdContinue>().contains32(cp),
    IdStart => CodePointSetData::new::<props::IdStart>().contains32(cp),
    Ideographic => CodePointSetData::new::<props::Ideographic>().contains32(cp),
    JoinControl => CodePointSetData::new::<props::JoinControl>().contains32(cp),
    LogicalOrderException => CodePointSetData::new::<props::LogicalOrderException>().contains32(cp),
    Lowercase => CodePointSetData::new::<props::Lowercase>().contains32(cp),
    Math => CodePointSetData::new::<props::Math>().contains32(cp),
    NoncharacterCodePoint => CodePointSetData::new::<props::NoncharacterCodePoint>().contains32(cp),
    PatternSyntax => CodePointSetData::new::<props::PatternSyntax>().contains32(cp),
    PatternWhiteSpace => CodePointSetData::new::<props::PatternWhiteSpace>().contains32(cp),
    QuotationMark => CodePointSetData::new::<props::QuotationMark>().contains32(cp),
    Radical => CodePointSetData::new::<props::Radical>().contains32(cp),
    RegionalIndicator => CodePointSetData::new::<props::RegionalIndicator>().contains32(cp),
    SentenceTerminal => CodePointSetData::new::<props::SentenceTerminal>().contains32(cp),
    SoftDotted => CodePointSetData::new::<props::SoftDotted>().contains32(cp),
    TerminalPunctuation => CodePointSetData::new::<props::TerminalPunctuation>().contains32(cp),
    UnifiedIdeograph => CodePointSetData::new::<props::UnifiedIdeograph>().contains32(cp),
    Uppercase => CodePointSetData::new::<props::Uppercase>().contains32(cp),
    VariationSelector => CodePointSetData::new::<props::VariationSelector>().contains32(cp),
    WhiteSpace => CodePointSetData::new::<props::WhiteSpace>().contains32(cp),
    XidContinue => CodePointSetData::new::<props::XidContinue>().contains32(cp),
    XidStart => CodePointSetData::new::<props::XidStart>().contains32(cp),
  }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strict_name_resolution_includes_aliases() {
    // Canonical + short alias.
    assert_eq!(
      resolve_property_name("Alphabetic"),
      Some(PropertyName::Binary(BinaryProperty::Alphabetic))
    );
    assert_eq!(
      resolve_property_name("Alpha"),
      Some(PropertyName::Binary(BinaryProperty::Alphabetic))
    );
    // Strict casing.
    assert_eq!(resolve_property_name("alphabetic"), None);
    assert_eq!(resolve_property_name("alpha"), None);

    // Enumerated property names + aliases.
    assert_eq!(resolve_property_name("gc"), Some(PropertyName::GeneralCategory));
    assert_eq!(resolve_property_name("General_Category"), Some(PropertyName::GeneralCategory));
    assert_eq!(resolve_property_name("GC"), None);

    assert_eq!(resolve_property_name("sc"), Some(PropertyName::Script));
    assert_eq!(resolve_property_name("scx"), Some(PropertyName::ScriptExtensions));

    // White_Space has an alias `space` (lowercase) in the UCD alias list.
    assert_eq!(
      resolve_property_name("White_Space"),
      Some(PropertyName::Binary(BinaryProperty::WhiteSpace))
    );
    assert_eq!(
      resolve_property_name("space"),
      Some(PropertyName::Binary(BinaryProperty::WhiteSpace))
    );
    // ICU's `WSpace` alias is not part of the ECMA-262 tables; keep matching strict.
    assert_eq!(resolve_property_name("WSpace"), None);
  }

  #[test]
  fn binary_property_names_match_ecma_table() {
    // ECMA-262 `table-binary-unicode-properties` uses the UCD aliases (Unicode v17.0.0). For most
    // properties the alias corresponds to ICU4X's `SHORT_NAME`; `White_Space` is the notable
    // exception (`space` vs `WSpace`), covered by the `"space"` special-case above.
    //
    // This test guards against accepting a different set of names than the spec table.
    const BINARY_PROPS: &[(&str, &str)] = &[
      ("ASCII", "ASCII"),
      ("ASCII_Hex_Digit", "AHex"),
      ("Alphabetic", "Alpha"),
      ("Any", "Any"),
      ("Assigned", "Assigned"),
      ("Bidi_Control", "Bidi_C"),
      ("Bidi_Mirrored", "Bidi_M"),
      ("Case_Ignorable", "CI"),
      ("Cased", "Cased"),
      ("Changes_When_Casefolded", "CWCF"),
      ("Changes_When_Casemapped", "CWCM"),
      ("Changes_When_Lowercased", "CWL"),
      ("Changes_When_NFKC_Casefolded", "CWKCF"),
      ("Changes_When_Titlecased", "CWT"),
      ("Changes_When_Uppercased", "CWU"),
      ("Dash", "Dash"),
      ("Default_Ignorable_Code_Point", "DI"),
      ("Deprecated", "Dep"),
      ("Diacritic", "Dia"),
      ("Emoji", "Emoji"),
      ("Emoji_Component", "EComp"),
      ("Emoji_Modifier", "EMod"),
      ("Emoji_Modifier_Base", "EBase"),
      ("Emoji_Presentation", "EPres"),
      ("Extended_Pictographic", "ExtPict"),
      ("Extender", "Ext"),
      ("Grapheme_Base", "Gr_Base"),
      ("Grapheme_Extend", "Gr_Ext"),
      ("Hex_Digit", "Hex"),
      ("IDS_Binary_Operator", "IDSB"),
      ("IDS_Trinary_Operator", "IDST"),
      ("ID_Continue", "IDC"),
      ("ID_Start", "IDS"),
      ("Ideographic", "Ideo"),
      ("Join_Control", "Join_C"),
      ("Logical_Order_Exception", "LOE"),
      ("Lowercase", "Lower"),
      ("Math", "Math"),
      ("Noncharacter_Code_Point", "NChar"),
      ("Pattern_Syntax", "Pat_Syn"),
      ("Pattern_White_Space", "Pat_WS"),
      ("Quotation_Mark", "QMark"),
      ("Radical", "Radical"),
      ("Regional_Indicator", "RI"),
      ("Sentence_Terminal", "STerm"),
      ("Soft_Dotted", "SD"),
      ("Terminal_Punctuation", "Term"),
      ("Unified_Ideograph", "UIdeo"),
      ("Uppercase", "Upper"),
      ("Variation_Selector", "VS"),
      ("White_Space", "space"),
      ("XID_Continue", "XIDC"),
      ("XID_Start", "XIDS"),
    ];

    for &(canonical, alias) in BINARY_PROPS {
      let Some(PropertyName::Binary(canonical_prop)) = resolve_property_name(canonical) else {
        panic!("expected canonical binary property {canonical:?} to resolve");
      };
      let Some(PropertyName::Binary(alias_prop)) = resolve_property_name(alias) else {
        panic!("expected alias {alias:?} (for {canonical:?}) to resolve");
      };
      assert_eq!(
        canonical_prop, alias_prop,
        "canonical name {canonical:?} and alias {alias:?} must resolve to the same property"
      );
    }
  }

  #[test]
  fn strict_value_resolution() {
    use icu_properties::props::{GeneralCategoryGroup as GcGroup, Script};

    assert_eq!(
      resolve_property_value(PropertyName::GeneralCategory, "Letter"),
      Some(ResolvedProperty::GeneralCategory(GcGroup::Letter))
    );
    assert_eq!(resolve_property_value(PropertyName::GeneralCategory, "letter"), None);

    // General_Category aliases (Unicode v17.0.0 / ECMA-262 tables).
    assert_eq!(
      resolve_property_value(PropertyName::GeneralCategory, "Lu"),
      Some(ResolvedProperty::GeneralCategory(GcGroup::UppercaseLetter))
    );
    assert_eq!(
      resolve_property_value(PropertyName::GeneralCategory, "Uppercase_Letter"),
      Some(ResolvedProperty::GeneralCategory(GcGroup::UppercaseLetter))
    );
    // Reject non-UCD spellings (no underscore).
    assert_eq!(
      resolve_property_value(PropertyName::GeneralCategory, "UppercaseLetter"),
      None
    );

    // Script long name and short code.
    assert_eq!(
      resolve_property_value(PropertyName::Script, "Latin"),
      Some(ResolvedProperty::Script(Script::Latin))
    );
    assert_eq!(
      resolve_property_value(PropertyName::Script, "Latn"),
      Some(ResolvedProperty::Script(Script::Latin))
    );
    assert_eq!(resolve_property_value(PropertyName::Script, "latin"), None);

    // Script values introduced in Unicode 17 (used as a proxy that ICU4X compiled data is >= 17).
    assert_eq!(
      resolve_property_value(PropertyName::Script, "Kirat_Rai"),
      Some(ResolvedProperty::Script(Script::KiratRai))
    );
    assert_eq!(
      resolve_property_value(PropertyName::Script, "Krai"),
      Some(ResolvedProperty::Script(Script::KiratRai))
    );
    // Reject Rust-style identifier spelling (not part of ECMA/UCD alias set).
    assert_eq!(resolve_property_value(PropertyName::Script, "KiratRai"), None);

    assert_eq!(
      resolve_property_value(PropertyName::Binary(BinaryProperty::Alphabetic), "true"),
      Some(ResolvedProperty::Binary(BinaryProperty::Alphabetic, true))
    );
    assert_eq!(
      resolve_property_value(PropertyName::Binary(BinaryProperty::Alphabetic), "false"),
      Some(ResolvedProperty::Binary(BinaryProperty::Alphabetic, false))
    );
    assert_eq!(
      resolve_property_value(PropertyName::Binary(BinaryProperty::Alphabetic), "False"),
      None
    );
  }

  #[test]
  fn compiled_data_contains_unicode17_script_assignment() {
    // U+16D40 KIRAT RAI VOWEL SIGN AA (Unicode 17.0.0; Script=Kirat_Rai).
    //
    // This is a sanity check that the ICU4X `compiled_data` used by this crate is at least
    // Unicode 17.0.0 (the version required by current test262 RegExp property escape tables).
    use icu_properties::props::Script;

    assert_eq!(
      CodePointMapData::<Script>::new().get32(0x16D40),
      Script::KiratRai
    );
    assert!(icu_properties::script::ScriptWithExtensions::new().has_script32(0x16D40, Script::KiratRai));
  }

  #[test]
  fn membership_including_surrogates() {
    use icu_properties::props::{GeneralCategoryGroup as GcGroup, Script};

    // Any matches everything, including surrogates.
    assert!(contains_code_point(
      ResolvedProperty::Binary(BinaryProperty::Any, true),
      0x0041
    ));
    assert!(contains_code_point(
      ResolvedProperty::Binary(BinaryProperty::Any, true),
      0xD800
    ));

    // ASCII is only ASCII.
    assert!(contains_code_point(
      ResolvedProperty::Binary(BinaryProperty::Ascii, true),
      0x7F
    ));
    assert!(!contains_code_point(
      ResolvedProperty::Binary(BinaryProperty::Ascii, true),
      0x80
    ));

    // Assigned includes surrogates (gc=Surrogate, not Cn).
    assert!(contains_code_point(
      ResolvedProperty::Binary(BinaryProperty::Assigned, true),
      0xD800
    ));

    // gc=Surrogate should match surrogate code points.
    assert!(contains_code_point(
      ResolvedProperty::GeneralCategory(GcGroup::Surrogate),
      0xD800
    ));
    assert!(!contains_code_point(
      ResolvedProperty::GeneralCategory(GcGroup::Surrogate),
      0x0041
    ));

    // Script and Script_Extensions treat surrogates as Unknown.
    assert!(contains_code_point(
      ResolvedProperty::Script(Script::Unknown),
      0xD800
    ));
    assert!(contains_code_point(
      ResolvedProperty::ScriptExtensions(Script::Unknown),
      0xD800
    ));

    // Latin letter.
    assert!(contains_code_point(
      ResolvedProperty::Script(Script::Latin),
      0x0041
    ));
    assert!(contains_code_point(
      ResolvedProperty::ScriptExtensions(Script::Latin),
      0x0041
    ));
    assert!(!contains_code_point(
      ResolvedProperty::Script(Script::Greek),
      0x0041
    ));

    // Test a documented Script_Extensions divergence: U+0650 has sc=Inherited but scx includes
    // Arabic + Syriac (ICU4X docs example).
    assert!(contains_code_point(
      ResolvedProperty::Script(Script::Inherited),
      0x0650
    ));
    assert!(contains_code_point(
      ResolvedProperty::ScriptExtensions(Script::Arabic),
      0x0650
    ));
    assert!(!contains_code_point(
      ResolvedProperty::ScriptExtensions(Script::Inherited),
      0x0650
    ));
  }
}
