// @lib: es5

function f() {}

namespace f {
  export const y = "ok";
}

const s: string = f.y;
void s;
