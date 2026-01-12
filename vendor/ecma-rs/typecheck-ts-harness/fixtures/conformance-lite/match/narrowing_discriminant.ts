// @lib: es5

type Tagged =
  | { kind: "a"; a: number }
  | { kind: "b"; b: string };

export function pick(x: Tagged) {
  if (x.kind === "a") {
    const a: number = x.a;
    return a;
  }

  const b: string = x.b;
  return b;
}
