use crate::regexp::RegExpSyntaxError;

/// Resolved `\p{...}` / `\P{...}` Unicode property query.
///
/// This is the glue between:
/// - The raw `UnicodePropertyValueExpression` source text (no normalization), and
/// - The internal property tables used by the RegExp engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedUnicodeProperty {
  CodePoint(ResolvedCodePointProperty),
  String(UnicodeStringProperty),
}

/// Resolved code point property query.
///
/// Note: this intentionally contains only the minimal set needed by current users/tests. The
/// mapping tables can be extended without changing the resolver algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedCodePointProperty {
  /// `General_Category=<value>`
  GeneralCategory(GeneralCategory),
  /// `Script=<value>`
  Script(Script),
  /// `Script_Extensions=<value>`
  ScriptExtensions(Script),
  /// Binary properties such as `ASCII`.
  Binary(BinaryCodePointProperty),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneralCategory {
  UppercaseLetter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Script {
  Latin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryCodePointProperty {
  ASCII,
  Assigned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnicodeStringProperty {
  RgiEmoji,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyName {
  Binary(BinaryCodePointProperty),
  NonBinary(NonBinaryPropertyName),
  String(UnicodeStringProperty),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonBinaryPropertyName {
  GeneralCategory,
  Script,
  ScriptExtensions,
}

#[inline]
fn syntax_error() -> RegExpSyntaxError {
  RegExpSyntaxError {
    message: "Invalid regular expression",
  }
}

/// Resolve the raw `UnicodePropertyValueExpression` source text (the text inside `\p{...}` /
/// `\P{...}`) into an internal property query.
///
/// This implements the ECMA-262 `UnicodePropertyValueExpression` handling, including the
/// `General_Category` lone-value precedence rule (`\p{Lu}` => `General_Category=Lu`).
///
/// `expr` is treated as a raw string: it is not trimmed, case-folded, or normalized.
pub(crate) fn resolve_unicode_property_value_expression(
  expr: &str,
  unicode_sets: bool,
) -> Result<ResolvedUnicodeProperty, RegExpSyntaxError> {
  // `UnicodePropertyValueExpression` is either:
  //   UnicodePropertyName = UnicodePropertyValue
  // or a lone name/value.
  if let Some(eq_pos) = expr.find('=') {
    // The parser should ensure there is at most one `=`, but be strict anyway.
    if expr[eq_pos.saturating_add(1)..].contains('=') {
      return Err(syntax_error());
    }

    let (name, value_with_eq) = expr.split_at(eq_pos);
    let value = &value_with_eq[1..]; // skip '='

    if name.is_empty() || value.is_empty() {
      return Err(syntax_error());
    }

    let Some(prop) = resolve_non_binary_property_name(name) else {
      // `name` must resolve to one of the non-binary properties; binary/unknown => SyntaxError.
      return Err(syntax_error());
    };

    match prop {
      NonBinaryPropertyName::GeneralCategory => {
        let Some(gc) = resolve_general_category_value(value) else {
          return Err(syntax_error());
        };
        Ok(ResolvedUnicodeProperty::CodePoint(
          ResolvedCodePointProperty::GeneralCategory(gc),
        ))
      }
      NonBinaryPropertyName::Script => {
        let Some(sc) = resolve_script_value(value) else {
          return Err(syntax_error());
        };
        Ok(ResolvedUnicodeProperty::CodePoint(
          ResolvedCodePointProperty::Script(sc),
        ))
      }
      NonBinaryPropertyName::ScriptExtensions => {
        let Some(sc) = resolve_script_value(value) else {
          return Err(syntax_error());
        };
        Ok(ResolvedUnicodeProperty::CodePoint(
          ResolvedCodePointProperty::ScriptExtensions(sc),
        ))
      }
    }
  } else {
    // Lone values have `General_Category` value precedence.
    if let Some(gc) = resolve_general_category_value(expr) {
      return Ok(ResolvedUnicodeProperty::CodePoint(
        ResolvedCodePointProperty::GeneralCategory(gc),
      ));
    }

    let Some(name) = resolve_property_name(expr) else {
      return Err(syntax_error());
    };

    match name {
      PropertyName::Binary(bin) => Ok(ResolvedUnicodeProperty::CodePoint(
        ResolvedCodePointProperty::Binary(bin),
      )),
      // Non-binary property names require an explicit value (`Script=Latin` etc).
      PropertyName::NonBinary(_) => Err(syntax_error()),
      PropertyName::String(prop) => {
        if !unicode_sets {
          return Err(syntax_error());
        }
        Ok(ResolvedUnicodeProperty::String(prop))
      }
    }
  }
}

#[inline]
fn resolve_property_name(expr: &str) -> Option<PropertyName> {
  if let Some(non_bin) = resolve_non_binary_property_name(expr) {
    return Some(PropertyName::NonBinary(non_bin));
  }
  if let Some(bin) = resolve_binary_code_point_property_name(expr) {
    return Some(PropertyName::Binary(bin));
  }
  if let Some(sp) = resolve_string_property_name(expr) {
    return Some(PropertyName::String(sp));
  }
  None
}

#[inline]
fn resolve_non_binary_property_name(expr: &str) -> Option<NonBinaryPropertyName> {
  match expr {
    // General_Category
    "General_Category" | "gc" => Some(NonBinaryPropertyName::GeneralCategory),
    // Script
    "Script" | "sc" => Some(NonBinaryPropertyName::Script),
    // Script_Extensions
    "Script_Extensions" | "scx" => Some(NonBinaryPropertyName::ScriptExtensions),
    _ => None,
  }
}

#[inline]
fn resolve_general_category_value(expr: &str) -> Option<GeneralCategory> {
  match expr {
    "Lu" | "Uppercase_Letter" => Some(GeneralCategory::UppercaseLetter),
    _ => None,
  }
}

#[inline]
fn resolve_script_value(expr: &str) -> Option<Script> {
  match expr {
    "Latin" => Some(Script::Latin),
    _ => None,
  }
}

#[inline]
fn resolve_binary_code_point_property_name(expr: &str) -> Option<BinaryCodePointProperty> {
  match expr {
    "ASCII" => Some(BinaryCodePointProperty::ASCII),
    "Assigned" => Some(BinaryCodePointProperty::Assigned),
    _ => None,
  }
}

#[inline]
fn resolve_string_property_name(expr: &str) -> Option<UnicodeStringProperty> {
  match expr {
    "RGI_Emoji" => Some(UnicodeStringProperty::RgiEmoji),
    _ => None,
  }
}

