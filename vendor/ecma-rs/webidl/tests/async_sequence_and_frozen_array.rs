mod toy;

use toy::{ToyRuntime, ToyValue};
use webidl::{
    convert_js_to_idl, resolve_overload, AsyncSequenceKind, IdlType, IdlValue, Optionality, Overload,
};

#[test]
fn async_sequence_conversion_prefers_async_iterator() {
    let mut rt = ToyRuntime::default();
    let x = rt.string("x");
    let obj = rt.make_iterable(vec![x], true);

    let ty = IdlType::AsyncSequence(Box::new(IdlType::DomString));
    let v = convert_js_to_idl(&mut rt, &ty, obj).unwrap();

    let IdlValue::AsyncSequence(seq) = v else {
        panic!("expected AsyncSequence, got {v:?}");
    };
    assert_eq!(seq.kind, AsyncSequenceKind::Async);
    assert_eq!(seq.object, obj);
    assert_eq!(rt.get_method_calls, 1);
}

#[test]
fn async_sequence_conversion_falls_back_to_sync_iterator() {
    let mut rt = ToyRuntime::default();
    let x = rt.string("x");
    let obj = rt.make_iterable(vec![x], false);

    let ty = IdlType::AsyncSequence(Box::new(IdlType::DomString));
    let v = convert_js_to_idl(&mut rt, &ty, obj).unwrap();

    let IdlValue::AsyncSequence(seq) = v else {
        panic!("expected AsyncSequence, got {v:?}");
    };
    assert_eq!(seq.kind, AsyncSequenceKind::Sync);
    assert_eq!(seq.object, obj);
    assert_eq!(rt.get_method_calls, 2);
}

#[test]
fn frozen_array_conversion_creates_frozen_array_object() {
    let mut rt = ToyRuntime::default();
    let obj = rt.make_iterable(vec![ToyValue::Number(1.0), ToyValue::Number(2.0)], false);

    let ty = IdlType::FrozenArray(Box::new(IdlType::Double));
    let v = convert_js_to_idl(&mut rt, &ty, obj).unwrap();

    let IdlValue::FrozenArray(array_obj) = v else {
        panic!("expected FrozenArray, got {v:?}");
    };
    assert!(rt.is_frozen(array_obj));
    assert_eq!(
        rt.array_elements(array_obj).unwrap(),
        vec![ToyValue::Number(1.0), ToyValue::Number(2.0)]
    );
    // FrozenArray conversion should call GetMethod(@@iterator) only once.
    assert_eq!(rt.get_method_calls, 1);
}

#[test]
fn union_selection_sequence_and_frozen_array_use_cached_iterator_method() {
    // sequence<T>
    let mut rt = ToyRuntime::default();
    let union_ty = IdlType::Union(vec![
        IdlType::Sequence(Box::new(IdlType::Double)),
        IdlType::DomString,
    ]);
    let obj = rt.make_iterable(vec![ToyValue::Number(1.0)], false);
    let v = convert_js_to_idl(&mut rt, &union_ty, obj).unwrap();
    let IdlValue::Union(u) = v else {
        panic!("expected union value");
    };
    assert!(matches!(u.selected_type, IdlType::Sequence(_)));
    assert!(matches!(*u.value, IdlValue::Sequence(_)));
    assert_eq!(rt.get_method_calls, 1);

    // FrozenArray<T>
    let mut rt = ToyRuntime::default();
    let union_ty = IdlType::Union(vec![
        IdlType::FrozenArray(Box::new(IdlType::Double)),
        IdlType::DomString,
    ]);
    let obj = rt.make_iterable(vec![ToyValue::Number(1.0)], false);
    let v = convert_js_to_idl(&mut rt, &union_ty, obj).unwrap();
    let IdlValue::Union(u) = v else {
        panic!("expected union value");
    };
    assert!(matches!(u.selected_type, IdlType::FrozenArray(_)));
    let IdlValue::FrozenArray(array_obj) = *u.value else {
        panic!("expected FrozenArray");
    };
    assert!(rt.is_frozen(array_obj));
    assert_eq!(rt.get_method_calls, 1);
}

