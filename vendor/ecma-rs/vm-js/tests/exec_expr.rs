use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, Scope, Value, Vm, VmError, VmHostHooks,
  VmHost, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn return_this(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(this)
}

fn return_arg_count(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Number(args.len() as f64))
}

#[test]
fn object_literal_member_get_set() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = {a: 1}; o.a === 1; o.a = 2; o.a"#)
    .unwrap();
  assert_eq!(value, Value::Number(2.0));
}

#[test]
fn object_prototype_has_own_property_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o={a:1}; o.hasOwnProperty("a") && !o.hasOwnProperty("toString")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_has_own_property_works_on_primitives() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""ab".hasOwnProperty("0") && "ab".hasOwnProperty("length")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_has_own_property_supports_symbol_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var s=Symbol("x"); var o={}; o[s]=1; o.hasOwnProperty(s)"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_has_own_property_works_on_typed_arrays() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var u=new Uint8Array(2); u.hasOwnProperty("0") && !u.hasOwnProperty("length")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn computed_member_get_set() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var o = {}; o["x"] = 3; o.x"#).unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn computed_member_object_key_get_set() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = {}; var k = {}; o[k] = 4; o[k]"#)
    .unwrap();
  assert_eq!(value, Value::Number(4.0));
}

#[test]
fn array_literal_index_get() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a = [1,2]; (a[0] === 1) && (a[1] === 2)"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_is_array_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Array.isArray([1]) && !Array.isArray({}) && !Array.isArray("x")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_for_each_iterates_existing_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; delete a[1]; var s=0; a.forEach(function(x){ s = s + x; }); s===4"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_for_each_binds_this_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o={sum:0}; [1,2].forEach(function(x){ this.sum = this.sum + x; }, o); o.sum===3"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_index_of_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o={}; var a=[1,2,3]; var b=[Number.NaN]; var c=[o]; a.indexOf(2)===1 && a.indexOf(2,2)===-1 && a.indexOf(2,-2)===1 && b.indexOf(Number.NaN)===-1 && c.indexOf(o)===0 && Array.prototype.indexOf.call("ab","b")===1"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_includes_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; var b=[Number.NaN]; var c=[]; c.length=1; a.includes(2) && !a.includes(2,2) && a.includes(2,-2) && b.includes(Number.NaN) && c.includes(undefined) && Array.prototype.includes.call("ab","b")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_filter_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; delete a[1]; var b=a.filter(function(x){ return x % 2 === 1; }); b.length===2 && b[0]===1 && b[1]===3 && Array.prototype.filter.call("ab",function(x){return x==="b";})[0]==="b""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_reduce_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; var s=a.reduce(function(acc,x){return acc+x;}); var b=[]; b.length=3; b[1]=2; b[2]=4; var t=b.reduce(function(acc,x){return acc+x;}); var ok=false; try { [].reduce(function(a,b){return a+b;}); } catch(e) { ok = e.name === "TypeError"; } s===6 && t===6 && ok && Array.prototype.reduce.call("ab", function(acc,x){return acc+x;}, "") === "ab""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_some_every_find_find_index_work() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; delete a[0]; var some=a.some(function(x){return x===2;}); var every=a.every(function(x){return x>0;}); var find=a.find(function(x){return x>1;}); var fi=a.findIndex(function(x){return x===2;}); some && every && find===2 && fi===1 && Array.prototype.some.call("ab",function(x){return x==="b";}) && Array.prototype.every.call("ab",function(x){return x!=="x";}) && Array.prototype.find.call("ab",function(x){return x==="b";})==="b" && Array.prototype.findIndex.call("ab",function(x){return x==="b";})===1"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_concat_works_and_preserves_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2]; var b=[3]; var c=a.concat(b,4); var d=[1,2,3]; delete d[1]; var e=d.concat([]); c.length===4 && c[2]===3 && c[3]===4 && e.length===3 && e[0]===1 && e[2]===3 && !e.hasOwnProperty("1")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_slice_copies_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a=[1,2,3]; var b=a.slice(1); b.length===2 && b[0]===2 && b[1]===3"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_slice_is_generic_and_boxes_primitives() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var b = Array.prototype.slice.call("ab"); b.length===2 && b[0]==="a" && b[1]==="b""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_slice_converts_start_end_via_to_number() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3,4]; var start={valueOf:function(){return 1;}}; var end={valueOf:function(){return 3;}}; var b=a.slice(start,end); b.length===2 && b[0]===2 && b[1]===3"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_push_appends_and_returns_length() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a=[]; var l=a.push(1,2); l===2 && a.length===2 && a[0]===1 && a[1]===2"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_pop_removes_last_element_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2]; var x=a.pop(); var b=[]; var y=b.pop(); var o={0:"a", length:1}; var z=Array.prototype.pop.call(o); x===2 && a.length===1 && a[0]===1 && y===undefined && b.length===0 && z==="a" && o.length===0 && !o.hasOwnProperty("0")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_shift_removes_first_element_preserves_holes_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; var x=a.shift(); var b=[1,2,3]; delete b[1]; var y=b.shift(); var o={0:"a",1:"b",length:2}; var z=Array.prototype.shift.call(o); x===1 && a.length===2 && a[0]===2 && a[1]===3 && y===1 && b.length===2 && !b.hasOwnProperty("0") && b[1]===3 && z==="a" && o[0]==="b" && o.length===1 && !o.hasOwnProperty("1")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_unshift_inserts_preserves_holes_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[2,3]; var l=a.unshift(0,1); var b=[1,2]; delete b[0]; b.unshift(9); var o={0:"b",length:1}; var l2=Array.prototype.unshift.call(o,"a"); l===4 && a.length===4 && a[0]===0 && a[1]===1 && a[2]===2 && a[3]===3 && b.length===3 && b[0]===9 && !b.hasOwnProperty("1") && b[2]===2 && l2===2 && o[0]==="a" && o[1]==="b" && o.length===2"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_splice_removes_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; var r=a.splice(1,1); r.length===1 && r[0]===2 && a.length===2 && a[0]===1 && a[1]===3"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_splice_inserts_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; var r=a.splice(1,0,9); r.length===0 && a.length===4 && a[0]===1 && a[1]===9 && a[2]===2 && a[3]===3"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_splice_converts_start_and_delete_count_via_to_number() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3,4]; var start={valueOf:function(){return 1;}}; var dc={valueOf:function(){return 2;}}; var r=a.splice(start,dc,9); r.length===2 && r[0]===2 && r[1]===3 && a.length===3 && a[0]===1 && a[1]===9 && a[2]===4"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_reverse_reverses_in_place_and_returns_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3]; var r=a.reverse(); (r===a) && a.length===3 && a[0]===3 && a[1]===2 && a[2]===1"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_reverse_preserves_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[1,2,3,4]; delete a[0]; a.reverse(); a.length===4 && a[0]===4 && a[1]===3 && a[2]===2 && a[3]===undefined && !a.hasOwnProperty("3")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn arithmetic_precedence() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1 + 2 * 3 === 7"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn logical_ops() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"true && false"#).unwrap();
  assert_eq!(value, Value::Bool(false));

  let value = rt.exec_script(r#"false || true"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"null ?? 5"#).unwrap();
  assert_eq!(value, Value::Number(5.0));
}

