// @lib: es5

namespace N {
  export const x = 1;
  export type T = { a: number };
}

const n: number = N.x;
const v: N.T = { a: 1 };
void n;
void v;
