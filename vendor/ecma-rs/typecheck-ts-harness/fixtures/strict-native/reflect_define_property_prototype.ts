// @lib: es5
declare const Foo: { prototype: object };

const Reflect = {
  defineProperty: (_o: object, _key: string, _desc: object) => {},
};

Reflect.defineProperty(Foo.prototype, "x", {});
