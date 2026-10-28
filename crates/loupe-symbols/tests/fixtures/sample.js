// Utility functions.
function plain(a, b) {
  return a + b;
}

const arrow = (x) => x + 1;

const viaExpression = function (y) {
  return y;
};

class Widget {
  constructor(name) {
    this.name = name;
  }

  render() {
    return this.name;
  }

  static build() {
    return new Widget("w");
  }
}

module.exports = { plain };
