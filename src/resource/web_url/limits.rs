/// Limits controlling allocations and work for WHATWG URL parsing/constructors.
#[derive(Debug, Clone)]
pub struct WebUrlLimits {
  /// Maximum accepted input length (URL string, base string, query string).
  pub max_input_bytes: usize,
  /// Maximum number of name/value pairs in a `URLSearchParams` list.
  pub max_query_pairs: usize,
  /// Maximum total decoded name/value bytes in a `URLSearchParams` list.
  pub max_total_query_bytes: usize,
}

impl Default for WebUrlLimits {
  fn default() -> Self {
    Self {
      // 1 MiB is large enough for real-world URLs while preventing pathological growth.
      max_input_bytes: 1024 * 1024,
      // Query strings rarely contain more than a few dozen pairs; allow plenty while bounding work.
      max_query_pairs: 1024,
      // Decoded bytes across all pairs (not percent-encoded bytes).
      max_total_query_bytes: 1024 * 1024,
    }
  }
}