#[test]
fn cond_operator() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"true ? 1 : 2"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn delete_member() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var o = {a: 1}; delete o.a; o.a"#).unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn call_expr_member_binds_this() {
  let mut rt = new_runtime();

  let call_id = rt.vm.register_native_call(return_this).unwrap();
  let global = rt.realm().global_object();
  let mut scope = rt.heap.scope();
  let name = scope.alloc_string("returnThis").unwrap();
  let func = scope.alloc_native_function(call_id, None, name, 0).unwrap();
  let ok = scope
    .create_data_property(global, PropertyKey::from_string(name), Value::Object(func))
    .unwrap();
  assert!(ok);
  drop(scope);

  let value = rt
    .exec_script(r#"var o = {}; o.f = returnThis; o.f() === o"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn call_expr_passes_arguments() {
  let mut rt = new_runtime();

  let call_id = rt.vm.register_native_call(return_arg_count).unwrap();
  let global = rt.realm().global_object();
  let mut scope = rt.heap.scope();
  let name = scope.alloc_string("argc").unwrap();
  let func = scope.alloc_native_function(call_id, None, name, 0).unwrap();
  let ok = scope
    .create_data_property(global, PropertyKey::from_string(name), Value::Object(func))
    .unwrap();
  assert!(ok);
  drop(scope);

  let value = rt.exec_script(r#"argc(1, 2, 3)"#).unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn function_expression_is_callable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var f = function (x) { return x + 1; }; f(1)"#)
    .unwrap();
  assert_eq!(value, Value::Number(2.0));
}

#[test]
fn function_objects_inherit_function_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var f = function () {}; typeof f.call === "function""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn new_operator_constructs_ecma_function() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function Foo(x) { this.x = x; } var o = new Foo(3); o.x"#)
    .unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn direct_eval_executes_string_source() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"eval("1 + 2")"#).unwrap();
  assert_eq!(value, Value::Number(3.0));

  let value = rt.exec_script(r#"eval(5)"#).unwrap();
  assert_eq!(value, Value::Number(5.0));
}

#[test]
fn template_literal_with_substitution_concatenates() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"`a${1}b`"#).unwrap();
  let Value::String(s) = value else {
    panic!("expected string from template literal");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "a1b");
}

