import { describe, expect, test } from 'vitest';
import { discount, shipping, subtotal, withTax } from './pricing';

describe('pricing', () => {
  test('subtotal adds up every line', () => {
    expect(subtotal([{ price: 64, qty: 1 }, { price: 18, qty: 2 }])).toBe(100);
  });

  test('tax is added and rounded to cents', () => {
    expect(withTax(174)).toBe(191.4);
    expect(withTax(0.05)).toBe(0.06);
  });

  test('discount codes take a percentage off', () => {
    for (const code of ['SAVE10', 'save25', 'BOGUS']) {
      console.log(`${code} on $200 → -$${discount(200, code)}`);
    }
    expect(discount(200, 'SAVE10')).toBe(20);
    expect(discount(200, 'BOGUS')).toBe(0);
  });

  test('standard shipping is free over $100', () => {
    expect(shipping(99, false)).toBe(7.5);
    expect(shipping(100, false)).toBe(0);
    expect(shipping(100, true)).toBe(15);
  });
});
