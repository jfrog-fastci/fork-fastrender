// @lib: es5

enum E {
  A = 1,
}

namespace E {
  export const B = 2;
}

const n: number = E.A + E.B;
void n;
