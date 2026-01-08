#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumericLiteral {
  /// WebIDL `integer` token (e.g. `-1`, `0xFF`).
  Integer(String),
  /// WebIDL `decimal` token (e.g. `3.14`, `6.022e23`).
  Decimal(String),
  Infinity { negative: bool },
  NaN,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultValue {
  Boolean(bool),
  Null,
  Undefined,
  Number(NumericLiteral),
  /// Parsed scalar value of a WebIDL `string` token (unescaped).
  String(String),
  /// The `[]` token.
  EmptySequence,
  /// The `{}` token.
  EmptyDictionary,
}

