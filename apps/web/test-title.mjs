import assert from 'node:assert/strict';
import { launchBrowser } from './test-browser.mjs';

// Run against an isolated `a web --no-open` with the echo fixtures enabled.
const browser = await launchBrowser();
try {
  const page = await browser.newPage();
  page.setDefaultTimeout(10_000);
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(process.argv[2]);
  await page.getByRole('button', { name: 'New session' }).click();
  await page.locator('#end').waitFor({ state: 'attached' });
  assert.equal(await page.title(), 'a', 'an untitled conversation has a default tab title');
  const send = async text => {
    await page.getByRole('textbox').fill(text);
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('#composer').value === '');
  };
  await send('/title fixture:title:Browser <title>');
  await page.waitForFunction(() => document.title === 'Browser <title>');
  assert.equal(await page.locator('#title').textContent(), 'Browser <title>');
  await send('/title fixture:title:quotes');
  await page.waitForFunction(() => document.querySelector('#transcript').textContent.includes('Title was not changed: the model returned an empty title.'));
  assert.equal(await page.title(), 'Browser <title>', 'an unusable reply preserves the projected tab title');
  await send('call background_task {"prompt":"child greeting","messages":[]}');
  await page.locator('[data-conversation]').nth(1).waitFor();
  const parent = await page.locator('[data-conversation]').first().getAttribute('data-conversation');
  await page.locator('[data-conversation]').nth(1).click();
  await page.waitForFunction(() => document.title === 'a');
  await send('/title fixture:title:Child title');
  await page.waitForFunction(() => document.title === 'Child title');
  await page.locator(`[data-conversation="${parent}"]`).click();
  await page.waitForFunction(() => document.title === 'Browser <title>');
  await page.reload();
  await page.waitForFunction(() => document.title === 'Browser <title>');
  assert.deepEqual(errors, [], 'title changes and navigation raise no browser exceptions');
  console.log('PASS: browser titles follow live changes, preserve usable metadata, switch and reload');
} finally {
  await browser.close();
}
