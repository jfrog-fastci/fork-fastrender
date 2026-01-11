use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{FunctionType, PointerType};
use inkwell::values::FunctionValue;
use inkwell::AddressSpace;

use crate::llvm::gc;

/// `native-js` ↔ `runtime-native` ABI boundary helpers.
///
/// # Problem
/// LLVM's statepoint GC support expects *GC references* to be typed as
/// `ptr addrspace(1)`, but our Rust runtime exports C ABI functions using normal
/// pointers (`*mut u8` / `ptr` == addrspace(0)).
///
/// To keep the LLVM IR type system (and GC verifier) happy *and* keep the ABI
/// stable for the runtime, `native-js` declares the runtime entrypoints with
/// addrspace(0) pointers ("raw"), and then emits internal wrapper functions
/// ("*_gc") that `addrspacecast` between addrspaces.
///
/// Generated code should exclusively call the `*_gc` wrappers and never call the
/// raw runtime functions directly with GC pointers.
///
/// # Naming
/// We intentionally keep the runtime's exported symbol names (`rt_*`) as the raw
/// externs, and suffix wrappers with `_gc` (e.g. `rt_alloc_gc`). This avoids any
/// need to rename the Rust runtime ABI.
pub struct RuntimeAbi<'ctx, 'm> {
  context: &'ctx Context,
  module: &'m Module<'ctx>,
  builder: Builder<'ctx>,
}

impl<'ctx, 'm> RuntimeAbi<'ctx, 'm> {
  pub fn new(context: &'ctx Context, module: &'m Module<'ctx>) -> Self {
    Self {
      context,
      module,
      builder: context.create_builder(),
    }
  }

