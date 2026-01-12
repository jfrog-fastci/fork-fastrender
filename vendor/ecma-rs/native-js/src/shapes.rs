//! Runtime-native shape table generation for `native-js`.
//!
//! `runtime-native` requires a registered table of [`runtime_native_abi::RtShapeDescriptor`]
//! values so the GC can precisely trace pointers within allocated objects.
//!
//! `types-ts-interned` exposes deterministic object-layout computation and GC-trace metadata. This
//! module bridges that metadata into the stable runtime ABI:
//! - mapping `types_ts_interned::ShapeId (u128)` → `runtime_native_abi::RtShapeId (u32)` (1-indexed)
//! - emitting `@__nativejs_shape_table` as an LLVM global constant
//! - emitting `@__nativejs_shape_ptr_offsets_*` arrays for each shape with pointer fields
//!
//! ## Safety invariant
//! The runtime-native ABI currently supports only **flat** pointer maps (a single list of byte
//! offsets). If a shape requires tag-dispatch tracing (e.g. contains a tagged union whose pointer
//! slots differ by variant), codegen must fail hard: misclassifying trace layouts is a memory
//! safety bug.
//!
//! See also: `types-ts-interned/src/gc_trace.rs` and `runtime-native-abi/src/lib.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use diagnostics::{Diagnostic, Span};
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::BasicType as _;
use inkwell::values::GlobalValue;
use inkwell::AddressSpace;
use runtime_native_abi::RtGcPrefix;
use typecheck_ts::Program;
use types_ts_interned as tti;

use crate::codes;

#[derive(Clone, Debug)]
pub struct ShapeInfo {
  /// Semantic shape id (`types-ts-interned`).
  pub shape_id: tti::ShapeId,
  /// Payload layout id (does not include the runtime GC header prefix).
  pub payload_layout: tti::LayoutId,
  /// Byte offset from object base pointer to the start of the payload.
  pub payload_base_offset: u32,
  /// Total object size in bytes, including the GC header prefix.
  pub size: u32,
  /// Object alignment in bytes (power of two), including the GC header requirements.
  pub align: u16,
  /// Byte offsets of GC-managed pointer slots from the object base pointer.
  pub ptr_offsets: Vec<u32>,
}

/// Summary of a shape needed during lowering (allocation/field access).
///
/// This intentionally avoids borrowing from the builder so codegen can keep a local copy without
/// threading lifetimes through the HIR backend.
#[derive(Clone, Copy, Debug)]
pub struct ShapeUse<'ctx> {
  pub shape_id: tti::ShapeId,
  pub payload_layout: tti::LayoutId,
  pub payload_base_offset: u32,
  pub size: u32,
  pub align: u16,
  /// Global constant storing the runtime-local `RtShapeId` (`i32`).
  pub rt_shape_id_global: GlobalValue<'ctx>,
}

#[derive(Clone, Copy, Debug)]
pub struct EmittedShapeTable<'ctx> {
  pub table_global: GlobalValue<'ctx>,
  pub len: usize,
}

#[derive(Debug, Default)]
pub struct ShapeTableBuilder<'ctx> {
  shapes: BTreeMap<tti::ShapeId, ShapeRecord<'ctx>>,
  emitted: Option<EmittedShapeTable<'ctx>>,
}

#[derive(Debug)]
struct ShapeRecord<'ctx> {
  info: ShapeInfo,
  rt_shape_id_global: GlobalValue<'ctx>,
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

