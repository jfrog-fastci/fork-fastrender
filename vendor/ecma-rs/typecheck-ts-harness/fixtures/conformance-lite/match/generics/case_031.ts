// @lib: es5

function pick<T, K extends keyof T>(obj: T, key: K): T[K] {
  return obj[key];
}

const obj = { a: 31, b: "ok" };
const a: number = pick(obj, "a");
const b: string = pick(obj, "b");
void a;
void b;
