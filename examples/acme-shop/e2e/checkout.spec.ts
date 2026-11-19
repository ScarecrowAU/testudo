import { test, expect } from '@playwright/test';
import { addToCart, openShop } from './shop';

test.describe('checkout', () => {
  test('the address form lists missing fields', async ({ page }) => {
    await openShop(page);
    await page.getByText('Checkout').click();
    await page.getByPlaceholder('Full name').fill('Ada Lovelace');
    await page.getByRole('button', { name: 'Place order' }).click();
    await expect(page.locator('#errors')).toHaveText('Please fill in: street, city, postcode');
    await page.waitForTimeout(500);
  });

  test('placing an order confirms it', async ({ page }) => {
    await openShop(page);
    await test.step('fill the cart', async () => {
      await addToCart(page, 'Wool Runner Rug', 'Copper Kettle');
    });
    await test.step('enter the address', async () => {
      await page.getByText('Checkout').click();
      await page.getByPlaceholder('Full name').pressSequentially('Ada Lovelace', { delay: 60 });
      await page.getByPlaceholder('Street address').pressSequentially('12 Analytical Way', { delay: 60 });
      await page.getByPlaceholder('City').fill('London');
      await page.getByPlaceholder('Postcode').fill('N1 9GU');
      await page.locator('select[name=ship]').selectOption({ index: 1 });
    });
    await test.step('pay', async () => {
      await page.getByRole('button', { name: 'Place order' }).click();
      await expect(page.locator('#confirmation')).toContainText('confirmed');
    });
    await page.waitForTimeout(800);
  });
});
