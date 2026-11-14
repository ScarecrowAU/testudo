import { expect, test } from 'vitest';

test('renders and clicks a button', async () => {
  document.body.innerHTML = '<button id=b>0</button>';
  const b = document.getElementById('b')!;
  b.onclick = () => (b.textContent = String(Number(b.textContent) + 1));
  for (let i = 0; i < 5; i++) {
    b.click();
    await new Promise((r) => setTimeout(r, 150));
  }
  expect(b.textContent).toBe('5');
});

test('fails on purpose', () => {
  expect(1 + 1).toBe(3);
});
