// @lib: es5

function first<T>(values: T[]): T {
  return values[0];
}

const n: number = first([30, 31]);
const s: string = first(["a", "b"]);
void n;
void s;
