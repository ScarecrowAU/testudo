import { test, expect } from '@playwright/test';
import { addToCart, openShop } from './shop';

test.describe('cart', () => {
  test('adding a product updates the badge', async ({ page }) => {
    await openShop(page);
    await addToCart(page, 'Copper Kettle', 'Stoneware Mug', 'Stoneware Mug');
    await expect(page.locator('#badge')).toHaveText('3');
    await page.waitForTimeout(400);
  });

  test('totals include tax', async ({ page }) => {
    await openShop(page);
    await addToCart(page, 'Arc Floor Lamp', 'Wall Clock');
    await page.locator('#cart-link').click();
    await expect(page.locator('#total')).toHaveText('$191.40');
    await page.waitForTimeout(600);
  });

  test('removing the last item empties the cart', async ({ page }) => {
    await openShop(page);
    await addToCart(page, 'Oak Lounge Chair');
    await page.locator('#cart-link').click();
    await page.getByRole('button', { name: 'Remove' }).click();
    await expect(page.getByText('Your cart is empty.')).toBeVisible();
    await expect(page.locator('#badge')).toHaveText('0');
  });
});
