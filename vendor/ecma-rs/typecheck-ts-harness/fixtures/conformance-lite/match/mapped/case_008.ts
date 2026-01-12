// @lib: es5

type Obj = { a: number; b: string };

type PickA = { [K in "a"]: Obj[K] };

const v: PickA = { a: 8 };
const n: number = v.a;
void n;
