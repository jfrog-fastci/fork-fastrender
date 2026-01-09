/// Limits controlling allocations and work for WHATWG Fetch core types.
///
/// These limits are intended for hostile input (e.g. JavaScript bindings) to ensure the Rust-side
/// Fetch primitives cannot allocate unbounded memory.
#[derive(Debug, Clone)]
pub struct WebFetchLimits {
  /// Maximum accepted byte length for URL strings consumed by Fetch.
  ///
  /// This applies to request URLs provided by callers as well as intermediate URL strings produced
  /// while resolving/canonicalizing URLs (e.g. relative resolution against a base URL).
  pub max_url_bytes: usize,
  /// Maximum number of headers in a `Headers` "header list" (including duplicates).
  pub max_header_count: usize,
  /// Maximum total bytes across all header names and values in a `Headers` "header list".
  ///
  /// This is the sum of `name.len() + value.len()` for each header entry in the list.
  pub max_total_header_bytes: usize,
  /// Maximum size of a request body in bytes.
  pub max_request_body_bytes: usize,
  /// Maximum size of a response body in bytes.
  ///
  /// Note: HTTP fetchers typically enforce response size via [`crate::resource::ResourcePolicy`],
  /// but this limit still matters for non-HTTP backends and for adapter-level enforcement.
  pub max_response_body_bytes: usize,
}

impl Default for WebFetchLimits {
  fn default() -> Self {
    Self {
      // Match `WebUrlLimits::max_input_bytes` so URL parsing/normalization stays bounded.
      max_url_bytes: 1024 * 1024,
      // Generous enough for real-world requests while preventing unbounded growth from hostile JS.
      max_header_count: 1024,
      // Roughly matches typical HTTP header limits (and the curl backend's block cap).
      max_total_header_bytes: 256 * 1024,
      // Request bodies are attacker-controlled when exposed to JS. Keep this conservative.
      max_request_body_bytes: 10 * 1024 * 1024,
      // Match the default `ResourcePolicy::max_response_bytes` to keep behavior consistent.
      max_response_body_bytes: 50 * 1024 * 1024,
    }
  }
}