#[test]
fn union_async_sequence_string_object_special_case() {
    let mut rt = ToyRuntime::default();
    let union_ty = IdlType::Union(vec![
        IdlType::AsyncSequence(Box::new(IdlType::DomString)),
        IdlType::DomString,
    ]);

    let string_obj = rt.string_object("hello");
    // String objects are iterable in JS; model that so we can ensure the special-case is honored.
    let h = rt.string("h");
    rt.add_iterable_methods(string_obj, vec![h], false);

    let v = convert_js_to_idl(&mut rt, &union_ty, string_obj).unwrap();
    let IdlValue::Union(u) = v else {
        panic!("expected union value");
    };
    assert!(matches!(u.selected_type, IdlType::DomString));
    let IdlValue::DomString(s) = *u.value else {
        panic!("expected DomString");
    };
    assert_eq!(rt.string_contents(s), "hello");
    // Special-case (d): do not call GetMethod for async sequence detection when V is a string
    // object and the union contains a string type.
    assert_eq!(rt.get_method_calls, 0);
}

#[test]
fn overload_resolution_uses_cached_iterator_method() {
    let overloads = vec![
        Overload {
            id: "seq",
            types: vec![IdlType::Sequence(Box::new(IdlType::Double))],
            optionality: vec![Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString],
            optionality: vec![Optionality::Required],
        },
    ];

    let mut rt = ToyRuntime::default();
    let obj = rt.make_iterable(vec![ToyValue::Number(1.0)], false);
    let res = resolve_overload(&mut rt, &overloads, &[obj]).unwrap();
    assert_eq!(res.overload_id, "seq");
    assert_eq!(rt.get_method_calls, 1);
    assert!(matches!(
        &res.values[0],
        webidl::OverloadArg::Value(IdlValue::Sequence(_))
    ));
}

#[test]
fn overload_resolution_frozen_array_and_async_sequence() {
    // FrozenArray<T> vs DOMString
    let overloads = vec![
        Overload {
            id: "frozen",
            types: vec![IdlType::FrozenArray(Box::new(IdlType::Double))],
            optionality: vec![Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString],
            optionality: vec![Optionality::Required],
        },
    ];
    let mut rt = ToyRuntime::default();
    let obj = rt.make_iterable(vec![ToyValue::Number(1.0)], false);
    let res = resolve_overload(&mut rt, &overloads, &[obj]).unwrap();
    assert_eq!(res.overload_id, "frozen");
    assert_eq!(rt.get_method_calls, 1);

    // async_sequence<T> vs DOMString with async iterable
    let overloads = vec![
        Overload {
            id: "async",
            types: vec![IdlType::AsyncSequence(Box::new(IdlType::DomString))],
            optionality: vec![Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString],
            optionality: vec![Optionality::Required],
        },
    ];
    let mut rt = ToyRuntime::default();
    let x = rt.string("x");
    let obj = rt.make_iterable(vec![x], true);
    let res = resolve_overload(&mut rt, &overloads, &[obj]).unwrap();
    assert_eq!(res.overload_id, "async");
    assert_eq!(rt.get_method_calls, 1);

    // Special-case (d): string object should pick the string overload without calling GetMethod.
    let mut rt = ToyRuntime::default();
    let obj = rt.string_object("hello");
    let h = rt.string("h");
    rt.add_iterable_methods(obj, vec![h], false);
    let res = resolve_overload(&mut rt, &overloads, &[obj]).unwrap();
    assert_eq!(res.overload_id, "str");
    assert_eq!(rt.get_method_calls, 0);
}

#[test]
fn union_sequence_and_frozen_array_string_object_special_case() {
    // sequence<T> vs DOMString
    let mut rt = ToyRuntime::default();
    let union_ty = IdlType::Union(vec![
        IdlType::Sequence(Box::new(IdlType::DomString)),
        IdlType::DomString,
    ]);

    let string_obj = rt.string_object("hello");
    // String objects are iterable; model that so we can ensure the special-case is honored.
    let x = rt.string("x");
    rt.add_iterable_methods(string_obj, vec![x], false);

    let v = convert_js_to_idl(&mut rt, &union_ty, string_obj).unwrap();
    let IdlValue::Union(u) = v else {
        panic!("expected union value");
    };
    assert!(matches!(u.selected_type, IdlType::DomString));
    // Special-case (d): do not call GetMethod(@@iterator) for sequence selection when V is a string
    // object and the union contains a string type.
    assert_eq!(rt.get_method_calls, 0);

    // FrozenArray<T> vs DOMString
    let mut rt = ToyRuntime::default();
    let union_ty = IdlType::Union(vec![
        IdlType::FrozenArray(Box::new(IdlType::DomString)),
        IdlType::DomString,
    ]);

    let string_obj = rt.string_object("hello");
    let x = rt.string("x");
    rt.add_iterable_methods(string_obj, vec![x], false);

    let v = convert_js_to_idl(&mut rt, &union_ty, string_obj).unwrap();
    let IdlValue::Union(u) = v else {
        panic!("expected union value");
    };
    assert!(matches!(u.selected_type, IdlType::DomString));
    // Special-case (d): do not call GetMethod(@@iterator) for FrozenArray selection when V is a
    // string object and the union contains a string type.
    assert_eq!(rt.get_method_calls, 0);
}

