import { test, expect } from '@playwright/test';
import { openShop, products } from './shop';

test.describe('catalogue', () => {
  test('lists every product', async ({ page }) => {
    await openShop(page);
    await expect(page.locator('.card')).toHaveCount(products.length);
    await page.waitForTimeout(600);
  });

  test('search narrows the results', async ({ page }) => {
    await openShop(page);
    await test.step('type a search', async () => {
      await page.getByPlaceholder('Search products').pressSequentially('lamp', { delay: 120 });
    });
    await test.step('only matches remain', async () => {
      await expect(page.locator('.card:not(.hide)')).toHaveCount(1);
      await expect(page.locator('.card:not(.hide)')).toContainText('Arc Floor Lamp');
    });
    await page.waitForTimeout(500);
  });
});
