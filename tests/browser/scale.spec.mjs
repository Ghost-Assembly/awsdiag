// The report at incident volume.
//
// Pleasant with one cluster and unusable with three hundred is not "working".
// These budgets are deliberately loose — they exist to catch an order-of-
// magnitude regression, not to police milliseconds.

import { test, expect } from '@playwright/test';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const REPORT = pathToFileURL(path.resolve(process.env.SCALE_REPORT_PATH)).href;

test('stays responsive with 300 clusters and 24,000 datapoints', async ({ page }) => {
  const problems = [];
  page.on('console', m => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', e => problems.push(String(e)));
  await page.route('**', r => r.request().url().startsWith('file://')
    ? r.continue() : (problems.push('network: ' + r.request().url()), r.abort()));

  const started = Date.now();
  await page.goto(REPORT);
  await page.waitForSelector('.chart canvas');
  const paint = Date.now() - started;

  await expect(page.locator('tbody tr.row')).toHaveCount(300);
  expect(paint).toBeLessThan(8000);

  const filterStart = Date.now();
  await page.getByLabel('Filter log clusters').fill('variant 42');
  await expect(page.locator('tbody tr.row')).toHaveCount(1);
  expect(Date.now() - filterStart).toBeLessThan(3000);

  await page.locator('th[data-sort="count"]').click();
  await expect(page.locator('tbody tr.row')).toHaveCount(1);

  expect(problems).toEqual([]);
});

test('sorting by count orders the column, in both directions', async ({ page }) => {
  await page.goto(REPORT);

  // Read the count column as numbers. Asserting monotonicity is the real
  // property; assuming which direction a click produces is not, and the
  // table already loads sorted by count so the first click toggles.
  // Third cell: sparkline, status, count. An empty selection would make
  // every monotonicity check below pass vacuously, so the count is asserted.
  const counts = async () => {
    const texts = await page.locator('tbody tr.row td:nth-child(3)').allInnerTexts();
    const nums = texts.slice(0, 20).map(t => Number(t.replace(/\D/g, '')));
    expect(nums.length).toBeGreaterThan(5);
    expect(nums.every(Number.isFinite)).toBe(true);
    return nums;
  };

  const ascending = a => a.every((v, i) => i === 0 || a[i - 1] <= v);
  const descending = a => a.every((v, i) => i === 0 || a[i - 1] >= v);

  const loaded = await counts();
  expect(descending(loaded)).toBe(true);   // loads worst-first

  await page.locator('th[data-sort="count"]').click();
  const afterOne = await counts();
  await page.locator('th[data-sort="count"]').click();
  const afterTwo = await counts();

  // Whichever way round, one click gives one order and the next the other.
  expect(ascending(afterOne) || descending(afterOne)).toBe(true);
  expect(ascending(afterOne)).toBe(descending(afterTwo));
  expect(afterOne[0]).not.toBe(afterTwo[0]);
});