  /// Ensure the common runtime wrappers exist in this LLVM module.
  pub fn ensure_wrappers(&self) -> RuntimeFns<'ctx> {
    RuntimeFns {
      rt_alloc_gc: self.rt_alloc_gc(),
      rt_alloc_pinned_gc: self.rt_alloc_pinned_gc(),
      rt_gc_safepoint_gc: self.rt_gc_safepoint_gc(),
      rt_write_barrier_gc: self.rt_write_barrier_gc(),
    }
  }

  fn ptr_raw(&self) -> PointerType<'ctx> {
    self.context.ptr_type(AddressSpace::default())
  }

  fn ptr_gc(&self) -> PointerType<'ctx> {
    self.context.ptr_type(gc::gc_address_space())
  }

  fn get_or_declare(&self, name: &str, ty: FunctionType<'ctx>) -> FunctionValue<'ctx> {
    if let Some(existing) = self.module.get_function(name) {
      return existing;
    }
    self.module.add_function(name, ty, None)
  }

  fn get_or_define_internal(
    &self,
    name: &str,
    ty: FunctionType<'ctx>,
    define_body: impl FnOnce(FunctionValue<'ctx>),
  ) -> FunctionValue<'ctx> {
    if let Some(existing) = self.module.get_function(name) {
      crate::stack_walking::apply_stack_walking_attrs(self.context, existing);
      if existing.get_first_basic_block().is_some() {
        return existing;
      }
      define_body(existing);
      return existing;
    }

    let func = self.module.add_function(name, ty, Some(Linkage::Internal));
    crate::stack_walking::apply_stack_walking_attrs(self.context, func);
    define_body(func);
    func
  }

  // -----------------------------------------------------------------------------
  // Raw runtime extern declarations (addrspace(0))
  // -----------------------------------------------------------------------------

  fn rt_alloc_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_alloc(size: usize, shape: u128) -> *mut u8`
    let i64_ty = self.context.i64_type();
    let i128_ty = self.context.i128_type();
    let fn_ty = self
      .ptr_raw()
      .fn_type(&[i64_ty.into(), i128_ty.into()], false);
    self.get_or_declare("rt_alloc", fn_ty)
  }

  fn rt_alloc_pinned_raw(&self) -> FunctionValue<'ctx> {
    // `runtime-native` exports:
    //   `rt_alloc_pinned(size: usize, shape: u128) -> *mut u8`
    let i64_ty = self.context.i64_type();
    let i128_ty = self.context.i128_type();
    let fn_ty = self
      .ptr_raw()
      .fn_type(&[i64_ty.into(), i128_ty.into()], false);
    self.get_or_declare("rt_alloc_pinned", fn_ty)
  }

  fn rt_gc_safepoint_raw(&self) -> FunctionValue<'ctx> {
    let fn_ty = self.context.void_type().fn_type(&[], false);
    self.get_or_declare("rt_gc_safepoint", fn_ty)
  }

  fn rt_write_barrier_raw(&self) -> FunctionValue<'ctx> {
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_raw().into(), self.ptr_raw().into()], false);
    self.get_or_declare("rt_write_barrier", fn_ty)
  }

  // -----------------------------------------------------------------------------
  // GC wrappers (addrspace(1))
  // -----------------------------------------------------------------------------

  fn rt_alloc_gc(&self) -> FunctionValue<'ctx> {
    let i64_ty = self.context.i64_type();
    let i128_ty = self.context.i128_type();
    let fn_ty = self
      .ptr_gc()
      .fn_type(&[i64_ty.into(), i128_ty.into()], false);

    self.get_or_define_internal("rt_alloc_gc", fn_ty, |func| {
      let raw = self.rt_alloc_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let size = func.get_nth_param(0).expect("size").into_int_value();
      let shape = func.get_nth_param(1).expect("shape").into_int_value();

      let raw_ptr = self
        .builder
        .build_call(raw, &[size.into(), shape.into()], "raw")
        .expect("call rt_alloc")
        .try_as_basic_value()
        .left()
        .expect("rt_alloc returns ptr")
        .into_pointer_value();

      let gc_ptr = self
        .builder
        .build_address_space_cast(raw_ptr, self.ptr_gc(), "gc_ptr")
        .expect("addrspacecast to gc ptr");

      self
        .builder
        .build_return(Some(&gc_ptr))
        .expect("return gc ptr");
    })
  }

  fn rt_alloc_pinned_gc(&self) -> FunctionValue<'ctx> {
    let i64_ty = self.context.i64_type();
    let i128_ty = self.context.i128_type();
    let fn_ty = self
      .ptr_gc()
      .fn_type(&[i64_ty.into(), i128_ty.into()], false);

    self.get_or_define_internal("rt_alloc_pinned_gc", fn_ty, |func| {
      let raw = self.rt_alloc_pinned_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let size = func.get_nth_param(0).expect("size").into_int_value();
      let shape = func.get_nth_param(1).expect("shape").into_int_value();

      let raw_ptr = self
        .builder
        .build_call(raw, &[size.into(), shape.into()], "raw")
        .expect("call rt_alloc_pinned")
        .try_as_basic_value()
        .left()
        .expect("rt_alloc_pinned returns ptr")
        .into_pointer_value();

      let gc_ptr = self
        .builder
        .build_address_space_cast(raw_ptr, self.ptr_gc(), "gc_ptr")
        .expect("addrspacecast to gc ptr");

      self
        .builder
        .build_return(Some(&gc_ptr))
        .expect("return gc ptr");
    })
  }

  fn rt_gc_safepoint_gc(&self) -> FunctionValue<'ctx> {
    let fn_ty = self.context.void_type().fn_type(&[], false);

    self.get_or_define_internal("rt_gc_safepoint_gc", fn_ty, |func| {
      let raw = self.rt_gc_safepoint_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);
      let _ = self
        .builder
        .build_call(raw, &[], "")
        .expect("call rt_gc_safepoint");
      self.builder.build_return(None).expect("return void");
    })
  }

  fn rt_write_barrier_gc(&self) -> FunctionValue<'ctx> {
    let fn_ty = self
      .context
      .void_type()
      .fn_type(&[self.ptr_gc().into(), self.ptr_gc().into()], false);

    self.get_or_define_internal("rt_write_barrier_gc", fn_ty, |func| {
      let raw = self.rt_write_barrier_raw();
      let entry = self.context.append_basic_block(func, "entry");
      self.builder.position_at_end(entry);

      let obj_gc = func.get_nth_param(0).expect("obj").into_pointer_value();
      let field_gc = func.get_nth_param(1).expect("field").into_pointer_value();

      let obj_raw = self
        .builder
        .build_address_space_cast(obj_gc, self.ptr_raw(), "obj_raw")
        .expect("addrspacecast obj");
      let field_raw = self
        .builder
        .build_address_space_cast(field_gc, self.ptr_raw(), "field_raw")
        .expect("addrspacecast field");

      let _ = self
        .builder
        .build_call(raw, &[obj_raw.into(), field_raw.into()], "")
        .expect("call rt_write_barrier");

      self.builder.build_return(None).expect("return void");
    })
  }
}

/// Handles to the runtime wrapper functions defined in an LLVM module.
#[derive(Clone, Copy)]
pub struct RuntimeFns<'ctx> {
  pub rt_alloc_gc: FunctionValue<'ctx>,
  pub rt_alloc_pinned_gc: FunctionValue<'ctx>,
  pub rt_gc_safepoint_gc: FunctionValue<'ctx>,
  pub rt_write_barrier_gc: FunctionValue<'ctx>,
}
