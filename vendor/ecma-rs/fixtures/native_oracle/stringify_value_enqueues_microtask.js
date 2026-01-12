throw {
  toString() {
    Promise.resolve().then(() => 0);
    return "boom";
  },
};
