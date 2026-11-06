import fs from "fs";

// Attached to add.
function add(a, b) {
  return a + b;
}

export class Store {
  count = 0;

  get(key) {
    return this.map[key];
  }
}

export const handler = async (req) => {
  return add(1, 2);
};

module.exports = { add };

exports.extra = function () {
  return 1;
};
