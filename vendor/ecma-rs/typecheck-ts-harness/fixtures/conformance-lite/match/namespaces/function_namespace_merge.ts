// @lib: es5

function f() {}

namespace f {
  export const x = 1;
}

const n: number = f.x;
void n;