#[test]
fn overload_resolution_sequence_and_frozen_array_string_object_special_case() {
    // sequence<T> vs DOMString
    let overloads = vec![
        Overload {
            id: "seq",
            types: vec![IdlType::Sequence(Box::new(IdlType::DomString))],
            optionality: vec![Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString],
            optionality: vec![Optionality::Required],
        },
    ];

    let mut rt = ToyRuntime::default();
    let string_obj = rt.string_object("hello");
    let x = rt.string("x");
    rt.add_iterable_methods(string_obj, vec![x], false);
    let res = resolve_overload(&mut rt, &overloads, &[string_obj]).unwrap();
    assert_eq!(res.overload_id, "str");
    assert_eq!(rt.get_method_calls, 0);

    // FrozenArray<T> vs DOMString
    let overloads = vec![
        Overload {
            id: "frozen",
            types: vec![IdlType::FrozenArray(Box::new(IdlType::DomString))],
            optionality: vec![Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString],
            optionality: vec![Optionality::Required],
        },
    ];

    let mut rt = ToyRuntime::default();
    let string_obj = rt.string_object("hello");
    let x = rt.string("x");
    rt.add_iterable_methods(string_obj, vec![x], false);
    let res = resolve_overload(&mut rt, &overloads, &[string_obj]).unwrap();
    assert_eq!(res.overload_id, "str");
    assert_eq!(rt.get_method_calls, 0);
}

#[test]
fn overload_resolution_string_object_special_case_at_nonzero_distinguishing_index() {
    // Overloads:
    //   f(DOMString, sequence<DOMString>)
    //   f(DOMString, DOMString)
    //
    // The distinguishing argument index is 1. String objects must be treated as strings even when
    // they appear after arguments that were already converted.
    let overloads = vec![
        Overload {
            id: "seq",
            types: vec![
                IdlType::DomString,
                IdlType::Sequence(Box::new(IdlType::DomString)),
            ],
            optionality: vec![Optionality::Required, Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString, IdlType::DomString],
            optionality: vec![Optionality::Required, Optionality::Required],
        },
    ];

    let mut rt = ToyRuntime::default();

    let first_arg = rt.string("prefix");

    let string_obj = rt.string_object("hello");
    // Model that String objects are iterable so we can detect any incorrect probing of @@iterator.
    let x = rt.string("x");
    rt.add_iterable_methods(string_obj, vec![x], false);

    let res = resolve_overload(&mut rt, &overloads, &[first_arg, string_obj]).unwrap();
    assert_eq!(res.overload_id, "str");
    // Special-case (d): do not call GetMethod(@@iterator) for sequence selection when V is a
    // string object and a string type is present at the distinguishing argument index.
    assert_eq!(rt.get_method_calls, 0);
}

#[test]
fn overload_resolution_string_object_async_sequence_special_case_at_nonzero_distinguishing_index() {
    // Overloads:
    //   f(DOMString, async_sequence<DOMString>)
    //   f(DOMString, DOMString)
    //
    // The distinguishing argument index is 1. String objects are iterable and could appear to
    // satisfy async_sequence via @@iterator, but requirement (d) forces them to be treated as
    // strings when a string type is present at that position.
    let overloads = vec![
        Overload {
            id: "async_seq",
            types: vec![
                IdlType::DomString,
                IdlType::AsyncSequence(Box::new(IdlType::DomString)),
            ],
            optionality: vec![Optionality::Required, Optionality::Required],
        },
        Overload {
            id: "str",
            types: vec![IdlType::DomString, IdlType::DomString],
            optionality: vec![Optionality::Required, Optionality::Required],
        },
    ];

    let mut rt = ToyRuntime::default();

    let first_arg = rt.string("prefix");

    let string_obj = rt.string_object("hello");
    // Model that String objects are iterable so we can detect any incorrect probing for
    // async_sequence matching (which checks @@asyncIterator and then @@iterator).
    let x = rt.string("x");
    rt.add_iterable_methods(string_obj, vec![x], false);

    let res = resolve_overload(&mut rt, &overloads, &[first_arg, string_obj]).unwrap();
    assert_eq!(res.overload_id, "str");
    // Special-case (d): do not call GetMethod for async sequence detection when V is a string
    // object and a string type is present at the distinguishing argument index.
    assert_eq!(rt.get_method_calls, 0);
}
