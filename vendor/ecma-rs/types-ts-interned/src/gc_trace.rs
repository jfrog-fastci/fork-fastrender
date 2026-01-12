use crate::layout::{Layout, LayoutId, PtrKind, TagLayout};
use crate::TypeStore;

/// GC tracing metadata derived from a native [`LayoutId`].
///
/// This is intended for AOT/native codegen and provides a deterministic way to
/// derive pointer-slot offsets from the `Layout` tree without re-implementing
/// layout recursion downstream.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GcTraceLayout {
  /// Pointer-free layout (no GC-managed pointers anywhere within the value).
  None,
  /// Layout that can be traced using an unconditional flat pointer-offset list.
  Flat { ptr_offsets: Vec<u32> },
  /// Struct-like layout that contains nested layouts requiring tag dispatch.
  ///
  /// `ptr_offsets` lists unconditional pointer slots within the struct, while
  /// `fields` contains nested layouts (e.g. tagged unions) that must be traced
  /// starting at `field.offset`.
  Struct {
    ptr_offsets: Vec<u32>,
    fields: Vec<FieldTrace>,
  },
  /// Tagged union whose pointer slots depend on the active discriminant value.
  TaggedUnion {
    tag: TagLayout,
    payload_offset: u32,
    variants: Vec<VariantTrace>,
  },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FieldTrace {
  pub offset: u32,
  pub trace: GcTraceLayout,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VariantTrace {
  pub discriminant: u32,
  /// Variant payload offset relative to the union's `payload_offset`.
  pub payload_offset: u32,
  pub trace: GcTraceLayout,
}

impl GcTraceLayout {
  /// Return the pointer offsets if this layout can be traced as a flat list.
  ///
  /// `GcTraceLayout::None` is treated as a valid flat layout with no pointers.
  pub fn as_flat_ptr_offsets(&self) -> Option<&[u32]> {
    match self {
      GcTraceLayout::None => Some(&[]),
      GcTraceLayout::Flat { ptr_offsets } => Some(ptr_offsets.as_slice()),
      _ => None,
    }
  }

  /// Whether tracing this layout requires reading a discriminant tag.
  ///
  /// This returns `true` when tag dispatch is required at any depth (e.g. a
  /// struct containing a tagged union field).
  pub fn requires_tag_dispatch(&self) -> bool {
    match self {
      GcTraceLayout::None | GcTraceLayout::Flat { .. } => false,
      GcTraceLayout::Struct { fields, .. } => {
        fields.iter().any(|f| f.trace.requires_tag_dispatch())
      }
      GcTraceLayout::TaggedUnion { .. } => true,
    }
  }
}

pub(crate) fn contains_gc_ptrs(store: &TypeStore, layout: LayoutId) -> bool {
  match store.layout(layout) {
    Layout::Scalar { .. } => false,
    Layout::Ptr { to } => matches!(
      to,
      PtrKind::GcObject { .. } | PtrKind::GcArray { .. } | PtrKind::GcString | PtrKind::GcAny
    ),
    Layout::Struct { fields, .. } => fields.iter().any(|f| contains_gc_ptrs(store, f.layout)),
    Layout::TaggedUnion { variants, .. } => {
      variants.iter().any(|v| contains_gc_ptrs(store, v.layout))
    }
  }
}

pub(crate) fn gc_trace(store: &TypeStore, layout: LayoutId) -> GcTraceLayout {
  match store.layout(layout) {
    Layout::Scalar { .. } => GcTraceLayout::None,
    Layout::Ptr { to } => match to {
      PtrKind::GcObject { .. } | PtrKind::GcArray { .. } | PtrKind::GcString | PtrKind::GcAny => {
        GcTraceLayout::Flat {
          ptr_offsets: vec![0],
        }
      }
      PtrKind::Opaque => GcTraceLayout::None,
    },
    Layout::Struct { fields, .. } => {
      let mut ptr_offsets: Vec<u32> = Vec::new();
      let mut nested: Vec<FieldTrace> = Vec::new();
      for field in fields {
        let trace = gc_trace(store, field.layout);
        if let Some(child_offsets) = trace.as_flat_ptr_offsets() {
          for &offset in child_offsets {
            ptr_offsets.push(field.offset.saturating_add(offset));
          }
        } else {
          nested.push(FieldTrace {
            offset: field.offset,
            trace,
          });
        }
      }

      ptr_offsets.sort_unstable();
      ptr_offsets.dedup();
      nested.sort_by_key(|f| f.offset);

      if nested.is_empty() {
        if ptr_offsets.is_empty() {
          GcTraceLayout::None
        } else {
          GcTraceLayout::Flat { ptr_offsets }
        }
      } else {
        GcTraceLayout::Struct {
          ptr_offsets,
          fields: nested,
        }
      }
    }
    Layout::TaggedUnion {
      tag,
      payload_offset,
      variants,
      ..
    } => {
      let mut traced_variants: Vec<VariantTrace> = Vec::with_capacity(variants.len());
      for variant in variants {
        traced_variants.push(VariantTrace {
          discriminant: variant.discriminant,
          payload_offset: variant.payload_offset,
          trace: gc_trace(store, variant.layout),
        });
      }

      let Some(first) = traced_variants.first() else {
        return GcTraceLayout::None;
      };

      if traced_variants
        .iter()
        .all(|v| matches!(v.trace, GcTraceLayout::None))
      {
        return GcTraceLayout::None;
      }

      let same_payload_offsets = traced_variants
        .iter()
        .all(|v| v.payload_offset == first.payload_offset);
      if same_payload_offsets {
        if let Some(first_offsets) = first.trace.as_flat_ptr_offsets() {
          let all_flat_and_equal = traced_variants.iter().all(|v| {
            v.trace
              .as_flat_ptr_offsets()
              .is_some_and(|offs| offs == first_offsets)
          });
          if all_flat_and_equal {
            if first_offsets.is_empty() {
              return GcTraceLayout::None;
            }

            let base = payload_offset.saturating_add(first.payload_offset);
            let mut ptr_offsets: Vec<u32> = first_offsets
              .iter()
              .map(|o| base.saturating_add(*o))
              .collect();
            ptr_offsets.sort_unstable();
            ptr_offsets.dedup();
            return GcTraceLayout::Flat { ptr_offsets };
          }
        }
      }

      GcTraceLayout::TaggedUnion {
        tag,
        payload_offset,
        variants: traced_variants,
      }
    }
  }
}
