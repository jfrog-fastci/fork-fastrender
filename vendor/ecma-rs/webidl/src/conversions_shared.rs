use crate::ir::WebIdlException;
use crate::runtime::{IteratorRecord, WebIdlJsRuntime};
use crate::{NumericConversionError, NumericConversionErrorKind};

const STRING_EXCEEDS_MAXIMUM_LENGTH: &str = "string exceeds maximum length";
const SEQUENCE_EXCEEDS_MAXIMUM_LENGTH: &str = "sequence exceeds maximum length";
const RECORD_EXCEEDS_MAXIMUM_ENTRY_COUNT: &str = "record exceeds maximum entry count";

/// Ensure `string` is within [`WebIdlLimits::max_string_code_units`].
#[inline]
pub(crate) fn enforce_string_code_units_limit<R: WebIdlJsRuntime>(
  rt: &mut R,
  string: R::JsValue,
) -> Result<(), R::Error> {
  // Use `with_string_code_units` so runtimes can validate/borrow their internal UTF-16 storage
  // without allocating a Rust string.
  let max_units = rt.limits().max_string_code_units;
  let len = rt.with_string_code_units(string, |units| units.len())?;
  if len > max_units {
    return Err(rt.throw_range_error(STRING_EXCEEDS_MAXIMUM_LENGTH));
  }
  Ok(())
}

/// Materialize an ECMAScript iterable into a Vec, enforcing [`WebIdlLimits::max_sequence_length`]
/// and rooting any previously-converted values provided via `append_roots`.
pub(crate) fn materialize_iterable<R, T>(
  rt: &mut R,
  iterable: R::JsValue,
  method: R::JsValue,
  mut convert_elem: impl FnMut(&mut R, R::JsValue) -> Result<T, R::Error>,
  mut append_roots: impl FnMut(&mut Vec<R::JsValue>, &T),
) -> Result<Vec<T>, R::Error>
where
  R: WebIdlJsRuntime,
{
  let mut iterator_record: IteratorRecord<R::JsValue> = rt.get_iterator_from_method(iterable, method)?;

  rt.with_stack_roots(
    &[
      iterable,
      iterator_record.iterator,
      iterator_record.next_method,
    ],
    |rt| {
      let mut out = Vec::<T>::new();
      // Roots for any JS values stored in `out` so far. These are not otherwise visible to the GC.
      let mut value_roots: Vec<R::JsValue> = Vec::new();

      loop {
        // Keep previously-converted values alive while we perform the next `IteratorStepValue`, which
        // can allocate and trigger GC.
        let next = rt.with_stack_roots(&value_roots, |rt| rt.iterator_step_value(&mut iterator_record))?;
        let Some(next) = next else {
          break;
        };

        if out.len() >= rt.limits().max_sequence_length {
          return Err(rt.throw_range_error(SEQUENCE_EXCEEDS_MAXIMUM_LENGTH));
        }

        let converted = rt.with_stack_roots(&value_roots, |rt| {
          rt.with_stack_roots(&[next], |rt| convert_elem(rt, next))
        })?;
        append_roots(&mut value_roots, &converted);
        out.push(converted);
      }

      Ok(out)
    },
  )
}

/// Materialize a WebIDL `record<K, V>` from an ECMAScript object, enforcing
/// [`WebIdlLimits::max_record_entries`] and applying "map/set" overwrite semantics.
pub(crate) fn materialize_record_entries<R, V>(
  rt: &mut R,
  obj: R::JsValue,
  mut convert_entry: impl FnMut(&mut R, R::JsValue, R::JsValue) -> Result<(String, V), R::Error>,
  mut append_value_roots: impl FnMut(&mut Vec<R::JsValue>, &V),
) -> Result<Vec<(String, V)>, R::Error>
where
  R: WebIdlJsRuntime,
{
  let keys = rt.with_stack_roots(&[obj], |rt| rt.own_property_keys(obj))?;

  // Root `obj` and any JS values produced by earlier property conversions for the duration of later
  // conversions.
  let mut roots: Vec<R::JsValue> = Vec::new();
  roots.push(obj);

  // Records use map/set semantics; when a converted key already exists, the value is overwritten
  // without changing insertion order.
  let mut out = Vec::<(String, V)>::new();
  let mut index_by_key = std::collections::HashMap::<String, usize>::new();

  for key in keys {
    let Some((typed_key, typed_value)) = rt.with_stack_roots(&roots, |rt| {
      let Some(desc) = rt.get_own_property(obj, key)? else {
        return Ok(None);
      };
      if !desc.enumerable {
        return Ok(None);
      }

      let key_value = rt.property_key_to_js_string(key)?;
      let value = rt.get(obj, key)?;
      let (typed_key, typed_value) =
        rt.with_stack_roots(&[key_value, value], |rt| convert_entry(rt, key_value, value))?;

      Ok(Some((typed_key, typed_value)))
    })?
    else {
      continue;
    };

    append_value_roots(&mut roots, &typed_value);

    if let Some(&idx) = index_by_key.get(&typed_key) {
      out[idx].1 = typed_value;
    } else {
      if out.len() >= rt.limits().max_record_entries {
        return Err(rt.throw_range_error(RECORD_EXCEEDS_MAXIMUM_ENTRY_COUNT));
      }
      index_by_key.insert(typed_key.clone(), out.len());
      out.push((typed_key, typed_value));
    }
  }

  Ok(out)
}

#[inline]
pub(crate) fn numeric_conversion_error_to_webidl_exception(err: NumericConversionError) -> WebIdlException {
  match err.kind() {
    NumericConversionErrorKind::TypeError => WebIdlException::type_error(err.message()),
    NumericConversionErrorKind::RangeError => WebIdlException::range_error(err.message()),
  }
}

#[inline]
pub(crate) fn numeric_conversion_error_to_js_error<R: WebIdlJsRuntime>(
  rt: &mut R,
  err: NumericConversionError,
) -> R::Error {
  match err.kind() {
    NumericConversionErrorKind::TypeError => rt.throw_type_error(err.message()),
    NumericConversionErrorKind::RangeError => rt.throw_range_error(err.message()),
  }
}
