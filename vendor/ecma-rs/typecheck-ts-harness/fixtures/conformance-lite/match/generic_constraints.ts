// @lib: es5

function takeX<T extends { x: number }>(arg: T) {
  const v: number = arg.x;
  return v;
}

export const ok = takeX({ x: 1, y: "extra" });
