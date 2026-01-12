// @lib: es5
const Object = {
  setPrototypeOf: (o: object, _p: object) => o,
};

const value: object = {};
Object.setPrototypeOf(value, {});
