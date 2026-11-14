import { test, expect } from '@playwright/test';

// Self-contained pages (no network dependency) so the demo runs offline.
const page1 = `data:text/html,<h1 id=t>Testudo</h1><button onclick="document.getElementById('t').textContent='clicked'; console.log('clicked')">go</button>`;

test('counter page updates on click', async ({ page }) => {
  await page.goto(page1);
  await page.waitForTimeout(400);
  await page.getByRole('button').click();
  await expect(page.locator('#t')).toHaveText('clicked');
  await page.waitForTimeout(400);
});

test('animated page streams frames', async ({ page }) => {
  await page.setContent(`<div id=b style="width:80px;height:80px;background:teal;position:relative"></div>
    <script>let x=0;setInterval(()=>{x=(x+8)%600;b.style.left=x+'px'},16)</script>`);
  await page.waitForTimeout(1500);
  await expect(page.locator('#b')).toBeVisible();
});

test('intentionally failing test', async ({ page }) => {
  await page.setContent('<p>nope</p>');
  await expect(page.locator('p')).toHaveText('yes', { timeout: 500 });
});
