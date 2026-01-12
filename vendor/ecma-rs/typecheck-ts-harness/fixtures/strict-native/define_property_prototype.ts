// @lib: es5
declare const Foo: { prototype: object };

const Object = {
  defineProperty: (_o: object, _key: string, _desc: object) => {},
};

Object.defineProperty(Foo.prototype, "x", {});
