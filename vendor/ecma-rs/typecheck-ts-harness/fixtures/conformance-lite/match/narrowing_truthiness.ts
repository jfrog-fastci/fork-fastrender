// @lib: es5

export function read(x: { value: number } | undefined) {
  if (x) {
    const v: number = x.value;
    return v;
  }

  const u: undefined = x;
  return 0;
}
