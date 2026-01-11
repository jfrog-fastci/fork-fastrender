use semver::Version;
use std::ops::Bound;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange {
  pub lower: Bound<Version>,
  pub upper: Bound<Version>,
}

impl VersionRange {
  pub fn any() -> Self {
    Self {
      lower: Bound::Unbounded,
      upper: Bound::Unbounded,
    }
  }

  pub fn contains(&self, version: &Version) -> bool {
    if !match &self.lower {
      Bound::Unbounded => true,
      Bound::Included(v) => version >= v,
      Bound::Excluded(v) => version > v,
    } {
      return false;
    }

    match &self.upper {
      Bound::Unbounded => true,
      Bound::Included(v) => version <= v,
      Bound::Excluded(v) => version < v,
    }
  }

  pub fn overlaps(&self, other: &Self) -> bool {
    // Compute the intersection [max(lower), min(upper)] and check if it's non-empty.
    let lower = max_lower(&self.lower, &other.lower);
    let upper = min_upper(&self.upper, &other.upper);

    match (&lower, &upper) {
      (_, Bound::Unbounded) => true,
      (Bound::Unbounded, _) => true,
      (Bound::Included(l), Bound::Included(u)) => l <= u,
      (Bound::Included(l), Bound::Excluded(u))
      | (Bound::Excluded(l), Bound::Included(u))
      | (Bound::Excluded(l), Bound::Excluded(u)) => l < u,
    }
  }