impl<'ctx> ShapeTableBuilder<'ctx> {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn emitted(&self) -> Option<EmittedShapeTable<'ctx>> {
    self.emitted
  }

  pub fn len(&self) -> usize {
    self.shapes.len()
  }

  /// Ensure a shape used by the current module is registered in the builder.
  ///
  /// Returns the recorded shape metadata plus the global `i32` storing the runtime-local shape id.
  pub fn ensure_shape_for_type(
    &mut self,
    context: &'ctx Context,
    module: &Module<'ctx>,
    program: &Program,
    ty: tti::TypeId,
    span: Span,
  ) -> Result<ShapeUse<'ctx>, Vec<Diagnostic>> {
    let store = program.interned_type_store();

    let evaluated = program.evaluate_type_interned(ty);
    let kind = program.interned_type_kind(evaluated);
    let tti::TypeKind::Object(obj) = kind else {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "expected an object type with a known shape layout, got {}",
          program.display_type(ty)
        ),
        span,
      )]);
    };
    let shape_id = store.object(obj).shape;

    // Fast path: already known.
    if let Some(existing) = self.shapes.get(&shape_id) {
      return Ok(ShapeUse {
        shape_id,
        payload_layout: existing.info.payload_layout,
        payload_base_offset: existing.info.payload_base_offset,
        size: existing.info.size,
        align: existing.info.align,
        rt_shape_id_global: existing.rt_shape_id_global,
      });
    }

    // Compute the payload layout for the evaluated object type.
    let layout_id = store.layout_of(evaluated);
    let payload_layout = match store.layout(layout_id) {
      tti::Layout::Ptr {
        to: tti::PtrKind::GcObject { layout },
      } => layout,
      other => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          format!(
            "expected object type to lower to a GC-managed object pointer layout, got {other:?}"
          ),
          span,
        )]);
      }
    };

    let info = compute_shape_info(&store, shape_id, payload_layout, span)?;

    // Create a per-shape `i32` constant global that will be filled in once all shapes are known and
    // we assign deterministic 1-indexed `RtShapeId`s.
    //
    // We deliberately keep the mapping stable by deferring assignment until emission time, rather
    // than depending on traversal order.
    let i32_ty = context.i32_type();
    let name = format!("__nativejs_rt_shape_id_{:032x}", shape_id.0);
    let g = module.add_global(i32_ty, None, &name);
    g.set_linkage(Linkage::Internal);
    g.set_constant(true);
    g.set_initializer(&i32_ty.const_zero());

    self.shapes.insert(
      shape_id,
      ShapeRecord {
        info,
        rt_shape_id_global: g,
      },
    );

    let rec = self
      .shapes
      .get(&shape_id)
      .expect("just inserted shape record");
    Ok(ShapeUse {
      shape_id,
      payload_layout: rec.info.payload_layout,
      payload_base_offset: rec.info.payload_base_offset,
      size: rec.info.size,
      align: rec.info.align,
      rt_shape_id_global: rec.rt_shape_id_global,
    })
  }

  /// Emit `@__nativejs_shape_table` and the associated pointer-offset arrays.
  ///
  /// This also finalizes the deterministic `ShapeId -> RtShapeId` mapping by filling in all
  /// `__nativejs_rt_shape_id_*` globals.
  pub fn emit_shape_table(
    &mut self,
    context: &'ctx Context,
    module: &Module<'ctx>,
  ) -> Result<Option<EmittedShapeTable<'ctx>>, Vec<Diagnostic>> {
    if self.emitted.is_some() {
      return Ok(self.emitted);
    }
    if self.shapes.is_empty() {
      self.emitted = None;
      return Ok(None);
    }

    let i32_ty = context.i32_type();
    let i16_ty = context.i16_type();
    let i64_ty = context.i64_type();
    let ptr_ty = context.ptr_type(AddressSpace::default());

    // Mirror `runtime_native_abi::RtShapeDescriptor`:
    // `{ u32, u16, u16, *const u32, u32, u32 }`.
    let rt_shape_desc_ty = context.struct_type(
      &[
        i32_ty.as_basic_type_enum(),
        i16_ty.as_basic_type_enum(),
        i16_ty.as_basic_type_enum(),
        ptr_ty.as_basic_type_enum(),
        i32_ty.as_basic_type_enum(),
        i32_ty.as_basic_type_enum(),
      ],
      false,
    );

    let mut desc_consts = Vec::with_capacity(self.shapes.len());

    for (idx, (_shape_id, rec)) in self.shapes.iter_mut().enumerate() {
      let rt_id_u32 = u32::try_from(idx)
        .ok()
        .and_then(|v| v.checked_add(1))
        .expect("shape table too large for u32 RtShapeId");
      rec
        .rt_shape_id_global
        .set_initializer(&i32_ty.const_int(rt_id_u32 as u64, false));

      let (ptr_offsets_ptr, ptr_offsets_len) = if rec.info.ptr_offsets.is_empty() {
        (ptr_ty.const_null(), 0u32)
      } else {
        let arr_ty = i32_ty.array_type(rec.info.ptr_offsets.len() as u32);
        let name = format!("__nativejs_shape_ptr_offsets_{rt_id_u32}");
        let g = module.add_global(arr_ty, None, &name);
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        let vals: Vec<_> = rec
          .info
          .ptr_offsets
          .iter()
          .map(|o| i32_ty.const_int(*o as u64, false))
          .collect();
        g.set_initializer(&i32_ty.const_array(&vals));
        (g.as_pointer_value(), rec.info.ptr_offsets.len() as u32)
      };

      let desc = rt_shape_desc_ty.const_named_struct(&[
        i32_ty.const_int(rec.info.size as u64, false).into(),
        i16_ty.const_int(rec.info.align as u64, false).into(),
        i16_ty.const_zero().into(), // flags
        ptr_offsets_ptr.into(),
        i32_ty.const_int(ptr_offsets_len as u64, false).into(),
        i32_ty.const_zero().into(), // reserved
      ]);
      desc_consts.push(desc);
    }

    let table_len = desc_consts.len();
    let table_ty = rt_shape_desc_ty.array_type(table_len as u32);
    let table = module.add_global(table_ty, None, "__nativejs_shape_table");
    table.set_linkage(Linkage::Internal);
    table.set_constant(true);
    table.set_initializer(&rt_shape_desc_ty.const_array(&desc_consts));

    // Optional convenience global for debugging / IR assertions.
    let len_global = module.add_global(i64_ty, None, "__nativejs_shape_table_len");
    len_global.set_linkage(Linkage::Internal);
    len_global.set_constant(true);
    len_global.set_initializer(&i64_ty.const_int(table_len as u64, false));

    let out = EmittedShapeTable {
      table_global: table,
      len: table_len,
    };
    self.emitted = Some(out);
    Ok(self.emitted)
  }
}

