/// Metadata about runtime functions callable from compiled code.
///
/// In the real system this will be the single registry for runtime entrypoints
/// (Task 315). For now it is minimal but encodes the important property:
/// whether the call may trigger GC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeFn {
  /// Allocation entrypoint: always may trigger GC.
  Alloc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcEffect {
  /// Guaranteed not to trigger GC (leaf, no allocation, no safepoint polls).
  NoGc,
  /// May trigger GC.
  MayGc,
}

impl RuntimeFn {
  pub fn llvm_name(self) -> &'static str {
    match self {
      RuntimeFn::Alloc => "rt_alloc",
    }
  }

  pub fn gc_effect(self) -> GcEffect {
    match self {
      RuntimeFn::Alloc => GcEffect::MayGc,
    }
  }
}

