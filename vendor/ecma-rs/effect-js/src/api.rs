/// Stable identifier for a known JavaScript/TypeScript API surface.
///
/// This is a re-export of [`knowledge_base::ApiId`], which is a stable 64-bit
/// FNV-1a hash of a knowledge-base canonical name (e.g. `"JSON.parse"`).
pub use knowledge_base::ApiId;
