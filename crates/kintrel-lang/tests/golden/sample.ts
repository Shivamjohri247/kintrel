import { readFile } from "fs/promises";
export { helper } from "./helper";

/** Price of a cart line, in cents. */
export function price(qty: number): number {
  return qty * 100;
}

export function price(qty: number, currency: string): number;

// Plain comment, separated by a blank line: glue.

@sealed
export class Cart extends Base implements Priced {
  private items: Item[] = [];

  /** Total price. */
  total(): number {
    return this.items.length;
  }

  static empty(): Cart {
    return new Cart();
  }
}

export interface Priced {
  price: number;
  total(): number;
}

type Money = { cents: number };
export enum Currency { USD, EUR }
export const TAX = 0.2, FEE = 1;
let counter = 0;

namespace Util {
  export const y = 2;
}

console.log("top-level statement: glue");
