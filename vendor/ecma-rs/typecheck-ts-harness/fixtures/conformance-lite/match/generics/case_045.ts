// @lib: es5

function identity<T>(value: T): T {
  return value;
}

const s: string = identity("ok");
const n: number = identity(45);
void s;
void n;
