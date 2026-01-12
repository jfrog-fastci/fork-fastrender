// @lib: es5

function first<T>(values: T[]): T {
  return values[0];
}

const n: number = first([46, 47]);
const s: string = first(["a", "b"]);
void n;
void s;
