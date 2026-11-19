export const TAX_RATE = 0.1;

export interface Line {
  price: number;
  qty: number;
}

export function subtotal(lines: Line[]): number {
  return lines.reduce((sum, l) => sum + l.price * l.qty, 0);
}

export function withTax(amount: number): number {
  return Math.round(amount * (1 + TAX_RATE) * 100) / 100;
}

/** Percentage codes like SAVE10; unknown codes give no discount. */
export function discount(amount: number, code: string): number {
  const m = /^SAVE(\d{1,2})$/.exec(code.trim().toUpperCase());
  return m ? Math.round(amount * Number(m[1])) / 100 : 0;
}

export function shipping(amount: number, express: boolean): number {
  if (express) return 15;
  return amount >= 100 ? 0 : 7.5;
}
