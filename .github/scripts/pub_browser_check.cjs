'use strict';
const fs = require('fs');
const path = require('path');
const {pathToFileURL} = require('url');
const [playwrightRoot, browserPath, html, output, expectedLocation] = process.argv.slice(2);
const resolvedPlaywright = path.resolve(playwrightRoot);
if (require(path.join(resolvedPlaywright, 'package.json')).version !== '1.65.0') {
  throw new Error('Expected exactly playwright-core 1.65.0');
}
const {chromium} = require(resolvedPlaywright);
fs.mkdirSync(output, {recursive: false});
const checks = [], errors = [], requests = [], consoleErrors = [];
const check = (value, label) => { if (!value) throw new Error(label); checks.push(label); };
(async () => {
  let browser;
  try {
    browser = await chromium.launch({executablePath: browserPath, headless: true,
      args: ['--disable-background-networking', '--disable-component-update', '--no-first-run']});
    const context = await browser.newContext({viewport: {width: 1440, height: 1000}, offline: true});
    context.on('request', request => { if (/^https?:/i.test(request.url())) requests.push(request.url()); });
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(String(error)));
    page.on('console', message => { if (message.type() === 'error') consoleErrors.push(message.text()); });
    await page.goto(pathToFileURL(path.resolve(html)).href, {waitUntil: 'load'});
    await page.locator('.ff-detail').waitFor();
    check(await page.locator('h1').innerText() === 'Tests under mutation', 'report rendered');
    check((await page.locator('.ff-stat-num').first().innerText()).replace(/\s/g, '') === '100.0%', 'score 100');
    check(await page.locator('[data-action="mutant"]').count() === 1, 'one retained mutant');
    check((await page.locator('.ff-detail-heading').innerText()).includes('KILLED'), 'killed result');
    check((await page.locator('.ff-sourcebox').innerText()).includes('Кошелёк α 🧪'), 'retained Unicode source');
    check(await page.evaluate(() => globalThis.pubAcceptanceInjected !== true), 'captured script text inert');
    await page.locator('[data-action="filter"][data-value="SURVIVED"]').click();
    check(await page.locator('[data-action="mutant"]').count() === 0, 'survivor filter');
    check((await page.locator('.ff-stat-num').first().innerText()).replace(/\s/g, '') === '100.0%', 'filter preserves score');
    await page.locator('[data-action="filter"][data-value="KILLED"]').click();
    check(await page.locator('[data-action="mutant"]').count() === 1, 'killed filter');
    await page.locator('[data-action="tab"][data-value="tests"]').click();
    check(await page.locator('#ff-tests-panel').isVisible(), 'tests tab');
    await page.locator('[data-action="tab"][data-value="metadata"]').click();
    check(await page.locator('#ff-metadata-panel').isVisible(), 'metadata tab');
    await page.locator('[data-action="tab"][data-value="change"]').click();
    await page.evaluate(() => Object.defineProperty(navigator, 'clipboard', {configurable: true,
      value: {writeText: async () => {throw new Error('Acceptance clipboard denied');}}}));
    await page.locator('[data-action="copy-location"]').click();
    const location = page.locator('#ff-copy-location');
    await location.waitFor();
    check(await location.inputValue() === expectedLocation, 'exact Unicode copy fallback');
    await page.screenshot({path: path.join(output, 'desktop.png'), fullPage: true});
    // A fresh mobile context avoids preserving a focused copy field's scroll
    // position while switching viewport size during a full-page capture.
    const mobileContext = await browser.newContext({viewport: {width: 390, height: 844}, offline: true});
    mobileContext.on('request', request => { if (/^https?:/i.test(request.url())) requests.push(request.url()); });
    const mobile = await mobileContext.newPage();
    mobile.on('pageerror', error => errors.push(String(error)));
    mobile.on('console', message => { if (message.type() === 'error') consoleErrors.push(message.text()); });
    await mobile.goto(pathToFileURL(path.resolve(html)).href, {waitUntil: 'load'});
    await mobile.locator('.ff-detail').waitFor();
    check(await mobile.locator('h1').count() === 1, 'single mobile report');
    check((await mobile.locator('.ff-stat-num').first().innerText()).replace(/\s/g, '') === '100.0%', 'mobile score 100');
    check(await mobile.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'mobile no overflow');
    check(await mobile.locator('#ff-file-select').isVisible(), 'mobile file selector');
    await mobile.screenshot({path: path.join(output, 'mobile.png'), fullPage: true});
    await mobileContext.close();
    check(errors.length === 0, 'no JavaScript errors');
    check(consoleErrors.length === 0, 'no console errors');
    check(requests.length === 0, 'no HTTP requests');
    fs.writeFileSync(path.join(output, 'verification.json'), JSON.stringify({status: 'passed',
      browser: browser.version(), cases: 2, assertions: checks.length, checks,
      jsErrors: errors, consoleErrors, httpRequests: requests, offline: true}, null, 2));
  } catch (error) {
    fs.writeFileSync(path.join(output, 'failure.json'), JSON.stringify({status: 'failed', error: String(error.stack || error),
      checks, jsErrors: errors, consoleErrors, httpRequests: requests}, null, 2));
    throw error;
  } finally { if (browser) await browser.close(); }
})().catch(error => {console.error(error); process.exitCode = 1;});
