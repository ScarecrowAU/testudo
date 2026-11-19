// A small, self-contained storefront: one HTML page with its own styles and
// script, served at https://acme.shop/ by Playwright's request routing, so
// the tests need no server or network.
import type { Page } from '@playwright/test';

export const products = [
  { id: 'kettle', name: 'Copper Kettle', price: 64, tint: '#d97706', tag: 'Kitchen' },
  { id: 'lamp', name: 'Arc Floor Lamp', price: 129, tint: '#2563eb', tag: 'Lighting' },
  { id: 'chair', name: 'Oak Lounge Chair', price: 349, tint: '#16a34a', tag: 'Furniture' },
  { id: 'mug', name: 'Stoneware Mug', price: 18, tint: '#9333ea', tag: 'Kitchen' },
  { id: 'rug', name: 'Wool Runner Rug', price: 210, tint: '#dc2626', tag: 'Home' },
  { id: 'clock', name: 'Wall Clock', price: 45, tint: '#0891b2', tag: 'Home' },
];

const css = `
  * { box-sizing: border-box; }
  body { margin: 0; font: 15px/1.4 system-ui, sans-serif; color: #1f2937; background: #f8fafc; }
  header { display: flex; align-items: center; gap: 24px; padding: 14px 28px; background: #111827; color: white; }
  header h1 { font-size: 20px; margin: 0; letter-spacing: .5px; }
  header nav { display: flex; gap: 16px; flex: 1; }
  header a { color: #d1d5db; text-decoration: none; cursor: pointer; }
  header a.on { color: white; font-weight: 600; }
  #cart-link { position: relative; padding-right: 22px; }
  #badge { position: absolute; right: 0; top: -6px; min-width: 18px; padding: 0 5px; border-radius: 9px;
           background: #f59e0b; color: #111827; font-size: 12px; font-weight: 700; text-align: center; }
  main { padding: 24px 28px; }
  .bar { display: flex; gap: 12px; margin-bottom: 18px; }
  input, select { padding: 9px 12px; border: 1px solid #cbd5e1; border-radius: 8px; font: inherit; background: white; }
  .bar input { flex: 1; }
  .grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 18px; }
  .card { background: white; border-radius: 12px; box-shadow: 0 1px 3px #0002; overflow: hidden; transition: transform .3s; }
  .card.hide { display: none; }
  .card .art { height: 120px; }
  .card .body { padding: 12px 14px; display: flex; align-items: center; gap: 8px; }
  .card .body div { flex: 1; }
  .card small { color: #6b7280; }
  .card b { display: block; }
  button { padding: 8px 14px; border: 0; border-radius: 8px; background: #111827; color: white; font: inherit; cursor: pointer; }
  button.ghost { background: #e5e7eb; color: #111827; }
  .added { animation: pop .6s; }
  @keyframes pop { 50% { transform: scale(1.06); } }
  table { width: 100%; border-collapse: collapse; background: white; border-radius: 12px; overflow: hidden; box-shadow: 0 1px 3px #0002; }
  td, th { padding: 12px 14px; text-align: left; border-bottom: 1px solid #e5e7eb; }
  .totals { margin-top: 18px; margin-left: auto; width: 280px; background: white; border-radius: 12px; padding: 14px; box-shadow: 0 1px 3px #0002; }
  .totals p { display: flex; justify-content: space-between; margin: 6px 0; }
  form { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; max-width: 640px; background: white; padding: 20px; border-radius: 12px; box-shadow: 0 1px 3px #0002; }
  form .wide { grid-column: span 2; }
  .error { color: #dc2626; font-size: 13px; }
  .notice { padding: 18px; border-radius: 12px; background: #dcfce7; color: #166534; font-weight: 600; }
  .spinner { width: 28px; height: 28px; border: 4px solid #e5e7eb; border-top-color: #111827; border-radius: 50%; animation: spin .8s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
`;

