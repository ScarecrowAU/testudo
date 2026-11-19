import { describe, expect, test } from 'vitest';
import { deliveryEstimate, money, orderNumber } from './format';

describe('format', () => {
  test('money uses the currency symbol and cents', () => {
    expect(money(1234.5)).toBe('$1,234.50');
    expect(money(9, 'EUR')).toBe('€9.00');
  });

  test('order numbers are zero-padded', () => {
    expect(orderNumber(42)).toBe('A-00042');
  });

  test('express delivery arrives the next day', () => {
    const from = new Date('2026-03-02T09:00:00Z');
    console.log('standard:', deliveryEstimate(from, false), 'express:', deliveryEstimate(from, true));
    expect(deliveryEstimate(from, true)).toBe('2026-03-03');
  });
});
