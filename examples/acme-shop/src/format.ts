export function money(amount: number, currency = 'USD'): string {
  return new Intl.NumberFormat('en-US', { style: 'currency', currency }).format(amount);
}

export function orderNumber(n: number): string {
  return `A-${String(n).padStart(5, '0')}`;
}

export function deliveryEstimate(from: Date, express: boolean): string {
  const days = express ? 1 : 4;
  const d = new Date(from);
  d.setDate(d.getDate() + days);
  return d.toISOString().slice(0, 10);
}
