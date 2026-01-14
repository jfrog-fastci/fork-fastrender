use vm_js::{Heap, HeapLimits, JsRuntime, MicrotaskQueue, SourceText, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn class_method_symbol_function_names() {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::default();

  // test262 expects anonymous symbols (`Symbol()`, `description === undefined`) to contribute an
  // empty string in `SetFunctionName` for class method definitions.
  let source = SourceText::new_charged_arc(
    &mut rt.heap,
    "<inline>",
    r#"
        var namedSym = Symbol('test262');
        var anonSym = Symbol();

        class A {
          id() {}
          [anonSym]() {}
          [namedSym]() {}
          static id() {}
          static [anonSym]() {}
          static [namedSym]() {}
        }

        function pair(f) {
          var d = Object.getOwnPropertyDescriptor(f, 'name').value;
          var v = f.name;
          return JSON.stringify(d) + '/' + JSON.stringify(v);
        }

        [
          pair(A.prototype.id),
          pair(A.prototype[anonSym]),
          pair(A.prototype[namedSym]),
          pair(A.id),
          pair(A[anonSym]),
          pair(A[namedSym]),
        ].join('|')
      "#,
  )
  .unwrap();

  // Execute via the AST interpreter entry point (used by `test262-semantic`) to catch any HIR/AST
  // mismatches.
  let value = rt.exec_script_source_with_hooks(&mut hooks, source).unwrap();

  assert_value_is_utf8(
    &rt,
    value,
    "\"id\"/\"id\"|\"\"/\"\"|\"[test262]\"/\"[test262]\"|\"id\"/\"id\"|\"\"/\"\"|\"[test262]\"/\"[test262]\"",
  );
}
