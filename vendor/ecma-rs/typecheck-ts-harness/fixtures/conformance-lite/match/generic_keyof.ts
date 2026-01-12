// @lib: es5

function getProp<T, K extends keyof T>(obj: T, key: K): T[K] {
  return obj[key];
}

const obj = { a: 1, b: "x" };

export const n: number = getProp(obj, "a");
export const s: string = getProp(obj, "b");
