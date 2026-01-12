use crate::shape::Shape;
use crate::{PropKey, ShapeId, TupleElem, TypeId, TypeKind, TypeStore};
use ahash::RandomState;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;

const HASH_KEY1: u64 = 0x9e37_79b9_7f4a_7c15;
const HASH_KEY2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const HASH_KEY3: u64 = 0x1656_67b1_9e37_79f9;
const HASH_KEY4: u64 = 0x85eb_ca6b_c8f6_9b07;

const LAYOUT_DOMAIN: u64 = 0x6c61_796f; // "layo"

const PTR_SIZE: u32 = 8;
const PTR_ALIGN: u32 = 8;

fn stable_state(domain: u64) -> RandomState {
  RandomState::with_seeds(
    HASH_KEY1 ^ domain,
    HASH_KEY2.wrapping_add(domain),
    HASH_KEY3 ^ (domain << 1),
    HASH_KEY4.wrapping_sub(domain),
  )
}

fn stable_hash64<T: Hash>(value: &T, domain: u64, salt: u64) -> u64 {
  let mut hasher = stable_state(domain).build_hasher();
  hasher.write_u64(salt);
  value.hash(&mut hasher);
  hasher.finish()
}

fn fingerprint<T: Hash>(value: &T, domain: u64, salt: u64) -> u128 {
  let base_salt = salt.wrapping_mul(2);
  let primary = stable_hash64(value, domain, base_salt);
  let secondary = stable_hash64(value, domain, base_salt.wrapping_add(1));
  ((primary as u128) << 64) | secondary as u128
}

