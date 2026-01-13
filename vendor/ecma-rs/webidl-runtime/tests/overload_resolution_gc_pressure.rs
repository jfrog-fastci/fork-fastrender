use std::rc::Rc;

use vm_js::{HeapLimits, Value, VmError};
use webidl::ir::{IdlType, NumericType, StringType};
use webidl_runtime::overload_resolution::{
  resolve_overload, ConvertedArgument, Optionality, OverloadArg, OverloadSig, WebIdlValue,
};
use webidl_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

#[test]
fn overload_resolution_sequence_conversion_is_gc_safe_under_extreme_gc_pressure() -> Result<(), VmError>
{
  // Trigger a GC cycle before every allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));

  let iterable = rt.alloc_object_value()?;

  // Root the iterable object for the duration of setup + resolution.
  rt.with_stack_roots(&[iterable], |rt| {
    let items: Rc<Vec<Value>> = Rc::new(vec![
      Value::Number(1.0),
      Value::Number(2.0),
      Value::Number(3.0),
    ]);

    let items_for_next = items.clone();
    let next = rt.alloc_function_value(move |rt, this, _args| {
      // Allocate an intermediate value (on every step) to stress rooting correctness.
      let _ = rt.alloc_object_value()?;

      let idx_key = rt.property_key_from_str("index")?;
      let idx_value = rt.get(this, idx_key)?;
      let idx = match idx_value {
        Value::Number(n) => n as usize,
        _ => 0,
      };
      let done = idx >= items_for_next.len();
      let value = if done {
        Value::Undefined
      } else {
        items_for_next[idx]
      };

      rt.define_data_property(this, idx_key, Value::Number((idx + 1) as f64), true)?;

      let result = rt.alloc_object_value()?;
      rt.with_stack_roots(&[result], |rt| {
        let done_key = rt.property_key_from_str("done")?;
        rt.define_data_property(result, done_key, Value::Bool(done), true)?;

        let value_key = rt.property_key_from_str("value")?;
        rt.define_data_property(result, value_key, value, true)?;
        Ok(result)
      })
    })?;

    // iterable.next = next
    rt.with_stack_roots(&[next], |rt| {
      let next_key = rt.property_key_from_str("next")?;
      rt.define_data_property(iterable, next_key, next, true)?;
      Ok(())
    })?;

    // iterable[Symbol.iterator] = () => { this.index = 0; return this; }
    let iter_method = rt.alloc_function_value(move |rt, this, _args| {
      let idx_key = rt.property_key_from_str("index")?;
      rt.define_data_property(this, idx_key, Value::Number(0.0), true)?;
      Ok(this)
    })?;

    rt.with_stack_roots(&[iter_method], |rt| {
      let iter_key = rt.symbol_iterator()?;
      rt.define_data_property(iterable, iter_key, iter_method, true)?;
      Ok(())
    })?;

    // Overloads: f(sequence<long>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::Sequence(Box::new(IdlType::Numeric(NumericType::Long))),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let out = resolve_overload(rt, &overloads, &[iterable])?;
    assert_eq!(out.overload_index, 0);

    let [ConvertedArgument::Value(WebIdlValue::Sequence { elem_ty, values })] = out.values.as_slice()
    else {
      panic!("expected one sequence argument, got {:?}", out.values);
    };

    assert_eq!(elem_ty.as_ref(), &IdlType::Numeric(NumericType::Long));
    assert_eq!(
      values,
      &[
        WebIdlValue::Long(1),
        WebIdlValue::Long(2),
        WebIdlValue::Long(3),
      ]
    );

    Ok(())
  })
}
