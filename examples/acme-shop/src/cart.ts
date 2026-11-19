export interface Item {
  id: string;
  price: number;
  qty: number;
}

export class Cart {
  private items = new Map<string, Item>();

  add(id: string, price: number, qty = 1): void {
    const item = this.items.get(id);
    if (item) item.qty += qty;
    else this.items.set(id, { id, price, qty });
  }

  remove(id: string): void {
    this.items.delete(id);
  }

  setQty(id: string, qty: number): void {
    if (qty <= 0) return this.remove(id);
    const item = this.items.get(id);
    if (!item) throw new Error(`not in cart: ${id}`);
    item.qty = qty;
  }

  get count(): number {
    let n = 0;
    for (const i of this.items.values()) n += i.qty;
    return n;
  }

  lines(): Item[] {
    return [...this.items.values()];
  }
}
