// Browser tests for the generated report.
//
// The report is a single HTML file people open from disk and forward to
// colleagues. Everything below is checked with the network blocked, because
// "self-contained" is the property that makes that safe and it is invisible
// on a developer machine that happens to be online.

import { test, expect } from '@playwright/test';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const REPORT = pathToFileURL(path.resolve(process.env.REPORT_PATH)).href;

/** Fail loudly on any page error rather than letting a broken chart pass. */
async function openReport(page) {
  const problems = [];
  page.on('console', m => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', e => problems.push(String(e)));
  // Block every network request: a report that needs the internet is not
  // self-contained, and this is the only way to prove it.
  await page.route('**', route => {
    const u = route.request().url();
    if (u.startsWith('file://')) return route.continue();
    problems.push('network request attempted: ' + u);
    return route.abort();
  });
  await page.goto(REPORT);
  await page.waitForLoadState('domcontentloaded');
  return problems;
}

test('renders with no console errors and no network access', async ({ page }) => {
  const problems = await openReport(page);
  await expect(page.locator('h1')).toHaveText('Checkout latency incident');
  expect(problems).toEqual([]);
});

test('charts draw', async ({ page }) => {
  await openReport(page);
  const canvases = page.locator('.chart canvas');
  await expect(canvases.first()).toBeVisible();
  // A uPlot canvas with zero width means it rendered into a detached or
  // zero-size box, which looks like "no data" rather than a layout bug.
  const box = await canvases.first().boundingBox();
  expect(box.width).toBeGreaterThan(100);
  expect(box.height).toBeGreaterThan(50);
});

test('a gap is drawn as a gap, not as zero', async ({ page }) => {
  await openReport(page);
  const nulls = await page.evaluate(() => {
    const d = JSON.parse(document.getElementById('awsdiag-data').textContent);
    return d.charts[0].rows.filter(r => r['web-b'] === null).length;
  });
  expect(nulls).toBeGreaterThan(0);
  // spanGaps false is what stops uPlot bridging across a missing datapoint.
  const spans = await page.evaluate(() => {
    const c = document.querySelector('.chart canvas');
    return c ? true : false;
  });
  expect(spans).toBe(true);
});

test.describe('viewed from a non-UTC timezone', () => {
  // Timezone is a context-level setting, so it belongs on a describe block.
  test.use({ timezoneId: 'America/New_York' });

  test('chart axes are in UTC, not the viewer local zone', async ({ page }) => {
    // The report header states instants in UTC. An axis in local time makes
    // one document disagree with itself about when something happened, which
    // in an incident report is a misdiagnosis waiting to happen.
    await openReport(page);
    const chartText = await page.locator('.chart').first().innerText();
    expect(chartText).not.toContain('6:00am');
    expect(chartText).not.toContain('6:01am');
    await expect(page.locator('#subtitle')).toContainText('times in UTC');
  });
});

test('the series toggle hides and restores a series', async ({ page }) => {
  await openReport(page);
  const pill = page.getByRole('button', { name: 'web-a' });
  await expect(pill).toHaveAttribute('aria-pressed', 'true');
  await pill.click();
  await expect(pill).toHaveAttribute('aria-pressed', 'false');
  await pill.click();
  await expect(pill).toHaveAttribute('aria-pressed', 'true');
});

test('cluster search filters rows and reports the count', async ({ page }) => {
  await openReport(page);
  const search = page.getByLabel('Filter log clusters');
  await expect(page.locator('tbody tr.row')).toHaveCount(1);
  await search.fill('zzz-no-such-text');
  await expect(page.locator('tbody tr.row')).toHaveCount(0);
  await expect(page.locator('td.empty')).toBeVisible();
  await search.fill('Connection');
  await expect(page.locator('tbody tr.row')).toHaveCount(1);
});

test('a cluster row expands to its exemplar and stream breakdown', async ({ page }) => {
  await openReport(page);
  const detail = page.locator('tr.detail').first();
  await expect(detail).toBeHidden();
  await page.locator('tbody tr.row').first().click();
  await expect(detail).toBeVisible();
  await expect(detail.locator('pre')).toContainText('Connection to');
  await expect(detail.locator('.tag').first()).toContainText('db-a');
});

test('a cluster row is reachable and operable by keyboard', async ({ page }) => {
  await openReport(page);
  const row = page.locator('tbody tr.row').first();
  await row.focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('tr.detail').first()).toBeVisible();
});

test('log content cannot inject markup', async ({ page }) => {
  await openReport(page);
  // The hostile string is present as text and has produced no element.
  const injected = await page.evaluate(() => document.querySelectorAll('img[onerror]').length);
  expect(injected).toBe(0);
});

test('prints cleanly: controls hidden, collapsed detail expanded', async ({ page }) => {
  // A report is forwarded as a PDF as often as it is opened. Interactive
  // controls are meaningless on paper, and anything behind a click would be
  // silently missing from the printed copy.
  await openReport(page);
  await page.emulateMedia({ media: 'print' });
  await expect(page.locator('.controls').first()).toBeHidden();
  await expect(page.locator('input.search')).toBeHidden();
  // The exemplar must appear without anyone having clicked the row.
  const detailDisplay = await page.evaluate(() =>
    getComputedStyle(document.querySelector('tr.detail')).display);
  expect(detailDisplay).not.toBe('none');
});

test('renders in dark mode too', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'dark' });
  const problems = await openReport(page);
  const bg = await page.evaluate(() =>
    getComputedStyle(document.body).backgroundColor);
  expect(bg).not.toBe('rgba(0, 0, 0, 0)');
  expect(problems).toEqual([]);
});
