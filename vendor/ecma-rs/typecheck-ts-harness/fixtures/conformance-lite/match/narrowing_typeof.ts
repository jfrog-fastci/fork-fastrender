// @lib: es5

export function format(x: string | number) {
  if (typeof x === "string") {
    const s: string = x;
    return s.toUpperCase();
  }

  const n: number = x;
  return n.toFixed(2);
}
