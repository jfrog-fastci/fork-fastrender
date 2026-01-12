// @lib: es5
declare const Foo: { prototype: object };

const Object = {
  defineProperties: (_o: object, _props: object) => {},
};

Object.defineProperties(Foo.prototype, {});