fn compute_shape_info(
  store: &Arc<tti::TypeStore>,
  shape_id: tti::ShapeId,
  payload_layout: tti::LayoutId,
  span: Span,
) -> Result<ShapeInfo, Vec<Diagnostic>> {
  let payload_layout_data = store.layout(payload_layout);
  let payload_size = payload_layout_data.size();
  let payload_align = payload_layout_data.align();

  let header_size = std::mem::size_of::<RtGcPrefix>() as u32;
  let header_align = std::mem::align_of::<RtGcPrefix>() as u32;

  // `runtime-native` requires `RtShapeDescriptor.align >= align_of::<ObjHeader>()`, so incorporate
  // the header alignment even if the payload itself is only byte-aligned.
  let align_u32 = payload_align.max(header_align);
  let payload_base_offset = align_up(header_size, align_u32);
  let size = align_up(payload_base_offset.saturating_add(payload_size), align_u32);

  let align_u16 = u16::try_from(align_u32).map_err(|_| {
    vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      format!("object alignment {align_u32} does not fit in u16 for RtShapeDescriptor.align"),
      span,
    )]
  })?;
  let size_u32 = size;

  let trace = store.gc_trace_layout(payload_layout);
  let payload_ptr_offsets: Vec<u32> = match trace {
    tti::GcTraceLayout::None => Vec::new(),
    tti::GcTraceLayout::Flat { ptr_offsets } => ptr_offsets,
    other => {
      return Err(vec![codes::HIR_CODEGEN_UNSUPPORTED_SHAPE_GC_TRACE.error(
        format!(
          "object shape requires tag-dispatch GC tracing, which is not supported by the runtime-native shape ABI yet (shape_id={:?}, trace={other:?})",
          shape_id
        ),
        span,
      )]);
    }
  };

  let mut ptr_offsets: Vec<u32> = payload_ptr_offsets
    .into_iter()
    .map(|off| payload_base_offset.saturating_add(off))
    .collect();
  ptr_offsets.sort_unstable();
  ptr_offsets.dedup();

  // Be conservative: validate that offsets stay within the computed object size.
  // This is a codegen-time invariant; the runtime will validate again at registration.
  let ptr_size = runtime_native_abi::RT_PTR_SIZE_BYTES as u32;
  for &off in &ptr_offsets {
    if off < header_size {
      return Err(vec![diagnostics::ice(
        span,
        format!(
          "computed pointer offset {off} is inside the GC header (header_size={header_size})"
        ),
      )]);
    }
    if off.saturating_add(ptr_size) > size_u32 {
      return Err(vec![diagnostics::ice(
        span,
        format!(
          "computed pointer offset {off} out of bounds for shape size {size_u32} (ptr_size={ptr_size})"
        ),
      )]);
    }
    if (off as usize) % runtime_native_abi::RT_PTR_ALIGN_BYTES != 0 {
      return Err(vec![diagnostics::ice(
        span,
        format!(
          "computed pointer offset {off} is not pointer-aligned (align={})",
          runtime_native_abi::RT_PTR_ALIGN_BYTES
        ),
      )]);
    }
  }

  Ok(ShapeInfo {
    shape_id,
    payload_layout,
    payload_base_offset,
    size: size_u32,
    align: align_u16,
    ptr_offsets,
  })
}