  pub fn since(&self) -> Option<&Version> {
    match &self.lower {
      Bound::Unbounded => None,
      Bound::Included(v) | Bound::Excluded(v) => Some(v),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionRangeSpec {
  Parsed(VersionRange),
  /// Could not be parsed into semver constraints; only matches under `TargetEnv::Unknown`.
  Unparsed { since: Option<String>, until: Option<String> },
}

impl VersionRangeSpec {
  pub fn from_since_until(since: Option<&str>, until: Option<&str>) -> Self {
    let parse_result = (|| {
      let lower = match since {
        None => Bound::Unbounded,
        Some(raw) => parse_bound(raw, FieldKind::Since)?,
      };
      let upper = match until {
        None => Bound::Unbounded,
        Some(raw) => parse_bound(raw, FieldKind::Until)?,
      };
      Ok::<_, ()>(VersionRange { lower, upper })
    })();

    match parse_result {
      Ok(range) => VersionRangeSpec::Parsed(range),
      Err(()) => VersionRangeSpec::Unparsed {
        since: since.map(|s| s.to_string()),
        until: until.map(|s| s.to_string()),
      },
    }
  }

  pub fn parsed(&self) -> Option<&VersionRange> {
    match self {
      VersionRangeSpec::Parsed(r) => Some(r),
      VersionRangeSpec::Unparsed { .. } => None,
    }
  }

  pub fn is_unparsed(&self) -> bool {
    matches!(self, VersionRangeSpec::Unparsed { .. })
  }

  pub fn display(&self) -> String {
    match self {
      VersionRangeSpec::Parsed(r) => {
        let lower = display_bound(&r.lower, BoundSide::Lower);
        let upper = display_bound(&r.upper, BoundSide::Upper);
        match (lower.as_str(), upper.as_str()) {
          ("*", "*") => "*".to_string(),
          ("*", _) => upper,
          (_, "*") => lower,
          _ => format!("{lower} {upper}"),
        }
      }
      VersionRangeSpec::Unparsed { since, until } => {
        let mut parts = Vec::new();
        if let Some(s) = since {
          parts.push(format!("since={s:?}"));
        }
        if let Some(u) = until {
          parts.push(format!("until={u:?}"));
        }
        if parts.is_empty() {
          "<unparsed>".to_string()
        } else {
          parts.join(" ")
        }
      }
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum FieldKind {
  Since,
  Until,
}

fn parse_bound(raw: &str, kind: FieldKind) -> Result<Bound<Version>, ()> {
  let raw = raw.trim();
  if raw.is_empty() {
    return Err(());
  }

  let (op, rest) = if let Some(rest) = raw.strip_prefix(">=") {
    (">=", rest)
  } else if let Some(rest) = raw.strip_prefix("<=") {
    ("<=", rest)
  } else if let Some(rest) = raw.strip_prefix('>') {
    (">", rest)
  } else if let Some(rest) = raw.strip_prefix('<') {
    ("<", rest)
  } else if let Some(rest) = raw.strip_prefix('=') {
    ("=", rest)
  } else {
    ("", raw)
  };

  let rest = rest.trim();

  // Web platform KB entries often use `since: "baseline"` as a human-readable
  // availability marker. Treat this as "no version constraint" so that web
  // entries can participate in `TargetEnv::Web` filtering and env-specific
  // overrides without needing semver.
  if rest.eq_ignore_ascii_case("baseline") {
    return Ok(Bound::Unbounded);
  }

  let version = parse_lenient_version(rest).ok_or(())?;

  match kind {
    FieldKind::Since => match op {
      "" | ">=" | "=" => Ok(Bound::Included(version)),
      ">" => Ok(Bound::Excluded(version)),
      _ => Err(()),
    },
    FieldKind::Until => match op {
      "" | "<" => Ok(Bound::Excluded(version)),
      "<=" | "=" => Ok(Bound::Included(version)),
      _ => Err(()),
    },
  }
}

fn parse_lenient_version(raw: &str) -> Option<Version> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }

  let raw = raw.strip_prefix('v').unwrap_or(raw);

  if let Ok(v) = Version::parse(raw) {
    return Some(v);
  }

  let mut it = raw.split('.');
  let major_str = it.next()?;
  let minor_str = it.next();
  let patch_str = it.next();
  if it.next().is_some() {
    return None;
  }

  let major = major_str.parse::<u64>().ok()?;
  let minor = minor_str.map(|s| s.parse::<u64>().ok()).unwrap_or(Some(0))?;
  let patch = patch_str.map(|s| s.parse::<u64>().ok()).unwrap_or(Some(0))?;
  Some(Version::new(major, minor, patch))
}

fn max_lower(a: &Bound<Version>, b: &Bound<Version>) -> Bound<Version> {
  match (a, b) {
    (Bound::Unbounded, x) | (x, Bound::Unbounded) => x.clone(),
    (Bound::Included(av), Bound::Included(bv)) => {
      if av >= bv {
        Bound::Included(av.clone())
      } else {
        Bound::Included(bv.clone())
      }
    }
    (Bound::Excluded(av), Bound::Excluded(bv)) => {
      if av >= bv {
        Bound::Excluded(av.clone())
      } else {
        Bound::Excluded(bv.clone())
      }
    }
    (Bound::Included(av), Bound::Excluded(bv)) => match av.cmp(bv) {
      std::cmp::Ordering::Less => Bound::Excluded(bv.clone()),
      std::cmp::Ordering::Equal => Bound::Excluded(av.clone()),
      std::cmp::Ordering::Greater => Bound::Included(av.clone()),
    },
    (Bound::Excluded(av), Bound::Included(bv)) => match av.cmp(bv) {
      std::cmp::Ordering::Less => Bound::Included(bv.clone()),
      std::cmp::Ordering::Equal => Bound::Excluded(av.clone()),
      std::cmp::Ordering::Greater => Bound::Excluded(av.clone()),
    },
  }
}

fn min_upper(a: &Bound<Version>, b: &Bound<Version>) -> Bound<Version> {
  match (a, b) {
    (Bound::Unbounded, x) | (x, Bound::Unbounded) => x.clone(),
    (Bound::Included(av), Bound::Included(bv)) => {
      if av <= bv {
        Bound::Included(av.clone())
      } else {
        Bound::Included(bv.clone())
      }
    }
    (Bound::Excluded(av), Bound::Excluded(bv)) => {
      if av <= bv {
        Bound::Excluded(av.clone())
      } else {
        Bound::Excluded(bv.clone())
      }
    }
    (Bound::Included(av), Bound::Excluded(bv)) => match av.cmp(bv) {
      std::cmp::Ordering::Less => Bound::Included(av.clone()),
      std::cmp::Ordering::Equal => Bound::Excluded(av.clone()),
      std::cmp::Ordering::Greater => Bound::Excluded(bv.clone()),
    },
    (Bound::Excluded(av), Bound::Included(bv)) => match av.cmp(bv) {
      std::cmp::Ordering::Less => Bound::Excluded(av.clone()),
      std::cmp::Ordering::Equal => Bound::Excluded(av.clone()),
      std::cmp::Ordering::Greater => Bound::Included(bv.clone()),
    },
  }
}

#[derive(Debug, Clone, Copy)]
enum BoundSide {
  Lower,
  Upper,
}

fn display_bound(bound: &Bound<Version>, side: BoundSide) -> String {
  match (side, bound) {
    (_, Bound::Unbounded) => "*".to_string(),
    (BoundSide::Lower, Bound::Included(v)) => format!(">={v}"),
    (BoundSide::Lower, Bound::Excluded(v)) => format!(">{v}"),
    (BoundSide::Upper, Bound::Included(v)) => format!("<={v}"),
    (BoundSide::Upper, Bound::Excluded(v)) => format!("<{v}"),
  }
}
