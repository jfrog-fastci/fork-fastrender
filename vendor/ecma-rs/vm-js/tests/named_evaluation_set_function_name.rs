use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn run_interpreter(source: &str) -> Value {
  let mut rt = new_runtime();
  rt.exec_script(source).unwrap()
}

fn run_compiled(source: &str) -> Value {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(rt.heap_mut(), "<named_evaluation>", source).unwrap();
  assert!(
    !script.requires_ast_fallback,
    "expected script to be supported by the HIR executor"
  );
  rt.exec_compiled_script(script).unwrap()
}

#[test]
fn named_evaluation_set_function_name_matches_node() {
  let source = r#"
    (() => {
      let ok = true;
      const check = (cond) => { if (!cond) ok = false; };

      // --- Variable declarators: inferred name is visible during class construction ---
      let out;
      let C = class { static { out = this.name } };
      check(out === "C");
      check(C.name === "C");

      // `static name() {}` must override the constructor's initial `"name"` property.
      let C2 = class { static name(){} };
      check(typeof C2.name === "function");
      check(C2.name.name === "name");

      // Sequence expressions break syntactic name inference.
      let C3 = (0, class {});
      check(C3.name === "");

       // --- Assignment: binding targets infer names, property targets do not ---
       out = undefined;
       let C4;
       C4 = class { static { out = this.name } };
       check(out === "C4");
       check(C4.name === "C4");

       // Parenthesized IdentifierReference breaks name inference.
       out = undefined;
       let C5;
       (C5) = class { static { out = this.name } };
       check(out === "");
       check(C5.name === "");

       out = undefined;
       let o = {};
       o.a = class { static { out = this.name } };
       check(out === "");
       check(o.a.name === "");

      // --- No dynamic post-hoc renaming ---
      let g = (0, function(){});
      let x;
      x = g;
      check(x.name === "");
      let { a6 } = { a6: g };
      check(a6.name === "");

      // --- Destructuring defaults infer names only for identifier targets ---
      out = undefined;
      let { a7 = class { static { out = this.name } } } = {};
      check(out === "a7");
      check(a7.name === "a7");

       out = undefined;
       let a9;
       ({ a9 = class { static { out = this.name } } } = {});
       check(out === "a9");
       check(a9.name === "a9");

       out = undefined;
       let a10;
       ({ a: a10 = class { static { out = this.name } } } = { a: undefined });
       check(out === "a10");
       check(a10.name === "a10");

       out = undefined;
       let a11;
       ({ a: (a11) = class { static { out = this.name } } } = { a: undefined });
       check(out === "");
       check(a11.name === "");

       // --- Parameter defaults infer names only for identifier params ---
       out = undefined;
       let paramClass;
       function f(a8 = class { static { out = this.name; paramClass = this; } }) {}
      f();
      check(out === "a8");
      check(paramClass.name === "a8");

      // --- Logical assignment infers names for bindings only ---
       out = undefined;
       let L;
       L ||= class { static { out = this.name } };
       check(out === "L");
       check(L.name === "L");

       out = undefined;
       let L2;
       (L2) ||= class { static { out = this.name } };
       check(out === "");
       check(L2.name === "");

       out = undefined;
        o.b ||= class { static { out = this.name } };
        check(out === "");
        check(o.b.name === "");

      return ok;
    })()
  "#;

  assert_eq!(run_interpreter(source), Value::Bool(true));
  assert_eq!(run_compiled(source), Value::Bool(true));
}
