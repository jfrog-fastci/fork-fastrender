//! Stable storage for compiled JavaScript source + lowered HIR.
//!
//! A user-defined [`crate::JsFunction`] stores a [`CompiledFunctionRef`] in its `[[Call]]` handler.
//! Since `CompiledFunctionRef` contains an `Arc<CompiledScript>`, function objects keep their
//! underlying compiled source/HIR alive even after the original compilation API returns.
//!
//! Note that [`CompiledScript`] lives **outside** the GC heap. To ensure compiled code is included
//! in [`crate::HeapLimits`], compilation charges estimated off-heap bytes via
//! [`crate::Heap::charge_external`].

use crate::heap::ExternalMemoryToken;
use crate::source::SourceText;
use crate::Heap;
use crate::VmError;
use diagnostics::FileId;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::sync::Arc;

/// A compiled JavaScript classic script (source text + lowered HIR).
#[derive(Debug)]
pub struct CompiledScript {
  pub source: SourceText,
  pub hir: Arc<hir_js::LowerResult>,
  #[allow(dead_code)]
  external_memory: ExternalMemoryToken,
}

impl CompiledScript {
  /// Parse and lower a classic script (ECMAScript dialect, `SourceType::Script`).
  pub fn compile_script(
    heap: &mut Heap,
    name: impl Into<Arc<str>>,
    text: impl Into<Arc<str>>,
  ) -> Result<Arc<CompiledScript>, VmError> {
    let source = SourceText::new_charged(heap, name, text)?;
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };

    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      parse_with_options(source.text.as_ref(), opts)
    }))
    .map_err(|_| VmError::InvariantViolation("parse-js panicked while compiling a script"))?
    .map_err(|err| VmError::Syntax(vec![err.to_diagnostic(FileId(0))]))?;

    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a script"))?;
    // HIR can be significantly larger than the source text; use a conservative estimate to ensure
    // heap limits apply to compiled code.
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    Ok(Arc::new(Self {
      source,
      hir: Arc::new(hir),
      external_memory,
    }))
  }
}

/// A reference to a user-defined function body within a [`CompiledScript`].
///
/// This is stored inside `JsFunction` call handlers so closures can outlive the compilation API
/// without holding dangling pointers into ephemeral AST arenas.
#[derive(Debug, Clone)]
pub struct CompiledFunctionRef {
  pub script: Arc<CompiledScript>,
  pub body: hir_js::BodyId,
}
