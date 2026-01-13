use once_cell::sync::Lazy;
use memchr::memchr;

/// Returns whether `needle` exists in the sorted list `haystack`.
#[inline]
fn contains_sorted(haystack: &[&'static str], needle: &str) -> bool {
  haystack
    .binary_search_by(|probe| (*probe).cmp(needle))
    .is_ok()
}

fn parse_backtick_words(table: &'static str) -> Vec<&'static str> {
  // The vendored property tables are embedded as small HTML snippets. We only want the backticked
  // identifiers listed inside the `<pre>...</pre>` block, not any incidental backticks in comments
  // or surrounding prose.
  //
  // (The caller includes files like `specs/tc39-ecma262/table-binary-unicode-properties.html`
  // where the header comment mentions non-binary properties like `Script` — those must not end up
  // being treated as supported binary property names.)
  let table = match table.find("<pre>") {
    Some(start) => {
      let after_start = start + "<pre>".len();
      match table[after_start..].find("</pre>") {
        Some(end_rel) => &table[after_start..after_start + end_rel],
        None => &table[after_start..],
      }
    }
    None => table,
  };

  let bytes = table.as_bytes();
  let mut out = Vec::new();
  let mut i = 0usize;
  while i < bytes.len() {
    let Some(start_rel) = memchr(b'`', &bytes[i..]) else {
      break;
    };
    let start = i + start_rel;
    let after_start = start + 1;
    let Some(end_rel) = memchr(b'`', &bytes[after_start..]) else {
      break;
    };
    let end = after_start + end_rel;
    if let Some(word) = table.get(after_start..end) {
      if !word.is_empty() {
        out.push(word);
      }
    }
    i = end + 1;
  }
  out
}

static BINARY_UNICODE_PROPERTIES: Lazy<Vec<&'static str>> = Lazy::new(|| {
  const TABLE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../specs/tc39-ecma262/table-binary-unicode-properties.html"
  ));
  let mut words = parse_backtick_words(TABLE);
  words.sort_unstable();
  words.dedup();
  words
});

static STRING_UNICODE_PROPERTIES: Lazy<Vec<&'static str>> = Lazy::new(|| {
  const TABLE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../specs/tc39-ecma262/table-binary-unicode-properties-of-strings.html"
  ));
  let mut words = parse_backtick_words(TABLE);
  words.sort_unstable();
  words.dedup();
  words
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NonBinaryProperty {
  GeneralCategory,
  Script,
  ScriptExtensions,
}

fn nonbinary_property(name: &str) -> Option<NonBinaryProperty> {
  match name {
    "General_Category" | "gc" => Some(NonBinaryProperty::GeneralCategory),
    "Script" | "sc" => Some(NonBinaryProperty::Script),
    "Script_Extensions" | "scx" => Some(NonBinaryProperty::ScriptExtensions),
    _ => None,
  }
}

fn is_supported_binary_property(name: &str, unicode_sets_mode: bool) -> bool {
  if contains_sorted(&BINARY_UNICODE_PROPERTIES, name) {
    return true;
  }
  unicode_sets_mode && contains_sorted(&STRING_UNICODE_PROPERTIES, name)
}

pub(super) fn is_unicode_property_of_strings(name: &str) -> bool {
  contains_sorted(&STRING_UNICODE_PROPERTIES, name)
}

/// Validate the `UnicodePropertyValueExpression` inside a `\p{...}`/`\P{...}` escape.
///
/// This uses strict matching per test262: no whitespace, no case folding, and
/// no hyphen/underscore equivalence.
pub(super) fn validate_unicode_property_value_expression(
  expr: &str,
  unicode_sets_mode: bool,
) -> bool {
  if expr.is_empty() {
    return false;
  }

  if let Some((name, value)) = expr.split_once("=") {
    // Only a single `=` is allowed.
    if value.contains("=") {
      return false;
    }
    if name.is_empty() || value.is_empty() {
      return false;
    }

    // Binary properties cannot have an explicit value.
    if is_supported_binary_property(name, unicode_sets_mode) {
      return false;
    }

    let Some(prop) = nonbinary_property(name) else {
      return false;
    };

    match prop {
      NonBinaryProperty::GeneralCategory => contains_sorted(GENERAL_CATEGORY_VALUES, value),
      NonBinaryProperty::Script | NonBinaryProperty::ScriptExtensions => {
        contains_sorted(SCRIPT_VALUES, value)
      }
    }
  } else {
    // LoneUnicodePropertyNameOrValue: either a supported binary property name,
    // or a General_Category value.
    if is_supported_binary_property(expr, unicode_sets_mode) {
      return true;
    }
    contains_sorted(GENERAL_CATEGORY_VALUES, expr)
  }
}