fn align_up(offset: u32, align: u32) -> u32 {
  debug_assert!(align != 0);
  let rem = offset % align;
  if rem == 0 {
    offset
  } else {
    offset + (align - rem)
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LayoutId(pub u128);

impl From<u128> for LayoutId {
  fn from(value: u128) -> Self {
    Self(value)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AbiScalar {
  Bool,
  I32,
  I64,
  U8,
  U16,
  U32,
  U64,
  F64,
}

impl AbiScalar {
  pub fn size(self) -> u32 {
    match self {
      AbiScalar::Bool => 1,
      AbiScalar::U8 => 1,
      AbiScalar::U16 => 2,
      AbiScalar::I32 | AbiScalar::U32 => 4,
      AbiScalar::I64 | AbiScalar::U64 | AbiScalar::F64 => 8,
    }
  }

  pub fn align(self) -> u32 {
    self.size()
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PtrKind {
  /// Heap-managed object with a known structural layout.
  GcObject { layout: LayoutId },
  /// Heap-managed array header. The element layout is retained to keep the
  /// pointer "typed" even though the ABI matches all pointers.
  GcArray { elem: LayoutId },
  /// Heap-managed string.
  GcString,
  /// Heap-managed pointer whose pointee layout is not known at compile time.
  ///
  /// This is still a GC-managed pointer (i.e. a precise GC root slot) but does
  /// not encode a layout identity.
  GcAny,
  /// Opaque pointer-like value.
  Opaque,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FieldKey {
  TupleIndex(u32),
  Prop(PropKey),
  /// Internal field used by the native layout engine.
  ///
  /// This is stored as an owned string so it can round-trip through serde
  /// snapshots/JSON output (borrowed `&'static str` cannot be deserialized).
  Internal(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FieldLayout {
  pub key: FieldKey,
  pub offset: u32,
  pub size: u32,
  pub align: u32,
  pub layout: LayoutId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TagLayout {
  pub abi: AbiScalar,
  pub offset: u32,
}

impl TagLayout {
  pub fn size(&self) -> u32 {
    self.abi.size()
  }

  pub fn align(&self) -> u32 {
    self.abi.align()
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VariantLayout {
  pub ty: TypeId,
  pub layout: LayoutId,
  pub discriminant: u32,
  pub payload_offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Layout {
  Scalar { abi: AbiScalar },
  Ptr { to: PtrKind },
  Struct {
    fields: Vec<FieldLayout>,
    size: u32,
    align: u32,
  },
  TaggedUnion {
    tag: TagLayout,
    payload_offset: u32,
    variants: Vec<VariantLayout>,
    size: u32,
    align: u32,
  },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GcTraceVariant {
  pub discriminant: u32,
  pub trace: Vec<GcTraceStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GcTraceStep {
  /// A GC-managed pointer at the given byte offset (relative to the value base).
  Ptr { offset: u32 },
  /// A tagged union that requires inspecting its tag before tracing the active
  /// payload variant.
  TaggedUnion { tag: TagLayout, variants: Vec<GcTraceVariant> },
}

impl Layout {
  pub fn size(&self) -> u32 {
    match self {
      Layout::Scalar { abi } => abi.size(),
      Layout::Ptr { .. } => PTR_SIZE,
      Layout::Struct { size, .. } | Layout::TaggedUnion { size, .. } => *size,
    }
  }

  pub fn align(&self) -> u32 {
    match self {
      Layout::Scalar { abi } => abi.align(),
      Layout::Ptr { .. } => PTR_ALIGN,
      Layout::Struct { align, .. } | Layout::TaggedUnion { align, .. } => *align,
    }
  }
}

#[derive(Debug, Default)]
pub(crate) struct LayoutStore {
  by_type: DashMap<TypeId, LayoutId>,
  by_shape: DashMap<ShapeId, LayoutId>,
  layouts: DashMap<LayoutId, Layout>,
}

impl LayoutStore {
  pub(crate) fn layout(&self, id: LayoutId) -> Layout {
    self
      .layouts
      .get(&id)
      .map(|entry| entry.value().clone())
      .expect("LayoutId not interned")
  }

  fn intern_layout(&self, layout: Layout) -> LayoutId {
    #[cfg(feature = "strict-determinism")]
    {
      let salt = 0u64;
      let id = LayoutId(fingerprint(&layout, LAYOUT_DOMAIN, salt));
      match self.layouts.entry(id) {
        Entry::Occupied(entry) => {
          if entry.get() == &layout {
            return id;
          }
          let next_id = LayoutId(fingerprint(&layout, LAYOUT_DOMAIN, salt.wrapping_add(1)));
          panic!("strict-determinism: layout ID collision for {id:?} (next candidate: {next_id:?})");
        }
        Entry::Vacant(entry) => {
          entry.insert(layout);
          id
        }
      }
    }

    #[cfg(not(feature = "strict-determinism"))]
    {
      let mut salt = 0u64;
      loop {
        let id = LayoutId(fingerprint(&layout, LAYOUT_DOMAIN, salt));
        match self.layouts.entry(id) {
          Entry::Occupied(entry) => {
            if entry.get() == &layout {
              return id;
            }
            salt = salt.wrapping_add(1);
          }
          Entry::Vacant(entry) => {
            entry.insert(layout);
            return id;
          }
        }
      }
    }
  }

  pub(crate) fn layout_of_type(&self, store: &TypeStore, ty: TypeId) -> LayoutId {
    if let Some(id) = self.by_type.get(&ty) {
      return *id;
    }

    let layout = match store.type_kind(ty) {
      TypeKind::Boolean | TypeKind::BooleanLiteral(_) => Layout::Scalar { abi: AbiScalar::Bool },
      TypeKind::Number | TypeKind::NumberLiteral(_) => Layout::Scalar { abi: AbiScalar::F64 },
      TypeKind::String | TypeKind::StringLiteral(_) | TypeKind::TemplateLiteral(_) => {
        Layout::Ptr { to: PtrKind::GcString }
      }
      TypeKind::Null | TypeKind::Undefined | TypeKind::Void => Layout::Scalar { abi: AbiScalar::U8 },
      TypeKind::Tuple(elems) => self.layout_tuple(store, &elems),
      TypeKind::Array { ty, .. } => {
        let elem = self.layout_of_type(store, ty);
        Layout::Ptr {
          to: PtrKind::GcArray { elem },
        }
      }
      TypeKind::Union(members) => self.layout_union(store, &members),
      TypeKind::Object(obj) => {
        let shape = store.object(obj).shape;
        let payload = self.layout_of_shape(store, shape);
        Layout::Ptr {
          to: PtrKind::GcObject { layout: payload },
        }
      }
      TypeKind::Callable { .. } => {
        let payload = self.canonical_closure_payload_layout();
        Layout::Ptr {
          to: PtrKind::GcObject { layout: payload },
        }
      }
      // Placeholder until native backend decides on a canonical JSValue ABI.
      _ => Layout::Ptr { to: PtrKind::Opaque },
    };

    let id = self.intern_layout(layout);
    self.by_type.insert(ty, id);
    id
  }

  pub(crate) fn gc_trace(&self, layout: LayoutId) -> Vec<GcTraceStep> {
    fn is_gc_ptr_kind(kind: &PtrKind) -> bool {
      matches!(
        kind,
        PtrKind::GcObject { .. } | PtrKind::GcArray { .. } | PtrKind::GcString | PtrKind::GcAny
      )
    }

    fn shift(trace: &[GcTraceStep], delta: u32) -> Vec<GcTraceStep> {
      trace
        .iter()
        .cloned()
        .map(|step| match step {
          GcTraceStep::Ptr { offset } => GcTraceStep::Ptr {
            offset: offset.saturating_add(delta),
          },
          GcTraceStep::TaggedUnion { tag, variants } => GcTraceStep::TaggedUnion {
            tag: TagLayout {
              abi: tag.abi,
              offset: tag.offset.saturating_add(delta),
            },
            variants: variants
              .into_iter()
              .map(|variant| GcTraceVariant {
                discriminant: variant.discriminant,
                trace: shift(&variant.trace, delta),
              })
              .collect(),
          },
        })
        .collect()
    }

    fn trace_layout(store: &LayoutStore, layout: LayoutId) -> Vec<GcTraceStep> {
      match store.layout(layout) {
        Layout::Scalar { .. } => Vec::new(),
        Layout::Ptr { to } => {
          if is_gc_ptr_kind(&to) {
            vec![GcTraceStep::Ptr { offset: 0 }]
          } else {
            Vec::new()
          }
        }
        Layout::Struct { fields, .. } => {
          let mut out = Vec::new();
          for field in fields {
            let nested = trace_layout(store, field.layout);
            out.extend(shift(&nested, field.offset));
          }
          out
        }
        Layout::TaggedUnion {
          tag,
          payload_offset,
          variants,
          ..
        } => {
          let variants = variants
            .into_iter()
            .map(|variant| GcTraceVariant {
              discriminant: variant.discriminant,
              trace: shift(&trace_layout(store, variant.layout), payload_offset),
            })
            .collect();
          vec![GcTraceStep::TaggedUnion { tag, variants }]
        }
      }
    }

    trace_layout(self, layout)
  }

  pub(crate) fn gc_ptr_offsets(&self, layout: LayoutId) -> Vec<u32> {
    fn is_gc_ptr_kind(kind: &PtrKind) -> bool {
      matches!(
        kind,
        PtrKind::GcObject { .. } | PtrKind::GcArray { .. } | PtrKind::GcString | PtrKind::GcAny
      )
    }

    fn shift(offsets: &std::collections::BTreeSet<u32>, delta: u32) -> std::collections::BTreeSet<u32> {
      offsets
        .iter()
        .map(|offset| offset.saturating_add(delta))
        .collect()
    }

    fn collect_unconditional(store: &LayoutStore, layout: LayoutId) -> std::collections::BTreeSet<u32> {
      match store.layout(layout) {
        Layout::Scalar { .. } => Default::default(),
        Layout::Ptr { to } => {
          if is_gc_ptr_kind(&to) {
            std::collections::BTreeSet::from([0])
          } else {
            Default::default()
          }
        }
        Layout::Struct { fields, .. } => {
          let mut out: std::collections::BTreeSet<u32> = Default::default();
          for field in fields {
            for offset in shift(&collect_unconditional(store, field.layout), field.offset) {
              out.insert(offset);
            }
          }
          out
        }
        Layout::TaggedUnion {
          payload_offset,
          variants,
          ..
        } => {
          // A tagged union only contains an *unconditional* GC pointer if it is
          // present in every variant at the same offset.
          let mut it = variants.into_iter();
          let Some(first) = it.next() else {
            return Default::default();
          };

          let mut common = shift(&collect_unconditional(store, first.layout), payload_offset);
          for variant in it {
            let offsets = shift(&collect_unconditional(store, variant.layout), payload_offset);
            common = common.intersection(&offsets).copied().collect();
            if common.is_empty() {
              break;
            }
          }
          common
        }
      }
    }

    collect_unconditional(self, layout).into_iter().collect()
  }

  fn canonical_closure_payload_layout(&self) -> LayoutId {
    let fn_ptr = self.intern_layout(Layout::Ptr { to: PtrKind::Opaque });
    let env = self.intern_layout(Layout::Ptr { to: PtrKind::GcAny });

    let fields = vec![
      FieldLayout {
        key: FieldKey::Internal("fn_ptr".to_string()),
        offset: 0,
        size: PTR_SIZE,
        align: PTR_ALIGN,
        layout: fn_ptr,
      },
      FieldLayout {
        key: FieldKey::Internal("env".to_string()),
        offset: PTR_SIZE,
        size: PTR_SIZE,
        align: PTR_ALIGN,
        layout: env,
      },
    ];
    let size = PTR_SIZE * 2;
    let align = PTR_ALIGN;
    self.intern_layout(Layout::Struct { fields, size, align })
  }

  fn layout_tuple(&self, store: &TypeStore, elems: &[TupleElem]) -> Layout {
    let mut offset: u32 = 0;
    let mut align: u32 = 1;
    let mut fields = Vec::with_capacity(elems.len());
    for (idx, elem) in elems.iter().enumerate() {
      let child = self.layout_of_type(store, elem.ty);
      let child_layout = self.layout(child);
      let field_align = child_layout.align();
      let field_size = child_layout.size();
      offset = align_up(offset, field_align);
      fields.push(FieldLayout {
        key: FieldKey::TupleIndex(idx as u32),
        offset,
        size: field_size,
        align: field_align,
        layout: child,
      });
      offset = offset.saturating_add(field_size);
      align = align.max(field_align);
    }
    let size = align_up(offset, align);
    Layout::Struct { fields, size, align }
  }

  fn tag_abi(variant_count: usize) -> AbiScalar {
    if variant_count <= u8::MAX as usize {
      AbiScalar::U8
    } else if variant_count <= u16::MAX as usize {
      AbiScalar::U16
    } else {
      AbiScalar::U32
    }
  }

  fn layout_union(&self, store: &TypeStore, members: &[TypeId]) -> Layout {
    debug_assert!(members.len() > 1);
    let mut members: Vec<TypeId> = members.to_vec();
    members.sort_by(|a, b| store.type_cmp(*a, *b));
    members.dedup();

    let mut variant_layouts = Vec::with_capacity(members.len());
    let mut payload_size: u32 = 0;
    let mut payload_align: u32 = 1;
    for ty in &members {
      let layout = self.layout_of_type(store, *ty);
      let l = self.layout(layout);
      payload_size = payload_size.max(l.size());
      payload_align = payload_align.max(l.align());
      variant_layouts.push((*ty, layout));
    }

    let tag = TagLayout {
      abi: Self::tag_abi(variant_layouts.len()),
      offset: 0,
    };
    let payload_offset = align_up(tag.size(), payload_align);
    let align = tag.align().max(payload_align);
    let size = align_up(payload_offset.saturating_add(payload_size), align);

    let variants = variant_layouts
      .into_iter()
      .enumerate()
      .map(|(idx, (ty, layout))| VariantLayout {
        ty,
        layout,
        discriminant: idx as u32,
        payload_offset,
      })
      .collect();

    Layout::TaggedUnion {
      tag,
      payload_offset,
      variants,
      size,
      align,
    }
  }

  fn layout_of_shape(&self, store: &TypeStore, shape_id: ShapeId) -> LayoutId {
    if let Some(id) = self.by_shape.get(&shape_id) {
      return *id;
    }

    let Shape { mut properties, .. } = store.shape(shape_id);
    properties.sort_by(|a, b| store.compare_prop_keys(&a.key, &b.key));

    let mut offset: u32 = 0;
    let mut align: u32 = 1;
    let mut fields = Vec::with_capacity(properties.len());

    for prop in properties {
      let child = self.layout_of_type(store, prop.data.ty);
      let child_layout = self.layout(child);
      let field_align = child_layout.align();
      let field_size = child_layout.size();
      offset = align_up(offset, field_align);
      fields.push(FieldLayout {
        key: FieldKey::Prop(prop.key),
        offset,
        size: field_size,
        align: field_align,
        layout: child,
      });
      offset = offset.saturating_add(field_size);
      align = align.max(field_align);
    }

    let size = align_up(offset, align);
    let layout = Layout::Struct { fields, size, align };
    let id = self.intern_layout(layout);
    self.by_shape.insert(shape_id, id);
    id
  }
}

/// Convenience wrapper for building and querying layouts without requiring a
/// `TypeStore` method call chain in downstream crates.
#[derive(Debug, Clone)]
pub struct LayoutComputer {
  store: Arc<TypeStore>,
}

impl LayoutComputer {
  pub fn new(store: Arc<TypeStore>) -> Self {
    Self { store }
  }

  pub fn store(&self) -> &TypeStore {
    &self.store
  }

  pub fn layout_of(&self, ty: TypeId) -> LayoutId {
    self.store.layout_of(ty)
  }

  pub fn layout_of_evaluated<E: crate::TypeExpander>(&self, ty: TypeId, expander: &E) -> LayoutId {
    self.store.layout_of_evaluated(ty, expander)
  }

  pub fn layout(&self, id: LayoutId) -> Layout {
    self.store.layout(id)
  }

  pub fn gc_trace(&self, layout: LayoutId) -> Vec<GcTraceStep> {
    self.store.gc_trace(layout)
  }

  pub fn gc_ptr_offsets(&self, layout: LayoutId) -> Vec<u32> {
    self.store.gc_ptr_offsets(layout)
  }
}