#[test]
fn assignment_addition_works_for_strings_and_numbers() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"var x = "a"; x += "b"; x"#).unwrap();
  let Value::String(s) = value else {
    panic!("expected string from x += \"b\"");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "ab");

  let value = rt.exec_script(r#"var n = 1; n += 2; n"#).unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn string_primitive_has_length_and_index_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""abc".length === 3 && "abc"[1] === "b" && ("abc"[9] === undefined)"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_char_at_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abc".charAt(1) === "b" && "abc".charAt(9) === "" && "abc".charAt(-1) === "" && String.prototype.charAt.call(123, 1) === "2""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_slice_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""abcd".slice(1, 3) === "bc" && "abcd".slice(-1) === "d""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_slice_is_generic_and_coerces_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o={toString:function(){return "ab";}}; var start={valueOf:function(){return 1;}}; String.prototype.slice.call(o,start) === "b""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_index_of_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abcd".indexOf("bc")===1 && "abcd".indexOf("x")===-1 && "abcd".indexOf("", 2)===2 && "ab".indexOf("a", -1)===0"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_index_of_is_generic_and_coerces_position() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o={toString:function(){return "ab";}}; var pos={valueOf:function(){return 1;}}; String.prototype.indexOf.call(o,"b",pos)===1"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_includes_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abcd".includes("bc") && !"abcd".includes("x") && "abcd".includes("", 2) && !"ab".includes("a", 1) && (function(){var pos={valueOf:function(){return 2;}}; return "abcd".includes("cd", pos);})() && String.prototype.includes.call(123,"23")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_starts_with_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abcd".startsWith("ab") && !"abcd".startsWith("bc") && "abcd".startsWith("bc", 1) && "abcd".startsWith("", 4) && String.prototype.startsWith.call(123, "12")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_ends_with_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abcd".endsWith("cd") && !"abcd".endsWith("bc") && "abcd".endsWith("bc", 3) && "abcd".endsWith("", 1) && "abcd".endsWith("cd", 1e999) && String.prototype.endsWith.call(123, "23")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_trim_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"(" \n\t\u2000abc\u2000 \r").trim() === "abc" && String.prototype.trim.call({toString:function(){return "\u3000x\u3000";}}) === "x""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_trim_start_end_work_and_are_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"(" \n\t\u2000abc\u2000 \r").trimStart() === "abc\u2000 \r"
        && (" \n\t\u2000abc\u2000 \r").trimEnd() === " \n\t\u2000abc"
        && String.prototype.trimStart.call({toString:function(){return "\u3000x\u3000";}}) === "x\u3000"
        && String.prototype.trimEnd.call({toString:function(){return "\u3000x\u3000";}}) === "\u3000x"
        && (" \n\t\u2000abc\u2000 \r").trimLeft() === "abc\u2000 \r"
        && (" \n\t\u2000abc\u2000 \r").trimRight() === " \n\t\u2000abc""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_substring_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abcd".substring(1, 3) === "bc" && "abcd".substring(2) === "cd" && "abcd".substring(-1, 2) === "ab" && "abcd".substring(3, 1) === "bc" && "abcd".substring(1, 1e999) === "bcd" && String.prototype.substring.call({toString:function(){return "ab";}}, 1) === "b""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_split_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a="a,b,c".split(","); var b="a,".split(","); var c=",".split(","); var d="abc".split(); var e="ab".split(""); var f="".split(","); var g="".split(""); a.length===3 && a[0]==="a" && a[2]==="c" && b.length===2 && b[1]==="" && c.length===2 && c[0]==="" && d.length===1 && d[0]==="abc" && "a,b".split(",", 1)[0]==="a" && "a,b".split(",", 2)[1]==="b" && e.length===2 && e[1]==="b" && f.length===1 && f[0]==="" && g.length===0 && String.prototype.split.call(123, "")[2]==="3""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_repeat_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var ok = "ab".repeat(3) === "ababab"
        && "ab".repeat(0) === ""
        && String.prototype.repeat.call(123, 2) === "123123"
        && "a".repeat(Number.NaN) === "";
      var neg = false;
      try { "a".repeat(-1); } catch(e) { neg = e.name === "RangeError"; }
      var inf = false;
      try { "a".repeat(1e999); } catch(e) { inf = e.name === "RangeError"; }
      ok && neg && inf"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_substr_works_and_is_generic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""abcd".substr(1, 2) === "bc" && "abcd".substr(-2) === "cd" && "abcd".substr(1, -1) === "" && String.prototype.substr.call(123, 1) === "23""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_prototype_to_lower_upper_case_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#""AbC".toLowerCase() === "abc" && "abc".toUpperCase() === "ABC" && "\u00df".toUpperCase() === "SS" && String.prototype.toLowerCase.call(123) === "123" && String.prototype.toUpperCase.call(123) === "123""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_parse_works_with_objects_arrays_and_reviver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o = JSON.parse(' { "a": 1, "b": [true, null, "x"], "c": {"d": 2} } ');
         var ok = o.a === 1 && o.b.length === 3 && o.b[0] === true && o.b[1] === null && o.b[2] === "x" && o.c.d === 2;
         ok = ok && JSON.parse(' "hi\\n" ') === "hi\n";
         var bad = false;
         try { JSON.parse("{"); } catch(e) { bad = e.name === "SyntaxError"; }
         var r = JSON.parse('{"a":1,"b":2}', function(k,v){ if (k === "b") return undefined; if (typeof v === "number") return v + 1; return v; });
         ok && bad && r.a === 2 && !r.hasOwnProperty("b")"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
