import assert from 'node:assert/strict';
import { launchBrowser } from './test-browser.mjs';

const browser = await launchBrowser();
try {
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.stack));
  await page.goto(process.argv[2]);
  await page.getByRole('button', { name: 'New session' }).click();
  await page.locator('#end').waitFor({ state: 'attached' });
  assert.deepEqual(errors, [], 'opening a live chat raised a browser exception');

  const draft = page.getByRole('textbox');
  const send = page.getByRole('button', { name: 'Send', exact: true });
  // Hold the real admission response so an edit can race it deterministically.
  await page.route('**/api/send', async route => {
    const response = await route.fetch();
    await draft.fill('edited while pending');
    await route.fulfill({ response });
  });
  await draft.fill('first echo');
  const admitted = page.waitForResponse(response => response.url().endsWith('/api/send'));
  await send.click();
  await (await admitted).finished();
  await page.waitForFunction(() =>
    document.querySelector('#transcript').textContent.split('first echo').length === 3);
  assert.equal(await draft.inputValue(), 'edited while pending');
  await page.unroute('**/api/send');

  // Reusing an acknowledged draft must still clear it on the next admission.
  for (const [text, occurrences] of [['first echo', 4], ['small live echo', 2]]) {
    await draft.fill(text);
    await send.click();
    await page.waitForFunction(() => document.querySelector('input[name=text]').value === '');
    await page.waitForFunction(
      ([t, count]) => document.querySelector('#transcript').textContent.split(t).length === count + 1,
      [text, occurrences]);
  }
  assert.deepEqual(errors, [], 'live chat raised a browser exception');
} finally {
  await browser.close();
}
