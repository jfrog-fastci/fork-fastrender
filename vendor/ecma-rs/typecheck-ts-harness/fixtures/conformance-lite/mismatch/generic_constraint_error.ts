// @lib: es5

function takeX<T extends { x: number }>(arg: T) {}

const bad = { y: 1 };
takeX(bad);
