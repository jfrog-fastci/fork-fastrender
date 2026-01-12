// @lib: es5

function f(x: "a"): 1;
function f(x: "b"): 2;
function f(x: string) {
  return 0 as any;
}

const a: 1 = f("a");
const b: 2 = f("b");

// Ensure overload return types flow into unions.
const v: 1 | 2 = 5 % 2 ? a : b;
void v;
