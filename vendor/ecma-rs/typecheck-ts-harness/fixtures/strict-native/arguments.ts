// @lib: es5
declare const arguments: { length: number };

function f() {
  const len = arguments.length;
  return len;
}
void f;
