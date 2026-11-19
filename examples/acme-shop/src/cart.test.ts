import { describe, expect, test } from 'vitest';
import { Cart } from './cart';

describe('cart', () => {
  test('adding the same product twice raises its quantity', () => {
    const cart = new Cart();
    cart.add('mug', 18);
    cart.add('mug', 18);
    console.log('lines:', JSON.stringify(cart.lines()));
    expect(cart.lines()).toEqual([{ id: 'mug', price: 18, qty: 2 }]);
  });

  test('count is the total quantity', () => {
    const cart = new Cart();
    cart.add('kettle', 64);
    cart.add('mug', 18, 3);
    expect(cart.count).toBe(4);
  });

  test('a quantity of zero removes the line', () => {
    const cart = new Cart();
    cart.add('lamp', 129);
    cart.setQty('lamp', 0);
    expect(cart.lines()).toEqual([]);
  });

  test('changing an unknown product throws', () => {
    const cart = new Cart();
    console.warn('expecting an error for a product that is not in the cart');
    expect(() => cart.setQty('rug', 2)).toThrow('not in cart: rug');
  });
});
