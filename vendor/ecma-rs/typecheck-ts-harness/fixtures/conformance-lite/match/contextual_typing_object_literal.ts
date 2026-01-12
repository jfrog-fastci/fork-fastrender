// @lib: es5

type Shape =
  | { kind: "circle"; radius: number }
  | { kind: "square"; size: number };

export const circle: Shape = { kind: "circle", radius: 1 };
export const square: Shape = { kind: "square", size: 2 };
