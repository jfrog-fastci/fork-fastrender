use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn bigint_is_available_and_literals_execute() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script("typeof BigInt === 'function' && typeof 1n === 'bigint' && (1n + 2n) === 3n")
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_constructor_converts_and_throws() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      "BigInt(true)===1n && BigInt(false)===0n && BigInt(' 0x10 ')===16n && BigInt('')===0n",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt(1.5); false } catch (e) { e.name === 'RangeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt(NaN); false } catch (e) { e.name === 'RangeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt(Infinity); false } catch (e) { e.name === 'RangeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt('0b2'); false } catch (e) { e.name === 'SyntaxError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  // Leading sign is not allowed with non-decimal-prefixed strings.
  let value = rt
    .exec_script("try { BigInt('-0x1'); false } catch (e) { e.name === 'SyntaxError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt(); false } catch (e) { e.name === 'TypeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { new BigInt(1); false } catch (e) { e.name === 'TypeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_static_methods_as_intn_as_uintn() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      "BigInt.asUintN(8, 0xabcdn) === 0xcdn && BigInt.asUintN(1, -1n) === 1n && BigInt.asIntN(3, 10n) === 2n",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt.asUintN(-1, 0n); false } catch (e) { e.name === 'RangeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script(
      "try { BigInt.asUintN(0, '0b2'); false } catch (e) { e.name === 'SyntaxError' }",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_prototype_methods_to_string_and_value_of() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      "(255n).toString(16) === 'ff' && BigInt.prototype.toString.length === 0 && (1n).toLocaleString() === '1'",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("Object.prototype.toString.call(1n) === '[object BigInt]'")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("BigInt.prototype.valueOf.call(Object(1n)) === 1n")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt.prototype.valueOf.call({}); false } catch (e) { e.name === 'TypeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { BigInt.prototype.toString.call({}); false } catch (e) { e.name === 'TypeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script("try { (1n).toString(1); false } catch (e) { e.name === 'RangeError' }")
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

