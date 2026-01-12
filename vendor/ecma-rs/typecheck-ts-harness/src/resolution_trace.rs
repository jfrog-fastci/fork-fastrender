use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolutionTraceMode {
  Classic,
  Node10,
  Node16,
  NodeNext,
  Bundler,
}

impl ResolutionTraceMode {
  pub const fn as_str(&self) -> &'static str {
    match self {
      ResolutionTraceMode::Classic => "classic",
      ResolutionTraceMode::Node10 => "node10",
      ResolutionTraceMode::Node16 => "node16",
      ResolutionTraceMode::NodeNext => "nodenext",
      ResolutionTraceMode::Bundler => "bundler",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolutionTraceKind {
  Import,
  Require,
}

impl ResolutionTraceKind {
  pub const fn as_str(&self) -> &'static str {
    match self {
      ResolutionTraceKind::Import => "import",
      ResolutionTraceKind::Require => "require",
    }
  }
}

/// Structured module resolution trace entry.
///
/// The field names are intentionally stable for diffing across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionTraceEntry {
  pub from: String,
  pub specifier: String,
  pub resolved: Option<String>,
  pub kind: Option<ResolutionTraceKind>,
  pub mode: ResolutionTraceMode,
}

#[derive(Debug, Default)]
struct ResolutionTraceCollectorInner {
  by_from: BTreeMap<String, BTreeMap<String, Vec<ResolutionTraceEvent>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolutionTraceEvent {
  resolved: Option<String>,
  kind: Option<ResolutionTraceKind>,
  mode: ResolutionTraceMode,
}

/// Thread-safe collector for module resolution traces.
///
/// Ordering is deterministic in the serialized output:
/// - group by `from` (sorted),
/// - then `specifier` (sorted),
/// - then insertion order for a specific `(from, specifier)` pair.
#[derive(Debug, Default)]
pub struct ResolutionTraceCollector {
  inner: Mutex<ResolutionTraceCollectorInner>,
}

impl ResolutionTraceCollector {
  pub fn record(
    &self,
    from: impl Into<String>,
    specifier: impl Into<String>,
    resolved: Option<impl Into<String>>,
    kind: Option<ResolutionTraceKind>,
    mode: ResolutionTraceMode,
  ) {
    let mut guard = self.inner.lock().unwrap();
    let by_spec = guard.by_from.entry(from.into()).or_default();
    by_spec
      .entry(specifier.into())
      .or_default()
      .push(ResolutionTraceEvent {
        resolved: resolved.map(Into::into),
        kind,
        mode,
      });
  }

  pub fn snapshot(&self) -> Vec<ResolutionTraceEntry> {
    let guard = self.inner.lock().unwrap();
    let mut out = Vec::new();
    for (from, by_spec) in &guard.by_from {
      for (specifier, events) in by_spec {
        for event in events {
          out.push(ResolutionTraceEntry {
            from: from.clone(),
            specifier: specifier.clone(),
            resolved: event.resolved.clone(),
            kind: event.kind,
            mode: event.mode,
          });
        }
      }
    }
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn collector_snapshot_is_grouped_and_deterministic() {
    let collector = ResolutionTraceCollector::default();
    collector.record(
      "/b.ts",
      "z",
      Some("/z.ts"),
      Some(ResolutionTraceKind::Import),
      ResolutionTraceMode::Node10,
    );
    collector.record(
      "/a.ts",
      "b",
      None::<String>,
      None,
      ResolutionTraceMode::Node10,
    );
    collector.record(
      "/a.ts",
      "a",
      Some("/a1.ts"),
      None,
      ResolutionTraceMode::Node10,
    );
    collector.record(
      "/a.ts",
      "a",
      Some("/a2.ts"),
      None,
      ResolutionTraceMode::Node10,
    );

    assert_eq!(
      collector.snapshot(),
      vec![
        ResolutionTraceEntry {
          from: "/a.ts".to_string(),
          specifier: "a".to_string(),
          resolved: Some("/a1.ts".to_string()),
          kind: None,
          mode: ResolutionTraceMode::Node10,
        },
        ResolutionTraceEntry {
          from: "/a.ts".to_string(),
          specifier: "a".to_string(),
          resolved: Some("/a2.ts".to_string()),
          kind: None,
          mode: ResolutionTraceMode::Node10,
        },
        ResolutionTraceEntry {
          from: "/a.ts".to_string(),
          specifier: "b".to_string(),
          resolved: None,
          kind: None,
          mode: ResolutionTraceMode::Node10,
        },
        ResolutionTraceEntry {
          from: "/b.ts".to_string(),
          specifier: "z".to_string(),
          resolved: Some("/z.ts".to_string()),
          kind: Some(ResolutionTraceKind::Import),
          mode: ResolutionTraceMode::Node10,
        },
      ]
    );
  }
}
