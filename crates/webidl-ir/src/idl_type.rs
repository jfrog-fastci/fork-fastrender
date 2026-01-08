#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeAnnotation {
  Clamp,
  EnforceRange,
  LegacyNullToEmptyString,
  LegacyTreatNonObjectAsNull,
  AllowShared,
  AllowResizable,
  Other { name: String, rhs: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NamedTypeKind {
  Unresolved,
  Interface,
  Dictionary,
  Enum,
  Typedef,
  CallbackFunction,
  CallbackInterface,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamedType {
  pub name: String,
  pub kind: NamedTypeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumericType {
  Byte,
  Octet,
  Short,
  UnsignedShort,
  Long,
  UnsignedLong,
  LongLong,
  UnsignedLongLong,
  Float,
  UnrestrictedFloat,
  Double,
  UnrestrictedDouble,
}

impl NumericType {
  pub fn is_integer(self) -> bool {
    matches!(
      self,
      NumericType::Byte
        | NumericType::Octet
        | NumericType::Short
        | NumericType::UnsignedShort
        | NumericType::Long
        | NumericType::UnsignedLong
        | NumericType::LongLong
        | NumericType::UnsignedLongLong
    )
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringType {
  DomString,
  ByteString,
  UsvString,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdlType {
  Any,
  Undefined,
  Boolean,
  Numeric(NumericType),
  BigInt,
  String(StringType),
  Object,
  Symbol,
  Named(NamedType),

  Nullable(Box<IdlType>),
  Union(Vec<IdlType>),

  Sequence(Box<IdlType>),
  FrozenArray(Box<IdlType>),
  AsyncSequence(Box<IdlType>),
  Record(Box<IdlType>, Box<IdlType>),
  Promise(Box<IdlType>),

  Annotated {
    annotations: Vec<TypeAnnotation>,
    inner: Box<IdlType>,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DistinguishabilityCategory {
  Undefined,
  Boolean,
  Numeric,
  BigInt,
  String,
  Object,
  Symbol,
  InterfaceLike,
  CallbackFunction,
  DictionaryLike,
  AsyncSequence,
  SequenceLike,
}

impl IdlType {
  pub fn is_nullable(&self) -> bool {
    matches!(self, IdlType::Nullable(_))
  }

  /// <https://webidl.spec.whatwg.org/#dfn-includes-undefined>
  pub fn includes_undefined(&self) -> bool {
    match self {
      IdlType::Undefined => true,
      IdlType::Nullable(inner) => inner.includes_undefined(),
      IdlType::Annotated { inner, .. } => inner.includes_undefined(),
      IdlType::Union(members) => members.iter().any(|m| m.includes_undefined()),
      _ => false,
    }
  }

  /// <https://webidl.spec.whatwg.org/#dfn-includes-a-nullable-type>
  pub fn includes_nullable_type(&self) -> bool {
    match self {
      IdlType::Nullable(_) => true,
      IdlType::Annotated { inner, .. } => inner.includes_nullable_type(),
      IdlType::Union(members) => number_of_nullable_member_types(members) == 1,
      _ => false,
    }
  }

  /// <https://webidl.spec.whatwg.org/#dfn-flattened-union-member-types>
  pub fn flattened_union_member_types(&self) -> Vec<IdlType> {
    let union = match self.innermost_type() {
      IdlType::Union(members) => members,
      other => return vec![other.clone()],
    };

    let mut out: Vec<IdlType> = Vec::new();
    flattened_union_member_types_into(&mut out, union);
    out
  }

  /// Returns the innermost type after stripping any number of `Annotated` and `Nullable` wrappers.
  ///
  /// This is a convenience for algorithms like "distinguishable" which operate on innermost types.
  pub fn innermost_type(&self) -> &IdlType {
    let mut t = self;
    loop {
      match t {
        IdlType::Annotated { inner, .. } => t = inner,
        IdlType::Nullable(inner) => t = inner,
        _ => return t,
      }
    }
  }

  /// Categorization per the "distinguishable" algorithm's table.
  ///
  /// Returns `None` for types that do not appear in the table (e.g. `any`, `Promise`, unions).
  pub fn category_for_distinguishability(&self) -> Option<DistinguishabilityCategory> {
    match self.innermost_type() {
      IdlType::Undefined => Some(DistinguishabilityCategory::Undefined),
      IdlType::Boolean => Some(DistinguishabilityCategory::Boolean),
      IdlType::Numeric(_) => Some(DistinguishabilityCategory::Numeric),
      IdlType::BigInt => Some(DistinguishabilityCategory::BigInt),
      IdlType::String(_) => Some(DistinguishabilityCategory::String),
      IdlType::Object => Some(DistinguishabilityCategory::Object),
      IdlType::Symbol => Some(DistinguishabilityCategory::Symbol),
      IdlType::Sequence(_) | IdlType::FrozenArray(_) => Some(DistinguishabilityCategory::SequenceLike),
      IdlType::AsyncSequence(_) => Some(DistinguishabilityCategory::AsyncSequence),
      IdlType::Record(_, _) => Some(DistinguishabilityCategory::DictionaryLike),
      IdlType::Named(NamedType { kind, .. }) => match kind {
        NamedTypeKind::Interface => Some(DistinguishabilityCategory::InterfaceLike),
        NamedTypeKind::Dictionary | NamedTypeKind::CallbackInterface => Some(DistinguishabilityCategory::DictionaryLike),
        NamedTypeKind::CallbackFunction => Some(DistinguishabilityCategory::CallbackFunction),
        NamedTypeKind::Enum => Some(DistinguishabilityCategory::String),
        NamedTypeKind::Typedef | NamedTypeKind::Unresolved => None,
      },
      IdlType::Promise(_) => None,
      IdlType::Any | IdlType::Union(_) | IdlType::Nullable(_) | IdlType::Annotated { .. } => None,
    }
  }
}

fn flattened_union_member_types_into(out: &mut Vec<IdlType>, members: &[IdlType]) {
  for m in members {
    let mut u = m;

    if let IdlType::Annotated { inner, .. } = u {
      u = inner;
    }
    if let IdlType::Nullable(inner) = u {
      u = inner;
    }

    match u {
      IdlType::Union(inner_members) => flattened_union_member_types_into(out, inner_members),
      other => {
        if !out.contains(other) {
          out.push(other.clone());
        }
      }
    }
  }
}

fn number_of_nullable_member_types(members: &[IdlType]) -> usize {
  let mut n = 0usize;
  for m in members {
    let mut u = m;

    if let IdlType::Annotated { inner, .. } = u {
      u = inner;
    }

    if let IdlType::Nullable(inner) = u {
      n += 1;
      u = inner;
    }

    if let IdlType::Union(inner_members) = u {
      n += number_of_nullable_member_types(inner_members);
    }
  }
  n
}