const script = `
  const products = ${JSON.stringify(products)};
  const cart = {};
  const money = (n) => '$' + n.toFixed(2);
  const main = () => document.querySelector('main');
  function badge() {
    const n = Object.values(cart).reduce((a, b) => a + b, 0);
    document.getElementById('badge').textContent = n;
  }
  function nav(view) {
    document.querySelectorAll('header nav a').forEach((a) => a.classList.toggle('on', a.dataset.view === view));
    history.pushState(null, '', view === 'shop' ? '/' : '/' + view);
    ({ shop, cart: showCart, checkout, account })[view]();
  }
  function shop() {
    main().innerHTML = '<div class="bar"><input id="search" placeholder="Search products"><select id="sort"><option>Featured</option><option>Price: low to high</option></select></div><div class="grid"></div>';
    const grid = main().querySelector('.grid');
    for (const p of products) {
      const card = document.createElement('div');
      card.className = 'card';
      card.dataset.name = p.name.toLowerCase();
      card.innerHTML = '<div class="art" style="background:linear-gradient(135deg,' + p.tint + ',' + p.tint + '55)"></div>' +
        '<div class="body"><div><b>' + p.name + '</b><small>' + p.tag + ' · ' + money(p.price) + '</small></div><button>Add</button></div>';
      card.querySelector('button').onclick = () => {
        cart[p.id] = (cart[p.id] || 0) + 1;
        card.classList.remove('added'); void card.offsetWidth; card.classList.add('added');
        console.log('added to cart: ' + p.id);
        badge();
      };
      grid.append(card);
    }
    document.getElementById('search').oninput = (e) => {
      const q = e.target.value.toLowerCase();
      grid.querySelectorAll('.card').forEach((c) => c.classList.toggle('hide', !c.dataset.name.includes(q)));
    };
  }
  function showCart() {
    const rows = Object.entries(cart).map(([id, qty]) => {
      const p = products.find((x) => x.id === id);
      return '<tr data-id="' + id + '"><td>' + p.name + '</td><td>' + qty + '</td><td>' + money(p.price * qty) + '</td><td><button class="ghost">Remove</button></td></tr>';
    });
    const subtotal = Object.entries(cart).reduce((a, [id, q]) => a + products.find((x) => x.id === id).price * q, 0);
    main().innerHTML = rows.length
      ? '<table><tr><th>Product</th><th>Qty</th><th>Price</th><th></th></tr>' + rows.join('') + '</table>' +
        '<div class="totals"><p><span>Subtotal</span><span>' + money(subtotal) + '</span></p><p><span>Tax (10%)</span><span>' + money(subtotal * 0.1) + '</span></p><p><b>Total</b><b id="total">' + money(subtotal * 1.1) + '</b></p></div>'
      : '<p class="notice" style="background:#f1f5f9;color:#475569">Your cart is empty.</p>';
    main().querySelectorAll('tr[data-id] button').forEach((b) => (b.onclick = () => {
      delete cart[b.closest('tr').dataset.id]; badge(); showCart();
    }));
  }
  function checkout() {
    main().innerHTML = '<form novalidate><input class="wide" name="name" placeholder="Full name"><input class="wide" name="street" placeholder="Street address">' +
      '<input name="city" placeholder="City"><input name="postcode" placeholder="Postcode"><select class="wide" name="ship"><option>Standard (3–5 days)</option><option>Express (next day)</option></select>' +
      '<div class="wide error" id="errors"></div><button class="wide">Place order</button></form>';
    main().querySelector('form').onsubmit = (e) => {
      e.preventDefault();
      const f = new FormData(e.target);
      const missing = ['name', 'street', 'city', 'postcode'].filter((k) => !f.get(k));
      document.getElementById('errors').textContent = missing.length ? 'Please fill in: ' + missing.join(', ') : '';
      if (missing.length) { console.warn('checkout blocked, missing ' + missing.join(', ')); return; }
      main().innerHTML = '<div style="display:flex;gap:14px;align-items:center"><div class="spinner"></div>Processing payment…</div>';
      setTimeout(() => {
        main().innerHTML = '<p class="notice" id="confirmation">Order #A-' + (1000 + Object.keys(cart).length) + ' confirmed. Thanks for shopping with Acme!</p>';
        console.log('order placed');
      }, 1800);
    };
  }
  function account() {
    main().innerHTML = '<form><input class="wide" name="email" placeholder="Email"><input class="wide" name="password" type="password" placeholder="Password"><button class="wide">Sign in</button></form>';
    main().querySelector('form').onsubmit = (e) => {
      e.preventDefault();
      const email = new FormData(e.target).get('email');
      main().innerHTML = '<p class="notice" id="welcome">Welcome back, ' + email + '</p>';
      document.querySelector('header [data-view=account]').textContent = email;
    };
  }
  document.querySelectorAll('header nav a').forEach((a) => (a.onclick = () => nav(a.dataset.view)));
  nav('shop');
`;

const html = `<!doctype html><html><head><title>Acme Shop</title><style>${css}</style></head><body>
<header><h1>ACME</h1><nav><a data-view="shop">Shop</a><a data-view="checkout">Checkout</a><a data-view="account">Account</a></nav>
<a id="cart-link" data-view="cart" onclick="nav('cart')" style="color:white;cursor:pointer">Cart <span id="badge">0</span></a></header>
<main></main><script>${script}</script></body></html>`;

export const origin = 'https://acme.shop';

/** Open the shop's catalogue. */
export async function openShop(page: Page) {
  await page.route(`${origin}/**`, (route) => route.fulfill({ contentType: 'text/html; charset=utf-8', body: html }));
  await page.goto(`${origin}/`);
}

/** Add products to the cart by name, the way a shopper would. */
export async function addToCart(page: Page, ...names: string[]) {
  for (const name of names) {
    await page.locator('.card', { hasText: name }).getByRole('button', { name: 'Add' }).click();
    await page.waitForTimeout(250);
  }
}
