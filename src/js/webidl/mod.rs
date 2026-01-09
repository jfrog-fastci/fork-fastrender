//! Web IDL scaffolding for JS bindings.
//!
//! FastRender uses the `engines/ecma-rs/webidl` crate as the authoritative source of:
//! - WebIDL conversion helpers (e.g. `DOMString`)
//! - the `JsRuntime` trait boundary used by those helpers
//!
//! This module re-exports that API surface as `fastrender::js::webidl` so generated bindings can
//! depend on a single in-crate path without duplicating conversion logic.

pub use webidl::*;
pub use webidl_vm_js::VmJsWebIdlCx;

// FastRender-specific VM/runtime scaffolding used by early generated bindings and host shims.
pub use webidl_js_runtime::{NativeHostFunction, VmJsRuntime, WebIdlBindingsRuntime};

#[cfg(test)]
mod tests {
  use super::{conversions, InterfaceId, VmJsWebIdlCx, WebIdlHooks, WebIdlLimits};
  use vm_js::{Heap, HeapLimits, Value, Vm, VmOptions};

  struct NoHooks;

  impl WebIdlHooks<Value> for NoHooks {
    fn is_platform_object(&self, _value: Value) -> bool {
      false
    }

    fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
      false
    }
  }

  #[test]
  fn domstring_conversion_roundtrips_code_units_via_ecma_webidl() -> Result<(), vm_js::VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let units: Vec<u16> = vec![0x0041, 0xD83D, 0xDE00, 0x0000, 0xFFFF];
    let s = {
      let mut scope = heap.scope();
      scope
        .alloc_string_from_code_units(&units)
        .expect("alloc string")
    };
    let _root = heap.add_root(Value::String(s)).expect("add_root");

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let out = conversions::dom_string(&mut cx, Value::String(s)).expect("DOMString conversion");
    drop(cx);

    let out_units = heap.get_string(out).expect("get string").as_code_units();
    assert_eq!(out_units, units.as_slice());
    Ok(())
  }
}