// Property value lists extracted from the test262-generated Unicode property
// escape tests (Unicode v17.0.0).

const GENERAL_CATEGORY_VALUES: &[&'static str] = &[
  "C",
  "Cased_Letter",
  "Cc",
  "Cf",
  "Close_Punctuation",
  "Cn",
  "Co",
  "Combining_Mark",
  "Connector_Punctuation",
  "Control",
  "Cs",
  "Currency_Symbol",
  "Dash_Punctuation",
  "Decimal_Number",
  "Enclosing_Mark",
  "Final_Punctuation",
  "Format",
  "Initial_Punctuation",
  "L",
  "LC",
  "Letter",
  "Letter_Number",
  "Line_Separator",
  "Ll",
  "Lm",
  "Lo",
  "Lowercase_Letter",
  "Lt",
  "Lu",
  "M",
  "Mark",
  "Math_Symbol",
  "Mc",
  "Me",
  "Mn",
  "Modifier_Letter",
  "Modifier_Symbol",
  "N",
  "Nd",
  "Nl",
  "No",
  "Nonspacing_Mark",
  "Number",
  "Open_Punctuation",
  "Other",
  "Other_Letter",
  "Other_Number",
  "Other_Punctuation",
  "Other_Symbol",
  "P",
  "Paragraph_Separator",
  "Pc",
  "Pd",
  "Pe",
  "Pf",
  "Pi",
  "Po",
  "Private_Use",
  "Ps",
  "Punctuation",
  "S",
  "Sc",
  "Separator",
  "Sk",
  "Sm",
  "So",
  "Space_Separator",
  "Spacing_Mark",
  "Surrogate",
  "Symbol",
  "Titlecase_Letter",
  "Unassigned",
  "Uppercase_Letter",
  "Z",
  "Zl",
  "Zp",
  "Zs",
  "cntrl",
  "digit",
  "punct",
];

