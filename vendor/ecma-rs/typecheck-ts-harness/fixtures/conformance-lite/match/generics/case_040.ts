// @lib: es5

interface Box<T> {
  value: T;
}

const b: Box<number> = { value: 40 };
const n: number = b.value;
void n;
