use std::rc::Rc;

use vm_js::{HeapLimits, Value, VmError};
use webidl::ir::{IdlType, NumericType, StringType};
use webidl_runtime::overload_resolution::{
  resolve_overload, ConvertedArgument, Optionality, OverloadArg, OverloadSig, WebIdlValue,
};
use webidl_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
  let Value::String(handle) = v else {
    panic!("expected JS string value");
  };
  rt.heap().get_string(handle).unwrap().to_utf8_lossy()
}

fn define_allocating_iterable(
  rt: &mut VmJsRuntime,
  iterable: Value,
  items: Rc<Vec<Value>>,
) -> Result<(), VmError> {
  // `items` must contain only primitive values; `vm-js` does not trace Rust locals, so if the list
  // contained GC handles (strings/objects) they could be collected under extreme GC pressure.
  let items_for_iter = items.clone();
  let iter_method = rt.alloc_function_value(move |rt, _this, _args| {
    let iterator = rt.alloc_object_value()?;
    rt.with_stack_roots(&[iterator], |rt| {
      let idx_key = rt.property_key_from_str("index")?;
      rt.define_data_property(iterator, idx_key, Value::Number(0.0), true)?;

      let items_for_next = items_for_iter.clone();
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

      rt.with_stack_roots(&[next], |rt| {
        let next_key = rt.property_key_from_str("next")?;
        rt.define_data_property(iterator, next_key, next, true)?;
        Ok(())
      })?;

      Ok(iterator)
    })
  })?;

  rt.with_stack_roots(&[iter_method], |rt| {
    let iter_key = rt.symbol_iterator()?;
    rt.define_data_property(iterable, iter_key, iter_method, true)?;
    Ok(())
  })
}

#[test]
fn overload_resolution_sequence_conversion_is_gc_safe_under_extreme_gc_pressure() -> Result<(), VmError>
{
  // Trigger a GC cycle before every allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));

  let iterable = rt.alloc_object_value()?;

  // Root the iterable object for the duration of setup + resolution.
  rt.with_stack_roots(&[iterable], |rt| {
    let items: Rc<Vec<Value>> = Rc::new(vec![
      Value::Number(1.5),
      Value::Number(2.5),
      Value::Number(3.5),
    ]);

    // iterable[Symbol.iterator] allocates and returns a fresh iterator object, so the overload
    // resolution implementation must correctly root the iterator record across GC-triggering
    // conversions between `IteratorStepValue` calls.
    define_allocating_iterable(rt, iterable, items)?;

    // Overloads: f(sequence<DOMString>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::Sequence(Box::new(IdlType::String(StringType::DomString))),
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

    assert_eq!(elem_ty.as_ref(), &IdlType::String(StringType::DomString));
    let strs = values
      .iter()
      .map(|v| match v {
        WebIdlValue::String(s) => as_utf8_lossy(rt, *s),
        other => panic!("expected DOMString element, got {other:?}"),
      })
      .collect::<Vec<_>>();
    assert_eq!(
      strs,
      &["1.5".to_string(), "2.5".to_string(), "3.5".to_string()]
    );

    Ok(())
  })
}

#[test]
fn overload_resolution_frozen_array_conversion_is_gc_safe_under_extreme_gc_pressure() -> Result<(), VmError>
{
  // Trigger a GC cycle before every allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));

  let iterable = rt.alloc_object_value()?;

  rt.with_stack_roots(&[iterable], |rt| {
    let items: Rc<Vec<Value>> = Rc::new(vec![
      Value::Number(1.5),
      Value::Number(2.5),
      Value::Number(3.5),
    ]);

    define_allocating_iterable(rt, iterable, items)?;

    // Overloads: f(FrozenArray<double>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::FrozenArray(Box::new(IdlType::Numeric(NumericType::Double))),
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

    assert_eq!(elem_ty.as_ref(), &IdlType::Numeric(NumericType::Double));
    let nums = values
      .iter()
      .map(|v| match v {
        WebIdlValue::Double(n) => *n,
        other => panic!("expected double element, got {other:?}"),
      })
      .collect::<Vec<_>>();
    assert_eq!(nums, vec![1.5, 2.5, 3.5]);

    Ok(())
  })
}

#[test]
fn overload_resolution_converts_args_left_of_distinguishing_index_under_extreme_gc_pressure(
) -> Result<(), VmError> {
  // Trigger a GC cycle before every allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));

  let iterable = rt.alloc_object_value()?;

  // Root the input arguments for the duration of setup + resolution.
  let first_arg = Value::Number(42.5);
  rt.with_stack_roots(&[first_arg, iterable], |rt| {
    let items: Rc<Vec<Value>> = Rc::new(vec![
      Value::Number(1.5),
      Value::Number(2.5),
      Value::Number(3.5),
    ]);

    define_allocating_iterable(rt, iterable, items)?;

    // Overloads:
    //   f(DOMString, sequence<DOMString>)
    //   f(DOMString, DOMString)
    //
    // The distinguishing argument index is 1, so overload resolution must convert the first
    // argument (DOMString) and keep it alive across the sequence conversion of the second argument.
    let overloads = vec![
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::String(StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::Sequence(Box::new(IdlType::String(StringType::DomString))),
            optionality: Optionality::Required,
            default: None,
          },
        ],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::String(StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
        ],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let out = resolve_overload(rt, &overloads, &[first_arg, iterable])?;
    assert_eq!(out.overload_index, 0);

    let [ConvertedArgument::Value(WebIdlValue::String(first)), ConvertedArgument::Value(WebIdlValue::Sequence { elem_ty, values })] =
      out.values.as_slice()
    else {
      panic!("unexpected resolved values: {:?}", out.values);
    };

    assert_eq!(as_utf8_lossy(rt, *first), "42.5");
    assert_eq!(elem_ty.as_ref(), &IdlType::String(StringType::DomString));
    let strs = values
      .iter()
      .map(|v| match v {
        WebIdlValue::String(s) => as_utf8_lossy(rt, *s),
        other => panic!("expected DOMString element, got {other:?}"),
      })
      .collect::<Vec<_>>();
    assert_eq!(
      strs,
      vec!["1.5".to_string(), "2.5".to_string(), "3.5".to_string()]
    );

    Ok(())
  })
}