const SCRIPT_VALUES: &[&'static str] = &[
  "Adlam",
  "Adlm",
  "Aghb",
  "Ahom",
  "Anatolian_Hieroglyphs",
  "Arab",
  "Arabic",
  "Armenian",
  "Armi",
  "Armn",
  "Avestan",
  "Avst",
  "Bali",
  "Balinese",
  "Bamu",
  "Bamum",
  "Bass",
  "Bassa_Vah",
  "Batak",
  "Batk",
  "Beng",
  "Bengali",
  "Berf",
  "Beria_Erfe",
  "Bhaiksuki",
  "Bhks",
  "Bopo",
  "Bopomofo",
  "Brah",
  "Brahmi",
  "Brai",
  "Braille",
  "Bugi",
  "Buginese",
  "Buhd",
  "Buhid",
  "Cakm",
  "Canadian_Aboriginal",
  "Cans",
  "Cari",
  "Carian",
  "Caucasian_Albanian",
  "Chakma",
  "Cham",
  "Cher",
  "Cherokee",
  "Chorasmian",
  "Chrs",
  "Common",
  "Copt",
  "Coptic",
  "Cpmn",
  "Cprt",
  "Cuneiform",
  "Cypriot",
  "Cypro_Minoan",
  "Cyrillic",
  "Cyrl",
  "Deseret",
  "Deva",
  "Devanagari",
  "Diak",
  "Dives_Akuru",
  "Dogr",
  "Dogra",
  "Dsrt",
  "Dupl",
  "Duployan",
  "Egyp",
  "Egyptian_Hieroglyphs",
  "Elba",
  "Elbasan",
  "Elym",
  "Elymaic",
  "Ethi",
  "Ethiopic",
  "Gara",
  "Garay",
  "Geor",
  "Georgian",
  "Glag",
  "Glagolitic",
  "Gong",
  "Gonm",
  "Goth",
  "Gothic",
  "Gran",
  "Grantha",
  "Greek",
  "Grek",
  "Gujarati",
  "Gujr",
  "Gukh",
  "Gunjala_Gondi",
  "Gurmukhi",
  "Guru",
  "Gurung_Khema",
  "Han",
  "Hang",
  "Hangul",
  "Hani",
  "Hanifi_Rohingya",
  "Hano",
  "Hanunoo",
  "Hatr",
  "Hatran",
  "Hebr",
  "Hebrew",
  "Hira",
  "Hiragana",
  "Hluw",
  "Hmng",
  "Hmnp",
  "Hung",
  "Imperial_Aramaic",
  "Inherited",
  "Inscriptional_Pahlavi",
  "Inscriptional_Parthian",
  "Ital",
  "Java",
  "Javanese",
  "Kaithi",
  "Kali",
  "Kana",
  "Kannada",
  "Katakana",
  "Kawi",
  "Kayah_Li",
  "Khar",
  "Kharoshthi",
  "Khitan_Small_Script",
  "Khmer",
  "Khmr",
  "Khoj",
  "Khojki",
  "Khudawadi",
  "Kirat_Rai",
  "Kits",
  "Knda",
  "Krai",
  "Kthi",
  "Lana",
  "Lao",
  "Laoo",
  "Latin",
  "Latn",
  "Lepc",
  "Lepcha",
  "Limb",
  "Limbu",
  "Lina",
  "Linb",
  "Linear_A",
  "Linear_B",
  "Lisu",
  "Lyci",
  "Lycian",
  "Lydi",
  "Lydian",
  "Mahajani",
  "Mahj",
  "Maka",
  "Makasar",
  "Malayalam",
  "Mand",
  "Mandaic",
  "Mani",
  "Manichaean",
  "Marc",
  "Marchen",
  "Masaram_Gondi",
  "Medefaidrin",
  "Medf",
  "Meetei_Mayek",
  "Mend",
  "Mende_Kikakui",
  "Merc",
  "Mero",
  "Meroitic_Cursive",
  "Meroitic_Hieroglyphs",
  "Miao",
  "Mlym",
  "Modi",
  "Mong",
  "Mongolian",
  "Mro",
  "Mroo",
  "Mtei",
  "Mult",
  "Multani",
  "Myanmar",
  "Mymr",
  "Nabataean",
  "Nag_Mundari",
  "Nagm",
  "Nand",
  "Nandinagari",
  "Narb",
  "Nbat",
  "New_Tai_Lue",
  "Newa",
  "Nko",
  "Nkoo",
  "Nshu",
  "Nushu",
  "Nyiakeng_Puachue_Hmong",
  "Ogam",
  "Ogham",
  "Ol_Chiki",
  "Ol_Onal",
  "Olck",
  "Old_Hungarian",
  "Old_Italic",
  "Old_North_Arabian",
  "Old_Permic",
  "Old_Persian",
  "Old_Sogdian",
  "Old_South_Arabian",
  "Old_Turkic",
  "Old_Uyghur",
  "Onao",
  "Oriya",
  "Orkh",
  "Orya",
  "Osage",
  "Osge",
  "Osma",
  "Osmanya",
  "Ougr",
  "Pahawh_Hmong",
  "Palm",
  "Palmyrene",
  "Pau_Cin_Hau",
  "Pauc",
  "Perm",
  "Phag",
  "Phags_Pa",
  "Phli",
  "Phlp",
  "Phnx",
  "Phoenician",
  "Plrd",
  "Prti",
  "Psalter_Pahlavi",
  "Qaac",
  "Qaai",
  "Rejang",
  "Rjng",
  "Rohg",
  "Runic",
  "Runr",
  "Samaritan",
  "Samr",
  "Sarb",
  "Saur",
  "Saurashtra",
  "Sgnw",
  "Sharada",
  "Shavian",
  "Shaw",
  "Shrd",
  "Sidd",
  "Siddham",
  "Sidetic",
  "Sidt",
  "SignWriting",
  "Sind",
  "Sinh",
  "Sinhala",
  "Sogd",
  "Sogdian",
  "Sogo",
  "Sora",
  "Sora_Sompeng",
  "Soyo",
  "Soyombo",
  "Sund",
  "Sundanese",
  "Sunu",
  "Sunuwar",
  "Sylo",
  "Syloti_Nagri",
  "Syrc",
  "Syriac",
  "Tagalog",
  "Tagb",
  "Tagbanwa",
  "Tai_Le",
  "Tai_Tham",
  "Tai_Viet",
  "Tai_Yo",
  "Takr",
  "Takri",
  "Tale",
  "Talu",
  "Tamil",
  "Taml",
  "Tang",
  "Tangsa",
  "Tangut",
  "Tavt",
  "Tayo",
  "Telu",
  "Telugu",
  "Tfng",
  "Tglg",
  "Thaa",
  "Thaana",
  "Thai",
  "Tibetan",
  "Tibt",
  "Tifinagh",
  "Tirh",
  "Tirhuta",
  "Tnsa",
  "Todhri",
  "Todr",
  "Tolong_Siki",
  "Tols",
  "Toto",
  "Tulu_Tigalari",
  "Tutg",
  "Ugar",
  "Ugaritic",
  "Unknown",
  "Vai",
  "Vaii",
  "Vith",
  "Vithkuqi",
  "Wancho",
  "Wara",
  "Warang_Citi",
  "Wcho",
  "Xpeo",
  "Xsux",
  "Yezi",
  "Yezidi",
  "Yi",
  "Yiii",
  "Zanabazar_Square",
  "Zanb",
  "Zinh",
  "Zyyy",
  "Zzzz",
];
