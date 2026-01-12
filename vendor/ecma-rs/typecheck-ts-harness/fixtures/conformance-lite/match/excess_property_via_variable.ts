// @lib: es5

type Point = { x: number };

const tmp = { x: 1, y: 2 };

// Excess property checks apply only to fresh object literals.
export const ok: Point = tmp;
