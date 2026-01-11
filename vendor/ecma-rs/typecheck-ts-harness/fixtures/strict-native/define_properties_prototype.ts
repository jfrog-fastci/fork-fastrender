// @lib: es2015
declare const Foo: { prototype: object };

const Object = {
  defineProperties: (_o: object, _props: object) => {},
};

Object.defineProperties(Foo.prototype, {});

