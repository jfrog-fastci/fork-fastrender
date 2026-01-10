use vm_js::import_attributes_from_options;
use vm_js::ImportCallError;
use vm_js::PropertyDescriptor;
use vm_js::PropertyKey;
use vm_js::PropertyKind;
use vm_js::TerminationReason;
use vm_js::Value;
use vm_js::Vm;
use vm_js::VmError;
use vm_js::VmOptions;
use vm_js::{Heap, HeapLimits};

fn data_desc(value: Value, enumerable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

#[test]
fn import_attributes_from_options_consumes_fuel() {
  let mut heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 16 * 1024 * 1024));
  let mut scope = heap.scope();
  let mut vm = Vm::new(VmOptions {
    default_fuel: Some(50),
    check_time_every: 1,
    ..VmOptions::default()
  });

  let options = scope.alloc_object().unwrap();
  let attributes = scope.alloc_object().unwrap();

  let value = scope.alloc_string("x").unwrap();
  for i in 0..2000 {
    let key = scope.alloc_string(&format!("k{i}")).unwrap();
    scope
      .define_property(
        attributes,
        PropertyKey::String(key),
        data_desc(Value::String(value), true),
      )
      .unwrap();
  }

  let k_with = scope.alloc_string("with").unwrap();
  scope
    .define_property(
      options,
      PropertyKey::String(k_with),
      data_desc(Value::Object(attributes), true),
    )
    .unwrap();

  let supported: [&str; 0] = [];
  let err = import_attributes_from_options(&mut vm, &mut scope, Value::Object(options), &supported)
    .unwrap_err();

  match err {
    ImportCallError::Vm(VmError::Termination(term)) => {
      assert_eq!(term.reason, TerminationReason::OutOfFuel)
    }
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }
}

