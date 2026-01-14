use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn typed_array_to_string_tag_is_inherited_from_typed_array_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    r#"
    (() => {
      const TypedArray = Object.getPrototypeOf(Int8Array);

      if (TypedArray.name !== "TypedArray") return false;
      if (TypedArray.length !== 0) return false;

      const protoDesc = Object.getOwnPropertyDescriptor(TypedArray, "prototype");
      if (!protoDesc) return false;
      if (protoDesc.writable !== false) return false;
      if (protoDesc.enumerable !== false) return false;
      if (protoDesc.configurable !== false) return false;

      // TypedArray cannot be called or constructed directly.
      let threw = false;
      try { TypedArray(); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false;
      try { new TypedArray(); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;

      // Prototype chain: %TypedArray% sits between TypedArray constructors and Function.prototype.
      if (Object.getPrototypeOf(Int8Array) !== TypedArray) return false;
      if (Object.getPrototypeOf(Uint8Array) !== TypedArray) return false;

      // Prototype chain: %TypedArray%.prototype is the parent of each concrete TypedArray prototype.
      if (Object.getPrototypeOf(Int8Array.prototype) !== TypedArray.prototype) return false;

      // Concrete TypedArray prototypes must not have their own @@toStringTag.
      if (Int8Array.prototype.hasOwnProperty(Symbol.toStringTag)) return false;

      const desc = Object.getOwnPropertyDescriptor(TypedArray.prototype, Symbol.toStringTag);
      if (!desc) return false;
      if (typeof desc.get !== "function") return false;
      if (desc.set !== undefined) return false;
      if (desc.enumerable !== false) return false;
      if (desc.configurable !== true) return false;
      if (desc.get.name !== "get [Symbol.toStringTag]") return false;
      if (desc.get.length !== 0) return false;

      // The accessor returns undefined when `this` isn't a TypedArray instance.
      if (TypedArray.prototype[Symbol.toStringTag] !== undefined) return false;
      if (desc.get.call({}) !== undefined) return false;
      if (desc.get.call([]) !== undefined) return false;

      // It returns the TypedArrayName for TypedArray instances.
      const ta = new Int8Array();
      if (ta[Symbol.toStringTag] !== "Int8Array") return false;
      if (Object.prototype.toString.call(ta) !== "[object Int8Array]") return false;

      // Concrete TypedArray prototypes are ordinary objects (no [[TypedArrayName]]).
      if (Object.prototype.toString.call(Int8Array.prototype) !== "[object Object]") return false;

      return true;
    })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn regexp_string_iterator_prototype_has_to_string_tag() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    r#"
    (() => {
      const iter = /./[Symbol.matchAll]("a");
      const proto = Object.getPrototypeOf(iter);

      // %RegExpStringIteratorPrototype%.[[Prototype]] === %IteratorPrototype%
      const iteratorProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
      if (Object.getPrototypeOf(proto) !== iteratorProto) return false;

      if (proto[Symbol.toStringTag] !== "RegExp String Iterator") return false;
      const tagDesc = Object.getOwnPropertyDescriptor(proto, Symbol.toStringTag);
      if (!tagDesc) return false;
      if (tagDesc.value !== "RegExp String Iterator") return false;
      if (tagDesc.writable !== false) return false;
      if (tagDesc.enumerable !== false) return false;
      if (tagDesc.configurable !== true) return false;

      // %RegExpStringIteratorPrototype%.next exists and is a writable data property.
      if (typeof proto.next !== "function") return false;
      const nextDesc = Object.getOwnPropertyDescriptor(proto, "next");
      if (!nextDesc) return false;
      if (nextDesc.writable !== true) return false;
      if (nextDesc.enumerable !== false) return false;
      if (nextDesc.configurable !== true) return false;
      if (nextDesc.value.name !== "next") return false;
      if (nextDesc.value.length !== 0) return false;

      // The iterator object itself should inherit `next` (no per-instance method).
      if (Object.prototype.hasOwnProperty.call(iter, "next")) return false;

      // `Object.prototype.toString` should consult @@toStringTag via the prototype chain.
      if (Object.prototype.toString.call(iter) !== "[object RegExp String Iterator]") return false;

      // Inherits %IteratorPrototype%[@@iterator].
      if (iter[Symbol.iterator]() !== iter) return false;

      return true;
    })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn iterator_prototype_to_string_tag_and_constructor_weird_setter() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    r#"
    (() => {
      if (typeof Iterator !== "function") return false;
      if (Object.getPrototypeOf(Iterator) !== Function.prototype) return false;

      let threw = false;
      try { Iterator(); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false;
      try { new Iterator(); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;

      class SubIterator extends Iterator {}
      const sub = new SubIterator();
      if (!(sub instanceof SubIterator)) return false;
      if (!(sub instanceof Iterator)) return false;

      const IteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
      if (Iterator.prototype !== IteratorPrototype) return false;

      // --- @@toStringTag ---
      const tagDesc = Object.getOwnPropertyDescriptor(Iterator.prototype, Symbol.toStringTag);
      if (!tagDesc) return false;
      if (typeof tagDesc.get !== "function") return false;
      if (typeof tagDesc.set !== "function") return false;
      if (tagDesc.configurable !== true) return false;
      if (tagDesc.enumerable !== false) return false;

      if (Iterator.prototype[Symbol.toStringTag] !== "Iterator") return false;
      if (tagDesc.get.call() !== "Iterator") return false;

      threw = false; try { tagDesc.set.call(undefined, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false; try { tagDesc.set.call(null, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false; try { tagDesc.set.call(true, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;

      threw = false; try { tagDesc.set.call(IteratorPrototype, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false; try { IteratorPrototype[Symbol.toStringTag] = ""; } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;

      // --- constructor ---
      const ctorDesc = Object.getOwnPropertyDescriptor(Iterator.prototype, "constructor");
      if (!ctorDesc) return false;
      if (typeof ctorDesc.get !== "function") return false;
      if (typeof ctorDesc.set !== "function") return false;
      if (ctorDesc.configurable !== true) return false;
      if (ctorDesc.enumerable !== false) return false;

      if (Iterator.prototype.constructor !== Iterator) return false;
      if (ctorDesc.get.call() !== Iterator) return false;

      threw = false; try { ctorDesc.set.call(undefined, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false; try { ctorDesc.set.call(null, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false; try { ctorDesc.set.call(true, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;

      threw = false; try { ctorDesc.set.call(IteratorPrototype, ""); } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;
      threw = false; try { IteratorPrototype.constructor = ""; } catch (e) { threw = e instanceof TypeError; }
      if (!threw) return false;

      // Freeze home before exercising the CreateDataPropertyOrThrow path.
      Object.freeze(IteratorPrototype);

      const sentinelTag = "a";
      const fake1 = Object.create(IteratorPrototype);
      fake1[Symbol.toStringTag] = sentinelTag;
      if (fake1[Symbol.toStringTag] !== sentinelTag) return false;

      const tagObj = { [Symbol.toStringTag]: sentinelTag + "a" };
      tagDesc.set.call(tagObj, sentinelTag);
      if (tagObj[Symbol.toStringTag] !== sentinelTag) return false;

      const sentinelObj = {};
      const fake2 = Object.create(IteratorPrototype);
      fake2.constructor = sentinelObj;
      if (fake2.constructor !== sentinelObj) return false;

      const ctorObj = { constructor: sentinelObj + "a" };
      ctorDesc.set.call(ctorObj, sentinelObj);
      if (ctorObj.constructor !== sentinelObj) return false;

      // Home values must remain stable.
      if (Iterator.prototype[Symbol.toStringTag] !== "Iterator") return false;
      if (Iterator.prototype.constructor !== Iterator) return false;

      return true;
    })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
