import { test, expect } from '@playwright/test';
import { openShop } from './shop';

test('signing in shows the account name', async ({ page }) => {
  await openShop(page);
  await page.getByText('Account').click();
  await page.getByPlaceholder('Email').pressSequentially('ada@example.com', { delay: 50 });
  await page.getByPlaceholder('Password').fill('correct horse');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('#welcome')).toHaveText('Welcome back, ada@example.com');
  await page.waitForTimeout(500);
});
