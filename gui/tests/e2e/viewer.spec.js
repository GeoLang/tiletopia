import { test, expect } from '@playwright/test';

test.describe('TileTopia Viewer', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    // Wait for Cesium to initialize
    await page.waitForSelector('#cesium-container .cesium-widget', { timeout: 15000 });
  });

  test('page loads with correct title', async ({ page }) => {
    await expect(page).toHaveTitle('TileTopia Dashboard');
  });

  test('sidebar navigation buttons exist', async ({ page }) => {
    await expect(page.locator('.nav-btn')).toHaveCount(11);
    await expect(page.locator('.nav-btn[data-view="viewer"]')).toBeVisible();
    await expect(page.locator('.nav-btn[data-view="catalog"]')).toBeVisible();
    await expect(page.locator('.nav-btn[data-view="terrain"]')).toBeVisible();
    await expect(page.locator('.nav-btn[data-view="entities"]')).toBeVisible();
  });

  test('toolbar buttons are visible', async ({ page }) => {
    await expect(page.locator('#tb-distance')).toBeVisible();
    await expect(page.locator('#tb-area')).toBeVisible();
    await expect(page.locator('#tb-height')).toBeVisible();
    await expect(page.locator('#tb-annotate')).toBeVisible();
    await expect(page.locator('#tb-style')).toBeVisible();
    await expect(page.locator('#tb-osm')).toBeVisible();
    await expect(page.locator('#tb-timeslider')).toBeVisible();
  });

  test('renderer selector exists with 3 options', async ({ page }) => {
    const select = page.locator('#renderer-choice');
    await expect(select).toBeVisible();
    const options = select.locator('option');
    await expect(options).toHaveCount(3);
    await expect(options.nth(0)).toHaveText('CesiumJS');
    await expect(options.nth(1)).toHaveText('deck.gl');
    await expect(options.nth(2)).toHaveText('MapLibre GL');
  });

  test('CesiumJS viewer initializes', async ({ page }) => {
    const canvas = page.locator('#cesium-container canvas');
    await expect(canvas.first()).toBeVisible();
  });

  test('switching to deck.gl shows overlay', async ({ page }) => {
    await page.selectOption('#renderer-choice', 'deckgl');
    await page.waitForSelector('.renderer-overlay', { timeout: 5000 });
    await expect(page.locator('.renderer-overlay')).toBeVisible();
  });

  test('switching back to CesiumJS removes overlay', async ({ page }) => {
    await page.selectOption('#renderer-choice', 'deckgl');
    await page.waitForSelector('.renderer-overlay', { timeout: 5000 });
    await page.selectOption('#renderer-choice', 'cesium');
    await expect(page.locator('.renderer-overlay')).not.toBeVisible();
  });

  test('switching to MapLibre shows overlay', async ({ page }) => {
    await page.selectOption('#renderer-choice', 'maplibre');
    await page.waitForSelector('.renderer-overlay', { timeout: 5000 });
    await expect(page.locator('.renderer-overlay')).toBeVisible();
  });

  test('time slider toggles on button click', async ({ page }) => {
    const slider = page.locator('#time-slider-container');
    await expect(slider).not.toBeVisible();
    await page.click('#tb-timeslider');
    await expect(slider).toBeVisible();
    await page.click('#tb-timeslider');
    await expect(slider).not.toBeVisible();
  });

  test('navigation switches panels', async ({ page }) => {
    // Click catalog nav
    await page.click('.nav-btn[data-view="catalog"]');
    await expect(page.locator('#panel-catalog')).toBeVisible();
    // Cesium container should be hidden
    await expect(page.locator('#cesium-container')).not.toBeVisible();
    // Switch back to viewer
    await page.click('.nav-btn[data-view="viewer"]');
    await expect(page.locator('#cesium-container')).toBeVisible();
  });

  test('OSM buildings button changes to loading state', async ({ page }) => {
    const btn = page.locator('#tb-osm');
    await expect(btn).toHaveText('🏢');
    // Mock the Overpass API to return empty
    await page.route('**/overpass-api.de/**', (route) => {
      route.fulfill({ status: 200, body: JSON.stringify({ elements: [] }) });
    });
    await btn.click();
    // Should show alert for no buildings found
    page.on('dialog', async (dialog) => {
      expect(dialog.message()).toContain('No buildings found');
      await dialog.accept();
    });
  });

  test('geocoder search bar is present', async ({ page }) => {
    await expect(page.locator('.cesium-geocoder-input')).toBeVisible();
  });

  test('upload section exists', async ({ page }) => {
    await expect(page.locator('#upload-btn')).toBeVisible();
    await expect(page.locator('#file-input')).toBeAttached();
  });

  test('server status shows disconnected without server', async ({ page }) => {
    // Wait for health check to complete
    await page.waitForTimeout(3000);
    const text = page.locator('#status-text');
    const content = await text.textContent();
    expect(content).toMatch(/Disconnected|No server/);
  });

  test('no JavaScript errors on page load', async ({ page }) => {
    const errors = [];
    page.on('pageerror', (err) => errors.push(err.message));
    await page.goto('/');
    await page.waitForSelector('#cesium-container .cesium-widget', { timeout: 15000 });
    await page.waitForTimeout(2000);
    // Filter out expected errors (terrain connection, etc.)
    const unexpected = errors.filter(
      (e) => !e.includes('Failed to fetch') && !e.includes('ECONNREFUSED') && !e.includes('terrain')
    );
    expect(unexpected).toEqual([]);
  });
});
