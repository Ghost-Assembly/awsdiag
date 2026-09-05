// Properties that only show up with more than one chart.

import { test, expect } from '@playwright/test';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const REPORT = pathToFileURL(path.resolve(process.env.PROBE_REPORT_PATH)).href;

const markerColours = () => [...document.querySelectorAll('.chart')].map(c =>
  Object.fromEntries([...c.querySelectorAll('.u-legend tr')].slice(1).map(tr => [
    tr.textContent.trim().split(':')[0].replace(/-+$/, ''),
    getComputedStyle(tr.querySelector('.u-marker')).borderColor,
  ])));

test('a series is the same colour in every chart and on its pill', async ({ page }) => {
  // Colours were assigned from a running counter per chart while the pills
  // indexed a de-duplicated list, so the same host was blue in one chart and
  // green in the next while its pill matched only the first. In a report
  // whose purpose is comparing hosts across panels, that misleads actively.
  await page.goto(REPORT);
  await page.waitForSelector('.chart canvas');

  const charts = await page.evaluate(markerColours);
  const withWebA = charts.filter(c => c['web-a']);
  expect(withWebA.length).toBeGreaterThan(1);
  const distinct = new Set(withWebA.map(c => c['web-a']));
  expect(distinct.size).toBe(1);

  const pill = page.getByRole('button', { name: 'web-a' });
  const pillColour = await pill.evaluate(b => getComputedStyle(b).backgroundColor);
  expect(pillColour).toBe([...distinct][0]);
});

test('two series never share a colour', async ({ page }) => {
  await page.goto(REPORT);
  await page.waitForSelector('.chart canvas');
  const charts = await page.evaluate(markerColours);
  const merged = Object.assign({}, ...charts);
  const colours = Object.values(merged);
  expect(new Set(colours).size).toBe(colours.length);
});

test('an unparsable timestamp is reported, not silently blank', async ({ page }) => {
  // A single NaN in the x column makes uPlot draw nothing while axes still
  // render, so bad input reads as "this metric had no data".
  await page.goto(REPORT);
  await expect(page.locator('.chart', { hasText: 'Broken timestamps' }))
    .toContainText('1 row skipped');
  // The two good rows still plot.
  await expect(page.locator('.chart', { hasText: 'Broken timestamps' })
    .locator('canvas').first()).toBeVisible();
});

test('a chart that returned no data says so instead of vanishing', async ({ page }) => {
  await page.goto(REPORT);
  await expect(page.locator('.chart', { hasText: 'Queried but empty' }))
    .toContainText('No data returned');
});

test('charts appear in the order they were declared', async ({ page }) => {
  // Empty charts were rendered in a separate pass, so a placeholder jumped
  // ahead of charts declared before it. The order a report presents its
  // panels in is the author's argument, not an implementation detail.
  await page.goto(REPORT);
  await page.waitForSelector('.chart canvas');
  const titles = await page.evaluate(() =>
    [...document.querySelectorAll('.chart h3')].map(h => h.textContent));
  expect(titles).toEqual([
    'CPU (%)', 'Network (B/s)', 'Broken timestamps', 'Queried but empty',
  ]);
});

test('the series toggle still reaches every chart when one is empty', async ({ page }) => {
  // The toggle used a positional index into the declared list, which diverges
  // from the plot list as soon as one chart has no plot.
  await page.goto(REPORT);
  await page.waitForSelector('.chart canvas');
  const pill = page.getByRole('button', { name: 'web-a' });
  await pill.click();
  const hidden = await page.evaluate(() =>
    [...document.querySelectorAll('.u-legend tr')]
      .filter(tr => tr.textContent.startsWith('web-a'))
      .every(tr => tr.classList.contains('u-off')));
  expect(hidden).toBe(true);
});

test('the zoom hint is present even with a single series', async ({ page }) => {
  await page.goto(REPORT);
  await expect(page.locator('.controls')).toContainText('drag to zoom');
});
