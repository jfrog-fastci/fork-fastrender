// @lib: es5

function f(x: "a"): 1;
function f(x: "b"): 2;
function f(x: string) {
  return 0 as any;
}

const v: 1 = f("b");
void v;
