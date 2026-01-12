// @lib: es5

class Box {
  value = 1;
}

export function read(x: Box | { other: number }) {
  if (x instanceof Box) {
    const v: number = x.value;
    return v;
  }

  const o: number = x.other;
  return o;
}
