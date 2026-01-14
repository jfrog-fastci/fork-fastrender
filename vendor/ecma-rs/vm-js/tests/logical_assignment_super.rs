use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn logical_assignment_super_property_dot_and_computed() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          // `&&=` (dot)
          {
            const rhs = 2;
            const proto = { p: 1 };
            const obj = { __proto__: proto, p: 0, m() { return (super.p &&= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj.p !== 2 || proto.p !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { p: 0 };
            const obj = { __proto__: proto, p: 1, m() { return (super.p &&= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj.p !== 1 || proto.p !== 0) return false;
          }

          // `||=` (dot)
          {
            const rhs = 2;
            const proto = { p: 1 };
            const obj = { __proto__: proto, p: 0, m() { return (super.p ||= rhs); } };
            const res = obj.m();
            if (res !== 1 || obj.p !== 0 || proto.p !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { p: 0 };
            const obj = { __proto__: proto, p: 1, m() { return (super.p ||= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj.p !== 2 || proto.p !== 0) return false;
          }

          // `??=` (dot)
          {
            const rhs = 2;
            const proto = { p: null };
            const obj = { __proto__: proto, p: 1, m() { return (super.p ??= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj.p !== 2 || proto.p !== null) return false;
          }
          {
            const rhs = 2;
            const proto = { p: 0 };
            const obj = { __proto__: proto, p: null, m() { return (super.p ??= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj.p !== null || proto.p !== 0) return false;
          }

          // Computed super properties.
          const k = 123;

          // `&&=` (computed)
          {
            const rhs = 2;
            const proto = { [k]: 1 };
            const obj = { __proto__: proto, [k]: 0, m() { return (super[k] &&= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj[k] !== 2 || proto[k] !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { [k]: 0 };
            const obj = { __proto__: proto, [k]: 1, m() { return (super[k] &&= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj[k] !== 1 || proto[k] !== 0) return false;
          }

          // `||=` (computed)
          {
            const rhs = 2;
            const proto = { [k]: 1 };
            const obj = { __proto__: proto, [k]: 0, m() { return (super[k] ||= rhs); } };
            const res = obj.m();
            if (res !== 1 || obj[k] !== 0 || proto[k] !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { [k]: 0 };
            const obj = { __proto__: proto, [k]: 1, m() { return (super[k] ||= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj[k] !== 2 || proto[k] !== 0) return false;
          }

          // `??=` (computed)
          {
            const rhs = 2;
            const proto = { [k]: null };
            const obj = { __proto__: proto, [k]: 1, m() { return (super[k] ??= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj[k] !== 2 || proto[k] !== null) return false;
          }
          {
            const rhs = 2;
            const proto = { [k]: 0 };
            const obj = { __proto__: proto, [k]: null, m() { return (super[k] ??= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj[k] !== null || proto[k] !== 0) return false;
          }

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn compiled_logical_assignment_super_property_dot_and_computed() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      (() => {
        // `&&=` (dot)
        {
          const rhs = 2;
          const proto = { p: 1 };
          const obj = { __proto__: proto, p: 0, m() { return (super.p &&= rhs); } };
          const res = obj.m();
          if (res !== 2 || obj.p !== 2 || proto.p !== 1) return false;
        }
        {
          const rhs = 2;
          const proto = { p: 0 };
          const obj = { __proto__: proto, p: 1, m() { return (super.p &&= rhs); } };
          const res = obj.m();
          if (res !== 0 || obj.p !== 1 || proto.p !== 0) return false;
        }

        // `||=` (dot)
        {
          const rhs = 2;
          const proto = { p: 1 };
          const obj = { __proto__: proto, p: 0, m() { return (super.p ||= rhs); } };
          const res = obj.m();
          if (res !== 1 || obj.p !== 0 || proto.p !== 1) return false;
        }
        {
          const rhs = 2;
          const proto = { p: 0 };
          const obj = { __proto__: proto, p: 1, m() { return (super.p ||= rhs); } };
          const res = obj.m();
          if (res !== 2 || obj.p !== 2 || proto.p !== 0) return false;
        }

        // `??=` (dot)
        {
          const rhs = 2;
          const proto = { p: null };
          const obj = { __proto__: proto, p: 1, m() { return (super.p ??= rhs); } };
          const res = obj.m();
          if (res !== 2 || obj.p !== 2 || proto.p !== null) return false;
        }
        {
          const rhs = 2;
          const proto = { p: 0 };
          const obj = { __proto__: proto, p: null, m() { return (super.p ??= rhs); } };
          const res = obj.m();
          if (res !== 0 || obj.p !== null || proto.p !== 0) return false;
        }

        // Computed super properties.
        const k = 123;

        // `&&=` (computed)
        {
          const rhs = 2;
          const proto = { [k]: 1 };
          const obj = { __proto__: proto, [k]: 0, m() { return (super[k] &&= rhs); } };
          const res = obj.m();
          if (res !== 2 || obj[k] !== 2 || proto[k] !== 1) return false;
        }
        {
          const rhs = 2;
          const proto = { [k]: 0 };
          const obj = { __proto__: proto, [k]: 1, m() { return (super[k] &&= rhs); } };
          const res = obj.m();
          if (res !== 0 || obj[k] !== 1 || proto[k] !== 0) return false;
        }

        // `||=` (computed)
        {
          const rhs = 2;
          const proto = { [k]: 1 };
          const obj = { __proto__: proto, [k]: 0, m() { return (super[k] ||= rhs); } };
          const res = obj.m();
          if (res !== 1 || obj[k] !== 0 || proto[k] !== 1) return false;
        }
        {
          const rhs = 2;
          const proto = { [k]: 0 };
          const obj = { __proto__: proto, [k]: 1, m() { return (super[k] ||= rhs); } };
          const res = obj.m();
          if (res !== 2 || obj[k] !== 2 || proto[k] !== 0) return false;
        }

        // `??=` (computed)
        {
          const rhs = 2;
          const proto = { [k]: null };
          const obj = { __proto__: proto, [k]: 1, m() { return (super[k] ??= rhs); } };
          const res = obj.m();
          if (res !== 2 || obj[k] !== 2 || proto[k] !== null) return false;
        }
        {
          const rhs = 2;
          const proto = { [k]: 0 };
          const obj = { __proto__: proto, [k]: null, m() { return (super[k] ??= rhs); } };
          const res = obj.m();
          if (res !== 0 || obj[k] !== null || proto[k] !== 0) return false;
        }

        return true;
      })()
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "script should execute via compiled (HIR) path"
  );

  assert_eq!(rt.exec_compiled_script(script)?, Value::Bool(true));
  Ok(())
}

#[test]
fn logical_assignment_super_computed_key_to_property_key_is_evaluated_once() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          const log = [];
          let rhsCount = 0;
          function rhs(v) { rhsCount++; log.push("rhs"); return v; }
          function key() {
            log.push("key");
            return { toString() { log.push("toString"); return "p"; } };
          }

          const proto = {
            get p() { log.push("get"); return this._p; },
            set p(v) { log.push("set:" + v); this._p = v; },
          };

          const obj = {
            __proto__: proto,
            _p: 1,
            m() {
              let res;

              // `&&=` assigns when truthy.
              log.length = 0; rhsCount = 0; this._p = 1;
              res = (super[key()] &&= rhs(2));
              if (res !== 2 || this._p !== 2 || rhsCount !== 1) return false;
              if (log.join(",") !== "key,toString,get,rhs,set:2") return false;

              log.length = 0; rhsCount = 0; this._p = 0;
              res = (super[key()] &&= rhs(2));
              if (res !== 0 || this._p !== 0 || rhsCount !== 0) return false;
              if (log.join(",") !== "key,toString,get") return false;

              // `||=` assigns when falsy.
              log.length = 0; rhsCount = 0; this._p = 0;
              res = (super[key()] ||= rhs(3));
              if (res !== 3 || this._p !== 3 || rhsCount !== 1) return false;
              if (log.join(",") !== "key,toString,get,rhs,set:3") return false;

              log.length = 0; rhsCount = 0; this._p = 5;
              res = (super[key()] ||= rhs(3));
              if (res !== 5 || this._p !== 5 || rhsCount !== 0) return false;
              if (log.join(",") !== "key,toString,get") return false;

              // `??=` assigns when nullish.
              log.length = 0; rhsCount = 0; this._p = 0;
              res = (super[key()] ??= rhs(4));
              if (res !== 0 || this._p !== 0 || rhsCount !== 0) return false;
              if (log.join(",") !== "key,toString,get") return false;

              log.length = 0; rhsCount = 0; delete this._p;
              res = (super[key()] ??= rhs(4));
              if (res !== 4 || this._p !== 4 || rhsCount !== 1) return false;
              if (log.join(",") !== "key,toString,get,rhs,set:4") return false;

              return true;
            },
          };

          return obj.m();
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn compiled_logical_assignment_super_computed_key_to_property_key_is_evaluated_once() -> Result<(), VmError>
{
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      (() => {
        const log = [];
        let rhsCount = 0;
        function rhs(v) { rhsCount++; log.push("rhs"); return v; }
        function key() {
          log.push("key");
          return { toString() { log.push("toString"); return "p"; } };
        }

        const proto = {
          get p() { log.push("get"); return this._p; },
          set p(v) { log.push("set:" + v); this._p = v; },
        };

        const obj = {
          __proto__: proto,
          _p: 1,
          m() {
            let res;

            // `&&=` assigns when truthy.
            log.length = 0; rhsCount = 0; this._p = 1;
            res = (super[key()] &&= rhs(2));
            if (res !== 2 || this._p !== 2 || rhsCount !== 1) return false;
            if (log.join(",") !== "key,toString,get,rhs,set:2") return false;

            log.length = 0; rhsCount = 0; this._p = 0;
            res = (super[key()] &&= rhs(2));
            if (res !== 0 || this._p !== 0 || rhsCount !== 0) return false;
            if (log.join(",") !== "key,toString,get") return false;

            // `||=` assigns when falsy.
            log.length = 0; rhsCount = 0; this._p = 0;
            res = (super[key()] ||= rhs(3));
            if (res !== 3 || this._p !== 3 || rhsCount !== 1) return false;
            if (log.join(",") !== "key,toString,get,rhs,set:3") return false;

            log.length = 0; rhsCount = 0; this._p = 5;
            res = (super[key()] ||= rhs(3));
            if (res !== 5 || this._p !== 5 || rhsCount !== 0) return false;
            if (log.join(",") !== "key,toString,get") return false;

            // `??=` assigns when nullish.
            log.length = 0; rhsCount = 0; this._p = 0;
            res = (super[key()] ??= rhs(4));
            if (res !== 0 || this._p !== 0 || rhsCount !== 0) return false;
            if (log.join(",") !== "key,toString,get") return false;

            log.length = 0; rhsCount = 0; delete this._p;
            res = (super[key()] ??= rhs(4));
            if (res !== 4 || this._p !== 4 || rhsCount !== 1) return false;
            if (log.join(",") !== "key,toString,get,rhs,set:4") return false;

            return true;
          },
        };

        return obj.m();
      })()
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "script should execute via compiled (HIR) path"
  );

  assert_eq!(rt.exec_compiled_script(script)?, Value::Bool(true));
  Ok(())
}
